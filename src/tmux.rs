//! Every tmux interaction in the program, plus /proc ancestry resolution.
//!
//! SPEC §3.2. No ratatui, no `claude`, no knowledge of `model::Session`. The
//! pane map stores session ids as opaque strings so this module stays
//! decoupled.
//!
//! Blast-radius invariants this file exists to enforce (SPEC §2, Appendix A.1):
//!   * `PaneId` is the only pane target type and can only be built by
//!     `PaneId::parse` (`^%\d+$`).
//!   * Every session name is re-validated here even though clap already
//!     validated it, and is addressed with tmux's EXACT-match target form.
//!   * `full_argv` refuses to run any tmux command whose `-t` value is empty.
//!   * Every mutating helper calls `assert_in_session` first, so a pane in
//!     `agents` or `dev` is never split, killed, resized, or respawned.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::Mutex;

pub const WINDOW_NAME: &str = "cc";
pub const OPT_MAP: &str = "@ccmux_map";
pub const OPT_SIDEBAR: &str = "@ccmux_sidebar";
pub const OPT_WIDTH: &str = "@ccmux_width";

/// `@ccmux_map` schema version. A different value is treated as an empty map.
const MAP_VERSION: u32 = 1;

/// Depth cap for the /proc ppid walk (SPEC §3.2).
const ANCESTRY_MAX_DEPTH: usize = 32;

/// `-F` format shared by `list_panes_in_session` and `list_panes_all`.
/// Field order is authoritative (SPEC §3.2) and mirrored by `parse_pane_line`.
const PANE_FMT: &str = concat!(
    "#{pane_id}\t",
    "#{pane_pid}\t",
    "#{pane_index}\t",
    "#{pane_left}\t",
    "#{pane_top}\t",
    "#{pane_width}\t",
    "#{pane_height}\t",
    "#{pane_active}\t",
    "#{session_name}\t",
    "#{window_index}",
);

const PANE_FIELDS: usize = 10;

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum TmuxError {
    /// `tmux` could not be spawned at all.
    NotFound(String),
    /// tmux ran and exited non-zero; carries trimmed stderr.
    Cmd { args: Vec<String>, code: i32, stderr: String },
    /// A target failed validation, or a pane is outside the ccmux session.
    BadTarget(String),
    /// tmux output did not match the requested `-F` format.
    Parse(String),
}

impl TmuxError {
    fn cmd(args: Vec<String>, code: i32, stderr: &str) -> TmuxError {
        TmuxError::Cmd { args, code, stderr: stderr.trim().to_string() }
    }
}

impl std::fmt::Display for TmuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TmuxError::NotFound(msg) => write!(f, "tmux not available: {msg}"),
            TmuxError::Cmd { args, code, stderr } => {
                // The footer shows this on one line, so lead with tmux's own
                // first stderr line ("can't find pane: %7") when there is one.
                let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
                if first.is_empty() {
                    write!(f, "tmux {} exited {code}", args.join(" "))
                } else {
                    write!(f, "{first}")
                }
            }
            TmuxError::BadTarget(msg) => write!(f, "bad target: {msg}"),
            TmuxError::Parse(msg) => write!(f, "unexpected tmux output: {msg}"),
        }
    }
}

impl std::error::Error for TmuxError {}

// ── PaneId: the only way to name a pane ─────────────────────────────────────

/// A validated tmux pane id in `%N` form. Stable across window/pane
/// renumbering (PROBE-FINDINGS §4). The ONLY accepted pane target type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(String);

impl PaneId {
    /// Accepts `^%\d+$` only. Everything else -> None.
    pub fn parse(s: &str) -> Option<PaneId> {
        let digits = s.strip_prefix('%')?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(PaneId(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Numeric value of the `N` in `%N`, for "lowest-numbered pane" ordering
    /// (SPEC §5.5). `BTreeMap` key order is lexicographic, where `%10` sorts
    /// before `%9`; this is the ordering the spec actually means.
    fn num(&self) -> u64 {
        pane_num(&self.0)
    }
}

fn pane_num(raw: &str) -> u64 {
    raw.strip_prefix('%').and_then(|d| d.parse::<u64>().ok()).unwrap_or(u64::MAX)
}

impl std::fmt::Display for PaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── Pane facts ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
///
/// SPEC NOTE: `top`, `width`, and `height` are parsed from `PANE_FMT` and
/// carried for geometry assertions (§1.3's verified table) even though the v1
/// layout logic only needs `left`; dropping them would make the pane-line
/// parser and its tests disagree with the format string.
#[allow(dead_code)]
pub struct PaneInfo {
    pub id: PaneId,
    /// `#{pane_pid}` — the pane's shell. Match target for the ancestry walk.
    pub pid: i32,
    pub index: u32,
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
    pub active: bool,
    pub session_name: String,
    pub window_index: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    /// vim `:vsplit` — side by side. tmux flag `-h`.
    Vertical,
    /// vim `:split` — stacked. tmux flag `-v`.
    Horizontal,
}

impl SplitDir {
    /// Vertical => "-h", Horizontal => "-v". Named explicitly because vim and
    /// tmux use opposite words for the same geometry.
    pub fn tmux_flag(self) -> &'static str {
        match self {
            SplitDir::Vertical => "-h",
            SplitDir::Horizontal => "-v",
        }
    }
}

// ── Socket selection ────────────────────────────────────────────────────────
//
// SPEC NOTE (addition, not a signature change): PROBE-FINDINGS §8 requires
// ccmux to be testable on a separate tmux server (`tmux -L ccmux`), but SPEC
// §3.2 fixes the runner as `tmux(args: &[&str])` with nowhere to thread a
// socket. Threading it through every call site would change the Tmux lane's
// whole surface, so the socket is process-global instead: seeded from
// `CCMUX_TMUX_SOCKET` on first use, overridable by `set_socket` if `main.rs`
// ever grows a `-L/--socket` flag. See the return-value report.
//
// Fail-closed: a socket name that is set but malformed makes EVERY tmux call
// return `BadTarget`. It must never silently fall back to the default socket,
// which is the user's live server.

/// Outer `None` = not yet initialized. Inner `None` = the default socket.
static SOCKET: Mutex<Option<Option<String>>> = Mutex::new(None);

fn lock_socket() -> std::sync::MutexGuard<'static, Option<Option<String>>> {
    SOCKET.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Override the tmux socket for the rest of the process. `None` restores the
/// default socket. Call before any other function in this module.
pub fn set_socket(name: Option<&str>) {
    *lock_socket() = Some(name.map(str::to_string).filter(|s| !s.is_empty()));
}

/// The socket in force: `set_socket`'s value, else `$CCMUX_TMUX_SOCKET`, else
/// the default socket.
pub fn socket() -> Option<String> {
    let mut lock = lock_socket();
    if lock.is_none() {
        *lock = Some(std::env::var("CCMUX_TMUX_SOCKET").ok().filter(|s| !s.is_empty()));
    }
    lock.clone().flatten()
}

fn valid_socket_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

// ── Target validation ───────────────────────────────────────────────────────

/// `^[A-Za-z0-9_-]{1,64}$` — the same rule `main::validate_session_name`
/// enforces at the CLI. Re-checked here so no path can reach tmux unvalidated.
fn valid_session_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Exact-match target for a ccmux-owned session.
///
/// SPEC NOTE: tmux resolves a bare `-t <name>` by exact match and then by
/// PREFIX match, so with only `ccmux-test-a` alive, `has-session -t ccmux`
/// exits 0 and every subsequent call silently retargets the wrong session —
/// exactly the class of bug RULE R1 exists to prevent, and reachable from
/// §10.2's own `ccmux-test-*` integration sessions. Verified on tmux 3.4 that
/// the `=<name>:` form is exact for `has-session`, `list-panes -s`,
/// `set-option`, and `show-options` alike. Bare `=<name>` is NOT usable: it
/// still prefix-matches under `list-panes` and errors under `set-option`.
fn session_target(session: &str) -> Result<String, TmuxError> {
    if !valid_session_name(session) {
        return Err(TmuxError::BadTarget(format!(
            "session name must match [A-Za-z0-9_-]{{1,64}}, got {session:?}"
        )));
    }
    Ok(format!("={session}:"))
}

/// Exact-match target for a session ccmux does NOT own (§5.4's foreign-pane
/// jump). Foreign names are the user's, so only the characters tmux itself
/// forbids in a session name are rejected.
fn foreign_session_target(session: &str) -> Result<String, TmuxError> {
    if session.is_empty()
        || session.len() > 256
        || session.contains(':')
        || session.contains('.')
        || session.chars().any(char::is_control)
    {
        return Err(TmuxError::BadTarget(format!(
            "not addressable as a tmux session: {session:?}"
        )));
    }
    Ok(format!("={session}:"))
}

fn require_shell_cmd(cmd: &str) -> Result<(), TmuxError> {
    if cmd.trim().is_empty() {
        return Err(TmuxError::BadTarget("empty shell-command".into()));
    }
    Ok(())
}

// ── Raw runner ──────────────────────────────────────────────────────────────

/// Prefix the socket flag and enforce the "never an empty `-t`" rule.
fn full_argv(args: &[&str]) -> Result<Vec<String>, TmuxError> {
    // Appendix A.1: `tmux split-window -t ""` does not error — tmux silently
    // falls back to the CALLER's current pane. Refuse at the choke point so no
    // future call site can reintroduce it.
    for (i, a) in args.iter().enumerate() {
        if *a == "-t" {
            match args.get(i + 1) {
                None => return Err(TmuxError::BadTarget("`-t` with no target".into())),
                Some(&"") => {
                    return Err(TmuxError::BadTarget("`-t` with an empty target".into()));
                }
                Some(_) => {}
            }
        }
    }

    let mut argv: Vec<String> = Vec::with_capacity(args.len() + 2);
    if let Some(sock) = socket() {
        if !valid_socket_name(&sock) {
            return Err(TmuxError::BadTarget(format!("invalid tmux socket name {sock:?}")));
        }
        argv.push("-L".to_string());
        argv.push(sock);
    }
    argv.extend(args.iter().map(|a| (*a).to_string()));
    Ok(argv)
}

/// The only place `std::process::Command::new("tmux")` appears for a captured
/// command. Always argv; never `sh -c`; never a formatted shell string.
pub fn tmux(args: &[&str]) -> Result<String, TmuxError> {
    let argv = full_argv(args)?;
    let out = Command::new("tmux")
        .args(&argv)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| TmuxError::NotFound(format!("cannot spawn tmux: {e}")))?;

    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        return Err(TmuxError::cmd(argv, code, &String::from_utf8_lossy(&out.stderr)));
    }

