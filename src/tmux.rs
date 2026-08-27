//! Every tmux interaction in the program.
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
//!   * Every mutating helper calls `assert_in_session` first, WITHOUT
//!     EXCEPTION, so a pane in `agents` or `dev` is never split, killed,
//!     resized, respawned, or even focused. The one command that deliberately
//!     targeted a foreign session — `focus_foreign_pane`, for the §5.4 jump to
//!     an interactive session — is gone along with the jump itself.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::Mutex;

pub const WINDOW_NAME: &str = "cc";
pub const OPT_MAP: &str = "@ccmux_map";
pub const OPT_SIDEBAR: &str = "@ccmux_sidebar";
pub const OPT_WIDTH: &str = "@ccmux_width";
pub const OPT_HIDDEN: &str = "@ccmux_hidden";

// ── Per-window (per-tab) options ────────────────────────────────────────────
//
// A tab is a tmux WINDOW of ccmux's own session, and every tab carries its own
// sidebar pane running its own ccmux process. That makes N concurrent writers,
// so the key space is partitioned instead of shared: each of these three
// options is a WINDOW option, written only by that window's own sidebar,
// addressed through that sidebar's own pane. Two sidebars acting in the same
// tick therefore write two DIFFERENT tmux options and both writes land.
//
// VERIFIED on tmux 3.4 (socket `ccmux-tabs`), and the reason the names are new
// rather than reused: `show-options -w -qv` does not inherit, but `#{@name}`
// format expansion DOES fall back window -> session -> global. A leftover
// SESSION value for `@ccmux_map` would therefore be reported as every window's
// value by `list_tabs`. New names cannot collide with a legacy session value.
//
// The same inheritance is what makes `@ccmux_width` ride along free in the
// window read: it stays session-scoped, written only by the launcher.
pub const OPT_TAB_SIDEBAR: &str = "@ccmux_tab_sidebar";
pub const OPT_TAB_MAP: &str = "@ccmux_tab_map";
pub const OPT_TAB_HIDDEN: &str = "@ccmux_tab_hidden";

/// The empty `@ccmux_map` value. Still written once at session creation, and
/// re-written by the legacy migration, purely so `main.rs`'s "this session is
/// not a ccmux session — refusing to modify it" ownership guard keeps working.
/// The LIVE map is `@ccmux_tab_map`, one per window.
pub const EMPTY_MAP_JSON: &str = r#"{"v":1,"panes":{}}"#;

/// `@ccmux_map` schema version. A different value is treated as an empty map.
const MAP_VERSION: u32 = 1;
/// `@ccmux_hidden` schema version. A different value is treated as empty.
const HIDDEN_VERSION: u32 = 1;
/// Most dismissals `@ccmux_hidden` will carry. tmux refuses a `set-option`
/// value over ~16 KB and a 36-char uuid costs 39 bytes serialized, so 256 sits
/// an order of magnitude under the wall. `reconcile` normally keeps the set far
/// smaller; the cap only bounds a pathological session that dismisses hundreds
/// of still-live agents.
const HIDDEN_MAX: usize = 256;

/// `@ccmux_tab_hidden` schema version. Deliberately 2, not 1: it is a log of
/// operations, not the `{"v":1,"ids":[…]}` set `@ccmux_hidden` carries, and a
/// distinct number makes a mis-read of either shape impossible to miss.
const HIDDEN_LOG_VERSION: u32 = 2;
/// Operations one window's `@ccmux_tab_hidden` fragment will carry. Pruning
/// keeps a settled fragment near empty; this only bounds a pathological tab.
/// Overflow drops the OLDEST op, which fails in the same safe direction as
/// `HIDDEN_MAX`: the row reappears.
const HIDDEN_OPS_MAX: usize = 128;

/// `-F` format for `list_panes_in_session`. Field order is authoritative
/// (SPEC §3.2) and mirrored by `parse_pane_line`.
///
/// `#{window_id}` is the ninth field and is NOT a duplicate of
/// `#{window_index}`: the operator's tmux runs `renumber-windows on`
/// (verified), so an index read at one tick names a different window at the
/// next, while a window id is never reused. Everything that must survive a
/// tick — the write-dedupe cache key, a dismissal's origin stamp, "is that pane
/// in MY window" — is keyed by the id. The index is display-only.
const PANE_FMT: &str = concat!(
    "#{pane_id}\t",
    "#{pane_index}\t",
    "#{pane_left}\t",
    "#{pane_top}\t",
    "#{pane_width}\t",
    "#{pane_height}\t",
    "#{pane_active}\t",
    "#{window_index}\t",
    "#{window_id}",
);

const PANE_FIELDS: usize = 9;

/// `-F` format for `list_tabs`. The two JSON blobs are LAST so a value that
/// somehow contained a tab could only corrupt the final field, never shift a
/// fixed one. JSON cannot contain a raw tab (serde_json escapes control
/// characters) and tmux does not re-expand format sequences inside an option
/// value — both verified on 3.4.
const TAB_FMT: &str = concat!(
    "#{window_id}\t",
    "#{window_index}\t",
    "#{@ccmux_tab_sidebar}\t",
    "#{@ccmux_width}\t",
    "#{@ccmux_tab_map}\t",
    "#{@ccmux_tab_hidden}",
);

