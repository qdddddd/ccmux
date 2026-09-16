//! Opt-in upgrade gates. All mutations are test-only and registry-checked.
//! Run only this module's ignored cases; SPEC §12.11 gives the invocation.

use super::*;
use anyhow::{Result, bail, ensure};
use std::{
    cell::{Cell, RefCell},
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
    // Preserve the category without the production poll's fixed 1000 ms text.
    if error.diagnostic.kind == CodexFailureKind::Timeout {
        CodexError::new(CodexFailureKind::Timeout, "live RPC deadline expired").into()
    } else { error.into() }
}

const SOCKET: &str = "ccmux-probe";
const MODEL: &str = "gpt-5.6-luna";
const CASE_TIMEOUT: Duration = Duration::from_secs(360);
const TURN_TIMEOUT: Duration = Duration::from_secs(120);
// Keep the sleep long enough for /quit and the active witness. Give command
// exit and the final model round separate budgets, including outer deadlines.
const COMMAND_EXIT_TIMEOUT: Duration = Duration::from_secs(60 + 30);
const FINAL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(180);
const ACTIVE_OBSERVER_TIMEOUT: Duration = Duration::from_secs(
    15 + COMMAND_EXIT_TIMEOUT.as_secs() + FINAL_RESPONSE_TIMEOUT.as_secs() + 30);
const ACTIVE_CASE_TIMEOUT: Duration = Duration::from_secs(
    CASE_TIMEOUT.as_secs() + ACTIVE_OBSERVER_TIMEOUT.as_secs());
static SERIAL: Mutex<()> = Mutex::new(());

// All post-mutation observations share a monotonic deadline. The snapshot is
// retained across retries, including a final socket timeout, for diagnosis.
#[derive(Clone)]
struct Waiter<'a> {
    deadline: Deadline<'a>,
    pause: &'a dyn Fn(Duration),
    token: Rc<str>,
    io_end: Rc<Cell<Duration>>,
}

struct IoScope { end: Rc<Cell<Duration>>, previous: Duration }

impl Drop for IoScope {
    fn drop(&mut self) { self.end.set(self.previous); }
}

impl<'a> Waiter<'a> {
    fn new(deadline: Deadline<'a>, token: &str) -> Self {
        Self::with_pause(deadline, token, &thread::sleep)
    }

    fn with_pause(deadline: Deadline<'a>, token: &str, pause: &'a dyn Fn(Duration)) -> Self {
        Self { deadline, pause, token:token.into(), io_end:Rc::new(Cell::new(deadline.end)) }
    }

    fn scoped(&self, budget: Duration) -> Self {
        Self { deadline:Deadline { clock:self.deadline.clock,
            end:self.deadline.end.min(self.deadline.clock.now() + budget) }, ..self.clone() }
    }

    fn describe(&self, value: &Value) -> String {
        fn scrub(value: &Value, token: &str) -> Value {
            let text = |s: &str| {
                let replaced = if token.is_empty() { s.to_owned() } else { s.replace(token, "[redacted]") };
                let clean = clean_text(&replaced);
                let clean = if token.is_empty() { clean } else { clean.replace(token, "[redacted]") };
                // Keep both ends of large fields (notably the TUI header and
                // prompt), leaving room for sibling status/error fields.
                if clean.chars().count() <= 1000 { clean } else {
                    let head: String = clean.chars().take(500).collect();
                    let tail: String = clean.chars().rev().take(500).collect::<Vec<_>>().into_iter().rev().collect();
                    format!("{head}...[truncated]...{tail}")
                }
            };
            match value {
                Value::String(s) => json!(text(s)),
                Value::Array(items) => Value::Array(items.iter().map(|v| scrub(v, token)).collect()),
                Value::Object(fields) => Value::Object(fields.iter().map(|(key, value)| {
                    let sensitive = matches!(key.to_ascii_lowercase().replace(['_','-'], "").as_str(),
                        "authorization" | "token" | "accesstoken" | "apikey" | "secret");
                    (text(key), if sensitive { json!("[redacted]") } else { scrub(value, token) })
                }).collect()),
                _ => value.clone(),
            }
        }
        // Redact BEFORE JSON escaping and truncation (tokens may contain quotes).
        let rendered = scrub(value, &self.token).to_string();
        let mut chars = rendered.chars();
        let mut bounded: String = chars.by_ref().take(4096).collect();
        if chars.next().is_some() { bounded.push_str("...[truncated]"); }
        bounded
    }

    fn until<T>(&self, condition: &str, mut observe: impl FnMut() -> Result<(Option<T>, Value)>) -> Result<T> {
        let previous = self.io_end.replace(self.io_end.get().min(self.deadline.end));
        let _scope = IoScope { end:self.io_end.clone(), previous };
        let mut last = json!("not observed yet");
        loop {
            if self.deadline.remaining().is_err() {
                bail!("timed out waiting for {condition}; last observed: {}", self.describe(&last));
            }
            let result = match observe() {
                Ok((result, snapshot)) => { last = snapshot; result }
                Err(error) => {
                    let timed_out = self.deadline.remaining().is_err() || error.downcast_ref::<CodexError>()
                        .is_some_and(|e| e.diagnostic.kind == CodexFailureKind::Timeout);
                    let details = self.describe(&json!({"observation":last,"read_error":error.to_string()}));
                    if timed_out { bail!("timed out waiting for {condition}; last observed: {details}"); }
                    bail!("while waiting for {condition}; last observed: {details}");
                }
            };
            if self.deadline.remaining().is_err() {
                bail!("timed out waiting for {condition}; last observed: {}", self.describe(&last));
            }
            if let Some(result) = result { return Ok(result); }
            (self.pause)(self.deadline.remaining()?.min(Duration::from_millis(250)));
        }
    }

    fn turn_failure(&self, id: &str, turn: &str, row: &Value) -> Result<()> {
        if matches!(row["status"].as_str(), Some("failed" | "interrupted")) {
            bail!("owned turn {turn} on {id} {}; error: {}", row["status"],
                self.describe(&row["error"]));
        }
        Ok(())
    }
}

fn turn_visible(row: &Value) -> bool {
    matches!(row["status"].as_str(), Some("inProgress" | "completed"))
}

fn thread_snapshot(id: &str, row: &Value, loaded: bool) -> Value {
    json!({"id":id,"loaded":loaded,"status":row.get("status"),
        "updatedAt":row.get("updatedAt"),"name":row.get("name")})
}

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

struct CreatedTurn {
    end: Duration,
    command_underway: bool,
}

// Issued only after /status verified this launch and the owned TUI remains
// alive. An ordinary close is forbidden while a creator has a pending turn.
struct AttachedTui { pane: String, thread: String }

/// Separate from the production Method/Rpc: no mutation method is added to
/// the lister. Every request other than the private creation path is guarded.
struct LiveRpc<'a> {
    transport: Option<Box<dyn Transport + 'a>>,
    registry: Rc<RefCell<Registry>>,
    work: PathBuf,
    next_id: u64,
    events: Vec<Value>,
    creator: bool,
    turns: BTreeMap<(String, String), CreatedTurn>,
    wait: Waiter<'a>,
}