    // Trim ONLY the trailing newline tmux appends. `get_user_option` returns
    // the raw `@ccmux_map` JSON through here, so no general `.trim()`.
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    while s.ends_with('\n') || s.ends_with('\r') {
        s.pop();
    }
    Ok(s)
}

/// Convenience: true when `tmux(args)` returned Ok.
pub fn tmux_ok(args: &[&str]) -> bool {
    tmux(args).is_ok()
}

/// SPEC NOTE: `attach-session` needs the real terminal, so it cannot go through
/// `tmux()`'s `Command::output()`, which hands tmux a pipe and yields
/// "open terminal failed: not a terminal" (verified). This is the second and
/// last `Command::new("tmux")` site, deliberately in the same module so all
/// tmux spawning still lives in `tmux.rs`.
fn tmux_inherit(args: &[&str]) -> Result<(), TmuxError> {
    let argv = full_argv(args)?;
    let status = Command::new("tmux")
        .args(&argv)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| TmuxError::NotFound(format!("cannot spawn tmux: {e}")))?;

    if status.success() {
        Ok(())
    } else {
        let code = status.code().unwrap_or(-1);
        Err(TmuxError::cmd(argv, code, ""))
    }
}

// ── Environment probes ──────────────────────────────────────────────────────

/// `std::env::var("TMUX").is_ok()` — true inside ANY tmux server, which is not
/// the same question as `inside_target_server()`. Only use this where the
/// server identity does not matter.
///
/// SPEC §3.2 surface, kept for the API; every call site now needs the stronger
/// `inside_target_server()`.
#[allow(dead_code)]
pub fn inside_tmux() -> bool {
    std::env::var("TMUX").map(|v| !v.is_empty()).unwrap_or(false)
}

/// The socket path of the server this process is a pane of: `$TMUX`'s first
/// comma-separated field. None when `$TMUX` is unset or malformed.
fn env_socket_path() -> Option<String> {
    let tmux = std::env::var("TMUX").ok().filter(|v| !v.is_empty())?;
    let path = tmux.split(',').next().unwrap_or("");
    if path.is_empty() { None } else { Some(path.to_string()) }
}

/// The socket path of the server every `tmux()` call in this process talks to,
/// asked of the server itself. `#{socket_path}` is a SERVER property, so even a
/// mis-resolved target yields the right answer (verified on tmux 3.4).
fn target_socket_path() -> Option<String> {
    let out = tmux(&["display-message", "-p", "#{socket_path}"]).ok()?;
    let out = out.trim();
    if out.is_empty() { None } else { Some(out.to_string()) }
}

/// True when `$TMUX` names the SAME tmux server every `tmux()` call is routed
/// to.
///
/// SPEC AMENDMENT (§9.6, §1.2 step 6): the code previously asked
/// `inside_tmux()`, which only says "some tmux". With `-L <socket>` the caller
/// sits on one server while every command goes to another, and the two
/// consequences are real: `switch-client` on a client-less server fails, and
/// `display-message -p '#{session_name}'` answers about the WRONG server
/// (`$TMUX_PANE` resolves against whichever server is being addressed), which
/// short-circuits the launcher into "already inside" without ever creating a
/// sidebar.
pub fn inside_target_server() -> bool {
    let Some(mine) = env_socket_path() else {
        return false;
    };
    let Some(theirs) = target_socket_path() else {
        // The server could not be asked (no session yet, or it is not running).
        // Fall back to the socket NAME, which is right whenever both servers
        // share a socket directory — the normal case.
        return same_socket_name(&mine);
    };
    same_path(&mine, &theirs)
}

/// Basename comparison against the socket in force. `-L` names have no `/`
/// (`valid_socket_name`), and tmux's default socket is literally `default`.
fn same_socket_name(env_path: &str) -> bool {
    let mine = env_path.rsplit('/').next().unwrap_or("");
    let want = socket().unwrap_or_else(|| "default".to_string());
    !mine.is_empty() && mine == want
}

/// Path equality through symlinks; falls back to a literal compare when either
/// path cannot be canonicalized.
fn same_path(a: &str, b: &str) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// `tmux display-message -p '#{session_name}'` on the TARGET server; None when
/// this process is not a pane of that server.
pub fn current_session_name() -> Option<String> {
    if !inside_target_server() {
        return None;
    }
    let name = tmux(&["display-message", "-p", "#{session_name}"]).ok()?;
    let name = name.trim().to_string();
    if name.is_empty() { None } else { Some(name) }
}

