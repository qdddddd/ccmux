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

// ── Even layout: spreading the content panes along one axis (§1.4) ──────────
//
// `split-window` HALVES its target, so a fourth `o` on a 120-column window
// leaves `34 42 21 20`. Evening it afterwards with a sequence of
// `resize-pane -x` calls does work — right to left, twice, because a single
// pass does not settle — but it costs one command per pane per pass and it
// only converges to a fixed point, it is not one by construction.
//
// A tmux layout string is. `select-layout` writes the whole window's geometry
// in ONE command, exactly as asked, so re-applying it is a byte-identical
// no-op and the per-tick `resize-pane -t <sidebar> -x <W>` that follows asks
// for a width the sidebar already has. Both were measured; see the return-value
// report. A malformed string is refused ("invalid layout", exit 1) with the
// window untouched, so the failure mode is fail-closed too.

/// tmux's own layout checksum (`layout_checksum` in layout-custom.c): a 16-bit
/// rotate-right-then-add over the layout body, rendered as `%04x`.
///
/// Verified byte-for-byte on tmux 3.4 against four layouts tmux itself printed
/// through `#{window_visible_layout}`; those are the test vectors.
pub fn layout_checksum(layout: &str) -> u16 {
    let mut csum: u16 = 0;
    for b in layout.bytes() {
        csum = (csum >> 1) + ((csum & 1) << 15);
        csum = csum.wrapping_add(u16::from(b));
    }
    csum
}

/// The geometry the non-sidebar panes of ONE window form.
///
/// Only two shapes divide cleanly along a single axis: a row (side by side)
/// and a column (stacked). A mix of `o` and `s` builds a TREE, where "even
/// along the axis" has no single meaning, and both plausible readings —
/// `select-layout tiled`, or evening the top-level groups — move panes the
/// operator deliberately placed. `Ragged` is therefore a no-op, not a
/// best-effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentShape {
    /// Side by side, each full height. `o` geometry: even the WIDTHS.
    Row,
    /// Stacked, each the full content width. `s` geometry: even the HEIGHTS.
    Column,
    /// Fewer than two content panes, a sidebar that is not the full-height
    /// left edge, or a tree that is neither of the above. Nothing to do.
    Ragged,
}

impl SplitDir {
    /// The shape a split in this direction leaves behind when it lands in a
    /// window that held one content pane, or already held that shape. This is
    /// the whole of the `o`-vs-`s` axis choice: `o` (`Vertical`, tmux `-h`)
    /// puts panes side by side and so divides WIDTH; `s` (`Horizontal`, tmux
    /// `-v`) stacks them and so divides HEIGHT.
    pub fn shape(self) -> ContentShape {
        match self {
            SplitDir::Vertical => ContentShape::Row,
            SplitDir::Horizontal => ContentShape::Column,
        }
    }
}

/// Window extent implied by a WINDOW-SCOPED pane slice, as `(cols, rows)`.
/// tmux reports no window geometry in `PANE_FMT`, but the panes tile the
/// window exactly, so the maxima of `left + width` and `top + height` are it.
fn window_extent(panes: &[PaneInfo]) -> (u16, u16) {
    let cols = panes.iter().map(|p| p.left.saturating_add(p.width)).max().unwrap_or(0);
    let rows = panes.iter().map(|p| p.top.saturating_add(p.height)).max().unwrap_or(0);
    (cols, rows)
}

