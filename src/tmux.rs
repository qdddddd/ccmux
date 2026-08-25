//! STUB — owner: Tmux lane. SPEC §3.2.
//!
//! Every tmux interaction in the program, plus /proc ancestry resolution.
//! No ratatui, no `claude`, no knowledge of `model::Session`. The pane map
//! stores session ids as opaque strings so this module stays decoupled.
//!
//! Signatures below are authoritative (SPEC §3). Bodies are the owner's
//! business — replace `todo!()`, do not change a signature without a spec
//! amendment.

use std::collections::BTreeMap;

pub const WINDOW_NAME: &str = "cc";
pub const OPT_MAP: &str = "@ccmux_map";
pub const OPT_SIDEBAR: &str = "@ccmux_sidebar";
pub const OPT_WIDTH: &str = "@ccmux_width";

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

impl std::fmt::Display for TmuxError {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        todo!()
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
    pub fn parse(_s: &str) -> Option<PaneId> {
        todo!()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── Pane facts ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
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
        todo!()
    }
}

// ── Raw runner ──────────────────────────────────────────────────────────────

/// The ONLY place `std::process::Command::new("tmux")` appears.
/// Always argv; never `sh -c`; never a formatted shell string.
pub fn tmux(_args: &[&str]) -> Result<String, TmuxError> {
    todo!()
}

/// Convenience: true when `tmux(args)` returned Ok.
pub fn tmux_ok(_args: &[&str]) -> bool {
    todo!()
}

// ── Environment probes ──────────────────────────────────────────────────────

/// `std::env::var("TMUX").is_ok()`
pub fn inside_tmux() -> bool {
    todo!()
}

/// `tmux display-message -p '#{session_name}'`; None when not inside tmux.
pub fn current_session_name() -> Option<String> {
    todo!()
}

/// `tmux has-session -t <session>` — exit 0 => true. A dead server also
/// yields exit != 0, which correctly reads as "does not exist".
pub fn has_session(_session: &str) -> bool {
    todo!()
}

/// `tmux -V` spawns successfully.
pub fn server_available() -> bool {
    todo!()
}

// ── Session lifecycle (launcher only) ───────────────────────────────────────

/// `tmux new-session -d -s <session> -n cc -P -F '#{pane_id}' -- <sidebar_cmd>`
/// `sidebar_cmd` is a shell-command string already built with `sh_quote`.
/// Returns the sidebar pane id.
pub fn create_session(_session: &str, _sidebar_cmd: &str) -> Result<PaneId, TmuxError> {
    todo!()
}

/// Applies the ccmux session options: `status off`, `mouse on`,
/// `@ccmux_sidebar`, `@ccmux_width`, and an empty `@ccmux_map`.
pub fn configure_session(_session: &str, _sidebar: &PaneId, _width: u16) -> Result<(), TmuxError> {
    todo!()
}

/// `switch-client -t` when inside tmux, else `attach-session -t`.
/// On the attach path this replaces the current terminal view and normally
/// does not return until the client detaches.
pub fn attach_or_switch(_session: &str) -> Result<(), TmuxError> {
    todo!()
}

// ── Pane enumeration and mutation ───────────────────────────────────────────

/// `tmux list-panes -t <session> -s -F '<FMT>'` — session-scoped (R3).
/// FMT = "#{pane_id}\t#{pane_pid}\t#{pane_index}\t#{pane_left}\t#{pane_top}\t\
///        #{pane_width}\t#{pane_height}\t#{pane_active}\t#{session_name}\t#{window_index}"
pub fn list_panes_in_session(_session: &str) -> Result<Vec<PaneInfo>, TmuxError> {
    todo!()
}

/// READ-ONLY server-wide enumeration. Permitted ONLY for interactive-session
/// discovery (§5.4). Never feeds reconciliation, never feeds a mutation.
pub fn list_panes_all() -> Result<Vec<PaneInfo>, TmuxError> {
    todo!()
}

/// R2 gate. Err(BadTarget) when `pane` is absent from `session`.
pub fn assert_in_session(_pane: &PaneId, _session: &str) -> Result<(), TmuxError> {
    todo!()
}

/// `tmux split-window <dir.tmux_flag()> -t <target> -P -F '#{pane_id}' -d -- <shell_cmd>`
/// `-d` keeps focus in the sidebar so the operator can keep driving the list.
/// Calls `assert_in_session(target, session)` first. Returns the new pane id.
pub fn split(
    _session: &str,
    _target: &PaneId,
    _dir: SplitDir,
    _shell_cmd: &str,
) -> Result<PaneId, TmuxError> {
    todo!()
}

/// Insert a pane to the LEFT of `target` (`split-window -h -b`). Used only to
/// heal a missing sidebar.
pub fn split_left_of(
    _session: &str,
    _target: &PaneId,
    _shell_cmd: &str,
) -> Result<PaneId, TmuxError> {
    todo!()
}

/// `tmux kill-pane -t <pane>`. R2-gated. SAFE with respect to Claude sessions:
/// PROBE-FINDINGS §3 proves the agent survives.
pub fn kill_pane(_session: &str, _pane: &PaneId) -> Result<(), TmuxError> {
    todo!()
}

/// `tmux select-pane -t <pane>`. R2-gated.
pub fn select_pane(_session: &str, _pane: &PaneId) -> Result<(), TmuxError> {
    todo!()
}

