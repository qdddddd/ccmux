//! State, event loop body, keymap dispatch, actions. SPEC §3.5.
//!
//! All IO orchestration lives here. Consumes `model`, `tmux`, `agents`. Never
//! imports `ui` (the DAG is acyclic: `ui` reads `app`, not the other way round).
//!
//! Panic policy (SPEC §9): no `unwrap`, no `expect`, no `panic!` on any sidebar
//! path. Every failure degrades to a footer message over the last good list.

use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::agents::{self, AgentsError};
use crate::model::{self, Group, Kind, ParseError, Row, Session};
use crate::tmux::{self, PaneEntry, PaneId, PaneInfo, PaneMap, SplitDir, TmuxError};

/// How long a flashed footer message stays up (SPEC §6.8 item 2).
const MSG_TTL: Duration = Duration::from_secs(4);
/// Backoff interval once `fail_streak` crosses `FAIL_BACKOFF_AT` (SPEC §4.2).
const BACKOFF: Duration = Duration::from_secs(10);
const FAIL_BACKOFF_AT: u32 = 3;
/// Lines requested from `claude logs` for the `L` overlay.
const LOGS_LINES: usize = 500;
/// SPEC §9.1: `poll_error` is truncated to 120 chars.
const POLL_ERR_MAX: usize = 120;
/// How long the `S` confirmation must have been on screen before `y` is
/// accepted (SPEC AMENDMENT §8.2).
///
/// crossterm hands us whatever the tty already buffered, so without this a
/// paste or a fast typist's "Sy" arrives as two key events in the same instant:
/// `S` opens the modal and the buffered `y` confirms it before a human could
/// read the prompt. `claude stop` is the one verb that ends a running agent, so
/// it must be answered by a keystroke made AFTER the question was visible.
const CONFIRM_ARM_DELAY: Duration = Duration::from_millis(250);
/// Names stored in `@ccmux_map`, truncated. tmux rejects a `set-option` value
/// over ~16 KB (measured: ok at 16323 bytes, "command too long" at 16324), and
/// `name` is the only unbounded field in a `PaneEntry`.
const MAP_NAME_MAX: usize = 80;
/// Columns left for the Claude panes when the sidebar is pinned (§1.3).
const MIN_CONTENT_COLS: u16 = 20;

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
    /// Scroll offset of the `?` overlay. Separate from `scroll` because
    /// `clamp_scroll` pins `scroll` to the session list's bounds every frame,
    /// which would make the help overlay unscrollable (SPEC §6.7/§8.9).
    pub help_scroll: usize,
    /// Rows an overlay body can show, written by `main.rs` every frame from the
    /// terminal height minus the overlay's border. `Mode::Logs` and `Mode::Help`
    /// clamp their scroll against it; the list's `viewport` is a different
    /// number because the sidebar has a header, footer and detail block.
    pub overlay_viewport: u16,
    /// Lines the `?` overlay renders, written by `main.rs` from
    /// `ui::help_line_count()` — the keymap table lives in `ui`, and `app` must
    /// not import it.
    pub help_lines: usize,

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
    /// When the `S` modal went up. `key_confirm` refuses a `y` that arrives
    /// within `CONFIRM_ARM_DELAY` of it, and `None` means "not armed" — both
    /// fail closed.
    pub confirm_armed_at: Option<Instant>,

    pub message: Option<(String, MsgLevel)>,
    pub msg_deadline: Option<Instant>,
    pub poll_error: Option<String>,
    pub fail_streak: u32,
    pub last_poll: Instant,

    pub should_quit: bool,
}

impl App {
    /// Does no IO beyond `env::var("HOME")` and `tmux::inside_target_server()`.
    pub fn new(tmux_session: String, sidebar_width: u16, interval: Duration, dark: bool) -> Self {
        // `inside_target_server`, not `inside_tmux`: under `--socket` the two
        // disagree, and every pane verb targets the socket, not `$TMUX`.
        let degraded = !tmux::inside_target_server();
        App {
            tmux_session,
            // Idempotent re-clamp of SPEC §1.1's 20..=120 rule: `main.rs` clamps
            // too, but `App` is constructed directly by tests as well.
            sidebar_width: sidebar_width.clamp(20, 120),
            interval,
            dark,
            home: std::env::var("HOME").ok(),

            sessions: Vec::new(),
            rows: Vec::new(),
            now_ms: 0,

            selected: 0,
            selected_key: None,
            scroll: 0,
            viewport: 0,
            help_scroll: 0,
            overlay_viewport: 0,
            help_lines: 0,

            filter: String::new(),
            show_completed: true,
            mode: Mode::Normal,
            prompt: None,
            logs: None,

            map: PaneMap::new(),
            map_dirty: false,
            interactive_panes: std::collections::BTreeMap::new(),
            sidebar_pane: None,
            panes: Vec::new(),
            degraded,

            confirm_armed_at: None,

            message: None,
            msg_deadline: None,
            poll_error: None,
            fail_streak: 0,
            // Backdated so the event loop's first iteration polls immediately.
            // `checked_sub` because a bare `Instant - Duration` can panic when
            // the process starts within `interval` of the monotonic epoch.
            last_poll: Instant::now()
                .checked_sub(interval)
                .unwrap_or_else(Instant::now),

            should_quit: false,
        }
    }

    /// One-time startup IO: load `@ccmux_map`, resolve `@ccmux_sidebar`,
    /// set `degraded`. Never fails; failures degrade.
    pub fn init(&mut self) {
        self.degraded = !tmux::inside_target_server();
        if self.degraded {
            // §9.5: the map is held in memory only; load/save are skipped.
            self.map = PaneMap::new();
            return;
        }
        self.map = tmux::load_map(&self.tmux_session);
        self.sidebar_pane = tmux::get_user_option(&self.tmux_session, tmux::OPT_SIDEBAR)
            .and_then(|s| PaneId::parse(&s));
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
        // 1
        self.now_ms = chrono::Utc::now().timestamp_millis();

        // 2 + 3 (+ §5.3 step 5)
        self.refresh_panes();

        // 4 — §9.1/§9.3: an Err keeps the last good list on screen; `[]` is a
        // valid empty result, not an error.
        match agents::poll() {
            Ok(sessions) => {
                self.sessions = sessions;
                self.poll_error = None;
                self.fail_streak = 0;
            }
            Err(e) => {
                self.fail_streak = self.fail_streak.saturating_add(1);
                self.poll_error = Some(model::truncate_end(&agents_msg(&e), POLL_ERR_MAX));
            }
        }

        // 5 — cleared and rebuilt wholesale, never merged, so it cannot go stale.
        self.interactive_panes.clear();
        if !self.degraded {
            for s in &self.sessions {
                if s.kind == Kind::Interactive && s.pid > 0
                    && let Some(info) = tmux::resolve_pane_for_pid(s.pid, &self.panes) {
                        self.interactive_panes.insert(s.session_id.clone(), info.id);
                    }
            }
        }

        // 6
        self.rebuild_rows();

        // 7
        self.save_map_now();

        // 8 — unconditional (§1.3); this is what heals a manual resize.
        self.pin_sidebar();

        // 9
        self.last_poll = Instant::now();
    }

    /// Poll interval in force: `interval`, or 10s once `fail_streak >= 3`.
    pub fn effective_interval(&self) -> Duration {
        if self.fail_streak >= FAIL_BACKOFF_AT {
            BACKOFF
        } else {
            self.interval
        }
    }

    /// True when a timed message just expired (caller should redraw).
    pub fn check_message_timeout(&mut self) -> bool {
        match self.msg_deadline {
            Some(deadline) if Instant::now() >= deadline => {
                self.message = None;
                self.msg_deadline = None;
                true
            }
            _ => false,
        }
    }

