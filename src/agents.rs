//! Owner: Agents lane. SPEC §3.3.
//!
//! Everything that shells out to `claude`, plus the shell-command templates for
//! panes. Builds strings; never runs tmux.
//!
//! Consumes `model::{Payload, ParseError}`, `tmux::sh_quote` (the pane command
//! templates are the shell boundary of RULE Q2 and must quote through the same
//! function the launcher uses), and `tmux::OPT_PANE_DETACHED` — the name of the
//! pane option `attach_pane_cmd` latches is single-sourced with the `PANE_FMT`
//! field that reads it back, so the two can never drift apart. Nothing else
//! from `tmux`.
//!
//! SPEC NOTE (polling): SPEC §4.2 pins polling as *synchronous, on the
//! event-loop thread* — "No threads, no channels, no async runtime" — and §10.3
//! lists any background thread as an explicit non-goal. `poll()` is therefore a
//! blocking call costing ~0.21s (PROBE-FINDINGS §1); `app.rs` calls it from
//! `tick()` every `agents_interval()`, and only when someone is watching
//! (§4.2's poll gate). AMENDED: the call is still
//! synchronous and thread-free, but it is now bounded by `POLL_TIMEOUT` — an
//! unbounded one froze the entire sidebar, Ctrl-C included, whenever `claude`
//! wedged.
//!
//! SPEC NOTE (RULE Q1): every function here builds an argv via
//! `Command::new(prog).args([..])`. Nothing in this module spawns `sh -c`. The
//! only shell string produced is `attach_pane_cmd`, which is handed to tmux
//! (RULE Q2) and quotes every interpolation through `tmux::sh_quote`.

use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crate::model::{ParseError, Payload};
use crate::tmux::{LATCH_SHELL, OPT_PANE_DETACHED, sh_quote};

#[derive(Debug)]
pub enum AgentsError {
    /// `claude` could not be spawned.
    NotFound(String),
    /// Non-zero exit; carries trimmed stderr and the exit code.
    Cmd { code: i32, stderr: String },
    /// Output was not the expected JSON.
    Parse(ParseError),
    /// Caller asked for a verb that needs a short id on a session without one.
    NotAttachable,
}

/// SPEC NOTE: these strings are **footer-ready**. §9.1 wants
/// `agents: <first non-empty stderr line, or "exit <code>">`, §9.2 wants
/// `agents: bad json: <excerpt>`, §9.8 wants `agents: claude not found on
/// PATH`, and §8.2 wants `stop failed: <stderr first line>`. Rendering
/// `format!("agents: {err}")` (truncated to 120 chars by the caller, per §9.1)
/// produces all four verbatim. Callers must NOT re-prefix.
impl std::fmt::Display for AgentsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `prog` is `claude_bin()`, so the default case renders exactly
            // "claude not found on PATH" (§9.8) and an overridden
            // CCMUX_CLAUDE_BIN still names the binary that actually failed.
            AgentsError::NotFound(prog) => write!(f, "{prog} not found on PATH"),
            AgentsError::Cmd { code, stderr } => match first_nonempty_line(stderr) {
                Some(line) => write!(f, "{line}"),
                None => write!(f, "exit {code}"),
            },
            AgentsError::Parse(ParseError::Json { excerpt, .. }) => write!(f, "bad json: {excerpt}"),
            AgentsError::Parse(ParseError::NotAnArray) => write!(f, "bad json: not a JSON array"),
            AgentsError::NotAttachable => write!(f, "session has no short id"),
        }
    }
}

impl std::error::Error for AgentsError {}

/// First line of `s` that is not blank after trimming, trimmed.
fn first_nonempty_line(s: &str) -> Option<&str> {
    s.lines().map(str::trim).find(|l| !l.is_empty())
}

/// Resolved once at startup: "claude" unless `CCMUX_CLAUDE_BIN` overrides it.
/// Everything in this module and every pane template uses this value.
pub fn claude_bin() -> String {
    static BIN: OnceLock<String> = OnceLock::new();
    BIN.get_or_init(|| {
        // An empty override is treated as unset: `CCMUX_CLAUDE_BIN=` must not
        // turn every spawn into a NotFound for the empty program name.
        std::env::var("CCMUX_CLAUDE_BIN")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "claude".to_string())
    })
    .clone()
}

/// Run `claude <args>` and return its captured output.
///
/// `Command::output()` nulls the child's stdin (mandatory: the sidebar holds the
/// terminal in raw mode and a child must never be able to read our keystrokes)
/// and pipes stdout/stderr, so nothing the child prints can corrupt the
/// alternate screen. It also reaps the child, which `spawn()` without a `wait`
/// would not — this process is long-lived and must not accumulate zombies.
fn run(args: &[&str]) -> Result<std::process::Output, AgentsError> {
    let bin = claude_bin();
    Command::new(&bin).args(args).output().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => AgentsError::NotFound(bin),
        _ => AgentsError::Cmd {
            code: -1,
            stderr: e.to_string(),
        },
    })
}

/// Wall-clock ceiling on one `claude agents --json --all`.
///
/// SPEC AMENDMENT (§4.2): the spec pins polling as synchronous on the event-loop
/// thread and §10.3 forbids a background thread, which left `poll()` unbounded —
/// a wedged `claude` froze the sidebar for as long as it stayed wedged, Ctrl-C
/// included, while the header still showed a healthy indicator. The synchronous
/// contract is kept; only the wait is now bounded, and a timeout is reported as
/// an ordinary `Cmd` error so §9.1's red indicator and footer fire.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(5);
/// `try_wait` granularity inside `run_bounded`.
const POLL_STEP: Duration = Duration::from_millis(20);

/// Wall-clock ceiling on one `claude stop <id>`.
///
/// `stop` is bounded and `delete` is not, and the asymmetry is the whole
/// point. A bound is a KILL: `run_bounded` sends SIGKILL to the `claude` it is
/// waiting on. Doing that to `claude rm` — which is removing a git worktree —
/// could interrupt it halfway through, and there is no undo for whatever it
/// half-did. `stop` asks the daemon to halt a session; killing the client that
/// asked leaves the daemon's own answer to that request unaffected, and if the
/// stop did land, the next poll says so.
///
/// The number is 15x the measured 0.66s, so it fires only for a `claude` that
/// is genuinely wedged. It matters most to `R`, which issues one of these per
/// in-scope agent with no operator between them: unbounded, one hung `claude`
/// would freeze the whole restart pass, and the sidebar with it.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Wall-clock ceiling on one `claude respawn <id>`.
///
/// Bounded for the same reason `stop` is, and it is the same trade: `R` issues
/// one of these per paneless agent with no operator between them, so an
/// unbounded wait would let a single wedged `claude` freeze the restart pass
/// and the sidebar with it. A kill here is as recoverable as a kill on `stop`
/// — the daemon owns the restart it was asked for — but it is NOT free: this
/// verb halts the worker and starts a new one, so a client killed mid-flight
/// can leave the session halted. That is why the caller polls on the error
/// path instead of guessing (`App::restart_headless_agents`).
///
/// Same 10 s as `STOP_TIMEOUT` and for the same reason: the measured call
/// returns as soon as the daemon has swapped the worker (it prints
/// `respawned <id>` and exits), so ten seconds fires only for a `claude` that
/// is genuinely stuck.
pub const RESPAWN_TIMEOUT: Duration = Duration::from_secs(10);

