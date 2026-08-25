//! Owner: Agents lane. SPEC §3.3.
//!
//! Everything that shells out to `claude`, plus the shell-command templates for
//! panes. Builds strings; never runs tmux.
//!
//! Consumes `model::{Session, ParseError}` and `tmux::sh_quote` (the pane
//! command templates are the shell boundary of RULE Q2 and must quote through
//! the same function the launcher uses). Nothing else from `tmux`.
//!
//! SPEC NOTE (polling): SPEC §4.2 pins polling as *synchronous, on the
//! event-loop thread* — "No threads, no channels, no async runtime" — and §10.3
//! lists any background thread as an explicit non-goal. `poll()` is therefore a
//! blocking call costing ~0.21s (PROBE-FINDINGS §1); `app.rs` calls it from
//! `tick()` every `effective_interval()`.
//!
//! SPEC NOTE (RULE Q1): every function here builds an argv via
//! `Command::new(prog).args([..])`. Nothing in this module spawns `sh -c`. The
//! only shell strings produced are `attach_pane_cmd` / `interactive_pane_cmd`,
//! which are handed to tmux (RULE Q2) and quote every interpolation through
//! `tmux::sh_quote`.

use std::process::Command;
use std::sync::OnceLock;

use crate::model::{ParseError, Session};
use crate::tmux::sh_quote;

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
            AgentsError::NotAttachable => write!(f, "session has no short id (interactive)"),
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
/// `Ok(vec![])` and the caller must clear `poll_error`.
pub fn poll() -> Result<Vec<Session>, AgentsError> {
    let out = run(&["agents", "--json", "--all"])?;
    check_status(&out)?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    crate::model::parse_sessions(&stdout).map_err(AgentsError::Parse)
}

// ── Verbs ───────────────────────────────────────────────────────────────────

/// `claude stop <id>` — DESTRUCTIVE. Callers MUST have passed §8.2's
/// confirmation gate. `id` is the 8-hex short id; interactive sessions have
/// none, so `Session::is_attachable()` must be checked first.
pub fn stop(id: &str) -> Result<(), AgentsError> {
    // Fail closed rather than invoking `claude stop ''`: an empty id is what an
    // unchecked `Option<String>` collapses to, and this is the destructive verb
    // (Appendix A.1 — the empty-target class of bug).
    if id.is_empty() {
        return Err(AgentsError::NotAttachable);
    }
    let out = run(&["stop", id])?;
    check_status(&out)?;
    Ok(())
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
/// The trailing `read` keeps the pane alive with a readable message when the
/// session has vanished between poll and open (§9.4).
///
/// Produces exactly:
///   <claude> attach <id>; rc=$?; printf '\n[ccmux] session exited (rc=%s). press enter to close pane.\n' "$rc"; read _
///
/// with `<claude>` and `<id>` passed through `sh_quote`.
pub fn attach_pane_cmd(id: &str) -> String {
    // The `\n` inside the printf format are LITERAL backslash-n for printf to
    // interpret, not Rust newlines.
    format!(
        "{} attach {}; rc=$?; \
         printf '\\n[ccmux] session exited (rc=%s). press enter to close pane.\\n' \"$rc\"; \
         read _",
        sh_quote(&claude_bin()),
        sh_quote(id)
    )
}

/// Shell command for a pane running a NEW interactive session in `cwd`.
///
/// Produces exactly:
///   cd <cwd> || { printf '[ccmux] cannot cd to %s\n' <cwd>; read _; exit 1; }; <claude>; rc=$?; printf '\n[ccmux] claude exited (rc=%s). press enter to close pane.\n' "$rc"; read _
///
/// with `<cwd>` and `<claude>` passed through `sh_quote`.
pub fn interactive_pane_cmd(cwd: &str) -> String {
    let q = sh_quote(cwd);
    format!(
        "cd {q} || {{ printf '[ccmux] cannot cd to %s\\n' {q}; read _; exit 1; }}; \
         {}; rc=$?; \
         printf '\\n[ccmux] claude exited (rc=%s). press enter to close pane.\\n' \"$rc\"; \
         read _",
        sh_quote(&claude_bin())
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
            "claude attach 1c45d64f; rc=$?; printf '\\n[ccmux] session exited (rc=%s). press enter to close pane.\\n' \"$rc\"; read _"
        );
    }

    #[test]
    fn interactive_pane_cmd_single_quotes_the_cwd() {
        assert_eq!(
            interactive_pane_cmd("/home/dev/my projects"),
            "cd '/home/dev/my projects' || { printf '[ccmux] cannot cd to %s\\n' '/home/dev/my projects'; read _; exit 1; }; claude; rc=$?; printf '\\n[ccmux] claude exited (rc=%s). press enter to close pane.\\n' \"$rc\"; read _"
        );
    }

    #[test]
    fn interactive_pane_cmd_leaves_a_plain_cwd_unquoted() {
        let cmd = interactive_pane_cmd("/home/dev/projects/af");
        assert!(cmd.starts_with("cd /home/dev/projects/af || {"), "{cmd}");
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
        let sessions = poll().expect("claude agents --json --all");
        for s in &sessions {
            assert!(!s.session_id.is_empty());
            assert_eq!(s.is_attachable(), s.id.is_some());
        }
        eprintln!("live_poll: {} sessions", sessions.len());
    }

    #[test]
    fn stop_and_logs_refuse_an_empty_id_without_spawning() {
        // Fail-closed guard: never `claude stop ''`.
        assert!(matches!(stop(""), Err(AgentsError::NotAttachable)));
        assert!(matches!(logs("", 10), Err(AgentsError::NotAttachable)));
    }
}