    /// THE keymap. Full dispatch table in §8. Pure with respect to the
    /// terminal: it may shell out, but it never touches stdout.
    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        // §8.9: Ctrl-c quits from ANY mode, immediately, without confirming.
        // Checked before mode dispatch on purpose.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.should_quit = true;
            return Action::Quit;
        }

        // §8.9: dispatch on mode FIRST; only Normal sees the §8.1 table.
        match self.mode.clone() {
            Mode::Normal => self.key_normal(key),
            Mode::Filter => self.key_filter(key),
            Mode::Confirm(_) => self.key_confirm(key),
            Mode::Prompt(_) => self.key_prompt(key),
            Mode::Help => self.key_help(key),
            Mode::Logs => self.key_logs(key),
        }
    }

    /// One bracketed-paste event's text.
    ///
    /// SPEC AMENDMENT (§8.9): `main.rs` now enables bracketed paste, so pasted
    /// text arrives here as ONE event instead of as a burst of key events the
    /// §8.1 keymap would execute. Pasting `Sync branch` into the focused
    /// sidebar previously ran `S`, `y` (stop the selected agent), `n` (new
    /// background prompt), the remaining letters as its task text, and `\r` to
    /// dispatch it.
    ///
    /// Normal mode DISCARDS the paste — pasted prose is not a keymap. Filter
    /// and Prompt take it as literal text, with control characters stripped so
    /// an embedded newline cannot submit anything.
    pub fn on_paste(&mut self, text: &str) -> Action {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        if clean.is_empty() {
            return Action::None;
        }
        match self.mode {
            Mode::Filter => {
                self.filter.push_str(&clean);
                self.rebuild_rows();
                Action::Redraw
            }
            Mode::Prompt(_) => {
                let Some(mut p) = self.prompt.clone() else {
                    return Action::None;
                };
                let mut at = p.cursor;
                if let Some(field) = p.fields.get_mut(p.focus) {
                    for c in clean.chars() {
                        insert_char_at(field, at, c);
                        at += 1;
                    }
                }
                p.cursor = at;
                self.prompt = Some(p);
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    // ── read-only accessors used by ui.rs ────────────────────────────────────

    pub fn selected_session(&self) -> Option<&Session> {
        match self.rows.get(self.selected) {
            Some(Row::Session { idx }) => self.sessions.get(*idx),
            _ => None,
        }
    }

    /// First live pane showing `session_id`: the reconciled `map` first, then
    /// the transient `interactive_panes` cache. Both are ccmux-scoped, so a
    /// `Some` result is always safe to pass to an R2-gated mutation.
    pub fn pane_of(&self, session_id: &str) -> Option<PaneId> {
        self.map
            .pane_for_session(session_id)
            .or_else(|| self.interactive_panes.get(session_id).cloned())
    }

    /// `#{pane_index}` of `pane`, for the sidebar's pane badge.
    pub fn pane_index_of(&self, pane: &PaneId) -> Option<u32> {
        self.panes.iter().find(|p| &p.id == pane).map(|p| p.index)
    }

    /// `pane_of(session_id).is_some()`. Drives the §6.4 open marker for both
    /// background (map) and interactive (/proc cache) sessions.
    ///
    /// SPEC §3.5 surface. `ui.rs` resolves the marker through its own private,
    /// field-only helper so its no-panic matrix can render an `App` built by
    /// struct literal; the two are specified identical.
    #[allow(dead_code)]
    pub fn is_open(&self, session_id: &str) -> bool {
        self.pane_of(session_id).is_some()
    }

    // ── selection ────────────────────────────────────────────────────────────

    pub fn select_next(&mut self) {
        let n = self.rows.len();
        let start = if self.selected >= n { 0 } else { self.selected + 1 };
        if let Some(i) = (start..n).find(|&i| self.is_session_row(i)) {
            self.set_selected(i);
        }
    }

    pub fn select_prev(&mut self) {
        let upper = self.selected.min(self.rows.len());
        if let Some(i) = (0..upper).rev().find(|&i| self.is_session_row(i)) {
            self.set_selected(i);
        }
    }

    pub fn select_first(&mut self) {
        if let Some(i) = (0..self.rows.len()).find(|&i| self.is_session_row(i)) {
            self.set_selected(i);
        }
    }

    pub fn select_last(&mut self) {
        if let Some(i) = (0..self.rows.len()).rev().find(|&i| self.is_session_row(i)) {
            self.set_selected(i);
        }
    }

    pub fn select_half_page(&mut self, down: bool) {
        let step = ((self.viewport as usize) / 2).max(1);
        for _ in 0..step {
            if down {
                self.select_next();
            } else {
                self.select_prev();
            }
        }
    }

    /// Move to the first session row of the next (or previous) non-empty group.
    /// `rows` only carries headers for non-empty groups (§6.3), so walking the
    /// header positions *is* walking the non-empty groups.
    pub fn cycle_group(&mut self, forward: bool) {
        let headers: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, Row::Header { .. }))
            .map(|(i, _)| i)
            .collect();
        if headers.is_empty() {
            return;
        }
        let cur = headers
            .iter()
            .rposition(|&h| h <= self.selected)
            .unwrap_or(0);
        let next = if forward {
            (cur + 1) % headers.len()
        } else {
            (cur + headers.len() - 1) % headers.len()
        };
        let Some(&h) = headers.get(next) else { return };
        if let Some(i) = (h + 1..self.rows.len()).find(|&i| self.is_session_row(i)) {
            self.set_selected(i);
        }
    }

    /// Re-point `selected` at `selected_key` after `rows` changed; falls back to
    /// the nearest valid session row, then to the first one.
    pub fn reanchor_selection(&mut self) {
        let session_rows: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.is_session_row(i))
            .collect();

        let Some(&first) = session_rows.first() else {
            // Empty list: `selected` parks at rows.len() per the field contract.
            self.selected = self.rows.len();
            self.selected_key = None;
            self.scroll = 0;
            return;
        };

        if let Some(key) = self.selected_key.clone()
            && let Some(&i) = session_rows
                .iter()
                .find(|&&i| self.key_at(i).as_deref() == Some(key.as_str()))
            {
                self.selected = i;
                self.clamp_scroll();
                return;
            }

        let target = self.selected;
        let nearest = session_rows
            .iter()
            .copied()
            .min_by_key(|&i| i.abs_diff(target))
            .unwrap_or(first);
        self.selected = nearest;
        self.selected_key = self.key_at(nearest);
        self.clamp_scroll();
    }

    pub fn clamp_scroll(&mut self) {
        let vp = self.viewport as usize;
        let total = self.rows.len();
        if vp == 0 || total == 0 {
            self.scroll = 0;
            return;
        }
        if self.selected < total {
            if self.selected < self.scroll {
                self.scroll = self.selected;
            } else if self.selected >= self.scroll.saturating_add(vp) {
                self.scroll = self.selected + 1 - vp;
            }
        }
        if self.scroll + vp > total {
            self.scroll = total.saturating_sub(vp);
        }
    }

    // ── verbs (each is one keymap entry's whole effect) ──────────────────────

    /// `o` (Vertical / tmux `-h`) and `s` (Horizontal / tmux `-v`). SPEC §8.4.
    pub fn act_open(&mut self, dir: SplitDir) {
        if self.degraded {
            self.flash("not inside tmux — open unavailable", MsgLevel::Warn);
            return;
        }
        let Some(sel) = self.selected_session() else {
            return;
        };
        if !sel.is_attachable() {
            // §9.7: there is no `claude attach` for an interactive session, so a
            // split would have nothing to run. Delegate to the §5.4 jump.
            self.act_jump_interactive();
            return;
        }
        let session_id = sel.session_id.clone();
        let short_id = sel.id.clone().unwrap_or_default();
        let name = sel.name.clone();

        let Some(anchor) = self.split_anchor() else {
            self.flash("no pane to split — sidebar unmapped", MsgLevel::Error);
            return;
        };
        let cmd = agents::attach_pane_cmd(&short_id);
        match tmux::split(&self.tmux_session, &anchor, dir, &cmd) {
            Ok(pane) => {
                self.map.insert(
                    &pane,
                    PaneEntry {
                        session_id,
                        short_id,
                        // Bounded: `@ccmux_map` has a hard ~16 KB ceiling and
                        // this is its only unbounded field. It is display-only,
                        // used when a session has vanished from polls.
                        name: model::truncate_end(&name, MAP_NAME_MAX),
                        opened_at: self.now_ms,
                    },
                );
                self.map_dirty = true;
                // Refresh before flashing: the new pane's `#{pane_index}` only
                // exists in a freshly enumerated list.
                self.refresh_panes();
                self.save_map_now();
                self.pin_sidebar();
                match self.pane_index_of(&pane) {
                    Some(i) => self.flash(format!("opened {name} in pane {i}"), MsgLevel::Info),
                    None => self.flash(format!("opened {name}"), MsgLevel::Info),
                }
            }
            Err(e) => self.flash(format!("split failed: {}", tmux_msg(&e)), MsgLevel::Error),
        }
    }

    /// `Enter` — open or jump. SPEC §8.3.
    pub fn act_enter(&mut self) {
        if self.degraded {
            self.flash("not inside tmux — open unavailable", MsgLevel::Warn);
            return;
        }
        let Some(sel) = self.selected_session() else {
            return;
        };
        if !sel.is_attachable() {
            self.act_jump_interactive();
            return;
        }
        let session_id = sel.session_id.clone();
        match self.pane_of(&session_id) {
            // Already open: jump rather than re-split. A UX preference, not a
            // correctness requirement — double-attach is legal (PROBE §3).
            Some(pane) => self.jump_to_ccmux_pane(&pane),
            None => self.act_open(SplitDir::Vertical),
        }
    }

    /// `x` — close the pane, leaving the agent running. SPEC §8.5.
    pub fn act_close_pane(&mut self) {
        if self.degraded {
            self.flash("not inside tmux — close unavailable", MsgLevel::Warn);
            return;
        }
        let Some(sel) = self.selected_session() else {
            return;
        };
        // STEP 3 IS A CORRECTNESS GATE, NOT POLITENESS (SPEC §8.5).
        // PROBE-FINDINGS §3 proves `kill-pane` leaves the agent running only for
        // BACKGROUND sessions, which are daemon-owned. An interactive session IS
        // a descendant of its pane's pid (PROBE §4), so killing the pane would
        // SIGHUP Claude and destroy in-flight work while we told the operator
        // "agent still running". Refuse before any tmux call is issued.
        if sel.kind == Kind::Interactive {
            self.flash(
                "refusing: closing this pane would end the interactive session — exit Claude inside the pane instead",
                MsgLevel::Warn,
            );
            return;
        }
        let session_id = sel.session_id.clone();
        let Some(pane) = self.pane_of(&session_id) else {
            self.flash("not open", MsgLevel::Warn);
            return;
        };
        if Some(&pane) == self.sidebar_pane.as_ref() {
            self.flash("refusing to close the sidebar", MsgLevel::Warn);
            return;
        }
        // Read the index before the kill; afterwards the pane is gone.
        let idx = self.pane_index_of(&pane);
        match tmux::kill_pane(&self.tmux_session, &pane) {
            Ok(()) => {
                self.map.remove(&pane);
                self.map_dirty = true;
                self.refresh_panes();
                self.save_map_now();
                self.pin_sidebar();
                // §8.5 step 9: this wording is the operator-facing statement of
                // PROBE-FINDINGS §3, shown every time, so nobody confuses `x`
                // with `S`.
                match idx {
                    Some(i) => self.flash(
                        format!("closed pane {i} — agent still running"),
                        MsgLevel::Info,
                    ),
                    None => self.flash("closed pane — agent still running", MsgLevel::Info),
                }
            }
            Err(e) => self.flash(format!("close failed: {}", tmux_msg(&e)), MsgLevel::Error),
        }
    }

    /// `S` — enter the confirmation modal. Never calls `agents::stop`. SPEC §8.2.
    pub fn act_request_stop(&mut self) {
        let Some(sel) = self.selected_session() else {
            return;
        };
        if !sel.is_attachable() {
            self.flash("cannot stop an interactive session", MsgLevel::Warn);
            return;
        }
        if sel.group() == Group::Completed {
            self.flash("session already completed", MsgLevel::Warn);
            return;
        }
        // THE capture. `act_confirm_stop` uses these values, not the live
        // cursor: a poll landing between `S` and `y` can reorder the list, and
        // without capture `y` would stop whatever slid under the cursor.
        self.mode = Mode::Confirm(Confirm::StopSession {
            session_id: sel.session_id.clone(),
            short_id: sel.id.clone().unwrap_or_default(),
            name: sel.name.clone(),
        });
        // The modal is not answerable until it has been on screen for
        // `CONFIRM_ARM_DELAY`; see the constant for why.
        self.confirm_armed_at = Some(Instant::now());
    }

    /// `y` inside the confirm modal. The ONLY caller of `agents::stop`.
    pub fn act_confirm_stop(&mut self) {
        let Mode::Confirm(Confirm::StopSession {
            short_id, name, ..
        }) = self.mode.clone()
        else {
            return;
        };
        self.mode = Mode::Normal;
        self.confirm_armed_at = None;

        // Fail closed: re-validate the CAPTURED id against the current poll.
        let still_there = self
            .sessions
            .iter()
            .any(|s| s.id.as_deref() == Some(short_id.as_str()));
        if !still_there {
            self.flash(
                format!("session {short_id} is gone — not stopped"),
                MsgLevel::Warn,
            );
            return;
        }
        match agents::stop(&short_id) {
            Ok(()) => {
                // The pane, if any, is left open — closing it is a separate `x`.
                self.flash(format!("stopped {name}"), MsgLevel::Info);
                self.act_force_refresh();
            }
            Err(e) => self.flash(format!("stop failed: {}", agents_msg(&e)), MsgLevel::Error),
        }
    }

    /// `L` — the on-demand, ANSI-stripped logs overlay. SPEC §6.7.
    pub fn act_open_logs(&mut self) {
        let Some(sel) = self.selected_session() else {
            return;
        };
        let Some(id) = sel.id.clone() else {
            self.flash("no logs for an interactive session", MsgLevel::Warn);
            return;
        };
        let title = sel.name.clone();
        match agents::logs(&id, LOGS_LINES) {
            Ok(text) => {
                self.logs = Some(LogsView {
                    title,
                    lines: text.lines().map(str::to_string).collect(),
                    scroll: 0,
                });
                self.mode = Mode::Logs;
            }
            Err(e) => self.flash(format!("logs failed: {}", agents_msg(&e)), MsgLevel::Error),
        }
    }

    /// `r` — SPEC §4.2: backdate `last_poll` so the NEXT loop iteration polls.
    /// Deliberately does not call `tick()` inline.
    pub fn act_force_refresh(&mut self) {
        self.last_poll = Instant::now()
            .checked_sub(self.effective_interval())
            .unwrap_or_else(Instant::now);
    }

    /// `Enter` inside a prompt. SPEC §8.6 / §8.7.
    pub fn act_submit_prompt(&mut self) {
        let Some(p) = self.prompt.clone() else {
            self.mode = Mode::Normal;
            return;
        };
        let cwd = p.fields.first().cloned().unwrap_or_default();

        match p.kind {
            PromptKind::NewBackground => {
                let task = p.fields.get(1).cloned().unwrap_or_default();
                let task = task.trim().to_string();
                if task.is_empty() {
                    self.flash("task cannot be empty", MsgLevel::Warn);
                    return; // stay in the prompt
                }
                if !Path::new(&cwd).is_dir() {
                    self.flash(format!("no such directory: {cwd}"), MsgLevel::Warn);
                    return;
                }
                // RULE Q4: pure argv, the task text never touches a shell.
                match agents::dispatch_background(&cwd, &task) {
                    Ok(()) => {
                        self.prompt = None;
                        self.mode = Mode::Normal;
                        self.flash("dispatched background session", MsgLevel::Info);
                        // ccmux does NOT auto-open it — the operator decides.
                        self.act_force_refresh();
                    }
                    Err(e) => {
                        self.flash(format!("dispatch failed: {}", agents_msg(&e)), MsgLevel::Error)
                    }
                }
            }
            PromptKind::NewInteractive => {
                if self.degraded {
                    self.flash("not inside tmux — new session unavailable", MsgLevel::Warn);
                    return;
                }
                if !Path::new(&cwd).is_dir() {
                    self.flash(format!("no such directory: {cwd}"), MsgLevel::Warn);
                    return;
                }
                let Some(anchor) = self.split_anchor() else {
                    self.flash("no pane to split — sidebar unmapped", MsgLevel::Error);
                    return;
                };
                let cmd = agents::interactive_pane_cmd(&cwd);
                match tmux::split(&self.tmux_session, &anchor, SplitDir::Vertical, &cmd) {
                    Ok(_pane) => {
                        // §8.7: NO `@ccmux_map` entry — there is no session_id
                        // yet. tick() step 5's /proc walk binds it next poll.
                        self.refresh_panes();
                        self.pin_sidebar();
                        self.prompt = None;
                        self.mode = Mode::Normal;
                        let shown = model::shorten_cwd(&cwd, self.home.as_deref(), 24);
                        self.flash(
                            format!("started interactive session in {shown}"),
                            MsgLevel::Info,
                        );
                    }
                    Err(e) => {
                        self.flash(format!("split failed: {}", tmux_msg(&e)), MsgLevel::Error)
                    }
                }
            }
        }
    }

    // ── messaging ────────────────────────────────────────────────────────────

    /// Shows `text` in the footer for 4s.
    pub fn flash(&mut self, text: impl Into<String>, level: MsgLevel) {
        self.message = Some((text.into(), level));
        self.msg_deadline = Instant::now().checked_add(MSG_TTL);
    }

    // ────────────────────────────────────────────────────────────────────────
    // private helpers
    // ────────────────────────────────────────────────────────────────────────

    fn is_session_row(&self, i: usize) -> bool {
        matches!(self.rows.get(i), Some(Row::Session { .. }))
    }

    fn key_at(&self, i: usize) -> Option<String> {
        match self.rows.get(i) {
            Some(Row::Session { idx }) => self.sessions.get(*idx).map(|s| s.session_id.clone()),
            _ => None,
        }
    }

    fn set_selected(&mut self, i: usize) {
        self.selected = i;
        self.selected_key = self.key_at(i);
        self.clamp_scroll();
    }

    fn rebuild_rows(&mut self) {
        self.rows = model::build_rows(&self.sessions, &self.filter, self.show_completed);
        self.reanchor_selection();
    }

    /// SPEC §5.3 steps 1-5, also run after every ccmux-issued split/kill so the
    /// fresh `#{pane_index}` is available for the confirmation message.
    fn refresh_panes(&mut self) {
        if self.degraded {
            return;
        }
        if let Ok(live) = tmux::list_panes_in_session(&self.tmux_session) {
            if self.map.reconcile(&live) {
                self.map_dirty = true;
            }
            self.panes = live;
            self.resolve_sidebar_pane();
        }
    }

    /// §5.3 step 5: re-resolve `@ccmux_sidebar` when the cached id is not live.
    /// Staying `None` is never fatal — `pin_sidebar` no-ops and `split_anchor`
    /// falls back to `leftmost_pane`.
    fn resolve_sidebar_pane(&mut self) {
        let live = self
            .sidebar_pane
            .as_ref()
            .is_some_and(|p| self.panes.iter().any(|i| &i.id == p));
        if live {
            return;
        }
        let panes = &self.panes;
        self.sidebar_pane = tmux::get_user_option(&self.tmux_session, tmux::OPT_SIDEBAR)
            .and_then(|s| PaneId::parse(&s))
            .filter(|p| panes.iter().any(|i| &i.id == p));
    }

    /// Width to pin to, or `None` when pinning would starve the Claude panes.
    ///
    /// SPEC AMENDMENT (§1.3, §9.8): the spec assumed "tmux clamps resize-pane".
    /// It does not — tmux honours the request and takes the columns from the
    /// other panes, so on a 30-column window the unconditional pin squeezed a
    /// Claude pane to ONE column and re-squeezed it every tick. The pin is now
    /// bounded by the window and skipped while the sidebar is alone.
    ///
    /// `@ccmux_width` is the source of truth (§1.2 step 5): a relaunch with a
    /// new `--width` writes it, and this reads it, so the already-running
    /// sidebar stops reverting the launcher's resize on the next tick.
    /// Pure geometry half of `pinned_width`, so the clamp is testable without
    /// tmux. `want` is the requested width.
    fn pinned_width_from(&self, want: u16) -> Option<u16> {
        let sb = self.sidebar_pane.as_ref()?;
        let win = tmux::window_of(&self.panes, sb)?;
        let scoped = tmux::panes_in_window(&self.panes, win);
        // Alone in the window: `resize-pane` is a no-op there anyway, and the
        // next split re-pins immediately.
        if scoped.len() < 2 {
            return None;
        }
        let window_cols = scoped
            .iter()
            .map(|p| p.left.saturating_add(p.width))
            .max()
            .unwrap_or(0);
        let room = window_cols.saturating_sub(MIN_CONTENT_COLS);
        if room == 0 {
            return None;
        }
        Some(want.min(room).max(1))
    }

    fn pinned_width(&self) -> Option<u16> {
        self.pinned_width_from(self.requested_width())
    }

    /// `@ccmux_width` when set and sane, else this process's `--width`.
    fn requested_width(&self) -> u16 {
        tmux::get_user_option(&self.tmux_session, tmux::OPT_WIDTH)
            .and_then(|s| s.trim().parse::<u16>().ok())
            .map(|w| w.clamp(crate::WIDTH_MIN, crate::WIDTH_MAX))
            .unwrap_or(self.sidebar_width)
    }

    fn pin_sidebar(&self) {
        if self.degraded {
            return;
        }
        let (Some(sb), Some(cols)) = (&self.sidebar_pane, self.pinned_width()) else {
            return;
        };
        tmux::pin_sidebar(&self.tmux_session, sb, cols);
    }

    /// Flush `@ccmux_map`.
    ///
    /// `map_dirty` is cleared on FAILURE too: tmux refuses a `set-option` value
    /// over ~16 KB, and a doomed write left dirty would be reissued every tick
    /// forever with nothing on screen to say the map had stopped persisting.
    /// The next map change retries once, and the operator is told.
    fn save_map_now(&mut self) {
        if self.degraded || !self.map_dirty {
            return;
        }
        match tmux::save_map(&self.tmux_session, &self.map) {
            Ok(()) => self.map_dirty = false,
            Err(e) => {
                self.map_dirty = false;
                self.flash(format!("pane map not saved: {}", tmux_msg(&e)), MsgLevel::Warn);
            }
        }
    }

    /// `#{window_index}` of the window the sidebar lives in — the only window
    /// ccmux may lay out. Falls back to the lowest window index present.
    fn sidebar_window(&self) -> Option<u32> {
        self.sidebar_pane
            .as_ref()
            .and_then(|sb| tmux::window_of(&self.panes, sb))
            .or_else(|| tmux::lowest_window(&self.panes))
    }

    /// SPEC §8.4 — deterministic, so two engineers cannot disagree.
    ///   1. the active pane, if it is not the sidebar
    ///   2. else the rightmost non-sidebar pane
    ///   3. else the sidebar itself (first split: it is the only pane, and the
    ///      caller's re-pin immediately restores its width)
    ///
    /// SPEC AMENDMENT (§8.4): all three steps run over the SIDEBAR'S WINDOW
    /// only. `self.panes` is session-scoped because `PaneMap::reconcile` needs
    /// every window, but `pane_left`, `pane_index` and `pane_active` are
    /// per-window values: unfiltered, step 1 matches the active pane of some
    /// other window and step 2's `max_by_key` happily returns a pane the
    /// operator cannot see, so `o`/`s`/`Enter` opens Claude into a window they
    /// are not looking at.
    fn split_anchor(&self) -> Option<PaneId> {
        let scoped = match self.sidebar_window() {
            Some(win) => tmux::panes_in_window(&self.panes, win),
            None => return None,
        };
        let sidebar = self.sidebar_pane.as_ref();
        if let Some(active) = scoped.iter().find(|p| p.active)
            && Some(&active.id) != sidebar {
                return Some(active.id.clone());
            }
        match sidebar.filter(|sb| scoped.iter().any(|p| &p.id == *sb)) {
            Some(sb) => tmux::rightmost_pane_excluding(&scoped, sb).or_else(|| Some(sb.clone())),
            // §9.8: `@ccmux_sidebar` unresolvable — fall back to the leftmost
            // pane OF THAT WINDOW.
            None => tmux::leftmost_pane(&scoped),
        }
    }

    fn jump_to_ccmux_pane(&mut self, pane: &PaneId) {
        match tmux::select_pane(&self.tmux_session, pane) {
            Ok(()) => match self.pane_index_of(pane) {
                Some(i) => self.flash(format!("jumped to pane {i}"), MsgLevel::Info),
                None => self.flash(format!("jumped to pane {pane}"), MsgLevel::Info),
            },
            Err(e) => self.flash(format!("jump failed: {}", tmux_msg(&e)), MsgLevel::Error),
        }
    }

    /// SPEC §5.4 / §8.3 interactive branch. First pass is the per-tick
    /// `interactive_panes` cache; the second, server-wide READ-ONLY pass runs
    /// only here, only on an explicit jump key (R3).
    fn act_jump_interactive(&mut self) {
        if self.degraded {
            self.flash("not inside tmux — jump unavailable", MsgLevel::Warn);
            return;
        }
        let Some(sel) = self.selected_session() else {
            return;
        };
        let session_id = sel.session_id.clone();
        let pid = sel.pid;

        if let Some(pane) = self.interactive_panes.get(&session_id).cloned() {
            self.jump_to_ccmux_pane(&pane);
            return;
        }
        let Ok(all) = tmux::list_panes_all() else {
            self.flash("interactive session is not in a tmux pane", MsgLevel::Warn);
            return;
        };
        let Some(info) = tmux::resolve_pane_for_pid(pid, &all) else {
            self.flash("interactive session is not in a tmux pane", MsgLevel::Warn);
            return;
        };
        // The one focus move permitted outside `cli.session`: it creates,
        // kills and resizes nothing.
        match tmux::focus_foreign_pane(&info.session_name, &info.id) {
            Ok(()) => self.flash(
                format!(
                    "jumped to {}:{}.{} (outside ccmux)",
                    info.session_name, info.window_index, info.index
                ),
                MsgLevel::Info,
            ),
            Err(e) => self.flash(format!("jump failed: {}", tmux_msg(&e)), MsgLevel::Error),
        }
    }

    fn open_prompt(&mut self, kind: PromptKind) {
        let cwd = self
            .selected_session()
            .map(|s| s.cwd.clone())
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let fields = match kind {
            PromptKind::NewBackground => vec![cwd, String::new()],
            PromptKind::NewInteractive => vec![cwd],
        };
        // §8.6: the task field starts empty and is focused.
        let focus = match kind {
            PromptKind::NewBackground => 1,
            PromptKind::NewInteractive => 0,
        };
        let cursor = fields.get(focus).map(|s| s.chars().count()).unwrap_or(0);
        self.prompt = Some(Prompt {
            kind,
            fields,
            focus,
            cursor,
        });
        self.mode = Mode::Prompt(kind);
    }

    // ── per-mode key handlers ────────────────────────────────────────────────

    fn key_normal(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // NOTE: uppercase chars arrive with SHIFT set, so char bindings must NOT
        // require empty modifiers. Only Ctrl-bindings inspect modifiers.
        match key.code {
            KeyCode::Char('d') if ctrl => {
                self.select_half_page(true);
                Action::Redraw
            }
            KeyCode::Char('u') if ctrl => {
                self.select_half_page(false);
                Action::Redraw
            }
            _ if ctrl => Action::None,

            KeyCode::Char('j') | KeyCode::Down => {
                self.select_next();
                Action::Redraw
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.select_prev();
                Action::Redraw
            }
            // `gg` also works: the second `g` is idempotent.
            KeyCode::Char('g') => {
                self.select_first();
                Action::Redraw
            }
            KeyCode::Char('G') => {
                self.select_last();
                Action::Redraw
            }
            KeyCode::Tab => {
                self.cycle_group(true);
                Action::Redraw
            }
            KeyCode::BackTab => {
                self.cycle_group(false);
                Action::Redraw
            }

            KeyCode::Enter => {
                self.act_enter();
                Action::Redraw
            }
            // vim geometry: `o` = :vsplit = side by side = tmux -h.
            KeyCode::Char('o') => {
                self.act_open(SplitDir::Vertical);
                Action::Redraw
            }
            // `s` = :split = stacked = tmux -v.
            KeyCode::Char('s') => {
                self.act_open(SplitDir::Horizontal);
                Action::Redraw
            }
            KeyCode::Char('x') => {
                self.act_close_pane();
                Action::Redraw
            }
            KeyCode::Char('S') => {
                self.act_request_stop();
                Action::Redraw
            }
            KeyCode::Char('n') => {
                self.open_prompt(PromptKind::NewBackground);
                Action::Redraw
            }
            KeyCode::Char('c') => {
                if self.degraded {
                    self.flash("not inside tmux — new session unavailable", MsgLevel::Warn);
                } else {
                    self.open_prompt(PromptKind::NewInteractive);
                }
                Action::Redraw
            }
            KeyCode::Char('L') => {
                self.act_open_logs();
                Action::Redraw
            }

            KeyCode::Char('/') => {
                // The existing filter is preserved, so `/` then Enter re-opens it.
                self.mode = Mode::Filter;
                Action::Redraw
            }
            KeyCode::Char('a') => {
                self.show_completed = !self.show_completed;
                self.rebuild_rows();
                Action::Redraw
            }
            KeyCode::Char('r') => {
                self.act_force_refresh();
                Action::Redraw
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                self.help_scroll = 0;
                Action::Redraw
            }
            KeyCode::Char('q') => {
                self.should_quit = true;
                Action::Quit
            }
            // SPEC AMENDMENT (§8.8): Esc clears an active filter and is
            // otherwise inert. It used to quit, which is the opposite of what
            // Esc means in a neovim-style app and of what this app's own
            // overlays do — one reflex keypress tore the explorer out of the
            // window. `q` and `Ctrl-c` remain the quit keys.
            KeyCode::Esc => {
                if self.filter.is_empty() {
                    self.flash("press q to quit", MsgLevel::Info);
                } else {
                    self.filter.clear();
                    self.rebuild_rows();
                }
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    fn key_filter(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => {
                // Commit: the filter stays active, the header shows `4/7`.
                self.mode = Mode::Normal;
                Action::Redraw
            }
            KeyCode::Esc => {
                self.filter.clear();
                self.rebuild_rows();
                self.mode = Mode::Normal;
                Action::Redraw
            }
            KeyCode::Char('u') if ctrl => {
                self.filter.clear();
                self.rebuild_rows();
                Action::Redraw
            }
            KeyCode::Char('w') if ctrl => {
                let trimmed = self.filter.trim_end();
                let cut = trimmed
                    .char_indices()
                    .rev()
                    .find(|(_, c)| c.is_whitespace())
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                self.filter.truncate(cut);
                self.rebuild_rows();
                Action::Redraw
            }
            _ if ctrl => Action::None,
            KeyCode::Backspace => {
                self.filter.pop();
                self.rebuild_rows();
                Action::Redraw
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.rebuild_rows();
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    /// §8.2: NO default-affirmative. Only the literal lowercase `y` confirms;
    /// `Enter`, `n`, `Esc`, `q` and everything else cancel with no side effect.
    ///
    /// SPEC AMENDMENT (§8.2): a `y` that arrives within `CONFIRM_ARM_DELAY` of
    /// the modal opening is treated as TYPE-AHEAD and cancels. It cannot be an
    /// answer to a question the operator has not seen yet, and the input it
    /// most likely came from — a paste, or "Sync" typed without `/` — would
    /// otherwise stop a running agent.
    fn key_confirm(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('y') && key.modifiers.is_empty() {
            let armed = self
                .confirm_armed_at
                .is_some_and(|t| t.elapsed() >= CONFIRM_ARM_DELAY);
            if armed {
                self.act_confirm_stop();
            } else {
                self.mode = Mode::Normal;
                self.confirm_armed_at = None;
                self.flash("ignored buffered 'y' — press S again", MsgLevel::Warn);
            }
        } else {
            self.mode = Mode::Normal;
            self.confirm_armed_at = None;
        }
        Action::Redraw
    }

    fn key_prompt(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.prompt = None;
                self.mode = Mode::Normal;
                return Action::Redraw;
            }
            KeyCode::Enter => {
                self.act_submit_prompt();
                return Action::Redraw;
            }
            _ => {}
        }

        let Some(mut p) = self.prompt.clone() else {
            self.mode = Mode::Normal;
            return Action::Redraw;
        };
        let nfields = p.fields.len().max(1);
        let len = |p: &Prompt| -> usize {
            p.fields.get(p.focus).map(|s| s.chars().count()).unwrap_or(0)
        };

        match key.code {
            KeyCode::Tab => {
                p.focus = (p.focus + 1) % nfields;
                p.cursor = len(&p);
            }
            KeyCode::BackTab => {
                p.focus = (p.focus + nfields - 1) % nfields;
                p.cursor = len(&p);
            }
            KeyCode::Left => p.cursor = p.cursor.saturating_sub(1),
            KeyCode::Right => p.cursor = (p.cursor + 1).min(len(&p)),
            KeyCode::Home => p.cursor = 0,
            KeyCode::End => p.cursor = len(&p),
            KeyCode::Backspace => {
                if p.cursor > 0 {
                    let at = p.cursor - 1;
                    if let Some(f) = p.fields.get_mut(p.focus) {
                        remove_char_at(f, at);
                    }
                    p.cursor = at;
                }
            }
            KeyCode::Delete => {
                let at = p.cursor;
                if let Some(f) = p.fields.get_mut(p.focus) {
                    remove_char_at(f, at);
                }
            }
            KeyCode::Char(c) if !ctrl => {
                let at = p.cursor;
                if let Some(f) = p.fields.get_mut(p.focus) {
                    insert_char_at(f, at, c);
                }
                p.cursor = at + 1;
            }
            _ => return Action::None,
        }
        self.prompt = Some(p);
        Action::Redraw
    }

    /// Rows an overlay body shows, never 0. `main.rs` writes
    /// `overlay_viewport`; a 0 means it has not drawn a frame yet.
    fn overlay_page(&self) -> usize {
        (self.overlay_viewport as usize).max(1)
    }

    /// Largest scroll offset that still shows content, for a `len`-line overlay.
    ///
    /// SPEC AMENDMENT (§8.9): the counter used to run to a fixed 64 (help) or
    /// `len - 1` (logs) while the renderers clamp to `len - visible`. `G` then
    /// parked the counter tens of steps past the real bottom and `k` looked
    /// dead for exactly that many presses.
    fn overlay_scroll_max(&self, len: usize) -> usize {
        len.saturating_sub(self.overlay_page())
    }

    /// §8.9: any key returns to Normal, except the scroll keys, which move
    /// `help_scroll`.
    fn key_help(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.overlay_page();
        let max = self.overlay_scroll_max(self.help_lines);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.help_scroll = (self.help_scroll + 1).min(max);
                Action::Redraw
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
                Action::Redraw
            }
            KeyCode::Char('d') if ctrl => {
                self.help_scroll = (self.help_scroll + page).min(max);
                Action::Redraw
            }
            KeyCode::Char('u') if ctrl => {
                self.help_scroll = self.help_scroll.saturating_sub(page);
                Action::Redraw
            }
            KeyCode::Char('g') => {
                self.help_scroll = 0;
                Action::Redraw
            }
            KeyCode::Char('G') => {
                self.help_scroll = max;
                Action::Redraw
            }
            _ => {
                self.mode = Mode::Normal;
                Action::Redraw
            }
        }
    }

    fn key_logs(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(mut v) = self.logs.clone() else {
            self.mode = Mode::Normal;
            return Action::Redraw;
        };
        let max = self.overlay_scroll_max(v.lines.len());
        let page = (self.overlay_page() / 2).max(1);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.logs = None;
                self.mode = Mode::Normal;
                return Action::Redraw;
            }
            KeyCode::Char('d') if ctrl => v.scroll = (v.scroll + page).min(max),
            KeyCode::Char('u') if ctrl => v.scroll = v.scroll.saturating_sub(page),
            _ if ctrl => return Action::None,
            KeyCode::Char('j') | KeyCode::Down => v.scroll = (v.scroll + 1).min(max),
            KeyCode::Char('k') | KeyCode::Up => v.scroll = v.scroll.saturating_sub(1),
            KeyCode::Char('g') => v.scroll = 0,
            KeyCode::Char('G') => v.scroll = max,
            _ => return Action::None,
        }
        self.logs = Some(v);
        Action::Redraw
    }
}