impl LiveRpc<'_> {
    fn send(&mut self, value: Value) -> Result<(), CodexError> {
        let transport = self.transport.as_mut().ok_or_else(CodexError::protocol)?;
        transport.set_deadline(self.wait.io_end.get());
        transport.send(value)
    }

    fn receive(&mut self) -> Result<Value, CodexError> {
        let transport = self.transport.as_mut().ok_or_else(CodexError::protocol)?;
        transport.set_deadline(self.wait.io_end.get());
        transport.receive()
    }

    fn exchange(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"id":id,"method":method,"params":params}))
            .map_err(|error| live_error(error).context(format!("sending live RPC {method}")))?;
        loop {
            let value = self.receive()
                .map_err(|error| live_error(error).context(format!("receiving live RPC {method}")))?;
            if value.get("method").is_some() {
                self.event(value);
                continue;
            }
            ensure!(value["id"] == id, "live RPC response id mismatch");
            ensure!(value.get("error").is_none(), "live RPC {method} rejected: {}",
                self.wait.describe(&value["error"]));
            return value.get("result").cloned().ok_or_else(|| anyhow::anyhow!("live RPC missing result"));
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        ensure!(method != "turn/start" || self.creator, "observer cannot start turns");
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
        ensure!(self.creator, "observer cannot create threads");
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
        ensure!(self.creator && self.turns.is_empty(), "turn needs an idle creator connection");
        let wait = self.wait.scoped(TURN_TIMEOUT);
        let value = wait.until(&format!("turn/start acknowledgement for thread {id}"), || {
            let value = self.request("turn/start", json!({
                "threadId":id, "input":[{"type":"text","text":prompt}], "model":MODEL, "effort":"low"
            }))?;
            Ok((Some(value), json!({"thread":id})))
        })?;
        let turn = value["turn"]["id"].as_str().ok_or_else(|| anyhow::anyhow!("missing turn id"))?.to_owned();
        self.turns.insert((id.into(), turn.clone()), CreatedTurn {
            end:wait.deadline.end, command_underway:false,
        });
        self.registry.borrow_mut().record(json!({"event":"creator_turn_started","id":id,"turn":turn}))?;
        Ok(turn)
    }

    fn created_progress(&mut self, id: &str, turn: &str, command: bool) -> Result<Value> {
        let key = (id.to_owned(), turn.to_owned());
        let owned = self.turns.get(&key).ok_or_else(|| anyhow::anyhow!("turn not owned by this connection"))?;
        let mut wait = self.wait.scoped(if command { Duration::from_secs(45) } else { TURN_TIMEOUT });
        wait.deadline.end = wait.deadline.end.min(owned.end);
        // receive() already blocks. Do not sleep between notification frames.
        let no_pause = |_| {};
        let wait = Waiter { pause:&no_pause, ..wait };
        let condition = if command { "in-progress sleep command notification" } else { "creator turn/completed notification" };
        let mut receive = false;
        wait.until(&format!("{condition} (thread {id}, turn {turn})"), || {
            if receive {
                let event = self.receive().map_err(live_error)?;
                ensure!(event.get("method").is_some(), "unexpected response while awaiting creator notification");
                self.event(event);
            }
            receive = true;
            let terminal = self.events.iter().rev().find(|e| e["method"] == "turn/completed"
                && e["params"]["threadId"] == id && e["params"]["turn"]["id"] == turn)
                .map(|e| e["params"]["turn"].clone()).unwrap_or(Value::Null);
            if matches!(terminal["status"].as_str(), Some("completed" | "failed" | "interrupted")) {
                self.turns.remove(&key);
                self.registry.borrow_mut().record(json!({"event":"creator_turn_terminal",
                    "id":id,"turn":turn,"status":terminal["status"]}))?;
                wait.turn_failure(id, turn, &terminal)?;
                ensure!(!command, "turn completed before creator handoff (thread {id}, turn {turn})");
                return Ok((Some(terminal.clone()), json!({"terminal":terminal})));
            }
            let underway = self.events.iter().rev().find(|e| sleep_underway(e, id, turn)).cloned();
            if command && let Some(event) = &underway {
                self.turns.get_mut(&key).unwrap().command_underway = true;
                self.registry.borrow_mut().record(json!({"event":"creator_command_underway",
                    "id":id,"turn":turn,"item":event["params"]["item"]["id"],"status":"inProgress"}))?;
                return Ok((Some(event.clone()), json!({"command":event})));
            }
            Ok((None, json!({"thread":id,"turn":turn,"terminal_notification":terminal,
                "command_underway":underway.is_some(),"creator_connected":true})))
        })
    }

    fn handoff(&mut self, id: &str, turn: &str, tui: &AttachedTui) -> Result<()> {
        let key = (id.to_owned(), turn.to_owned());
        ensure!(tui.thread == id && self.turns.len() == 1
            && self.turns.get(&key).is_some_and(|t| t.command_underway),
            "creator handoff requires its in-progress command and matching attached TUI");
        self.registry.borrow_mut().record(json!({"event":"creator_handoff",
            "id":id,"turn":turn,"pane":tui.pane,"command_status":"inProgress"}))?;
        self.turns.remove(&key);
        self.close()
    }

    fn read(&mut self, id: &str) -> Result<Value> {
        Ok(self.request("thread/read", json!({"threadId":id,"includeTurns":false}))?["thread"].clone())
    }

    fn turn_once(&mut self, id: &str, turn: &str) -> Result<Value> {
        let result = self.request("thread/turns/list", json!({
            "threadId":id,"limit":1,"sortDirection":"desc","itemsView":"full"
        }))?;
        // The index can lag turn/start, and a visible row can lack status.
        // Neither is a terminal result for the newly requested turn.
        Ok(result["data"].as_array().and_then(|rows| rows.iter().find(|v| v["id"] == turn))
            .cloned().unwrap_or(Value::Null))
    }

    fn wait_turn(
        &mut self, id: &str, turn: &str, condition: &str, budget: Duration,
        ready: impl Fn(&Value) -> bool,
    ) -> Result<Value> {
        let wait = self.wait.scoped(budget);
        wait.until(&format!("{condition} (thread {id}, turn {turn})"), || {
            let row = self.turn_once(id, turn)?;
            wait.turn_failure(id, turn, &row)?;
            let snapshot = json!({"thread":id,"turn":turn,"row":row});
            Ok((ready(&row).then_some(row), snapshot))
        })
    }

    fn turn(&mut self, id: &str, turn: &str) -> Result<Value> {
        self.wait_turn(id, turn, "turn visibility", Duration::from_secs(15), turn_visible)
    }

    fn completed(&mut self, id: &str, turn: &str) -> Result<Value> {
        self.wait_turn(id, turn, "turn completion", Duration::from_secs(90),
            |row| row["status"] == "completed")
    }

    fn active_completed(&mut self, id: &str, turn: &str) -> Result<Value> {
        self.wait_turn(id, turn, "successful sleep command completion",
            COMMAND_EXIT_TIMEOUT, sleep_completed)?;
        // Start the model's budget only once command completion is observed.
        // Repeated completed-command rows must not keep renewing this wait.
        self.wait_turn(id, turn, "completed turn with successful sleep and final done",
            FINAL_RESPONSE_TIMEOUT, active_completion_ready)
    }

    fn thread_state(&mut self, id: &str, status: &str, loaded: bool) -> Result<Value> {
        let wait = self.wait.scoped(Duration::from_secs(15));
        wait.until(&format!("thread {id} status {status}, loaded={loaded}"), || {
            let membership = self.loaded(100)?.0.contains(id);
            let row = self.read(id)?;
            let snapshot = thread_snapshot(id, &row, membership);
            Ok(((membership == loaded && row["status"]["type"] == status).then_some(row), snapshot))
        })
    }

    fn history_ids(&mut self, db: bool, archived: bool, limit: u64, ids: &[&str])
        -> Result<(BTreeMap<String, Value>, usize)>
    {
        let wait = self.wait.scoped(Duration::from_secs(15));
        wait.until(&format!("history visibility db_only={db}, archived={archived}, ids={ids:?}"), || {
            let (rows, pages) = self.history(db, archived, limit)?;
            let snapshot = json!({"pages":pages,"owned_rows":ids.iter()
                .map(|id| json!({"id":id,"row":rows.get(*id)})).collect::<Vec<_>>()});
            let ready = ids.iter().all(|id| rows.contains_key(*id));
            Ok((ready.then_some((rows, pages)), snapshot))
        })
    }

    fn loaded_ids(&mut self, limit: u64, ids: &[&str]) -> Result<(BTreeSet<String>, usize)> {
        let wait = self.wait.scoped(Duration::from_secs(15));
        wait.until(&format!("loaded pagination containing {ids:?}"), || {
            let (rows, pages) = self.loaded(limit)?;
            let snapshot = json!({"count":rows.len(),"pages":pages,
                "membership":ids.iter().map(|id| json!({"id":id,"loaded":rows.contains(*id)})).collect::<Vec<_>>()});
            let ready = ids.iter().all(|id| rows.contains(*id));
            Ok((ready.then_some((rows, pages)), snapshot))
        })
    }

    fn materialize(&mut self, name: &str) -> Result<String> {
        let id = self.create(name)?;
        let turn = self.start_turn(&id, "Reply exactly indexed. Do not use tools.")?;
        // Keep this SAME subscribed connection until an authoritative terminal
        // notification. Read history only afterwards; an immediate rollout
        // snapshot is not our creator's lifecycle signal.
        self.created_progress(&id, &turn, false)?;
        self.thread_state(&id, "idle", true)?;
        self.history_ids(true, false, 100, &[&id])?;
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

    fn close(&mut self) -> Result<()> {
        ensure!(self.turns.is_empty(), "creator still owns a pending turn; wait for terminal or explicit handoff");
        let mut transport = self.transport.take().ok_or_else(|| anyhow::anyhow!("live RPC already closed"))?;
        transport.set_deadline(self.wait.io_end.get());
        let result = transport.close();
        drop(transport); // close frame AND socket gone before last-client observation
        result.map_err(|error| live_error(error).context("closing live RPC client"))?;
        self.registry.borrow_mut().record(json!({"event":"rpc_closed","creator":self.creator}))?;
        Ok(())
    }
}

