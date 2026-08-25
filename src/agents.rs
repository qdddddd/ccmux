//! STUB — owner: Agents lane. SPEC §3.3.
//!
//! Everything that shells out to `claude`, plus the shell-command templates for
//! panes. Builds strings; never runs tmux.
//!
//! Consumes `model::{Session, ParseError}` and `tmux::sh_quote` (the pane
//! command templates are the shell boundary of RULE Q2 and must quote through
//! the same function the launcher uses). Nothing else from `tmux`.

use crate::model::{ParseError, Session};

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

impl std::fmt::Display for AgentsError {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        todo!()
    }
}

impl std::error::Error for AgentsError {}

/// Resolved once at startup: "claude" unless `CCMUX_CLAUDE_BIN` overrides it.
/// Everything in this module and every pane template uses this value.
pub fn claude_bin() -> String {
    todo!()
}

// ── Polling ─────────────────────────────────────────────────────────────────

/// `claude agents --json --all`.
/// ALWAYS passes `--all`: without it completed sessions are omitted and the
/// Completed group can never populate. Hiding completed rows is a UI concern
/// (the `a` key), not a fetch concern.
/// Measured cost 0.21s (PROBE-FINDINGS §1).
pub fn poll() -> Result<Vec<Session>, AgentsError> {
    todo!()
}

// ── Verbs ───────────────────────────────────────────────────────────────────

/// `claude stop <id>` — DESTRUCTIVE. Callers MUST have passed §8.2's
/// confirmation gate. `id` is the 8-hex short id; interactive sessions have
/// none, so `Session::is_attachable()` must be checked first.
pub fn stop(_id: &str) -> Result<(), AgentsError> {
    todo!()
}

/// `claude logs <id>`. Output is a RAW ANSI/PTY DUMP including alt-screen setup
/// and cursor moves (PROBE-FINDINGS §2) — always pass it through `strip_ansi`
/// before display. Returns the last `lines` lines after stripping.
pub fn logs(_id: &str, _lines: usize) -> Result<String, AgentsError> {
    todo!()
}

/// `claude --bg <task>` executed with `current_dir(cwd)`, returning
/// immediately. Pure argv — `task` and `cwd` never touch a shell.
/// Empty `task` is rejected before the call by app.rs.
pub fn dispatch_background(_cwd: &str, _task: &str) -> Result<(), AgentsError> {
    todo!()
}

// ── Pane command templates (shell strings; see §7) ──────────────────────────

/// Shell command for a pane that attaches a background session.
/// The trailing `read` keeps the pane alive with a readable message when the
/// session has vanished between poll and open (§9.4).
///
/// Produces exactly:
///   <claude> attach <id>; rc=$?; printf '\n[ccmux] session exited (rc=%s). press enter to close pane.\n' "$rc"; read _
///
/// with `<claude>` and `<id>` passed through `sh_quote`.
pub fn attach_pane_cmd(_id: &str) -> String {
    todo!()
}

/// Shell command for a pane running a NEW interactive session in `cwd`.
///
/// Produces exactly:
///   cd <cwd> || { printf '[ccmux] cannot cd to %s\n' <cwd>; read _; exit 1; }; <claude>; rc=$?; printf '\n[ccmux] claude exited (rc=%s). press enter to close pane.\n' "$rc"; read _
///
/// with `<cwd>` and `<claude>` passed through `sh_quote`.
pub fn interactive_pane_cmd(_cwd: &str) -> String {
    todo!()
}

// ── ANSI ────────────────────────────────────────────────────────────────────

/// Strip CSI (`ESC [ ... final`), OSC (`ESC ] ... BEL | ESC \`), and two-char
/// `ESC <byte>` sequences; drop remaining C0 controls except `\n` and `\t`;
/// normalize `\r\n` and lone `\r` to `\n`. Pure, allocation-only, no regex dep.
pub fn strip_ansi(_raw: &str) -> String {
    todo!()
}
