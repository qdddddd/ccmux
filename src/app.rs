//! STUB — owner: Integrator lane. SPEC §3.5.
//!
//! State, event loop body, keymap dispatch, actions. All IO orchestration.
//! Consumes `model`, `tmux`, `agents`. Never imports `ui` (the DAG is acyclic:
//! `ui` reads `app`, not the other way round).

use std::time::{Duration, Instant};

use crossterm::event::KeyEvent;

use crate::model::{Row, Session};
use crate::tmux::{PaneId, PaneInfo, PaneMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// `S` — the only destructive confirmation in v1.
    StopSession {
        session_id: String,
        short_id: String,
        name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// `n` — two fields: 0 = cwd, 1 = task text.
    NewBackground,
    /// `c` — one field: cwd.
    NewInteractive,
}

#[derive(Debug, Clone)]
pub struct Prompt {
    pub kind: PromptKind,
    /// `NewBackground`: ["<cwd>", "<task>"]. `NewInteractive`: ["<cwd>"].
    pub fields: Vec<String>,
    pub focus: usize,
    pub cursor: usize,
}

#[derive(Debug, Clone)]
pub struct LogsView {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Filter,
    Confirm(Confirm),
    Prompt(PromptKind),
    Help,
    Logs,
}

/// What a keypress resolved to. Returned by `App::on_key` so the event loop can
/// tell "redraw" from "quit" without inspecting state, and so the keymap is
/// testable without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Redraw,
    Quit,
}

pub struct App {
    // config
    pub tmux_session: String,
    pub sidebar_width: u16,
    pub interval: Duration,
    pub dark: bool,
    pub home: Option<String>,

    // data
    pub sessions: Vec<Session>,
    pub rows: Vec<Row>,
    pub now_ms: i64,

    // selection: `selected` indexes `rows` and ALWAYS points at Row::Session
    // (or equals rows.len() when the list is empty). `selected_key` is the
    // session_id under the cursor; it is what survives a re-sort or regroup.
    pub selected: usize,
    pub selected_key: Option<String>,
    pub scroll: usize,
    /// Session rows the list can currently show. Written by `main.rs` every
    /// frame from `ui::list_viewport_rows(terminal_height)`. `app.rs` reads it
    /// for Ctrl-d/Ctrl-u and scroll clamping but never computes it.
    pub viewport: u16,

    // view state
    pub filter: String,
    pub show_completed: bool,
    pub mode: Mode,
    pub prompt: Option<Prompt>,
    pub logs: Option<LogsView>,

    // tmux
    pub map: PaneMap,
    pub map_dirty: bool,
    /// TRANSIENT resolution cache for interactive sessions, rebuilt from /proc
    /// every tick and NEVER persisted to `@ccmux_map`.
    /// session_id -> the ccmux pane hosting it (§5.4 first pass).
    /// Interactive sessions get no map entry (§5.5) because they have no
    /// session_id at open time; this cache is what makes their open marker,
    /// their `Enter` jump, and §8.5's refusal check work.
    pub interactive_panes: std::collections::BTreeMap<String, PaneId>,
    pub sidebar_pane: Option<PaneId>,
    pub panes: Vec<PaneInfo>,
    /// True when running outside tmux: list/filter/refresh/logs work, every
    /// pane verb refuses with a message (§9.5).
    pub degraded: bool,

    // messaging + health
    pub message: Option<(String, MsgLevel)>,
    pub msg_deadline: Option<Instant>,
    pub poll_error: Option<String>,
    pub fail_streak: u32,
    pub last_poll: Instant,

    pub should_quit: bool,
}

impl App {
    /// Does no IO beyond `env::var("HOME")` and `tmux::inside_tmux()`.
    pub fn new(
        _tmux_session: String,
        _sidebar_width: u16,
        _interval: Duration,
        _dark: bool,
    ) -> Self {
        todo!()
    }

    /// One-time startup IO: load `@ccmux_map`, resolve `@ccmux_sidebar`,
    /// set `degraded`. Never fails; failures degrade.
    pub fn init(&mut self) {
        todo!()
    }

