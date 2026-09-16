//! Opt-in upgrade gates. All mutations are test-only and registry-checked.
//! Run only this module's ignored cases; SPEC §12.11 gives the invocation.

use super::*;
use anyhow::{Result, bail, ensure};
use std::{
    cell::RefCell,
    fs::{self, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    panic::{AssertUnwindSafe, catch_unwind},
    process::{Command, Output, Stdio},
    rc::Rc,
    sync::Mutex,
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

fn live_error(error: CodexError) -> anyhow::Error {
    if error.diagnostic.kind == CodexFailureKind::Timeout {
        anyhow::anyhow!("live RPC deadline expired")
    } else {
        error.into()
    }
}

const SOCKET: &str = "ccmux-probe";
const MODEL: &str = "gpt-5.6-luna";
const CASE_TIMEOUT: Duration = Duration::from_secs(240);
static SERIAL: Mutex<()> = Mutex::new(());

fn settings(get: impl Fn(&str) -> Option<String>) -> Result<CodexConfig> {
    ensure!(get("CCMUX_CODEX_LIVE_TEST").as_deref() == Some("1"),
        "set CCMUX_CODEX_LIVE_TEST=1 explicitly");
    let config = CodexConfig {
        url: get("CCMUX_CODEX_LIVE_URL").filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("set CCMUX_CODEX_LIVE_URL explicitly"))?,
        token_file: get("CCMUX_CODEX_LIVE_TOKEN_FILE").filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("set CCMUX_CODEX_LIVE_TOKEN_FILE explicitly"))?.into(),
        bin: get("CCMUX_CODEX_LIVE_BIN").unwrap_or_else(|| "codex".into()),
    };
    validate_url(&config.url)?;
    ensure!(config.token_file.is_absolute(), "live token file must be absolute");
    Ok(config)
}

// Command output stays in memory. Never include raw stderr, screen contents,
// protocol errors, or a Command's Debug (which may include credentials).
fn output(mut command: Command) -> Result<Output> {
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped())
        .stderr(Stdio::piped()).spawn().map_err(|_| anyhow::anyhow!("probe command spawn failed"))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let read = |stream: Box<dyn Read + Send>| thread::spawn(move || {
        let mut bytes = Vec::new();
        stream.take(MAX_MESSAGE as u64 + 1).read_to_end(&mut bytes).map(|_| bytes)
    });
    let out = read(Box::new(stdout));
    let err = read(Box::new(stderr));
    let until = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait()? { break status; }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            // No command in this harness starts a child with inherited pipes.
            let _ = out.join();
            let _ = err.join();
            bail!("probe command timed out");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = out.join().map_err(|_| anyhow::anyhow!("probe stdout reader failed"))??;
    let stderr = err.join().map_err(|_| anyhow::anyhow!("probe stderr reader failed"))??;
    ensure!(stdout.len() <= MAX_MESSAGE && stderr.len() <= MAX_MESSAGE, "probe output too large");
    Ok(Output { status, stdout, stderr })
}

fn tmux_command(args: &[&str]) -> Command {
    let mut command = Command::new("tmux");
    command.args(["-L", SOCKET, "-f", "/dev/null"]).args(args)
        .env_remove("TMUX").env_remove("TMUX_PANE").env_remove("CODEX_REMOTE_TOKEN");
    command
}