// ── free helpers ────────────────────────────────────────────────────────────

/// Char-index insert. Byte offsets are derived from `char_indices`, so a CJK or
/// emoji cwd cannot split a code point (§6.6's char-based rule, applied to input).
fn insert_char_at(s: &mut String, char_idx: usize, c: char) {
    let at = byte_of(s, char_idx);
    s.insert(at, c);
}

/// Char-index delete. A `char_idx` past the end is a no-op.
fn remove_char_at(s: &mut String, char_idx: usize) {
    if let Some((at, c)) = s.char_indices().nth(char_idx) {
        let end = at + c.len_utf8();
        s.replace_range(at..end, "");
    }
}

fn byte_of(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

fn first_line_or(stderr: &str, fallback: String) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .unwrap_or(fallback)
}

/// Footer text for an `AgentsError`, per SPEC §9.1/§9.2/§9.8.
///
/// Deliberately matches on the variants rather than using `Display`: the wording
/// is spec'd per case ("claude not found on PATH", "bad json: <excerpt>") and
/// `app.rs` must not depend on another lane's `Display` formatting.
fn agents_msg(e: &AgentsError) -> String {
    match e {
        AgentsError::NotFound(_) => "claude not found on PATH".to_string(),
        AgentsError::Cmd { code, stderr } => first_line_or(stderr, format!("exit {code}")),
        AgentsError::Parse(ParseError::Json { excerpt, .. }) => format!("bad json: {excerpt}"),
        AgentsError::Parse(ParseError::NotAnArray) => "bad json: not an array".to_string(),
        AgentsError::NotAttachable => "session has no id".to_string(),
    }
}

