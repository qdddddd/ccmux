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
use std::time::{Duration, Instant};

use crate::model::{ParseError, Payload};
use crate::tmux::{OPT_PANE_DETACHED, sh_quote};

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
const POLL_TIMEOUT: Duration = Duration::from_secs(5);
/// `try_wait` granularity inside `run_bounded`.
const POLL_STEP: Duration = Duration::from_millis(20);

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
fn run_checked(args: &[&str]) -> Result<(), AgentsError> {
    let out = run(args)?;
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
fn run_checked(args: &[&str]) -> Result<(), AgentsError> {
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
pub fn stop(id: &str) -> Result<(), AgentsError> {
    // Fail closed rather than invoking `claude stop ''`: an empty id is what an
    // unchecked `Option<String>` collapses to, and this is the destructive verb
    // (Appendix A.1 — the empty-target class of bug).
    if id.is_empty() {
        return Err(AgentsError::NotAttachable);
    }
    run_checked(&["stop", id])
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
    run_checked(&["rm", id])
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
///   while :; do <claude> attach <id>; rc=$?;
///   [ -n "$TMUX_PANE" ] && tmux set-option -p -t "$TMUX_PANE" @ccmux_detached 1 2>/dev/null;
///   printf '\n[ccmux] attach exited (rc=%s). resume: %s attach %s\n' "$rc" <claude> <id>;
///   printf '[ccmux] enter=resume  s=shell  q=close pane: ';
///   read ans || ans=q;
///   case "$ans" in s|S) break;; q|Q) exit "$rc";; esac;
///   [ -n "$TMUX_PANE" ] && tmux set-option -p -u -t "$TMUX_PANE" @ccmux_detached 2>/dev/null;
///   done; printf '\n'; [ -x "${SHELL:-}" ] || SHELL=/bin/sh; exec "$SHELL" -l
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
/// The latch is CLEARED again on the resume path, and only there. ccmux itself
/// never clears it — it cannot, because `#{pane_current_command}` would say
/// `claude` for an attach to any session at all (PROBE-FINDINGS §4) — but this
/// template knows which session it is about to re-attach, because it is the one
/// baked into the string. So a pane detached and resumed in place gets its
/// mapping back; a pane the operator hand-attaches from the `s` shell does not,
/// and must not.
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
         {bin} attach {id}; rc=$?; \
         [ -n \"$TMUX_PANE\" ] && tmux set-option -p -t \"$TMUX_PANE\" {opt} 1 2>/dev/null; \
         printf '\\n[ccmux] attach exited (rc=%s). resume: %s attach %s\\n' \"$rc\" {bin_shown} {id_shown}; \
         printf '[ccmux] enter=resume  s=shell  q=close pane: '; \
         read ans || ans=q; \
         case \"$ans\" in s|S) break;; q|Q) exit \"$rc\";; esac; \
         [ -n \"$TMUX_PANE\" ] && tmux set-option -p -u -t \"$TMUX_PANE\" {opt} 2>/dev/null; \
         done; \
         printf '\\n'; \
         [ -x \"${{SHELL:-}}\" ] || SHELL=/bin/sh; \
         exec \"$SHELL\" -l",
        opt = OPT_PANE_DETACHED,
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
             claude attach 1c45d64f; rc=$?; \
             [ -n \"$TMUX_PANE\" ] && tmux set-option -p -t \"$TMUX_PANE\" @ccmux_detached 1 2>/dev/null; \
             printf '\\n[ccmux] attach exited (rc=%s). resume: %s attach %s\\n' \"$rc\" claude 1c45d64f; \
             printf '[ccmux] enter=resume  s=shell  q=close pane: '; \
             read ans || ans=q; \
             case \"$ans\" in s|S) break;; q|Q) exit \"$rc\";; esac; \
             [ -n \"$TMUX_PANE\" ] && tmux set-option -p -u -t \"$TMUX_PANE\" @ccmux_detached 2>/dev/null; \
             done; \
             printf '\\n'; \
             [ -x \"${SHELL:-}\" ] || SHELL=/bin/sh; \
             exec \"$SHELL\" -l"
        );
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

    /// Enter is the resume, and it is the whole reason a detached pane is worth
    /// parking rather than closing. It must re-run the SAME attach — the loop
    /// is what does that — and must clear the latch first, so the pane it hands
    /// back to ccmux is one the sidebar can call open again.
    #[test]
    fn attach_pane_cmd_resumes_the_same_session_and_clears_the_latch() {
        let cmd = attach_pane_cmd("1c45d64f");
        assert!(cmd.starts_with("while :; do claude attach 1c45d64f;"), "{cmd}");
        let clear = cmd
            .find(&format!(
                "[ -n \"$TMUX_PANE\" ] && tmux set-option -p -u -t \"$TMUX_PANE\" {OPT_PANE_DETACHED} 2>/dev/null"
            ))
            .expect("latch clear");
        let read = cmd.find("read ans || ans=q").expect("read");
        let done = cmd.find("; done; ").expect("loop end");
        // Read the key, then clear, then loop back into the attach: a pane that
        // reaches the attach a second time has always cleared the latch first.
        assert!(read < clear && clear < done, "{cmd}");
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
        // Before the prompt, so a pane that is parked has always latched.
        let latch = cmd.find("set-option").expect("latch");
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
        ] {
            let at = cmd.find(&op).unwrap_or_else(|| panic!("{op} missing from {cmd}"));
            assert!(
                cmd[..at].ends_with("[ -n \"$TMUX_PANE\" ] && "),
                "unguarded {op} in {cmd}"
            );
        }
        assert_eq!(cmd.matches("tmux set-option").count(), 2, "{cmd}");
    }

    /// RULE Q4 at the one boundary that has a shell on the other side. The id
    /// reaches `printf` as an ARGUMENT, never inside its format string, so a
    /// `%` in it is inert as well as unquoted-safe.
    #[test]
    fn attach_pane_cmd_quotes_a_hostile_id() {
        let cmd = attach_pane_cmd("a'; rm -rf ~; echo '%s");
        assert!(
            cmd.starts_with("while :; do claude attach 'a'\\''; rm -rf ~; echo '\\''%s'; rc=$?"),
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
        assert!(cmd.starts_with("while :; do claude attach 'a b'; rc=$?"), "{cmd}");
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
        // Fail-closed guard: never `claude stop ''`, never `claude rm ''`.
        assert!(matches!(stop(""), Err(AgentsError::NotAttachable)));
        assert!(matches!(delete(""), Err(AgentsError::NotAttachable)));
        assert!(matches!(logs("", 10), Err(AgentsError::NotAttachable)));
        assert!(test_spawn::calls().is_empty(), "an empty id must not reach the boundary");
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