/// `tmux resize-pane -t <pane> -x <cols>`. R2-gated.
/// Verified no-op (exit 0) when `pane` is the window's only pane.
pub fn resize_pane_width(_session: &str, _pane: &PaneId, _cols: u16) -> Result<(), TmuxError> {
    todo!()
}

/// `resize_pane_width` with errors swallowed. Call this on every tick and after
/// every split/kill (§1.3).
pub fn pin_sidebar(_session: &str, _sidebar: &PaneId, _cols: u16) {
    todo!()
}

/// Leftmost pane by `#{pane_left}`, tie-broken by lowest `pane_index`.
pub fn leftmost_pane(_panes: &[PaneInfo]) -> Option<PaneId> {
    todo!()
}

/// Rightmost pane by `#{pane_left}` EXCLUDING `sidebar`. None when the sidebar
/// is alone in the window.
pub fn rightmost_pane_excluding(_panes: &[PaneInfo], _sidebar: &PaneId) -> Option<PaneId> {
    todo!()
}

/// `tmux switch-client -t <session>` then `select-pane -t <pane>`. Used to jump
/// to an interactive session living in a foreign tmux session (§5.4). This is
/// the one mutation permitted outside `cli.session`, and it only moves the
/// client's focus — it creates, kills, and resizes nothing.
pub fn focus_foreign_pane(_session_name: &str, _pane: &PaneId) -> Result<(), TmuxError> {
    todo!()
}

// ── User options (map persistence) ──────────────────────────────────────────

/// `tmux show-options -t <session> -qv <key>`.
/// The `-q` is MANDATORY: without it an unset user option exits 1 with
/// "invalid option". With it: empty stdout, exit 0. Returns None for empty.
pub fn get_user_option(_session: &str, _key: &str) -> Option<String> {
    todo!()
}

/// `tmux set-option -t <session> <key> <value>` as argv, so `value` needs no
/// escaping whatsoever — JSON with quotes and backslashes round-trips verbatim
/// (verified).
pub fn set_user_option(_session: &str, _key: &str, _value: &str) -> Result<(), TmuxError> {
    todo!()
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
        todo!()
    }

    pub fn get(&self, _pane: &PaneId) -> Option<&PaneEntry> {
        todo!()
    }

    pub fn insert(&mut self, _pane: &PaneId, _entry: PaneEntry) {
        todo!()
    }

    pub fn remove(&mut self, _pane: &PaneId) {
        todo!()
    }

    /// First pane currently mapped to `session_id`, lowest pane id first.
    pub fn pane_for_session(&self, _session_id: &str) -> Option<PaneId> {
        todo!()
    }

    /// All panes mapped to `session_id`, ascending. Double-attach is legal
    /// (PROBE-FINDINGS §3), so this may return more than one.
    pub fn panes_for_session(&self, _session_id: &str) -> Vec<PaneId> {
        todo!()
    }

    /// Drop entries whose pane id is not in `live`. Entries whose *Claude
    /// session* has vanished are KEPT as long as the pane exists — the pane is
    /// still on screen showing its exit notice and must remain closable.
    /// Returns true when anything was removed.
    pub fn reconcile(&mut self, _live: &[PaneInfo]) -> bool {
        todo!()
    }
}

/// Read `@ccmux_map` and deserialize. Any failure (unset, empty, bad JSON,
/// `v != 1`) yields `PaneMap::new()` — never an error. A corrupt map must not
/// stop the sidebar from starting.
pub fn load_map(_session: &str) -> PaneMap {
    todo!()
}

/// Serialize and write to `@ccmux_map`.
pub fn save_map(_session: &str, _map: &PaneMap) -> Result<(), TmuxError> {
    todo!()
}

// ── Shell quoting (§7) ──────────────────────────────────────────────────────

/// POSIX single-quote escaping for the ONE place tmux needs a shell string.
pub fn sh_quote(_s: &str) -> String {
    todo!()
}

/// Join `parts` into a single `sh`-safe command line: `sh_quote` each, join
/// with a single space.
pub fn sh_join(_parts: &[&str]) -> String {
    todo!()
}

// ── /proc ancestry (§5.4) ───────────────────────────────────────────────────

/// Parse `/proc/<pid>/stat`.
/// PARSE RULE: `comm` (field 2) is parenthesized and MAY CONTAIN SPACES AND
/// PARENTHESES. Find the LAST b')' in the line; the remainder splits on
/// whitespace as [state, ppid, ...]; ppid is index 1. Never `split_whitespace`
/// the whole line. Returns None on any IO or parse failure.
pub fn ppid_of(_pid: i32) -> Option<i32> {
    todo!()
}

/// Walk `ppid_of` upward from `pid`, inclusive of `pid`, stopping at pid <= 1,
/// at a repeated pid, or after `max_depth` steps (use 32).
pub fn ancestry(_pid: i32, _max_depth: usize) -> Vec<i32> {
    todo!()
}

/// Walk up from `pid` and return the first pane whose `PaneInfo::pid` appears
/// in the ancestry chain. This is how an interactive Claude session is mapped
/// to the pane that hosts it (PROBE-FINDINGS §4). Background sessions are
/// daemon-owned and will always return None here — that is expected, not an error.
pub fn resolve_pane_for_pid(_pid: i32, _panes: &[PaneInfo]) -> Option<PaneInfo> {
    todo!()
}