/// `tmux has-session -t <session>` — exit 0 => true. A dead server also
/// yields exit != 0, which correctly reads as "does not exist".
pub fn has_session(session: &str) -> bool {
    match session_target(session) {
        Ok(target) => tmux_ok(&["has-session", "-t", &target]),
        Err(_) => false,
    }
}

/// `tmux -V` spawns successfully.
pub fn server_available() -> bool {
    Command::new("tmux").arg("-V").stdin(Stdio::null()).output().is_ok()
}

// ── Session lifecycle (launcher only) ───────────────────────────────────────

/// `tmux new-session -d -s <session> -n cc -P -F '#{pane_id}' -- <sidebar_cmd>`
/// `sidebar_cmd` is a shell-command string already built with `sh_quote`.
/// Returns the sidebar pane id.
pub fn create_session(session: &str, sidebar_cmd: &str) -> Result<PaneId, TmuxError> {
    // `-s` takes a NAME, not a target, so it is validated but not `=`-wrapped.
    if !valid_session_name(session) {
        return Err(TmuxError::BadTarget(format!(
            "session name must match [A-Za-z0-9_-]{{1,64}}, got {session:?}"
        )));
    }
    require_shell_cmd(sidebar_cmd)?;

    // RULE Q3: `--` immediately before the shell-command.
    let out = tmux(&[
        "new-session",
        "-d",
        "-s",
        session,
        "-n",
        WINDOW_NAME,
        "-P",
        "-F",
        "#{pane_id}",
        "--",
        sidebar_cmd,
    ])?;
    parse_pane_id_output(&out)
}

/// Applies the ccmux session options: `status off`, `mouse on`,
/// `@ccmux_sidebar`, `@ccmux_width`, and an empty `@ccmux_map`.
pub fn configure_session(session: &str, sidebar: &PaneId, width: u16) -> Result<(), TmuxError> {
    let target = session_target(session)?;

    set_user_option(session, OPT_SIDEBAR, sidebar.as_str())?;
    set_user_option(session, OPT_WIDTH, &width.to_string())?;
    set_user_option(session, OPT_MAP, &serialize_map(&PaneMap::new()))?;

    // `status` and `mouse` are cosmetic: a tmux build that renamed either must
    // not stop the launcher from producing a working session.
    let _ = tmux(&["set-option", "-t", &target, "status", "off"]);
    let _ = tmux(&["set-option", "-t", &target, "mouse", "on"]);
    Ok(())
}

/// `switch-client -t` when this process is a pane of the TARGET server, else
/// `attach-session -t`. On the attach path this replaces the current terminal
/// view and normally does not return until the client detaches.
///
/// The predicate is `inside_target_server()`, not `inside_tmux()`: with
/// `-L <socket>` the caller's server has no client on the target socket, and
/// `switch-client` there fails with "no current client".
pub fn attach_or_switch(session: &str) -> Result<(), TmuxError> {
    let target = session_target(session)?;
    if inside_target_server() {
        tmux_inherit(&["switch-client", "-t", &target])
    } else {
        tmux_inherit(&["attach-session", "-t", &target])
    }
}

// ── Pane enumeration and mutation ───────────────────────────────────────────

/// `tmux list-panes -t <session> -s -F '<FMT>'` — session-scoped (R3).
pub fn list_panes_in_session(session: &str) -> Result<Vec<PaneInfo>, TmuxError> {
    let target = session_target(session)?;
    let out = tmux(&["list-panes", "-t", &target, "-s", "-F", PANE_FMT])?;
    parse_pane_lines(&out)
}

/// READ-ONLY server-wide enumeration. Permitted ONLY for interactive-session
/// discovery (§5.4). Never feeds reconciliation, never feeds a mutation.
pub fn list_panes_all() -> Result<Vec<PaneInfo>, TmuxError> {
    let out = tmux(&["list-panes", "-a", "-F", PANE_FMT])?;
    parse_pane_lines(&out)
}

fn parse_pane_id_output(out: &str) -> Result<PaneId, TmuxError> {
    let first = out.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    PaneId::parse(first)
        .ok_or_else(|| TmuxError::Parse(format!("expected a pane id like %7, got {first:?}")))
}

fn parse_pane_lines(out: &str) -> Result<Vec<PaneInfo>, TmuxError> {
    let mut panes = Vec::new();
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        panes.push(parse_pane_line(line)?);
    }
    Ok(panes)
}

fn parse_pane_line(line: &str) -> Result<PaneInfo, TmuxError> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < PANE_FIELDS {
        return Err(TmuxError::Parse(format!(
            "expected {PANE_FIELDS} tab-separated fields, got {} in {line:?}",
            parts.len()
        )));
    }

    let bad = |what: &str, v: &str| TmuxError::Parse(format!("bad {what} {v:?} in {line:?}"));

    let id = PaneId::parse(parts[0]).ok_or_else(|| bad("pane_id", parts[0]))?;
    let pid: i32 = parts[1].parse().map_err(|_| bad("pane_pid", parts[1]))?;
    let index: u32 = parts[2].parse().map_err(|_| bad("pane_index", parts[2]))?;
    let left: u16 = parts[3].parse().map_err(|_| bad("pane_left", parts[3]))?;
    let top: u16 = parts[4].parse().map_err(|_| bad("pane_top", parts[4]))?;
    let width: u16 = parts[5].parse().map_err(|_| bad("pane_width", parts[5]))?;
    let height: u16 = parts[6].parse().map_err(|_| bad("pane_height", parts[6]))?;
    let active = parts[7] == "1";

    // A foreign session name could itself contain a tab; window_index is always
    // the LAST field, so everything between field 8 and it is the name.
    let last = parts.len() - 1;
    let session_name = parts[8..last].join("\t");
    let window_index: u32 = parts[last].parse().map_err(|_| bad("window_index", parts[last]))?;

    Ok(PaneInfo {
        id,
        pid,
        index,
        left,
        top,
        width,
        height,
        active,
        session_name,
        window_index,
    })
}

/// R2 gate. Err(BadTarget) when `pane` is absent from `session`.
pub fn assert_in_session(pane: &PaneId, session: &str) -> Result<(), TmuxError> {
    // A failure to enumerate propagates rather than being read as "absent":
    // callers must fail closed, never mutate on an unverified target.
    let panes = list_panes_in_session(session)?;
    if panes.iter().any(|p| &p.id == pane) {
        Ok(())
    } else {
        Err(TmuxError::BadTarget(format!("pane {pane} is not in tmux session {session:?}")))
    }
}

/// `tmux split-window <dir.tmux_flag()> -t <target> -P -F '#{pane_id}' -d -- <shell_cmd>`
/// `-d` keeps focus in the sidebar so the operator can keep driving the list.
/// Calls `assert_in_session(target, session)` first. Returns the new pane id.
pub fn split(
    session: &str,
    target: &PaneId,
    dir: SplitDir,
    shell_cmd: &str,
) -> Result<PaneId, TmuxError> {
    require_shell_cmd(shell_cmd)?;
    assert_in_session(target, session)?;
    let out = tmux(&[
        "split-window",
        dir.tmux_flag(),
        "-t",
        target.as_str(),
        "-P",
        "-F",
        "#{pane_id}",
        "-d",
        "--",
        shell_cmd,
    ])?;
    parse_pane_id_output(&out)
}