/// Classify the content panes of one window. Pass a WINDOW-SCOPED slice
/// (`panes_in_window`); `left`/`top` mean nothing across windows.
///
/// The sidebar must be the full-height left edge — which is what §1.3 pins it
/// to — or the remainder of the window is not a rectangle and no single axis
/// divides it. That guard also covers the degenerate case where the operator's
/// very first `s` split the SIDEBAR itself.
pub fn content_shape(panes: &[PaneInfo], sidebar: &PaneId) -> ContentShape {
    let Some(sb) = panes.iter().find(|p| &p.id == sidebar) else {
        return ContentShape::Ragged;
    };
    let (win_w, win_h) = window_extent(panes);
    if sb.left != 0 || sb.top != 0 || sb.height != win_h || win_h == 0 {
        return ContentShape::Ragged;
    }
    let mut content: Vec<&PaneInfo> = panes.iter().filter(|p| &p.id != sidebar).collect();
    if content.len() < 2 {
        return ContentShape::Ragged;
    }
    let x0 = sb.width.saturating_add(1);
    let inner_w = win_w.saturating_sub(x0);

    // A clean row: every pane full height, tiling [x0, win_w) left to right
    // with exactly one separator column between neighbours.
    content.sort_by_key(|p| p.left);
    let mut x = x0;
    let mut row = true;
    for p in &content {
        if p.top != 0 || p.height != win_h || p.left != x {
            row = false;
            break;
        }
        x = p.left.saturating_add(p.width).saturating_add(1);
    }
    if row && x == win_w.saturating_add(1) {
        return ContentShape::Row;
    }

    // A clean column: every pane the full inner width at x0, tiling
    // [0, win_h) top to bottom with one separator row between neighbours.
    content.sort_by_key(|p| p.top);
    let mut y = 0u16;
    let mut col = true;
    for p in &content {
        if p.left != x0 || p.width != inner_w || p.top != y {
            col = false;
            break;
        }
        y = p.top.saturating_add(p.height).saturating_add(1);
    }
    if col && y == win_h.saturating_add(1) {
        return ContentShape::Column;
    }

    ContentShape::Ragged
}

/// `total` cells divided among `n` panes, the remainder spread ONE CELL PER
/// PANE from the front, so no two panes ever differ by more than a single cell.
///
/// This is what tmux's own `even-horizontal` does, and the reason not to pile
/// the whole remainder on one pane: at `n = 3` either policy looks fine
/// (`28 + 28 + 27` against `27 + 27 + 29`), but remainder-to-one degrades with
/// the pane count — 78 columns over 8 panes is seven panes of 9 beside one of
/// 15, which is not what "evenly" means.
///
/// `None` when `total < n`, i.e. when some pane would get zero cells: tmux's
/// minimum pane size is 1, and refusing outright is what stops a window too
/// narrow to share from being re-laid-out into something invalid on every
/// split.
pub fn even_shares(total: u16, n: usize) -> Option<Vec<u16>> {
    let n16 = u16::try_from(n).ok()?;
    if n16 == 0 || total < n16 {
        return None;
    }
    let mut shares = vec![total / n16; n];
    for share in shares.iter_mut().take(usize::from(total % n16)) {
        *share += 1;
    }
    Some(shares)
}

/// Does the cell order `even_layout` is about to emit — the sidebar first,
/// then the content panes in visual order — match the window's PANE-LIST
/// order?
///
/// tmux binds cells to panes POSITIONALLY and the pane ids written into a
/// layout string are cosmetic: `layout_parse` walks the parsed tree and hands
/// its leaves to `w->panes` in list order, discarding the id it read from each
/// one. Verified on 3.4 — a layout whose ids were permuted `2,1,0` while its
/// cells stayed put was accepted verbatim, no pane moved, and tmux rewrote the
/// ids back to canonical order on read-back.
///
/// `#{pane_index}` IS a pane's position in that list, so "every cell reaches
/// the pane it was drawn for" is exactly "the emitted order increases in
/// `index`". Every window ccmux can build satisfies that already — `o`, `s`,
/// the `-b` sidebar heal and a split of a middle pane all leave pane-list order
/// equal to visual order — but nothing in tmux enforces it, and the cost of
/// being wrong is not a cosmetic one: the sidebar cell is emitted first, so a
/// disagreeing pane list would drop the SIDEBAR into a content cell and hand
/// its slot to an agent. So it is checked, and a window that fails the check is
/// left exactly as it is.
fn cells_follow_pane_list_order(sidebar: &PaneInfo, ordered_content: &[&PaneInfo]) -> bool {
    let mut prev = sidebar.index;
    for pane in ordered_content {
        if pane.index <= prev {
            return false;
        }
        prev = pane.index;
    }
    true
}