    /// Called when `last_poll.elapsed() >= effective_interval()`.
    /// Order is fixed:
    ///   1. now_ms = Utc::now().timestamp_millis()
    ///   2. panes = list_panes_in_session(tmux_session)   [skipped when degraded]
    ///   3. map.reconcile(&panes) -> map_dirty |= changed
    ///   4. agents::poll() -> sessions (on Err: keep last good, bump fail_streak)
    ///   5. rebuild `interactive_panes`: for every session with
    ///      `kind == Interactive`, tmux::resolve_pane_for_pid(pid, &panes)
    ///      [skipped when degraded; cleared and rebuilt, never merged]
    ///   6. rebuild rows, re-anchor selection by selected_key
    ///   7. flush the map if map_dirty
    ///   8. pin_sidebar (unconditional, §1.3)
    ///   9. last_poll = Instant::now()
    pub fn tick(&mut self) {
        todo!()
    }

    /// Poll interval in force: `interval`, or 10s once `fail_streak >= 3`.
    pub fn effective_interval(&self) -> Duration {
        todo!()
    }

    /// True when a timed message just expired (caller should redraw).
    pub fn check_message_timeout(&mut self) -> bool {
        todo!()
    }

    /// THE keymap. Full dispatch table in §8. Pure with respect to the
    /// terminal: it may shell out, but it never touches stdout.
    pub fn on_key(&mut self, _key: KeyEvent) -> Action {
        todo!()
    }

    // ── read-only accessors used by ui.rs ────────────────────────────────────

    pub fn selected_session(&self) -> Option<&Session> {
        todo!()
    }

    /// First live pane showing `session_id`: the reconciled `map` first, then
    /// the transient `interactive_panes` cache. Both are ccmux-scoped, so a
    /// `Some` result is always safe to pass to an R2-gated mutation.
    pub fn pane_of(&self, _session_id: &str) -> Option<PaneId> {
        todo!()
    }

    /// `#{pane_index}` of `pane`, for the sidebar's pane badge.
    pub fn pane_index_of(&self, _pane: &PaneId) -> Option<u32> {
        todo!()
    }

    /// `pane_of(session_id).is_some()`. Drives the §6.4 open marker for both
    /// background (map) and interactive (/proc cache) sessions.
    pub fn is_open(&self, _session_id: &str) -> bool {
        todo!()
    }

    // ── selection ────────────────────────────────────────────────────────────

    pub fn select_next(&mut self) {
        todo!()
    }

    pub fn select_prev(&mut self) {
        todo!()
    }

    pub fn select_first(&mut self) {
        todo!()
    }

    pub fn select_last(&mut self) {
        todo!()
    }

    pub fn select_half_page(&mut self, _down: bool) {
        todo!()
    }

    /// Move to the first session row of the next (or previous) non-empty group.
    pub fn cycle_group(&mut self, _forward: bool) {
        todo!()
    }

    /// Re-point `selected` at `selected_key` after `rows` changed; falls back to
    /// the nearest valid session row, then to the first one.
    pub fn reanchor_selection(&mut self) {
        todo!()
    }

    pub fn clamp_scroll(&mut self) {
        todo!()
    }

    // ── verbs (each is one keymap entry's whole effect) ──────────────────────

    pub fn act_open(&mut self, _dir: crate::tmux::SplitDir) {
        todo!()
    }

    pub fn act_enter(&mut self) {
        todo!()
    }

    pub fn act_close_pane(&mut self) {
        todo!()
    }

    pub fn act_request_stop(&mut self) {
        todo!()
    }

    pub fn act_confirm_stop(&mut self) {
        todo!()
    }

    pub fn act_open_logs(&mut self) {
        todo!()
    }

    pub fn act_force_refresh(&mut self) {
        todo!()
    }

    pub fn act_submit_prompt(&mut self) {
        todo!()
    }

    // ── messaging ────────────────────────────────────────────────────────────

    /// Shows `text` in the footer for 4s.
    pub fn flash(&mut self, _text: impl Into<String>, _level: MsgLevel) {
        todo!()
    }
}