fn sleep_underway(event: &Value, id: &str, turn: &str) -> bool {
    event.get("id").is_none() && event["method"] == "item/started"
        && event["params"]["threadId"] == id && event["params"]["turnId"] == turn
        && event["params"]["item"]["type"] == "commandExecution"
        && event["params"]["item"]["status"] == "inProgress"
        && event["params"]["item"]["command"].as_str().is_some_and(|s| s.contains("/usr/bin/sleep 60"))
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

fn composer_ready(snapshot: &Value) -> bool {
    let screen = snapshot["screen"].as_str().unwrap_or("");
    snapshot["latch"] == "" && !screen.contains("[ccmux] Codex attach exited")
        && !screen.contains("Resuming session") && !screen.contains("model:     loading")
        && screen.lines().rev().find(|line| line.trim_start().starts_with('›'))
            .is_some_and(|line| !line.contains("Error:"))
}

fn command_visible(snapshot: &Value, text: &str) -> bool {
    snapshot["latch"] == "" && snapshot["screen"].as_str().unwrap_or("").lines()
        .rev().find_map(|line| line.trim_start().strip_prefix('›'))
        .is_some_and(|line| line.trim() == text)
}

fn status_identifies(snapshot: &Value, id: &str) -> bool {
    let screen = snapshot["screen"].as_str().unwrap_or("");
    snapshot["latch"] == "" && !screen.contains("[ccmux] Codex attach exited")
        && screen.rsplit_once("Session:").is_some_and(|(_, value)| {
            value.chars().filter(|c| !c.is_whitespace() && *c != '│')
                .take(100).collect::<String>().starts_with(id)
        })
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
    launches: BTreeMap<String, String>,
    server_started: bool,
    _lock: RunLock,
}

impl<'a> Harness<'a> {
    fn new(clock: &'a dyn Clock, budget: Duration) -> Result<Self> {
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
            config, client, clock, end:clock.now() + budget, work, run,
            registry:Rc::new(RefCell::new(Registry { journal:Some(journal), ..Registry::default() })),
            panes:BTreeSet::new(), clients:BTreeMap::new(), launches:BTreeMap::new(),
            server_started:false, _lock:guard,
        })
    }

    fn tmux(&self, args: &[&str]) -> Result<String> {
        let result = output(tmux_command(args))?;
        ensure!(result.status.success(), "throwaway tmux {} failed: {}", args[0],
            self.wait(Duration::from_secs(1)).describe(&json!({
                "exit_code":result.status.code(),"stderr":String::from_utf8_lossy(&result.stderr)
            })));
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
        let keepalive = self.end.saturating_sub(self.clock.now()).as_secs().saturating_add(60).to_string();
        let pane = self.tmux(&["new-session","-d","-s",&self.run,"-x","180","-y","48",
            "-P","-F","#{pane_id}","/usr/bin/sleep",&keepalive])?;
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
            transport:Some(transport), registry:self.registry.clone(), work:self.work.clone(), next_id:0, events:Vec::new(),
            creator:false, turns:BTreeMap::new(), wait:Waiter::new(deadline, &self.client.prepared.token),
        };
        let info = rpc.request("initialize", json!({
            "clientInfo":{"name":"ccmux_probe","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":true}
        }))?;
        let identity = info["userAgent"].as_str().ok_or_else(|| anyhow::anyhow!("server identity missing"))?;
        self.registry.borrow_mut().record(json!({
            "event":"server_version","value":self.client.redact(identity)
        }))?;
        rpc.send(json!({"method":"initialized","params":{}})).map_err(live_error)?;
        Ok(rpc)
    }

    fn rpc(&self, budget: Duration) -> Result<LiveRpc<'a>> {
        self.rpc_until(self.end.min(self.clock.now() + budget))
    }

    fn creator(&self) -> Result<LiveRpc<'a>> {
        // One connection for the case's creation work; each turn gets its own
        // full lifecycle deadline, including start and notification delivery.
        let mut rpc = self.rpc_until(self.end)?;
        rpc.creator = true;
        Ok(rpc)
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

    fn wait(&self, budget: Duration) -> Waiter<'a> {
        Waiter::new(Deadline { clock:self.clock,
            end:self.end.min(self.clock.now() + budget) }, &self.client.prepared.token)
    }

    fn snapshot(&self, pane: &str) -> Result<Value> {
        let screen = self.screen(pane)?;
        let latch = self.tmux(&["show-options","-pqv","-t",pane,"@ccmux_detached"])?;
        Ok(json!({"pane":pane,"latch":latch.trim(),"screen":screen.trim_end()}))
    }

    fn keys(&self, pane: &str, text: &str) -> Result<()> {
        let wait = self.wait(Duration::from_secs(30));
        wait.until(&format!("Codex input prompt in pane {pane} before {text}"), || {
            let snapshot = self.snapshot(pane)?;
            ensure!(snapshot["latch"] != "1", "Codex TUI parked: {}", wait.describe(&snapshot));
            Ok((composer_ready(&snapshot).then_some(()), snapshot))
        })?;
        self.tmux(&["send-keys","-t",pane,"-l",text])?;
        // Sending bytes is not proof the TUI has rendered/accepted them.
        // Send Enter once, only after the composer displays our command.
        wait.scoped(Duration::from_secs(10)).until(&format!("{text} in pane {pane} composer"), || {
            let snapshot = self.snapshot(pane)?;
            Ok((command_visible(&snapshot, text).then_some(()), snapshot))
        })?;
        self.tmux(&["send-keys","-t",pane,"Enter"])?;
        Ok(())
    }

    fn open(&mut self, id: &str) -> Result<String> {
        ensure!(self.registry.borrow().threads.contains_key(id), "probe attach ownership guard");
        let command = crate::agents::codex_probe_attach_cmd(id, &self.config)?;
        let pane = self.tmux(&["new-window","-d","-t",&format!("={}:", self.run),
            "-P","-F","#{pane_id}","/bin/sh","-c",&command])?.trim().to_owned();
        self.remember_pane(&pane)?;
        self.check_pane(&pane)?;
        // Detached new windows need not inherit new-session's dimensions.
        self.tmux(&["set-option","-w","-t",&pane,"window-size","manual"])?;
        self.tmux(&["resize-window","-t",&pane,"-x","180","-y","48"])?;
        self.keys(&pane, "/status")?;
        self.attached(&pane, id)?;
        let root = self.tmux(&["display-message","-p","-t",&pane,"#{pane_pid}"])?;
        let clients = pane_clients(root.trim().parse()?)?;
        self.registry.borrow_mut().record(json!({"event":"clients","pane":pane,
            "processes":clients.iter().map(|p| json!({"pid":p.pid,"started":p.started})).collect::<Vec<_>>()}))?;
        self.clients.insert(pane.clone(), clients);
        self.launches.insert(pane.clone(), id.into());
        Ok(pane)
    }

    fn attached(&self, pane: &str, id: &str) -> Result<()> {
        let wait = self.wait(Duration::from_secs(15));
        wait.until(&format!("/status identifying thread {id} in pane {pane}"), || {
            let snapshot = self.snapshot(pane)?;
            ensure!(snapshot["latch"] != "1", "Codex TUI parked: {}", wait.describe(&snapshot));
            Ok((status_identifies(&snapshot, id).then_some(()), snapshot))
        })
    }

    fn handoff_target(&self, pane: &str, id: &str) -> Result<AttachedTui> {
        // open() already proved /status identity before turn/start. The sleep
        // task sends no identity-changing TUI command. Recheck its pane,
        // detached latch and recorded native descendants just before handoff.
        ensure!(self.launches.get(pane).is_some_and(|launch| launch == id),
            "handoff TUI launch identity was not verified");
        let snapshot = self.snapshot(pane)?;
        ensure!(snapshot["latch"] == "" && !clients_exited(&self.clients[pane])?,
            "handoff TUI exited: {}", self.wait(Duration::from_secs(1)).describe(&snapshot));
        Ok(AttachedTui { pane:pane.into(), thread:id.into() })
    }

    fn quit(&self, pane: &str) -> Result<()> {
        self.keys(pane, "/quit")?;
        self.wait(Duration::from_secs(15)).until(&format!("pane {pane} parked rc=0 and its clients exited"), || {
            let mut snapshot = self.snapshot(pane)?;
            snapshot["clients_exited"] = json!(clients_exited(&self.clients[pane])?);
            let screen = snapshot["screen"].as_str().unwrap_or("");
            let ready = snapshot["latch"] == "1" && screen.contains("[ccmux] Codex attach exited (rc=0)")
                && screen.contains("$(cat ") && snapshot["clients_exited"] == true;
            Ok((ready.then_some(()), snapshot))
        })
    }

    fn kill_pane(&mut self, pane: &str) -> Result<()> {
        self.check_pane(pane)?;
        self.tmux(&["kill-pane","-t",pane])?;
        self.panes.remove(pane);
        self.wait(Duration::from_secs(10)).until(&format!("pane {pane} and its clients gone after kill-pane"), || {
            let panes = self.tmux(&["list-panes","-a","-F","#{pane_id}"])?;
            let gone = !panes.lines().any(|id| id == pane);
            let clients_gone = self.clients.get(pane).map(|c| clients_exited(c)).transpose()?.unwrap_or(true);
            let snapshot = json!({"pane":pane,"pane_gone":gone,"clients_exited":clients_gone});
            Ok(((gone && clients_gone).then_some(()), snapshot))
        })
    }

    fn unloaded_fixture(&self, id: &str) -> Result<()> {
        // Wait for each mutation separately; issuing unarchive while archive
        // is still becoming visible can race the fixture's own setup.
        let mut rpc = self.rpc(Duration::from_secs(60))?;
        rpc.request("thread/archive", json!({"threadId":id}))?;
        rpc.history_ids(true, true, 100, &[id])?;
        let wait = rpc.wait.scoped(Duration::from_secs(15));
        wait.until(&format!("thread {id} absent from loaded list after archive"), || {
            let loaded = rpc.loaded(100)?.0.contains(id);
            Ok(((!loaded).then_some(()), json!({"id":id,"loaded":loaded})))
        })?;
        rpc.request("thread/unarchive", json!({"threadId":id}))?;
        rpc.history_ids(true, false, 100, &[id])?;
        rpc.thread_state(id, "notLoaded", false)?;
        rpc.close()
    }

    fn cleanup(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        let cleanup_wait = |budget| Waiter::new(Deadline {
            clock:self.clock, end:self.clock.now() + budget }, &self.client.prepared.token);
        if self.server_started {
            let stopped = (|| -> Result<()> {
                let existing = output(tmux_command(&["list-sessions","-F","#{session_name}"]))?;
                if server_absent(existing.status.code(), &existing.stdout, &existing.stderr) { return Ok(()); }
                ensure!(existing.status.success() && String::from_utf8_lossy(&existing.stdout).trim() == self.run,
                    "cleanup refuses a foreign session on ccmux-probe");
                let panes = self.tmux(&["list-panes","-a","-F","#{pane_id}"])?;
                ensure!(panes.lines().all(|id| self.panes.contains(id)), "cleanup refuses an unregistered pane");
                self.tmux(&["kill-server"])?;
                cleanup_wait(Duration::from_secs(10)).until("throwaway tmux server exit after kill-server", || {
                    let after = output(tmux_command(&["list-sessions"]))?;
                    let gone = server_absent(after.status.code(), &after.stdout, &after.stderr);
                    Ok((gone.then_some(()), json!({"exit_code":after.status.code(),
                        "stdout":String::from_utf8_lossy(&after.stdout),"stderr":String::from_utf8_lossy(&after.stderr)})))
                })
            })();
            if let Err(error) = stopped { errors.push(error.to_string()); }
        }
        let clients_gone = cleanup_wait(Duration::from_secs(10)).until("all owned client processes to exit", || {
            let mut remaining = Vec::new();
            for clients in self.clients.values() {
                for stamp in clients {
                    if !clients_exited(&[*stamp])? { remaining.push(stamp.pid); }
                }
            }
            Ok((remaining.is_empty().then_some(()), json!({"remaining_pids":remaining})))
        });
        if let Err(error) = clients_gone { errors.push(error.to_string()); }
        let ids: Vec<_> = self.registry.borrow().threads.keys().cloned().collect();
        // Registered probe threads are disposable. Cleanup deliberately may
        // archive a live/unknown-state turn and interrupt it, so a failed or
        // hung experiment leaves no running probe work behind. Do not wait
        // indefinitely for terminal state. These teardown aborts are NEVER
        // lifecycle evidence; with_cleanup preserves the original failure.
        let archive = cleanup_owned(&ids, |id| {
            // Cleanup has a fresh budget, even when the test used all of its own.
            let mut rpc = self.rpc_until(self.clock.now() + Duration::from_secs(30))?;
            let (already_archived, _) = rpc.history(true, true, 100)?;
            if !already_archived.contains_key(id) {
                rpc.request("thread/archive", json!({"threadId":id}))?;
            }
            rpc.history_ids(true, true, 100, &[id])?;
            rpc.close()?;
            self.registry.borrow_mut().record(json!({"event":"archived","id":id}))
        });
        if let Err(error) = archive { errors.push(error.to_string()); }
        self.record(json!({"event":"cleanup","ids":ids,"ok":errors.is_empty()}))?;
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }

}