/// Insert a pane to the LEFT of `target` (`split-window -h -b`). Used only to
/// heal a missing sidebar.
pub fn split_left_of(
    session: &str,
    target: &PaneId,
    shell_cmd: &str,
) -> Result<PaneId, TmuxError> {
    require_shell_cmd(shell_cmd)?;
    assert_in_session(target, session)?;
    let out = tmux(&[
        "split-window",
        "-h",
        "-b",
        "-t",
        target.as_str(),
        "-P",
        "-F",
        "#{pane_id}",
        "-d",
        "--",
        shell_cmd,
    ])?;
    parse_pane_id_output(&out)
}

/// `tmux kill-pane -t <pane>`. R2-gated. SAFE with respect to Claude sessions:
/// PROBE-FINDINGS §3 proves the agent survives — FOR BACKGROUND SESSIONS. The
/// interactive refusal lives in `app::act_close_pane` (§8.5 step 3), because
/// this module does not know a pane's session kind.
pub fn kill_pane(session: &str, pane: &PaneId) -> Result<(), TmuxError> {
    assert_in_session(pane, session)?;
    tmux(&["kill-pane", "-t", pane.as_str()]).map(|_| ())
}

/// `tmux select-pane -t <pane>`. R2-gated.
pub fn select_pane(session: &str, pane: &PaneId) -> Result<(), TmuxError> {
    assert_in_session(pane, session)?;
    // The pane may live in another window of the same session; without this the
    // client stays on the current window and the "jump" appears to do nothing.
    tmux(&["select-window", "-t", pane.as_str()])?;
    tmux(&["select-pane", "-t", pane.as_str()]).map(|_| ())
}

/// `tmux resize-pane -t <pane> -x <cols>`. R2-gated.
/// Verified no-op (exit 0) when `pane` is the window's only pane.
pub fn resize_pane_width(session: &str, pane: &PaneId, cols: u16) -> Result<(), TmuxError> {
    if cols == 0 {
        return Err(TmuxError::BadTarget("resize width must be >= 1".into()));
    }
    assert_in_session(pane, session)?;
    let cols = cols.to_string();
    tmux(&["resize-pane", "-t", pane.as_str(), "-x", &cols]).map(|_| ())
}

/// `resize_pane_width` with errors swallowed. Call this on every tick and after
/// every split/kill (§1.3).
pub fn pin_sidebar(session: &str, sidebar: &PaneId, cols: u16) {
    let _ = resize_pane_width(session, sidebar, cols);
}

/// `#{window_index}` of the window holding `pane`.
pub fn window_of(panes: &[PaneInfo], pane: &PaneId) -> Option<u32> {
    panes.iter().find(|p| &p.id == pane).map(|p| p.window_index)
}

/// The subset of `panes` in `window`.
///
/// MANDATORY before any layout decision. `list_panes_in_session` is
/// session-scoped (`-s`, every window) because `PaneMap::reconcile` must see
/// panes in other windows or it would delete their map entries — but
/// `#{pane_left}`, `#{pane_index}` and `#{pane_active}` are all PER-WINDOW
/// coordinates. Choosing a split anchor from the unfiltered list can therefore
/// return a pane in a window the operator is not even looking at.
pub fn panes_in_window(panes: &[PaneInfo], window: u32) -> Vec<PaneInfo> {
    panes
        .iter()
        .filter(|p| p.window_index == window)
        .cloned()
        .collect()
}

/// Lowest `#{window_index}` present in `panes`. The fallback window when the
/// sidebar cannot be resolved.
pub fn lowest_window(panes: &[PaneInfo]) -> Option<u32> {
    panes.iter().map(|p| p.window_index).min()
}

/// `#{window_index}` of the window named `name`, if the session has one.
/// READ-ONLY.
pub fn window_index_named(session: &str, name: &str) -> Option<u32> {
    let target = session_target(session).ok()?;
    let out = tmux(&[
        "list-windows",
        "-t",
        &target,
        "-F",
        "#{window_index}\t#{window_name}",
    ])
    .ok()?;
    out.lines()
        .filter_map(|l| l.split_once('\t'))
        .find(|(_, n)| *n == name)
        .and_then(|(i, _)| i.parse().ok())
}

/// Leftmost pane by `#{pane_left}`, tie-broken by lowest `pane_index`.
/// Pass a WINDOW-SCOPED slice (`panes_in_window`); `pane_left` means nothing
/// across windows.
pub fn leftmost_pane(panes: &[PaneInfo]) -> Option<PaneId> {
    panes.iter().min_by_key(|p| (p.left, p.index)).map(|p| p.id.clone())
}

/// Rightmost pane by `#{pane_left}` EXCLUDING `sidebar`. None when the sidebar
/// is alone in the window. Pass a WINDOW-SCOPED slice (`panes_in_window`).
pub fn rightmost_pane_excluding(panes: &[PaneInfo], sidebar: &PaneId) -> Option<PaneId> {
    panes
        .iter()
        .filter(|p| &p.id != sidebar)
        .max_by_key(|p| (p.left, p.index))
        .map(|p| p.id.clone())
}

/// `tmux switch-client -t <session>` then `select-pane -t <pane>`. Used to jump
/// to an interactive session living in a foreign tmux session (§5.4). This is
/// the one mutation permitted outside `cli.session`, and it only moves the
/// client's focus — it creates, kills, and resizes nothing.
///
/// SPEC NOTE: `select-window -t <pane>` is issued between the two spec'd calls.
/// Without it the client lands on whatever window the foreign session last had
/// active, not the one holding `pane`, and the jump silently misses. Verified
/// on tmux 3.4 that a `%N` pane id is an accepted `select-window` target. Still
/// focus-only, so the doc comment's promise holds.
pub fn focus_foreign_pane(session_name: &str, pane: &PaneId) -> Result<(), TmuxError> {
    // Deliberately NOT R2-gated: the whole point is a pane outside cli.session.
    // Safe because every command here only moves the client's focus.
    let target = foreign_session_target(session_name)?;
    tmux(&["switch-client", "-t", &target])?;
    tmux(&["select-window", "-t", pane.as_str()])?;
    tmux(&["select-pane", "-t", pane.as_str()]).map(|_| ())
}

// ── User options (map persistence) ──────────────────────────────────────────

/// `tmux show-options -t <session> -qv <key>`.
/// The `-q` is MANDATORY: without it an unset user option exits 1 with
/// "invalid option". With it: empty stdout, exit 0. Returns None for empty.
pub fn get_user_option(session: &str, key: &str) -> Option<String> {
    let target = session_target(session).ok()?;
    let value = tmux(&["show-options", "-t", &target, "-qv", key]).ok()?;
    if value.is_empty() { None } else { Some(value) }
}

/// `tmux set-option -t <session> <key> <value>` as argv, so `value` needs no
/// escaping whatsoever — JSON with quotes and backslashes round-trips verbatim
/// (verified).
pub fn set_user_option(session: &str, key: &str, value: &str) -> Result<(), TmuxError> {
    if !key.starts_with('@') {
        // Keeps this function from ever writing a real tmux option by accident.
        return Err(TmuxError::BadTarget(format!("not a user option: {key:?}")));
    }
    let target = session_target(session)?;
    tmux(&["set-option", "-t", &target, key, value]).map(|_| ())
}