fn server_absent(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> bool {
    let error = String::from_utf8_lossy(stderr);
    code == Some(1) && stdout.is_empty() && error.contains(SOCKET)
        && (error.contains("no server running") || error.contains("No such file or directory")
            || error.contains("Connection refused"))
}

struct RunLock(PathBuf);

impl Drop for RunLock {
    fn drop(&mut self) { let _ = fs::remove_file(&self.0); }
}

#[derive(Default)]
struct Registry {
    threads: BTreeMap<String, String>,
    journal: Option<File>,
}

impl Registry {
    fn record(&mut self, event: Value) -> Result<()> {
        // Callers supply only IDs, locally authored labels, counts, timings,
        // and redacted version strings. No turns, screens, or wire payloads.
        if let Some(file) = &mut self.journal {
            serde_json::to_writer(&mut *file, &event)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        Ok(())
    }

    fn created(&mut self, id: &str, name: &str) -> Result<()> {
        ensure!(valid_codex_id(id) && name.starts_with("ccmux-probe-"),
            "invalid owned thread identity");
        ensure!(!self.threads.contains_key(id), "server reused a probe thread id");
        // Retain ownership even if the durable write fails, so cleanup runs.
        self.threads.insert(id.into(), name.into());
        self.record(json!({"event":"created","id":id,"name":name}))
    }

    fn authorize(&self, method: &str, params: &Value, work: &Path) -> Result<()> {
        match method {
            "initialize" | "thread/loaded/list" => Ok(()),
            "thread/list" => {
                ensure!(params["cwd"].as_str() == work.to_str(), "listing must use the probe cwd");
                Ok(())
            }
            "thread/read" | "thread/turns/list" | "thread/name/set" | "turn/start"
            | "thread/unsubscribe" | "thread/archive" | "thread/unarchive" => {
                let id = params["threadId"].as_str().unwrap_or("");
                ensure!(self.threads.contains_key(id), "probe thread ownership guard");
                if method == "thread/name/set" {
                    ensure!(params["name"].as_str() == self.threads.get(id).map(String::as_str),
                        "probe name guard");
                }
                Ok(())
            }
            _ => bail!("method outside the live harness allowlist"),
        }
    }
}

/// Separate from the production Method/Rpc: no mutation method is added to
/// the lister. Every request other than the private creation path is guarded.
struct LiveRpc<'a> {
    transport: Box<dyn Transport + 'a>,
    registry: Rc<RefCell<Registry>>,
    work: PathBuf,
    next_id: u64,
    events: Vec<Value>,
}

impl LiveRpc<'_> {
    fn exchange(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.transport.send(json!({"id":id,"method":method,"params":params})).map_err(live_error)?;
        loop {
            let value = self.transport.receive().map_err(live_error)?;
            if value.get("method").is_some() {
                self.event(value);
                continue;
            }
            ensure!(value["id"] == id, "live RPC response id mismatch");
            ensure!(value.get("error").is_none(), "live RPC {method} rejected (body withheld)");
            return value.get("result").cloned().ok_or_else(|| anyhow::anyhow!("live RPC missing result"));
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.registry.borrow().authorize(method, &params, &self.work)?;
        self.exchange(method, params)
    }

    fn event(&mut self, value: Value) {
        // Requests receive NO reply: even an error can deny an approval.
        if value.get("id").is_some() { return; }
        let params = &value["params"];
        if params["threadId"].as_str().is_some_and(|id| self.registry.borrow().threads.contains_key(id))
            && matches!(value["method"].as_str(), Some("item/started" | "turn/completed"))
        {
            self.events.push(value);
        }
    }

    fn create(&mut self, name: &str) -> Result<String> {
        ensure!(name.starts_with("ccmux-probe-"), "probe name guard");
        let result = self.exchange("thread/start", json!({
            "cwd":self.work, "model":MODEL, "ephemeral":false,
            "approvalPolicy":"never", "approvalsReviewer":"user", "sandbox":"read-only",
            "config":{"model_reasoning_effort":"low"},
            "developerInstructions":"Isolated ccmux lifecycle test. Do only the exact task. Never inspect files, credentials, environment, networks, MCP tools, or other sessions. Never spawn agents. Keep replies short."
        }))?;
        let id = result["thread"]["id"].as_str()
            .ok_or_else(|| anyhow::anyhow!("creation returned no thread id; cleanup cannot be proved"))?;
        self.registry.borrow_mut().created(id, name)?;
        self.request("thread/name/set", json!({"threadId":id,"name":name}))?;
        Ok(id.into())
    }

    fn start_turn(&mut self, id: &str, prompt: &str) -> Result<String> {
        let value = self.request("turn/start", json!({
            "threadId":id, "input":[{"type":"text","text":prompt}], "model":MODEL, "effort":"low"
        }))?;
        Ok(value["turn"]["id"].as_str().ok_or_else(|| anyhow::anyhow!("missing turn id"))?.into())
    }

    fn read(&mut self, id: &str) -> Result<Value> {
        Ok(self.request("thread/read", json!({"threadId":id,"includeTurns":false}))?["thread"].clone())
    }

    fn turn(&mut self, id: &str, turn: &str) -> Result<Value> {
        let result = self.request("thread/turns/list", json!({
            "threadId":id,"limit":1,"sortDirection":"desc","itemsView":"full"
        }))?;
        let value = result["data"].as_array().and_then(|rows| rows.iter().find(|v| v["id"] == turn))
            .ok_or_else(|| anyhow::anyhow!("latest turn missing"))?;
        Ok(value.clone())
    }

    fn completed(&mut self, id: &str, turn: &str) -> Result<Value> {
        loop {
            let value = self.turn(id, turn)?;
            match value["status"].as_str() {
                Some("completed") => return Ok(value),
                Some("inProgress") => thread::sleep(Duration::from_millis(500)),
                _ => bail!("owned turn failed or interrupted"),
            }
        }
    }

    fn materialize(&mut self, name: &str) -> Result<String> {
        let id = self.create(name)?;
        let turn = self.start_turn(&id, "Reply exactly indexed. Do not use tools.")?;
        self.completed(&id, &turn)?;
        Ok(id)
    }

    fn loaded(&mut self, limit: u64) -> Result<(BTreeSet<String>, usize)> {
        let (rows, pages) = self.pages("thread/loaded/list", json!({"limit":limit}))?;
        let ids = rows.iter().map(|v| v.as_str().map(String::from)
            .ok_or_else(|| anyhow::anyhow!("invalid loaded id"))).collect::<Result<BTreeSet<_>>>()?;
        ensure!(ids.len() == rows.len(), "duplicate loaded id");
        Ok((ids, pages))
    }

    fn history(&mut self, db: bool, archived: bool, limit: u64) -> Result<(BTreeMap<String, Value>, usize)> {
        let (rows, pages) = self.pages("thread/list", json!({
            "limit":limit, "cwd":self.work, "sortKey":"updated_at","sortDirection":"desc",
            "sourceKinds":["cli","vscode","exec","appServer","unknown"],
            "modelProviders":[], "useStateDbOnly":db, "archived":archived
        }))?;
        let mut by_id = BTreeMap::new();
        for row in rows {
            let id = row["id"].as_str().ok_or_else(|| anyhow::anyhow!("invalid history id"))?.to_owned();
            ensure!(by_id.insert(id, row).is_none(), "duplicate history id");
        }
        Ok((by_id, pages))
    }

    fn pages(&mut self, method: &str, mut params: Value) -> Result<(Vec<Value>, usize)> {
        let mut rows = Vec::new();
        let mut cursors = BTreeSet::new();
        let mut pages = 0;
        params["cursor"] = Value::Null;
        loop {
            let result = self.request(method, params.clone())?;
            pages += 1;
            rows.extend(result["data"].as_array().ok_or_else(|| anyhow::anyhow!("invalid page"))?.clone());
            match result.get("nextCursor") {
                None | Some(Value::Null) => return Ok((rows, pages)),
                Some(Value::String(cursor)) if !cursor.is_empty() && cursors.insert(cursor.clone()) => {
                    params["cursor"] = json!(cursor);
                }
                _ => bail!("invalid or repeated live cursor"),
            }
        }
    }

    fn close(mut self) -> Result<()> {
        self.transport.close().map_err(live_error)?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct ProcessStamp { pid: u32, parent: u32, started: u64, zombie: bool }

fn process_stamp(pid: u32) -> Result<Option<ProcessStamp>> {
    let raw = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("could not verify an owned client process"),
    };
    parse_process_stamp(pid, &raw).map(Some)
}

fn parse_process_stamp(pid: u32, raw: &str) -> Result<ProcessStamp> {
    let fields: Vec<_> = raw.rsplit_once(") ").map(|(_, rest)| rest.split_whitespace().collect())
        .ok_or_else(|| anyhow::anyhow!("invalid process stat"))?;
    ensure!(fields.len() > 19, "short process stat");
    Ok(ProcessStamp { pid, parent:fields[1].parse()?, started:fields[19].parse()?,
        zombie:fields[0] == "Z" || fields[0] == "X" })
}

fn pane_clients(root: u32) -> Result<Vec<ProcessStamp>> {
    ensure!(process_stamp(root)?.is_some(), "probe pane process missing");
    let mut table = BTreeMap::new();
    // Only stat identities/ancestry, never argv or environ. No process is
    // signalled here, and only descendants of a verified owned pane are saved.
    for entry in fs::read_dir("/proc")?.flatten() {
        if let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>()
            && let Ok(Some(stamp)) = process_stamp(pid)
        { table.insert(pid, stamp); }
    }
    let mut descendants = BTreeSet::from([root]);
    loop {
        let before = descendants.len();
        for stamp in table.values() {
            if descendants.contains(&stamp.parent) { descendants.insert(stamp.pid); }
        }
        if descendants.len() == before { break; }
    }
    let clients: Vec<_> = table.into_values().filter(|p| p.pid != root && descendants.contains(&p.pid)).collect();
    ensure!(!clients.is_empty(), "no client process under the owned pane");
    Ok(clients)
}

fn clients_exited(clients: &[ProcessStamp]) -> Result<bool> {
    for old in clients {
        if let Some(now) = process_stamp(old.pid)?
            && now.started == old.started && !now.zombie
        { return Ok(false); }
    }
    Ok(true)
}

struct Harness<'a> {
    config: CodexConfig,
    client: CodexClient,
    clock: &'a dyn Clock,
    end: Duration,
    work: PathBuf,
    run: String,
    registry: Rc<RefCell<Registry>>,
    panes: BTreeSet<String>,
    clients: BTreeMap<String, Vec<ProcessStamp>>,
    server_started: bool,
    _lock: RunLock,
}

impl<'a> Harness<'a> {
    fn new(clock: &'a dyn Clock) -> Result<Self> {
        let config = settings(|key| std::env::var(key).ok())?;
        let base = PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME missing"))?)
            .join(".local/tmp");
        fs::create_dir_all(&base)?;
        let lock = base.join("ccmux-probe-live.lock");
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&lock)
            .map_err(|_| anyhow::anyhow!("live harness lock exists or is unavailable; do not run concurrently"))?;
        let guard = RunLock(lock);
        let run = format!("ccmux-probe-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos());
        writeln!(file, "{run}")?;
        let root = base.join(&run);
        fs::DirBuilder::new().mode(0o700).create(&root)?;
        let work = root.join("work");
        fs::create_dir(&work)?;
        let journal = OpenOptions::new().write(true).create_new(true).mode(0o600)
            .open(root.join("registry.jsonl"))?;
        let client = prepare(&config)?;
        eprintln!("live registry: {}", root.join("registry.jsonl").display());
        Ok(Self {
            config, client, clock, end:clock.now() + CASE_TIMEOUT, work, run,
            registry:Rc::new(RefCell::new(Registry { journal:Some(journal), ..Registry::default() })),
            panes:BTreeSet::new(), clients:BTreeMap::new(), server_started:false, _lock:guard,
        })
    }

    fn tmux(&self, args: &[&str]) -> Result<String> {
        let result = output(tmux_command(args))?;
        ensure!(result.status.success(), "throwaway tmux command failed");
        ensure!(!String::from_utf8_lossy(&result.stdout).contains(&self.client.prepared.token),
            "credential appeared in tmux output (withheld)");
        String::from_utf8(result.stdout).map_err(|_| anyhow::anyhow!("invalid tmux output"))
    }

    fn setup(&mut self) -> Result<()> {
        let existing = output(tmux_command(&["list-sessions"]))?;
        ensure!(server_absent(existing.status.code(), &existing.stdout, &existing.stderr),
            "ccmux-probe must have no server/sessions; refusing to adopt or kill an existing server");
        // Set this before spawning: a failed command can still have created it.
        // Cleanup checks the exact run's session and pane inventory before kill.
        self.server_started = true;
        let pane = self.tmux(&["new-session","-d","-s",&self.run,"-x","180","-y","48",
            "-P","-F","#{pane_id}","/usr/bin/sleep","600"])?;
        self.remember_pane(pane.trim())?;
        let mut version = Command::new(&self.config.bin);
        version.arg("--version").env_remove("TMUX").env_remove("TMUX_PANE")
            .env_remove("CODEX_REMOTE_TOKEN");
        let result = output(version)?;
        ensure!(result.status.success(), "codex --version failed");
        let cli = self.client.redact(&String::from_utf8_lossy(&result.stdout));
        self.record(json!({"event":"cli_version","value":cli}))?;
        self.rpc(Duration::from_secs(10))?.close()?;
        Ok(())
    }

    fn record(&self, value: Value) -> Result<()> {
        self.registry.borrow_mut().record(value.clone())?;
        eprintln!("{value}");
        Ok(())
    }

    fn rpc_until(&self, end: Duration) -> Result<LiveRpc<'a>> {
        let deadline = Deadline { clock:self.clock, end };
        let transport = TcpConnector.connect(&self.client.prepared, deadline).map_err(live_error)?;
        let mut rpc = LiveRpc {
            transport, registry:self.registry.clone(), work:self.work.clone(), next_id:0, events:Vec::new(),
        };
        let info = rpc.request("initialize", json!({
            "clientInfo":{"name":"ccmux_probe","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":true}
        }))?;
        let identity = info["userAgent"].as_str().ok_or_else(|| anyhow::anyhow!("server identity missing"))?;
        self.registry.borrow_mut().record(json!({
            "event":"server_version","value":self.client.redact(identity)
        }))?;
        rpc.transport.send(json!({"method":"initialized","params":{}})).map_err(live_error)?;
        Ok(rpc)
    }

    fn rpc(&self, budget: Duration) -> Result<LiveRpc<'a>> {
        self.rpc_until(self.end.min(self.clock.now() + budget))
    }

    fn name(&self, label: &str) -> String { format!("{}-{label}", self.run) }

    fn remember_pane(&mut self, pane: &str) -> Result<()> {
        ensure!(pane.strip_prefix('%').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
            "invalid probe pane id");
        self.panes.insert(pane.into());
        self.registry.borrow_mut().record(json!({"event":"pane","id":pane}))
    }

    fn check_pane(&self, pane: &str) -> Result<()> {
        ensure!(self.panes.contains(pane), "probe pane ownership guard");
        let inventory = self.tmux(&["list-panes","-a","-F","#{session_name} #{pane_id}"])?;
        ensure!(inventory.lines().any(|row| row == format!("{} {pane}", self.run)),
            "owned pane is no longer in the probe session");
        Ok(())
    }

    fn screen(&self, pane: &str) -> Result<String> {
        self.check_pane(pane)?;
        self.tmux(&["capture-pane","-p","-t",pane])
    }

    fn keys(&self, pane: &str, text: &str) -> Result<()> {
        self.check_pane(pane)?;
        self.tmux(&["send-keys","-t",pane,"-l",text])?;
        // Allow slash command completion before Enter, as in the recorded probe.
        thread::sleep(Duration::from_millis(200));
        self.tmux(&["send-keys","-t",pane,"Enter"])?;
        Ok(())
    }

    fn until(&self, budget: Duration, mut predicate: impl FnMut() -> Result<bool>) -> Result<()> {
        let end = self.end.min(self.clock.now() + budget);
        loop {
            ensure!(self.clock.now() < end, "live condition timed out");
            if predicate()? { return Ok(()); }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn open(&mut self, id: &str) -> Result<String> {
        ensure!(self.registry.borrow().threads.contains_key(id), "probe attach ownership guard");
        let command = crate::agents::codex_probe_attach_cmd(id, &self.config)?;
        let pane = self.tmux(&["new-window","-d","-t",&format!("={}:", self.run),
            "-P","-F","#{pane_id}","/bin/sh","-c",&command])?.trim().to_owned();
        self.remember_pane(&pane)?;
        self.until(Duration::from_secs(20), || {
            let screen = self.screen(&pane)?;
            ensure!(!screen.contains("[ccmux] Codex attach exited"), "Codex TUI exited before readiness");
            Ok(screen.contains("OpenAI Codex"))
        })?;
        self.keys(&pane, "/status")?;
        self.until(Duration::from_secs(15), || Ok(self.screen(&pane)?.contains(id)))?;
        let root = self.tmux(&["display-message","-p","-t",&pane,"#{pane_pid}"])?;
        let clients = pane_clients(root.trim().parse()?)?;
        self.registry.borrow_mut().record(json!({"event":"clients","pane":pane,
            "processes":clients.iter().map(|p| json!({"pid":p.pid,"started":p.started})).collect::<Vec<_>>()}))?;
        self.clients.insert(pane.clone(), clients);
        Ok(pane)
    }

    fn attached(&self, pane: &str, id: &str) -> Result<bool> {
        let latch = self.tmux(&["show-options","-pqv","-t",pane,"@ccmux_detached"])?;
        let screen = self.screen(pane)?;
        Ok(latch.trim().is_empty() && screen.contains(id)
            && !screen.contains("[ccmux] Codex attach exited"))
    }

    fn quit(&self, pane: &str) -> Result<()> {
        self.keys(pane, "/quit")?;
        self.until(Duration::from_secs(15), || {
            let latch = self.tmux(&["show-options","-pqv","-t",pane,"@ccmux_detached"])?;
            let screen = self.screen(pane)?;
            Ok(latch.trim() == "1" && screen.contains("[ccmux] Codex attach exited (rc=0)")
                && screen.contains("$(cat ") && clients_exited(&self.clients[pane])?)
        })

    }

    fn kill_pane(&mut self, pane: &str) -> Result<()> {
        self.check_pane(pane)?;
        self.tmux(&["kill-pane","-t",pane])?;
        self.panes.remove(pane);
        if let Some(clients) = self.clients.get(pane) {
            self.until(Duration::from_secs(10), || clients_exited(clients))?;
        }
        Ok(())
    }

    fn unloaded_fixture(&self, id: &str) -> Result<()> {
        // This is NOT the natural idle timer. Restore an owned archived rollout
        // and require it to be notLoaded before any resume/read-side-effect check.
        let mut rpc = self.rpc(Duration::from_secs(15))?;
        rpc.request("thread/archive", json!({"threadId":id}))?;
        rpc.request("thread/unarchive", json!({"threadId":id}))?;
        ensure!(!rpc.loaded(100)?.0.contains(id), "archive/unarchive did not yield an unloaded fixture");
        ensure!(rpc.read(id)?["status"]["type"] == "notLoaded", "unloaded fixture has wrong status");
        rpc.close()
    }

    fn cleanup(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        if self.server_started {
            let stopped = (|| -> Result<()> {
                let existing = output(tmux_command(&["list-sessions","-F","#{session_name}"]))?;
                if server_absent(existing.status.code(), &existing.stdout, &existing.stderr) { return Ok(()); }
                ensure!(existing.status.success() && String::from_utf8_lossy(&existing.stdout).trim() == self.run,
                    "cleanup refuses a foreign session on ccmux-probe");
                let panes = self.tmux(&["list-panes","-a","-F","#{pane_id}"])?;
                ensure!(panes.lines().all(|id| self.panes.contains(id)), "cleanup refuses an unregistered pane");
                self.tmux(&["kill-server"])?;
                let after = output(tmux_command(&["list-sessions"]))?;
                ensure!(server_absent(after.status.code(), &after.stdout, &after.stderr),
                    "throwaway tmux server remains");
                Ok(())
            })();
            if stopped.is_err() { errors.push("throwaway tmux cleanup failed"); }
        }
        let clients_gone = (|| -> Result<()> {
            let end = Instant::now() + Duration::from_secs(10);
            loop {
                let mut all_exited = true;
                for clients in self.clients.values() { all_exited &= clients_exited(clients)?; }
                if all_exited { return Ok(()); }
                ensure!(Instant::now() < end, "owned client process remains");
                thread::sleep(Duration::from_millis(100));
            }
        })();
        if clients_gone.is_err() { errors.push("owned client exit could not be verified"); }
        let ids: Vec<_> = self.registry.borrow().threads.keys().cloned().collect();
        let archive = cleanup_owned(&ids, |id| {
            // Cleanup has a fresh budget, even when the test used all of its own.
            let mut rpc = self.rpc_until(self.clock.now() + Duration::from_secs(15))?;
            let (already_archived, _) = rpc.history(true, true, 100)?;
            if !already_archived.contains_key(id) {
                rpc.request("thread/archive", json!({"threadId":id}))?;
            }
            let (archived, _) = rpc.history(true, true, 100)?;
            ensure!(archived.contains_key(id), "archive not visible in DB-only history");
            rpc.close()?;
            self.registry.borrow_mut().record(json!({"event":"archived","id":id}))
        });
        if archive.is_err() { errors.push("registered thread archival failed (see registry)"); }
        self.record(json!({"event":"cleanup","ids":ids,"ok":errors.is_empty()}))?;
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }
}

fn cleanup_owned(ids: &[String], mut archive: impl FnMut(&str) -> Result<()>) -> Result<()> {
    let mut failed = Vec::new();
    for id in ids {
        if archive(id).is_err() { failed.push(id.clone()); }
    }
    ensure!(failed.is_empty(), "cleanup failed for {}", failed.join(", "));
    Ok(())
}

fn with_cleanup<T>(
    state: &mut T,
    case: impl FnOnce(&mut T) -> Result<()>,
    cleanup: impl FnOnce(&mut T) -> Result<()>,
) -> Result<()> {
    let result = catch_unwind(AssertUnwindSafe(|| case(state)));
    // Preserve both failures; a successful body can never mask failed cleanup.
    if let Err(error) = cleanup(state) {
        bail!("live cleanup FAILED: {error}; case succeeded: {}", matches!(result, Ok(Ok(()))));
    }
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn run(case: impl FnOnce(&mut Harness<'_>) -> Result<()>) {
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let clock = MonotonicClock(Instant::now());
    let mut harness = Harness::new(&clock).unwrap_or_else(|error| panic!("{error}"));
    let result = with_cleanup(&mut harness, |h| h.setup().and_then(|_| case(h)), Harness::cleanup);
    if let Err(error) = result { panic!("live case FAILED: {error}"); }
}

fn unchanged(before: &Value, after: &Value) -> Result<()> {
    let before = before["updatedAt"].as_i64().ok_or_else(|| anyhow::anyhow!("updatedAt missing"))?;
    ensure!(after["updatedAt"].as_i64() == Some(before), "read/attach advanced updatedAt");
    Ok(())
}

#[test]
#[ignore = "opt-in owned Codex threads and tmux -L ccmux-probe; see SPEC §12.11"]
fn live_multi_attach_and_loaded_unloaded_resume() {
    run(|h| {
        let mut creator = h.rpc(Duration::from_secs(90))?;
        let id = creator.materialize(&h.name("resume"))?;
        creator.close()?;
        let before = h.rpc(Duration::from_secs(10))?.read(&id)?;
        ensure!(before["status"]["type"] == "idle", "loaded resume fixture is not idle");
        // Cross a timestamp second, so an accidental touch cannot compare equal.
        thread::sleep(Duration::from_millis(1100));
        let first = h.open(&id)?;
        let second = h.open(&id)?;
        ensure!(h.attached(&first, &id)? && h.attached(&second, &id)?,
            "two concurrent attachments were not maintained");
        unchanged(&before, &h.rpc(Duration::from_secs(10))?.read(&id)?)?;
        h.quit(&first)?;
        ensure!(h.attached(&second, &id)?, "second attachment disappeared");
        h.quit(&second)?;
        h.kill_pane(&first)?;
        h.kill_pane(&second)?;
        h.unloaded_fixture(&id)?;
        let before = h.rpc(Duration::from_secs(10))?.read(&id)?;
        thread::sleep(Duration::from_millis(1100));
        let pane = h.open(&id)?;
        let mut observer = h.rpc(Duration::from_secs(10))?;
        ensure!(observer.loaded(100)?.0.contains(&id), "resume did not load the thread");
        unchanged(&before, &observer.read(&id)?)?;
        observer.close()?;
        h.quit(&pane)?;
        h.record(json!({"event":"multi_attach_resume_pass","id":id}))
    });
}

#[test]
#[ignore = "opt-in owned Codex threads and tmux -L ccmux-probe; see SPEC §12.11"]
fn live_metadata_reads_do_not_load_or_subscribe() {
    run(|h| {
        let mut creator = h.rpc(Duration::from_secs(90))?;
        let id = creator.materialize(&h.name("reads"))?;
        let mut creator = Some(creator);
        for unloaded in [false, true] {
            if unloaded {
                creator.take().unwrap().close()?;
                h.unloaded_fixture(&id)?;
            }
            for method in ["thread/list","thread/loaded/list","thread/read"] {
                // Baseline on a different connection, so only the method under
                // test can acquire this observer's subscription.
                let mut before = h.rpc(Duration::from_secs(10))?;
                ensure!(before.loaded(100)?.0.contains(&id) != unloaded, "fixture load state changed");
                let metadata = before.read(&id)?;
                before.close()?;
                thread::sleep(Duration::from_millis(1100));
                let mut rpc = h.rpc(Duration::from_secs(10))?;
                match method {
                    "thread/list" => { rpc.history(true, false, 100)?; }
                    "thread/loaded/list" => { rpc.loaded(100)?; }
                    _ => { rpc.read(&id)?; }
                }
                let status = rpc.request("thread/unsubscribe", json!({"threadId":id}))?;
                ensure!(status["status"] == if unloaded { "notLoaded" } else { "notSubscribed" },
                    "{method} loaded/subscribed to the owned thread");
                rpc.close()?;
                let mut after = h.rpc(Duration::from_secs(10))?;
                ensure!(after.loaded(100)?.0.contains(&id) != unloaded, "{method} changed loaded membership");
                unchanged(&metadata, &after.read(&id)?)?;
                after.close()?;
            }
        }
        h.record(json!({"event":"read_side_effects_pass","id":id}))
    });
}

#[test]
#[ignore = "opt-in owned Codex threads and tmux -L ccmux-probe; see SPEC §12.11"]
fn live_db_freshness_pagination_and_poll_cost() {
    run(|h| {
        let mut creator = h.rpc(Duration::from_secs(120))?;
        let first = creator.materialize(&h.name("index-a"))?;
        // DB-only must see the write BEFORE any scan-and-repair call.
        ensure!(creator.history(true, false, 1)?.0.contains_key(&first), "DB-only index is stale");
        let second = creator.materialize(&h.name("index-b"))?;
        let (db, pages) = creator.history(true, false, 1)?;
        ensure!(db.contains_key(&first) && db.contains_key(&second) && pages >= 2,
            "history pagination did not cross the owned rows");
        let (scan, _) = creator.history(false, false, 1)?;
        let (again, _) = creator.history(true, false, 1)?;
        for id in [&first, &second] {
            for field in ["id","name","cwd","createdAt","updatedAt","ephemeral","source"] {
                ensure!(db[id].get(field).is_some() && db[id][field] == scan[id][field]
                    && db[id][field] == again[id][field], "DB-only and scan metadata differ");
            }
        }
        let (loaded, loaded_pages) = creator.loaded(1)?;
        ensure!(loaded.contains(&first) && loaded.contains(&second) && loaded_pages >= 2,
            "loaded pagination did not cross the owned rows");
        creator.close()?;
        let mut samples = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            let observation = h.client.poll(chrono::Utc::now().timestamp_millis());
            samples.push(start.elapsed().as_millis());
            ensure!(observation.complete, "production poll incomplete: {:?}", observation.diagnostic);
            ensure!(observation.sessions.iter().any(|s| s.session_id == first)
                && observation.sessions.iter().any(|s| s.session_id == second), "production poll lost owned rows");
        }
        samples.sort_unstable();
        h.record(json!({"event":"index_pagination_poll_pass","history_pages":pages,
            "loaded_pages":loaded_pages,"poll_ms":samples,"p95_ms":samples[4],
            "budget_ms":POLL_TIMEOUT.as_millis()}))
    });
}

fn active_exit(h: &mut Harness<'_>, graceful: bool) -> Result<()> {
    let mut creator = h.rpc(Duration::from_secs(120))?;
    let id = creator.create(&h.name(if graceful { "active-graceful" } else { "active-abrupt" }))?;
    let turn = creator.start_turn(&id,
        "Run /usr/bin/sleep 60 with the shell execution tool in the foreground. Do not background it or shorten it. After it exits, reply exactly done. Do not run other commands or tools.")?;
    loop {
        if creator.events.iter().any(|e| e["params"]["threadId"] == id && e["params"]["turnId"] == turn
            && e["params"]["item"]["type"] == "commandExecution"
            && e["params"]["item"]["command"].as_str().is_some_and(|s| s.contains("/usr/bin/sleep 60")))
        { break; }
        let event = creator.transport.receive().map_err(live_error)?;
        creator.event(event);
    }
    let pane = h.open(&id)?;
    creator.close()?;
    if graceful { h.quit(&pane)?; } else { h.kill_pane(&pane)?; }
    // New observers only read; none resumes or subscribes after the last TUI
    // exits. Assert active first, so completion before exit cannot pass.
    let mut observer = h.rpc(Duration::from_secs(100))?;
    ensure!(observer.turn(&id, &turn)?["status"] == "inProgress", "turn was not active after client exit");
    let completed = observer.completed(&id, &turn)?;
    let items = completed["items"].as_array().ok_or_else(|| anyhow::anyhow!("completion items missing"))?;
    ensure!(items.iter().any(|i| i["type"] == "commandExecution" && i["exitCode"] == 0
        && i["command"].as_str().is_some_and(|s| s.contains("/usr/bin/sleep 60"))),
        "sleep completion evidence missing");
    ensure!(items.iter().any(|i| i["type"] == "agentMessage"
        && i["text"].as_str().is_some_and(|s| s.trim() == "done")), "final done response missing");
    let subscription = observer.request("thread/unsubscribe", json!({"threadId":id}))?;
    ensure!(matches!(subscription["status"].as_str(), Some("notSubscribed" | "notLoaded")),
        "completion observer subscribed");
    observer.close()?;
    h.record(json!({"event":"active_last_client_completion_pass","id":id,"turn":turn,"graceful":graceful}))
}

#[test]
#[ignore = "opt-in owned Codex threads and tmux -L ccmux-probe; see SPEC §12.11"]
fn live_active_turn_survives_graceful_last_client_exit() {
    run(|h| active_exit(h, true));
}

#[test]
#[ignore = "opt-in owned Codex threads and tmux -L ccmux-probe; see SPEC §12.11"]
fn live_active_turn_survives_abrupt_last_client_exit() {
    run(|h| active_exit(h, false));
}

// These tests exercise the harness safety gates without env changes, network,
// tmux, credentials, or ignored cases.
#[test]
fn live_settings_never_fall_back_to_production_names() {
    let production = |key: &str| match key {
        "CCMUX_CODEX_LIVE_TEST" => Some("1".into()),
        "CCMUX_CODEX_URL" => Some("ws://127.0.0.1:8965".into()),
        "CCMUX_CODEX_TOKEN_FILE" => Some("/secret/production".into()),
        _ => None,
    };
    assert!(settings(production).is_err());
    for missing in ["CCMUX_CODEX_LIVE_TEST","CCMUX_CODEX_LIVE_URL","CCMUX_CODEX_LIVE_TOKEN_FILE"] {
        assert!(settings(|key| if key == missing { None } else { match key {
            "CCMUX_CODEX_LIVE_TEST" => Some("1".into()),
            "CCMUX_CODEX_LIVE_URL" => Some("ws://127.0.0.1:8965".into()),
            "CCMUX_CODEX_LIVE_TOKEN_FILE" => Some("/test/token".into()),
            _ => None,
        }}).is_err());
    }
    let config = settings(|key| match key {
        "CCMUX_CODEX_LIVE_TEST" => Some("1".into()),
        "CCMUX_CODEX_LIVE_URL" => Some("ws://127.0.0.1:8965".into()),
        "CCMUX_CODEX_LIVE_TOKEN_FILE" => Some("/test/token".into()),
        _ => None,
    }).ok().unwrap();
    assert_eq!(config.bin, "codex");
    assert_eq!(config.token_file, Path::new("/test/token"));
}

#[test]
fn live_tmux_command_is_isolated_and_existing_servers_are_not_adopted() {
    let command = tmux_command(&["list-sessions"]);
    let args: Vec<_> = command.get_args().map(|s| s.to_str().unwrap()).collect();
    assert_eq!(args, ["-L","ccmux-probe","-f","/dev/null","list-sessions"]);
    for name in ["TMUX","TMUX_PANE","CODEX_REMOTE_TOKEN"] {
        assert!(command.get_envs().any(|(key, value)| key == name && value.is_none()));
    }
    assert!(server_absent(Some(1), b"", b"no server running on /tmp/tmux-1/ccmux-probe"));
    assert!(server_absent(Some(1), b"", b"error connecting to /tmp/tmux-1/ccmux-probe (No such file or directory)"));
    assert!(!server_absent(Some(0), b"operator-session\n", b""));
    assert!(!server_absent(Some(1), b"", b"error connecting to /tmp/tmux-1/ccmux-probe (Permission denied)"));
    assert!(!server_absent(None, b"", b""));
}

#[test]
fn live_registry_rejects_unowned_mutations_and_deletion_even_when_owned() {
    let id = "01a0a609-12a6-7000-8000-000000000001";
    let mut registry = Registry::default();
    assert!(registry.created(id, "operator-thread").is_err());
    registry.created(id, "ccmux-probe-owned").unwrap();
    for method in ["thread/name/set","turn/start","thread/unsubscribe","thread/archive","thread/unarchive"] {
        assert!(registry.authorize(method, &json!({"threadId":"foreign"}), Path::new("/probe")).is_err());
        assert!(registry.authorize(method, &json!({"threadId":id,"name":"ccmux-probe-owned"}),
            Path::new("/probe")).is_ok());
    }
    for method in ["thread/delete","thread/start","thread/resume","turn/interrupt"] {
        assert!(registry.authorize(method, &json!({"threadId":id}), Path::new("/probe")).is_err());
    }
    assert!(registry.authorize("thread/list", &json!({"cwd":"/operator","useStateDbOnly":false}),
        Path::new("/probe")).is_err());
    assert!(registry.authorize("thread/name/set", &json!({"threadId":id,"name":"different"}),
        Path::new("/probe")).is_err());
}

#[test]
fn live_cleanup_attempts_every_owned_id_and_never_passes_after_failure() {
    let ids = vec!["first".into(),"second".into(),"third".into()];
    let mut called = Vec::new();
    let result = cleanup_owned(&ids, |id| {
        called.push(id.to_owned());
        if id == "second" { bail!("archive failed"); }
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(called, ids);
    assert!(cleanup_owned(&ids, |_| Ok(())).is_ok());
}

#[test]
fn live_cleanup_runs_after_success_error_and_panic_and_its_failure_is_fatal() {
    for mode in 0..3 {
        let mut cleaned = false;
        let result = catch_unwind(AssertUnwindSafe(|| with_cleanup(&mut cleaned, |_| {
            match mode {
                0 => Ok(()),
                1 => bail!("case failed"),
                _ => panic!("case panicked"),
            }
        }, |cleaned| { *cleaned = true; Ok(()) })));
        assert!(cleaned);
        match mode {
            0 => assert!(result.unwrap().is_ok()),
            1 => assert!(result.unwrap().is_err()),
            _ => assert!(result.is_err()),
        }
    }
    let result = with_cleanup(&mut (), |_| Ok(()), |_| bail!("archive failed"));
    assert!(result.unwrap_err().to_string().contains("cleanup FAILED"));
}

#[test]
fn live_registry_retains_cleanup_ownership_if_journaling_fails() {
    let id = "01a0a609-12a6-7000-8000-000000000001";
    let mut registry = Registry { journal:Some(File::open("/dev/null").unwrap()), ..Registry::default() };
    assert!(registry.created(id, "ccmux-probe-owned").is_err());
    assert!(registry.threads.contains_key(id));
    assert!(registry.authorize("thread/archive", &json!({"threadId":id}), Path::new("/probe")).is_ok());
}

struct FakeLiveTransport {
    sent: Rc<RefCell<Vec<Value>>>,
    replies: std::collections::VecDeque<Value>,
}

impl Transport for FakeLiveTransport {
    fn send(&mut self, value: Value) -> Result<(), CodexError> {
        self.sent.borrow_mut().push(value);
        Ok(())
    }
    fn receive(&mut self) -> Result<Value, CodexError> {
        self.replies.pop_front().ok_or_else(CodexError::protocol)
    }
    fn close(&mut self) -> Result<(), CodexError> { Ok(()) }
}

#[test]
fn live_rpc_registers_before_mutation_and_never_replies_to_server_requests() {
    let id = "01a0a609-12a6-7000-8000-000000000001";
    let registry = Rc::new(RefCell::new(Registry::default()));
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut rpc = LiveRpc {
        transport:Box::new(FakeLiveTransport { sent:sent.clone(), replies: [
            json!({"id":0,"method":"item/commandExecution/requestApproval","params":{}}),
            json!({"id":0,"result":{"thread":{"id":id}}}),
            json!({"id":1,"result":{}}),
            json!({"id":2,"error":{"code":-1,"message":"synthetic-secret"}}),
        ].into() }),
        registry:registry.clone(), work:"/probe".into(), next_id:0, events:Vec::new(),
    };
    assert!(rpc.request("thread/archive", json!({"threadId":"foreign"})).is_err());
    assert!(rpc.create("not-a-probe").is_err());
    assert!(sent.borrow().is_empty());
    assert_eq!(rpc.create("ccmux-probe-owned").unwrap(), id);
    assert_eq!(registry.borrow().threads[id], "ccmux-probe-owned");
    assert_eq!(sent.borrow().len(), 2); // no reply to the approval
    assert_eq!(sent.borrow()[0]["method"], "thread/start");
    assert_eq!(sent.borrow()[1]["method"], "thread/name/set");
    let error = rpc.request("thread/archive", json!({"threadId":id})).unwrap_err();
    assert!(!format!("{error:?} {error}").contains("synthetic-secret"));
    assert!(rpc.request("thread/delete", json!({"threadId":id})).is_err());
    assert_eq!(sent.borrow().len(), 3);
}

#[test]
fn live_process_identity_parsing_handles_parentheses_and_records_start_time() {
    let mut fields = vec!["0"; 20];
    fields[0] = "S";
    fields[1] = "123";
    fields[19] = "987654";
    let raw = format!("456 (codex ) helper) {}", fields.join(" "));
    let stamp = parse_process_stamp(456, &raw).unwrap();
    assert_eq!((stamp.pid, stamp.parent, stamp.started, stamp.zombie), (456,123,987654,false));
    fields[0] = "Z";
    assert!(parse_process_stamp(456, &format!("456 (codex) {}", fields.join(" "))).unwrap().zombie);
    assert!(parse_process_stamp(456, "456 (short) S 123").is_err());
}