/// Footer text for a `TmuxError`. Same rationale as `agents_msg`.
fn tmux_msg(e: &TmuxError) -> String {
    match e {
        TmuxError::NotFound(m) => format!("tmux not found: {m}"),
        TmuxError::Cmd { code, stderr, .. } => first_line_or(stderr, format!("exit {code}")),
        TmuxError::BadTarget(t) => format!("bad target: {t}"),
        TmuxError::Parse(m) => format!("bad tmux output: {m}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, State, Status};

    /// Builds an `App` WITHOUT touching tmux. `App::new` probes `$TMUX` and
    /// calls `PaneMap::new`; these tests must run on a machine with live agent
    /// sessions and must never shell out, so they construct the struct directly.
    fn app() -> App {
        App {
            tmux_session: "ccmux-test".into(),
            sidebar_width: 34,
            interval: Duration::from_millis(2500),
            dark: true,
            home: Some("/home/dev".into()),
            sessions: Vec::new(),
            rows: Vec::new(),
            now_ms: 1_787_640_000_000,
            selected: 0,
            selected_key: None,
            scroll: 0,
            viewport: 20,
            help_scroll: 0,
            overlay_viewport: 20,
            help_lines: 21,
            filter: String::new(),
            show_completed: true,
            mode: Mode::Normal,
            prompt: None,
            logs: None,
            map: PaneMap::default(),
            map_dirty: false,
            interactive_panes: std::collections::BTreeMap::new(),
            sidebar_pane: None,
            panes: Vec::new(),
            // NOT degraded: the gates under test must fire on their own merits,
            // not because the degraded check short-circuited them.
            degraded: false,
            // Armed in the past: the tests answer the modal instantly, which a
            // human cannot, and `CONFIRM_ARM_DELAY` exists to reject exactly
            // that. `confirm_gate_ignores_type_ahead` covers the delay itself.
            confirm_armed_at: Instant::now().checked_sub(Duration::from_secs(1)),
            message: None,
            msg_deadline: None,
            poll_error: None,
            fail_streak: 0,
            last_poll: Instant::now(),
            should_quit: false,
        }
    }

    fn bg(short: &str, name: &str, state: State) -> Session {
        Session {
            pid: 0,
            id: Some(short.to_string()),
            session_id: format!("{short}-uuid"),
            cwd: "/home/dev/projects".into(),
            kind: Kind::Background,
            started_at: 1_787_600_000_000,
            name: name.to_string(),
            status: Status::Busy,
            state: Some(state),
        }
    }

    fn inter(uuid: &str, name: &str) -> Session {
        Session {
            pid: 4242,
            id: None,
            session_id: uuid.to_string(),
            cwd: "/home/dev/projects".into(),
            kind: Kind::Interactive,
            started_at: 1_787_610_000_000,
            name: name.to_string(),
            status: Status::Busy,
            state: None,
        }
    }

    fn press(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn load(app: &mut App, sessions: Vec<Session>) {
        app.sessions = sessions;
        app.rebuild_rows();
        app.select_first();
    }

    fn pane(id: &str, window: u32, index: u32, left: u16, width: u16, active: bool) -> PaneInfo {
        PaneInfo {
            id: PaneId::parse(id).expect("pane id"),
            pid: 0,
            index,
            left,
            top: 0,
            width,
            height: 40,
            active,
            session_name: "ccmux-test".into(),
            window_index: window,
        }
    }

    #[test]
    fn confirm_gate_ignores_type_ahead() {
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);

        // `S` immediately followed by a `y` that was already in the tty buffer.
        a.on_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        assert!(matches!(a.mode, Mode::Confirm(_)));
        let act = a.on_key(press('y'));

        assert_eq!(act, Action::Redraw);
        assert_eq!(a.mode, Mode::Normal, "the modal must close");
        assert!(a.confirm_armed_at.is_none());
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Warn);
        assert_eq!(text, "ignored buffered 'y' — press S again");

        // The session is untouched and still selectable.
        assert_eq!(a.sessions.len(), 1);

        // Once the modal has been on screen long enough, `y` is honoured. The
        // session is gone from the poll, so the fail-closed path proves the
        // gate was passed without `claude stop` ever being spawned.
        a.on_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        a.confirm_armed_at = Instant::now().checked_sub(Duration::from_secs(1));
        a.sessions.clear();
        a.rebuild_rows();
        a.on_key(press('y'));
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "session 1c45d64f is gone — not stopped");
    }

    #[test]
    fn confirm_cancel_keys_disarm_the_modal() {
        for key in [press('n'), press('q'), KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)] {
            let mut a = app();
            load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);
            a.on_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
            a.on_key(key);
            assert_eq!(a.mode, Mode::Normal);
            assert!(a.confirm_armed_at.is_none(), "cancel must disarm");
        }
    }

    #[test]
    fn paste_is_never_executed_as_a_keymap() {
        let mut a = app();
        load(
            &mut a,
            vec![
                bg("1c45d64f", "bt/reg-update", State::Working),
                bg("629da7fc", "kernel bugs", State::Working),
            ],
        );

        // The exact text that used to run S, y, n and a dispatch.
        let act = a.on_paste("Sync branch\n");
        assert_eq!(act, Action::None);
        assert_eq!(a.mode, Mode::Normal, "Normal mode must discard a paste");
        assert!(a.filter.is_empty());
        assert!(a.prompt.is_none());
        assert!(a.message.is_none());

        // Filter mode takes it as text, minus the control characters.
        a.on_key(press('/'));
        assert_eq!(a.on_paste("kernel\nbugs"), Action::Redraw);
        assert_eq!(a.filter, "kernelbugs");
    }

    #[test]
    fn paste_into_a_prompt_is_literal_text() {
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);
        a.on_key(press('n'));
        assert_eq!(a.mode, Mode::Prompt(PromptKind::NewBackground));
        assert_eq!(a.on_paste("重构 pipeline\r\n"), Action::Redraw);
        let p = a.prompt.clone().expect("prompt");
        assert_eq!(p.fields.get(1).map(String::as_str), Some("重构 pipeline"));
        // char-indexed, so the two wide chars count once each
        assert_eq!(p.cursor, 11);
    }

    #[test]
    fn filter_mode_ignores_unhandled_ctrl_chords() {
        // `key_filter`'s `_ if ctrl` arm sits BEFORE its printable-char arm, so
        // Ctrl-a/Ctrl-e/Ctrl-l cannot leak their bare letter into the needle.
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);
        a.on_key(press('/'));
        for c in ['a', 'e', 'l', 'k', 'r'] {
            assert_eq!(
                a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
                Action::None
            );
        }
        assert_eq!(a.filter, "");

        // The two chords that ARE bound still work.
        for c in "af/reg".chars() {
            a.on_key(press(c));
        }
        a.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(a.filter, "");
        for c in "af reg".chars() {
            a.on_key(press(c));
        }
        a.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(a.filter, "af ");
        a.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(a.filter, "");
    }

    #[test]
    fn split_anchor_never_leaves_the_sidebars_window() {
        let mut a = app();
        a.sidebar_pane = PaneId::parse("%1");
        // Window 1 is the ccmux layout; window 2 is a window the operator made
        // themselves, whose panes have their own `pane_left` and `pane_active`.
        a.panes = vec![
            pane("%1", 1, 1, 0, 34, false),
            pane("%10", 1, 2, 35, 65, false),
            pane("%11", 2, 1, 0, 10, false),
            pane("%12", 2, 2, 41, 59, true),
        ];
        assert_eq!(a.split_anchor(), PaneId::parse("%10"));

        // The active pane of the ccmux window wins, and only that one.
        a.panes[1].active = true;
        assert_eq!(a.split_anchor(), PaneId::parse("%10"));

        // Sidebar alone in its window: it anchors its own first split.
        a.panes = vec![pane("%1", 1, 1, 0, 100, true), pane("%12", 2, 1, 0, 100, true)];
        assert_eq!(a.split_anchor(), PaneId::parse("%1"));

        // Sidebar unresolvable: the leftmost pane of the lowest window, never a
        // pane from some other window.
        a.sidebar_pane = None;
        a.panes = vec![
            pane("%9", 3, 1, 0, 100, false),
            pane("%10", 1, 2, 35, 65, false),
            pane("%11", 1, 1, 0, 34, false),
        ];
        assert_eq!(a.split_anchor(), PaneId::parse("%11"));

        // No panes at all: nothing to anchor, and nothing is split.
        a.panes.clear();
        assert_eq!(a.split_anchor(), None);
    }

    #[test]
    fn pinned_width_never_starves_the_content_panes() {
        let mut a = app();
        a.sidebar_pane = PaneId::parse("%1");

        // Roomy window: the request is honoured.
        a.panes = vec![pane("%1", 1, 1, 0, 34, false), pane("%2", 1, 2, 35, 165, true)];
        assert_eq!(a.pinned_width_from(34), Some(34));
        assert_eq!(a.pinned_width_from(60), Some(60));

        // 30-column window: pinning 34 left the Claude pane at ONE column.
        a.panes = vec![pane("%1", 1, 1, 0, 19, false), pane("%2", 1, 2, 20, 10, true)];
        assert_eq!(a.pinned_width_from(34), Some(10));

        // Narrower than the content floor: do not resize at all.
        a.panes = vec![pane("%1", 1, 1, 0, 12, false), pane("%2", 1, 2, 13, 7, true)];
        assert_eq!(a.pinned_width_from(34), None);

        // Alone in its window: nothing to take columns from.
        a.panes = vec![pane("%1", 1, 1, 0, 200, true), pane("%9", 2, 1, 0, 200, false)];
        assert_eq!(a.pinned_width_from(34), None);

        // Unresolved sidebar: no pin.
        a.sidebar_pane = None;
        assert_eq!(a.pinned_width_from(34), None);
    }

    #[test]
    fn logs_scroll_stops_where_the_view_stops() {
        let mut a = app();
        a.overlay_viewport = 28;
        a.logs = Some(LogsView {
            title: "bt/reg-update".into(),
            lines: (0..60).map(|i| format!("log line {i}")).collect(),
            scroll: 0,
        });
        a.mode = Mode::Logs;

        a.on_key(press('G'));
        assert_eq!(a.logs.as_ref().map(|l| l.scroll), Some(60 - 28));
        // One `k` must move the view, not burn off phantom scroll.
        a.on_key(press('k'));
        assert_eq!(a.logs.as_ref().map(|l| l.scroll), Some(60 - 29));

        // Held `j` cannot run past the last full page.
        for _ in 0..500 {
            a.on_key(press('j'));
        }
        assert_eq!(a.logs.as_ref().map(|l| l.scroll), Some(60 - 28));

        // A log shorter than the viewport does not scroll at all.
        a.logs = Some(LogsView {
            title: String::new(),
            lines: vec!["one".into(), "two".into()],
            scroll: 0,
        });
        a.on_key(press('j'));
        a.on_key(press('G'));
        assert_eq!(a.logs.as_ref().map(|l| l.scroll), Some(0));
    }

    #[test]
    fn help_overlay_scroll_is_its_own_field_and_is_capped() {
        let mut a = app();
        a.viewport = 10;
        a.overlay_viewport = 10;
        a.help_lines = 21;
        a.scroll = 7;

        assert_eq!(a.on_key(press('?')), Action::Redraw);
        assert_eq!(a.mode, Mode::Help);
        assert_eq!(a.help_scroll, 0, "`?` must open the overlay at the top");

        a.on_key(press('j'));
        a.on_key(press('j'));
        assert_eq!(a.help_scroll, 2);
        a.on_key(press('k'));
        assert_eq!(a.help_scroll, 1);
        // `k` at the top saturates rather than wrapping.
        a.on_key(press('k'));
        a.on_key(press('k'));
        assert_eq!(a.help_scroll, 0);

        a.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(a.help_scroll, 10);
        a.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(a.help_scroll, 0);

        // Held `j` stops at the last SCROLLABLE line, so one `k` moves the
        // view. A fixed cap instead left `k` dead for dozens of presses.
        for _ in 0..500 {
            a.on_key(press('j'));
        }
        assert_eq!(a.help_scroll, 21 - 10);
        a.on_key(press('G'));
        assert_eq!(a.help_scroll, 21 - 10);
        a.on_key(press('k'));
        assert_eq!(a.help_scroll, 21 - 11, "`k` after `G` must move the view");

        // The list scroll is untouched throughout; `clamp_scroll` owns it.
        assert_eq!(a.scroll, 7);
        assert_eq!(a.mode, Mode::Help, "scroll keys must not close the overlay");

        // Anything else returns to Normal.
        a.on_key(press('z'));
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn stop_on_interactive_row_warns_and_stays_normal() {
        let mut a = app();
        load(&mut a, vec![inter("uuid-i", "scratch")]);
        // Uppercase arrives with SHIFT set — the keymap must not require
        // empty modifiers for char bindings.
        let act = a.on_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        assert_eq!(act, Action::Redraw);
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(
            a.message.as_ref().map(|m| m.1),
            Some(MsgLevel::Warn),
            "S on an interactive row must warn, not open the modal"
        );
    }

    #[test]
    fn close_pane_on_interactive_row_issues_no_tmux_call() {
        let mut a = app();
        load(&mut a, vec![inter("uuid-i", "scratch")]);
        // SPEC §8.5 step 3. If the interactive branch is ever "simplified" back
        // into the background path, `pane_of` -> `kill_pane` would run against a
        // live tmux server here and destroy work. The absence of a map entry and
        // of any pane is what keeps this test honest: the refusal must come
        // BEFORE any lookup, and the flash must be the refusal wording.
        let act = a.on_key(press('x'));
        assert_eq!(act, Action::Redraw);
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Warn);
        assert!(
            text.starts_with("refusing:"),
            "expected the §8.5 refusal, got {text:?}"
        );
        assert!(a.map.panes.is_empty());
        assert!(!a.map_dirty, "a refused `x` must not dirty the map");
    }

    #[test]
    fn confirm_modal_uses_the_captured_short_id() {
        let mut a = app();
        load(
            &mut a,
            vec![bg("1c45d64f", "bt/reg-update", State::Working)],
        );
        a.on_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        match &a.mode {
            Mode::Confirm(Confirm::StopSession { short_id, name, .. }) => {
                assert_eq!(short_id, "1c45d64f");
                assert_eq!(name, "bt/reg-update");
            }
            other => panic!("expected the confirm modal, got {other:?}"),
        }

        // A poll lands between `S` and `y` and the session disappears. The
        // captured id must still be the one acted on, and the re-validation
        // must fail closed — so `claude stop` is never invoked.
        a.sessions = vec![bg("deadbeef", "something else", State::Working)];
        a.rebuild_rows();

        // Backdate the arming so this stands in for a human who read the modal;
        // the type-ahead window itself is covered separately.
        a.confirm_armed_at = Instant::now().checked_sub(Duration::from_secs(1));
        let act = a.on_key(press('y'));
        assert_eq!(act, Action::Redraw);
        assert_eq!(a.mode, Mode::Normal);
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Warn);
        assert_eq!(text, "session 1c45d64f is gone — not stopped");
    }

    #[test]
    fn any_key_but_y_cancels_the_confirm_modal() {
        let mut a = app();
        load(
            &mut a,
            vec![bg("1c45d64f", "bt/reg-update", State::Working)],
        );
        a.on_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        assert!(matches!(a.mode, Mode::Confirm(_)));

        // `n` is a Normal-mode binding; inside the modal it must only cancel.
        a.on_key(press('n'));
        assert_eq!(a.mode, Mode::Normal);
        assert!(a.prompt.is_none(), "`n` inside the modal must not open a prompt");
        assert!(a.message.is_none(), "cancelling has no side effect");

        // Enter is explicitly NOT a default-affirmative.
        a.on_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(a.mode, Mode::Normal);
        assert!(a.message.is_none());
    }

    #[test]
    fn pane_of_resolves_through_map_then_interactive_cache() {
        let mut a = app();
        let bg_pane = PaneId::parse("%25").expect("valid pane id");
        let int_pane = PaneId::parse("%26").expect("valid pane id");

        a.map.insert(
            &bg_pane,
            PaneEntry {
                session_id: "1c45d64f-uuid".into(),
                short_id: "1c45d64f".into(),
                name: "bt/reg-update".into(),
                opened_at: 0,
            },
        );
        a.interactive_panes
            .insert("uuid-i".into(), int_pane.clone());

        assert_eq!(a.pane_of("1c45d64f-uuid"), Some(bg_pane));
        assert_eq!(a.pane_of("uuid-i"), Some(int_pane));
        assert_eq!(a.pane_of("nobody"), None);
        assert!(!a.is_open("nobody"));
    }

    #[test]
    fn navigation_never_lands_on_a_header_and_survives_an_empty_list() {
        let mut a = app();
        let mut idle_interactive = inter("uuid-i", "three");
        idle_interactive.status = Status::Idle;
        load(
            &mut a,
            vec![
                bg("aaaaaaaa", "one", State::Working),
                bg("bbbbbbbb", "two", State::Done),
                idle_interactive,
            ],
        );
        // Working(one) + Idle(three) + Completed(two) => 3 headers, 3 sessions,
        // and 2 spacers (one before each group after the first).
        assert_eq!(a.rows.len(), 8);

        for _ in 0..12 {
            a.on_key(press('j'));
            assert!(
                matches!(a.rows.get(a.selected), Some(Row::Session { .. })),
                "j landed on row {:?}",
                a.rows.get(a.selected)
            );
        }
        for _ in 0..12 {
            a.on_key(press('k'));
            assert!(matches!(a.rows.get(a.selected), Some(Row::Session { .. })));
        }
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(a.rows.get(a.selected), Some(Row::Session { .. })));

        // Empty list: G, j, k, Ctrl-d must not panic and must not index rows.
        a.sessions.clear();
        a.rebuild_rows();
        assert_eq!(a.selected, a.rows.len());
        a.on_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        a.on_key(press('j'));
        a.on_key(press('k'));
        a.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(a.selected_session().is_none());
    }

    #[test]
    fn filter_edits_live_and_esc_clears_it() {
        let mut a = app();
        load(
            &mut a,
            vec![
                bg("aaaaaaaa", "bt/reg-update", State::Working),
                bg("bbbbbbbb", "kernel bugs", State::Working),
            ],
        );
        a.on_key(press('/'));
        assert_eq!(a.mode, Mode::Filter);
        for c in "kernel".chars() {
            a.on_key(press(c));
        }
        assert_eq!(a.filter, "kernel");
        assert_eq!(
            a.rows
                .iter()
                .filter(|r| matches!(r, Row::Session { .. }))
                .count(),
            1
        );
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(a.mode, Mode::Normal);

        // §8.8: Esc in Normal with a filter clears it.
        let act = a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(act, Action::Redraw);
        assert!(a.filter.is_empty());
        assert!(!a.should_quit);
        // AMENDED §8.8: with no filter, Esc is inert — it must NOT quit.
        assert_eq!(
            a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Redraw
        );
        assert!(!a.should_quit);
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn ctrl_c_quits_from_every_mode() {
        for mode in [
            Mode::Normal,
            Mode::Filter,
            Mode::Help,
            Mode::Logs,
            Mode::Prompt(PromptKind::NewBackground),
            Mode::Confirm(Confirm::StopSession {
                session_id: "x".into(),
                short_id: "x".into(),
                name: "x".into(),
            }),
        ] {
            let mut a = app();
            a.mode = mode;
            let act = a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert_eq!(act, Action::Quit);
            assert!(a.should_quit);
        }
    }

    #[test]
    fn prompt_editing_is_char_indexed() {
        let mut a = app();
        load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);
        a.on_key(press('n'));
        assert_eq!(a.mode, Mode::Prompt(PromptKind::NewBackground));

        // Multi-byte input must not split a code point.
        for c in "重构 α".chars() {
            a.on_key(press(c));
        }
        let p = a.prompt.clone().expect("prompt");
        assert_eq!(p.fields.get(1).map(String::as_str), Some("重构 α"));
        assert_eq!(p.cursor, 4);

        a.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        a.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        a.on_key(press('x'));
        let p = a.prompt.clone().expect("prompt");
        assert_eq!(p.fields.get(1).map(String::as_str), Some("x重构 "));

        // Tab moves to the cwd field, which was prefilled from the selection.
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let p = a.prompt.clone().expect("prompt");
        assert_eq!(p.focus, 0);
        assert_eq!(p.fields.first().map(String::as_str), Some("/home/dev/projects"));

        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(a.prompt.is_none());
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn empty_task_is_rejected_without_dispatching() {
        let mut a = app();
        load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);
        a.on_key(press('n'));
        // Enter with an empty task: must stay in the prompt and never reach
        // `agents::dispatch_background`.
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(a.mode, Mode::Prompt(PromptKind::NewBackground));
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Warn);
        assert_eq!(text, "task cannot be empty");
    }

    #[test]
    fn backoff_widens_the_interval_after_three_failures() {
        let mut a = app();
        assert_eq!(a.effective_interval(), Duration::from_millis(2500));
        a.fail_streak = 3;
        assert_eq!(a.effective_interval(), Duration::from_secs(10));
        a.fail_streak = 0;
        assert_eq!(a.effective_interval(), Duration::from_millis(2500));
    }

    /// Small ergonomic shim so assertions read cleanly without `unwrap`.
    trait MsgExt {
        fn unwrap_or_default_msg(self) -> (String, MsgLevel);
    }
    impl MsgExt for Option<(String, MsgLevel)> {
        fn unwrap_or_default_msg(self) -> (String, MsgLevel) {
            self.unwrap_or((String::new(), MsgLevel::Info))
        }
    }
}