fn cleanup_owned(ids: &[String], mut archive: impl FnMut(&str) -> Result<()>) -> Result<()> {
    let mut failed = Vec::new();
    for id in ids {
        if let Err(error) = archive(id) { failed.push(format!("{id}: {error}")); }
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
        let body = match &result {
            Ok(Ok(())) => "succeeded".into(),
            Ok(Err(error)) => error.to_string(),
            Err(_) => "panicked".into(),
        };
        bail!("live cleanup FAILED: {error}; case: {body}");
    }
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn run(budget: Duration, case: impl FnOnce(&mut Harness<'_>) -> Result<()>) {
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let clock = MonotonicClock(Instant::now());
    let mut harness = Harness::new(&clock, budget).unwrap_or_else(|error| panic!("{error}"));
    let result = with_cleanup(&mut harness, |h| h.setup().and_then(|_| case(h)), Harness::cleanup);
    if let Err(error) = result { panic!("live case FAILED: {error}"); }
}

fn unchanged(before: &Value, after: &Value) -> Result<()> {
    let before = before["updatedAt"].as_i64().ok_or_else(|| anyhow::anyhow!("updatedAt missing"))?;
    ensure!(after["updatedAt"].as_i64() == Some(before),
        "read/attach changed updatedAt: expected {before}, observed {:?}", after["updatedAt"].as_i64());
    Ok(())
}

#[test]
#[ignore = "opt-in owned Codex threads and tmux -L ccmux-probe; see SPEC §12.11"]
fn live_multi_attach_and_loaded_unloaded_resume() {
    run(CASE_TIMEOUT, |h| {
        let mut creator = h.creator()?;
        let id = creator.materialize(&h.name("resume"))?;
        creator.close()?;
        let before = h.rpc(Duration::from_secs(30))?.thread_state(&id, "idle", true)?;
        // Cross a timestamp second, so an accidental touch cannot compare equal.
        thread::sleep(Duration::from_millis(1100));
        let first = h.open(&id)?;
        let second = h.open(&id)?;
        h.attached(&first, &id)?;
        h.attached(&second, &id)?;
        unchanged(&before, &h.rpc(Duration::from_secs(30))?.thread_state(&id, "idle", true)?)?;
        h.quit(&first)?;
        h.attached(&second, &id)?;
        h.quit(&second)?;
        h.kill_pane(&first)?;
        h.kill_pane(&second)?;
        h.unloaded_fixture(&id)?;
        let before = h.rpc(Duration::from_secs(30))?.thread_state(&id, "notLoaded", false)?;
        thread::sleep(Duration::from_millis(1100));
        let pane = h.open(&id)?;
        let mut observer = h.rpc(Duration::from_secs(30))?;
        unchanged(&before, &observer.thread_state(&id, "idle", true)?)?;
        observer.close()?;
        h.quit(&pane)?;
        h.record(json!({"event":"multi_attach_resume_pass","id":id}))
    });
}

#[test]
#[ignore = "opt-in owned Codex threads and tmux -L ccmux-probe; see SPEC §12.11"]
fn live_metadata_reads_do_not_load_or_subscribe() {
    run(CASE_TIMEOUT, |h| {
        let mut creator = h.creator()?;
        let id = creator.materialize(&h.name("reads"))?;
        let mut creator = Some(creator);
        for unloaded in [false, true] {
            if unloaded {
                creator.take().unwrap().close()?;
                h.unloaded_fixture(&id)?;
            }
            for method in ["thread/list","thread/loaded/list","thread/read"] {
                // Settle the setup mutation on a DIFFERENT connection first.
                let mut before = h.rpc(Duration::from_secs(30))?;
                let metadata = before.thread_state(&id, if unloaded { "notLoaded" } else { "idle" }, !unloaded)?;
                before.close()?;
                thread::sleep(Duration::from_millis(1100));
                let mut rpc = h.rpc(Duration::from_secs(10))?;
                match method {
                    "thread/list" => { rpc.history(true, false, 100)?; }
                    "thread/loaded/list" => { rpc.loaded(100)?; }
                    _ => { rpc.read(&id)?; }
                }
                let status = rpc.request("thread/unsubscribe", json!({"threadId":id}))?;
                // Do NOT retry this check: a retry could consume a subscription
                // created by the read and turn a real regression into a pass.
                ensure!(status["status"] == if unloaded { "notLoaded" } else { "notSubscribed" },
                    "{method} loaded/subscribed to {id}: {}", rpc.wait.describe(&status));
                rpc.close()?;
                let mut after = h.rpc(Duration::from_secs(10))?;
                let loaded = after.loaded(100)?.0.contains(&id);
                let row = after.read(&id)?;
                ensure!(loaded != unloaded, "{method} changed loaded membership: {}",
                    after.wait.describe(&thread_snapshot(&id, &row, loaded)));
                unchanged(&metadata, &row)?;
                after.close()?;
            }
        }
        h.record(json!({"event":"read_side_effects_pass","id":id}))
    });
}

const METADATA_FIELDS: [&str; 7] = ["id","name","cwd","createdAt","updatedAt","ephemeral","source"];

fn metadata_matches(left: &BTreeMap<String, Value>, right: &BTreeMap<String, Value>, ids: &[&str]) -> bool {
    ids.iter().all(|id| METADATA_FIELDS.iter().all(|field| {
        left.get(*id).and_then(|row| row.get(*field))
            .is_some_and(|value| right.get(*id).and_then(|row| row.get(*field)) == Some(value))
    }))
}

fn owned_metadata(rows: &BTreeMap<String, Value>, ids: &[&str]) -> Value {
    json!(ids.iter().map(|id| {
        let fields: serde_json::Map<_, _> = METADATA_FIELDS.iter()
            .map(|key| ((*key).into(), rows.get(*id).and_then(|row| row.get(*key)).cloned().unwrap_or(Value::Null)))
            .collect();
        json!({"id":id,"metadata":fields})
    }).collect::<Vec<_>>())
}

#[test]
#[ignore = "opt-in owned Codex threads and tmux -L ccmux-probe; see SPEC §12.11"]
fn live_db_freshness_pagination_and_poll_cost() {
    run(CASE_TIMEOUT, |h| {
        let mut creator = h.creator()?;
        let first = creator.materialize(&h.name("index-a"))?;
        creator.history_ids(true, false, 1, &[&first])?;
        let second = creator.materialize(&h.name("index-b"))?;
        let ids = [first.as_str(), second.as_str()];
        let expected = BTreeMap::from([
            (first.clone(), creator.read(&first)?), (second.clone(), creator.read(&second)?),
        ]);
        // Wait for DB-only visibility BEFORE the first scan. Never let scan
        // repair the index and then claim the original DB-only read was fresh.
        let wait = creator.wait.scoped(Duration::from_secs(15));
        let (db, pages) = wait.until("DB-only metadata and pagination before any scan-and-repair", || {
            let (rows, pages) = creator.history(true, false, 1)?;
            let ready = pages >= 2 && metadata_matches(&expected, &rows, &ids);
            let snapshot = json!({"pages":pages,"expected":owned_metadata(&expected, &ids),
                "db":owned_metadata(&rows, &ids)});
            Ok((ready.then_some((rows, pages)), snapshot))
        })?;
        let wait = creator.wait.scoped(Duration::from_secs(15));
        wait.until("scan and DB-only metadata matching the pre-scan sample", || {
            let (scan, _) = creator.history(false, false, 1)?;
            let (again, _) = creator.history(true, false, 1)?;
            let ready = metadata_matches(&db, &scan, &ids) && metadata_matches(&db, &again, &ids);
            Ok((ready.then_some(()), json!({"before":owned_metadata(&db, &ids),
                "scan":owned_metadata(&scan, &ids),"after":owned_metadata(&again, &ids)})))
        })?;
        let (_, loaded_pages) = creator.loaded_ids(1, &ids)?;
        ensure!(loaded_pages >= 2, "loaded pagination did not cross two pages: {loaded_pages}");
        creator.close()?;
        let wait = h.wait(Duration::from_secs(15));
        let mut warmup_attempts = 0;
        wait.until("production poll complete with both materialized probe rows", || {
            warmup_attempts += 1;
            let observation = h.client.poll(chrono::Utc::now().timestamp_millis());
            let present: Vec<_> = ids.iter().map(|id| observation.sessions.iter().any(|s| s.session_id == *id)).collect();
            let ready = observation.complete && present.iter().all(|yes| *yes);
            Ok((ready.then_some(()), json!({"complete":observation.complete,"present":present,
                "diagnostic":observation.diagnostic.map(|d| d.message)})))
        })?;
        let mut samples = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            let observation = h.client.poll(chrono::Utc::now().timestamp_millis());
            samples.push(start.elapsed().as_millis());
            ensure!(observation.complete, "measured production poll incomplete: {:?}",
                observation.diagnostic);
            ensure!(ids.iter().all(|id| observation.sessions.iter().any(|s| s.session_id == *id)),
                "measured production poll lost settled probe rows");
        }
        samples.sort_unstable();
        h.record(json!({"event":"index_pagination_poll_pass","history_pages":pages,
            "loaded_pages":loaded_pages,"warmup_attempts":warmup_attempts,"poll_ms":samples,
            "p95_ms":samples[4],"budget_ms":POLL_TIMEOUT.as_millis()}))
    });
}

fn sleep_completed(row: &Value) -> bool {
    row["items"].as_array().is_some_and(|items| items.iter().any(|i|
        i["type"] == "commandExecution" && i["status"] == "completed" && i["exitCode"] == 0
        && i["command"].as_str().is_some_and(|s| s.contains("/usr/bin/sleep 60"))))
}

fn active_completion_ready(row: &Value) -> bool {
    row["status"] == "completed" && sleep_completed(row)
        && row["items"].as_array().is_some_and(|items| items.iter().any(|i|
            i["type"] == "agentMessage" && i["text"].as_str().is_some_and(|s| s.trim() == "done")))
}

fn active_exit(h: &mut Harness<'_>, graceful: bool) -> Result<()> {
    let mut creator = h.creator()?;
    let id = creator.materialize(&h.name(if graceful { "active-graceful" } else { "active-abrupt" }))?;
    // Attach BEFORE starting the active turn: TUI bootstrap should not spend
    // the sleep interval that supplies our zero-client active witness.
    let pane = h.open(&id)?;
    let turn = creator.start_turn(&id,
        "Run /usr/bin/sleep 60 with the shell execution tool in the foreground. Do not background it or shorten it. After it exits, reply exactly done. Do not run other commands or tools.")?;
    // The owned item/started must identify this turn's foreground sleep
    // command AND status inProgress. A turn/start ack alone is not underway.
    creator.created_progress(&id, &turn, true)?;
    let tui = h.handoff_target(&pane, &id)?;
    creator.handoff(&id, &turn, &tui)?;
    if graceful { h.quit(&pane)?; } else { h.kill_pane(&pane)?; }
    // Observers never resume/subscribe. Missing or partial rows may settle, but
    // observing only completion cannot prove an ACTIVE turn survived exit.
    let mut observer = h.rpc(ACTIVE_OBSERVER_TIMEOUT)?;
    let row = observer.turn(&id, &turn)?;
    ensure!(row["status"] == "inProgress", "no active witness after last client exit: {}",
        observer.wait.describe(&row));
    h.record(json!({"event":"zero_client_active","id":id,"turn":turn,"graceful":graceful}))?;
    observer.active_completed(&id, &turn)?;
    let subscription = observer.request("thread/unsubscribe", json!({"threadId":id}))?;
    ensure!(matches!(subscription["status"].as_str(), Some("notSubscribed" | "notLoaded")),
        "completion observer subscribed: {}", observer.wait.describe(&subscription));
    observer.close()?;
    h.record(json!({"event":"active_last_client_completion_pass","id":id,"turn":turn,"graceful":graceful}))
}

#[test]
#[ignore = "opt-in owned Codex threads and tmux -L ccmux-probe; see SPEC §12.11"]
fn live_active_turn_survives_graceful_last_client_exit() {
    run(ACTIVE_CASE_TIMEOUT, |h| active_exit(h, true));
}

#[test]
#[ignore = "opt-in owned Codex threads and tmux -L ccmux-probe; see SPEC §12.11"]
fn live_active_turn_survives_abrupt_last_client_exit() {
    run(ACTIVE_CASE_TIMEOUT, |h| active_exit(h, false));
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
    let clock = MonotonicClock(Instant::now());
    let mut rpc = LiveRpc {
        transport:Some(Box::new(FakeLiveTransport { sent:sent.clone(), replies: [
            json!({"id":0,"method":"item/commandExecution/requestApproval","params":{}}),
            json!({"id":0,"result":{"thread":{"id":id}}}),
            json!({"id":1,"result":{}}),
            json!({"id":2,"error":{"code":-1,"message":"synthetic-secret"}}),
        ].into() })),
        registry:registry.clone(), work:"/probe".into(), next_id:0, events:Vec::new(),
        creator:true, turns:BTreeMap::new(), wait:Waiter::new(Deadline::new(&clock), "synthetic-secret"),
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

const WAIT_ID: &str = "01a0a609-12a6-7000-8000-000000000001";
const WAIT_OTHER: &str = "01a0a609-12a6-7000-8000-000000000002";
const WAIT_SECRET: &str = "secret\"\\value";

#[derive(Default)]
struct WaitClock(std::cell::Cell<Duration>);

impl Clock for WaitClock {
    fn now(&self) -> Duration { self.0.get() }
}

impl WaitClock {
    fn advance(&self, duration: Duration) { self.0.set(self.0.get() + duration); }
}

fn scripted_live_rpc<'a>(
    clock: &'a WaitClock, pause: &'a dyn Fn(Duration), results: Vec<Value>, budget: Duration,
) -> (LiveRpc<'a>, Rc<RefCell<Vec<Value>>>) {
    let mut registry = Registry::default();
    registry.created(WAIT_ID, "ccmux-probe-waits").unwrap();
    let sent = Rc::new(RefCell::new(Vec::new()));
    let replies = results.into_iter().enumerate()
        .map(|(id, result)| json!({"id":id,"result":result})).collect();
    (LiveRpc {
        transport:Some(Box::new(FakeLiveTransport { sent:sent.clone(), replies })),
        registry:Rc::new(RefCell::new(registry)), work:"/probe".into(), next_id:0, events:Vec::new(),
        creator:true, turns:BTreeMap::new(), wait:Waiter::with_pause(Deadline { clock, end:clock.now() + budget }, WAIT_SECRET, pause),
    }, sent)
}

fn turn_page(row: Value) -> Value {
    if row.is_null() { json!({"data":[]}) } else { json!({"data":[row]}) }
}

#[test]
fn live_turn_waits_through_index_lag_partial_and_unknown_statuses() {
    for completed in [false, true] {
        let clock = WaitClock::default();
        let pause = |d| clock.advance(d);
        let target = if completed { "completed" } else { "inProgress" };
        let (mut rpc, sent) = scripted_live_rpc(&clock, &pause, vec![
            json!({"turn":{"id":"new-turn"}}),
            turn_page(Value::Null),
            turn_page(json!({"id":"old-turn","status":"completed"})),
            turn_page(json!({"id":"new-turn"})),
            turn_page(json!({"id":"new-turn","status":"future-status"})),
            turn_page(json!({"id":"new-turn","status":target})),
        ], Duration::from_secs(5));
        let turn = rpc.start_turn(WAIT_ID, "trivial").unwrap();
        let row = if completed { rpc.completed(WAIT_ID, &turn) } else { rpc.turn(WAIT_ID, &turn) }.unwrap();
        assert_eq!(row["id"], "new-turn");
        assert_eq!(row["status"], target);
        assert_eq!(clock.now(), Duration::from_secs(1));
        let calls = sent.borrow();
        assert_eq!(calls.len(), 6);
        assert_eq!(calls[0]["method"], "turn/start");
        assert!(calls[1..].iter().all(|v| v["method"] == "thread/turns/list"
            && v["params"]["threadId"] == WAIT_ID));
    }
}

#[test]
fn live_explicit_turn_failure_reports_the_redacted_error_payload() {
    for status in ["failed","interrupted"] {
        let clock = WaitClock::default();
        let pause = |d| clock.advance(d);
        let (mut rpc, sent) = scripted_live_rpc(&clock, &pause, vec![
            turn_page(Value::Null),
            turn_page(json!({"id":"turn","status":status,"error":{
                "message":format!("model failed {WAIT_SECRET}"),"code":"model_error",
                "details":{"authorization":"another private credential"}
            }})),
        ], Duration::from_secs(5));
        let error = rpc.completed(WAIT_ID, "turn").unwrap_err().to_string();
        assert!(error.contains(status) && error.contains("model_error") && error.contains("model failed"));
        assert!(error.contains("error:") && error.contains("redacted"));
        assert!(!error.contains(WAIT_SECRET) && !error.contains("another private credential"));
        assert_eq!(sent.borrow().len(), 2);
        assert_eq!(clock.now(), Duration::from_millis(250)); // explicit failure does not wait out the budget
    }
}

#[test]
fn live_missing_and_unknown_turn_status_timeout_with_last_observation() {
    for row in [Value::Null, json!({"id":"turn"}), json!({"id":"turn","status":"queued"}),
        json!({"id":"turn","status":17})]
    {
        let clock = WaitClock::default();
        let pause = |d| clock.advance(d);
        let (mut rpc, sent) = scripted_live_rpc(&clock, &pause,
            vec![turn_page(row.clone()); 4], Duration::from_secs(1));
        let error = rpc.completed(WAIT_ID, "turn").unwrap_err().to_string();
        assert!(error.contains("timed out waiting for turn completion"));
        assert!(error.contains(WAIT_ID) && error.contains("turn turn") && error.contains("last observed:"));
        assert!(error.contains(&format!("\"row\":{row}")));
        assert_eq!(clock.now(), Duration::from_secs(1));
        assert_eq!(sent.borrow().len(), 4);
    }
}

#[test]
fn live_active_completion_waits_for_items_after_the_completed_status() {
    let clock = WaitClock::default();
    let pause = |d| clock.advance(d);
    let command = json!({"type":"commandExecution","command":"/usr/bin/sleep 60",
        "status":"completed","exitCode":0});
    let (mut rpc, sent) = scripted_live_rpc(&clock, &pause, vec![
        turn_page(json!({"id":"turn","status":"inProgress"})),
        turn_page(json!({"id":"turn","status":"completed"})),
        turn_page(json!({"id":"turn","status":"completed","items":[command.clone()]})),
        turn_page(json!({"id":"turn","status":"completed","items":[command,
            {"type":"agentMessage","text":"done"}]})),
    ], Duration::from_secs(5));
    let row = rpc.wait_turn(WAIT_ID, "turn", "completion evidence",
        Duration::from_secs(5), active_completion_ready).unwrap();
    assert!(active_completion_ready(&row));
    assert_eq!(sent.borrow().len(), 4);
    assert_eq!(clock.now(), Duration::from_millis(750));
    // A completed first observation is still NOT an inProgress witness.
    assert!(turn_visible(&row));
    assert_ne!(row["status"], "inProgress");
}

fn sleep_turn_row(command_done: bool, turn_done: bool) -> Value {
    let mut items = vec![json!({"type":"commandExecution","command":"/bin/zsh -c '/usr/bin/sleep 60'",
        "status":if command_done { "completed" } else { "inProgress" },
        "exitCode":if command_done { json!(0) } else { Value::Null }})];
    if turn_done { items.push(json!({"type":"agentMessage","text":"done"})); }
    json!({"id":"turn","status":if turn_done { "completed" } else { "inProgress" },"items":items})
}

#[test]
fn live_exit_completion_allows_sleep_then_a_slow_final_model_round() {
    let clock = WaitClock::default();
    // Setup/attach/exit already spent most of the ordinary case budget.
    clock.advance(Duration::from_secs(300));
    let pause = |d| clock.advance(d);
    let pending = turn_page(sleep_turn_row(false, false));
    let command_done = turn_page(sleep_turn_row(true, false));
    let mut rows = vec![pending.clone()]; // active witness after last-client exit
    rows.extend(vec![pending; 240]); // a full 60-second sleep still remains
    rows.push(command_done.clone());
    rows.extend(vec![command_done; 600]); // final model round takes another 150 seconds
    rows.push(turn_page(sleep_turn_row(true, true)));
    let (mut rpc, sent) = scripted_live_rpc(&clock, &pause, rows,
        ACTIVE_OBSERVER_TIMEOUT.min(ACTIVE_CASE_TIMEOUT.saturating_sub(clock.now())));
    assert_eq!(rpc.turn(WAIT_ID, "turn").unwrap()["status"], "inProgress");
    assert!(active_completion_ready(&rpc.active_completed(WAIT_ID, "turn").unwrap()));
    assert_eq!(clock.now(), Duration::from_secs(510));
    assert!(sent.borrow().iter().all(|v| v["method"] == "thread/turns/list"));
}

#[test]
fn live_exit_final_round_deadline_is_not_renewed_and_preserves_last_row() {
    let clock = WaitClock::default();
    let pause = |d| clock.advance(d);
    let mut command_done = sleep_turn_row(true, false);
    command_done["items"][0]["aggregatedOutput"] = json!(WAIT_SECRET);
    let mut rows = vec![turn_page(sleep_turn_row(false, false)); 240];
    rows.push(turn_page(command_done.clone()));
    rows.extend(vec![turn_page(command_done); 721]); // no terminal response
    let (mut rpc, _) = scripted_live_rpc(&clock, &pause, rows, ACTIVE_OBSERVER_TIMEOUT);
    let error = rpc.active_completed(WAIT_ID, "turn").unwrap_err().to_string();
    assert_eq!(clock.now(), Duration::from_secs(60 + 180));
    assert!(error.contains("timed out waiting for completed turn with successful sleep and final done"));
    assert!(error.contains("last observed:") && error.contains("\"status\":\"inProgress\""));
    assert!(error.contains("\"status\":\"completed\"") && error.contains("\"exitCode\":0"));
    assert!(error.contains("redacted") && !error.contains(WAIT_SECRET));
}

#[test]
fn live_exit_command_wait_is_bounded_and_requires_successful_completion() {
    let completed = sleep_turn_row(true, false);
    assert!(sleep_completed(&completed));
    for (field, value) in [
        ("status", json!("inProgress")), ("status", Value::Null), ("exitCode", json!(1)),
    ] {
        let mut pending = completed.clone();
        pending["items"][0][field] = value;
        assert!(!sleep_completed(&pending));
        let clock = WaitClock::default();
        let pause = |d| clock.advance(d);
        let (mut rpc, _) = scripted_live_rpc(&clock, &pause, vec![turn_page(pending); 361],
            ACTIVE_OBSERVER_TIMEOUT);
        let error = rpc.active_completed(WAIT_ID, "turn").unwrap_err().to_string();
        assert_eq!(clock.now(), Duration::from_secs(90));
        assert!(error.contains("timed out waiting for successful sleep command completion"));
        assert!(error.contains("last observed:"));
    }
}

#[test]
fn live_thread_state_waits_for_loaded_membership_and_metadata_to_converge() {
    for (loaded, status) in [(false, "notLoaded"), (true, "idle")] {
        let clock = WaitClock::default();
        let pause = |d| clock.advance(d);
        let ids = if loaded { json!([WAIT_ID]) } else { json!([]) };
        let stale = if loaded { "notLoaded" } else { "idle" };
        let (mut rpc, sent) = scripted_live_rpc(&clock, &pause, vec![
            json!({"data":if loaded { json!([]) } else { json!([WAIT_ID]) }}),
            json!({"thread":{"id":WAIT_ID,"status":{"type":stale}}}),
            json!({"data":ids}),
            json!({"thread":{"id":WAIT_ID,"status":null}}),
            json!({"data":ids}),
            json!({"thread":{"id":WAIT_ID,"status":{"type":status}}}),
        ], Duration::from_secs(5));
        assert_eq!(rpc.thread_state(WAIT_ID, status, loaded).unwrap()["status"]["type"], status);
        assert_eq!(sent.borrow().len(), 6);
        assert!(sent.borrow().iter().all(|v| matches!(v["method"].as_str(),
            Some("thread/loaded/list" | "thread/read"))));
        assert_eq!(clock.now(), Duration::from_millis(500));
    }
}

#[test]
fn live_history_and_loaded_waits_retry_a_whole_paginated_observation() {
    for history in [false, true] {
        let clock = WaitClock::default();
        let pause = |d| clock.advance(d);
        let row = |id: &str| if history { json!({"id":id}) } else { json!(id) };
        let (mut rpc, sent) = scripted_live_rpc(&clock, &pause, vec![
            json!({"data":[]}),
            json!({"data":[row(WAIT_ID)],"nextCursor":"next-page"}),
            json!({"data":[row(WAIT_OTHER)],"nextCursor":null}),
        ], Duration::from_secs(5));
        let ids = [WAIT_ID, WAIT_OTHER];
        let pages = if history { rpc.history_ids(true, true, 1, &ids).unwrap().1 }
            else { rpc.loaded_ids(1, &ids).unwrap().1 };
        assert_eq!(pages, 2);
        let calls = sent.borrow();
        assert!(calls[0]["params"]["cursor"].is_null());
        assert!(calls[1]["params"]["cursor"].is_null()); // start a new walk after the pending sample
        assert_eq!(calls[2]["params"]["cursor"], "next-page");
        if history { assert!(calls.iter().all(|v| v["params"]["useStateDbOnly"] == true)); }
        assert_eq!(clock.now(), Duration::from_millis(250));
    }
}

#[test]
fn live_wait_timeout_retains_last_value_and_redacts_before_escaping_or_truncation() {
    let clock = WaitClock::default();
    let pause = |d| clock.advance(d);
    let wait = Waiter::with_pause(Deadline::new(&clock), WAIT_SECRET, &pause);
    let mut attempts = 0;
    let error = wait.until("archived index row", || {
        attempts += 1;
        if attempts == 2 {
            clock.advance(Duration::from_secs(1));
            return Err(live_error(CodexError::timeout()));
        }
        Ok((None::<()>, json!({"stage":"index pending","message":format!("\u{1b}[31m{WAIT_SECRET}"),
            "nested":{"api_key":"provider-key"},"padding":"x".repeat(5000)})))
    }).unwrap_err();
    let error = format!("{error:?} {error}");
    let escaped = serde_json::to_string(WAIT_SECRET).unwrap();
    assert!(error.contains("archived index row") && error.contains("index pending"));
    assert!(error.contains("redacted") && error.contains("truncated"));
    assert!(!error.contains(WAIT_SECRET) && !error.contains(&escaped[1..escaped.len()-1]));
    assert!(!error.contains("provider-key") && !error.contains('\u{1b}'));

    let clock = WaitClock::default();
    let pause = |d| clock.advance(d);
    let wait = Waiter::with_pause(Deadline::new(&clock), WAIT_SECRET, &pause);
    let error = wait.until("ready within deadline", || {
        clock.advance(Duration::from_secs(1));
        Ok((Some(()), json!({"ready":true})))
    }).unwrap_err().to_string();
    assert!(error.contains("timed out waiting for ready within deadline") && error.contains("\"ready\":true"));
}

#[test]
fn live_tui_readiness_waits_for_input_echo_and_launch_identity_without_branding() {
    let clock = WaitClock::default();
    let pause = |d| clock.advance(d);
    let wait = Waiter::with_pause(Deadline { clock:&clock, end:Duration::from_secs(5) }, WAIT_SECRET, &pause);
    let mut snapshots = std::collections::VecDeque::from([
        json!({"latch":"","screen":""}),
        json!({"latch":"","screen":"model:     loading\nResuming session…\n› Ask Codex"}),
        json!({"latch":"","screen":"Codex new banner\n› Ask Codex"}),
    ]);
    wait.until("TUI input prompt", || {
        let snapshot = snapshots.pop_front().unwrap();
        Ok((composer_ready(&snapshot).then_some(()), snapshot))
    }).unwrap();
    assert_eq!(clock.now(), Duration::from_millis(500));
    assert!(!command_visible(&json!({"latch":"","screen":"› /stat"}), "/status"));
    assert!(command_visible(&json!({"latch":"","screen":"› /status"}), "/status"));
    assert!(!status_identifies(&json!({"latch":"","screen":format!(
        "Resuming {WAIT_ID}\n│ Session: {WAIT_OTHER} │")}), WAIT_ID));
    assert!(status_identifies(&json!({"latch":"","screen":format!(
        "│ Session: {}\n│ {} │\n› Ask Codex", &WAIT_ID[..20], &WAIT_ID[20..])}), WAIT_ID));
    let parked = json!({"latch":"1","screen":format!(
        "│ Session: {WAIT_ID} │\n[ccmux] Codex attach exited (rc=0). resume: {WAIT_ID}")});
    assert!(!status_identifies(&parked, WAIT_ID) && !composer_ready(&parked));
    let error = wait.scoped(Duration::from_millis(500)).until::<()>("TUI /status for launch id", || {
        Ok((None, json!({"pane":"%3","screen":format!("last frame {WAIT_SECRET}")})))
    }).unwrap_err().to_string();
    assert!(error.contains("TUI /status for launch id") && error.contains("last frame"));
    assert!(!error.contains(WAIT_SECRET));
}

#[test]
fn live_cleanup_failure_keeps_the_case_condition_and_cleanup_diagnostic() {
    let error = with_cleanup(&mut (), |_| bail!("turn visibility: last row null"),
        |_| bail!("archive visibility: last row idle")).unwrap_err().to_string();
    assert!(error.contains("cleanup FAILED"));
    assert!(error.contains("turn visibility: last row null") && error.contains("archive visibility: last row idle"));
}

struct WaitDeadlineWire<'a> { clock: &'a WaitClock, armed: Cell<Duration> }

impl Read for WaitDeadlineWire<'_> {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        self.clock.advance(self.armed.get());
        Err(io::ErrorKind::WouldBlock.into())
    }
}