/// `run`, with a deadline. On expiry the child is killed and reaped, and the
/// call reports `Cmd { code: -1, stderr: "timed out after Ns" }`.
///
/// Only for commands whose output is SMALL. It waits on `try_wait` while the
/// pipes go unread, so a child that produced more than a pipe buffer would
/// block on write and be killed as if it had hung; `logs` (a raw PTY dump) must
/// keep using `run`.
fn run_bounded(args: &[&str], timeout: Duration) -> Result<std::process::Output, AgentsError> {
    let bin = claude_bin();
    let mut child = Command::new(&bin)
        // Same three redirections `output()` implies, spelled out because
        // `spawn()` does not: stdin MUST be null (the sidebar holds the
        // terminal in raw mode and a child must never read our keystrokes) and
        // both output streams MUST be piped so nothing lands on the alternate
        // screen.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args)
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => AgentsError::NotFound(bin.clone()),
            _ => AgentsError::Cmd { code: -1, stderr: e.to_string() },
        })?;

    let deadline = Instant::now().checked_add(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AgentsError::Cmd { code: -1, stderr: e.to_string() });
            }
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            // Kill AND wait: an unreaped child would become a zombie in a
            // process that lives for days.
            let _ = child.kill();
            let _ = child.wait();
            return Err(AgentsError::Cmd {
                code: -1,
                stderr: format!("timed out after {}s", timeout.as_secs()),
            });
        }
        std::thread::sleep(POLL_STEP);
    }

    // The child has exited, so both pipes are at EOF; `wait_with_output` reads
    // them and returns the status `try_wait` already reaped.
    child
        .wait_with_output()
        .map_err(|e| AgentsError::Cmd { code: -1, stderr: e.to_string() })
}