const TAB_FIELDS: usize = 6;

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
    pub fn num(&self) -> u64 {
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

// ── WindowId: an IDENTITY, never a target ───────────────────────────────────

/// A validated tmux window id in `@N` form.
///
/// BLAST RADIUS (RULE R1): this type is deliberately NOT a target type, and no
/// function in this crate ever renders one into a `-t` argument. Verified on
/// tmux 3.4 that window-id targets have a silent-mistarget mode a pane target
/// does not: a bare `-t '@2'` reaches a window in ANOTHER session, and even a
/// session-qualified `-t '=ccmux:@99'` returns exit 0 while reading and writing
/// ccmux's CURRENT window. A pane target has no such mode — a stale `%99` fails
/// loudly with "no such window" — so every window-scoped call here is addressed
/// by an `assert_in_session`-gated `PaneId` instead.
///
/// A grep for `-t` next to a `WindowId` must return nothing. That is the
/// invariant a reviewer can check mechanically.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(String);

impl WindowId {
    /// Accepts `^@\d+$` only. Everything else -> None.
    pub fn parse(s: &str) -> Option<WindowId> {
        let digits = s.strip_prefix('@')?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(WindowId(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Numeric value of the `N` in `@N`. NUMERIC, for the same reason
    /// `PaneId::num` is: `"@9" > "@10"` lexicographically, and this value
    /// orders the adoption rule and breaks ties in the dismissal fold.
    pub fn num(&self) -> u64 {
        self.0
            .strip_prefix('@')
            .and_then(|d| d.parse::<u64>().ok())
            .unwrap_or(u64::MAX)
    }
}

impl std::fmt::Display for WindowId {
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
    pub index: u32,
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
    pub active: bool,
    pub window_index: u32,
    /// Stable across `renumber-windows`, unlike `window_index`.
    pub window_id: WindowId,
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

    // The status bar is left ALONE, deliberately: with tabs it is the tab
    // strip, and the operator's own tmux config already styles it and places
    // it. Inheriting rather than setting `status on` means ccmux never
    // overrides that — if they turn the bar off globally, ccmux respects it.
    //
    // `mouse` is cosmetic: a tmux build that renamed it must not stop the
    // launcher from producing a working session.
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

/// `tmux list-panes -t <session> -s -F '<FMT>'` — session-scoped (R3), and
/// the ONLY pane enumeration in the program. ccmux never lists the server.
pub fn list_panes_in_session(session: &str) -> Result<Vec<PaneInfo>, TmuxError> {
    let target = session_target(session)?;
    let out = tmux(&["list-panes", "-t", &target, "-s", "-F", PANE_FMT])?;
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
    let index: u32 = parts[1].parse().map_err(|_| bad("pane_index", parts[1]))?;
    let left: u16 = parts[2].parse().map_err(|_| bad("pane_left", parts[2]))?;
    let top: u16 = parts[3].parse().map_err(|_| bad("pane_top", parts[3]))?;
    let width: u16 = parts[4].parse().map_err(|_| bad("pane_width", parts[4]))?;
    let height: u16 = parts[5].parse().map_err(|_| bad("pane_height", parts[5]))?;
    let active = parts[6] == "1";
    let window_index: u32 = parts[7].parse().map_err(|_| bad("window_index", parts[7]))?;
    let window_id = WindowId::parse(parts[8]).ok_or_else(|| bad("window_id", parts[8]))?;

    Ok(PaneInfo {
        id,
        index,
        left,
        top,
        width,
        height,
        active,
        window_index,
        window_id,
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

/// `tmux new-window -d -t '=<session>:' -n cc -P -F … -- <first_cmd>` — the one
/// new mutating command tabs add. Returns (window id, window index, pane id).
///
/// BLAST RADIUS: there is NO parameter through which a window target can be
/// passed. The target is built by `session_target` alone, which is tmux's
/// exact-match form, so this call is structurally incapable of naming a window
/// or a session that is not ccmux's own. `new-window` also refuses a pane
/// target outright, so no other target form is even available here.
///
/// `-d` keeps the client where it is; `t` moves it afterwards through the
/// already-gated `select_pane`.
pub fn new_tab(session: &str, first_cmd: &str) -> Result<(WindowId, u32, PaneId), TmuxError> {
    require_shell_cmd(first_cmd)?;
    let target = session_target(session)?;
    let out = tmux(&[
        "new-window",
        "-d",
        "-t",
        &target,
        "-n",
        WINDOW_NAME,
        "-P",
        "-F",
        "#{window_id}\t#{window_index}\t#{pane_id}",
        "--",
        first_cmd,
    ])?;
    parse_new_tab_output(&out)
}

fn parse_new_tab_output(out: &str) -> Result<(WindowId, u32, PaneId), TmuxError> {
    let first = out.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let parts: Vec<&str> = first.split('\t').collect();
    let bad = || TmuxError::Parse(format!("expected `@N<TAB>N<TAB>%N` from new-window, got {first:?}"));
    let (Some(w), Some(i), Some(p)) = (parts.first(), parts.get(1), parts.get(2)) else {
        return Err(bad());
    };
    let window = WindowId::parse(w).ok_or_else(bad)?;
    let index: u32 = i.parse().map_err(|_| bad())?;
    let pane = PaneId::parse(p).ok_or_else(bad)?;
    Ok((window, index, pane))
}

/// `tmux kill-pane -t <pane>`. R2-gated. SAFE with respect to Claude sessions:
/// PROBE-FINDINGS §3 proves the agent survives, because every session ccmux
/// lists is a daemon-owned background one.
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

// ── Tabs: per-window state, read in ONE call ────────────────────────────────

/// Everything one tab persists, plus its identity.
#[derive(Debug, Clone)]
pub struct TabInfo {
    pub window: WindowId,
    pub index: u32,
    /// `@ccmux_tab_sidebar` — the pane running that tab's ccmux process.
    pub sidebar: Option<PaneId>,
    /// `@ccmux_tab_map` — the panes THAT tab opened.
    pub map: PaneMap,
    /// `@ccmux_tab_hidden` — that tab's fragment of the shared dismissal log.
    pub hidden: HiddenLog,
}

/// `tmux list-windows -t '=<session>:' -F …` — the whole cross-tab picture and
/// the session-scoped `@ccmux_width`, in ONE invocation. Session-targeted with
/// the exact-match form for the same reason every other read is.
///
/// This REPLACES the per-tick `show-options @ccmux_width` rather than adding to
/// it: session options are visible in a window format context (verified), so
/// the cross-tab picture costs nothing over today's tick.
///
/// A window whose fields are absent, malformed, or version-mismatched yields
/// the empty value, never an error — the same policy `load_map` has always had.
pub fn list_tabs(session: &str) -> Result<(Vec<TabInfo>, Option<u16>), TmuxError> {
    let target = session_target(session)?;
    let out = tmux(&["list-windows", "-t", &target, "-F", TAB_FMT])?;
    Ok(parse_tab_lines(&out))
}

fn parse_tab_lines(out: &str) -> (Vec<TabInfo>, Option<u16>) {
    let mut tabs = Vec::new();
    let mut width: Option<u16> = None;
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(TAB_FIELDS, '\t').collect();
        if parts.len() < TAB_FIELDS {
            continue;
        }
        let (Some(window), Ok(index)) = (WindowId::parse(parts[0]), parts[1].parse::<u32>()) else {
            continue;
        };
        if width.is_none() {
            width = parts[3].trim().parse::<u16>().ok();
        }
        tabs.push(TabInfo {
            window,
            index,
            sidebar: PaneId::parse(parts[2]),
            map: parse_map(parts[4]),
            hidden: parse_hidden_log(parts[5]),
        });
    }
    (tabs, width)
}

/// A window option write, addressed by a PANE of that window.
///
/// `set-option -w -t %N` resolves to %N's window (verified), and a stale pane
/// target fails LOUDLY (`no such window: %99`, exit 1) rather than silently
/// retargeting. The only thing it could otherwise reach is a pane in another
/// session — which is exactly what `assert_in_session` has always blocked. So
/// the R2 gate is mandatory here, without exception, like every other mutation.
fn set_window_option_at(
    session: &str,
    pane: &PaneId,
    key: &str,
    value: &str,
) -> Result<(), TmuxError> {
    if !key.starts_with('@') {
        return Err(TmuxError::BadTarget(format!("not a user option: {key:?}")));
    }
    assert_in_session(pane, session)?;
    tmux(&["set-option", "-w", "-t", pane.as_str(), key, value]).map(|_| ())
}

/// Record `pane` as its own window's sidebar.
///
/// The one option in this design with more than one writer (the launcher's heal
/// and the sidebar's own self-registration), and safely so: both write the same
/// function of ground truth — the pane id of the process that IS that window's
/// sidebar. No writer reads the value to compute the value, so there is no
/// read-modify-write to lose and every writer converges on the same answer.
pub fn set_tab_sidebar(session: &str, pane: &PaneId) -> Result<(), TmuxError> {
    set_window_option_at(session, pane, OPT_TAB_SIDEBAR, pane.as_str())
}

/// Serialize and write `@ccmux_tab_map` for `pane`'s window, skipping the write
/// when this process last wrote the identical value for that window.
pub fn save_tab_map(
    session: &str,
    pane: &PaneId,
    window: &WindowId,
    map: &PaneMap,
) -> Result<(), TmuxError> {
    let json = serde_json::to_string(map)
        .map_err(|e| TmuxError::Parse(format!("cannot serialize pane map: {e}")))?;
    set_window_option_cached(session, pane, window, OPT_TAB_MAP, json)
}

/// Serialize and write `@ccmux_tab_hidden` for `pane`'s window.
pub fn save_tab_hidden(
    session: &str,
    pane: &PaneId,
    window: &WindowId,
    log: &HiddenLog,
) -> Result<(), TmuxError> {
    let json = serde_json::to_string(log)
        .map_err(|e| TmuxError::Parse(format!("cannot serialize hidden log: {e}")))?;
    set_window_option_cached(session, pane, window, OPT_TAB_HIDDEN, json)
}

/// The ONE write into a window this process does not own: `t` seeds the new
/// tab's map through the Claude pane BEFORE that tab's sidebar exists.
///
/// Deliberately uncached, so a one-shot foreign-window write can never seed a
/// cache entry the owning process would later trust.
pub fn write_tab_map_uncached(
    session: &str,
    pane: &PaneId,
    map: &PaneMap,
) -> Result<(), TmuxError> {
    let json = serde_json::to_string(map)
        .map_err(|e| TmuxError::Parse(format!("cannot serialize pane map: {e}")))?;
    set_window_option_at(session, pane, OPT_TAB_MAP, &json)
}

// ── The pane map ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaneEntry {
    /// `model::Session::session_id` (UUID). Opaque to this module.
    pub session_id: String,
    /// 8-hex short id. Empty only when read back from a `@ccmux_map` written
    /// before the field existed (`#[serde(default)]`); every entry ccmux writes
    /// has one, because `act_open` refuses a session without it.
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

fn parse_map(raw: &str) -> PaneMap {
    match serde_json::from_str::<PaneMap>(raw) {
        Ok(map) if map.v == MAP_VERSION => map,
        _ => PaneMap::new(),
    }
}

/// Read the LEGACY session-scoped `@ccmux_map`. Any failure (unset, empty, bad
/// JSON, `v != 1`) yields `PaneMap::new()` — never an error. Only the one-time
/// migration reads this now; the live map is `@ccmux_tab_map`, per window.
pub fn load_map(session: &str) -> PaneMap {
    match get_user_option(session, OPT_MAP) {
        Some(raw) => parse_map(&raw),
        None => PaneMap::new(),
    }
}

/// Last value written per (option, session, WINDOW), so a steady-state tick
/// issues no `set-option` at all (§5.2).
///
/// The window is part of the key and that is load-bearing, not tidiness: a
/// single process can write two windows' copies of the same option (the `t`
/// path), and without the window one window's value would suppress the other's
/// write inside this process's own cache.
///
/// It is an in-process `static` and therefore gives ZERO protection across
/// processes — which is precisely why nothing in this design relies on it for
/// correctness. Cross-process safety comes from the key space being
/// partitioned so that two writers of one option are not representable.
static LAST_SAVED: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn lock_saved() -> std::sync::MutexGuard<'static, Option<HashMap<String, String>>> {
    LAST_SAVED.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `key`, `session` and a window id contain no spaces, so this is unambiguous.
fn cache_key(key: &str, session: &str, window: &WindowId) -> String {
    format!("{key} {session} {}", window.as_str())
}

/// `set_window_option_at`, skipped when the identical value was last written.
///
/// The cache hit-check runs BEFORE the R2 gate, deliberately: a hit means no
/// write, so there is no target to gate and no tmux spawn at all. That is what
/// keeps `seed_saved_value` a working hermetic seam now that writes are
/// pane-targeted — gate-first would make every seeded test shell out to the
/// operator's live default socket.
fn set_window_option_cached(
    session: &str,
    pane: &PaneId,
    window: &WindowId,
    key: &str,
    json: String,
) -> Result<(), TmuxError> {
    let ck = cache_key(key, session, window);
    if lock_saved().as_ref().and_then(|c| c.get(&ck)) == Some(&json) {
        return Ok(());
    }
    set_window_option_at(session, pane, key, &json)?;
    lock_saved().get_or_insert_with(HashMap::new).insert(ck, json);
    Ok(())
}

/// Pre-seed the write-dedupe cache so a `save_tab_*` carrying this exact value
/// returns `Ok(())` without spawning tmux.
///
/// Tests only, and the reason is a hard rule rather than a convenience: the
/// unit suite must never reach a tmux server, because the default socket is
/// the operator's live one. This is the seam that lets an `app` test drive
/// `App::shutdown` through the real `save_tab_hidden` and stay hermetic.
#[cfg(test)]
pub fn seed_saved_value(session: &str, key: &str, window: &WindowId, json: &str) {
    lock_saved()
        .get_or_insert_with(HashMap::new)
        .insert(cache_key(key, session, window), json.to_string());
}

// ── The dismissed set (`d` / `u`) ───────────────────────────────────────────

/// Sessions dismissed from the sidebar's list with `d`, oldest first.
///
/// A `Vec`, not a `Set`: the order IS the undo stack, so `u` pops the back.
/// De-duplicated on insert, so it is still a set by content.
///
/// Ids are `model::Session::session_id` (the stable uuid). A name is not
/// unique and a short id can be absent — a dismissal keyed by either would
/// follow the wrong row, or no row.
///
/// Purely a VIEW filter. Nothing here stops, kills, or attaches anything: the
/// agent goes on running and `claude` never hears about it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HiddenSet {
    /// Schema version. Current = 1. A different value is treated as empty.
    pub v: u32,
    #[serde(default)]
    pub ids: Vec<String>,
}

impl HiddenSet {
    /// v = 1, empty.
    pub fn new() -> Self {
        HiddenSet { v: HIDDEN_VERSION, ids: Vec::new() }
    }

    /// The dismissed ids, oldest first. What `model::build_rows` filters on.
    pub fn ids(&self) -> &[String] {
        &self.ids
    }

    /// Dismiss `id`. False when it was already hidden (nothing changed).
    ///
    /// At `HIDDEN_MAX` the OLDEST dismissal is dropped, never the newest: the
    /// back of the vec is what `u` needs, and the dropped id simply reappears
    /// in the list, which is the safe direction to fail in.
    pub fn dismiss(&mut self, id: &str) -> bool {
        if id.is_empty() || self.ids.iter().any(|h| h == id) {
            return false;
        }
        self.ids.push(id.to_string());
        while self.ids.len() > HIDDEN_MAX {
            self.ids.remove(0);
        }
        true
    }

    /// Undo the most recent dismissal, returning the id it restored.
    pub fn undo(&mut self) -> Option<String> {
        self.ids.pop()
    }

    /// Drop ids that no longer appear in `live`, so the set cannot grow without
    /// bound as sessions come and go. True when anything was dropped.
    ///
    /// `retain` preserves order, so removing from the middle of the stack
    /// leaves `undo` pointing at the same newest dismissal it did before.
    ///
    /// The caller must only pass a poll it actually got: reconciling against an
    /// empty list because `claude agents` failed would erase every dismissal.
    pub fn reconcile<'a, I: IntoIterator<Item = &'a str>>(&mut self, live: I) -> bool {
        let alive: HashSet<&str> = live.into_iter().collect();
        let before = self.ids.len();
        self.ids.retain(|id| alive.contains(id.as_str()));
        self.ids.len() != before
    }
}

/// Read `@ccmux_hidden`. Any failure (unset, empty, bad JSON, `v != 1`) yields
/// an empty set — a corrupt option must never hide rows at random, and showing
/// too much is the safe direction.
pub fn load_hidden(session: &str) -> HiddenSet {
    let raw = match get_user_option(session, OPT_HIDDEN) {
        Some(raw) => raw,
        None => return HiddenSet::new(),
    };
    match serde_json::from_str::<HiddenSet>(&raw) {
        Ok(h) if h.v == HIDDEN_VERSION => h,
        _ => HiddenSet::new(),
    }
}

// ── The dismissal log: one shared set, still one writer per option ──────────
//
// A dismissal is about the SESSION LIST, which is the same list in every tab,
// so its effect must be session-wide. Session-wide and single-writer are
// reconciled by making each window's option an op-log FRAGMENT and the shared
// set a pure fold of every fragment. Nobody ever read-modify-writes a shared
// value, so the failure this design exists to prevent — measured on tmux 3.4,
// where two concurrent read-modify-writes of one session option left
// `{"v":1,"ids":["A"]}` and silently dropped B's dismissal — is not
// representable.

/// One dismissal or restoration, stamped so every reader orders it identically.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HiddenOp {
    /// `model::Session::session_id` (the stable uuid).
    pub id: String,
    /// true = `d` (dismiss), false = `u` (a tombstone that restores the row).
    pub add: bool,
    /// Lamport stamp: `max(now_ms, largest seq ever seen + 1)`.
    pub seq: u64,
    /// `WindowId::num()` of the tab that minted it — the deterministic
    /// tiebreak for a true same-millisecond collision.
    pub org: u64,
}

impl HiddenOp {
    /// The total order every process resolves winners by. `add` is last only to
    /// make the order total; two ops can never share a `(seq, org)` unless they
    /// are the same op, because `seq` strictly increases per process.
    fn rank(&self) -> (u64, u64, bool) {
        (self.seq, self.org, self.add)
    }
}

/// One window's fragment of the shared dismissal log.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HiddenLog {
    /// Schema version. Current = 2. A different value is treated as empty.
    pub v: u32,
    #[serde(default)]
    pub ops: Vec<HiddenOp>,
}

impl Default for HiddenLog {
    fn default() -> Self {
        HiddenLog::new()
    }
}

impl HiddenLog {
    pub fn new() -> Self {
        HiddenLog { v: HIDDEN_LOG_VERSION, ops: Vec::new() }
    }

    /// Append `op` unless byte-identical to one already held. Idempotent on
    /// purpose: adoption of an orphaned fragment can legitimately re-offer an
    /// op this fragment already carries, and that must be a no-op, not a
    /// duplicate. Returns true when the log changed.
    pub fn push(&mut self, op: HiddenOp) -> bool {
        if op.id.is_empty() || self.ops.contains(&op) {
            return false;
        }
        self.ops.push(op);
        while self.ops.len() > HIDDEN_OPS_MAX {
            let Some(oldest) = self
                .ops
                .iter()
                .enumerate()
                .min_by_key(|(_, o)| o.rank())
                .map(|(i, _)| i)
            else {
                break;
            };
            self.ops.remove(oldest);
        }
        true
    }

    /// Largest stamp in this fragment — one half of the Lamport clock.
    pub fn max_seq(&self) -> u64 {
        self.ops.iter().map(|o| o.seq).max().unwrap_or(0)
    }

    /// Drop every op naming an id in `ids`. Used by the two-strike absence rule,
    /// which retires a dismissal whose session has left the poll for good.
    pub fn forget<'a, I: IntoIterator<Item = &'a str>>(&mut self, ids: I) -> bool {
        let gone: HashSet<&str> = ids.into_iter().collect();
        let before = self.ops.len();
        self.ops.retain(|o| !gone.contains(o.id.as_str()));
        self.ops.len() != before
    }
}

/// Fold every fragment into the shared dismissed set.
///
/// A last-writer-wins register per id, keyed by the total order `(seq, org)` —
/// a standard convergent structure, not an ad-hoc merge. The fold is a pure
/// function of the MULTISET of ops, and every process reads every fragment each
/// tick, so every process computes the same set and the same undo target.
///
/// The result is ordered by winning stamp ASCENDING, which is exactly today's
/// "oldest first" contract: `model::build_rows` is unaffected and
/// `HiddenSet::undo` still pops the newest dismissal.
pub fn fold_hidden<'a, I: IntoIterator<Item = &'a HiddenLog>>(frags: I) -> HiddenSet {
    let mut winner: BTreeMap<&str, &HiddenOp> = BTreeMap::new();
    for log in frags {
        for op in &log.ops {
            match winner.get(op.id.as_str()) {
                Some(cur) if cur.rank() >= op.rank() => {}
                _ => {
                    winner.insert(op.id.as_str(), op);
                }
            }
        }
    }
    let mut live: Vec<&HiddenOp> = winner.into_values().filter(|o| o.add).collect();
    live.sort_by(|a, b| (a.seq, a.org, &a.id).cmp(&(b.seq, b.org, &b.id)));
    let mut set = HiddenSet::new();
    for op in live {
        set.dismiss(&op.id);
    }
    set
}

/// Garbage-collect `mine` against what the other fragments already carry.
/// Every step is a write to one's own key, so it can never race.
///
///   * COMPACT — keep only my highest-ranked op per id; my own older ops on
///     that id can never win again.
///   * P1 superseded — drop my op on X when another fragment holds a strictly
///     higher-ranked op on X.
///   * P2 tombstone GC — drop my `add:false` op on X once no other fragment
///     holds any op on X at all. There is then nothing left for it to suppress,
///     and the fold's answer for X is identical with or without it.
///
/// P1 then P2 settle a resolved disagreement to empty in two ticks.
/// Returns true when `mine` changed.
pub fn prune_hidden_log(mine: &mut HiddenLog, others: &[&HiddenLog]) -> bool {
    let mut best: BTreeMap<&str, (u64, u64, bool)> = BTreeMap::new();
    for log in others {
        for op in &log.ops {
            let r = op.rank();
            if best.get(op.id.as_str()).is_none_or(|cur| *cur < r) {
                best.insert(op.id.as_str(), r);
            }
        }
    }
    // COMPACT first, so P1/P2 judge one op per id.
    let mut top: BTreeMap<String, (u64, u64, bool)> = BTreeMap::new();
    for op in &mine.ops {
        let r = op.rank();
        if top.get(op.id.as_str()).is_none_or(|cur| *cur < r) {
            top.insert(op.id.clone(), r);
        }
    }
    let before = mine.ops.len();
    mine.ops.retain(|op| {
        if top.get(op.id.as_str()) != Some(&op.rank()) {
            return false; // COMPACT: superseded by a later op of my own
        }
        match best.get(op.id.as_str()) {
            Some(other) => *other <= op.rank(), // P1
            None => op.add,                    // P2
        }
    });
    mine.ops.len() != before
}

fn parse_hidden_log(raw: &str) -> HiddenLog {
    match serde_json::from_str::<HiddenLog>(raw) {
        Ok(l) if l.v == HIDDEN_LOG_VERSION => l,
        _ => HiddenLog::new(),
    }
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

// ── Tests (pure; no tmux server, no `claude`) ───────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, index: u32, left: u16) -> PaneInfo {
        pane_in(id, index, left, 1)
    }

    fn pane_in(id: &str, index: u32, left: u16, window: u32) -> PaneInfo {
        PaneInfo {
            id: PaneId::parse(id).expect("test pane id"),
            index,
            left,
            top: 0,
            width: 80,
            height: 24,
            active: false,
            window_index: window,
            window_id: WindowId::parse(&format!("@{window}")).expect("test window id"),
        }
    }

    fn op(id: &str, add: bool, seq: u64, org: u64) -> HiddenOp {
        HiddenOp { id: id.into(), add, seq, org }
    }

    fn log(ops: &[HiddenOp]) -> HiddenLog {
        HiddenLog { v: HIDDEN_LOG_VERSION, ops: ops.to_vec() }
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

    // ── pane line parsing ───────────────────────────────────────────────────

    #[test]
    fn parse_pane_line_reads_every_field() {
        let line = "%25\t2\t35\t0\t239\t76\t1\t1\t@0";
        let p = parse_pane_line(line).expect("parses");
        assert_eq!(p.id.as_str(), "%25");
        assert_eq!(p.index, 2);
        assert_eq!(p.left, 35);
        assert_eq!(p.top, 0);
        assert_eq!(p.width, 239);
        assert_eq!(p.height, 76);
        assert!(p.active);
        assert_eq!(p.window_index, 1);
        assert_eq!(p.window_id.as_str(), "@0");
    }

    #[test]
    fn parse_pane_lines_rejects_short_and_malformed_rows() {
        assert!(parse_pane_line("%1\t1\t0").is_err());
        assert!(parse_pane_line("nope\t1\t0\t0\t80\t24\t0\t1\t@0").is_err());
        assert!(parse_pane_line("%1\tx\t0\t0\t80\t24\t0\t1\t@0").is_err());
        // The ninth field is the window id, and a bad one is fatal like the rest.
        assert!(parse_pane_line("%1\t1\t0\t0\t80\t24\t0\t1\t1").is_err());
        assert!(parse_pane_line("%1\t1\t0\t0\t80\t24\t0\t1").is_err());
        // Blank lines are skipped, not fatal.
        let panes = parse_pane_lines("%1\t1\t0\t0\t80\t24\t1\t1\t@0\n\n").expect("parses");
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
        let panes = vec![pane("%24", 1, 0), pane("%25", 2, 35), pane("%26", 3, 155)];
        let sidebar = PaneId::parse("%24").expect("id");
        assert_eq!(leftmost_pane(&panes), Some(sidebar.clone()));
        assert_eq!(rightmost_pane_excluding(&panes, &sidebar), PaneId::parse("%26"));

        // Sidebar alone => no anchor to the right of it.
        assert_eq!(rightmost_pane_excluding(&panes[..1], &sidebar), None);
        assert_eq!(leftmost_pane(&[]), None);

        // Stacked panes share `left`; the lower pane_index wins for leftmost.
        let stacked = vec![pane("%9", 2, 35), pane("%8", 1, 35)];
        assert_eq!(leftmost_pane(&stacked), PaneId::parse("%8"));
    }

    #[test]
    fn window_scoping_separates_per_window_coordinates() {
        // `pane_left` restarts at 0 in every window, so an unscoped `min`/`max`
        // over a session's panes mixes windows that share nothing.
        let panes = vec![
            pane("%1", 1, 0),
            pane("%10", 2, 35),
            pane_in("%11", 1, 0, 2),
            pane_in("%12", 2, 41, 2),
        ];

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

        let live = vec![pane("%24", 1, 0), pane("%25", 2, 35)];
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
        assert!(m.reconcile(&[pane("%1", 1, 0)]));
        assert!(m.panes.is_empty());
    }

    #[test]
    fn reconcile_keeps_a_pane_whose_claude_session_vanished() {
        // The Claude session is gone from the poll, but its pane is still on
        // screen showing "[ccmux] session exited"; `x` must still close it.
        let mut m = PaneMap::new();
        m.insert(&PaneId::parse("%25").expect("id"), entry("uuid-gone"));
        assert!(!m.reconcile(&[pane("%25", 2, 35)]));
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

    // ── the dismissed set (`@ccmux_hidden`) ─────────────────────────────────

    #[test]
    fn hidden_set_is_an_undo_stack() {
        let mut h = HiddenSet::new();
        assert!(h.dismiss("uuid-a"));
        assert!(h.dismiss("uuid-b"));
        // Already hidden: no duplicate, and nothing changed.
        assert!(!h.dismiss("uuid-a"));
        assert_eq!(h.ids(), ["uuid-a", "uuid-b"]);
        // An empty id can never be a session_id, and must not become an entry
        // that hides nothing and can never reconcile away.
        assert!(!h.dismiss(""));

        // Newest out first.
        assert_eq!(h.undo().as_deref(), Some("uuid-b"));
        assert_eq!(h.undo().as_deref(), Some("uuid-a"));
        assert_eq!(h.undo(), None, "empty is a no-op, not a panic");
    }

    #[test]
    fn hidden_set_reconciles_out_a_session_that_left_the_poll() {
        let mut h = HiddenSet::new();
        h.dismiss("uuid-a");
        h.dismiss("uuid-gone");
        h.dismiss("uuid-b");

        assert!(h.reconcile(["uuid-a", "uuid-b"]));
        assert_eq!(h.ids(), ["uuid-a", "uuid-b"], "order survives a mid-stack drop");
        // `undo` still points at the newest SURVIVING dismissal.
        assert_eq!(h.undo().as_deref(), Some("uuid-b"));

        // Idempotent: a second pass over the same poll changes nothing.
        let mut h2 = HiddenSet::new();
        h2.dismiss("uuid-a");
        assert!(!h2.reconcile(["uuid-a"]));
    }

    #[test]
    fn hidden_set_is_size_bounded_and_drops_the_oldest() {
        let mut h = HiddenSet::new();
        for i in 0..HIDDEN_MAX + 10 {
            assert!(h.dismiss(&format!("uuid-{i:04}")));
        }
        assert_eq!(h.ids().len(), HIDDEN_MAX);
        // The oldest went; the newest — the one `u` needs — stayed.
        assert_eq!(h.ids().first().map(String::as_str), Some("uuid-0010"));
        assert_eq!(h.undo().as_deref(), Some(&format!("uuid-{:04}", HIDDEN_MAX + 9)[..]));
        // Comfortably inside tmux's ~16 KB set-option ceiling with real uuids.
        let mut real = HiddenSet::new();
        for i in 0..HIDDEN_MAX {
            real.dismiss(&format!("1c45d64f-9bba-4038-8de7-d5f112c9{i:04}"));
        }
        let json = serde_json::to_string(&real).expect("serializes");
        assert!(json.len() < 16_000, "{} bytes", json.len());
    }

    #[test]
    fn hidden_set_round_trips_and_a_bad_option_value_hides_nothing() {
        let mut h = HiddenSet::new();
        h.dismiss("1c45d64f-9bba-4038-8de7-d5f112c92360");
        h.dismiss("674b1d29-2222-4038-8de7-d5f112c92362");

        let first = serde_json::to_string(&h).expect("serializes");
        assert_eq!(
            first,
            r#"{"v":1,"ids":["1c45d64f-9bba-4038-8de7-d5f112c92360","674b1d29-2222-4038-8de7-d5f112c92362"]}"#
        );
        let back: HiddenSet = serde_json::from_str(&first).expect("deserializes");
        assert_eq!(back, h, "order is part of the value: it is the undo stack");
        assert_eq!(serde_json::to_string(&back).expect("re-serializes"), first);

        // An empty set is the shape `load_hidden` falls back to.
        assert_eq!(serde_json::to_string(&HiddenSet::new()).expect("ok"), r#"{"v":1,"ids":[]}"#);
        // Tolerates a value written by a version that had no `ids` yet.
        let sparse: HiddenSet = serde_json::from_str(r#"{"v":1}"#).expect("defaults fill in");
        assert!(sparse.ids().is_empty());
        // A future schema is not readable as this one; showing too much is the
        // safe direction, so `load_hidden` treats it as empty.
        let future: HiddenSet = serde_json::from_str(r#"{"v":2,"ids":["x"]}"#).expect("parses");
        assert_ne!(future.v, HIDDEN_VERSION);
    }

    // ── WindowId, tabs, and the dismissal log ───────────────────────────────

    #[test]
    fn window_id_accepts_only_at_digits_and_orders_numerically() {
        assert_eq!(WindowId::parse("@0").map(|w| w.to_string()), Some("@0".into()));
        for bad in ["", "@", "@a", "0", "%1", "=ccmux:@1", "@1 ", "@-1"] {
            assert!(WindowId::parse(bad).is_none(), "{bad:?} must not parse");
        }
        // The `%9`/`%10` bug, again: adoption and the fold's tiebreak both rest
        // on this being numeric, not lexicographic.
        let nine = WindowId::parse("@9").expect("id");
        let ten = WindowId::parse("@10").expect("id");
        assert!(nine.num() < ten.num());
        assert!(nine.as_str() > ten.as_str(), "the lexicographic order is the trap");
    }

    #[test]
    fn parse_tab_lines_reads_every_window_and_the_session_width() {
        let out = concat!(
            "@0\t1\t%3\t34\t{\"v\":1,\"panes\":{\"%5\":{\"session_id\":\"u-a\"}}}\t",
            "{\"v\":2,\"ops\":[{\"id\":\"u-x\",\"add\":true,\"seq\":9,\"org\":0}]}\n",
            "@7\t2\t\t34\t\t\n",
        );
        let (tabs, width) = parse_tab_lines(out);
        assert_eq!(width, Some(34), "@ccmux_width rides along in the window read");
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].window.as_str(), "@0");
        assert_eq!(tabs[0].index, 1);
        assert_eq!(tabs[0].sidebar, PaneId::parse("%3"));
        assert_eq!(tabs[0].map.panes.len(), 1);
        assert_eq!(tabs[0].hidden.ops.len(), 1);
        // An unmarked window reads as empty everywhere, never as an error.
        assert_eq!(tabs[1].window.as_str(), "@7");
        assert!(tabs[1].sidebar.is_none());
        assert!(tabs[1].map.panes.is_empty());
        assert!(tabs[1].hidden.ops.is_empty());

        // Corrupt values hide nothing and drop nothing: same policy as `load_map`.
        let (tabs, _) = parse_tab_lines("@1\t1\tnot-a-pane\tzz\t{oops\t{\"v\":99}\n");
        assert_eq!(tabs.len(), 1);
        assert!(tabs[0].sidebar.is_none());
        assert!(tabs[0].map.panes.is_empty());
        assert!(tabs[0].hidden.ops.is_empty());
        // A line that cannot even be identified is skipped, not fatal.
        assert!(parse_tab_lines("garbage\n").0.is_empty());
        assert!(parse_tab_lines("").0.is_empty());
    }

    #[test]
    fn parse_new_tab_output_wants_all_three_fields() {
        let ok = parse_new_tab_output("@4\t3\t%12\n").expect("parses");
        assert_eq!((ok.0.as_str(), ok.1, ok.2.as_str()), ("@4", 3, "%12"));
        for bad in ["", "@4\t3", "4\t3\t%12", "@4\tx\t%12", "@4\t3\t12"] {
            assert!(parse_new_tab_output(bad).is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn set_window_option_at_refuses_a_non_user_option() {
        // The guard runs before the R2 gate's `list-panes`, so this asserts
        // without reaching any tmux server.
        let pane = PaneId::parse("%1").expect("id");
        assert!(matches!(
            set_window_option_at("ccmux", &pane, "status", "off"),
            Err(TmuxError::BadTarget(_))
        ));
    }

    /// THE concurrency claim, as a pure function: two tabs that dismiss
    /// different rows in the same tick write two DIFFERENT options, so the fold
    /// sees both. The measured failure this replaces — two concurrent
    /// read-modify-writes of one session option — silently kept only one.
    #[test]
    fn two_tabs_dismissing_in_one_tick_both_survive_the_fold() {
        let a = log(&[op("uuid-a", true, 100, 1)]);
        let b = log(&[op("uuid-b", true, 100, 2)]);
        let folded = fold_hidden([&a, &b]);
        assert_eq!(folded.ids(), ["uuid-a", "uuid-b"], "neither dismissal is lost");
        // Every process reads every fragment, so every process computes the
        // same set and the same undo target regardless of read order.
        assert_eq!(fold_hidden([&b, &a]), folded, "the fold is order-independent");
    }

    #[test]
    fn the_fold_is_a_last_writer_wins_register_per_id() {
        // Same row, both tabs dismiss it: the effect is identical either way.
        let a = log(&[op("uuid-x", true, 100, 1)]);
        let b = log(&[op("uuid-x", true, 100, 2)]);
        assert_eq!(fold_hidden([&a, &b]).ids(), ["uuid-x"], "one row, not two");

        // A dismisses X while B undoes it: the later stamp wins.
        let dismiss = log(&[op("uuid-x", true, 100, 1)]);
        let undo = log(&[op("uuid-x", false, 101, 2)]);
        assert!(fold_hidden([&dismiss, &undo]).ids().is_empty());
        // ... and a later re-dismissal outranks the tombstone again.
        let again = log(&[op("uuid-x", true, 102, 1)]);
        assert_eq!(fold_hidden([&dismiss, &undo, &again]).ids(), ["uuid-x"]);

        // A true same-millisecond tie breaks on the origin window, identically
        // for every reader.
        let lo = log(&[op("uuid-x", true, 100, 1)]);
        let hi = log(&[op("uuid-x", false, 100, 2)]);
        assert!(fold_hidden([&lo, &hi]).ids().is_empty());
        assert!(fold_hidden([&hi, &lo]).ids().is_empty());
    }

    #[test]
    fn the_fold_orders_the_undo_stack_oldest_first_across_tabs() {
        let a = log(&[op("first", true, 10, 1), op("third", true, 30, 1)]);
        let b = log(&[op("second", true, 20, 2)]);
        let mut folded = fold_hidden([&a, &b]);
        assert_eq!(folded.ids(), ["first", "second", "third"], "oldest first");
        // `u` pops the newest dismissal, whichever tab made it.
        assert_eq!(folded.undo().as_deref(), Some("third"));
        assert_eq!(folded.undo().as_deref(), Some("second"));
    }

    #[test]
    fn pruning_settles_a_resolved_disagreement_to_empty() {
        // My tombstone supersedes their dismissal, so it must be KEPT while
        // theirs still exists — dropping it would un-hide the row.
        let theirs = log(&[op("uuid-x", true, 10, 2)]);
        let mut mine = log(&[op("uuid-x", false, 11, 1)]);
        assert!(!prune_hidden_log(&mut mine, &[&theirs]));
        assert_eq!(mine.ops.len(), 1);
        assert!(fold_hidden([&theirs, &mine]).ids().is_empty());

        // P1: they drop their superseded dismissal.
        let mut theirs2 = theirs.clone();
        assert!(prune_hidden_log(&mut theirs2, &[&mine]));
        assert!(theirs2.ops.is_empty());

        // P2: with nothing left to suppress, my tombstone goes too — and the
        // fold's answer for that id is unchanged by its absence.
        assert!(prune_hidden_log(&mut mine, &[&theirs2]));
        assert!(mine.ops.is_empty(), "settled in two ticks, both writing only their own key");
        assert!(fold_hidden([&mine, &theirs2]).ids().is_empty());
    }

    #[test]
    fn pruning_compacts_my_own_history_without_resurrecting_a_row() {
        // Dismiss, undo, dismiss again — all mine, nobody else involved.
        let mut mine = log(&[
            op("uuid-x", true, 10, 1),
            op("uuid-x", false, 11, 1),
            op("uuid-x", true, 12, 1),
        ]);
        assert!(prune_hidden_log(&mut mine, &[]));
        assert_eq!(mine.ops, vec![op("uuid-x", true, 12, 1)]);
        assert_eq!(fold_hidden([&mine]).ids(), ["uuid-x"]);

        // The dangerous direction: a lone tombstone must not be dropped while
        // an older dismissal of my own is still in the log beside it.
        let mut mine = log(&[op("uuid-y", true, 10, 1), op("uuid-y", false, 11, 1)]);
        prune_hidden_log(&mut mine, &[]);
        assert!(fold_hidden([&mine]).ids().is_empty(), "the row stayed restored");
    }

    #[test]
    fn a_hidden_log_is_bounded_and_drops_the_oldest_op() {
        let mut l = HiddenLog::new();
        for i in 0..HIDDEN_OPS_MAX + 5 {
            assert!(l.push(op(&format!("uuid-{i:04}"), true, i as u64 + 1, 1)));
        }
        assert_eq!(l.ops.len(), HIDDEN_OPS_MAX);
        assert_eq!(l.ops.first().map(|o| o.id.as_str()), Some("uuid-0005"));
        assert_eq!(l.max_seq(), HIDDEN_OPS_MAX as u64 + 5);
        // Idempotent: adoption re-offering an op it already holds is a no-op.
        let dup = l.ops[0].clone();
        assert!(!l.push(dup));
        assert!(!l.push(op("", true, 1, 1)), "an empty id can never be a session id");
    }

    #[test]
    fn a_hidden_log_round_trips_and_a_future_version_reads_empty() {
        let l = log(&[op("uuid-a", true, 1787640000000, 3)]);
        let json = serde_json::to_string(&l).expect("serializes");
        assert_eq!(
            json,
            r#"{"v":2,"ops":[{"id":"uuid-a","add":true,"seq":1787640000000,"org":3}]}"#
        );
        assert_eq!(parse_hidden_log(&json), l);
        // Unset, corrupt, or a schema this build does not know: empty, never an
        // error — showing too much is the safe direction.
        assert!(parse_hidden_log("").ops.is_empty());
        assert!(parse_hidden_log("{oops").ops.is_empty());
        assert!(parse_hidden_log(r#"{"v":99,"ops":[{"id":"x","add":true,"seq":1,"org":0}]}"#).ops.is_empty());
        // The `@ccmux_hidden` SET shape must not be readable as a log.
        assert!(parse_hidden_log(r#"{"v":1,"ids":["x"]}"#).ops.is_empty());
    }

    #[test]
    fn forget_retires_every_op_naming_a_dead_session() {
        let mut l = log(&[
            op("uuid-a", true, 10, 1),
            op("uuid-gone", true, 11, 1),
            op("uuid-gone", false, 12, 1),
        ]);
        assert!(l.forget(["uuid-gone"]));
        assert_eq!(l.ops, vec![op("uuid-a", true, 10, 1)]);
        assert!(!l.forget(["uuid-nothing"]));
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

        // per-window state, read back through the ONE `list_tabs` call
        let win = panes
            .iter()
            .find(|p| p.id == sidebar)
            .map(|p| p.window_id.clone())
            .expect("sidebar window");
        let (tabs, width) = list_tabs(sess).expect("list_tabs");
        assert_eq!(tabs.len(), 1);
        assert_eq!(width, Some(34), "@ccmux_width rides along in the window read");
        assert!(tabs[0].sidebar.is_none(), "unset window option reads as absent");
        assert!(tabs[0].map.panes.is_empty());
        assert!(tabs[0].hidden.ops.is_empty());

        set_tab_sidebar(sess, &sidebar).expect("set_tab_sidebar");
        let (tabs, _) = list_tabs(sess).expect("relist tabs");
        assert_eq!(tabs[0].sidebar.as_ref(), Some(&sidebar));

        // dismissal-log persistence through @ccmux_tab_hidden
        let mut log = HiddenLog::new();
        assert!(log.push(op("1c45d64f-9bba-4038-8de7-d5f112c92360", true, 7, 0)));
        save_tab_hidden(sess, &sidebar, &win, &log).expect("save_tab_hidden");
        let (tabs, _) = list_tabs(sess).expect("relist tabs");
        assert_eq!(tabs[0].hidden, log, "@ccmux_tab_hidden round-trips");
        assert_eq!(fold_hidden([&tabs[0].hidden]).ids(), [
            "1c45d64f-9bba-4038-8de7-d5f112c92360"
        ]);

        // map persistence through @ccmux_tab_map
        let mut map = PaneMap::new();
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
        save_tab_map(sess, &sidebar, &win, &map).expect("save_tab_map");
        let (tabs, _) = list_tabs(sess).expect("relist tabs");
        assert_eq!(tabs[0].map, map, "JSON round-trips through argv verbatim");
        // The legacy session-scoped map is written once and never updated.
        assert_eq!(get_user_option(sess, OPT_MAP).as_deref(), Some(EMPTY_MAP_JSON));

        // reconcile after a real kill
        kill_pane(sess, &p2).expect("kill_pane");
        pin_sidebar(sess, &sidebar, 34);
        let live = list_panes_in_session(sess).expect("relist");
        assert_eq!(live.len(), 2);
        assert!(map.reconcile(&live), "the killed pane is dropped");
        assert_eq!(map.panes.len(), 1);
        save_tab_map(sess, &sidebar, &win, &map).expect("save_tab_map");
        let (tabs, _) = list_tabs(sess).expect("relist tabs");
        assert_eq!(tabs[0].map.panes.len(), 1);

        // focus verbs must not error on a live pane
        select_pane(sess, &p1).expect("select_pane");
        resize_pane_width(sess, &sidebar, 34).expect("resize_pane_width");

        // TABS: a second window, its own sidebar, its own option namespace
        let (w2, idx2, claude) = new_tab(sess, &sidebar_cmd).expect("new_tab");
        assert!(idx2 >= 2, "a tab is appended, never renumbering the first");
        let mut seed = PaneMap::new();
        seed.insert(&claude, PaneEntry {
            session_id: "uuid-c".into(),
            short_id: "77aa11bb".into(),
            name: "c".into(),
            opened_at: 1787640000002,
        });
        write_tab_map_uncached(sess, &claude, &seed).expect("seed the new tab map");
        let bar2 = split_left_of(sess, &claude, &sidebar_cmd).expect("tab sidebar");
        set_tab_sidebar(sess, &bar2).expect("mark tab 2");

        let (tabs, _) = list_tabs(sess).expect("relist tabs");
        assert_eq!(tabs.len(), 2);
        let t2 = tabs.iter().find(|t| t.window == w2).expect("tab 2 present");
        assert_eq!(t2.sidebar.as_ref(), Some(&bar2));
        assert_eq!(t2.map, seed, "the tab map was seeded before its sidebar existed");
        let t1 = tabs.iter().find(|t| t.window == win).expect("tab 1 present");
        assert_eq!(t1.map.panes.len(), 1, "tab 1's map is untouched by tab 2");

        // TWO WRITERS, ONE TICK: two windows, two options, both land. This is
        // the whole argument for per-window state — the same pair of writers
        // against ONE session option loses a write.
        let mut log1 = HiddenLog::new();
        log1.push(op("uuid-from-tab-1", true, 100, win.num()));
        let mut log2 = HiddenLog::new();
        log2.push(op("uuid-from-tab-2", true, 100, w2.num()));
        save_tab_hidden(sess, &sidebar, &win, &log1).expect("tab 1 flush");
        save_tab_hidden(sess, &bar2, &w2, &log2).expect("tab 2 flush");
        let (tabs, _) = list_tabs(sess).expect("relist tabs");
        let folded = fold_hidden(tabs.iter().map(|t| &t.hidden));
        assert!(folded.ids().contains(&"uuid-from-tab-1".to_string()));
        assert!(folded.ids().contains(&"uuid-from-tab-2".to_string()));

        // A window option dies with its window, which is what the adoption
        // rule in `app.rs` exists to cover.
        kill_pane(sess, &claude).expect("kill the tab's claude pane");
        kill_pane(sess, &bar2).expect("kill the tab's sidebar");
        let (tabs, _) = list_tabs(sess).expect("relist tabs");
        assert_eq!(tabs.len(), 1, "the window went with its last pane");

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