impl Write for WaitDeadlineWire<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> { Ok(bytes.len()) }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

impl SocketIo for WaitDeadlineWire<'_> {
    fn read_timeout(&self, duration: Duration) -> io::Result<()> { self.armed.set(duration); Ok(()) }
    fn write_timeout(&self, _duration: Duration) -> io::Result<()> { Ok(()) }
}

#[test]
fn live_scoped_deadline_reaches_socket_reads_and_restores_the_connection_budget() {
    let clock = WaitClock::default();
    let deadline = Deadline::new(&clock);
    let stream = TimedStream { deadline, inner:WaitDeadlineWire {
        clock:&clock, armed:Cell::new(Duration::ZERO),
    }};
    let socket = WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Client, None);
    let mut transport = WsTransport { socket, deadline };
    transport.set_deadline(Duration::from_millis(250));
    let error = transport.receive().unwrap_err();
    assert_eq!(error.diagnostic.kind, CodexFailureKind::Timeout);
    assert_eq!(clock.now(), Duration::from_millis(250)); // not the connection's original one second

    let clock = WaitClock::default();
    let pause = |d| clock.advance(d);
    let wait = Waiter::with_pause(Deadline::new(&clock), WAIT_SECRET, &pause);
    for panic in [false, true] {
        let result = catch_unwind(AssertUnwindSafe(|| {
            wait.scoped(Duration::from_millis(100)).until::<()>("scoped read", || {
                assert_eq!(wait.io_end.get(), Duration::from_millis(100));
                if panic { panic!("test callback"); }
                bail!("test read failure")
            })
        }));
        if panic { assert!(result.is_err()); } else { assert!(result.unwrap().is_err()); }
        assert_eq!(wait.io_end.get(), Duration::from_secs(1));
    }
}