/// Non-zero exit → `Cmd`. Checked BEFORE stdout is looked at, so a failing
/// command with garbage on stdout surfaces as `Cmd`, never as `Parse`.
/// A process killed by a signal has no code; it reports as `-1`.
fn check_status(out: &std::process::Output) -> Result<(), AgentsError> {
    if out.status.success() {
        return Ok(());
    }
    Err(AgentsError::Cmd {
        code: out.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

// ── Polling ─────────────────────────────────────────────────────────────────

/// `claude agents --json --all`.
/// ALWAYS passes `--all`: without it completed sessions are omitted and the
/// Completed group can never populate. Hiding completed rows is a UI concern
/// (the `a` key), not a fetch concern.
/// Measured cost 0.21s (PROBE-FINDINGS §1).
///
/// §9.3: an exit-0 `[]` is a **valid empty result**, not an error — it returns
/// an empty, COMPLETE `Payload` and the caller must clear `poll_error`.
///
/// Returns the whole `model::Payload`, not just its sessions, so the caller can
/// tell a payload that lost rows from one that genuinely shrank.
pub fn poll() -> Result<Payload, AgentsError> {
    let out = run_bounded(&["agents", "--json", "--all"], POLL_TIMEOUT)?;
    check_status(&out)?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    crate::model::parse_sessions(&stdout).map_err(AgentsError::Parse)
}

// ── Verbs ───────────────────────────────────────────────────────────────────

/// `run`, with stdout discarded and a non-zero exit turned into `Cmd`. The one
/// entry point for the two verbs that change a session's existence.
///
/// SAFETY SEAM: under `cfg(test)` this NEVER spawns. It records the argv and
/// returns a canned result instead. That is not a convenience — `delete` runs
/// `claude rm`, which removes a real worktree, and the unit suite runs on the
/// same machine as the operator's real sessions. A test that drives the `Ctrl+X`
/// keymap must not be one collision away from destroying work, so the process
/// boundary is closed in test builds by construction. `logs` and `poll` keep
/// spawning: they are read-only.
#[cfg(not(test))]
fn run_checked(args: &[&str], timeout: Option<Duration>) -> Result<(), AgentsError> {
    let out = match timeout {
        Some(t) => run_bounded(args, t)?,
        None => run(args)?,
    };
    if out.status.success() {
        return Ok(());
    }
    Err(AgentsError::Cmd {
        code: out.status.code().unwrap_or(-1),
        stderr: failure_text(
            &String::from_utf8_lossy(&out.stderr),
            &String::from_utf8_lossy(&out.stdout),
        ),
    })
}

/// Which stream carries the explanation of a failed verb.
///
/// `check_status` reads stderr, which is right for every verb but one:
/// `claude rm` REFUSES to delete a worktree holding unpushed commits or
/// uncommitted changes, and it prints that refusal on **stdout** with stderr
/// left empty, exiting 1 (PROBE-FINDINGS §2, verified 2.1.246). A stderr-only
/// reading renders the footer as `delete failed: exit 1` and drops the one
/// sentence that says the work is safe and what to do about it. stderr still
/// wins whenever it has anything to say.
fn failure_text(stderr: &str, stdout: &str) -> String {
    let e = stderr.trim();
    if !e.is_empty() {
        return e.to_string();
    }
    stdout.trim().to_string()
}

#[cfg(test)]
fn run_checked(args: &[&str], _timeout: Option<Duration>) -> Result<(), AgentsError> {
    test_spawn::intercept(args)
}

/// The `cfg(test)` stand-in for `run_checked`'s process boundary, and the
/// assertion surface the keymap tests use to prove WHICH argv a keypress built.
///
/// Thread-local: the test harness gives every `#[test]` its own thread, so each
/// test starts with an empty recorder and parallel tests cannot see each other.
#[cfg(test)]
pub mod test_spawn {
    use super::AgentsError;
    use std::cell::RefCell;

    thread_local! {
        /// Every argv `run_checked` was asked to spawn, in order.
        static CALLS: RefCell<Vec<Vec<String>>> = const { RefCell::new(Vec::new()) };
        /// A failure queued for the NEXT intercepted call.
        static FAIL_NEXT: RefCell<Option<String>> = const { RefCell::new(None) };
        /// A REAL block queued for the NEXT intercepted call; see `slow_next`.
        static SLOW_NEXT: RefCell<Option<std::time::Duration>> = const { RefCell::new(None) };
    }

    pub(super) fn intercept(args: &[&str]) -> Result<(), AgentsError> {
        CALLS.with(|c| c.borrow_mut().push(args.iter().map(|s| (*s).to_string()).collect()));
        if let Some(d) = SLOW_NEXT.with(|c| c.borrow_mut().take()) {
            std::thread::sleep(d);
        }
        match FAIL_NEXT.with(|c| c.borrow_mut().take()) {
            Some(stderr) => Err(AgentsError::Cmd { code: 1, stderr }),
            None => Ok(()),
        }
    }

    /// The argv of every state-changing `claude` call this test would have made.
    pub fn calls() -> Vec<Vec<String>> {
        CALLS.with(|c| c.borrow().clone())
    }

    /// `calls()` flattened to `"stop 1c45d64f"` form, for terse assertions.
    pub fn joined() -> Vec<String> {
        calls().into_iter().map(|c| c.join(" ")).collect()
    }

    pub fn reset() {
        CALLS.with(|c| c.borrow_mut().clear());
        FAIL_NEXT.with(|c| *c.borrow_mut() = None);
        SLOW_NEXT.with(|c| *c.borrow_mut() = None);
    }

    /// Make the next intercepted call fail with `stderr`.
    pub fn fail_next(stderr: &str) {
        FAIL_NEXT.with(|c| *c.borrow_mut() = Some(stderr.to_string()));
    }

    /// Make the next intercepted call BLOCK for `d`, the way the real process
    /// boundary does — `claude stop` measures ~0.66 s and `claude rm` ~0.71 s
    /// against a live fleet.
    ///
    /// This is the one thing the interceptor cannot fake with a backdated
    /// clock: `Ctrl+X`'s burst guard reads a real `Instant`, and the bug it
    /// exists to stop is one where the SHELL-OUT, not the operator, supplied
    /// the gap between two presses. A test that never blocks cannot see it.
    /// Callers keep the sleep as short as the guard allows.
    pub fn slow_next(d: std::time::Duration) {
        SLOW_NEXT.with(|c| *c.borrow_mut() = Some(d));
    }
}

/// `claude stop <id>` — DESTRUCTIVE, but RECOVERABLE: the conversation is kept
/// and `claude attach <id>` resumes it (PROBE-FINDINGS §2). `id` is the 8-hex
/// short id; a session without one cannot be stopped, so
/// `Session::is_attachable()` must be checked first.
///
/// STOP-THEN-ATTACH IS ALSO HOW AN AGENT CHANGES VERSION, which is the second
/// caller's whole reason for existing. The resumed worker is a genuinely NEW
/// process, and it resolves `claude` at launch, so it comes up on whatever the
/// CLI is now rather than the version the session was dispatched with
/// (PROBE-FINDINGS §2, measured). `Ctrl+X` calls this to halt a session; `R`
/// calls it to refresh one, and the pane's own `claude attach` is the resume.
///
/// Bounded by `STOP_TIMEOUT` — see the constant for why `delete` is not.
pub fn stop(id: &str) -> Result<(), AgentsError> {
    // Fail closed rather than invoking `claude stop ''`: an empty id is what an
    // unchecked `Option<String>` collapses to, and this is the destructive verb
    // (Appendix A.1 — the empty-target class of bug).
    if id.is_empty() {
        return Err(AgentsError::NotAttachable);
    }
    run_checked(&["stop", id], Some(STOP_TIMEOUT))
}

/// `claude respawn <id>` — THE RESTART THAT NEEDS NO PANE.
///
/// `stop` + `attach` is how a PANED agent changes version, and `attach` was
/// the only resume ccmux had — which is why `R`'s agent scope could never be
/// wider than the panes it was about to respawn. This is the CLI's own verb
/// for the rest: "Restart a background session (or all of them) so it picks up
/// the current Claude Code version" (`claude respawn --help`, 2.1.266). One
/// call halts the worker and starts a new one, with no client and no terminal
/// anywhere in it (PROBE-FINDINGS §2).
///
/// IT TAKES THE SHORT ID — the same 8-hex `id` `stop` and `attach` take, which
/// is the reason it replaced the `claude --bg --resume <sessionId>` pair this
/// feature was first built on. That pair had two defects this verb does not:
///
///   * `--bg --resume` has a documented FORK branch — "starts a copy and says
///     so when the session is already running" (`claude --help`) — and it
///     reports the copy on stdout with exit 0. ccmux issued it immediately
///     after its own `stop`, inside the window where the daemon still called
///     the session running, and measured ~5% of resumes came back as copies:
///     an extra live agent and a duplicated conversation, counted as a
///     success. `respawn` has no such branch (measured: same short id, same
///     `sessionId`, one row before and after, a new worker pid).
///   * Two calls meant a stop could land with its partner never sent — a
///     halted agent nothing would bring back. One call cannot.
///
/// IT DOES NOT CHECK STATE, so the caller's gates are load-bearing rather than
/// belt-and-braces. Measured 2026-09-09 on throwaways: `respawn` on a
/// `stopped` session UN-STOPS it (pid `null` -> a live pid), and `respawn` on
/// a `working` session interrupts the work mid-flight (`working`/`busy` ->
/// `blocked`/`idle`). `restart::agent_verdict` and `Session::has_worker` are
/// what keep this verb off both, and `--all` — which would hit every one of
/// them at once — is never built.
///
/// No `current_dir`: the session carries its own directory.
///
/// Bounded by `RESPAWN_TIMEOUT` — see the constant.
pub fn respawn(id: &str) -> Result<(), AgentsError> {
    // Same fail-closed guard as `stop`: an empty id is what an unchecked
    // `Option<String>` collapses to, and this verb restarts a worker.
    if id.is_empty() {
        return Err(AgentsError::NotAttachable);
    }
    run_checked(&["respawn", id], Some(RESPAWN_TIMEOUT))
}

/// WHO IS ATTACHED TO A SESSION, ANYWHERE ON THIS MACHINE.
///
/// The one question `R`'s paneless population turns on and that no tmux read
/// can answer. `claude stop` and `claude respawn` both KILL the session's
/// attach client — measured: the pane prints `Session <id> has exited.` and
/// parks — and `claude agents --json` lists every session on the box, while
/// every tmux record ccmux holds is scoped to its own tmux session (R2/R3).
/// Subtracting only what ccmux itself has open therefore left three shapes
/// classified "paneless" that are somebody's live terminal:
///
///   * a second ccmux workspace (`--session NAME`, `-L socket`) holding the
///     session in a pane of ITS window;
///   * a `claude attach` the operator ran by hand, even in ccmux's own window,
///     which is in no `@ccmux_tab_map` and so is no pane ccmux knows;
///   * any attach on another tmux server, or on no tmux at all.
///
/// So the evidence is taken from the thing that actually dies: the client
/// process. `/proc/<pid>/cmdline` is read for every process on the machine and
/// matched against the exact argv `claude attach <id>` — the shape
/// `attach_pane_cmd` builds and the shape a human types. It is READ-ONLY and
/// it can only ever REFUSE to act; nothing here authorises touching a pane, a
/// session, or a process, so R2 and R3 are untouched. It is also not §5.4's
/// deleted ancestry walk coming back: no ppid chain, no `list-panes -a`, no
/// resolving a session to a pane — one flat argv match, used to subtract.
///
/// `None` means the scan itself could not run (no `/proc`), and the caller
/// must then skip the paneless pass entirely rather than read "found nothing"
/// out of "could not look" — the fail-open reading that made this bug.
///
/// WHAT IT CANNOT SEE, stated so the next reader does not assume otherwise: an
/// attach on ANOTHER machine against a shared daemon, an attach started in the
/// instant after the scan, and a client launched through a wrapper whose
/// argv[0] is neither `claude` nor `claude_bin()`'s own name. The first two
/// are the same snapshot race every other read in this pass carries; the third
/// is why the match is on argv[0]'s file name rather than on a full path.
pub fn attached_ids() -> Option<BTreeSet<String>> {
    let bin = claude_bin();
    let want = file_name(&bin);
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        // Only the numeric entries are processes; `self`, `sys`, `net` and the
        // rest are not, and a non-UTF-8 name cannot be a pid.
        let Some(pid) = name.to_str() else { continue };
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // A process that exits between the listing and the read is not an
        // error, it is just gone — and a gone client holds nothing.
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if let Some(id) = attach_id(&raw, want) {
            out.insert(id);
        }
    }
    Some(out)
}

/// The file-name component of a program path, for `attached_ids`' argv[0]
/// match. Pure so the rule is testable without `/proc`.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The id a `/proc/<pid>/cmdline` is attached to, if it is a `claude attach`
/// client at all. NUL-separated argv, exactly as the kernel writes it.
///
/// Positive evidence only, and in the argv POSITIONS the verb has: argv[0]'s
/// file name is `claude` (or whatever `CCMUX_CLAUDE_BIN` names), argv[1] is
/// `attach`, argv[2] is the id. Matching `attach` anywhere in any argv would
/// make `grep attach …` or an editor holding this file read as a live client
/// and silently shrink the population `R` restarts.
fn attach_id(cmdline: &[u8], want: &str) -> Option<String> {
    let mut argv = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty());
    let argv0 = std::str::from_utf8(argv.next()?).ok()?;
    let base = file_name(argv0);
    if base != "claude" && base != want {
        return None;
    }
    if argv.next()? != b"attach" {
        return None;
    }
    let id = std::str::from_utf8(argv.next()?).ok()?;
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

/// `claude rm <id>` — IRREVERSIBLE. PROBE-FINDINGS §2, verbatim from
/// `claude rm --help` on 2.1.246: "Delete a background session and its
/// worktree. Unlike `stop`, works on already-exited sessions."
///
/// It takes the git worktree with it, so uncommitted work in that worktree is
/// gone. `stop` is the recoverable verb; this one is not, and `u` undoes a
/// dismissal, never this. The ONLY caller is the second `Ctrl+X` press inside
/// its window (§8.2).
///
/// Same fail-closed empty-id guard as `stop`, for the same reason: `claude rm ''`
/// must never be built.
pub fn delete(id: &str) -> Result<(), AgentsError> {
    if id.is_empty() {
        return Err(AgentsError::NotAttachable);
    }
    // Deliberately UNBOUNDED — see `STOP_TIMEOUT`. A kill halfway through a
    // worktree removal is not a timeout, it is damage.
    run_checked(&["rm", id], None)
}

/// `claude logs <id>`. Output is a RAW ANSI/PTY DUMP including alt-screen setup
/// and cursor moves (PROBE-FINDINGS §2) — always pass it through `strip_ansi`
/// before display. Returns the last `lines` lines after stripping.
pub fn logs(id: &str, lines: usize) -> Result<String, AgentsError> {
    if id.is_empty() {
        return Err(AgentsError::NotAttachable);
    }
    let out = run(&["logs", id])?;
    check_status(&out)?;
    let text = strip_ansi(&String::from_utf8_lossy(&out.stdout));
    Ok(last_lines(&text, lines))
}

/// Last `lines` lines of `text`, rejoined with `\n`. `lines == 0` → "".
///
/// SPEC NOTE: deliberately literal — no trailing-blank-line trimming. A raw PTY
/// dump ends in blank lines once the cursor-motion escapes are stripped, but
/// dropping them would change *which* lines "the last `lines` lines" names, and
/// `Mode::Logs` scrolls anyway (§6.7).
fn last_lines(text: &str, lines: usize) -> String {
    if lines == 0 {
        return String::new();
    }
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// `claude --bg <task>` executed with `current_dir(cwd)`, returning
/// immediately. Pure argv — `task` and `cwd` never touch a shell.
/// Empty `task` is rejected before the call by app.rs.
///
/// RULE Q4: the prompt is a positional argument, so arbitrary prose — quotes,
/// `$`, backticks, newlines, `;` — is inert.
///
/// SPEC NOTE: uses `output()` rather than `spawn()`. Appendix A verified that
/// `claude --bg` dispatches and returns immediately, so the block is
/// negligible; in exchange the child is reaped and a non-zero exit surfaces as
/// `Cmd` with real stderr, which `spawn()` without a `wait` discards.
pub fn dispatch_background(cwd: &str, task: &str) -> Result<(), AgentsError> {
    let bin = claude_bin();
    let out = Command::new(&bin)
        .arg("--bg")
        .arg(task)
        .current_dir(cwd)
        .output()
        .map_err(|e| match e.kind() {
            // `current_dir` on a missing directory also reports NotFound; app.rs
            // validates `is_dir` first (§8.6), so name the binary here.
            std::io::ErrorKind::NotFound => AgentsError::NotFound(bin.clone()),
            _ => AgentsError::Cmd {
                code: -1,
                stderr: e.to_string(),
            },
        })?;
    check_status(&out)?;
    Ok(())
}

// ── Pane command templates (shell strings; see §7) ──────────────────────────

/// Shell command for a pane that attaches a background session.
///
/// `claude attach` is a raw-mode TUI, so `Ctrl+Z` reaches it as the byte 0x1A
/// and it treats that as DETACH: it exits, rc 0, with no signal involved
/// (PROBE-FINDINGS §2). The pane therefore outlives the attach on the
/// operator's most ordinary keypress, and what it outlives it into is this
/// template's business. It latches `@ccmux_detached`, prints the outcome and
/// the exact command that resumes the session, and then PARKS on one `read`:
///
///   * enter — or anything not listed below — clears the latch and re-runs the
///     attach. That is the resume, one key instead of a retyped command. It is
///     still not `fg`: nothing was ever stopped, so this is a fresh attach to
///     the same session, which is legal (PROBE §3).
///   * `s` — `exec` the operator's login shell over the pane, which is the
///     landing the operator asked for; see below for why it is not automatic.
///   * `q`, or EOF — leave with the attach's own rc and let the pane close.
///
/// NOTHING HERE STARTS AN INTERACTIVE SHELL ON ITS OWN, and that is the whole
/// reason there is a loop rather than a straight `exec`. An interactive shell
/// runs the operator's startup files; those files run in a pane whose `$TMUX`
/// names THE SERVER CCMUX IS HOLDING; and a startup file is under no obligation
/// to be careful with it. Measured on the author's machine (tmux 3.4, throwaway
/// socket): one `exec "$SHELL" -l` in a pane took that server from 1 session
/// and 2 panes to 3 sessions and 15, because `~/.zshrc` reaches a helper that
/// runs `tmux new -s dev -d` and then tmux-resurrect's `restore.sh` against
/// whatever server it finds — restoring a saved workspace over the live ccmux
/// session, renaming its window and overwriting its layout. Dropping `-l` does
/// not help: an interactive zsh reads `~/.zshrc` either way.
///
/// ccmux cannot vet startup files, so it does not run them. It hands the pane
/// over when the operator asks, which is the same consent as typing `zsh`, and
/// on no other path. This is the only place the question can arise: every pane
/// ccmux creates is created WITH a command (`create_session`, `new_tab`,
/// `split`, `respawn_pane`), so before this template no ccmux server had ever
/// run an interactive shell. It is also the blast radius (R1-R4) reaching one
/// step further than `assert_in_session` can: a shell ccmux started can name
/// any session it likes through a tmux call ccmux never makes.
///
/// Produces exactly this, as ONE line (wrapped here to be read), with
/// `<claude>` and `<id>` through `sh_quote` — twice over for the two that are
/// printed rather than run:
///
///   while :; do
///   [ -n "$TMUX_PANE" ] && tmux set-option -p -u -t "$TMUX_PANE" @ccmux_detached 2>/dev/null;
///   <claude> attach <id>; rc=$?;
///   [ -n "$TMUX_PANE" ] && tmux set-option -p -t "$TMUX_PANE" @ccmux_detached 1 2>/dev/null;
///   printf '\n[ccmux] attach exited (rc=%s). resume: %s attach %s\n' "$rc" <claude> <id>;
///   printf '[ccmux] enter=resume  s=shell  q=close pane: ';
///   read ans || ans=q;
///   case "$ans" in s|S) break;; q|Q) exit "$rc";; esac;
///   done;
///   [ -n "$TMUX_PANE" ] && tmux set-option -p -t "$TMUX_PANE" @ccmux_detached shell 2>/dev/null;
///   printf '\n'; [ -x "${SHELL:-}" ] || SHELL=/bin/sh; exec "$SHELL" -l
///
/// The two values reach `printf` as ARGUMENTS, never inside its format string,
/// so a `%` in an overridden `CCMUX_CLAUDE_BIN` is inert. They are `sh_quote`d
/// TWICE for that half, and once for the attach itself: the shell strips one
/// layer on the way to `printf`, so a single quoting would PRINT
/// `/opt/my tools/claude` — an instruction that runs `/opt/my` when pasted
/// back. The attach half is unaffected either way; only the human-facing line
/// needed the second layer.
///
/// THE `tmux set-option` IS THE LATCH, and it is why the map can stop lying.
/// `#{pane_current_command}` cannot answer "is the attach still running":
/// tmux reads the pane's FOREGROUND PROCESS GROUP LEADER (`tcgetpgrp`), and
/// tmux runs this whole string as `$SHELL -c`, which — being non-interactive —
/// never hands the terminal to its child. Verified on tmux 3.4: a pane running
/// `claude attach <id>; …` reports `zsh`, exactly as the shell that replaces it
/// afterwards does; only an `exec`ed command (`exec sleep 60` → `sleep`) is
/// visible. So the pane states its own transition instead: a PANE-scoped user
/// option, which dies with the pane, is written before the pane parks and rides
/// back to ccmux inside the `list-panes` of §5.3 at zero extra cost.
///
/// THE LATCH IS CLEARED AT THE TOP OF THE LOOP, immediately before the attach,
/// and nowhere else. ccmux itself never clears it — it cannot, because
/// `#{pane_current_command}` would say `claude` for an attach to any session at
/// all (PROBE-FINDINGS §4) — but this template knows which session it is about
/// to attach, because it is the one baked into the string. The rule it enforces
/// is therefore exact: **while this wrapper is running its attach, the pane is
/// attached**, and the latch says so.
///
/// The clear USED to sit after the `read`, on the resume path. Same effect for
/// the operator's enter, and wrong for the other way a fresh attach reaches a
/// latched pane: `R` respawns this command into a pane it has just stopped the
/// agent of, and `respawn-pane -k` keeps PANE OPTIONS. So the wrapper R killed
/// had latched the pane on its way out, the new wrapper attached perfectly
/// well, and the pane was left permanently marked as parked — with no open
/// marker in the sidebar and, next `R`, skipped as "the operator's shell".
/// Measured live: after an `R` that restarted its agent, `%1` held a healthy
/// `claude attach` and `#{@ccmux_detached}` was `1`. Clearing on the way IN
/// closes that by construction, whoever started the wrapper.
///
/// A pane the operator hand-attaches from the `s` shell still does not get its
/// mapping back, and must not: `s` breaks OUT of the loop, so the clear is
/// never reached again and the latch stands for the life of that shell.
///
/// THE LATCH IS RE-WRITTEN AS `shell` ON THE WAY OUT OF THE LOOP, after `done`
/// and before the `exec` — the one place that is reached by `s` and by nothing
/// else, since `q` exits and enter loops. It is the pane's statement that it
/// is no longer a prompt but the operator's shell, and it exists for one
/// reader: `Ctrl+X`'s delete (§8.2) closes every pane still parked on a prompt
/// whose resume can no longer work, and a shell is not that — the operator
/// may be running something in it, or have hand-attached another session
/// from it, and killing it would take their foreground job with it. Measured
/// before this mark existed: a `sleep 600` started from the `s` shell of a
/// session's pane died with the pane on that session's delete, and so did a
/// pane hand-attached to a DIFFERENT session from the same shell. Both
/// values still read as detached to everything else — the marker, the badge
/// and `R`'s skip are unchanged — and `x` still closes a shell pane on the
/// operator's say-so.
///
/// `[ -n "$TMUX_PANE" ]` is not belt and braces. `set-option -p` with an EMPTY
/// `-t` does not fail — verified on 3.4, rc 0 — it resolves to the session's
/// ACTIVE pane, so an absent `$TMUX_PANE` would latch whichever pane the
/// operator happens to be looking at, quite possibly a live attach, and retire
/// its mapping. The guard is what makes the fallback "no latch at all", which
/// is the pre-existing behaviour and always safe. `2>/dev/null` is left for the
/// other case it really does cover: a tmux too old for pane options.
///
/// The shell — and the parked prompt before it — inherits the pane's cwd;
/// nothing `cd`s. That is the directory the operator launched ccmux from
/// (tmux's split inherits it) and it certainly exists. The session's own cwd is
/// deliberately NOT used: `restart::plan` rebuilds this exact string from a
/// `PaneEntry` that carries only a short id, so a landing directory read off
/// `model::Session` would differ depending on whether the pane came from
/// `o`/`s`/`t` or from `R` — and a background session's real directory is a
/// `.claude/worktrees/…` checkout the daemon owns, which is not somewhere to
/// drop an operator with a live agent in it.
pub fn attach_pane_cmd(id: &str) -> String {
    let bin = sh_quote(&claude_bin());
    let id = sh_quote(id);
    // Quoted a second time because these two are PRINTED, not run: the shell
    // strips one layer handing them to `printf`, and what survives is what the
    // operator is told to paste.
    let bin_shown = sh_quote(&bin);
    let id_shown = sh_quote(&id);
    // The `\n` inside the printf formats are LITERAL backslash-n for printf to
    // interpret, not Rust newlines.
    format!(
        "while :; do \
         [ -n \"$TMUX_PANE\" ] && tmux set-option -p -u -t \"$TMUX_PANE\" {opt} 2>/dev/null; \
         {bin} attach {id}; rc=$?; \
         [ -n \"$TMUX_PANE\" ] && tmux set-option -p -t \"$TMUX_PANE\" {opt} 1 2>/dev/null; \
         printf '\\n[ccmux] attach exited (rc=%s). resume: %s attach %s\\n' \"$rc\" {bin_shown} {id_shown}; \
         printf '[ccmux] enter=resume  s=shell  q=close pane: '; \
         read ans || ans=q; \
         case \"$ans\" in s|S) break;; q|Q) exit \"$rc\";; esac; \
         done; \
         [ -n \"$TMUX_PANE\" ] && tmux set-option -p -t \"$TMUX_PANE\" {opt} {shell} 2>/dev/null; \
         printf '\\n'; \
         [ -x \"${{SHELL:-}}\" ] || SHELL=/bin/sh; \
         exec \"$SHELL\" -l",
        opt = OPT_PANE_DETACHED,
        shell = LATCH_SHELL,
    )
}

// ── ANSI ────────────────────────────────────────────────────────────────────

/// Strip CSI (`ESC [ ... final`), OSC (`ESC ] ... BEL | ESC \`), and two-char
/// `ESC <byte>` sequences; drop remaining C0 controls except `\n` and `\t`;
/// normalize `\r\n` and lone `\r` to `\n`. Pure, allocation-only, no regex dep.
pub fn strip_ansi(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.chars().peekable();

    while let Some(c) = it.next() {
        match c {
            // `\r` is normalized BEFORE the drop-C0 rule below, otherwise it
            // would be swallowed as a control instead of becoming `\n`.
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                out.push('\n');
            }
            '\u{1b}' => match it.peek().copied() {
                // CSI: ESC [ , params 0x30..=0x3F, intermediates 0x20..=0x2F,
                // terminated by a final byte 0x40..=0x7E. Dispatched before the
                // generic two-char rule so a CSI is never half-eaten.
                Some('[') => {
                    it.next();
                    for b in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&b) {
                            break;
                        }
                    }
                    // Unterminated CSI at end of input: consumed, nothing emitted.
                }
                // OSC: ESC ] ... terminated by BEL or ST (ESC \).
                Some(']') => {
                    it.next();
                    while let Some(b) = it.next() {
                        if b == '\u{7}' {
                            break;
                        }
                        if b == '\u{1b}' {
                            if it.peek() == Some(&'\\') {
                                it.next();
                            }
                            break;
                        }
                    }
                }
                // Two-char `ESC <byte>` (charset selects, RIS, IND, NEL, ...).
                Some(_) => {
                    it.next();
                }
                // Trailing lone ESC: dropped.
                None => {}
            },
            '\n' | '\t' => out.push(c),
            // Remaining C0 controls and DEL are dropped; everything else,
            // including all non-ASCII, is kept verbatim.
            _ if (c as u32) < 0x20 || c == '\u{7f}' => {}
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── §10.1 agents.rs ─────────────────────────────────────────────────────

    // NOTE: `claude_bin()` is "claude" unless CCMUX_CLAUDE_BIN is set, and
    // `sh_quote("claude")` is the identity (§7: it matches the safe charset), so
    // the expected strings below are the §3.3 templates verbatim.

    #[test]
    fn attach_pane_cmd_matches_spec_template() {
        assert_eq!(
            attach_pane_cmd("1c45d64f"),
            "while :; do \
             [ -n \"$TMUX_PANE\" ] && tmux set-option -p -u -t \"$TMUX_PANE\" @ccmux_detached 2>/dev/null; \
             claude attach 1c45d64f; rc=$?; \
             [ -n \"$TMUX_PANE\" ] && tmux set-option -p -t \"$TMUX_PANE\" @ccmux_detached 1 2>/dev/null; \
             printf '\\n[ccmux] attach exited (rc=%s). resume: %s attach %s\\n' \"$rc\" claude 1c45d64f; \
             printf '[ccmux] enter=resume  s=shell  q=close pane: '; \
             read ans || ans=q; \
             case \"$ans\" in s|S) break;; q|Q) exit \"$rc\";; esac; \
             done; \
             [ -n \"$TMUX_PANE\" ] && tmux set-option -p -t \"$TMUX_PANE\" @ccmux_detached shell 2>/dev/null; \
             printf '\\n'; \
             [ -x \"${SHELL:-}\" ] || SHELL=/bin/sh; \
             exec \"$SHELL\" -l"
        );
    }

    /// THE SHELL MARK. The only way past `done` is the operator's `s` (`q`
    /// exits, enter loops), so a write there says exactly "this pane is now a
    /// shell" — and it must sit AFTER the loop, so a resumed attach never
    /// carries it, and BEFORE the `exec`, so the shell never starts unmarked.
    /// It is the value §8.2's delete reads to leave the pane alone, and it is
    /// written once: a second write anywhere would be a second opinion.
    #[test]
    fn attach_pane_cmd_marks_the_pane_as_a_shell_on_the_way_out_of_the_loop() {
        let cmd = attach_pane_cmd("1c45d64f");
        let mark = format!("tmux set-option -p -t \"$TMUX_PANE\" {OPT_PANE_DETACHED} {LATCH_SHELL} 2>/dev/null");
        let at = cmd.find(&mark).expect("shell mark");
        let done = cmd.find("; done; ").expect("loop end");
        let exec = cmd.find("exec \"$SHELL\"").expect("exec");
        assert!(done < at && at < exec, "{cmd}");
        assert_eq!(cmd.matches(&mark).count(), 1, "one mark, in one place: {cmd}");
        // And the parked value is still what the loop writes, before the
        // prompt — the two are different states and stay different values.
        assert!(cmd.contains(&format!("{OPT_PANE_DETACHED} 1 2>/dev/null")), "{cmd}");
        assert_ne!(LATCH_SHELL, "1");
    }

    /// The template ends in `exec`, and that is the whole point: once the
    /// operator has asked for the shell, the pane must hold the shell and not a
    /// wrapper waiting behind it. A `read`, a `wait`, or anything after the
    /// `exec` would leave the process the attach ran under sitting on the pane
    /// forever.
    #[test]
    fn attach_pane_cmd_ends_by_execing_an_interactive_login_shell() {
        let cmd = attach_pane_cmd("1c45d64f");
        assert!(cmd.ends_with("exec \"$SHELL\" -l"), "{cmd}");
        assert!(!cmd.contains("read _"), "the old close-the-pane trailer is gone: {cmd}");
        // $SHELL, with a fallback that cannot be an empty program name.
        assert!(cmd.contains("[ -x \"${SHELL:-}\" ] || SHELL=/bin/sh"), "{cmd}");
    }

    /// THE HAZARD THAT KEEPS THE `exec` BEHIND A KEY. An interactive shell runs
    /// the operator's startup files, in a pane whose `$TMUX` names the server
    /// ccmux is holding. Measured on tmux 3.4: one unconditional
    /// `exec "$SHELL" -l` in a pane took a throwaway server from 1 session /
    /// 2 panes to 3 sessions / 15, because the rc chain reaches
    /// `tmux new -s dev -d` plus tmux-resurrect's `restore.sh` and restores a
    /// saved workspace over the live ccmux session. ccmux cannot vet startup
    /// files, so the only safe rule is that it never runs them unasked: every
    /// path from the attach's exit to the `exec` must go through the `read`.
    #[test]
    fn attach_pane_cmd_never_reaches_the_shell_without_a_keystroke() {
        let cmd = attach_pane_cmd("1c45d64f");
        let attach = cmd.find("claude attach").expect("attach");
        let read = cmd.find("read ans || ans=q").expect("read");
        let exec = cmd.find("exec \"$SHELL\"").expect("exec");
        assert!(attach < read && read < exec, "{cmd}");
        // And the only way out of the loop towards it is the operator's `s`.
        assert!(cmd.contains("case \"$ans\" in s|S) break;; q|Q) exit \"$rc\";; esac"), "{cmd}");
        let done = cmd.find("; done; ").expect("loop end");
        assert!(done < exec, "the exec is outside the loop: {cmd}");
        assert_eq!(cmd.matches("exec \"$SHELL\"").count(), 1, "one shell handover only: {cmd}");
    }

    /// THE INVARIANT: while this wrapper is running its attach, the pane is not
    /// latched. The clear sits at the TOP of the loop, before the attach, so it
    /// holds on every path that reaches an attach — the first launch, the
    /// operator's enter, and an `R` that respawned this command into a pane a
    /// previous wrapper had latched on its way out (`respawn-pane -k` keeps
    /// pane options, so that latch would otherwise stand over a healthy
    /// attach). Enter still re-runs the SAME attach; the loop is what does that.
    #[test]
    fn attach_pane_cmd_clears_the_latch_before_every_attach() {
        let cmd = attach_pane_cmd("1c45d64f");
        let clear = cmd
            .find(&format!(
                "[ -n \"$TMUX_PANE\" ] && tmux set-option -p -u -t \"$TMUX_PANE\" {OPT_PANE_DETACHED} 2>/dev/null"
            ))
            .expect("latch clear");
        let attach = cmd.find("claude attach 1c45d64f;").expect("attach");
        let read = cmd.find("read ans || ans=q").expect("read");
        let done = cmd.find("; done; ").expect("loop end");
        // Clear, then attach — first thing in the body, so it runs however the
        // wrapper got here.
        assert!(cmd.starts_with("while :; do [ -n \"$TMUX_PANE\""), "{cmd}");
        assert!(clear < attach, "{cmd}");
        // And nothing clears it after the prompt: `s` breaks out of the loop,
        // so a pane handed to the operator's shell stays latched forever.
        assert!(read < done, "{cmd}");
        assert_eq!(
            cmd.matches("set-option -p -u").count(),
            1,
            "one clear, in one place: {cmd}"
        );
    }

    /// A failed attach must still be readable once the prompt is on screen —
    /// §9.4's race puts a real `rc=1` here — so `$rc` is captured immediately
    /// after the attach, printed verbatim, and carried out as the pane's own
    /// exit status when the operator closes it.
    #[test]
    fn attach_pane_cmd_surfaces_the_exit_code() {
        let cmd = attach_pane_cmd("1c45d64f");
        assert!(cmd.contains("attach 1c45d64f; rc=$?;"), "{cmd}");
        assert!(cmd.contains("(rc=%s)"), "{cmd}");
        assert!(cmd.contains("\"$rc\""), "{cmd}");
        assert!(cmd.contains("q|Q) exit \"$rc\""), "{cmd}");
    }

    /// The latch is what stops `@ccmux_tab_map` claiming the pane once the
    /// attach is gone, so it must name the option `PANE_FMT` reads back, be
    /// PANE-scoped, and target the pane it runs in.
    #[test]
    fn attach_pane_cmd_latches_the_pane_option_on_itself() {
        let cmd = attach_pane_cmd("1c45d64f");
        assert!(
            cmd.contains(&format!(
                "tmux set-option -p -t \"$TMUX_PANE\" {OPT_PANE_DETACHED} 1 2>/dev/null"
            )),
            "{cmd}"
        );
        // Before the prompt, so a pane that is parked has always latched. The
        // clear is also a `set-option`, so the latch is found by its own text.
        let latch = cmd
            .find(&format!(
                "tmux set-option -p -t \"$TMUX_PANE\" {OPT_PANE_DETACHED} 1 2>/dev/null"
            ))
            .expect("latch");
        let prompt = cmd.find("printf '[ccmux] enter=resume").expect("prompt");
        assert!(latch < prompt, "{cmd}");
    }

    /// REGRESSION. `set-option -p` with an EMPTY `-t` does not fail — measured
    /// on tmux 3.4, rc 0 — it resolves to the session's ACTIVE pane. So a pane
    /// with no `$TMUX_PANE` would not degrade to "no latch": it would latch
    /// whatever the operator is looking at, and retire a live attach's mapping.
    /// `2>/dev/null` never covered that; the guard does.
    #[test]
    fn attach_pane_cmd_never_latches_a_pane_it_cannot_name() {
        let cmd = attach_pane_cmd("1c45d64f");
        for op in [
            format!("tmux set-option -p -t \"$TMUX_PANE\" {OPT_PANE_DETACHED} 1"),
            format!("tmux set-option -p -u -t \"$TMUX_PANE\" {OPT_PANE_DETACHED}"),
            format!("tmux set-option -p -t \"$TMUX_PANE\" {OPT_PANE_DETACHED} {LATCH_SHELL}"),
        ] {
            let at = cmd.find(&op).unwrap_or_else(|| panic!("{op} missing from {cmd}"));
            assert!(
                cmd[..at].ends_with("[ -n \"$TMUX_PANE\" ] && "),
                "unguarded {op} in {cmd}"
            );
        }
        assert_eq!(cmd.matches("tmux set-option").count(), 3, "{cmd}");
    }

    /// RULE Q4 at the one boundary that has a shell on the other side. The id
    /// reaches `printf` as an ARGUMENT, never inside its format string, so a
    /// `%` in it is inert as well as unquoted-safe.
    #[test]
    fn attach_pane_cmd_quotes_a_hostile_id() {
        let cmd = attach_pane_cmd("a'; rm -rf ~; echo '%s");
        assert!(
            cmd.contains("; claude attach 'a'\\''; rm -rf ~; echo '\\''%s'; rc=$?"),
            "{cmd}"
        );
        // The format string is a fixed literal: exactly three conversions.
        let fmt_start = cmd.find("printf '").expect("printf") + "printf '".len();
        let fmt_end = fmt_start + cmd[fmt_start..].find("' ").expect("format ends");
        assert_eq!(cmd[fmt_start..fmt_end].matches("%s").count(), 3);
        // The prompt behind it is a format string too, and it takes no
        // arguments at all — so it must carry no conversions either.
        let p2 = cmd.find("printf '[ccmux] enter=resume").expect("prompt") + "printf '".len();
        let p2_end = p2 + cmd[p2..].find("'; ").expect("prompt ends");
        assert!(!cmd[p2..p2_end].contains('%'), "{cmd}");
    }

    /// REGRESSION. The resume line is an INSTRUCTION, and the shell strips one
    /// layer of quoting on the way to `printf`, so a value that needed quoting
    /// used to be PRINTED bare: `/opt/my tools/claude attach id` runs
    /// `/opt/my` when pasted back. The printed copy is quoted twice, so what
    /// reaches the screen is still a single shell word. The attach itself was
    /// never affected — that half is quoted once and never printed — which is
    /// why this shows up only in the human-facing half.
    #[test]
    fn attach_pane_cmd_prints_a_resume_line_that_can_be_pasted_back() {
        let cmd = attach_pane_cmd("a b");
        // Run: one layer, so the shell sees `attach 'a b'`.
        assert!(cmd.contains("; claude attach 'a b'; rc=$?"), "{cmd}");
        // Printed: two, so the shell hands `printf` the literal text `'a b'`.
        assert!(cmd.contains(r#""$rc" claude ''\''a b'\''';"#), "{cmd}");
        assert_eq!(sh_quote(&sh_quote("a b")), r"''\''a b'\'''");

        // And an ordinary id is untouched by either layer, so the common line
        // reads exactly as it always did.
        let plain = attach_pane_cmd("1c45d64f");
        assert!(plain.contains("resume: %s attach %s\\n' \"$rc\" claude 1c45d64f;"), "{plain}");
    }

    #[test]
    fn strip_ansi_removes_an_alt_screen_preamble_entirely() {
        assert_eq!(strip_ansi("\x1b[?1049h\x1b[H\x1b[2J"), "");
    }

    #[test]
    fn strip_ansi_removes_csi_osc_and_two_char_escapes_and_keeps_newlines() {
        // `\x1b7` is DECSC — a genuine two-char `ESC <byte>` sequence.
        let raw = "\x1b[1;32mgreen\x1b[0m\n\x1b]0;title\x07plain\n\x1b7tail";
        assert_eq!(strip_ansi(raw), "green\nplain\ntail");
    }

    #[test]
    fn strip_ansi_leaves_the_final_byte_of_a_three_char_escape() {
        // SPEC NOTE (§3.3): the rule is CSI, OSC, and *two-char* `ESC <byte>`.
        // A charset designator like `ESC ( B` is three bytes, so the literal
        // rule consumes `ESC (` and leaves the `B`. Implemented as specified;
        // reported to the Integrator as a spec question, not fixed unilaterally.
        assert_eq!(strip_ansi("\x1b(Btail"), "Btail");
    }

    #[test]
    fn strip_ansi_handles_osc_terminated_by_st() {
        assert_eq!(strip_ansi("a\x1b]8;;http://x\x1b\\b"), "ab");
    }

    #[test]
    fn strip_ansi_normalizes_carriage_returns() {
        assert_eq!(strip_ansi("a\r\nb\rc"), "a\nb\nc");
    }

    #[test]
    fn strip_ansi_drops_other_c0_but_keeps_tab_and_unicode() {
        assert_eq!(strip_ansi("a\x00b\x07c\td\u{7f}e\u{2026}"), "abc\tde\u{2026}");
    }

    #[test]
    fn strip_ansi_survives_truncated_sequences() {
        assert_eq!(strip_ansi("keep\x1b[1;3"), "keep");
        assert_eq!(strip_ansi("keep\x1b]0;unterminated"), "keep");
        assert_eq!(strip_ansi("keep\x1b"), "keep");
    }

    #[test]
    fn strip_ansi_is_identity_on_clean_text() {
        let s = "plain text\nwith\ttabs\nand ünïcode";
        assert_eq!(strip_ansi(s), s);
    }

    // ── supporting behaviour ────────────────────────────────────────────────

    #[test]
    fn claude_bin_defaults_to_claude() {
        // The OnceLock is process-wide; this asserts the default only when the
        // test runner itself has no override in the environment.
        if std::env::var("CCMUX_CLAUDE_BIN").is_err() {
            assert_eq!(claude_bin(), "claude");
        }
    }

    #[test]
    fn last_lines_takes_the_tail() {
        assert_eq!(last_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(last_lines("a\nb\nc", 9), "a\nb\nc");
        assert_eq!(last_lines("a\nb\nc", 0), "");
        assert_eq!(last_lines("", 5), "");
    }

    #[test]
    fn display_is_footer_ready() {
        assert_eq!(
            AgentsError::NotFound("claude".into()).to_string(),
            "claude not found on PATH"
        );
        assert_eq!(
            AgentsError::Cmd {
                code: 1,
                stderr: "\n  daemon is not running\nsecond line".into(),
            }
            .to_string(),
            "daemon is not running"
        );
        assert_eq!(
            AgentsError::Cmd {
                code: 2,
                stderr: "   ".into()
            }
            .to_string(),
            "exit 2"
        );
        assert_eq!(
            AgentsError::Parse(ParseError::Json {
                msg: "expected value".into(),
                excerpt: "oops".into(),
            })
            .to_string(),
            "bad json: oops"
        );
        assert_eq!(
            AgentsError::Parse(ParseError::NotAnArray).to_string(),
            "bad json: not a JSON array"
        );
    }

    /// Live smoke check against the real CLI. `#[ignore]`d so CI (and anyone
    /// without a running daemon) never spawns `claude`.
    ///
    /// READ-ONLY BY CONSTRUCTION: it runs `agents --json --all` and nothing
    /// else. No test in this module ever calls `stop`, and none ever will.
    ///
    ///   cargo test -- --ignored live_poll
    #[test]
    #[ignore]
    fn live_poll_parses_the_real_payload() {
        let payload = poll().expect("claude agents --json --all");
        for s in &payload.sessions {
            assert!(!s.session_id.is_empty());
            assert_eq!(s.is_attachable(), s.id.is_some());
        }
        eprintln!(
            "live_poll: {} sessions, {} rows dropped",
            payload.sessions.len(),
            payload.dropped
        );
    }

    /// Live check on the attach scan, against the real machine. `#[ignore]`d
    /// so CI never depends on a `/proc` or on who happens to be attached.
    ///
    /// READ-ONLY BY CONSTRUCTION: it reads `/proc/<pid>/cmdline` and nothing
    /// else, spawns nothing, and touches no session. What it pins is the one
    /// thing a unit test on `attach_id` cannot — that the scan RUNS here, and
    /// that every id it reports has the shape `claude attach` takes, so a
    /// wrong match cannot quietly shrink the population `R` restarts.
    ///
    ///   cargo test -- --ignored live_attach_scan --nocapture
    #[test]
    #[ignore]
    fn live_attach_scan() {
        let ids = attached_ids().expect("this host has a readable /proc");
        eprintln!("live attach clients: {ids:?}");
        for id in &ids {
            assert!(
                !id.is_empty() && id.len() <= 64 && !id.contains('/'),
                "not an id a `claude attach` would take: {id:?}"
            );
        }
    }

    /// THE DRIFT GUARD, live half (see `App::note_drift` for the other).
    ///
    /// `state: "stopped"` and then `state: "blocked"` both shipped unmodelled,
    /// and both times the CLI had been emitting the value for a while before
    /// anyone noticed a purple `?` on a row. This asks the real fleet the
    /// question directly: is there a `state` or `status` word out there that
    /// this build cannot name? A failure here is not a bug in ccmux's logic —
    /// it is ccmux being older than the CLI, and the fix is to add the variant.
    ///
    /// `#[ignore]`d, so it shells out ONLY when run by name, and READ-ONLY by
    /// construction: `agents --json --all` and nothing else. It is worth
    /// running after every `claude` upgrade.
    ///
    ///   cargo test -- --ignored live_state_and_status
    ///
    /// It can only see states the fleet happens to be in right now, which is
    /// why it is the second half of the guard and not the whole of it: the
    /// runtime warning catches what a snapshot misses.
    #[test]
    #[ignore]
    fn live_state_and_status_values_are_all_modelled() {
        use crate::model::{State, Status};
        let payload = poll().expect("claude agents --json --all");
        let mut unmodelled: Vec<String> = Vec::new();
        for s in &payload.sessions {
            if let Some(State::Unknown(v)) = &s.state {
                unmodelled.push(format!("state {v:?} (session {})", s.session_id));
            }
            // An ABSENT `status` key parses to `Unknown("")` and is not drift:
            // every `state: "done"` row omits it.
            if let Status::Unknown(v) = &s.status
                && !v.is_empty()
            {
                unmodelled.push(format!("status {v:?} (session {})", s.session_id));
            }
        }
        eprintln!(
            "live fleet: {} sessions, {} unmodelled value(s)",
            payload.sessions.len(),
            unmodelled.len()
        );
        assert!(
            unmodelled.is_empty(),
            "the CLI reports state/status values this build does not model — \
             add the variant to `model::State`/`model::Status`, give it a group \
             and a glyph, and do NOT leave it in `Unknown`:\n  {}",
            unmodelled.join("\n  ")
        );
    }

    #[test]
    fn stop_delete_and_logs_refuse_an_empty_id_without_spawning() {
        // Fail-closed guard: never `claude stop ''`, never `claude rm ''`,
        // never `claude respawn ''`.
        assert!(matches!(stop(""), Err(AgentsError::NotAttachable)));
        assert!(matches!(delete(""), Err(AgentsError::NotAttachable)));
        assert!(matches!(logs("", 10), Err(AgentsError::NotAttachable)));
        assert!(matches!(respawn(""), Err(AgentsError::NotAttachable)));
        assert!(test_spawn::calls().is_empty(), "an empty id must not reach the boundary");
    }

    /// THE PANELESS RESTART'S ARGV, PINNED, and the pinning is the point.
    ///
    /// `claude respawn <shortId>` and nothing else. Two argvs it must never be:
    /// `--all`, which restarts every background session on the machine —
    /// dormant, stopped and working alike — and is the exact opposite of a pass
    /// built on per-session gates; and `--bg --resume <uuid>`, whose documented
    /// fork branch exits 0 while starting a COPY of the conversation, which
    /// ccmux would have counted as a success (PROBE-FINDINGS §2).
    #[test]
    fn the_paneless_restart_names_one_short_id_and_nothing_else() {
        test_spawn::reset();
        assert!(respawn("4fc47ebd").is_ok());
        assert_eq!(test_spawn::joined(), vec!["respawn 4fc47ebd"]);
        let argv = test_spawn::calls();
        assert!(
            !argv.iter().any(|c| c.iter().any(|a| a == "--all" || a == "--resume")),
            "`R` must never build `respawn --all` or `--bg --resume`: {argv:?}"
        );
    }

    /// THE ATTACH-CLIENT MATCH, in the argv positions the verb actually has.
    ///
    /// This decides whether a live session is somebody's terminal, and both
    /// halves of getting it wrong are damage: too loose and `R` silently stops
    /// restarting agents that nobody has open, too tight and it kills the
    /// operator's attach in another window.
    #[test]
    fn an_attach_client_is_recognised_by_argv_and_nothing_looser() {
        let cmd = |parts: &[&str]| parts.join("\0").into_bytes();

        assert_eq!(attach_id(&cmd(&["claude", "attach", "1c45d64f"]), "claude").as_deref(), Some("1c45d64f"));
        // A full path is what `CCMUX_CLAUDE_BIN` produces, and what the shell
        // records when the operator types one.
        assert_eq!(
            attach_id(&cmd(&["/home/dev/.local/bin/claude", "attach", "1c45d64f"]), "claude").as_deref(),
            Some("1c45d64f")
        );
        // An override with its own name still matches — on the file name, so
        // the same binary reached by two paths is one client.
        assert_eq!(
            attach_id(&cmd(&["/opt/x/claude-dev", "attach", "1c45d64f"]), "claude-dev").as_deref(),
            Some("1c45d64f")
        );
        // The trailing NUL the kernel writes must not become a fourth argv.
        assert_eq!(
            attach_id(b"claude\0attach\x001c45d64f\0", "claude").as_deref(),
            Some("1c45d64f")
        );

        for (why, argv) in [
            ("another verb", cmd(&["claude", "stop", "1c45d64f"])),
            ("no id", cmd(&["claude", "attach"])),
            ("an empty id", cmd(&["claude", "attach", ""])),
            ("another program", cmd(&["grep", "attach", "1c45d64f"])),
            // The shape that would make an editor holding this source file, or
            // a `grep`, read as a live client and silently shrink the
            // population `R` restarts.
            ("`attach` anywhere but argv[1]", cmd(&["claude", "logs", "attach", "1c45d64f"])),
            ("nothing at all", Vec::new()),
        ] {
            assert!(attach_id(&argv, "claude").is_none(), "matched {why}: {argv:?}");
        }
    }

    /// PROBE-FINDINGS §2: a refused `claude rm` exits 1, says why on STDOUT,
    /// and leaves stderr empty. Reading stderr alone renders `exit 1`.
    #[test]
    fn a_failure_that_explains_itself_on_stdout_is_still_readable() {
        assert_eq!(
            failure_text("", "kept 35f940dd — worktree has commits that are not pushed anywhere\n  worktree kept at /tmp/x"),
            "kept 35f940dd — worktree has commits that are not pushed anywhere\n  worktree kept at /tmp/x"
        );
        // stderr still wins whenever it has anything to say.
        assert_eq!(failure_text(" boom \n", "noise"), "boom");
        assert_eq!(failure_text("   ", "   "), "");
    }

    #[test]
    fn the_two_verbs_are_pure_argv_and_name_the_documented_subcommands() {
        test_spawn::reset();
        assert!(stop("1c45d64f").is_ok());
        assert!(delete("1c45d64f").is_ok());
        // RULE Q4: positional argv, no shell, no flags invented on the side.
        assert_eq!(test_spawn::joined(), vec!["stop 1c45d64f", "rm 1c45d64f"]);

        // A non-zero exit surfaces through the same footer-ready Display both
        // verbs already share.
        test_spawn::fail_next("no such session: 1c45d64f");
        let err = delete("1c45d64f").expect_err("queued failure");
        assert_eq!(err.to_string(), "no such session: 1c45d64f");
    }
}