/// The `<checksum>,<layout>` string that spreads the content panes of one
/// window evenly along their axis, with the sidebar at exactly `sidebar_cols`.
///
/// `want` is `Some(shape)` on the split path — `o` must have produced a row and
/// `s` a column, and a split that produced neither built a tree the operator
/// gets to keep. On the kill path it is `None`: whatever clean shape the
/// survivors form is the one to even.
///
/// `sidebar_cols` MUST be the same number the caller's `pin_sidebar` will
/// assert. That is the whole of the fixed-point argument: the layout puts the
/// sidebar where the pin wants it, so the pin is a no-op forever after.
///
/// `None` means "leave the window alone" for every reason: no sidebar, a
/// ragged tree, the wrong axis, fewer than two content panes, a window with no
/// room to give each pane a cell, or a pane list whose order disagrees with
/// what the operator sees (`cells_follow_pane_list_order`).
pub fn even_layout(
    panes: &[PaneInfo],
    sidebar: &PaneId,
    sidebar_cols: u16,
    want: Option<ContentShape>,
) -> Option<String> {
    let shape = content_shape(panes, sidebar);
    if shape == ContentShape::Ragged || want.is_some_and(|w| w != shape) {
        return None;
    }
    let sb = panes.iter().find(|p| &p.id == sidebar)?;
    let (win_w, win_h) = window_extent(panes);
    // `+ 2` = the separator column plus at least one column of content.
    if sidebar_cols == 0 || win_w < sidebar_cols.saturating_add(2) {
        return None;
    }
    let x0 = sidebar_cols + 1;
    let inner_w = win_w - x0;
    let mut content: Vec<&PaneInfo> = panes.iter().filter(|p| &p.id != sidebar).collect();
    let n = content.len();
    let seps = u16::try_from(n.checked_sub(1)?).ok()?;

    // Visual order along the axis being divided: that is the order the cells
    // come out in, and therefore the order tmux will bind panes to them in.
    match shape {
        ContentShape::Row => content.sort_by_key(|p| p.left),
        ContentShape::Column => content.sort_by_key(|p| p.top),
        ContentShape::Ragged => return None,
    }
    if !cells_follow_pane_list_order(sb, &content) {
        return None;
    }

    // The trailing `,<id>` of each cell below is COSMETIC — tmux parses it and
    // throws it away, then renumbers. It is written anyway because that is the
    // shape `#{window_visible_layout}` prints, which is what makes a generated
    // string diffable against one tmux produced. The guard above, not the id,
    // is what puts each pane in the right cell.
    let mut cells = String::new();
    match shape {
        ContentShape::Row => {
            let shares = even_shares(inner_w.checked_sub(seps)?, n)?;
            let mut x = x0;
            for (p, w) in content.iter().zip(&shares) {
                if !cells.is_empty() {
                    cells.push(',');
                }
                cells.push_str(&format!("{w}x{win_h},{x},0,{}", p.id.num()));
                x = x.saturating_add(*w).saturating_add(1);
            }
        }
        ContentShape::Column => {
            let shares = even_shares(win_h.checked_sub(seps)?, n)?;
            let mut y = 0u16;
            for (p, h) in content.iter().zip(&shares) {
                if !cells.is_empty() {
                    cells.push(',');
                }
                cells.push_str(&format!("{inner_w}x{h},{x0},{y},{}", p.id.num()));
                y = y.saturating_add(*h).saturating_add(1);
            }
        }
        ContentShape::Ragged => return None,
    }

    let sb_cell = format!("{sidebar_cols}x{win_h},0,0,{}", sb.id.num());
    // `{}` is tmux's left-right split, `[]` its top-bottom one. A column of
    // content panes is one cell of the top-level row, so it nests.
    let body = match shape {
        ContentShape::Row => format!("{win_w}x{win_h},0,0{{{sb_cell},{cells}}}"),
        _ => format!("{win_w}x{win_h},0,0{{{sb_cell},{inner_w}x{win_h},{x0},0[{cells}]}}"),
    };
    Some(format!("{:04x},{body}", layout_checksum(&body)))
}