// This transport is one physical connection for the entire creator lifecycle.
// Running out of scripted frames simulates a blocked read until its deadline.
#[derive(Default)]
struct LifecycleTrace {
    methods: Vec<String>,
    timeline: Vec<String>,
    deadlines: Vec<Duration>,
    closes: usize,
    drops: usize,
    terminal_received: bool,
}

struct LifecycleTransport<'a> {
    clock: &'a WaitClock,
    end: Duration,
    trace: Rc<RefCell<LifecycleTrace>>,
    frames: std::collections::VecDeque<(Duration, Value)>,
}

impl Drop for LifecycleTransport<'_> {
    fn drop(&mut self) { self.trace.borrow_mut().drops += 1; }
}

impl Transport for LifecycleTransport<'_> {
    fn send(&mut self, value: Value) -> Result<(), CodexError> {
        let mut trace = self.trace.borrow_mut();
        assert_eq!(trace.closes, 0);
        assert_eq!(trace.drops, 0);
        let method = value["method"].as_str().unwrap();
        trace.methods.push(method.into());
        trace.timeline.push(method.into());
        Ok(())
    }
    fn receive(&mut self) -> Result<Value, CodexError> {
        let (delay, value) = self.frames.pop_front().unwrap_or((self.end.saturating_sub(self.clock.now()), Value::Null));
        self.clock.advance(delay);
        if self.clock.now() >= self.end { return Err(CodexError::timeout()); }
        if value["method"] == "turn/completed" {
            let mut trace = self.trace.borrow_mut();
            trace.terminal_received = true;
            trace.timeline.push(format!("terminal:{}:{}:{}", value["params"]["threadId"],
                value["params"]["turn"]["id"], value["params"]["turn"]["status"]));
        }
        Ok(value)
    }
    fn close(&mut self) -> Result<(), CodexError> {
        self.trace.borrow_mut().closes += 1;
        Ok(())
    }
    fn set_deadline(&mut self, end: Duration) {
        self.end = end;
        self.trace.borrow_mut().deadlines.push(end);
    }
}