// ── The pane map ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaneEntry {
    /// `model::Session::session_id` (UUID). Opaque to this module.
    pub session_id: String,
    /// 8-hex short id when known; empty for interactive sessions.
    #[serde(default)]
    pub short_id: String,
    /// Name at open time, for display when the session has vanished from polls.
    #[serde(default)]
    pub name: String,
    /// epoch ms
    #[serde(default)]
    pub opened_at: i64,
}

/// Serialized into `@ccmux_map`. Lives and dies with the tmux session.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaneMap {
    /// Schema version. Current = 1. A different value is treated as an empty map.
    pub v: u32,
    /// pane_id ("%25") -> entry. BTreeMap so serialization is deterministic and
    /// "did it change" comparisons are byte-stable.
    pub panes: BTreeMap<String, PaneEntry>,
}

impl PaneMap {
    /// v = 1, empty.
    pub fn new() -> Self {
        PaneMap { v: MAP_VERSION, panes: BTreeMap::new() }
    }

    /// SPEC §3.2 surface. `App` looks up by session id, not by pane.
    #[allow(dead_code)]
    pub fn get(&self, pane: &PaneId) -> Option<&PaneEntry> {
        self.panes.get(pane.as_str())
    }

    pub fn insert(&mut self, pane: &PaneId, entry: PaneEntry) {
        self.panes.insert(pane.as_str().to_string(), entry);
    }

    pub fn remove(&mut self, pane: &PaneId) {
        self.panes.remove(pane.as_str());
    }

    /// First pane currently mapped to `session_id`, lowest pane id first.
    pub fn pane_for_session(&self, session_id: &str) -> Option<PaneId> {
        self.panes_for_session(session_id).into_iter().next()
    }

    /// All panes mapped to `session_id`, ascending. Double-attach is legal
    /// (PROBE-FINDINGS §3), so this may return more than one.
    pub fn panes_for_session(&self, session_id: &str) -> Vec<PaneId> {
        let mut out: Vec<PaneId> = self
            .panes
            .iter()
            .filter(|(_, e)| e.session_id == session_id)
            .filter_map(|(k, _)| PaneId::parse(k))
            .collect();
        // Numeric, not lexicographic: `%9` must precede `%10` (§5.5).
        out.sort_by_key(PaneId::num);
        out
    }

    /// Drop entries whose pane id is not in `live`. Entries whose *Claude
    /// session* has vanished are KEPT as long as the pane exists — the pane is
    /// still on screen showing its exit notice and must remain closable.
    /// Returns true when anything was removed.
    pub fn reconcile(&mut self, live: &[PaneInfo]) -> bool {
        let alive: HashSet<&str> = live.iter().map(|p| p.id.as_str()).collect();
        let before = self.panes.len();
        // Unparseable keys can never appear in `alive`, so a corrupt key is
        // dropped here too and counts as a change.
        self.panes.retain(|k, _| alive.contains(k.as_str()));
        self.panes.len() != before
    }
}

fn serialize_map(map: &PaneMap) -> String {
    serde_json::to_string(map).unwrap_or_else(|_| r#"{"v":1,"panes":{}}"#.to_string())
}

/// Read `@ccmux_map` and deserialize. Any failure (unset, empty, bad JSON,
/// `v != 1`) yields `PaneMap::new()` — never an error. A corrupt map must not
/// stop the sidebar from starting.
pub fn load_map(session: &str) -> PaneMap {
    let raw = match get_user_option(session, OPT_MAP) {
        Some(raw) => raw,
        None => return PaneMap::new(),
    };
    match serde_json::from_str::<PaneMap>(&raw) {
        Ok(map) if map.v == MAP_VERSION => map,
        _ => PaneMap::new(),
    }
}

/// Last value written per session, so a steady-state tick issues no
/// `set-option` at all (§5.2). `BTreeMap` makes the comparison byte-stable.
static LAST_SAVED: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn lock_saved() -> std::sync::MutexGuard<'static, Option<HashMap<String, String>>> {
    LAST_SAVED.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Serialize and write to `@ccmux_map`.
pub fn save_map(session: &str, map: &PaneMap) -> Result<(), TmuxError> {
    let json = serde_json::to_string(map)
        .map_err(|e| TmuxError::Parse(format!("cannot serialize pane map: {e}")))?;

    if lock_saved().as_ref().and_then(|c| c.get(session)) == Some(&json) {
        return Ok(());
    }

    set_user_option(session, OPT_MAP, &json)?;
    lock_saved().get_or_insert_with(HashMap::new).insert(session.to_string(), json);
    Ok(())
}

// ── Shell quoting (§7) ──────────────────────────────────────────────────────

/// POSIX single-quote escaping for the ONE place tmux needs a shell string.
pub fn sh_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b)) {
        return s.into();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Join `parts` into a single `sh`-safe command line: `sh_quote` each, join
/// with a single space.
pub fn sh_join(parts: &[&str]) -> String {
    parts.iter().map(|p| sh_quote(p)).collect::<Vec<String>>().join(" ")
}

// ── /proc ancestry (§5.4) ───────────────────────────────────────────────────

/// PARSE RULE: `comm` (field 2) is parenthesized and MAY CONTAIN SPACES AND
/// PARENTHESES. Find the LAST b')' in the line; the remainder splits on
/// whitespace as [state, ppid, ...]; ppid is index 1. Split out from
/// `ppid_of` so the §10.1 `(a b) c)` fixture is testable without /proc.
fn parse_stat_ppid(stat: &str) -> Option<i32> {
    let close = stat.rfind(')')?;
    let mut tail = stat.get(close + 1..)?.split_whitespace();
    let _state = tail.next()?;
    tail.next()?.parse::<i32>().ok()
}

/// Parse `/proc/<pid>/stat`. Returns None on any IO or parse failure.
pub fn ppid_of(pid: i32) -> Option<i32> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat_ppid(&stat)
}

/// Walk `ppid_of` upward from `pid`, inclusive of `pid`, stopping at pid <= 1,
/// at a repeated pid, or after `max_depth` steps (use 32).
pub fn ancestry(pid: i32, max_depth: usize) -> Vec<i32> {
    let mut chain: Vec<i32> = Vec::new();
    if pid <= 1 || max_depth == 0 {
        return chain;
    }
    let mut cur = pid;
    loop {
        if chain.contains(&cur) {
            break;
        }
        chain.push(cur);
        if chain.len() >= max_depth {
            break;
        }
        match ppid_of(cur) {
            Some(parent) if parent > 1 => cur = parent,
            _ => break,
        }
    }
    chain
}

/// Walk up from `pid` and return the first pane whose `PaneInfo::pid` appears
/// in the ancestry chain. This is how an interactive Claude session is mapped
/// to the pane that hosts it (PROBE-FINDINGS §4). Background sessions are
/// daemon-owned and will always return None here — that is expected, not an error.
pub fn resolve_pane_for_pid(pid: i32, panes: &[PaneInfo]) -> Option<PaneInfo> {
    // Chain order is nearest-ancestor-first, so the first hit is the innermost
    // pane hosting the process.
    for ancestor in ancestry(pid, ANCESTRY_MAX_DEPTH) {
        if let Some(pane) = panes.iter().find(|p| p.pid == ancestor) {
            return Some(pane.clone());
        }
    }
    None
}