/// `tmux select-layout -t <pane> <checksum,layout>`. R2-gated.
///
/// BLAST RADIUS (R1/R2): `select-layout` acts on a WINDOW — the one holding
/// `target` — and it is addressed by an `assert_in_session`-gated `PaneId` for
/// precisely the reason `WindowId` is never rendered into a `-t`: a bare `@N`
/// reaches a window in ANOTHER session, and a session-qualified `@N` that does
/// not exist returns exit 0 while rewriting ccmux's CURRENT window. A pane
/// target has neither mode. The "a grep for `-t` next to a `WindowId` returns
/// nothing" invariant is untouched by this function.
///
/// `layout` is checked against `^[0-9a-f]{4},` before the call, so it can never
/// be mistaken for an option flag and a caller cannot smuggle a layout name
/// (`tiled`, `even-horizontal`) through this door.
///
/// Verified on tmux 3.4: a malformed layout is refused with "invalid layout"
/// and exit 1, leaving geometry unchanged; a pane in a NON-current window is
/// laid out without moving the client.
pub fn apply_layout(session: &str, target: &PaneId, layout: &str) -> Result<(), TmuxError> {
    let checksummed = layout.split_once(',').is_some_and(|(sum, rest)| {
        sum.len() == 4
            && sum.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && !rest.is_empty()
    });
    if !checksummed {
        return Err(TmuxError::BadTarget(format!("not a tmux layout string: {layout:?}")));
    }
    assert_in_session(target, session)?;
    tmux(&["select-layout", "-t", target.as_str(), layout]).map(|_| ())
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

    // ── Even layout (§1.4) ──────────────────────────────────────────────────

    /// A pane with real geometry. The `pane`/`pane_in` helpers above fix
    /// `top`/`width`/`height`, which is exactly what layout arithmetic reads.
    ///
    /// `index` is `#{pane_index}`, i.e. the pane's position in the WINDOW'S
    /// PANE LIST — the order tmux binds layout cells in, so it is load-bearing
    /// here and not decoration. Every fixture below numbers its panes in the
    /// order the operator sees them, which is what a real ccmux window looks
    /// like (SPEC §1.3's table: `1:34 2:85`, `1:34 2:42 3:42`, ...).
    fn geo(id: &str, index: u32, left: u16, top: u16, width: u16, height: u16) -> PaneInfo {
        PaneInfo {
            id: PaneId::parse(id).expect("test pane id"),
            index,
            left,
            top,
            width,
            height,
            active: false,
            window_index: 1,
            window_id: WindowId::parse("@1").expect("test window id"),
        }
    }

    /// Sidebar 34 wide, then three content panes side by side, mid-halving:
    /// the state a third `o` leaves behind on a 120x40 window.
    fn row_window() -> Vec<PaneInfo> {
        vec![
            geo("%0", 1, 0, 0, 34, 40),
            geo("%3", 2, 35, 0, 1, 40),
            geo("%2", 3, 37, 0, 1, 40),
            geo("%1", 4, 39, 0, 81, 40),
        ]
    }

    /// The same window after four `s` presses: a column of four, halving down
    /// to 20/9/4/4.
    fn column_window() -> Vec<PaneInfo> {
        vec![
            geo("%0", 1, 0, 0, 34, 40),
            geo("%1", 2, 35, 0, 85, 20),
            geo("%2", 3, 35, 21, 85, 9),
            geo("%3", 4, 35, 31, 85, 4),
            geo("%4", 5, 35, 36, 85, 4),
        ]
    }

    fn sb() -> PaneId {
        PaneId::parse("%0").expect("id")
    }

    /// tmux's checksum, not a re-derivation of it. Every vector below is a
    /// string tmux 3.4 either PRINTED through `#{window_visible_layout}` or
    /// ACCEPTED through `select-layout`; if this function ever drifts, tmux
    /// answers "invalid layout" and the whole feature silently stops working.
    #[test]
    fn layout_checksum_matches_the_strings_tmux_itself_printed() {
        for (want, body) in [
            (0x11ce, "120x40,0,0{34x40,0,0,0,85x40,35,0,1}"),
            (0x9285, "120x40,0,0{34x40,0,0,0,1x40,35,0,2,83x40,37,0,1}"),
            (0x31fd, "120x40,0,0{34x40,0,0,0,1x40,35,0,3,1x40,37,0,2,81x40,39,0,1}"),
            (0xee36, "120x40,0,0{34x40,0,0,0,85x40,35,0[85x20,35,0,1,85x19,35,21,2]}"),
            (
                0xdab9,
                "120x40,0,0{34x40,0,0,0,85x40,35,0[85x20,35,0,1,85x9,35,21,2,85x9,35,31,3]}",
            ),
            // The row and the column this feature writes, both applied live on
            // 3.4 and ACCEPTED — which is itself a checksum check, since
            // `layout_parse` refuses a string whose leading `%04x` disagrees
            // with the body. The column one tmux then echoed back byte for
            // byte through `#{window_visible_layout}`.
            (0xfa10, "120x40,0,0{34x40,0,0,0,28x40,35,0,3,28x40,64,0,2,27x40,93,0,1}"),
            (
                0x3f34,
                "120x40,0,0{34x40,0,0,0,85x40,35,0[85x10,35,0,1,85x9,35,11,2,85x9,35,21,3,85x9,35,31,4]}",
            ),
        ] {
            assert_eq!(layout_checksum(body), want, "checksum of {body}");
        }
    }

    /// The share arithmetic, including where the leftover cells go.
    #[test]
    fn even_shares_spreads_the_remainder_one_cell_per_pane() {
        // 120 window - 34 sidebar - 3 separators = 83 content columns over 3
        // panes. 83/3 = 27 remainder 2, so two panes get 28 and one gets 27.
        assert_eq!(even_shares(83, 3), Some(vec![28, 28, 27]));
        // 40 rows - 3 separators = 37 over 4 panes: 10 + 9 + 9 + 9.
        assert_eq!(even_shares(37, 4), Some(vec![10, 9, 9, 9]));
        // Exact division leaves nothing over.
        assert_eq!(even_shares(84, 3), Some(vec![28, 28, 28]));
        assert_eq!(even_shares(85, 1), Some(vec![85]));
        // One cell each is the floor, and it is legal: tmux's minimum is 1.
        assert_eq!(even_shares(3, 3), Some(vec![1, 1, 1]));
        // Below it, refuse rather than emit a zero-sized pane.
        assert_eq!(even_shares(2, 3), None);
        assert_eq!(even_shares(0, 1), None);
        assert_eq!(even_shares(99, 0), None);
    }

    /// The property the policy exists for, over every division this feature can
    /// hit: NO TWO PANES DIFFER BY MORE THAN ONE CELL, and the shares always
    /// tile the space exactly.
    ///
    /// Piling the whole remainder on one pane satisfies neither at scale — 78
    /// columns over 8 panes would be seven of 9 beside one of 15 — and it is
    /// unregressable by an example-based test, because every individual number
    /// still looks plausible. This checks the invariant instead.
    #[test]
    fn even_shares_never_lets_two_panes_differ_by_more_than_a_cell() {
        for n in 1..=16usize {
            for total in 0..=200u16 {
                let Some(shares) = even_shares(total, n) else {
                    assert!(
                        total < u16::try_from(n).expect("n fits"),
                        "refused a division that had room: {total} over {n}"
                    );
                    continue;
                };
                assert_eq!(shares.len(), n, "{total} over {n}");
                assert_eq!(shares.iter().sum::<u16>(), total, "shares tile {total} over {n}");
                let hi = *shares.iter().max().expect("non-empty");
                let lo = *shares.iter().min().expect("non-empty");
                assert!(hi - lo <= 1, "{total} over {n} spread {shares:?}");
                assert!(lo >= 1, "no pane may be given zero cells: {shares:?}");
            }
        }
    }

    #[test]
    fn content_shape_tells_a_row_from_a_column_from_a_tree() {
        assert_eq!(content_shape(&row_window(), &sb()), ContentShape::Row);
        assert_eq!(content_shape(&column_window(), &sb()), ContentShape::Column);

        // A mix of `o` and `s`: %1 alone on the left, %2 over %3 on the right.
        // Neither axis divides this, so it is a tree and it stays one.
        let mixed = vec![
            geo("%0", 1, 0, 0, 34, 40),
            geo("%1", 2, 35, 0, 42, 40),
            geo("%2", 3, 78, 0, 42, 20),
            geo("%3", 4, 78, 21, 42, 19),
        ];
        assert_eq!(content_shape(&mixed, &sb()), ContentShape::Ragged);

        // Nothing to even: the sidebar alone, and the sidebar plus one.
        assert_eq!(content_shape(&[geo("%0", 1, 0, 0, 120, 40)], &sb()), ContentShape::Ragged);
        let one = vec![geo("%0", 1, 0, 0, 34, 40), geo("%1", 2, 35, 0, 85, 40)];
        assert_eq!(content_shape(&one, &sb()), ContentShape::Ragged);

        // A sidebar that is not the full-height left edge — what the very first
        // `s` would leave if it split the sidebar itself. The remainder of the
        // window is not a rectangle, so no axis divides it.
        let split_sidebar = vec![
            geo("%0", 1, 0, 0, 34, 20),
            geo("%9", 2, 0, 21, 34, 19),
            geo("%1", 3, 35, 0, 85, 40),
        ];
        assert_eq!(content_shape(&split_sidebar, &sb()), ContentShape::Ragged);

        // An unresolvable sidebar is not a licence to lay out the window.
        assert_eq!(
            content_shape(&row_window(), &PaneId::parse("%99").expect("id")),
            ContentShape::Ragged
        );
    }

    /// The exact byte string `select-layout` accepted on a live 3.4 server.
    #[test]
    fn even_layout_row_reproduces_the_string_tmux_accepted() {
        const WANT: &str = "fa10,120x40,0,0{34x40,0,0,0,28x40,35,0,3,28x40,64,0,2,27x40,93,0,1}";
        assert_eq!(
            even_layout(&row_window(), &sb(), 34, Some(ContentShape::Row)).as_deref(),
            Some(WANT)
        );
        // The kill path asks for no particular axis and gets the same answer.
        assert_eq!(even_layout(&row_window(), &sb(), 34, None).as_deref(), Some(WANT));
    }

    #[test]
    fn even_layout_column_reproduces_the_string_tmux_accepted() {
        const WANT: &str = "3f34,120x40,0,0{34x40,0,0,0,85x40,35,0\
            [85x10,35,0,1,85x9,35,11,2,85x9,35,21,3,85x9,35,31,4]}";
        assert_eq!(
            even_layout(&column_window(), &sb(), 34, Some(ContentShape::Column)).as_deref(),
            Some(WANT)
        );
        assert_eq!(even_layout(&column_window(), &sb(), 34, None).as_deref(), Some(WANT));
    }

    /// THE FIXED-POINT PROOF, in pure code: feed `even_layout` the geometry its
    /// own output describes and it returns that output again, unchanged — and
    /// the sidebar cell it writes is exactly the width `pin_sidebar` asserts,
    /// so the per-tick `resize-pane -x 34` that follows has nothing to do.
    #[test]
    fn evening_an_already_even_window_is_the_same_layout_again() {
        let evened_row = vec![
            geo("%0", 1, 0, 0, 34, 40),
            geo("%3", 2, 35, 0, 28, 40),
            geo("%2", 3, 64, 0, 28, 40),
            geo("%1", 4, 93, 0, 27, 40),
        ];
        let once = even_layout(&row_window(), &sb(), 34, None).expect("row layout");
        assert_eq!(even_layout(&evened_row, &sb(), 34, None).as_deref(), Some(once.as_str()));
        assert!(once.contains("34x40,0,0,0"), "sidebar cell is the pinned width: {once}");

        let evened_col = vec![
            geo("%0", 1, 0, 0, 34, 40),
            geo("%1", 2, 35, 0, 85, 10),
            geo("%2", 3, 35, 11, 85, 9),
            geo("%3", 4, 35, 21, 85, 9),
            geo("%4", 5, 35, 31, 85, 9),
        ];
        let once = even_layout(&column_window(), &sb(), 34, None).expect("column layout");
        assert_eq!(even_layout(&evened_col, &sb(), 34, None).as_deref(), Some(once.as_str()));
        assert!(once.contains("34x40,0,0,0"), "sidebar cell is the pinned width: {once}");
    }

    /// The pane ids a layout string names, in the order tmux binds them.
    ///
    /// Flattening `{}`/`[]` to commas leaves a plain token stream: every cell
    /// contributes `WxH`, `X`, `Y`, and a LEAF cell alone adds `ID`. The token
    /// after a CONTAINER's `Y` is the `WxH` of its first child, and a `WxH`
    /// token always holds an `x` while an id never does, so the two cannot be
    /// confused.
    fn layout_cell_ids(layout: &str) -> Vec<u64> {
        let body = layout.split_once(',').map_or(layout, |(_, b)| b);
        let flat: String = body
            .chars()
            .map(|c| if "{}[]".contains(c) { ',' } else { c })
            .collect();
        let toks: Vec<&str> = flat.split(',').filter(|t| !t.is_empty()).collect();
        let mut ids = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            if !toks[i].contains('x') {
                i += 1;
                continue;
            }
            match toks.get(i + 3) {
                Some(t) if !t.contains('x') => {
                    ids.push(t.parse().expect("cell id"));
                    i += 4;
                }
                _ => i += 3,
            }
        }
        ids
    }

    /// tmux does NOT read the pane ids in a layout string. It binds cells to
    /// panes POSITIONALLY, in the window's pane-list order, and rewrites
    /// whatever ids it was handed.
    ///
    /// Verified on 3.4: applying
    /// `fa10,120x40,0,0{34x40,0,0,0,28x40,35,0,3,28x40,64,0,2,27x40,93,0,1}`
    /// — content ids `3,2,1` — to a window whose panes were %0 %1 %2 %3 left to
    /// right moved no pane at all, and read back renumbered `0,1,2,3`.
    ///
    /// So `even_layout` preserves identity only while the order it emits cells
    /// in (the sidebar, then the content along the axis) IS the pane-list
    /// order, which `#{pane_index}` reports. Nothing about the strings this
    /// module generates would reveal that going wrong — every other test here
    /// compares whole strings — while the cost of it going wrong is the SIDEBAR
    /// being handed a content cell. Hence both a runtime guard and this test.
    #[test]
    fn every_layout_cell_reaches_the_pane_it_was_drawn_for() {
        for (name, panes) in [("row", row_window()), ("column", column_window())] {
            let layout = even_layout(&panes, &sb(), 34, None).expect(name);
            let mut by_index: Vec<&PaneInfo> = panes.iter().collect();
            by_index.sort_by_key(|p| p.index);
            let want: Vec<u64> = by_index.iter().map(|p| p.id.num()).collect();
            assert_eq!(layout_cell_ids(&layout), want, "{name} cell order: {layout}");
        }
    }

    /// And a window whose pane list disagrees with what the operator sees is
    /// left alone, rather than laid out into a permutation of itself.
    #[test]
    fn even_layout_refuses_a_pane_list_that_disagrees_with_the_eye() {
        // The sidebar is not first in the pane list. Its cell is emitted first
        // regardless, so laying this out would put a content pane in the
        // sidebar's slot and the SIDEBAR in a content cell.
        let mut sidebar_late = row_window();
        sidebar_late[0].index = 9;
        assert_eq!(content_shape(&sidebar_late, &sb()), ContentShape::Row);
        assert_eq!(even_layout(&sidebar_late, &sb(), 34, None), None);

        // Two content panes whose pane-list order is the reverse of their
        // left-to-right order: evening them would swap what they show.
        let mut swapped = row_window();
        swapped[1].index = 4;
        swapped[3].index = 2;
        assert_eq!(content_shape(&swapped, &sb()), ContentShape::Row);
        assert_eq!(even_layout(&swapped, &sb(), 34, None), None);

        // The same guard down a column.
        let mut col = column_window();
        col[2].index = 5;
        col[4].index = 3;
        assert_eq!(content_shape(&col, &sb()), ContentShape::Column);
        assert_eq!(even_layout(&col, &sb(), 34, None), None);

        // A repeated index is not an order either: tmux cannot hold two panes
        // in one list slot, so a list claiming it is not one to lay out from.
        let mut dup = row_window();
        dup[2].index = dup[1].index;
        assert_eq!(even_layout(&dup, &sb(), 34, None), None);

        // The guard is the ONLY thing refusing these: put the indices back in
        // the order the eye reads and each window lays out again.
        assert!(even_layout(&row_window(), &sb(), 34, None).is_some());
        assert!(even_layout(&column_window(), &sb(), 34, None).is_some());
    }

    /// `o` divides width, `s` divides height, and neither key ever evens the
    /// other axis: a split that produced the wrong shape built a tree.
    #[test]
    fn even_layout_divides_the_axis_the_key_asked_for() {
        assert_eq!(SplitDir::Vertical.shape(), ContentShape::Row);
        assert_eq!(SplitDir::Horizontal.shape(), ContentShape::Column);

        // `s` landing in a row, and `o` landing in a column: both no-ops.
        assert_eq!(
            even_layout(&row_window(), &sb(), 34, Some(SplitDir::Horizontal.shape())),
            None
        );
        assert_eq!(
            even_layout(&column_window(), &sb(), 34, Some(SplitDir::Vertical.shape())),
            None
        );

        // And the matching key divides the matching axis: widths differ across
        // the row, heights differ down the column, never the other way.
        let row = even_layout(&row_window(), &sb(), 34, Some(SplitDir::Vertical.shape()))
            .expect("row");
        assert!(row.contains("28x40,35,0,3"), "row panes keep full height: {row}");
        let col = even_layout(&column_window(), &sb(), 34, Some(SplitDir::Horizontal.shape()))
            .expect("column");
        assert!(col.contains("85x10,35,0,1"), "column panes keep full width: {col}");
    }

    #[test]
    fn even_layout_is_a_clean_no_op_with_nothing_to_even() {
        // Sidebar alone in the window.
        assert_eq!(even_layout(&[geo("%0", 1, 0, 0, 120, 40)], &sb(), 34, None), None);
        // Sidebar plus exactly one content pane: already "even".
        let one = vec![geo("%0", 1, 0, 0, 34, 40), geo("%1", 2, 35, 0, 85, 40)];
        assert_eq!(even_layout(&one, &sb(), 34, None), None);
        assert_eq!(even_layout(&one, &sb(), 34, Some(ContentShape::Row)), None);
        // No panes at all.
        assert_eq!(even_layout(&[], &sb(), 34, None), None);
        // A tree the operator built with a mix of `o` and `s`.
        let mixed = vec![
            geo("%0", 1, 0, 0, 34, 40),
            geo("%1", 2, 35, 0, 42, 40),
            geo("%2", 3, 78, 0, 42, 20),
            geo("%3", 4, 78, 21, 42, 19),
        ];
        assert_eq!(even_layout(&mixed, &sb(), 34, None), None);
        assert_eq!(even_layout(&mixed, &sb(), 34, Some(ContentShape::Row)), None);
    }

    /// Too little room to give every pane a cell: refuse, and keep refusing.
    /// A window this small must not be re-laid-out into something invalid on
    /// every split, which is the difference between a no-op and a thrash.
    #[test]
    fn even_layout_refuses_a_window_with_no_room_to_share() {
        // A 14-column window with three content panes tiles perfectly well —
        // the shape is a row — but pinning the sidebar to 10 leaves 3 inner
        // columns, and 2 of those are separators. One cell for three panes.
        let cramped = vec![
            geo("%0", 1, 0, 0, 8, 10),
            geo("%1", 2, 9, 0, 1, 10),
            geo("%2", 3, 11, 0, 1, 10),
            geo("%3", 4, 13, 0, 1, 10),
        ];
        assert_eq!(content_shape(&cramped, &sb()), ContentShape::Row);
        assert_eq!(even_layout(&cramped, &sb(), 10, None), None, "one cell for three panes");

        // The same refusal from the other end: a sidebar so wide there is no
        // content column left at all, and a nonsensical zero-width one.
        let starved = vec![geo("%0", 1, 0, 0, 34, 40), geo("%1", 2, 35, 0, 2, 40), geo("%2", 3, 38, 0, 2, 40)];
        assert_eq!(content_shape(&starved, &sb()), ContentShape::Row);
        assert_eq!(even_layout(&starved, &sb(), 39, None), None, "sidebar leaves no content");
        assert_eq!(even_layout(&starved, &sb(), 0, None), None, "zero-width sidebar refused");
        // One cell each IS legal, and is the floor: 40 - 36 - 1 = 3 inner
        // columns, one separator, one column per pane.
        assert_eq!(
            even_layout(&starved, &sb(), 36, None).as_deref(),
            Some("a204,40x40,0,0{36x40,0,0,0,1x40,37,0,1,1x40,39,0,2}")
        );
    }

    /// `apply_layout` is the one new mutating door, and it only opens for a
    /// checksummed layout string — never for a layout NAME like `tiled`, which
    /// would rearrange panes the operator placed, and never for something that
    /// could be read as a flag.
    #[test]
    fn apply_layout_refuses_anything_that_is_not_a_layout_string() {
        let p = sb();
        for bad in ["", "tiled", "even-horizontal", "-Ewhatever", "abcd", "ABCD,120x40,0,0", "zzzz,120x40,0,0", "a15,120x40,0,0", "a150,"] {
            assert!(
                matches!(apply_layout("ccmux", &p, bad), Err(TmuxError::BadTarget(_))),
                "{bad:?} must be refused before any tmux call"
            );
        }
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