fn lifecycle_rpc<'a>(clock: &'a WaitClock, frames: Vec<Value>)
    -> (LiveRpc<'a>, Rc<RefCell<LifecycleTrace>>)
{
    let mut registry = Registry::default();
    registry.created(WAIT_ID, "ccmux-probe-lifecycle").unwrap();
    let trace = Rc::new(RefCell::new(LifecycleTrace::default()));
    let end = clock.now() + CASE_TIMEOUT;
    (LiveRpc {
        transport:Some(Box::new(LifecycleTransport { clock, end, trace:trace.clone(),
            frames:frames.into_iter().map(|v| (Duration::ZERO, v)).collect() })),
        registry:Rc::new(RefCell::new(registry)), work:"/probe".into(), next_id:0, events:Vec::new(),
        creator:true, turns:BTreeMap::new(), wait:Waiter::new(Deadline { clock, end }, WAIT_SECRET),
    }, trace)
}

fn terminal_event(id: &str, turn: &str, status: Value) -> Value {
    json!({"method":"turn/completed","params":{"threadId":id,
        "turn":{"id":turn,"status":status,"error":null,"items":[]}}})
}

fn command_event(id: &str, turn: &str) -> Value {
    json!({"method":"item/started","params":{"threadId":id,"turnId":turn,
        "item":{"id":"command","type":"commandExecution","command":"/bin/zsh -c '/usr/bin/sleep 60'",
            "status":"inProgress"}}})
}

#[test]
fn live_materialization_keeps_one_creator_until_terminal_then_reads_history() {
    let clock = WaitClock::default();
    let (mut rpc, trace) = lifecycle_rpc(&clock, vec![
        json!({"id":0,"result":{"thread":{"id":WAIT_ID}}}),
        json!({"id":1,"result":{}}),
        json!({"id":2,"result":{"turn":{"id":"materialize","status":"inProgress"}}}),
        json!({"method":"turn/started","params":{"threadId":WAIT_ID,"turn":{"id":"materialize"}}}),
        json!({"id":0,"method":"item/commandExecution/requestApproval","params":{"threadId":WAIT_ID}}),
        terminal_event(WAIT_OTHER, "materialize", json!("interrupted")),
        terminal_event(WAIT_ID, "older-turn", json!("interrupted")),
        terminal_event(WAIT_ID, "materialize", json!("completed")),
        json!({"id":3,"result":{"data":[WAIT_ID]}}),
        json!({"id":4,"result":{"thread":{"id":WAIT_ID,"status":{"type":"idle"}}}}),
        json!({"id":5,"result":{"data":[{"id":WAIT_ID}]}}),
    ]);
    rpc.registry.borrow_mut().threads.clear(); // materialize must register its own returned ID
    assert_eq!(rpc.materialize("ccmux-probe-lifecycle").unwrap(), WAIT_ID);
    assert!(rpc.turns.is_empty());
    let expected = ["thread/start","thread/name/set","turn/start",
        "thread/loaded/list","thread/read","thread/list"];
    assert_eq!(trace.borrow().methods, expected);
    assert!(trace.borrow().terminal_received);
    let terminal = format!("terminal:\"{WAIT_ID}\":\"materialize\":\"completed\"");
    let timeline = trace.borrow().timeline.clone();
    assert!(timeline.iter().position(|v| v == &terminal).unwrap()
        < timeline.iter().position(|v| v == "thread/loaded/list").unwrap());
    assert_eq!((trace.borrow().closes, trace.borrow().drops), (0,0));
    rpc.close().unwrap();
    assert_eq!((trace.borrow().closes, trace.borrow().drops), (1,1));
    drop(rpc);
    assert_eq!(trace.borrow().drops, 1);
}

#[test]
fn live_creator_accepts_terminal_notification_before_start_response_on_same_socket() {
    let clock = WaitClock::default();
    let (mut rpc, trace) = lifecycle_rpc(&clock, vec![
        terminal_event(WAIT_ID, "turn", json!("completed")),
        json!({"id":0,"result":{"turn":{"id":"turn"}}}),
    ]);
    let turn = rpc.start_turn(WAIT_ID, "trivial").unwrap();
    let row = rpc.created_progress(WAIT_ID, &turn, false).unwrap();
    assert_eq!(row["status"], "completed");
    assert_eq!(trace.borrow().methods, ["turn/start"]);
    rpc.close().unwrap();
}

#[test]
fn live_creator_waits_for_matching_terminal_and_redacts_actual_terminal_failures() {
    for status in ["failed","interrupted"] {
        let clock = WaitClock::default();
        let mut terminal = terminal_event(WAIT_ID, "turn", json!(status));
        terminal["params"]["turn"]["error"] = json!({"message":format!("test {WAIT_SECRET}"),
            "code":"test_error","authorization":"private"});
        let (mut rpc, trace) = lifecycle_rpc(&clock, vec![
            json!({"id":0,"result":{"turn":{"id":"turn"}}}),
            terminal_event(WAIT_OTHER, "turn", json!("failed")),
            terminal_event(WAIT_ID, "turn", Value::Null),
            terminal_event(WAIT_ID, "turn", json!("future-status")),
            terminal,
        ]);
        rpc.start_turn(WAIT_ID, "trivial").unwrap();
        let error = rpc.created_progress(WAIT_ID, "turn", false).unwrap_err();
        let text = format!("{error:?} {error}");
        assert!(text.contains(status) && text.contains("test_error") && text.contains("redacted"));
        assert!(!text.contains(WAIT_SECRET) && !text.contains("private"));
        assert!(rpc.turns.is_empty()); // terminal error is known; cleanup can close
        assert_eq!(trace.borrow().methods, ["turn/start"]);
        rpc.close().unwrap();
    }
}

#[test]
fn live_creator_uses_one_turn_deadline_and_timeout_keeps_last_notification() {
    for status in [Value::Null, json!("future-status")] {
        let clock = WaitClock::default();
        let (mut rpc, trace) = lifecycle_rpc(&clock, vec![
            json!({"id":0,"result":{"turn":{"id":"turn"}}}),
            terminal_event(WAIT_ID, "turn", status.clone()),
        ]);
        rpc.start_turn(WAIT_ID, "trivial").unwrap();
        // The deadline covers the whole lifecycle, not a fresh budget at each
        // step, and not the short budget used by ordinary read-only observers.
        clock.advance(Duration::from_secs(100));
        let error = rpc.created_progress(WAIT_ID, "turn", false).unwrap_err().to_string();
        assert!(error.contains("timed out waiting for creator turn/completed notification"));
        assert!(error.contains("creator_connected") && error.contains("terminal_notification"));
        assert!(error.contains(&format!("\"status\":{status}")));
        assert_eq!(clock.now(), TURN_TIMEOUT);
        assert!(trace.borrow().deadlines.iter().all(|end| *end == TURN_TIMEOUT));
        assert_eq!((trace.borrow().closes, trace.borrow().drops), (0,0));
    }
}

#[test]
fn live_pending_creator_cannot_close_or_handoff_without_both_witnesses() {
    let clock = WaitClock::default();
    let (mut rpc, trace) = lifecycle_rpc(&clock, vec![
        json!({"id":0,"result":{"turn":{"id":"turn"}}}),
        command_event(WAIT_ID, "turn"),
    ]);
    let turn = rpc.start_turn(WAIT_ID, "sleep").unwrap();
    let tui = AttachedTui { pane:"%1".into(), thread:WAIT_ID.into() };
    assert!(rpc.close().unwrap_err().to_string().contains("pending turn"));
    assert!(rpc.handoff(WAIT_ID, &turn, &tui).is_err());
    assert_eq!(trace.borrow().closes, 0);
    rpc.created_progress(WAIT_ID, &turn, true).unwrap();
    let wrong = AttachedTui { pane:"%2".into(), thread:WAIT_OTHER.into() };
    assert!(rpc.handoff(WAIT_ID, &turn, &wrong).is_err());
    assert!(rpc.close().is_err()); // underway alone is not a normal terminal
    assert_eq!(trace.borrow().closes, 0);
    rpc.handoff(WAIT_ID, &turn, &tui).unwrap();
    assert_eq!((trace.borrow().closes, trace.borrow().drops), (1,1));
    assert_eq!(trace.borrow().methods, ["turn/start"]); // no polling reconnects or mutations
}

#[test]
fn live_sleep_witness_requires_owned_turn_and_in_progress_command_item() {
    let event = command_event(WAIT_ID, "turn");
    assert!(sleep_underway(&event, WAIT_ID, "turn"));
    assert!(!sleep_underway(&event, WAIT_OTHER, "turn"));
    assert!(!sleep_underway(&event, WAIT_ID, "other"));
    for (path, value) in [
        ("/id", json!(0)),
        ("/method", json!("item/completed")),
        ("/params/item/type", json!("agentMessage")),
        ("/params/item/status", json!("completed")),
        ("/params/item/status", Value::Null),
        ("/params/item/command", json!("/usr/bin/true")),
    ] {
        let mut invalid = event.clone();
        if path == "/id" { invalid["id"] = value; }
        else { *invalid.pointer_mut(path).unwrap() = value; }
        assert!(!sleep_underway(&invalid, WAIT_ID, "turn"), "{path}");
    }
    let clock = WaitClock::default();
    let (mut rpc, trace) = lifecycle_rpc(&clock, vec![
        json!({"id":0,"result":{"turn":{"id":"turn"}}}),
        terminal_event(WAIT_ID, "turn", json!("completed")),
    ]);
    rpc.start_turn(WAIT_ID, "sleep").unwrap();
    let error = rpc.created_progress(WAIT_ID, "turn", true).unwrap_err().to_string();
    assert!(error.contains("completed before creator handoff"));
    assert_eq!(trace.borrow().closes, 0);
}

#[test]
fn live_short_observers_cannot_create_threads_or_start_turns() {
    let clock = WaitClock::default();
    let (mut rpc, trace) = lifecycle_rpc(&clock, Vec::new());
    rpc.creator = false;
    assert!(rpc.create("ccmux-probe-observer").is_err());
    assert!(rpc.start_turn(WAIT_ID, "trivial").is_err());
    assert!(rpc.request("turn/start", json!({"threadId":WAIT_ID})).is_err());
    assert!(trace.borrow().methods.is_empty());
    assert_eq!(trace.borrow().closes, 0);
}