// ── Tests (pure; no tmux server, no `claude`) ───────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, pid: i32, index: u32, left: u16) -> PaneInfo {
        PaneInfo {
            id: PaneId::parse(id).expect("test pane id"),
            pid,
            index,
            left,
            top: 0,
            width: 80,
            height: 24,
            active: false,
            session_name: "ccmux".into(),
            window_index: 1,
        }
    }

    fn entry(session_id: &str) -> PaneEntry {
        PaneEntry {
            session_id: session_id.into(),
            short_id: session_id.chars().take(8).collect(),
            name: "n".into(),
            opened_at: 1787640000000,
        }
    }

    // ── PaneId ──────────────────────────────────────────────────────────────

    #[test]
    fn pane_id_accepts_only_percent_digits() {
        assert_eq!(PaneId::parse("%25").map(|p| p.as_str().to_string()), Some("%25".to_string()));
        assert_eq!(PaneId::parse("%0").map(|p| p.as_str().to_string()), Some("%0".to_string()));
        for bad in ["", "25", "%", "%2a", "agents:2.1", "%-1", "%2 ", " %2", "%%2"] {
            assert!(PaneId::parse(bad).is_none(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn pane_id_display_round_trips() {
        let p = PaneId::parse("%7").expect("parses");
        assert_eq!(p.to_string(), "%7");
        assert_eq!(p.num(), 7);
    }

    // ── SplitDir ────────────────────────────────────────────────────────────

    #[test]
    fn split_dir_maps_vim_words_to_tmux_flags() {
        assert_eq!(SplitDir::Vertical.tmux_flag(), "-h");
        assert_eq!(SplitDir::Horizontal.tmux_flag(), "-v");
    }

    // ── sh_quote / sh_join (SPEC §7 table, verbatim) ────────────────────────

    #[test]
    fn sh_quote_matches_the_spec_table() {
        assert_eq!(sh_quote("/home/dev/projects/af"), "/home/dev/projects/af");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("my project"), "'my project'");
        assert_eq!(sh_quote("it's here"), "'it'\\''s here'");
        assert_eq!(sh_quote("a;rm -rf /"), "'a;rm -rf /'");
        assert_eq!(sh_quote("$(whoami)"), "'$(whoami)'");
        assert_eq!(sh_quote("back`tick`"), "'back`tick`'");
        assert_eq!(sh_quote("a\nb"), "'a\nb'");
    }

    #[test]
    fn sh_join_quotes_each_part() {
        assert_eq!(
            sh_join(&["/usr/bin/ccmux", "sidebar", "--session", "cc mux"]),
            "/usr/bin/ccmux sidebar --session 'cc mux'"
        );
        assert_eq!(sh_join(&[]), "");
        assert_eq!(sh_join(&["", "x"]), "'' x");
    }

    // ── /proc parsing ───────────────────────────────────────────────────────

    #[test]
    fn parse_stat_ppid_uses_the_last_close_paren() {
        // comm itself contains spaces AND parentheses: `(a b) c)`.
        let stat = "4242 ((a b) c)) S 2936154 4242 4242 0 -1 4194304 496 1199 0 0";
        assert_eq!(parse_stat_ppid(stat), Some(2936154));
    }

    #[test]
    fn parse_stat_ppid_handles_the_ordinary_shape() {
        let stat = "3020476 (zsh) S 2936154 3020476 3020476 0 -1 4194304 496 1199 0 0 0 0\n";
        assert_eq!(parse_stat_ppid(stat), Some(2936154));
    }

    #[test]
    fn parse_stat_ppid_rejects_garbage() {
        assert_eq!(parse_stat_ppid(""), None);
        assert_eq!(parse_stat_ppid("no parens here"), None);
        assert_eq!(parse_stat_ppid("1 (x) S"), None);
        assert_eq!(parse_stat_ppid("1 (x) S notanumber"), None);
    }

    #[test]
    fn ppid_of_resolves_this_process() {
        // Linux-only, and this crate targets Linux (/proc is the whole point).
        let me = std::process::id() as i32;
        assert!(ppid_of(me).is_some());
        assert_eq!(ppid_of(0), None);
        assert_eq!(ppid_of(-1), None);
    }

    #[test]
    fn ancestry_is_inclusive_bounded_and_stops_at_init() {
        let me = std::process::id() as i32;
        let chain = ancestry(me, ANCESTRY_MAX_DEPTH);
        assert_eq!(chain.first().copied(), Some(me));
        assert!(chain.len() <= ANCESTRY_MAX_DEPTH);
        assert!(chain.iter().all(|&p| p > 1));

        assert_eq!(ancestry(me, 1), vec![me]);
        assert!(ancestry(me, 0).is_empty());
        assert!(ancestry(1, 32).is_empty());
        assert!(ancestry(0, 32).is_empty());
    }

    #[test]
    fn resolve_pane_for_pid_prefers_the_nearest_ancestor() {
        let me = std::process::id() as i32;
        let chain = ancestry(me, ANCESTRY_MAX_DEPTH);

        // Pretend our own process is a pane's shell.
        let panes = vec![pane("%3", me, 1, 0)];
        assert_eq!(resolve_pane_for_pid(me, &panes).map(|p| p.id), PaneId::parse("%3"));

        // A daemon-owned pid whose chain touches no pane resolves to None —
        // the expected outcome for every background session.
        assert!(resolve_pane_for_pid(me, &[pane("%3", 999_999_999, 1, 0)]).is_none());

        if chain.len() >= 2 {
            let parent = chain[1];
            let panes = vec![pane("%1", parent, 1, 0), pane("%2", me, 2, 40)];
            // %2 holds the pid itself, so it beats the further-up ancestor %1.
            assert_eq!(resolve_pane_for_pid(me, &panes).map(|p| p.id), PaneId::parse("%2"));
        }
    }

    // ── pane line parsing ───────────────────────────────────────────────────

    #[test]
    fn parse_pane_line_reads_every_field() {
        let line = "%25\t3020315\t2\t35\t0\t239\t76\t1\tccmux\t1";
        let p = parse_pane_line(line).expect("parses");
        assert_eq!(p.id.as_str(), "%25");
        assert_eq!(p.pid, 3020315);
        assert_eq!(p.index, 2);
        assert_eq!(p.left, 35);
        assert_eq!(p.top, 0);
        assert_eq!(p.width, 239);
        assert_eq!(p.height, 76);
        assert!(p.active);
        assert_eq!(p.session_name, "ccmux");
        assert_eq!(p.window_index, 1);
    }

    #[test]
    fn parse_pane_line_tolerates_a_tab_in_a_foreign_session_name() {
        let line = "%1\t10\t1\t0\t0\t80\t24\t0\ta\tb\t3";
        let p = parse_pane_line(line).expect("parses");
        assert_eq!(p.session_name, "a\tb");
        assert_eq!(p.window_index, 3);
    }

    #[test]
    fn parse_pane_lines_rejects_short_and_malformed_rows() {
        assert!(parse_pane_line("%1\t10\t1").is_err());
        assert!(parse_pane_line("nope\t10\t1\t0\t0\t80\t24\t0\ts\t1").is_err());
        assert!(parse_pane_line("%1\tx\t1\t0\t0\t80\t24\t0\ts\t1").is_err());
        // Blank lines are skipped, not fatal.
        let panes = parse_pane_lines("%1\t10\t1\t0\t0\t80\t24\t1\ts\t1\n\n").expect("parses");
        assert_eq!(panes.len(), 1);
        assert!(parse_pane_lines("").expect("empty is fine").is_empty());
    }

    #[test]
    fn parse_pane_id_output_wants_a_pane_id() {
        assert_eq!(parse_pane_id_output("%12\n").ok().map(|p| p.to_string()), Some("%12".into()));
        assert!(parse_pane_id_output("").is_err());
        assert!(parse_pane_id_output("no server running").is_err());
    }

    // ── geometry helpers ────────────────────────────────────────────────────

    #[test]
    fn leftmost_and_rightmost_pick_the_right_panes() {
        let panes = vec![pane("%24", 1, 1, 0), pane("%25", 2, 2, 35), pane("%26", 3, 3, 155)];
        let sidebar = PaneId::parse("%24").expect("id");
        assert_eq!(leftmost_pane(&panes), Some(sidebar.clone()));
        assert_eq!(rightmost_pane_excluding(&panes, &sidebar), PaneId::parse("%26"));

        // Sidebar alone => no anchor to the right of it.
        assert_eq!(rightmost_pane_excluding(&panes[..1], &sidebar), None);
        assert_eq!(leftmost_pane(&[]), None);

        // Stacked panes share `left`; the lower pane_index wins for leftmost.
        let stacked = vec![pane("%9", 1, 2, 35), pane("%8", 2, 1, 35)];
        assert_eq!(leftmost_pane(&stacked), PaneId::parse("%8"));
    }

    #[test]
    fn window_scoping_separates_per_window_coordinates() {
        // `pane_left` restarts at 0 in every window, so an unscoped `min`/`max`
        // over a session's panes mixes windows that share nothing.
        let mut panes = vec![
            pane("%1", 1, 1, 0),
            pane("%10", 2, 2, 35),
            pane("%11", 3, 1, 0),
            pane("%12", 4, 2, 41),
        ];
        panes[2].window_index = 2;
        panes[3].window_index = 2;

        let sidebar = PaneId::parse("%1").expect("id");
        assert_eq!(window_of(&panes, &sidebar), Some(1));
        assert_eq!(window_of(&panes, &PaneId::parse("%12").expect("id")), Some(2));
        assert_eq!(window_of(&panes, &PaneId::parse("%99").expect("id")), None);
        assert_eq!(lowest_window(&panes), Some(1));
        assert_eq!(lowest_window(&[]), None);

        let w1 = panes_in_window(&panes, 1);
        assert_eq!(w1.len(), 2);
        assert_eq!(leftmost_pane(&w1), Some(sidebar.clone()));
        // Unscoped this returns %12 (left 41) — a pane in a window the operator
        // is not looking at.
        assert_eq!(rightmost_pane_excluding(&w1, &sidebar), PaneId::parse("%10"));
        assert_eq!(
            rightmost_pane_excluding(&panes, &sidebar),
            PaneId::parse("%12"),
            "regression guard: the unscoped call is the bug, keep it visible"
        );

        assert_eq!(panes_in_window(&panes, 2).len(), 2);
        assert!(panes_in_window(&panes, 7).is_empty());
    }

    // ── PaneMap ─────────────────────────────────────────────────────────────

    #[test]
    fn pane_map_new_is_versioned_and_empty() {
        let m = PaneMap::new();
        assert_eq!(m.v, MAP_VERSION);
        assert!(m.panes.is_empty());
    }

    #[test]
    fn pane_map_insert_get_remove() {
        let p = PaneId::parse("%25").expect("id");
        let mut m = PaneMap::new();
        assert!(m.get(&p).is_none());
        m.insert(&p, entry("1c45d64f-9bba-4038-8de7-d5f112c92360"));
        assert_eq!(m.get(&p).map(|e| e.short_id.as_str()), Some("1c45d64f"));
        m.remove(&p);
        assert!(m.get(&p).is_none());
    }

    #[test]
    fn panes_for_session_sorts_numerically_not_lexicographically() {
        let mut m = PaneMap::new();
        for id in ["%10", "%9", "%2"] {
            m.insert(&PaneId::parse(id).expect("id"), entry("uuid-a"));
        }
        m.insert(&PaneId::parse("%3").expect("id"), entry("uuid-b"));

        let panes = m.panes_for_session("uuid-a");
        let ids: Vec<&str> = panes.iter().map(|p| p.as_str()).collect();
        assert_eq!(ids, vec!["%2", "%9", "%10"]);
        assert_eq!(m.pane_for_session("uuid-a"), PaneId::parse("%2"));
        assert_eq!(m.pane_for_session("uuid-b"), PaneId::parse("%3"));
        assert_eq!(m.pane_for_session("uuid-missing"), None);
        assert!(m.panes_for_session("uuid-missing").is_empty());
    }

    #[test]
    fn reconcile_drops_absent_panes_and_keeps_live_ones() {
        let mut m = PaneMap::new();
        m.insert(&PaneId::parse("%25").expect("id"), entry("uuid-a"));
        m.insert(&PaneId::parse("%26").expect("id"), entry("uuid-b"));

        let live = vec![pane("%24", 1, 1, 0), pane("%25", 2, 2, 35)];
        assert!(m.reconcile(&live), "dropping %26 is a change");
        assert_eq!(m.panes.len(), 1);
        assert!(m.get(&PaneId::parse("%25").expect("id")).is_some());

        // Idempotent: a second pass changes nothing.
        assert!(!m.reconcile(&live));

        // A pane ccmux did not create (%24) is never adopted.
        assert!(m.get(&PaneId::parse("%24").expect("id")).is_none());
    }

    #[test]
    fn reconcile_drops_corrupt_keys() {
        let mut m = PaneMap::new();
        m.panes.insert("agents:2.1".into(), entry("uuid-a"));
        assert!(m.reconcile(&[pane("%1", 1, 1, 0)]));
        assert!(m.panes.is_empty());
    }

    #[test]
    fn reconcile_keeps_a_pane_whose_claude_session_vanished() {
        // The Claude session is gone from the poll, but its pane is still on
        // screen showing "[ccmux] session exited"; `x` must still close it.
        let mut m = PaneMap::new();
        m.insert(&PaneId::parse("%25").expect("id"), entry("uuid-gone"));
        assert!(!m.reconcile(&[pane("%25", 7, 2, 35)]));
        assert_eq!(m.panes.len(), 1);
    }

    #[test]
    fn pane_map_round_trips_byte_identically() {
        let mut m = PaneMap::new();
        m.insert(&PaneId::parse("%25").expect("id"), entry("1c45d64f-9bba-4038-8de7-d5f112c92360"));
        m.insert(&PaneId::parse("%2").expect("id"), entry("674b1d29-2222-4038-8de7-d5f112c92362"));

        let first = serde_json::to_string(&m).expect("serializes");
        let back: PaneMap = serde_json::from_str(&first).expect("deserializes");
        let second = serde_json::to_string(&back).expect("re-serializes");
        assert_eq!(first, second, "serialization must be byte-stable");
        assert_eq!(m, back);
        // BTreeMap ordering is what makes save_map's "did it change" check work.
        assert!(first.starts_with(r#"{"v":1,"panes":{"%2":"#));
    }

    #[test]
    fn pane_map_tolerates_missing_optional_fields() {
        let raw = r#"{"v":1,"panes":{"%1":{"session_id":"uuid-a"}}}"#;
        let m: PaneMap = serde_json::from_str(raw).expect("defaults fill in");
        let e = m.get(&PaneId::parse("%1").expect("id")).expect("entry");
        assert_eq!(e.short_id, "");
        assert_eq!(e.opened_at, 0);
    }

    #[test]
    fn serialize_map_matches_the_spec_shape() {
        assert_eq!(serialize_map(&PaneMap::new()), r#"{"v":1,"panes":{}}"#);
    }

    // ── target validation (the R1/R2 guards) ────────────────────────────────

    #[test]
    fn session_target_is_exact_match_and_rejects_junk() {
        assert_eq!(session_target("ccmux").expect("valid"), "=ccmux:");
        assert_eq!(session_target("ccmux-test-a").expect("valid"), "=ccmux-test-a:");
        let long = "x".repeat(65);
        for bad in ["", "a:b", "a.b", "agents 2", "$(x)", "-x;y", long.as_str()] {
            assert!(session_target(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn foreign_session_target_allows_user_names_but_not_separators() {
        assert_eq!(foreign_session_target("agents").expect("valid"), "=agents:");
        assert_eq!(foreign_session_target("my work").expect("valid"), "=my work:");
        for bad in ["", "a:b", "a.b", "a\nb"] {
            assert!(foreign_session_target(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn full_argv_refuses_an_empty_or_missing_target() {
        // Appendix A.1: this is the bug that created stray panes in a live
        // session. It must be unreachable, not merely unlikely.
        assert!(matches!(
            full_argv(&["split-window", "-h", "-t", ""]),
            Err(TmuxError::BadTarget(_))
        ));
        assert!(matches!(full_argv(&["kill-pane", "-t"]), Err(TmuxError::BadTarget(_))));
        assert!(full_argv(&["list-panes", "-t", "=ccmux:", "-s"]).is_ok());
    }

    #[test]
    fn set_user_option_refuses_non_user_options() {
        assert!(matches!(set_user_option("ccmux", "status", "off"), Err(TmuxError::BadTarget(_))));
    }

    #[test]
    fn require_shell_cmd_rejects_blank_commands() {
        assert!(require_shell_cmd("   ").is_err());
        assert!(require_shell_cmd("claude attach 1c45d64f").is_ok());
    }

    #[test]
    fn socket_name_validation_is_conservative() {
        assert!(valid_socket_name("ccmux"));
        assert!(valid_socket_name("ccmux-test.1"));
        assert!(!valid_socket_name(""));
        assert!(!valid_socket_name("../../tmp/evil"));
        assert!(!valid_socket_name("a b"));
    }

    // ── live smoke test ─────────────────────────────────────────────────────

    /// Exercises the real tmux calls end to end. `#[ignore]`d so `cargo test`
    /// stays hermetic; run with `cargo test -- --ignored live_round_trip`.
    ///
    /// PROBE-FINDINGS RULE T1 is enforced structurally, not by discipline: the
    /// first statement pins the process to the `ccmux` socket, so every command
    /// this test issues lands on a server that contains only panes it created.
    /// An empty target, a typo'd name, or a `kill-server` cannot reach the
    /// user's `agents`/`dev` sessions, which live on a different socket.
    #[test]
    #[ignore = "mutates a tmux server; run explicitly"]
    fn live_round_trip() {
        set_socket(Some("ccmux"));
        assert_eq!(socket().as_deref(), Some("ccmux"), "must be on the throwaway socket");

        let sess = "ccmux-test-live";
        let sidebar_cmd = sh_join(&["sleep", "600"]);

        let _ = tmux(&["kill-session", "-t", "=ccmux-test-live:"]);
        assert!(!has_session(sess), "start clean");

        // create + configure
        let sidebar = create_session(sess, &sidebar_cmd).expect("create_session");
        assert!(has_session(sess));
        configure_session(sess, &sidebar, 34).expect("configure_session");
        assert_eq!(get_user_option(sess, OPT_SIDEBAR).as_deref(), Some(sidebar.as_str()));
        assert_eq!(get_user_option(sess, OPT_WIDTH).as_deref(), Some("34"));
        assert!(get_user_option(sess, "@definitely_unset").is_none());

        // enumeration
        let panes = list_panes_in_session(sess).expect("list_panes_in_session");
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].id, sidebar);
        assert_eq!(panes[0].session_name, sess);
        assert_eq!(leftmost_pane(&panes), Some(sidebar.clone()));
        assert_eq!(rightmost_pane_excluding(&panes, &sidebar), None);

        // R2 gate: a pane id that is not in this session is refused.
        let ghost = PaneId::parse("%99999").expect("id");
        assert!(matches!(assert_in_session(&ghost, sess), Err(TmuxError::BadTarget(_))));
        assert!(matches!(kill_pane(sess, &ghost), Err(TmuxError::BadTarget(_))));

        // splits + the §1.3 geometry trace
        let p1 = split(sess, &sidebar, SplitDir::Vertical, &sidebar_cmd).expect("vsplit");
        pin_sidebar(sess, &sidebar, 34);
        let p2 = split(sess, &p1, SplitDir::Horizontal, &sidebar_cmd).expect("hsplit");
        pin_sidebar(sess, &sidebar, 34);

        let panes = list_panes_in_session(sess).expect("relist");
        assert_eq!(panes.len(), 3);
        let bar = panes.iter().find(|p| p.id == sidebar).expect("sidebar present");
        assert_eq!((bar.left, bar.width), (0, 34), "sidebar stays leftmost at 34 cols");
        assert!(rightmost_pane_excluding(&panes, &sidebar).is_some());

        // map persistence through @ccmux_map
        let mut map = load_map(sess);
        assert!(map.panes.is_empty());
        map.insert(&p1, PaneEntry {
            session_id: "uuid-a".into(),
            short_id: "1c45d64f".into(),
            name: "a b\"c".into(),
            opened_at: 1787640000000,
        });
        map.insert(&p2, PaneEntry {
            session_id: "uuid-b".into(),
            short_id: "629da7fc".into(),
            name: "b".into(),
            opened_at: 1787640000001,
        });
        save_map(sess, &map).expect("save_map");
        assert_eq!(load_map(sess), map, "JSON round-trips through argv verbatim");

        // reconcile after a real kill
        kill_pane(sess, &p2).expect("kill_pane");
        pin_sidebar(sess, &sidebar, 34);
        let live = list_panes_in_session(sess).expect("relist");
        assert_eq!(live.len(), 2);
        assert!(map.reconcile(&live), "the killed pane is dropped");
        assert_eq!(map.panes.len(), 1);
        save_map(sess, &map).expect("save_map");
        assert_eq!(load_map(sess).panes.len(), 1);

        // focus verbs must not error on a live pane
        select_pane(sess, &p1).expect("select_pane");
        resize_pane_width(sess, &sidebar, 34).expect("resize_pane_width");

        // /proc: the pane's own shell resolves back to its pane
        let all = list_panes_all().expect("list_panes_all");
        let shell_pid = live.iter().find(|p| p.id == p1).expect("p1").pid;
        assert_eq!(resolve_pane_for_pid(shell_pid, &all).map(|p| p.id), Some(p1.clone()));

        // cleanup: only ever this socket's server
        let _ = tmux(&["kill-session", "-t", "=ccmux-test-live:"]);
        assert!(!has_session(sess));
        set_socket(None);
    }

    // ── error rendering ─────────────────────────────────────────────────────

    #[test]
    fn tmux_error_displays_the_first_stderr_line() {
        let e = TmuxError::cmd(vec!["kill-pane".into()], 1, "can't find pane: %2\n");
        assert_eq!(e.to_string(), "can't find pane: %2");

        let e = TmuxError::cmd(vec!["kill-pane".into(), "-t".into()], 1, "");
        assert_eq!(e.to_string(), "tmux kill-pane -t exited 1");

        assert!(TmuxError::BadTarget("x".into()).to_string().contains("bad target"));
        assert!(TmuxError::Parse("x".into()).to_string().contains("unexpected"));
        assert!(TmuxError::NotFound("x".into()).to_string().contains("not available"));
    }
}
