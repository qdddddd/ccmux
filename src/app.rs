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

use std::collections::{BTreeMap, BTreeSet};

use crate::agents::{self, AgentsError};
use crate::model::{self, Group, ParseError, Row, Session};
use crate::restart::{self, Role};
use crate::tmux::{
    self, HiddenLog, HiddenOp, HiddenSet, PaneEntry, PaneId, PaneInfo, PaneMap, SplitDir, TabInfo,
    TmuxError, WindowId,
};

/// Ceiling on `R`'s WHOLE agent pass, across every session in scope.
///
/// `agents::POLL_TIMEOUT` and `agents::STOP_TIMEOUT` bound one call each; this
/// bounds the sum, and it is the bound that actually matters, because the
/// per-call bounds MULTIPLY: seven in-scope agents against a wedged `claude`
/// would be seven poll timeouts plus seven stop timeouts — nearly two minutes
/// of a sidebar that accepts no keys. Sixty seconds is roughly ten times the
/// measured cost of the largest fleet observed in a ccmux session (7 in-pane
/// agents at ~0.87 s each), so it never fires in ordinary use; when it does,
/// the remaining agents are counted as not restarted and THE PANES STILL GET
/// RESPAWNED, because the pane pass is what `R` was always for and it must not
/// be lost to a wedged daemon.
///
/// ONE BUDGET FOR BOTH SUB-PASSES — the paned agents and the paneless ones.
/// Splitting it would let a long paned pass starve the half that has no pane
/// behind it, which is the half whose restart nothing else would ever make.
///
/// IT IS A START GATE, NOT A STOPWATCH, and the arithmetic is stated because
/// it was once stated wrong. The deadline is checked before each poll+act
/// group, and no call can be cut short once it has begun — `run_bounded` owns
/// their timeouts and takes no remaining-budget argument. So an agent whose
/// turn comes a millisecond before the deadline still gets its full
/// `POLL_TIMEOUT` (5 s) and then `STOP_TIMEOUT` (10 s) for a paned one, or
/// `RESPAWN_TIMEOUT` (10 s) plus the verifying `POLL_TIMEOUT` (5 s) for a
/// paneless one. The true worst case is therefore
///
///   AGENT_BUDGET + POLL_TIMEOUT + RESPAWN_TIMEOUT + POLL_TIMEOUT = 80 s
///
/// not 60. Tightening that to 60 would mean cancelling a `claude stop` in
/// flight, which is not a thing a caller can do and not a thing worth wanting:
/// the stop is the destructive half, and abandoning it mid-call would leave
/// exactly the ambiguity `Agents::stranded` exists to name. Abandoning a
/// paneless RESPAWN mid-call is worse still — that is the ambiguity actually
/// happening, and the verifying poll is what settles it. The 20 s of overshoot
/// is bounded, once per pass, and — since the
/// frame now comes first (`main::run_sidebar`) — spent on a drawn sidebar
/// whose buffered keys are discarded rather than on a blank one.
const AGENT_BUDGET: Duration = Duration::from_secs(60);

/// How long a flashed footer message stays up (SPEC §6.8 item 2).
const MSG_TTL: Duration = Duration::from_secs(4);
/// Backoff interval once `fail_streak` crosses `FAIL_BACKOFF_AT` (SPEC §4.2).
const BACKOFF: Duration = Duration::from_secs(10);
const FAIL_BACKOFF_AT: u32 = 3;
/// Consecutive byte-identical payloads before the idle ladder starts widening
/// the gap between `claude agents` spawns (SPEC §4.2).
///
/// Four, not one: a fleet that changed on the previous poll is a fleet worth
/// watching closely for a moment, and at the 2500 ms default this still means
/// ten seconds of a completely unchanging listing before anything slows down.
const IDLE_BACKOFF_AT: u32 = 4;
/// Ceiling on the idle ladder. Thirty seconds is the longest an operator who
/// is STARING at an unchanged sidebar can be shown stale data, and it is only
/// reachable after the listing has been identical for six polls running; any
/// keypress, any visibility change and any observed change collapse it back to
/// `interval` at once.
const IDLE_MAX: Duration = Duration::from_secs(30);
/// Lines requested from `claude logs` for the `L` overlay.
const LOGS_LINES: usize = 500;
/// SPEC §9.1: `poll_error` is truncated to 120 chars.
const POLL_ERR_MAX: usize = 120;
/// How long the second `Ctrl+X` has to arrive for it to mean DELETE
/// (SPEC §8.2). Agent view's own shortcut table words it "press again within
/// two seconds to delete it", and this is that two seconds.
const CX_WINDOW: Duration = Duration::from_secs(2);
/// Minimum gap between two `Ctrl+X` presses for the second one to count.
///
/// Moving the binding off `S` removed the paste and prose exposure — no run of
/// text can produce a Ctrl chord, and Normal mode discards pastes anyway — but
/// it did not remove the two ways the same chord can arrive without a human
/// meaning it twice: a burst the tty had already buffered while the UI was
/// blocked, and a held key auto-repeating. EVERY press stamps the clock — on
/// entry AND again after the shell-out it may have blocked in — so neither
/// stream can ever accumulate a gap: a burst of any length performs exactly one
/// stop, and a repeat stream performs exactly one stop no matter how long the
/// key is held.
///
/// It is 750 ms rather than a shorter bar because of the held key. With
/// `KeyEventKind::Press`-only events there is no release to observe,
/// so "press, wait 660 ms, press, release" and "hold for 665 ms" are the SAME
/// event stream — no settle can separate them after the fact, and at 250 ms
/// every stock auto-repeat delay (GNOME 500 ms, KDE 600 ms, X11 `xset` default
/// 660 ms) cleared the bar, so a hold released just after its first repeat
/// deleted a session. 750 ms clears all of them with margin. It also gives
/// the operator time to read what the press answers: 60 characters of "delete
/// <name> and its worktree — cannot be undone", which 250 ms never was.
/// A press inside the bar is not lost — it says so in the footer and the
/// window stays open (`act_ctrl_x`).
const CX_MIN_GAP: Duration = Duration::from_millis(750);
/// How long a qualifying second press waits before the delete actually runs.
///
/// `CX_MIN_GAP` alone cannot stop a held key whose auto-repeat DELAY was
/// configured above it, so the delete is also SETTLED: it is scheduled, and a
/// further `Ctrl+X` inside this window — the signature of a repeat stream,
/// whose next event is 25–40 ms away — cancels it. A deliberate double press
/// has no third event and the delete fires. The cost is one event-loop beat of
/// latency on the one action in ccmux that cannot be undone.
const CX_SETTLE: Duration = Duration::from_millis(180);
/// LOAD-BEARING, so it is pinned at compile time: the settle must be shorter
/// than the gap guard. That is what makes a settling delete and another
/// qualifying press mutually exclusive — any press early enough to catch a
/// pending delete is by definition inside `CX_MIN_GAP`, so it cancels rather
/// than schedules. Retuning either constant past the other would silently
/// reopen that overlap.
const _: () = assert!(CX_SETTLE.as_millis() < CX_MIN_GAP.as_millis());
/// Names stored in `@ccmux_map`, truncated. tmux rejects a `set-option` value
/// over ~16 KB (measured: ok at 16323 bytes, "command too long" at 16324), and
/// `name` is the only unbounded field in a `PaneEntry`.
const MAP_NAME_MAX: usize = 80;
/// Columns left for the Claude panes when the sidebar is pinned (§1.3).
const MIN_CONTENT_COLS: u16 = 20;
/// Columns a session name may take in a flashed message. The `d` flash has to
/// fit `hidden <name> — u to undo` into a 34-column footer, and the part that
/// must survive is the part that says how to get the row back.
const LABEL_MAX: usize = 14;
/// Columns a cwd may take in a §8.6 flash, on `LABEL_MAX`'s precedent: bound
/// the variable half so the actionable tail survives the default sidebar. The
/// widest fixed tail is ` does not exist — ⏎ again to create it` (~39 cols),
/// the overflow carve is at most 4 rows × 34 default columns, and a no-space
/// path token hard-wraps, so a path elided to 60 fills ≤ 2 rows and leaves the
/// tail its own ≤ 2. `shorten_cwd` keeps the FINAL components, so what the
/// flash names is the identifying part of the path — the same rule the detail
/// block renders every cwd under.
const CWD_FLASH_MAX: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgLevel {
    Info,
    Warn,
    Error,
}

/// The session the FIRST `Ctrl+X` press acted on, and when the window opened.
///
/// Every field is CAPTURED at that first press. The second press hands
/// `short_id` to `claude rm` and never re-reads the cursor, so a poll that
/// re-sorts the list — or a row that slides under the cursor — cannot change
/// what gets deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopArm {
    pub session_id: String,
    pub short_id: String,
    pub name: String,
    /// Stamped AFTER `claude stop` returned, so the shell-out does not eat the
    /// window the operator is told they have.
    pub at: Instant,
}

/// A delete that qualified and is waiting out `CX_SETTLE`. Holds its own copy
/// of the capture: once scheduled it is answerable to nothing on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDelete {
    pub target: StopArm,
    pub at: Instant,
}

/// Polls a `PendingJump` gets to find its session before it is dropped.
///
/// Counted in POLLS, not seconds, and that is the point: the sidebar's poll
/// cadence is not a constant. The idle ladder stretches it toward `IDLE_MAX`,
/// a failure streak adds `BACKOFF`, and the §4.2 gate stops it dead while
/// nobody is looking at this tab — so any wall-clock deadline would expire the
/// intent during a gap in which the sidebar learned NOTHING. A poll budget only
/// burns down on evidence: eight consecutive ANSWERS from `claude agents` that
/// did not settle the intent are eight refutations, whatever they took in wall
/// time.
///
/// An ANSWER, not a spawn. `poll_step` reports that `claude agents` RAN, which
/// is a different claim: its `Err` path leaves `self.sessions` exactly as it
/// found them, so a failed poll learned nothing about the id and must refute
/// nothing. Charging it let a transiently failing CLI — `FAIL_BACKOFF` is 10s —
/// spend the whole budget in ~80s without the sidebar having once seen a session
/// list, and then not jump when the list finally arrived. `tick_pending_jump`
/// reads `poll_error` for the difference.
///
/// The budget answers ONE question — does this session exist at all — and it is
/// spent only on that. `tick_pending_jump`'s other wait, for a footer that can
/// carry the filtered-out line, charges nothing: by then the session is listed
/// and the question is answered. Eight is generous against what is left.
/// `dispatch_and_close` forces a poll immediately, and the daemon lists a `--bg`
/// session on that poll or the next.
const JUMP_POLLS: u32 = 8;

/// `n`'s cursor intent: move the selection onto the session just dispatched,
/// once it appears (§8.6).
///
/// It has to be an intent rather than a move because the row does not exist
/// yet. `claude --bg` returns as soon as the daemon accepts the task, and the
/// forced poll that follows usually runs before the session is listed — so a
/// one-shot "select it now" would miss, and pointing `selected_key` at an
/// absent key would hand the cursor to `reanchor_selection`'s nearest-index
/// fallback and park it on an unrelated row. Exactly the trap
/// `act_undo_dismiss` documents.
///
/// THE INTENT IS CANCELLED THE MOMENT THE OPERATOR TAKES THE CURSOR, by
/// `cancel_pending_jump` in `key_normal` — on the KEYPRESS, not on the move it
/// produces. `set_selected` was the obvious funnel and it was wrong at both ends
/// of the list: `select_prev` on the top row and `select_next` on the bottom one
/// find no target row and never reach `set_selected` at all, so `k` and `j` —
/// the two commonest keys there are — cancelled nothing exactly where a jump is
/// most startling, and `Tab` on an empty list cancelled nothing either
/// (`cycle_group` returns early with no headers). A key that moves the cursor
/// nowhere is still the operator taking the cursor.
///
/// `claims_the_cursor` names the keys instead, and it is deliberately WIDER than
/// the moves: every verb that acts on the ROW UNDER the cursor is on it too.
/// `Ctrl+X` is the reason. Its first press stops the selection, opens a
/// two-second delete window and forces a poll — the poll most likely to be the
/// one that lists the new session — so a jump landing inside that window
/// retargeted the cursor between the operator's two presses, and
/// `arm_second_press` refused the delete with `moved off <name> — nothing
/// deleted`: a message whose whole purpose is to tell the operator moving apart
/// from the list moving, blaming them for a move ccmux had made itself.
///
/// A jump that yanks the selection out from under a keypress is worse than no
/// jump at all.
///
/// An EAGER cancel, deliberately, and not a `selected_key` snapshot compared at
/// fire time. A snapshot cannot tell the operator apart from
/// `reanchor_selection`, which re-points `selected_key` by itself whenever the
/// row under the cursor leaves the list — and, worse, whenever the cursor had
/// no key to start with. Dispatch from an EMPTY list is exactly that: the
/// snapshot would be `None`, the first arriving poll would park the cursor on
/// the first row through the nearest-index fallback, and the comparison would
/// read that as "the operator moved" and cancel the jump on the one screen
/// where it is most obviously wanted. Naming the KEYS is both narrower and
/// correct: a keypress is the one event in this program that only the operator
/// can produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingJump {
    /// The 8-hex short id `claude --bg` printed. Matched against `Session::id`,
    /// because that is the only identifier the dispatch gets back; the cursor
    /// is then driven with the row's `session_id`, which is what `key_at`
    /// returns and what `reanchor_selection` searches for. THE KEY MISMATCH is
    /// the whole reason this is a lookup and not an assignment.
    pub short_id: String,
    /// Polls left before the intent is dropped. See `JUMP_POLLS`.
    pub polls_left: u32,
}

/// Why `App::kill_owned_pane` left a pane standing.
#[derive(Debug)]
enum KillRefused {
    /// The pane is some tab's sidebar. Never a kill target, from any verb.
    Sidebar,
    /// tmux refused — R2's gate, or the pane was already gone.
    Tmux(TmuxError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// `n` — two fields: 0 = cwd, 1 = task text.
    NewBackground,
}

#[derive(Debug, Clone)]
pub struct Prompt {
    pub kind: PromptKind,
    /// `NewBackground`: ["<cwd>", "<task>"].
    pub fields: Vec<String>,
    pub focus: usize,
    pub cursor: usize,
    /// §8.6: the EXPANDED absolute path a missing-cwd `Enter` armed for
    /// creation, so the next `Enter` runs `create_dir` instead of refusing
    /// again. Lives inside `Prompt` on purpose: `Esc` and every mode change
    /// drop the prompt and the arm with it. Any edit to a field clears it
    /// (`key_prompt` / `on_paste`), and the second `Enter` re-expands the field
    /// and compares before creating, so a stale arm can never mkdir a path the
    /// operator is no longer looking at.
    pub pending_create: Option<String>,
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
    /// Restart this process in place: `main` tears the terminal down, flushes
    /// what is owed to tmux, and `exec`s `App::pending_restart`. Distinct from
    /// `Quit` because it must NOT leave the event loop by the exit path — that
    /// path returns to a launcher that is no longer there.
    Restart,
    Quit,
}

/// Where a session is on screen, resolved across EVERY tab.
///
/// Rebuilt from scratch on every pane refresh as
/// `union(every tab's @ccmux_tab_map) ∩ live panes`. The intersection with the
/// live pane list is what makes a cross-tab `x` correct in the same frame it
/// happens: a pane another tab killed is gone from every process's view at once,
/// without anyone having to write to a window they do not own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPane {
    /// The pane `Enter` jumps to and `x` closes: one in THIS sidebar's own tab
    /// when the session has one there, else the lowest-numbered live one.
    pub pane: PaneId,
    /// `#{window_index}` of that pane's tab — what the sidebar's badge shows.
    /// `None` before any pane inventory has been taken.
    pub window_index: Option<u32>,
    /// Stable identity of that pane's tab, for "is this my tab" comparisons.
    pub window: Option<WindowId>,
    /// Is `claude attach` still the thing in that pane?
    ///
    /// MAPPED BUT NOT ATTACHED is a real state, not a transient. Since the
    /// attach hands its pane to a shell instead of ending with it
    /// (`agents::attach_pane_cmd`), a pane ccmux opened now outlives the
    /// session it opened — indefinitely, with the operator working in it. The
    /// map still owns that pane, and must: `x` is the only way ccmux can close
    /// what ccmux opened. But everything that means "this session is ON SCREEN"
    /// — `ui::is_open`'s `▌`, the tab badge, `Enter`'s jump, and `R`'s licence
    /// to respawn — has to read this flag, not mere membership, or the sidebar
    /// keeps pointing at a shell.
    ///
    /// True with no pane inventory at all (degraded, or before the first
    /// refresh), where the intersection that would prove otherwise is skipped.
    pub attached: bool,
}

/// Who, if anyone, can see this sidebar right now — the poll gate's input.
///
/// SPEC §4.2 (amended): a `claude agents` spawn exists to put fresh rows in
/// front of a human. tmux answers "is a human in front of this pane" locally
/// and for free in the `list-panes` ccmux already runs every tick, so the
/// question is asked there rather than assumed.
///
/// The question is per WINDOW, not per session: a client attached through a
/// grouped session renders one of these windows while leaving the sidebar's
/// own session at zero attached clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watchers {
    /// Some client — of my session or of one grouped with it — is rendering
    /// this sidebar's window. The only state the fast path is for.
    Onscreen,
    /// Nothing is rendering my window, but my session has clients: they are on
    /// another tab. Nothing this process draws reaches a screen until the
    /// operator switches back.
    OtherTab,
    /// Nothing is rendering my window and my session has no clients at all —
    /// detached, or overnight.
    Detached,
    /// The pane inventory cannot answer: degraded, a failed enumeration, or no
    /// resolvable own pane. **Counts as watched.** The gate closes only on
    /// positive evidence that nobody is looking; a guess, or a stale row from
    /// the last good listing, must never be able to stop the sidebar updating.
    Unknown,
}

impl Watchers {
    /// Does this state justify spawning `claude agents`?
    pub fn polls(self) -> bool {
        matches!(self, Watchers::Onscreen | Watchers::Unknown)
    }
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
    /// Sessions dismissed with `d`, oldest first — the newest is what `u`
    /// restores. A VIEW filter only: it is passed to `model::build_rows`
    /// alongside `filter` and `show_completed` and touches nothing else. The
    /// agents keep running and `claude` is never told.
    pub hidden: HiddenSet,
    /// Set by `d`, `u` and reconciliation; flushed to `@ccmux_hidden` by the
    /// next `tick`, and by `shutdown` when there is no next tick. Never written
    /// from the keypress itself, so `d` issues no tmux command at all.
    pub hidden_dirty: bool,
    /// Dismissed ids that were missing from the PREVIOUS complete poll — the
    /// first strike of `reconcile_hidden`'s two. In memory only, deliberately:
    /// it is a debounce, not state worth outliving the process, and keeping it
    /// out of `HiddenSet` keeps `@ccmux_hidden` the `{"v":1,"ids":[…]}` it
    /// round-trips today.
    pub hidden_absent: std::collections::BTreeSet<String>,
    pub mode: Mode,
    pub prompt: Option<Prompt>,
    pub logs: Option<LogsView>,

    /// MY window's fragment of the shared dismissal log — the only thing this
    /// process ever writes to `@ccmux_tab_hidden`. `hidden` above is the FOLD
    /// of this fragment and every other tab's, recomputed each refresh.
    pub hidden_log: HiddenLog,
    /// Every OTHER tab's fragment as of the previous refresh, keyed by
    /// `WindowId::num()`. Kept so a fragment orphaned by a closed tab can be
    /// adopted rather than silently un-hiding every row that tab dismissed.
    pub last_frags: BTreeMap<u64, HiddenLog>,
    /// Lamport clock: the largest stamp seen in any fragment, including my own
    /// writes. `next_seq` is `max(now_ms, seq_seen + 1)`, so a stamp is
    /// monotonic per process even across a backward wall-clock step.
    pub seq_seen: u64,

    // tmux
    pub map: PaneMap,
    pub map_dirty: bool,
    pub sidebar_pane: Option<PaneId>,
    pub panes: Vec<PaneInfo>,
    /// This process's OWN pane, from `$TMUX_PANE`, accepted only when it
    /// appears in `list_panes_in_session`. It is what every window-scoped write
    /// is addressed through, so with no own pane there is no addressable target
    /// and persistence behaves exactly as it does when degraded: in memory.
    pub own_pane: Option<PaneId>,
    /// The window `own_pane` lives in — this process's tab.
    pub own_window: Option<WindowId>,
    /// Every tab of the session, from one `list-windows` per refresh.
    pub tabs: Vec<TabInfo>,
    /// `session_id` -> where it is open, across every tab.
    pub open: BTreeMap<String, OpenPane>,
    /// `@ccmux_width`, which rides along free in the `list_tabs` read.
    pub width_opt: Option<u16>,
    /// The shell command that starts a sidebar, handed down by `main.rs` so
    /// `t` can give the tab it creates its own sidebar. `app` never imports
    /// `main`; the DAG stays acyclic.
    pub sidebar_cmd: Option<String>,
    /// My window's stored map and hidden fragment are adopted exactly ONCE.
    /// After that this process is their sole author, and re-adopting the stored
    /// copy would let a stale read undo a write it has not seen yet.
    pub own_state_loaded: bool,
    /// The one-time import of a session written by a build that had no tabs.
    pub migrated: bool,
    /// True when running outside tmux: list/filter/refresh/logs work, every
    /// pane verb refuses with a message (§9.5).
    pub degraded: bool,

    // messaging + health
    /// `Ctrl+X`'s delete window: `Some` for `CX_WINDOW` after a press that
    /// stopped (or found already stopped) a session. `None` is "the next press
    /// is a first press", which is the recoverable verb — fail closed.
    pub stop_arm: Option<StopArm>,
    /// When the LAST `Ctrl+X` arrived, acted on or not. The burst / auto-repeat
    /// guard; see `CX_MIN_GAP`.
    pub cx_last_press: Option<Instant>,
    /// A qualifying second press, waiting out `CX_SETTLE`; see the constant.
    pub pending_delete: Option<PendingDelete>,
    /// `n`'s cursor intent, waiting for the dispatched session to be listed.
    /// See `PendingJump`. `None` is the normal state — at most one is ever
    /// live, and a second `n` replaces the first (the newest dispatch is the
    /// one the operator is watching for).
    pub pending_jump: Option<PendingJump>,

    pub message: Option<(String, MsgLevel)>,
    pub msg_deadline: Option<Instant>,
    pub poll_error: Option<String>,
    pub fail_streak: u32,
    pub last_poll: Instant,

    /// THE DRIFT GUARD (see `note_drift`). Every unmodelled `state=`/`status=`
    /// value whose warning has been DELIVERED — it held an uncovered footer
    /// for its full window. These never warn again. Bounded by the CLI's
    /// vocabulary, which is a handful of words.
    pub drift_seen: BTreeSet<String>,
    /// Unmodelled values seen in a payload but not yet delivered. A value sits
    /// here while an overlay covers the footer or another flash owns it, and is
    /// re-posted on a later poll rather than being retired unread.
    pub drift_pending: BTreeSet<String>,
    /// The exact warning text `announce_drift` last put in the footer, kept so
    /// the next poll can tell "it ran its course" from "something stomped it".
    pub drift_flash: Option<String>,

    // ── poll gate (SPEC §4.2, amended) ──────────────────────────────────────
    //
    // `last_poll` above times the TICK, which stays on `interval` whatever the
    // gate decides: the tick is two local tmux reads plus the §1.3 pin, it
    // costs no network, and it is what notices the operator coming back. These
    // four time the `claude agents` spawn, which is the expensive thing.
    /// When `agents::poll()` last actually ran. Separate from `last_poll`
    /// precisely so a skipped poll does not also skip the pane refresh that
    /// would notice the sidebar becoming visible again.
    pub last_agents: Instant,
    /// Consecutive polls whose payload hashed identical to the one before.
    /// Drives the idle ladder; reset by any change, any keypress and any
    /// transition back to `Onscreen`.
    pub idle_streak: u32,
    /// Hash of the last applied payload. `None` before the first one.
    pub payload_fp: Option<u64>,
    /// A poll owed on the next tick whatever the gate and the ladder say: `r`,
    /// every post-verb refresh, the first tick of the process, and the edge
    /// back to `Onscreen`.
    pub force_poll: bool,
    /// `watchers().polls()` as of the previous tick — one half of the edge
    /// detector that makes becoming visible refresh immediately.
    pub was_watched: bool,
    /// True while the gate is closed. Purely for the operator: the header dot
    /// goes hollow so the frame tmux replays when you switch back says "this
    /// was paused" rather than pretending it is live. NOT an error state.
    pub quiesced: bool,
    /// Did the LAST `list_panes_in_session` succeed?
    ///
    /// `refresh_panes` keeps the previous inventory when an enumeration fails,
    /// because the pane map, `pane_of` and the open list are all better served
    /// by a slightly stale answer than by no answer. The poll gate is the one
    /// reader for which that is false: a stale row is not evidence about who
    /// is looking NOW, and reading it as such is how a sidebar that was in a
    /// background tab when the session got renamed stayed quiesced forever.
    /// So the gate reads this flag first and answers `Unknown` — which polls.
    pub panes_fresh: bool,

    /// Did the LAST `list_tabs` succeed?
    ///
    /// The other half of `panes_fresh`, and it is separate because the two
    /// reads fail separately: `refresh_panes` keeps the previous `self.tabs`
    /// when `list-windows` errors, which is right for every drawing reader and
    /// wrong for the one decision that turns on ABSENCE. `Plan::held` is built
    /// from the tab maps, so an unread `list-windows` empties it — and an
    /// empty `held` is indistinguishable, to `headless_targets`, from a fleet
    /// no pane holds. `R` therefore refuses to plan at all without both reads:
    /// see `restart_others`.
    pub tabs_fresh: bool,

    pub should_quit: bool,

    /// `R`'s decision, made and consumed within one keypress: a binary that has
    /// been resolved and PROVEN to run, waiting for `main::event_loop` to hand
    /// this process's image over to it on `Action::Restart`. No pane has been
    /// killed at this point — that is the next image's job.
    pub pending_restart: Option<restart::Pending>,

    /// Seam for `tmux::respawn_pane`, for the same reason `dispatch` is one:
    /// `R` is the one verb whose whole job is killing processes in panes, and
    /// a hermetic test of WHICH panes it picks must be able to run it without
    /// a tmux server. Production (`App::new`) points it at the real command.
    pub respawn: fn(&str, &PaneId, &str) -> Result<(), TmuxError>,

    /// Seam for `tmux::kill_pane`, for the same reason `respawn` is one: `x`
    /// and `Ctrl+X`'s second press both kill panes through `kill_owned_pane`,
    /// and a hermetic test of WHICH panes a delete closes must be able to run
    /// that path without a tmux server. R2 lives inside the real function —
    /// `assert_in_session` on a validated `PaneId` — so the seam narrows
    /// nothing. Production (`App::new`) points it at the real command.
    pub kill_pane: fn(&str, &PaneId) -> Result<(), TmuxError>,

    /// Seam for `restart::probe`. The real one SPAWNS the candidate binary,
    /// and in a unit test the candidate is the test harness — `--version` is
    /// not a thing libtest answers, so every `R` test would exercise the
    /// refusal path and nothing else. Production points it at the real probe.
    pub probe: fn(&Path) -> Result<(), String>,

    /// Seam for `agents::dispatch_background`, and nothing else. Production
    /// (`App::new`) points it at the real function; the unit-test fixture
    /// points it at a PANICKING stub so no hermetic test can ever spawn the
    /// operator's real `claude` — `CCMUX_CLAUDE_BIN` cannot serve, because
    /// `claude_bin()` is a process-wide `OnceLock` that other tests may have
    /// already resolved to `claude`.
    pub dispatch: fn(&str, &str) -> Result<Option<String>, AgentsError>,

    /// Seam for `agents::poll`, for `R`'s agent pass ONLY — `tick` still hands
    /// `poll_step` the real function directly.
    ///
    /// It is a seam for the same reason `respawn` is: the pass decides WHICH
    /// live agents to stop, and a hermetic test of that decision must be able
    /// to script the fleet — including a fleet that CHANGES between two reads,
    /// which is the race the re-poll exists to narrow and cannot be staged any
    /// other way.
    pub agents_poll: fn() -> Result<model::Payload, AgentsError>,

    /// Seam for `agents::stop`, for `R`'s agent pass. `Ctrl+X` calls
    /// `agents::stop` directly; this exists so an `R` test can record the ORDER
    /// of the stops against the respawns, which is the ordering property the
    /// whole feature rests on and which two separate recorders cannot show.
    ///
    /// `agents::stop` is already hermetic under `cfg(test)` — its process
    /// boundary routes to `test_spawn::intercept` — so a test stub that calls
    /// through it spawns nothing while still proving the argv.
    pub agents_stop: fn(&str) -> Result<(), AgentsError>,

    /// Seam for `agents::respawn`, `R`'s restart for an agent with no client.
    ///
    /// It exists for the same two reasons `agents_stop` does, and one more
    /// that is specific to it: this is the only restart ccmux has that is not
    /// a pane, so a test of what a FAILED one is counted as — `not restarted`
    /// when the worker is still there, `left stopped` when the daemon halted
    /// it anyway — can only be written against one recorder that sees the verb
    /// and the polls around it on one timeline.
    pub agents_respawn: fn(&str) -> Result<(), AgentsError>,

    /// Seam for `agents::attached_ids`, the machine-wide "who has a client"
    /// read that scopes the paneless population.
    ///
    /// A seam because the production one walks the REAL `/proc` — every
    /// process on the operator's machine, their own live agents among them —
    /// and because the property that matters is a refusal: a test must be able
    /// to say "this session has an attach ccmux cannot see" and "the scan
    /// could not run at all" without arranging either on the host.
    pub agents_attached: fn() -> Option<BTreeSet<String>>,

    /// `AGENT_BUDGET`, as a field so a test can set it to zero and prove the
    /// pane pass survives an exhausted budget without sleeping for a minute.
    pub agent_budget: Duration,
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
            hidden: HiddenSet::new(),
            hidden_dirty: false,
            hidden_absent: std::collections::BTreeSet::new(),
            hidden_log: HiddenLog::new(),
            last_frags: BTreeMap::new(),
            seq_seen: 0,
            mode: Mode::Normal,
            prompt: None,
            logs: None,

            map: PaneMap::new(),
            map_dirty: false,
            sidebar_pane: None,
            panes: Vec::new(),
            own_pane: None,
            own_window: None,
            tabs: Vec::new(),
            open: BTreeMap::new(),
            width_opt: None,
            sidebar_cmd: None,
            own_state_loaded: false,
            migrated: false,
            degraded,

            stop_arm: None,
            cx_last_press: None,
            pending_delete: None,
            pending_jump: None,

            message: None,
            msg_deadline: None,
            poll_error: None,
            fail_streak: 0,
            drift_seen: BTreeSet::new(),
            drift_pending: BTreeSet::new(),
            drift_flash: None,
            // Backdated so the event loop's first iteration polls immediately.
            // `checked_sub` because a bare `Instant - Duration` can panic when
            // the process starts within `interval` of the monotonic epoch.
            last_poll: Instant::now()
                .checked_sub(interval)
                .unwrap_or_else(Instant::now),

            last_agents: Instant::now()
                .checked_sub(interval)
                .unwrap_or_else(Instant::now),
            idle_streak: 0,
            payload_fp: None,
            // The FIRST tick polls unconditionally. The launcher creates the
            // session detached and attaches a moment later, so a gate applied
            // to tick one would meet `session_attached == 0` and open the
            // sidebar on an empty list for a tick. Startup behaviour is
            // therefore byte-for-byte what it was.
            force_poll: true,
            was_watched: false,
            quiesced: false,
            // Nothing has been enumerated yet, and in degraded mode nothing
            // ever will be: both must read as "cannot answer", which polls.
            panes_fresh: false,
            tabs_fresh: false,

            should_quit: false,

            pending_restart: None,
            respawn: tmux::respawn_pane,
            kill_pane: tmux::kill_pane,
            probe: restart::probe,
            dispatch: agents::dispatch_background,
            agents_poll: agents::poll,
            agents_stop: agents::stop,
            agents_respawn: agents::respawn,
            agents_attached: agents::attached_ids,
            agent_budget: AGENT_BUDGET,
        }
    }

    /// Startup state, set `degraded`. Never fails; failures degrade.
    ///
    /// Loading no longer happens here: with per-tab state a process cannot know
    /// what to load until it knows which window it is in, and that answer comes
    /// from the pane inventory. The first `tick` runs immediately (`last_poll`
    /// is backdated), so `refresh_panes` adopts this window's stored map and
    /// dismissal fragment before the first frame is drawn.
    pub fn init(&mut self) {
        self.degraded = !tmux::inside_target_server();
        // §9.5: outside tmux the map and the dismissed set are held in memory
        // only; load/save are skipped. `d` and `u` still work — unlike the pane
        // verbs they need nothing from tmux to be correct.
        self.map = PaneMap::new();
        self.hidden = HiddenSet::new();
        self.hidden_log = HiddenLog::new();
        // A fresh process has seen no polls, so no id has a strike against it.
        // Adopting a stored fragment must not import one either: an id
        // dismissed in a previous run is owed the same two chances as a fresh
        // dismissal.
        self.hidden_absent.clear();
    }

    /// Called when `last_poll.elapsed() >= tick_interval()`.
    /// Order is fixed:
    ///   1. now_ms = Utc::now().timestamp_millis()
    ///   2. panes = list_panes_in_session(tmux_session)   [skipped when degraded]
    ///   3. map.reconcile(&panes) -> map_dirty |= changed
    ///      3b. read the visibility pair off my own pane's row and open or
    ///      close the poll gate — `observe_watchers`, which must run AFTER the
    ///      inventory it reads and BEFORE the poll it gates
    ///   4. agents::poll() -> sessions, IF `poll_due()` (on Err: keep last
    ///      good, bump fail_streak)
    ///      4b. on Ok ONLY: reconcile the dismissed set against the fresh poll
    ///      — 4 and 4b are `apply_poll`, which is where their whole policy
    ///      lives so it can be tested without shelling out to `claude`
    ///   5. rebuild rows, re-anchor selection by selected_key
    ///      5b. settle `n`'s pending cursor jump against the rebuilt rows —
    ///      `tick_pending_jump`, which needs both the fresh `sessions` and the
    ///      rows the filters just decided
    ///   6. flush the map if map_dirty, and the dismissed set if hidden_dirty
    ///   7. pin_sidebar (unconditional, §1.3)
    ///   8. last_poll = Instant::now()
    ///
    /// Steps 1-3 and 5-8 run EVERY tick, gate or no gate, and every
    /// `tick_interval()` — which is `interval`, flat, with no backoff in it.
    /// They are local: two tmux reads on a unix socket and the §1.3 pin. Only
    /// step 4 spawns `claude`, and only step 4 is skipped or backed off —
    /// which is what keeps "the operator switched back to this tab" a fact
    /// this process learns within one `interval` rather than one backoff.
    pub fn tick(&mut self) {
        // 1
        self.now_ms = chrono::Utc::now().timestamp_millis();

        // 2 + 3 (+ §5.3 step 5)
        self.refresh_panes();

        // 3b
        self.observe_watchers();

        // 4 + 4b — gated. A skipped poll is NOT a failed poll: nothing here
        // touches `poll_error`, `fail_streak`, or `reconcile_hidden`'s
        // two-strike debounce, so the header stays healthy and no dismissal
        // ages toward being dropped on the strength of a poll never taken.
        let polled = self.poll_step(agents::poll);

        // 5
        self.rebuild_rows();

        // 5b — `n`'s cursor intent, once the rows it asks about are rebuilt.
        self.tick_pending_jump(polled);

        // 6
        self.save_map_now();
        self.save_hidden_now();

        // 7 — unconditional (§1.3); this is what heals a manual resize.
        self.pin_sidebar();

        // 8
        self.last_poll = Instant::now();
    }

    /// Steps 4 and 4b of `tick`, split out from the IO that produces the
    /// argument so the whole policy is reachable from a test. `tick` is now a
    /// thin shell around `agents::poll()`; everything it decides, it decides
    /// here.
    ///
    /// §9.1/§9.3: an `Err` keeps the last good list on screen; an exit-0 `[]`
    /// is a valid empty result, not an error, and clears `poll_error`.
    pub fn apply_poll(&mut self, res: Result<model::Payload, agents::AgentsError>) {
        match res {
            Ok(payload) => {
                let complete = payload.is_complete();
                // Interactive sessions are NEVER listed. ccmux is a
                // background-agent explorer: there is no `claude attach` for an
                // interactive session, and one owned by Claude Desktop has no
                // tmux pane to jump to either, so such a row is permanently
                // un-openable.
                //
                // This is a display POLICY, so it lives here and not in
                // `model::parse_sessions` — the parser's job is to report what
                // the CLI actually said, and its tests assert exactly that.
                // Filtering here also keeps `Kind` out of `build_rows`, so the
                // view filters (`/`, `a`, `d`) never have to know about it, and
                // the header's `total` counts what ccmux actually manages.
                //
                // Deliberately NOT folded into `complete`: that flag means "the
                // payload lost rows to parse errors", which makes
                // `reconcile_hidden` distrust the whole poll. An intentional
                // exclusion is not a loss and must not suppress reconciliation.
                // THE DRIFT GUARD reads the payload as the CLI sent it, BEFORE
                // the interactive filter below. The other half of the guard —
                // `agents::tests::live_state_and_status_values_are_all_modelled`
                // — iterates `poll()`'s raw payload, and the two halves must
                // scan the same population or the ignored test can fail over a
                // row the running sidebar never had a word to say about.
                self.note_drift(&payload.sessions);
                self.sessions = payload
                    .sessions
                    .into_iter()
                    .filter(|s| s.kind != model::Kind::Interactive)
                    .collect();
                self.poll_error = None;
                self.fail_streak = 0;
                self.note_idle();
                self.reconcile_hidden(complete);
            }
            Err(e) => {
                self.fail_streak = self.fail_streak.saturating_add(1);
                self.poll_error = Some(model::truncate_end(&agents_msg(&e), POLL_ERR_MAX));
                // A failed poll says nothing about whether the fleet is still.
                // `fail_interval` owns the cadence from here, and
                // `agents_interval` takes the longer of the two, so clearing
                // the ladder cannot make a failing CLI poll faster.
                self.idle_streak = 0;
            }
        }
    }

    /// THE DRIFT GUARD. Say out loud that the CLI reported a `state` or
    /// `status` this build does not model — and keep saying it until it has
    /// actually been said somewhere a human could have read it.
    ///
    /// `state: "stopped"` shipped unmodelled, was fixed, and the fix carried a
    /// doc comment warning that it would happen again. It then happened again,
    /// with `state: "blocked"` — two sessions genuinely waiting on the operator
    /// sat under **Idle** behind a purple `?` for as long as the CLI had been
    /// emitting the value. Nothing in ccmux said a word, because an unmodelled
    /// value renders as a legal-looking row: `Unknown` is quiet by design.
    ///
    /// So the quiet is what this removes. The rules it deliberately keeps:
    ///   * It NEVER panics and NEVER hides the row. An unrecognised value is
    ///     the CLI being newer than this build, not the operator doing anything
    ///     wrong; the row still renders (`?`, purple) and still groups (Idle,
    ///     or Blocked when `status` says `waiting`).
    ///   * It fires ONCE per distinct value per run. A warning on every poll is
    ///     a warning nobody reads.
    ///   * An ABSENT `status` key is not drift. Most `state: "done"` rows carry
    ///     no `status` at all (9 of 14 on 2026-08-31), which parses to
    ///     `Status::Unknown("")` — the empty string is the tell, and it is
    ///     skipped.
    ///
    /// "ONCE" IS COUNTED IN DELIVERIES, NOT IN ATTEMPTS, and that distinction
    /// is the whole difference between a guard and a gesture. The warning's
    /// only channel is the single shared `message` slot on a 4s timer, which
    /// any keypress flash overwrites and which `Mode::Help`, `Mode::Logs`, a
    /// prompt and the filter line all cover completely. Retiring a value at
    /// POST time therefore let one stray `d` — or having the help overlay open
    /// when the poll landed — silence it for the rest of the process, putting
    /// the row straight back to being a quiet purple `?` under Idle. So a value
    /// moves from `drift_pending` to `drift_seen` only once `announce_drift`
    /// has OBSERVED its warning run a full window in an uncovered footer with
    /// nothing replacing it; anything else puts it back in the queue for the
    /// next poll.
    ///
    /// The second half of the guard is the `#[ignore]`d live test
    /// `agents::tests::live_state_and_status_values_are_all_modelled`, which
    /// asks the real fleet the same question ahead of an operator having to
    /// notice it on screen. It reads `poll()`'s raw payload, which is why this
    /// is handed the raw payload too.
    fn note_drift(&mut self, raw: &[Session]) {
        for sess in raw {
            if let model::Status::Unknown(v) = &sess.status
                && !v.is_empty()
            {
                let label = format!("status {v:?}");
                if !self.drift_seen.contains(&label) {
                    self.drift_pending.insert(label);
                }
            }
            if let Some(model::State::Unknown(v)) = &sess.state {
                let label = format!("state {v:?}");
                if !self.drift_seen.contains(&label) {
                    self.drift_pending.insert(label);
                }
            }
        }
        self.announce_drift();
    }

    /// True while something owns or covers the footer, so a warning posted now
    /// would not be read. `Help`, `Logs` and `Prompt` all render over the WHOLE
    /// sidebar rect (§6.7), and `Filter` owns the footer line itself.
    fn footer_is_covered(&self) -> bool {
        !matches!(self.mode, Mode::Normal)
    }

    /// Event-loop hook, called next to `check_message_timeout` — returns true
    /// when it just put a warning on screen and the caller should redraw.
    ///
    /// A poll is the WRONG and only-nearly-right moment to retry. Polls
    /// legitimately quiesce: the visibility gate stops them dead when no client
    /// is attached, and the idle ladder stretches them to 30s on a still fleet.
    /// A warning stomped by a `d` at the top of the ladder would then wait half
    /// a minute for its next chance, and one stomped after the last client
    /// detached would wait for a client to come back. The moment the footer
    /// actually frees up is a tick, not a poll, so the retry lives on the tick.
    pub fn tick_drift(&mut self) -> bool {
        let before = self.message.clone();
        self.announce_drift();
        self.message != before
    }

    /// Post the pending drift warning when the footer can actually carry it,
    /// and retire values only once their warning has been seen through.
    fn announce_drift(&mut self) {
        // First settle the fate of the warning posted last time. Delivery is
        // OBSERVED, never assumed.
        if let Some(txt) = self.drift_flash.clone() {
            let still_ours = matches!(&self.message, Some((m, _)) if *m == txt);
            if still_ours && !self.footer_is_covered() {
                // On screen and still counting down. Leave it be.
                return;
            }
            self.drift_flash = None;
            if self.message.is_none() && !self.footer_is_covered() {
                // Gone, with nothing in its place and nothing over it: it ran
                // its whole window where it could be read. That is as much as
                // this can honestly check, and it is enough to stop nagging.
                self.drift_seen.append(&mut self.drift_pending);
            }
            // Otherwise it was clobbered by another flash or hidden behind an
            // overlay. `drift_pending` still holds the values; a later poll
            // posts them again.
        }
        if self.drift_pending.is_empty() || self.footer_is_covered() || self.message.is_some() {
            return;
        }
        // Truncated to the sidebar's own budget by `draw_footer`; naming the
        // first value and counting the rest keeps the sentence readable at 34
        // columns, where the whole list of them would not fit.
        let first = self.drift_pending.iter().next().cloned().unwrap_or_default();
        let msg = match self.drift_pending.len() - 1 {
            0 => format!("unmodelled {first} — update ccmux"),
            n => format!("unmodelled {first} +{n} more — update ccmux"),
        };
        self.drift_flash = Some(msg.clone());
        self.flash(msg, MsgLevel::Warn);
    }

    /// Fold the payload just applied into the idle ladder.
    ///
    /// "Unchanged" is the hash of every field of every listed session, in the
    /// order the CLI emitted them — not just the count, and not just identity:
    /// a session going Working -> Idle changes no id and no row count, and it
    /// is precisely the change the operator is watching for.
    ///
    /// Relative ages (`2m`) are NOT in the hash and must not be: they are
    /// recomputed from `now_ms` on every draw, which still happens every tick,
    /// so a widened poll interval never freezes them.
    fn note_idle(&mut self) {
        let fp = Self::fingerprint(&self.sessions);
        if self.payload_fp == Some(fp) {
            self.idle_streak = self.idle_streak.saturating_add(1);
        } else {
            self.idle_streak = 0;
        }
        self.payload_fp = Some(fp);
    }

    fn fingerprint(sessions: &[model::Session]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        sessions.len().hash(&mut h);
        sessions.hash(&mut h);
        h.finish()
    }

    /// 4b — drop dismissals for sessions that are gone, WITHOUT a hair trigger.
    ///
    /// Reconciliation is tidiness, not safety: `HIDDEN_MAX` already bounds the
    /// set, so nothing here has to be eager. Being eager is expensive, because
    /// dropping an id also drops the `u` that would bring the row back, and
    /// that is not recoverable by any keypress.
    ///
    /// Two things make a SUCCESSFUL poll under-report:
    ///   * `model::parse_sessions` skips individual malformed rows. That is
    ///     detected exactly — `Payload::dropped` counts them — and a lossy
    ///     payload is no evidence about any missing id, so it concludes
    ///     nothing: it neither drops an id nor lets one off.
    ///   * `claude` exits 0 with `[]` mid-hiccup, which parses perfectly and
    ///     cannot be detected at all. So absence is debounced instead: an id
    ///     is dropped only when TWO consecutive complete polls agree it is
    ///     gone. One bad poll can no longer erase a dismissal, let alone the
    ///     whole set.
    ///
    /// An `Err` poll never reaches here: nothing is concluded from a poll we
    /// did not get.
    fn reconcile_hidden(&mut self, complete: bool) {
        if !complete {
            return;
        }
        let live: std::collections::HashSet<&str> =
            self.sessions.iter().map(|s| s.session_id.as_str()).collect();
        let absent: std::collections::BTreeSet<String> = self
            .hidden
            .ids()
            .iter()
            .filter(|id| !live.contains(id.as_str()))
            .cloned()
            .collect();
        // Survivors: alive right now, or absent for the first time. Passing the
        // union to `reconcile` keeps the "is it gone" question in one place —
        // `HiddenSet` still just retains what the caller says is live.
        let spared: Vec<String> = self
            .hidden
            .ids()
            .iter()
            .filter(|id| live.contains(id.as_str()) || !self.hidden_absent.contains(*id))
            .cloned()
            .collect();
        let retired: Vec<String> = self
            .hidden
            .ids()
            .iter()
            .filter(|id| !spared.iter().any(|s| s == *id))
            .cloned()
            .collect();
        if self.hidden.reconcile(spared.iter().map(String::as_str)) {
            self.hidden_dirty = true;
        }
        // Retire the OPS too, or the next fold would re-hide the id from my own
        // fragment. Other tabs drop their ops on the same id from the same
        // `claude agents` data; any transient disagreement concerns a session
        // that is not in the list at all, so nothing on screen can show it.
        if self.hidden_log.forget(retired.iter().map(String::as_str)) {
            self.hidden_dirty = true;
        }
        self.hidden_absent = absent;
    }

    /// The tick cadence: `interval`, flat.
    ///
    /// Neither the poll gate nor the failure backoff may touch it, and the
    /// backoff is the one that had to be taken OUT. The tick is the local
    /// work — the pane inventory, the visibility read, the §1.3 pin — and it
    /// is also the gate's own edge detector. `fail_streak` can only be cleared
    /// by a successful poll, and a poll is exactly what a shut gate skips, so
    /// a sidebar that quiesced while the CLI was failing used to keep a 10 s
    /// tick forever: it then took up to a backoff, not up to an interval, to
    /// notice the operator had come back, and the §1.3 pin and the pane
    /// reconcile were slowed with it for a `claude` that was never retried.
    /// The backoff belongs to the spawn, and now lives only in
    /// `agents_interval`.
    pub fn tick_interval(&self) -> Duration {
        self.interval
    }

    /// The failure half of the poll cadence: `interval`, or 10s once
    /// `fail_streak >= 3` (SPEC §4.2). A failing `claude` is retried less
    /// often; nothing else slows down with it.
    pub fn fail_interval(&self) -> Duration {
        if self.fail_streak >= FAIL_BACKOFF_AT {
            BACKOFF
        } else {
            self.interval
        }
    }

    /// Who can see this sidebar, read off THIS process's own pane row in the
    /// inventory `refresh_panes` already took. No tmux call of its own.
    ///
    /// Every unknown answers `Unknown`, which polls: an enumeration that
    /// failed this tick (`panes_fresh` false, which also covers degraded mode
    /// and the pre-first-tick state), no own pane (`$TMUX_PANE` unset or
    /// naming a pane of another session), or a fresh inventory that does not
    /// carry my pane.
    ///
    /// "On screen" is `#{window_active_clients}` when tmux answered it —
    /// clients rendering THIS window, from whichever session — and only when
    /// it did not does it fall back to the `window_active`/`session_attached`
    /// pair. The pair alone reads a grouped session (`new-session -t ccmux`)
    /// as detached while the operator is looking straight at the sidebar.
    ///
    /// When nothing is rendering the window, the two silent labels are
    /// attributed by THIS session's own client count: they are diagnostic, and
    /// the gate treats them identically.
    pub fn watchers(&self) -> Watchers {
        if !self.panes_fresh {
            return Watchers::Unknown;
        }
        let Some(me) = self.own_pane.as_ref() else {
            return Watchers::Unknown;
        };
        let Some(row) = self.panes.iter().find(|p| &p.id == me) else {
            return Watchers::Unknown;
        };
        let onscreen = match row.window_viewers {
            Some(viewers) => viewers > 0,
            None => row.session_clients > 0 && row.window_active,
        };
        if onscreen {
            Watchers::Onscreen
        } else if row.session_clients == 0 {
            Watchers::Detached
        } else {
            Watchers::OtherTab
        }
    }

    /// Step 3b of `tick`: latch the gate, and force a poll on the edge back
    /// into view.
    ///
    /// The edge is what makes a long idle ladder safe. Coming back to the tab
    /// must not mean waiting out however far the ladder had grown while nobody
    /// was looking, so the transition both clears the ladder and owes a poll
    /// on this very tick — at most `interval` after the switch, never longer.
    fn observe_watchers(&mut self) {
        let watched = self.watchers().polls();
        if watched && !self.was_watched {
            self.idle_streak = 0;
            self.force_poll = true;
        }
        self.was_watched = watched;
        self.quiesced = !watched;
    }

    /// Gap between `claude agents` spawns while the payload keeps coming back
    /// identical: `interval` until `IDLE_BACKOFF_AT`, then doubling to
    /// `IDLE_MAX`.
    ///
    /// At the 2500 ms default that is 2.5s, 2.5s, 2.5s, 2.5s, 5s, 10s, 20s,
    /// then 30s for as long as nothing moves.
    pub fn idle_interval(&self) -> Duration {
        let steps = self.idle_streak.saturating_sub(IDLE_BACKOFF_AT.saturating_sub(1));
        if steps == 0 {
            return self.interval;
        }
        // `1 << 5` is already past `IDLE_MAX` at any sane `interval`; the clamp
        // is there so a long streak cannot overflow the shift.
        self.interval.saturating_mul(1u32 << steps.min(5)).min(IDLE_MAX)
    }

    /// The interval the `claude agents` spawn actually honours: the failure
    /// backoff and the idle ladder, whichever is longer. A failing CLI and a
    /// still fleet must not be able to talk each other into polling faster.
    ///
    /// This is where the failure backoff applies, and the only place. A forced
    /// poll still outranks it (`poll_due`), so returning to a quiesced tab
    /// refreshes on the next tick even mid-backoff.
    pub fn agents_interval(&self) -> Duration {
        self.fail_interval().max(self.idle_interval())
    }

    /// Steps 4 and 4b of `tick`, with the decision in front of the spawn.
    /// Returns whether `claude agents` actually ran.
    ///
    /// `fetch` is a CLOSURE, not a value, and that is the whole design: an
    /// eagerly evaluated `agents::poll()` at the call site would spawn the
    /// process and then throw its answer away, which is precisely the waste
    /// this gate exists to remove. It is also the seam the gate's tests drive,
    /// so they can assert what a skipped poll does not do — no `poll_error`,
    /// no `fail_streak`, no reconciliation strike — without shelling out.
    pub fn poll_step(
        &mut self,
        fetch: impl FnOnce() -> Result<model::Payload, AgentsError>,
    ) -> bool {
        if !self.poll_due() {
            return false;
        }
        self.force_poll = false;
        self.apply_poll(fetch());
        self.last_agents = Instant::now();
        true
    }

    /// Should this tick spawn `claude agents`?
    ///
    /// Order is the contract: a forced poll outranks everything, so `r` and
    /// every post-verb refresh land immediately whatever the gate or the
    /// ladder say. Otherwise the gate closes on positive evidence that nobody
    /// is looking, and only then does the clock get a say.
    pub fn poll_due(&self) -> bool {
        if self.force_poll {
            return true;
        }
        if !self.watchers().polls() {
            return false;
        }
        self.last_agents.elapsed() >= self.agents_interval()
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
        // A keypress is a human at the keyboard, which is the strongest
        // evidence the sidebar has that it is being read. Collapse the idle
        // ladder: whatever it had grown to, the next tick is back on
        // `interval`. It does not force a poll — that would put a `claude`
        // spawn behind every `j` — it only stops the sidebar being slow to
        // update for someone who is demonstrably looking at it.
        self.idle_streak = 0;

        // §8.9: Ctrl-c quits from ANY mode, immediately, without confirming.
        // Checked before mode dispatch on purpose.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.disarm_ctrl_x();
            self.should_quit = true;
            return Action::Quit;
        }

        // §8.9: dispatch on mode FIRST; only Normal sees the §8.1 table.
        let action = match self.mode.clone() {
            Mode::Normal => self.key_normal(key),
            Mode::Filter => self.key_filter(key),
            Mode::Prompt(_) => self.key_prompt(key),
            Mode::Help => self.key_help(key),
            Mode::Logs => self.key_logs(key),
        };
        // §8.2: `Ctrl+X`'s window belongs to the list in Normal mode. Anything
        // that leaves Normal — `/`, `n`, `?`, `L` — or quits closes it, because
        // the footer that carries the warning is no longer the thing on screen
        // and the operator's attention has moved with it. One place, so no
        // handler can forget.
        if self.mode != Mode::Normal || self.should_quit {
            self.disarm_ctrl_x();
        }
        action
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
                p.pending_create = None; // §8.6: a paste is an edit
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

    /// First live pane showing `session_id`, ACROSS EVERY TAB. Every entry in
    /// `open` came from some tab's map, i.e. from a pane ccmux opened inside
    /// `tmux_session`, so a `Some` result is always safe to pass to an R2-gated
    /// mutation — the session gate already spans every window (`list-panes -s`).
    pub fn pane_of(&self, session_id: &str) -> Option<PaneId> {
        self.open
            .get(session_id)
            .filter(|o| o.attached)
            .map(|o| o.pane.clone())
    }

    /// The pane `x` closes: `pane_of`, or — when the session's pane has fallen
    /// through to a shell — that pane anyway.
    ///
    /// `x` is the one verb that acts on the PANE rather than on the session, so
    /// it is the one verb that must still reach a detached one. ccmux opened it;
    /// without this, the pane ccmux created would be unclosable from the sidebar
    /// that created it, and the operator would have to go find it and `exit`.
    pub fn pane_of_any(&self, session_id: &str) -> Option<PaneId> {
        self.open.get(session_id).map(|o| o.pane.clone())
    }

    /// True when `pane` is any tab's sidebar. `x` refuses those: with a sidebar
    /// per tab, "the sidebar" is no longer a single pane, and closing another
    /// tab's would leave that tab blind until the next launch.
    fn is_any_sidebar(&self, pane: &PaneId) -> bool {
        self.sidebar_pane.as_ref() == Some(pane)
            || self.own_pane.as_ref() == Some(pane)
            || self.tabs.iter().any(|t| t.sidebar.as_ref() == Some(pane))
    }

    /// " in tab N" when `pane` is in another tab, empty when it is in mine.
    /// The pane index alone stopped naming anything once tabs existed: pane 2
    /// exists in every window.
    fn tab_suffix(&self, pane: &PaneId) -> String {
        let win = self.pane_window(pane);
        match (win, self.own_window.as_ref()) {
            (Some(w), Some(mine)) if &w == mine => String::new(),
            (Some(w), _) => self
                .tabs
                .iter()
                .find(|t| t.window == w)
                .map(|t| format!(" in tab {}", t.index))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// `#{pane_index}` of `pane`, for the sidebar's pane badge.
    pub fn pane_index_of(&self, pane: &PaneId) -> Option<u32> {
        self.panes.iter().find(|p| &p.id == pane).map(|p| p.index)
    }

    /// `pane_of(session_id).is_some()`. Drives the §6.4 open marker.
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

    /// Step 5b of `tick`: settle `pending_jump` against the list as it now
    /// stands (§8.6). `polled` is `poll_step`'s answer — did `claude agents`
    /// actually run this tick.
    ///
    /// Runs AFTER `rebuild_rows`, because both questions it asks are about the
    /// rebuilt list: is the session listed at all (`sessions`), and does it have
    /// a ROW (`is_visible` — `/`, `a` and `d` all decide that).
    ///
    /// Four ways out, and only two of them spend the intent: the cursor moves,
    /// or the operator is told why it cannot. "Not listed yet" costs a poll off
    /// the budget — see `JUMP_POLLS` for why that budget is counted in answers
    /// rather than in seconds or ticks. "Listed, but the footer cannot carry the
    /// reason" costs nothing and simply waits.
    pub fn tick_pending_jump(&mut self, polled: bool) {
        let Some(jump) = self.pending_jump.clone() else {
            return;
        };
        // A poll that RAN is not a poll that ANSWERED. `apply_poll` clears
        // `poll_error` on every `Ok` and sets it on every `Err`, and its `Err`
        // path leaves `self.sessions` untouched — so this is exactly "the poll
        // this tick spawned came back with a list". A failing CLI refutes
        // nothing and is charged nothing.
        let answered = polled && self.poll_error.is_none();

        // THE KEY MISMATCH resolved: `--bg` named it by short id, the cursor is
        // keyed by uuid. Everything past here is the ordinary `selected_key` /
        // `reanchor_selection` path — there is no second selection mechanism.
        let listed = self
            .sessions
            .iter()
            .find(|s| s.id.as_deref() == Some(jump.short_id.as_str()))
            .map(|s| (s.session_id.clone(), session_label(s)));

        if let Some((session_id, label)) = listed {
            if self.is_visible(&session_id) {
                self.pending_jump = None;
                self.selected_key = Some(session_id);
                self.reanchor_selection();
                return;
            }
            // `/` or `a` is hiding it. Clearing the operator's filter to chase
            // a row they did not ask to see would be a far bigger surprise than
            // a cursor that stayed put, so say so instead — the precedent `u`
            // set with `restored X — filtered out`.
            //
            // But only when the footer can actually carry the sentence. The
            // filter line OWNS the footer while `/` is open (`draw_footer`
            // returns before `footer_message` is ever consulted, and
            // `overflow_message` declines to carve the detail block), and
            // `Mode::Help`, `Mode::Logs` and a prompt cover the whole sidebar
            // rect — so flashing here would set a message nobody can read, and
            // `check_message_timeout` would drop it `MSG_TTL` later. The intent
            // would be spent on nothing: no cursor move AND no explanation, in
            // the one mode where the operator is most likely to be editing the
            // very filter the message is about.
            //
            // So HOLD it, exactly as THE DRIFT GUARD holds a warning it could
            // not deliver (`announce_drift`), and settle on a later tick.
            if self.footer_is_covered() {
                // AND CHARGE NOTHING FOR THE WAIT. The budget answers a
                // different question — "does this session exist at all" — and
                // that question is already answered here; only one sentence is
                // outstanding. Charging the wait made the hold worthless in the
                // case it exists for: at the 2500ms default the whole budget is
                // ~20s of thinking with `/` open, measured, after which the
                // operator got neither the cursor move nor the explanation,
                // which is exactly the bug.
                //
                // Unbounded, and safe to be, because a covered footer is also a
                // covered KEYMAP: `key_filter`, `key_help`, `key_logs` and
                // `key_prompt` touch no selection at all (`j`/`k` in the
                // overlays scroll the overlay), so while this is held the
                // operator has no key that can move the cursor and no jump can
                // arrive behind one. The first return to Normal frees the
                // footer, and the next tick settles it — or their own first
                // cursor key cancels it, which is the outcome the design wants.
                // And if the session LEAVES the list while held, `listed` goes
                // back to `None` and the budget resumes below, so nothing is
                // ever held on a row that no longer exists.
                return;
            }
            self.pending_jump = None;
            self.flash(format!("dispatched {label} — filtered out"), MsgLevel::Warn);
            return;
        }

        // Not listed yet. Only an ANSWER is evidence; a tick whose gate was
        // shut, or whose poll failed, learned nothing and must not time the
        // intent out.
        if answered {
            let left = jump.polls_left.saturating_sub(1);
            // Dropped SILENTLY at zero. The dispatch already flashed, and it was
            // telling the truth — the agent is running either way, and only the
            // cursor move is owed. A warning arriving eight polls later would
            // land on a screen the operator has long since moved on from,
            // attached to no keypress of theirs.
            self.pending_jump = if left == 0 {
                None
            } else {
                Some(PendingJump { polls_left: left, ..jump })
            };
        }
    }

    /// Drop `n`'s cursor intent because the operator just took the cursor.
    /// Called from `key_normal`, on the keypress; see `PendingJump`.
    fn cancel_pending_jump(&mut self) {
        self.pending_jump = None;
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
            // §9.7: `claude attach` takes the 8-hex short id, so without one a
            // split would have nothing to run. Rare but reachable on a listed
            // row: `parse_sessions` honours an explicit `kind: "background"`
            // even when the CLI omitted `id`.
            self.flash("no short id — cannot open this session", MsgLevel::Warn);
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
                // §1.4: `o` built a row, `s` a column — even that axis, then
                // pin. The pin is a no-op afterwards because `even_content`
                // laid the sidebar out at the width it is about to ask for.
                self.even_content(Some(dir.shape()), Force::No);
                self.pin_sidebar();
                match self.pane_index_of(&pane) {
                    Some(i) => self.flash(format!("opened {name} in pane {i}"), MsgLevel::Info),
                    None => self.flash(format!("opened {name}"), MsgLevel::Info),
                }
            }
            Err(e) => self.flash(format!("split failed: {}", tmux_msg(&e)), MsgLevel::Error),
        }
    }

    /// `t` — open the selected session in a NEW TAB, and go there.
    ///
    /// A tab is a tmux window of ccmux's own session carrying its own pinned
    /// sidebar pane, so the list is on screen wherever the operator is. The
    /// guards mirror `act_open`'s exactly.
    ///
    /// THE ORDER IS FORCED, not stylistic. The Claude pane is created FIRST, as
    /// the new window's only pane, and the window's `@ccmux_tab_map` is written
    /// through it before the sidebar exists. Create the sidebar first and its
    /// process is already running when the map is written, so its own next
    /// flush — built from the map it loaded BEFORE that write — clobbers the
    /// entry, orphaning a Claude pane from every map forever and leaving it
    /// unclosable by `x`. This ordering makes the write provably precede the
    /// existence of any process in that window, which is what keeps
    /// `@ccmux_tab_map` a single-writer option.
    pub fn act_open_tab(&mut self) {
        if self.degraded {
            self.flash("not inside tmux — tabs unavailable", MsgLevel::Warn);
            return;
        }
        let Some(sel) = self.selected_session() else {
            return;
        };
        if !sel.is_attachable() {
            self.flash("no short id — cannot open this session", MsgLevel::Warn);
            return;
        }
        let session_id = sel.session_id.clone();
        let short_id = sel.id.clone().unwrap_or_default();
        let name = sel.name.clone();
        let Some(sidebar_cmd) = self.sidebar_cmd.clone() else {
            // Only reachable when `App` was built without `main.rs` handing the
            // command down; every real sidebar has one.
            self.flash("sidebar command unknown — cannot open a tab", MsgLevel::Error);
            return;
        };

        let attach = agents::attach_pane_cmd(&short_id);
        let (_, index, claude) = match tmux::new_tab(&self.tmux_session, &attach) {
            Ok(t) => t,
            Err(e) => {
                self.flash(format!("new tab failed: {}", tmux_msg(&e)), MsgLevel::Error);
                return;
            }
        };

        let mut seed = PaneMap::new();
        seed.insert(
            &claude,
            PaneEntry {
                session_id,
                short_id,
                name: model::truncate_end(&name, MAP_NAME_MAX),
                opened_at: self.now_ms,
            },
        );
        if let Err(e) = tmux::write_tab_map_uncached(&self.tmux_session, &claude, &seed) {
            self.flash(format!("tab map not saved: {}", tmux_msg(&e)), MsgLevel::Warn);
        }

        match tmux::split_left_of(&self.tmux_session, &claude, &sidebar_cmd) {
            Ok(sidebar) => {
                // Mark the tab HERE, synchronously, rather than leaving it to
                // the new process's first tick 30-40 ms later. Nothing about
                // this window's state is read to compute the value — it is the
                // id of the pane this call just created for the purpose — so
                // it is the same function of ground truth every other writer
                // of this key computes (§11.1), and the child's
                // `sidebar_choice` adopts it instead of re-registering.
                let _ = tmux::set_tab_sidebar(&self.tmux_session, &sidebar);
                // Pin immediately, or the tab flashes a 50/50 split until that
                // sidebar's first tick.
                tmux::pin_sidebar(&self.tmux_session, &sidebar, self.requested_width())
            }
            Err(e) => self.flash(format!("tab sidebar failed: {}", tmux_msg(&e)), MsgLevel::Warn),
        }

        // The one thing that moves the client, and it rides on the existing
        // R2-gated helper: `select_pane` issues `select-window -t <pane>` first.
        if let Err(e) = tmux::select_pane(&self.tmux_session, &claude) {
            self.flash(format!("jump failed: {}", tmux_msg(&e)), MsgLevel::Error);
        }
        self.refresh_panes();
        self.pin_sidebar();
        self.flash(format!("opened {name} in tab {index}"), MsgLevel::Info);
    }

    /// `r`'s geometry half — re-assert ccmux's own layout NOW (SPEC §1.3, §1.4).
    ///
    /// The manual counterpart to §1.3 and §1.4, and the reason it exists:
    /// neither runs unconditionally any more. The pin writes only on a width
    /// that disagrees with the target, the even pass runs only on the two
    /// verbs that made the layout uneven, and both stand down while the window
    /// is zoomed — so a window the operator has rearranged with tmux's own
    /// keys can sit in a shape ccmux will not touch, and nothing short of
    /// opening or closing a pane put it back.
    ///
    /// FORCED THROUGH BOTH GATES, INCLUDING THE ZOOM. The gates exist to stop
    /// writes nobody asked for; this keypress is the ask. Re-asserting a
    /// layout necessarily ends a zoom — `select-layout` and `resize-pane` both
    /// clear the flag — so the flash says so rather than leaving it to be
    /// discovered.
    ///
    /// EVENS BEFORE IT PINS, the same order `o` and `x` use: the layout string
    /// carries the sidebar cell at exactly the width the pin then asserts, so
    /// the pin lands on a window already the right shape instead of fighting
    /// it. A window whose panes form a tree is left alone by the even pass
    /// (§1.4) and still gets its width back.
    ///
    /// Re-enumerates first. Every other caller of these two runs inside a tick
    /// or straight after a split, where the snapshot is fresh; this one runs
    /// whenever the operator presses the key, up to a whole `interval` after
    /// the last reading, and a stale width is exactly what it is here to fix.
    ///
    /// FLASHES ONLY WHEN THE WINDOW ACTUALLY MOVED, compared as geometry
    /// before against geometry after. `r` is pressed to refresh the list far
    /// more often than to fix a layout, and a verb that announced a re-assert
    /// on every press would be announcing a no-op nearly every time — and
    /// would bury the flashes that mean something. An unchanged window says
    /// nothing, exactly as `r` always did.
    pub fn act_repin(&mut self) {
        if self.degraded {
            return;
        }
        self.refresh_panes();
        let Some(sb) = self.sidebar_pane.clone() else {
            return;
        };
        let was_zoomed = tmux::window_zoomed(&self.panes, &sb);
        let before = self.geometry();
        self.even_content(None, Force::Yes);
        self.pin_sidebar_with(Force::Yes);
        self.refresh_panes();
        if before == self.geometry() {
            return;
        }
        match tmux::pane_width(&self.panes, &sb) {
            Some(w) if was_zoomed => {
                self.flash(format!("layout re-asserted, {w} cols — zoom released"), MsgLevel::Info)
            }
            Some(w) => self.flash(format!("layout re-asserted, {w} cols"), MsgLevel::Info),
            None => self.flash("layout re-asserted", MsgLevel::Info),
        }
    }

    /// Every pane's id and cell, in a stable order — the comparison `act_repin`
    /// reports on. Ids included, so a pane appearing or leaving between the two
    /// readings counts as movement rather than passing unnoticed because the
    /// remaining cells happen to line up.
    fn geometry(&self) -> Vec<(String, u16, u16, u16, u16)> {
        let mut g: Vec<_> = self
            .panes
            .iter()
            .map(|p| (p.id.as_str().to_string(), p.left, p.top, p.width, p.height))
            .collect();
        g.sort();
        g
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
            self.flash("no short id — cannot open this session", MsgLevel::Warn);
            return;
        }
        let session_id = sel.session_id.clone();
        // FRESH EVIDENCE FIRST. `open` is rebuilt by `tick`, and a keypress is
        // not a tick: between the two, `Ctrl+Z` in a pane can retire its
        // mapping without this process hearing about it for a whole
        // `tick_interval` (2.5 s by default, up to 10 s on the idle ladder).
        // Jumping on that stale answer drops the operator back into the shell
        // they just left and calls it the session — the one outcome this verb
        // exists to avoid. `finish_restart` re-reads for the same reason, and
        // `act_open` already pays for a refresh on the other side of the
        // branch, so this costs one `list-panes` on a verb the operator asked
        // for, never on a tick.
        self.refresh_panes();
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
        // §8.5's interactive refusal is GONE, not relaxed — the spec no
        // longer carries the step at all.
        // PROBE-FINDINGS §3 proves `kill-pane` leaves the agent running for
        // BACKGROUND sessions, which are daemon-owned — and `apply_poll` now
        // lists nothing else, so the refusal had no row left to fire on. An
        // interactive session, which IS a descendant of its pane's pid
        // (PROBE §4), can no longer be selected here at all.
        let session_id = sel.session_id.clone();
        // `pane_of_any`, not `pane_of`: a pane whose attach has exited into a
        // shell is still a pane ccmux opened and still the operator's to close
        // from here, even though nothing about the row says "open" any more.
        let Some(pane) = self.pane_of_any(&session_id) else {
            self.flash("not open", MsgLevel::Warn);
            return;
        };
        // Read the index and the tab before the kill; afterwards the pane is
        // gone.
        let idx = self.pane_index_of(&pane);
        let where_ = self.tab_suffix(&pane);
        match self.kill_owned_pane(&pane) {
            Ok(killed_here) => {
                self.settle_after_kill(killed_here);
                // §8.5's last step: this wording is the operator-facing
                // statement of PROBE-FINDINGS §3, shown every time, so nobody
                // confuses `x` with `Ctrl+X`.
                //
                // ONE WORDING, and deliberately. A version that said "— shell"
                // for a detached pane read `PaneInfo::detached` out of an
                // inventory `tick` refreshes and a keypress does not, so it
                // announced the wrong thing for up to a `tick_interval` after
                // a `Ctrl+Z` — and `x` has no other reason to re-read tmux.
                // The sentence below is true either way: the agent is a
                // background one, daemon-owned, and closing its pane never
                // touched it (§8.5).
                match idx {
                    Some(i) => self.flash(
                        format!("closed pane {i}{where_} — agent still running"),
                        MsgLevel::Info,
                    ),
                    None => self.flash(
                        format!("closed pane{where_} — agent still running"),
                        MsgLevel::Info,
                    ),
                }
            }
            Err(KillRefused::Sidebar) => {
                self.flash("refusing to close the sidebar", MsgLevel::Warn);
            }
            Err(KillRefused::Tmux(e)) => {
                self.flash(format!("close failed: {}", tmux_msg(&e)), MsgLevel::Error);
            }
        }
    }

    /// THE kill. `x` and `Ctrl+X`'s second press both close panes through
    /// here and nowhere else, so every rule a kill carries is written once:
    ///
    /// - a sidebar is never a target — with a sidebar per tab, "the sidebar"
    ///   is no longer one pane, and closing another tab's would leave that
    ///   tab blind until the next launch;
    /// - the kill itself is R2-gated inside `tmux::kill_pane` (through the
    ///   `kill_pane` seam), `assert_in_session` on a validated `PaneId`;
    /// - a pane in ANOTHER tab is killed but NOT unmapped from here: that
    ///   window's option has exactly one writer and it is not this process.
    ///   Its owner reconciles the entry away on its next tick, and until then
    ///   every process already hides it, because `open` is rebuilt as
    ///   `union(maps) ∩ live panes`.
    ///
    /// Returns whether the pane was in the sidebar's own window — the one
    /// fact `settle_after_kill` needs and cannot re-derive, because by then
    /// the pane is gone from the inventory. §1.4: only the window that
    /// actually lost a pane is uneven, and only its own sidebar process lays
    /// it out — evening mine on someone else's kill would snap a layout my
    /// operator may have dragged by hand, which is the same fight the tick is
    /// kept out of.
    ///
    /// SAFE with respect to Claude sessions: PROBE-FINDINGS §3 proves the
    /// agent survives a `kill-pane`, because every session ccmux lists is a
    /// daemon-owned background one.
    fn kill_owned_pane(&mut self, pane: &PaneId) -> Result<bool, KillRefused> {
        if self.is_any_sidebar(pane) {
            return Err(KillRefused::Sidebar);
        }
        let killed_here = self.sidebar_window().is_some()
            && tmux::window_of(&self.panes, pane) == self.sidebar_window();
        (self.kill_pane)(&self.tmux_session, pane).map_err(KillRefused::Tmux)?;
        if self.map.get(pane).is_some() {
            self.map.remove(pane);
            self.map_dirty = true;
        }
        Ok(killed_here)
    }

    /// What follows a kill — or a run of them — exactly once: re-inventory,
    /// flush my window's map, even my window if it is the one that lost a
    /// pane, and re-pin the sidebar. `killed_here` is the OR of every
    /// `kill_owned_pane` answer this settle is for. A kill leaves the
    /// survivors uneven; no `SplitDir` is in play, so whatever clean shape
    /// they still form is the axis.
    ///
    /// Callers flash AFTER this, not before: `save_map_now` has a flash of
    /// its own on failure, and the verb's own message must be the one left
    /// standing.
    fn settle_after_kill(&mut self, killed_here: bool) {
        self.refresh_panes();
        self.save_map_now();
        if killed_here {
            self.even_content(None, Force::No);
        }
        self.pin_sidebar();
    }

    /// `Ctrl+X`'s second press, after `claude rm` returned Ok: close every
    /// pane ccmux has `session_id` open in — in every tab, and including one
    /// the wrapper has already parked (`@ccmux_detached`) for this session,
    /// which is the exact pane that used to be left behind. Returns how many
    /// closed.
    ///
    /// AFTER the rm, never before. `rm` is what makes the attach exit, so by
    /// the time it returns the pane is parked and killing the wrapper is
    /// clean; and if `rm` refuses, nothing here runs and the pane keeps
    /// showing a session that still exists, refusal text and all.
    ///
    /// A pane this cannot kill is simply left where it is, on screen, for
    /// `x`; a sidebar is skipped and never counted, and so is a pane the
    /// operator has taken to the `s` shell (`panes_showing`). Degraded mode
    /// has no panes to close and touches no tmux.
    fn close_panes_of(&mut self, session_id: &str) -> usize {
        if self.degraded {
            return 0;
        }
        // The inventory can be a `tick_interval` old: this runs from the
        // event-loop tick, not from a keypress that just refreshed.
        self.refresh_panes();
        let targets = self.panes_showing(session_id);
        let mut closed = 0usize;
        let mut killed_here = false;
        for pane in &targets {
            if let Ok(here) = self.kill_owned_pane(pane) {
                closed += 1;
                killed_here |= here;
            }
        }
        if closed > 0 {
            self.settle_after_kill(killed_here);
        }
        closed
    }

    /// Every live pane mapped to `session_id`, across every tab, ascending,
    /// minus any the operator has taken to a shell. Double-attach is legal
    /// (PROBE-FINDINGS §3), so this may be several.
    ///
    /// Read the way `rebuild_open` reads: my window's map is `self.map`, the
    /// record this process writes, and the copy under my own `TabInfo` is a
    /// stored shadow that may lag it, so that one is skipped; every other
    /// tab's map is read from its window. Intersected with the live inventory
    /// when there is one, exactly as `open` is — a pane a map still names but
    /// tmux no longer has is not a target, and R2 would only refuse it.
    ///
    /// A SHELL PANE IS NOT A TARGET. The map still names it — `reconcile`
    /// keeps an entry while its pane lives, and `x` still closes it — but the
    /// wrapper re-wrote its latch as `shell` when the operator answered `s`
    /// (`PaneInfo::shell`), and that is the one fact this verb needs: the
    /// kill exists for a prompt whose resume can no longer work, and a shell
    /// is not a prompt. Measured before the check: a `sleep 600` started from
    /// the `s` shell died with the pane on the session's delete, and so did a
    /// pane hand-attached to a DIFFERENT session from that shell. A PARKED
    /// pane (`detached` without `shell`) stays a target — it is the pane the
    /// verb exists for. With no inventory nothing can be told apart and the
    /// map is taken as read; `close_panes_of` refreshes first, and a refresh
    /// that failed leaves R2 refusing every kill anyway.
    fn panes_showing(&self, session_id: &str) -> Vec<PaneId> {
        let mine = self.own_window.as_ref();
        let mut out = self.map.panes_for_session(session_id);
        for tab in &self.tabs {
            if Some(&tab.window) == mine {
                continue;
            }
            out.extend(tab.map.panes_for_session(session_id));
        }
        out.sort_by_key(PaneId::num);
        out.dedup();
        if !self.panes.is_empty() {
            out.retain(|p| self.panes.iter().any(|i| &i.id == p && !i.shell));
        }
        out
    }

    // ── `Ctrl+X` — stop, and again inside the window to delete (§8.2) ───────

    /// THE `Ctrl+X` entry point. First press stops the selected session with no
    /// modal; a second press inside `CX_WINDOW` deletes it and its worktree.
    ///
    /// Every press stamps `cx_last_press` before anything else can return, so
    /// the burst / auto-repeat guard can never be skipped by an early exit —
    /// and the press stamps it AGAIN on the way out, which is the half that
    /// makes the guard measure what it claims to. See the re-stamp below.
    pub fn act_ctrl_x(&mut self) -> Action {
        let now = Instant::now();
        let gap = self
            .cx_last_press
            .map(|t| now.saturating_duration_since(t))
            .unwrap_or(CX_MIN_GAP);
        self.cx_last_press = Some(now);

        if gap < CX_MIN_GAP {
            // Buffered burst, or auto-repeat. It acts on nothing, and it
            // CANCELS a settling delete: a second chord this soon after the one
            // that scheduled it is a repeat stream, not a human pressing twice.
            // The arm itself survives, so a human who merely tapped too fast
            // still has their window.
            self.pending_delete = None;
            // Say so. Silence here is indistinguishable from a wedged sidebar:
            // the press acts on nothing and — before this — asked for no redraw
            // either, so the screen kept whatever stale line was on it. While
            // the window is open `arm_hint` outranks this in the footer, so the
            // warning is never displaced; it is read exactly when there is no
            // warning to read, which is the case that looked broken.
            self.flash("too fast — press Ctrl+X again", MsgLevel::Warn);
            return Action::Redraw;
        }

        // A lapsed arm is dropped before anything can read it, so the press
        // below is a first press again — "stops it again", never "deletes it".
        if self
            .stop_arm
            .as_ref()
            .is_some_and(|a| now.saturating_duration_since(a.at) > CX_WINDOW)
        {
            self.stop_arm = None;
        }

        match self.stop_arm.take() {
            Some(arm) => self.arm_second_press(arm),
            None => self.stop_and_arm(),
        }
        // LOAD-BEARING. `stop_and_arm` shells out to `claude stop` and then to
        // `claude agents --json`, and that blocks the whole UI for the better
        // part of a second — measured 0.66 s + 0.19 s. Stamping only on entry
        // measured the gap between the times two presses were DEQUEUED, not
        // between the keystrokes: a second `Ctrl+X` pressed during the freeze,
        // before any frame carrying the warning had ever been drawn, was read
        // with a gap of ~1.1 s, sailed past `CX_MIN_GAP` and deleted the
        // session and its worktree. Re-stamping here restarts the clock when
        // the UI became responsive again, so anything the tty buffered while
        // it was frozen reads as the burst it is.
        self.cx_last_press = Some(Instant::now());
        Action::Redraw
    }

    /// First press: stop the selection and open the delete window.
    fn stop_and_arm(&mut self) {
        let Some(sel) = self.selected_session() else {
            self.flash("no session selected", MsgLevel::Warn);
            return;
        };
        if !sel.is_attachable() {
            // §9.7's wording, unchanged by the move off `S`.
            self.flash("no short id — cannot stop this session", MsgLevel::Warn);
            return;
        }
        let session_id = sel.session_id.clone();
        let short_id = sel.id.clone().unwrap_or_default();
        let name = sel.name.clone();
        let label = session_label(sel);
        let done = sel.group() == Group::Completed;

        if done {
            // Nothing to stop — and refusing here would strand the session that
            // the PREVIOUS press stopped, which is Completed by the time the
            // window lapses. `claude rm` is documented to work on already-exited
            // sessions (PROBE-FINDINGS §2); this is the press that offers it.
            self.flash(format!("{label} is already stopped"), MsgLevel::Info);
            self.stop_arm = Some(StopArm {
                session_id,
                short_id,
                name,
                at: Instant::now(),
            });
            return;
        }

        match agents::stop(&short_id) {
            Ok(()) => {
                self.act_force_refresh();
                // Stamped AFTER the shell-out: the window the footer promises
                // must be two seconds of the operator's time, not two seconds
                // minus however long `claude stop` took.
                self.stop_arm = Some(StopArm {
                    session_id,
                    short_id,
                    name,
                    at: Instant::now(),
                });
                // The pane, if any, is left open — closing it is a separate `x`.
                self.flash(format!("stopped {label}"), MsgLevel::Info);
            }
            // A stop that failed must NOT arm: the escalation is only ever an
            // escalation of a stop that happened.
            Err(e) => self.flash(format!("stop failed: {}", agents_msg(&e)), MsgLevel::Error),
        }
    }

    /// Second press inside the window. Schedules the delete; it does not run it.
    fn arm_second_press(&mut self, arm: StopArm) {
        // The cursor must still be on the row the first press stopped. It is
        // the only check that reads the live selection at all — and it reads it
        // to REFUSE, never to retarget: what would be deleted is `arm`.
        //
        // Moving off the row cancels the window rather than re-arming on the
        // new row. Re-arming would stop whatever is now selected, and the row
        // most likely to be selected is the neighbour the cursor fell to when
        // the just-stopped session moved into Completed and `a` had that group
        // hidden. Two deliberate presses would then stop an innocent agent.
        // Cancelling costs one keypress and can stop nothing.
        let same = self
            .selected_session()
            .is_some_and(|s| s.session_id == arm.session_id);
        if !same {
            let label = model::truncate_end(&arm.name, LABEL_MAX);
            // Two different things bring us here and the operator can only act
            // on one of them. Either the cursor MOVED off a row that is still
            // on screen — `k`, or `Tab` — or the row itself LEFT the list while
            // the cursor stood still: `a` hides Completed, or a `/` filter
            // matches the worktree path that `claude stop` just reverted to the
            // parent directory, and the forced refresh drops the row. Saying
            // "moved off" for the second case blames the operator for something
            // the list did, and hides the recovery, which is to clear the
            // filter (or press `a`) and press `Ctrl+X` twice on the stopped
            // row — a first press there arms without stopping anything.
            let msg = if self.is_visible(&arm.session_id) {
                format!("moved off {label} — nothing deleted")
            } else {
                format!("{label} left the list — nothing deleted")
            };
            self.flash(msg, MsgLevel::Warn);
            return;
        }
        self.pending_delete = Some(PendingDelete {
            target: arm,
            at: Instant::now(),
        });
    }

    /// Expire a lapsed window and run a settled delete. Called from the event
    /// loop every iteration, next to `check_message_timeout`; returns true when
    /// the caller should redraw.
    ///
    /// The delete runs HERE and not in the keypress so that a repeat stream has
    /// its chance to cancel it (`CX_SETTLE`), and so the footer stops promising
    /// a window that has closed even when no key is ever pressed again.
    pub fn tick_stop_arm(&mut self) -> bool {
        let mut redraw = false;
        if self
            .stop_arm
            .as_ref()
            .is_some_and(|a| a.at.elapsed() > CX_WINDOW)
        {
            self.stop_arm = None;
            redraw = true;
        }
        if self
            .pending_delete
            .as_ref()
            .is_some_and(|p| p.at.elapsed() >= CX_SETTLE)
        {
            if let Some(p) = self.pending_delete.take() {
                self.run_delete(p.target);
            }
            redraw = true;
        }
        redraw
    }

    /// The ONLY caller of `agents::delete`. SPEC §8.2.
    fn run_delete(&mut self, arm: StopArm) {
        // Nothing settled may fire into a mode that is not the one it was
        // scheduled from, or into a process on its way out.
        if self.mode != Mode::Normal || self.should_quit {
            return;
        }
        let label = model::truncate_end(&arm.name, LABEL_MAX);
        // Fail closed: re-validate the CAPTURED id against the current poll. A
        // poll can land between the press and the settle.
        let still_there = self
            .sessions
            .iter()
            .any(|s| s.id.as_deref() == Some(arm.short_id.as_str()));
        if !still_there {
            self.flash(
                format!("session {} is gone — not deleted", arm.short_id),
                MsgLevel::Warn,
            );
            return;
        }
        match agents::delete(&arm.short_id) {
            Ok(()) => {
                // The session is gone, so every pane showing it has nothing
                // left to resume: its wrapper has parked on a prompt whose
                // `enter` cannot work. Close them — all of them, across tabs.
                // A STOP leaves its pane parked on purpose (resume is valid
                // there); only a delete gets this far.
                let closed = self.close_panes_of(&arm.session_id);
                let mut msg = format!("deleted {label} + worktree");
                match closed {
                    0 => {}
                    1 => msg.push_str(" · closed 1 pane"),
                    n => msg.push_str(&format!(" · closed {n} panes")),
                }
                self.flash(msg, MsgLevel::Warn);
                self.act_force_refresh();
            }
            Err(e) => self.flash(
                format!("delete failed: {}", agents_msg(&e)),
                MsgLevel::Error,
            ),
        }
        // Same re-stamp, same reason as `act_ctrl_x`'s, for the other blocking
        // shell-outs on this path: `claude rm`, the pane kills and the forced
        // refresh freeze the UI for at least as long as `claude stop` does —
        // which is why this stays the LAST thing here — and this one runs
        // from the event-loop tick where no keypress stamped anything at all.
        // Without it a `Ctrl+X` buffered during the freeze dequeues with a
        // stale gap, reads as a fresh FIRST press, and stops whatever row the
        // cursor fell to when the deleted session left the list.
        self.cx_last_press = Some(Instant::now());
    }

    /// Close the delete window and drop anything settling in it. Called on
    /// every departure from Normal mode, on `q`, and on `Esc`.
    pub fn disarm_ctrl_x(&mut self) {
        self.stop_arm = None;
        self.pending_delete = None;
    }

    /// The footer line while the window is open — `None` when it is not.
    ///
    /// It names the session because the cursor is free to move while the window
    /// is open, and it says what delete TAKES because nothing undoes it: `u`
    /// undoes a dismissal, never this.
    pub fn arm_hint(&self) -> Option<String> {
        let arm = self.stop_arm.as_ref()?;
        let label = model::truncate_end(&arm.name, LABEL_MAX);
        Some(format!(
            "Ctrl+X again: delete {label} and its worktree — cannot be undone"
        ))
    }

    /// `d` — dismiss the selected session FROM THE LIST.
    ///
    /// Non-destructive by construction: it adds one uuid to `hidden` and
    /// rebuilds the rows. No `claude stop`, no `kill-pane`, no `set-option` —
    /// the persistence write is deferred to the next `tick`, so the keypress
    /// path issues no tmux command whatsoever. The agent runs on, its pane (if
    /// any) stays open, and `total` in the header still counts it.
    ///
    /// No confirmation on purpose: guarding a change of what is merely on
    /// screen would teach the operator to answer prompts reflexively, which is
    /// how a real destructive prompt gets confirmed by accident. `u` is the
    /// safety net instead, and the flash names it.
    pub fn act_dismiss(&mut self) {
        let Some(sel) = self.selected_session() else {
            self.flash("no session selected", MsgLevel::Warn);
            return;
        };
        let id = sel.session_id.clone();
        let label = session_label(sel);
        // The row is the ONLY place `x` and `Enter` can be reached from, so
        // hiding a session ccmux opened a pane for leaves that pane on screen
        // with no affordance left to close or jump to it. Not killing it is
        // correct — `d` touches nothing outside this view — but saying nothing
        // is not, so this variant spends the name to buy the warning and stays
        // inside a 34-column footer. `@ccmux_map` keeps its entry on purpose:
        // the pane is live, and `u` + `x` is the recovery.
        let owns_pane = self.map.pane_for_session(&id).is_some();
        // Where the cursor lands, decided BEFORE the row goes away: the next
        // session below, else the one above. `reanchor_selection` re-finds it
        // by key, so it is right even though the rebuild renumbers every row —
        // and when there is no neighbour at all, `selected_key = None` parks
        // `selected` at `rows.len()` per the field contract.
        let neighbour = self.neighbour_key(self.selected);
        if !self.hidden.dismiss(&id) {
            return;
        }
        // The durable half: one op appended to MY fragment. Still no tmux
        // command — the write is deferred to the next `tick` exactly as before,
        // which is what keeps this keypath free of `assert_in_session`'s
        // `list-panes` and the unit suite hermetic.
        let (seq, org) = (self.next_seq(), self.own_org());
        self.hidden_log.push(HiddenOp { id: id.clone(), add: true, seq, org });
        // A fresh dismissal starts with zero strikes against it. Without this
        // an id could carry a strike across `d` -> absent poll -> `u` -> `d`:
        // `reconcile_hidden` only recomputes the strike set from `hidden` on a
        // COMPLETE poll, so one lossy poll in between leaves the old strike
        // standing and the re-dismissed id would be dropped after a single
        // absence rather than two.
        self.hidden_absent.remove(&id);
        self.selected_key = neighbour;
        self.hidden_dirty = true;
        self.rebuild_rows();
        if owns_pane {
            self.flash("hidden — pane open, u to undo", MsgLevel::Warn);
        } else {
            self.flash(format!("hidden {label} — u to undo"), MsgLevel::Info);
        }
    }

    /// `u` — undo the most recent dismissal.
    ///
    /// Mandatory, not a nicety: `d` is one keypress and makes a row vanish.
    /// The cursor follows the restored session WHEN IT IS ON SCREEN, so the
    /// undo is visible even when polls re-sorted the list in between.
    ///
    /// When `/` or `a` still hides the restored row there is nothing to follow,
    /// and pointing `selected_key` at a key `reanchor_selection` cannot find
    /// would hand the cursor to `reanchor_selection`'s nearest-index fallback —
    /// i.e. park it on an unrelated session, and leave it there after the
    /// filter is cleared. So the rebuild runs on the CURRENT key first (undo
    /// only ADDS rows, so that key is always still there), and the cursor moves
    /// only once the restored row is known to exist.
    pub fn act_undo_dismiss(&mut self) {
        let Some(id) = self.hidden.undo() else {
            self.flash("nothing to undo", MsgLevel::Info);
            return;
        };
        // A tombstone, not a deletion: the dismissal it undoes may live in
        // another tab's fragment, which this process must never write. Its
        // stamp is strictly greater than the dismissal's — guaranteed, not
        // hoped, because that dismissal came out of the fold `seq_seen` was
        // just computed from.
        let (seq, org) = (self.next_seq(), self.own_org());
        self.hidden_log.push(HiddenOp { id: id.clone(), add: false, seq, org });
        self.hidden_dirty = true;
        self.rebuild_rows();
        let visible = self.is_visible(&id);
        if visible {
            self.selected_key = Some(id.clone());
            self.reanchor_selection();
        }

        let label = self
            .sessions
            .iter()
            .find(|s| s.session_id == id)
            .map(session_label)
            // Dismissed, then it ended and left the poll. Un-hiding is still
            // right (the next reconcile drops the id), but the name is no
            // longer ours to state, so name it by its id head instead.
            .unwrap_or_else(|| short_id_of(&id));
        // `/` or `a` may have changed since the dismissal, in which case the
        // row is back in the model but still off screen. Saying so is the
        // difference between "undo worked" and "u does nothing".
        if visible {
            self.flash(format!("restored {label}"), MsgLevel::Info);
        } else {
            self.flash(format!("restored {label} — filtered out"), MsgLevel::Warn);
        }
    }

    /// `L` — the on-demand, ANSI-stripped logs overlay. SPEC §6.7.
    pub fn act_open_logs(&mut self) {
        let Some(sel) = self.selected_session() else {
            return;
        };
        let Some(id) = sel.id.clone() else {
            self.flash("no short id — no logs for this session", MsgLevel::Warn);
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

    /// `r` — SPEC §4.2: backdate `last_poll` so the NEXT loop iteration ticks.
    /// Deliberately does not call `tick()` inline.
    ///
    /// `force_poll` is what carries it through the gate and the idle ladder.
    /// Backdating alone would only buy a TICK, and a tick whose gate is shut
    /// or whose ladder has not elapsed spawns nothing — so `r` in a background
    /// tab, and `r` on a fleet that has been still for a minute, would both
    /// have become no-ops. Every post-verb refresh routes through here too,
    /// which is what makes a `Ctrl+X` stop show up on the next frame.
    pub fn act_force_refresh(&mut self) {
        self.force_poll = true;
        self.last_poll = Instant::now()
            .checked_sub(self.tick_interval())
            .unwrap_or_else(Instant::now);
    }

    /// `R` — restart the ccmux binaries in place (SPEC §8.11).
    ///
    /// WHAT IT RESTARTS: every ccmux process in the session — this sidebar,
    /// every other tab's sidebar, and every Claude pane ccmux opened — and the
    /// AGENTS behind those panes that are not busy (`restart_agents`). The
    /// SESSION, not this tab, because `R` exists for the moment after
    /// `cargo install`, and the point of that moment is that nothing is left
    /// running the old binary. A per-tab `R` would leave the other tabs on the
    /// replaced image and give the operator no way to reach them: this process
    /// cannot `exec` another process, so a respawn is the only mechanism there
    /// is, and it works across tabs exactly as well as within one.
    ///
    /// The agents are the population `R` used to miss entirely, and missing
    /// them made the verb a half-truth: a daemon-owned worker keeps the
    /// `claude` it was launched with for its whole life, so restarting the
    /// attach CLIENT changed nothing about the version doing the work.
    ///
    /// WHAT IT DOES NOT RESTART: anything ccmux cannot prove it created. See
    /// `restart::plan` — the rule lives there, pure and tested, because it is
    /// the one part of this verb that is not recoverable if it is wrong.
    ///
    /// NO CONFIRMATION, NO ARM. `Ctrl+X` has one because its second press
    /// deletes a session and its git worktree; there is no undo for that.
    /// `R` kills only attach clients, sidebars whose entire state is in tmux
    /// window options, and — since the agent pass — workers that are provably
    /// not working, and it puts every one of them straight back. `claude stop`
    /// is the RECOVERABLE verb: the conversation is kept and the attach resumes
    /// it (PROBE-FINDINGS §2), which is what makes an unconfirmed `R` still the
    /// right call. The worst an accidental one costs is the panes' scrollback
    /// and a few seconds. An arm on a recoverable verb is a tax on the common
    /// case, and the confirm modal it would revive was deleted outright.
    ///
    /// What keeps that true is `restart_agents` refusing to touch a Working or
    /// Blocked session. `R` must stay safe to press at any moment, including
    /// the moment eleven agents are mid-edit, and it buys that by leaving a
    /// busy agent on its old binary until it finishes — the trade the operator
    /// chose, stated in the footer as `N busy`.
    ///
    /// NOTHING IS KILLED HERE. `R` resolves the new binary, proves it runs, and
    /// hands this process's image over to it; every OTHER pane is respawned by
    /// the image that comes up, in `finish_restart`. That ordering is the whole
    /// safety argument, and it was learned the hard way: respawning first meant
    /// a binary that passed a file-mode pre-check and then failed at `execve`
    /// had already killed another tab's sidebar — the pane closed, the layout
    /// collapsed, and the only survivor could say nothing but "restart failed".
    /// A `respawn-pane -k` has no undo, so it must never be spent on a binary
    /// that has not yet proven it can run. Here, a refusal costs a flash.
    pub fn act_restart(&mut self) -> Action {
        if self.degraded {
            self.flash("not inside tmux — restart unavailable", MsgLevel::Warn);
            return Action::Redraw;
        }
        // Re-resolved HERE, not inherited: `sidebar_cmd` was built at launch
        // from a `current_exe()` that now names a deleted inode. See
        // `restart::exe_path`.
        let Some(exe) = restart::exe_path() else {
            self.flash("cannot find the ccmux binary — not restarting", MsgLevel::Error);
            return Action::Redraw;
        };
        // RUN it, do not merely stat it: an execute bit is not an `execve`.
        if let Err(why) = (self.probe)(&exe) {
            self.flash(format!("{why} — not restarting"), MsgLevel::Error);
            return Action::Redraw;
        }
        self.pending_restart = Some(restart::Pending { exe });
        Action::Restart
    }

    /// The second half of `R`, run by the image the first half `exec`d into —
    /// and only by it (`restart::is_handoff`).
    ///
    /// THIS PROCESS IS THE PROOF. It is the new binary, it started, it is
    /// about to draw; that is the licence to kill another tab's sidebar pane
    /// and to stop another session's agent, and nothing weaker is. The panes it restarts are exactly the panes
    /// `restart::plan` says ccmux created — an unmanaged pane in ccmux's own
    /// window is never one of them.
    ///
    /// The evidence is re-read first, in this fresh process, so the plan is
    /// built from what tmux says NOW rather than from a snapshot taken before
    /// the operator's last keypress — and if either read FAILS there is no
    /// plan to build. This image's inventory starts empty, so a failed read
    /// leaves a blank picture rather than a stale one, and every rule below
    /// reads absence as a decision. `restart_others` refuses on that.
    pub fn finish_restart(&mut self) {
        if self.degraded {
            return;
        }
        self.refresh_panes();
        self.restart_others();
    }

    /// Plan, restart the agents, respawn the panes — everything
    /// `finish_restart` does after the reads, so the rules it enforces are
    /// testable without a tmux server and without a `claude`.
    ///
    /// THE AGENTS GO FIRST, AND THE ORDER IS THE FEATURE. A pane is respawned
    /// with `claude attach <id>`, and an agent is restarted by `claude stop
    /// <id>` followed by something that resumes it. Those are not two
    /// independent passes that happen to share a session id — they are the two
    /// halves of one operation, and only one order composes:
    ///
    ///   stop, then attach : the attach IS the resume. It brings the worker
    ///                       back as a NEW process, which resolves `claude`
    ///                       at launch and therefore comes up on the binary
    ///                       installed now (PROBE-FINDINGS §2). The pane shows
    ///                       a live session, on the new version, and ccmux
    ///                       issued exactly one extra command to get it.
    ///   attach, then stop : the pane re-attaches the OLD worker, draws it,
    ///                       and is then stopped out from under itself. The
    ///                       operator is left looking at a pane whose session
    ///                       is halted, with nothing to resume it, after
    ///                       pressing a key called "restart".
    ///
    /// So every stop lands before ANY respawn. Not merely before its own pane's
    /// respawn: two panes may hold the same session (double-attach is legal,
    /// PROBE-FINDINGS §3), so "its own pane" is not always one pane, and a
    /// whole-phase boundary is the only version of this rule with no ordering
    /// left to get wrong.
    ///
    /// The pane pass is UNCONDITIONAL on what the agent pass did. A stop that
    /// failed, an agent left busy, a session that vanished — none of them may
    /// cost a pane its respawn, which is the job `R` had before agents were
    /// part of it and the one whose omission the operator would actually be
    /// stuck with.
    ///
    /// The dependency runs the OTHER way, which is why there is a third phase.
    /// Phase 1 can only report that it stopped a session; whether that was a
    /// RESTART depends on phase 2 bringing a pane back to resume it, so the
    /// counting waits until phase 2 has run and is done in `stopped`/`resumed`
    /// terms rather than at the `claude stop` call site.
    fn restart_others(&mut self) {
        // PHASE 0 — CAN THIS IMAGE SEE ANYTHING AT ALL?
        //
        // The reads happened in `finish_restart`, in a FRESH process whose
        // `panes` and `tabs` start empty, so a `list-panes` or `list-windows`
        // that errored leaves not a stale picture but a blank one. Everything
        // below reads absence as a decision: no pane in the plan means nothing
        // to respawn, and — the part that bites — no session in `Plan::held`
        // means every live agent on the machine looks like nobody's. Renaming
        // the tmux session out from under a running ccmux is enough to produce
        // that (the session-scoped reads then fail), and the fail-open reading
        // stopped and restarted the very sessions this window had open.
        //
        // So: no evidence, no plan. `R` has already restarted THIS sidebar by
        // `exec` — that much is done and cannot be undone — and the operator is
        // told plainly that the rest could not be attempted, rather than being
        // handed counts computed from a blank inventory.
        if !self.panes_fresh || !self.tabs_fresh {
            self.flash(
                "cannot read this session's panes — restarted this sidebar only",
                MsgLevel::Warn,
            );
            return;
        }

        // Built from THIS image's argv[0], which is the path `act_restart`
        // resolved and probed. `sidebar_cmd` is the launcher's string, built
        // from a `current_exe()` that a `cargo install` may since have turned
        // into a deleted inode; respawning with it leaves a pane running
        // nothing at all.
        let cmd = restart::exe_path().and_then(|exe| restart::sidebar_command(&exe));
        let tabs = self.restart_tabs();
        let plan = restart::plan(&tabs, &self.panes, self.own_pane.as_ref());

        // PHASE 1 — the agents, BOTH populations. Nothing has been respawned
        // yet, and that is the point: every stop this pass makes lands here, so
        // "every stop before any respawn" is a property of the phase boundary
        // rather than of a case analysis. The paneless restarts complete in
        // here too — they touch no pane, so they have nothing in PHASE 2 to
        // wait for, and `stopped` carries only the paned ids that are still
        // owed a resume.
        let (mut agents, stopped) = self.restart_agents(&plan);

        // PHASE 2 — the panes, exactly as before, plus a record of WHICH
        // sessions a pane actually came back holding. That record is what
        // phase 3 needs and nothing else can supply: a respawn that failed is
        // a resume that never happened.
        let mut failed = 0usize;
        let mut resumed: BTreeSet<&str> = BTreeSet::new();
        for t in &plan.targets {
            let shell_cmd = match &t.role {
                Role::Sidebar => match cmd.as_ref() {
                    Some(c) => c.clone(),
                    // No path to start a sidebar with. Counted as a failure and
                    // reported; a pane is never respawned into a guess.
                    None => {
                        failed += 1;
                        continue;
                    }
                },
                Role::Claude { short_id } => agents::attach_pane_cmd(short_id),
            };
            // R2 lives inside the helper: every one of these is an
            // `assert_in_session`-gated write on a validated `PaneId`.
            match (self.respawn)(&self.tmux_session, &t.pane, &shell_cmd) {
                Err(_) => failed += 1,
                Ok(()) => {
                    if let Role::Claude { short_id } = &t.role {
                        // ANY pane is enough: one session can sit in two panes
                        // (double-attach is legal, PROBE-FINDINGS §3), and one
                        // `claude attach` that came up is one resume.
                        resumed.insert(short_id.as_str());
                    }
                }
            }
        }

        // PHASE 3 — the verdict on phase 1, which only phase 2 can give.
        //
        // A `claude stop` that returned Ok is HALF a restart. The other half is
        // the pane coming back on `claude attach <id>`, and it can fail on its
        // own: the pane was killed between the phases, or `respawn-pane`
        // errored. Counting the stop as the restart reported an upgrade for a
        // worker that was in fact halted with nothing left to resume it — the
        // one outcome of `R` an operator has to act on, filed under the one
        // word that says no action is needed. So the tally waits for phase 2,
        // and what it did not resume gets its own word (`Agents::stranded`).
        for id in stopped {
            if resumed.contains(id) {
                agents.restarted += 1;
            } else {
                agents.stranded += 1;
            }
        }

        // +1 for this process: it has already restarted, by the better
        // mechanism, which is why it is here to do the rest.
        self.flash(
            restart::note(&restart::Report {
                sidebars: plan.sidebars + 1,
                panes: plan.claude,
                failed,
                skipped: plan.skipped(),
                agents,
            }),
            MsgLevel::Info,
        );
    }

    /// `R`'s THIRD population: the agents themselves (SPEC §8.11).
    ///
    /// A background agent keeps the `claude` it was launched with for as long
    /// as it lives, so an operator who upgrades and presses `R` used to get a
    /// session of new sidebars and new attach clients talking to workers on a
    /// version from days ago — measured on this host: of 7 live workers, 4 were
    /// on 2.1.251 and 1 on 2.1.247 while the CLI had moved to 2.1.260. This
    /// pass closes that, in two sub-passes that differ in the call each one
    /// makes and in nothing else.
    ///
    /// THE SCOPE RULE, one sentence for both: **`R` restarts agents that have
    /// a LIVE WORKER, that ccmux can bring back, and that are neither working
    /// nor blocked nor already stopped.**
    ///
    ///   * SUB-PASS A, THE PANED AGENTS (`Plan::agent_ids`) — the sessions a
    ///     pane in this window is about to re-attach. The resume is that
    ///     pane's respawned `claude attach <id>`, which PHASE 2 has not run
    ///     yet, so this sub-pass only stops and hands the ids back for PHASE 3
    ///     to settle. Liveness is not tested here and does not need to be: the
    ///     pane is respawned either way, and its `claude attach` resumes the
    ///     session whether a worker was up or not.
    ///   * SUB-PASS B, THE PANELESS AGENTS (`restart::headless_targets`) — a
    ///     live worker with NO CLIENT ANYWHERE. `claude respawn <id>` does both
    ///     halves inside the daemon and needs no pane, so this sub-pass
    ///     completes and settles its own counts. Liveness IS the scope here: a
    ///     `pid: null` session is dormant, and "restarting" it would spawn a
    ///     worker nobody asked for.
    ///
    /// The two populations are DISJOINT by construction — `Plan::held` for the
    /// panes ccmux owns in THIS tmux session, `agents::attached_ids` for every
    /// `claude attach` client on the machine — so no session is restarted twice
    /// and nothing is counted twice. The second subtraction is not a
    /// refinement: the fleet is machine-wide while every tmux record ccmux
    /// holds is scoped to its own session, so without it a session open in
    /// another ccmux workspace read as paneless and had its attach killed.
    ///
    /// WHICH STATES: `restart::agent_verdict`, the same function for both —
    /// Idle and `done` yes; Working and Blocked `busy`; `stopped` refused and
    /// not counted, because a stopped session is not running an old binary,
    /// it is not running. Applying that to only one population would let `R`
    /// revive a stopped session when it happened to have a pane and not
    /// otherwise. In sub-pass B the gate is load-bearing rather than
    /// belt-and-braces: `claude respawn` checks nothing itself, and was
    /// measured un-stopping a `stopped` session and interrupting a `working`
    /// one mid-task (PROBE-FINDINGS §2).
    ///
    /// THE STATE IS RE-READ IMMEDIATELY BEFORE EVERY ACT, in both sub-passes,
    /// and that is not belt-and-braces. `App::sessions` is up to one poll
    /// interval old at the best of times, and in the process that runs this
    /// pass it is EMPTY — `finish_restart` runs before the first `tick`.
    /// Worse, a single snapshot taken at the top of the loop would age by one
    /// `claude stop` (~0.66 s measured) for every session ahead of the current
    /// one, so the last decision of a seven-agent pass would rest on data four
    /// seconds old. One poll per stop keeps every decision within one
    /// `claude agents --json` round trip (~0.21 s measured) of the act it
    /// authorises. The window cannot be closed — the CLI has no conditional
    /// stop, and no read-then-act pair is atomic — so it is made as small as a
    /// caller can make it, and what remains is stated in the docs rather than
    /// hidden.
    ///
    /// WHAT IT RETURNS. The counts it can settle on its own — everything
    /// sub-pass B did, plus sub-pass A's `busy` and `failed`, both of which
    /// leave the agent RUNNING on its old binary — plus the paned ids it
    /// actually stopped, which it cannot settle. And `Agents::blind`, which is
    /// not a count at all: sub-pass B did not run. A stopped session is owed a
    /// resume, and for those the resume is phase 2's `claude attach`; until
    /// that has been attempted, "restarted" is not yet a fact about any of
    /// them. `restart_others` closes the books.
    ///
    /// FAIL CLOSED, AND DO NOT CASCADE. A stop or a respawn that errors is
    /// counted and the pass moves on to the next session: one unreachable
    /// agent must not cost the other eleven their upgrade. A POLL that fails
    /// ends the pass instead, and the difference is deliberate — a stop
    /// failure is about one session, while a poll failure means ccmux can no
    /// longer tell working from idle, and every stop after that would be a
    /// guess. Retrying it would also spend the whole budget on a `claude` that
    /// has already said it cannot answer. Either way the remaining agents are
    /// counted as not restarted and the footer says so.
    fn restart_agents<'p>(&self, plan: &'p restart::Plan) -> (restart::Agents, Vec<&'p str>) {
        let mut out = restart::Agents::default();
        // Stopped, and therefore OWED A RESUME BY PHASE 2. Returned rather
        // than counted: whether each of these is a `restarted` or a `stranded`
        // is a question only PHASE 2 can answer, and this function runs before
        // it. Sub-pass B's sessions are NOT in here — `claude respawn` does
        // both halves in one call, so they settle here.
        let mut stopped: Vec<&'p str> = Vec::new();
        // `checked_add` because a `Duration` this far out could overflow the
        // monotonic clock; `None` then means "no deadline", which the per-call
        // bounds still cover. ONE deadline for both sub-passes: `AGENT_BUDGET`
        // caps the pass, not each half of it.
        let deadline = Instant::now().checked_add(self.agent_budget);
        if self.restart_paned_agents(plan, deadline, &mut out, &mut stopped) {
            self.restart_headless_agents(plan, deadline, &mut out);
        }
        (out, stopped)
    }

    /// SUB-PASS A — the agents whose resume is a pane this plan is about to
    /// respawn. Stops only; PHASE 3 decides what each stop turned out to be.
    ///
    /// Returns whether the pass may CONTINUE. `false` means the fleet could
    /// not be read or the budget is gone, and neither says anything good about
    /// running sub-pass B next: the same poll would be asked the same question
    /// and the same clock would already be past.
    fn restart_paned_agents<'p>(
        &self,
        plan: &'p restart::Plan,
        deadline: Option<Instant>,
        out: &mut restart::Agents,
        stopped: &mut Vec<&'p str>,
    ) -> bool {
        let ids = plan.agent_ids();
        for (i, id) in ids.iter().enumerate() {
            let left = ids.len() - i;
            if deadline.is_some_and(|d| Instant::now() >= d) {
                out.failed += left;
                return false;
            }
            // THE RE-POLL, immediately before this one stop.
            let fresh = match (self.agents_poll)() {
                Ok(p) => p,
                Err(_) => {
                    out.failed += left;
                    return false;
                }
            };
            // A session ccmux has open in a pane but that the fleet no longer
            // lists cannot be read, so it cannot be shown to be idle. Fail
            // closed: not stopped, and counted.
            let Some(sess) = fresh.sessions.iter().find(|s| s.id.as_deref() == Some(*id)) else {
                out.failed += 1;
                continue;
            };
            match restart::agent_verdict(sess) {
                restart::Verdict::Restart => match (self.agents_stop)(id) {
                    Ok(()) => stopped.push(id),
                    Err(_) => out.failed += 1,
                },
                restart::Verdict::Busy => out.busy += 1,
                // Already stopped: no worker, nothing to bring onto the new
                // binary, and nothing to report as an exception. Its pane is
                // reported on its own account — a stopped session's attach has
                // exited, so the pane is parked and counted as `skipped`.
                restart::Verdict::NotRunning => {}
            }
        }
        true
    }

    /// SUB-PASS B — the LIVE agents NOBODY HAS OPEN, restarted here.
    ///
    /// WHERE IT SITS, AND WHY. Inside PHASE 1, after sub-pass A:
    ///
    ///   * It is still PHASE 1, so every stop this pass makes lands before any
    ///     pane is respawned, and `every_stop_lands_before_every_respawn` stays
    ///     a property of the whole pass rather than of one half of it. These
    ///     sessions touch no pane, so nothing here has an ordering relationship
    ///     with PHASE 2 in either direction; being inside the earlier phase
    ///     costs nothing and keeps the invariant with no case analysis.
    ///   * The PANED stops go first because their resume is PHASE 2, which
    ///     cannot begin until PHASE 1 ends. Every second spent here before them
    ///     would be a second longer that every paned session sits halted with
    ///     no attach yet.
    ///
    /// ONE VERB, NOT A PAIR. `claude respawn <id>` halts the worker and starts
    /// the new one inside the daemon, so this sub-pass cannot leave a stop
    /// without its partner and cannot be interrupted between them. It replaced
    /// `claude stop` + `claude --bg --resume <sessionId>`, whose documented
    /// fork branch ("starts a copy and says so when the session is already
    /// running") fired inside exactly the window ccmux created by resuming
    /// immediately after its own stop — measured at roughly one resume in
    /// twenty, each one an extra live agent and a duplicated conversation
    /// reported as a success. See `agents::respawn`.
    ///
    /// WHO IS IN SCOPE, and the second half of it is why this function reads
    /// `/proc` at all. The fleet is MACHINE-WIDE; `Plan::held` is scoped to
    /// ccmux's own tmux session (R2, R3). Subtracting only `held` therefore
    /// classified as "paneless" every session held by a second ccmux
    /// workspace, by a hand-run `claude attach`, or by any attach on another
    /// tmux server — and `claude respawn` KILLS that client, leaving the
    /// operator's other window parked on a dead session while this footer
    /// counted it `restarted`. `agents::attached_ids` answers the question from
    /// the process that actually dies.
    ///
    /// A SCAN THAT COULD NOT RUN IS NOT AN EMPTY SCAN. `None` skips the whole
    /// sub-pass and sets `Agents::blind`, because the alternative is the
    /// fail-open reading — absent evidence authorising the act — and here that
    /// act is stopping every attach client on the machine.
    ///
    /// THE ENUMERATION POLL. Sub-pass A knows its ids from the plan; this one
    /// cannot — a session with no pane is by definition not in any tmux record,
    /// so the fleet itself is the only place the population exists. One poll
    /// answers that, and then the per-target re-poll runs as usual, so the rule
    /// "the state is re-read immediately before every act" holds here exactly
    /// as it does there. If that first poll fails, nothing is counted: unlike
    /// sub-pass A, ccmux cannot name a single agent it missed, and inventing a
    /// number for the footer would be worse than silence.
    ///
    /// WHAT A FAILED RESPAWN IS COUNTED AS, and it is not decided from the
    /// error. `run_bounded` SIGKILLs the client at `RESPAWN_TIMEOUT` while the
    /// daemon carries out the restart regardless, so an `Err` says nothing
    /// about the worker — and the two outcomes it hides are the two words that
    /// must never be swapped. One poll separates them, on `has_worker` and
    /// nothing finer: NO WORKER is `left stopped`, the halt landed and nothing
    /// will resume it, which is the one outcome of `R` an operator has to act
    /// on; A WORKER is `not restarted`, whose documented meaning — that agent
    /// is still running and ccmux could not change it — is true of it.
    ///
    /// The residual imprecision is deliberate and lies on the safe side: a
    /// respawn that fully LANDED and lost only its report also shows a worker,
    /// so it reads as `not restarted` and the operator may press `R` again,
    /// which restarts an idle agent that was already fine. The reverse — a
    /// halted agent filed under a word that says it is running — is the bug
    /// this replaced, and it strands work silently. Nothing here reads `pid`'s
    /// VALUE: `has_worker` is the only accessor, exactly as `Session::pid`
    /// requires.
    fn restart_headless_agents(
        &self,
        plan: &restart::Plan,
        deadline: Option<Instant>,
        out: &mut restart::Agents,
    ) {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return;
        }
        // BEFORE THE FLEET IS EVEN READ: without this, there is no population
        // this function is entitled to compute.
        let Some(attached) = (self.agents_attached)() else {
            out.blind = true;
            return;
        };
        let Ok(fleet) = (self.agents_poll)() else {
            return;
        };
        let targets = restart::headless_targets(&fleet.sessions, &plan.held, &attached);
        for (i, t) in targets.iter().enumerate() {
            let left = targets.len() - i;
            if deadline.is_some_and(|d| Instant::now() >= d) {
                out.failed += left;
                return;
            }
            // THE RE-POLL, immediately before this one act — and it re-asks
            // every question the enumeration asked, because all of them can
            // have changed in the meantime.
            let fresh = match (self.agents_poll)() {
                Ok(p) => p,
                Err(_) => {
                    out.failed += left;
                    return;
                }
            };
            // Gone from the fleet since the enumeration: `claude rm`, or a
            // daemon that dropped it. There is no worker to restart and none
            // will be missed, so nothing is counted — unlike sub-pass A, where
            // an absent row is a session ccmux still has open in a pane.
            let Some(sess) = fresh.sessions.iter().find(|s| s.session_id == t.session_id) else {
                continue;
            };
            // A worker that exited inside the window is DORMANT now, and
            // dormant is exactly what this sub-pass must never resurrect —
            // `claude respawn` on a session with no worker STARTS one
            // (measured, PROBE-FINDINGS §2). The enumeration's `has_worker` is
            // a snapshot like every other; this is the one that authorises the
            // act.
            if !sess.has_worker() {
                continue;
            }
            match restart::agent_verdict(sess) {
                restart::Verdict::Restart => {}
                // It picked up work between the enumeration and now: running,
                // left on its old binary, and that is what `busy` means.
                // `respawn` would interrupt it mid-task (measured: `working`
                // becomes `blocked`), which is the whole reason for this gate.
                restart::Verdict::Busy => {
                    out.busy += 1;
                    continue;
                }
                // Stopped inside the window — no worker, nothing owed, and
                // `respawn` would un-stop it.
                restart::Verdict::NotRunning => continue,
            }
            if (self.agents_respawn)(&t.short_id).is_ok() {
                out.restarted += 1;
                continue;
            }
            // THE VERIFYING POLL. See the doc above: the error says nothing
            // about the worker, and the three outcomes it hides need three
            // different words.
            match (self.agents_poll)() {
                Ok(after) => match after.sessions.iter().find(|s| s.session_id == t.session_id) {
                    // Gone from the fleet: same rule as above, nothing to say.
                    None => {}
                    Some(s) if s.has_worker() => out.failed += 1,
                    Some(_) => out.stranded += 1,
                },
                // Two failures in a row, and the second is a poll: ccmux can no
                // longer tell working from halted, so it stops guessing. This
                // one gets the word that asks the operator to LOOK, because the
                // outcome it might be hiding is a halted agent; the rest are
                // `failed`, exactly as any other poll failure ends the pass.
                Err(_) => {
                    out.stranded += 1;
                    out.failed += left - 1;
                    return;
                }
            }
        }
    }

    /// The tab records `R` plans from: `list-windows` for every OTHER window,
    /// and this process's own in-memory map for THIS one.
    ///
    /// `self.map` is the authoritative copy of my window's map; the stored
    /// `@ccmux_tab_map` is a lagging shadow of it, and `self.tabs` is a
    /// snapshot of that shadow taken at some earlier tick. `act_open` inserts
    /// into `self.map`, then refreshes, and only THEN writes — so immediately
    /// after an `o` or an `s` the snapshot provably lacks the pane that was
    /// just opened. Planning from it silently skips that pane: it keeps running
    /// the `claude` binary the operator just replaced, while the footer counts
    /// it as restarted. Reading `self.map` for my own window makes the freshest
    /// record the one that decides.
    ///
    /// This does not weaken the ownership rule. Every entry in `self.map` was
    /// written by ccmux when it created the pane — it is the same evidence `x`
    /// and `Ctrl+X` already act on — and `restart::plan` still requires the
    /// pane to be live before it becomes a target.
    fn restart_tabs(&self) -> Vec<TabInfo> {
        let mut tabs = self.tabs.clone();
        let Some(win) = self.own_window.clone() else {
            return tabs;
        };
        match tabs.iter_mut().find(|t| t.window == win) {
            Some(mine) => mine.map = self.map.clone(),
            // `list-windows` failed, or has not yet carried my window. My own
            // map is still evidence about my own panes, so it is planned from
            // rather than dropped on the floor.
            None => {
                let index = self
                    .panes
                    .iter()
                    .find(|i| i.window_id == win)
                    .map(|i| i.window_index)
                    .unwrap_or(0);
                tabs.push(TabInfo {
                    window: win,
                    index,
                    sidebar: self.sidebar_pane.clone(),
                    map: self.map.clone(),
                    hidden: HiddenLog::new(),
                });
            }
        }
        tabs
    }

    /// `Enter` inside a prompt. SPEC §8.6 (§8.7, the interactive prompt, is
    /// gone along with its `c` binding).
    ///
    /// The cwd field is trimmed and `~`-expanded before any check, so the path
    /// the detail block renders (`~/x`) is also a path the operator may type.
    /// A cwd that does not exist is not rejected outright any more: when its
    /// PARENT is a directory, the first `Enter` arms creation and the second
    /// runs `std::fs::create_dir` — one level, never a tree, and never over an
    /// existing file or symlink (`create_dir` is `mkdir(2)`: EEXIST on any
    /// pre-existing entry, dangling symlinks included, so nothing is followed
    /// or overwritten). This mkdir is the ONLY filesystem write in ccmux.
    pub fn act_submit_prompt(&mut self) {
        let Some(p) = self.prompt.clone() else {
            self.mode = Mode::Normal;
            return;
        };

        match p.kind {
            PromptKind::NewBackground => {
                let task = p.fields.get(1).cloned().unwrap_or_default();
                let task = task.trim().to_string();
                if task.is_empty() {
                    self.flash("task cannot be empty", MsgLevel::Warn);
                    return; // stay in the prompt
                }
                let raw = p.fields.first().map(|s| s.trim().to_string()).unwrap_or_default();
                if raw.is_empty() {
                    // Reachable when the list is empty AND `$PWD` was unreadable
                    // at `open_prompt` — the degraded corner. Refuse; never arm.
                    self.flash("cwd cannot be empty", MsgLevel::Warn);
                    return;
                }
                let cwd = match expand_tilde(&raw, self.home.as_deref()) {
                    Ok(c) => c,
                    Err(msg) => {
                        self.disarm_create();
                        self.flash(msg, MsgLevel::Warn);
                        return;
                    }
                };
                if Path::new(&cwd).is_dir() {
                    self.dispatch_and_close(&cwd, &task, None);
                    return;
                }

                // Not a directory. Decide between refusing and offering mkdir.
                let path = Path::new(&cwd);
                let shown = model::shorten_cwd(&cwd, self.home.as_deref(), CWD_FLASH_MAX);
                // `symlink_metadata` does not follow the final component: an
                // entry that exists but is not an enterable directory — a plain
                // file, a dangling symlink, a symlink to a file — is refused,
                // never created over. (A symlink to a real directory already
                // passed `is_dir` above.)
                if std::fs::symlink_metadata(path).is_ok() {
                    self.disarm_create();
                    self.flash(format!("not a directory: {shown}"), MsgLevel::Warn);
                    return;
                }
                if !cwd.starts_with('/') {
                    // Creating relative to the SIDEBAR's cwd would materialise
                    // the directory somewhere the operator never named.
                    self.disarm_create();
                    self.flash(format!("no such directory: {shown}"), MsgLevel::Warn);
                    return;
                }
                let parent_ok = path.parent().is_some_and(Path::is_dir);
                if !parent_ok {
                    // One level only: a typo'd deep path must fail loudly, not
                    // materialise a tree.
                    let parent = path
                        .parent()
                        .map(|pp| pp.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.disarm_create();
                    self.flash(
                        format!(
                            "parent does not exist: {}",
                            model::shorten_cwd(&parent, self.home.as_deref(), CWD_FLASH_MAX)
                        ),
                        MsgLevel::Warn,
                    );
                    return;
                }
                if p.pending_create.as_deref() == Some(cwd.as_str()) {
                    // Second Enter on the SAME expanded path: create + dispatch.
                    match std::fs::create_dir(path) {
                        Ok(()) => self.dispatch_and_close(&cwd, &task, Some(shown)),
                        Err(e) => {
                            // Name the path: the operator must know WHAT was
                            // not created without re-reading the field.
                            self.disarm_create();
                            self.flash(format!("could not create {shown}: {e}"), MsgLevel::Error);
                        }
                    }
                } else {
                    if let Some(pr) = self.prompt.as_mut() {
                        pr.pending_create = Some(cwd.clone());
                    }
                    self.flash(
                        format!("{shown} does not exist — ⏎ again to create it"),
                        MsgLevel::Warn,
                    );
                }
            }
        }
    }

    /// The one exit that dispatches: `agents::dispatch_background` through the
    /// `dispatch` seam — RULE Q4 intact, the task text and cwd stay pure argv.
    /// `created` carries the `~`-shortened path when this submit mkdir'd it,
    /// so the flash names what now exists on disk even if dispatch then fails.
    fn dispatch_and_close(&mut self, cwd: &str, task: &str, created: Option<String>) {
        match (self.dispatch)(cwd, task) {
            Ok(short_id) => {
                self.prompt = None;
                self.mode = Mode::Normal;
                match created {
                    Some(c) => self.flash(format!("created {c} — dispatched background session"), MsgLevel::Info),
                    None => self.flash("dispatched background session", MsgLevel::Info),
                }
                // The CURSOR follows the new session; the PANE does not. ccmux
                // still does NOT auto-open it — the operator decides with
                // `Enter` — but it no longer makes the operator hunt for the row
                // their own keypress just created.
                //
                // ARMED here rather than acted on here, because the row does not
                // exist yet: see `PendingJump`. `short_id` is `None` when the
                // CLI's banner could not be parsed, and then this is exactly the
                // old behaviour — flash, refresh, cursor untouched.
                self.pending_jump = short_id.map(|short_id| PendingJump {
                    short_id,
                    polls_left: JUMP_POLLS,
                });
                self.act_force_refresh();
            }
            Err(e) => {
                // The directory, if this submit created it, stays — and stays
                // named, so the operator knows the mkdir half happened.
                self.disarm_create();
                match created {
                    Some(c) => self.flash(
                        format!("created {c}, but dispatch failed: {}", agents_msg(&e)),
                        MsgLevel::Error,
                    ),
                    None => self.flash(format!("dispatch failed: {}", agents_msg(&e)), MsgLevel::Error),
                }
            }
        }
    }

    /// Clears a pending mkdir offer, keeping the prompt itself.
    fn disarm_create(&mut self) {
        if let Some(pr) = self.prompt.as_mut() {
            pr.pending_create = None;
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

    /// Key of the session row after `from`, else the one before it. `None`
    /// when `from` is the only session row on screen.
    fn neighbour_key(&self, from: usize) -> Option<String> {
        let after = (from.saturating_add(1)..self.rows.len()).find(|&i| self.is_session_row(i));
        let before = || (0..from.min(self.rows.len())).rev().find(|&i| self.is_session_row(i));
        after.or_else(before).and_then(|i| self.key_at(i))
    }

    /// True when `session_id` currently has a row — i.e. no filter is hiding it.
    fn is_visible(&self, session_id: &str) -> bool {
        self.rows.iter().any(|r| match r {
            Row::Session { idx } => {
                self.sessions.get(*idx).map(|s| s.session_id.as_str()) == Some(session_id)
            }
            _ => false,
        })
    }

    /// NOT where `n`'s pending jump is cancelled, though it looks like the
    /// funnel for it. Every cursor verb goes through here only when it FOUND a
    /// row to move to: `k` on the top row and `j` on the bottom one never
    /// arrive, and those are precisely the presses a jump would surprise. The
    /// cancel is on the keypress instead — `key_normal`, `claims_the_cursor`.
    fn set_selected(&mut self, i: usize) {
        self.selected = i;
        self.selected_key = self.key_at(i);
        self.clamp_scroll();
    }

    fn rebuild_rows(&mut self) {
        self.rows = model::build_rows(
            &self.sessions,
            &self.filter,
            self.show_completed,
            self.hidden.ids(),
        );
        self.reanchor_selection();
    }

    /// SPEC §5.3 steps 1-5, also run after every ccmux-issued split/kill so the
    /// fresh `#{pane_index}` is available for the confirmation message.
    ///
    /// Two tmux reads, both session-scoped: `list-panes -s` (every window) and
    /// `list-windows` (every tab's options plus `@ccmux_width`). The second
    /// REPLACES the per-tick `show-options @ccmux_width` this used to cost, so
    /// the whole cross-tab picture is free.
    fn refresh_panes(&mut self) {
        if self.degraded {
            return;
        }
        let Ok(live) = tmux::list_panes_in_session(&self.tmux_session) else {
            // The previous inventory stays: every other reader wants the last
            // good answer. The poll gate does not, and `panes_fresh` is how it
            // is told the difference.
            self.panes_fresh = false;
            return;
        };
        self.panes = live;
        self.panes_fresh = true;
        match tmux::list_tabs(&self.tmux_session) {
            Ok((tabs, width)) => {
                self.tabs = tabs;
                self.width_opt = width;
                self.tabs_fresh = true;
            }
            // Same trade as the pane inventory: the previous tabs stay, because
            // every drawing reader wants the last good answer. `tabs_fresh` is
            // how the one reader that must not treat a stale — or, in this
            // fresh post-`exec` image, EMPTY — map as evidence of absence is
            // told the difference.
            Err(_) => self.tabs_fresh = false,
        }
        self.resolve_identity();
        self.adopt_own_state();
        self.migrate_legacy();
        // MY window's map only — the thing I write. Entries for panes in other
        // tabs are read from their own windows' copies, never reconciled here.
        if self.map.reconcile(&self.panes) {
            self.map_dirty = true;
        }
        self.adopt_orphan_fragments();
        self.refold_hidden();
        self.rebuild_open();
    }

    /// Who am I, and which pane is my window's sidebar?
    ///
    /// `$TMUX_PANE` answers the first question for free — no tmux call — and is
    /// accepted only when it appears in `list_panes_in_session`, which is the
    /// existing R2 evidence: a sidebar hand-launched inside `dev` can never
    /// claim a pane of ccmux's session, and a pane id from another tmux server
    /// cannot collide, because `degraded` already proved `$TMUX` and the target
    /// socket are the same server.
    ///
    /// The marker is adopted rather than overwritten when it names a live pane
    /// of MY window: a second, hand-launched `ccmux sidebar` in a window that
    /// already has one must not steal the marker, or it would pin and anchor
    /// against itself while the real sidebar keeps re-asserting.
    fn resolve_identity(&mut self) {
        if self
            .own_pane
            .as_ref()
            .is_none_or(|p| !self.panes.iter().any(|i| &i.id == p))
        {
            let panes = &self.panes;
            self.own_pane = std::env::var("TMUX_PANE")
                .ok()
                .and_then(|s| PaneId::parse(s.trim()))
                .filter(|p| panes.iter().any(|i| &i.id == p));
        }
        self.own_window = self
            .own_pane
            .as_ref()
            .and_then(|p| self.panes.iter().find(|i| &i.id == p))
            .map(|i| i.window_id.clone());

        let (choice, register) = self.sidebar_choice();
        if register && let Some(me) = self.own_pane.clone() {
            // Self-registration: the marker was unset, named a dead pane, or
            // named a pane in some other window. One write, once per process.
            let _ = tmux::set_tab_sidebar(&self.tmux_session, &me);
        }
        self.sidebar_pane = choice;
    }

    /// Which pane is MY window's sidebar, and does the marker need writing?
    /// Pure, so the rule is testable without a tmux server.
    pub fn sidebar_choice(&self) -> (Option<PaneId>, bool) {
        let Some(me) = self.own_pane.clone() else {
            // No own pane: keep whatever was resolved, but never trust a dead
            // one. `pin_sidebar` no-ops and `split_anchor` falls back.
            let live = self
                .sidebar_pane
                .clone()
                .filter(|p| self.panes.iter().any(|i| &i.id == p));
            return (live, false);
        };
        let recorded = self
            .own_tab()
            .and_then(|t| t.sidebar.clone())
            .filter(|p| self.pane_window(p) == self.own_window);
        match recorded {
            // Adopt, do not steal: a second `ccmux sidebar` hand-launched into
            // a window that already has one must pin and anchor against the
            // real sidebar, not against itself.
            Some(sb) => (Some(sb), false),
            None => (Some(me), true),
        }
    }

    /// This process's own `TabInfo`, if the enumeration carries it.
    fn own_tab(&self) -> Option<&TabInfo> {
        let win = self.own_window.as_ref()?;
        self.tabs.iter().find(|t| &t.window == win)
    }

    fn pane_window(&self, pane: &PaneId) -> Option<WindowId> {
        self.panes
            .iter()
            .find(|i| &i.id == pane)
            .map(|i| i.window_id.clone())
    }

    /// `WindowId::num()` of my tab — the origin stamp on every op I mint.
    fn own_org(&self) -> u64 {
        self.own_window.as_ref().map(WindowId::num).unwrap_or(0)
    }

    /// Read my window's stored map and dismissal fragment — ONCE.
    ///
    /// After this, this process is their sole author. Re-adopting the stored
    /// copy every tick would let a read taken before my own last write undo it.
    ///
    /// The flag latches ONLY once the enumeration actually carries my window.
    /// It used to latch unconditionally, so a single failed `list-windows` on
    /// the tick that first resolved identity — `refresh_panes` swallows that
    /// error and leaves `self.tabs` untouched, while `own_window` still comes
    /// back from the successful `list-panes` — left the sidebar running for its
    /// whole life on an empty map and an empty dismissal fragment, and then
    /// overwriting this window's durable `@ccmux_tab_map` / `@ccmux_tab_hidden`
    /// with those empties: dismissals back on screen, and every pane the
    /// previous process had opened orphaned from every map and unclosable by
    /// `x`. `migrate_legacy` right below already had the correct shape.
    ///
    /// Adoption MERGES rather than replaces, so an `o` or a `d` pressed in the
    /// ticks before the enumeration resolved is not thrown away by the
    /// adoption that finally succeeds. In the ordinary case both sides are
    /// empty and a merge is a replace.
    fn adopt_own_state(&mut self) {
        if self.own_state_loaded || self.own_window.is_none() {
            return;
        }
        let Some((map, log)) = self.own_tab().map(|t| (t.map.clone(), t.hidden.clone())) else {
            return; // retry once the window enumeration resolves
        };
        // Built fresh rather than merged in place, so the adopted map always
        // carries the CURRENT schema version whatever this process started
        // with. Interim entries win a tie: they name a pane this process
        // opened, and the stored copy predates it.
        let mut merged = PaneMap::new();
        merged.panes = map.panes;
        merged.panes.extend(std::mem::take(&mut self.map.panes));
        self.map = merged;
        for op in log.ops {
            if self.hidden_log.push(op) {
                self.hidden_dirty = true;
            }
        }
        self.own_state_loaded = true;
    }

    /// One-time import of a session written by a build that had no tabs.
    ///
    /// The legacy `@ccmux_map` entries for panes in MY window become my tab's
    /// map, and `@ccmux_map` is reset to the empty value — not unset, because
    /// `main.rs`'s ownership guard reads its presence to prove the session is
    /// ccmux's. The legacy `@ccmux_hidden` ids become dismissal ops with tiny
    /// stamps, so any real op outranks them, and the option is cleared to mark
    /// it consumed (`get_user_option` already reads empty as absent).
    fn migrate_legacy(&mut self) {
        if self.migrated {
            return;
        }
        let (Some(_), Some(_)) = (&self.own_pane, &self.own_window) else {
            return; // retry once identity resolves
        };
        if !self.own_state_loaded {
            // This consumes the legacy options as it reads them, and the
            // imported result only reaches tmux through a flush — which is now
            // deferred until adoption. Migrating first would let an exit in
            // between destroy the legacy value durably with nothing written in
            // its place.
            return;
        }
        self.migrated = true;

        let legacy = tmux::load_map(&self.tmux_session);
        if !legacy.panes.is_empty() {
            for (key, entry) in &legacy.panes {
                let Some(pane) = PaneId::parse(key) else { continue };
                if self.pane_window(&pane) == self.own_window && self.map.get(&pane).is_none() {
                    self.map.insert(&pane, entry.clone());
                    self.map_dirty = true;
                }
            }
            let _ = tmux::set_user_option(&self.tmux_session, tmux::OPT_MAP, tmux::EMPTY_MAP_JSON);
        }

        let legacy_hidden = tmux::load_hidden(&self.tmux_session);
        if !legacy_hidden.ids().is_empty() {
            let org = self.own_org();
            for (i, id) in legacy_hidden.ids().iter().enumerate() {
                let seq = i as u64 + 1;
                if self.hidden_log.push(HiddenOp { id: id.clone(), add: true, seq, org }) {
                    self.hidden_dirty = true;
                }
            }
            let _ = tmux::set_user_option(&self.tmux_session, tmux::OPT_HIDDEN, "");
        }
    }

    /// A window option dies with its window, so a closed tab would take every
    /// dismissal it made with it and silently un-hide those rows.
    ///
    /// The adopter — the live tab with the lowest `WindowId::num()`, where LIVE
    /// means its `@ccmux_tab_sidebar` names a pane that still exists — merges an
    /// orphaned fragment's ops into its own VERBATIM. Ops carry their own stamp
    /// and origin, so adoption changes only WHERE an op is stored and nothing
    /// about the fold: it is semantics-preserving by construction, and a race
    /// between two would-be adopters resolves to a byte-identical duplicate
    /// that `HiddenLog::push` drops.
    pub fn adopt_orphan_fragments(&mut self) {
        let live: std::collections::BTreeSet<u64> =
            self.tabs.iter().map(|t| t.window.num()).collect();
        let orphans: Vec<HiddenLog> = self
            .last_frags
            .iter()
            .filter(|(num, _)| !live.contains(num))
            .map(|(_, log)| log.clone())
            .collect();
        if !orphans.is_empty() && self.is_adopter() {
            for log in orphans {
                for op in log.ops {
                    if self.hidden_log.push(op) {
                        self.hidden_dirty = true;
                    }
                }
            }
        }
        let mine = self.own_window.clone();
        self.last_frags = self
            .tabs
            .iter()
            .filter(|t| Some(&t.window) != mine.as_ref())
            .map(|t| (t.window.num(), t.hidden.clone()))
            .collect();
    }

    /// True when this tab is the lowest-numbered one that still has a sidebar
    /// process. A sidebar pane exists exactly while its process does, so "its
    /// marker names a live pane" is the liveness test.
    fn is_adopter(&self) -> bool {
        let Some(mine) = self.own_window.as_ref() else {
            return false;
        };
        let lowest = self
            .tabs
            .iter()
            .filter(|t| {
                t.sidebar
                    .as_ref()
                    .is_some_and(|p| self.panes.iter().any(|i| &i.id == p))
            })
            .map(|t| t.window.num())
            .min();
        lowest == Some(mine.num())
    }

    /// Recompute the shared dismissed set, and garbage-collect my fragment.
    ///
    /// The fold runs over every OTHER tab's STORED fragment plus my own
    /// IN-MEMORY log — never my stored copy. I am the authority on my fragment,
    /// and my last `d` may not have been flushed yet; folding the stored copy
    /// would make a dismissal flicker back onto the screen for one frame.
    pub fn refold_hidden(&mut self) {
        let mine = self.own_window.clone();
        let others: Vec<&HiddenLog> = self
            .tabs
            .iter()
            .filter(|t| Some(&t.window) != mine.as_ref())
            .map(|t| &t.hidden)
            .collect();
        self.seq_seen = others
            .iter()
            .map(|l| l.max_seq())
            .chain(std::iter::once(self.hidden_log.max_seq()))
            .chain(std::iter::once(self.seq_seen))
            .max()
            .unwrap_or(0);
        if tmux::prune_hidden_log(&mut self.hidden_log, &others) {
            self.hidden_dirty = true;
        }
        self.hidden = tmux::fold_hidden(others.into_iter().chain(std::iter::once(&self.hidden_log)));
    }

    /// The next Lamport stamp. Monotonic per process even if the wall clock
    /// steps backwards, and always strictly greater than anything this process
    /// has observed — which is what guarantees a `u` tombstone outranks the
    /// dismissal it targets, since that dismissal came out of the fold.
    fn next_seq(&mut self) -> u64 {
        let now = self.now_ms.max(0) as u64;
        let seq = now.max(self.seq_seen.saturating_add(1));
        self.seq_seen = seq;
        seq
    }

    /// `union(every tab's map) ∩ live panes`, indexed by session id.
    ///
    /// My own in-memory map wins over the stored copy of my window, for the
    /// same reason the fold uses my in-memory log. With no pane inventory at
    /// all — degraded, or before the first refresh — the intersection is
    /// skipped, so `is_open` answers exactly what it always did.
    pub fn rebuild_open(&mut self) {
        let mine = self.own_window.clone();
        let mut union: BTreeMap<String, String> = BTreeMap::new();
        for tab in &self.tabs {
            if Some(&tab.window) == mine.as_ref() {
                continue;
            }
            for (pane, entry) in &tab.map.panes {
                union.insert(pane.clone(), entry.session_id.clone());
            }
        }
        for (pane, entry) in &self.map.panes {
            union.insert(pane.clone(), entry.session_id.clone());
        }

        let have_inventory = !self.panes.is_empty();
        let mut open: BTreeMap<String, OpenPane> = BTreeMap::new();
        for (key, session_id) in union {
            let Some(pane) = PaneId::parse(&key) else { continue };
            let info = self.panes.iter().find(|i| i.id == pane);
            if have_inventory && info.is_none() {
                continue; // dead, or in a window this session cannot see
            }
            let cand = OpenPane {
                pane,
                window_index: info.map(|i| i.window_index),
                window: info.map(|i| i.window_id.clone()),
                // ONLY THE PANE MAY CLEAR THIS. `agents::attach_pane_cmd`
                // unsets the latch on its own resume path — it is entitled to,
                // because the id it is about to re-attach is the one baked into
                // its own command string — so a pane detached and resumed in
                // place comes back here as attached on the next tick.
                //
                // ccmux never clears it from the outside, and could not. tmux
                // would report a hand-reattached pane as
                // `#{pane_current_command} == claude`, and PROBE-FINDINGS §4 is
                // exactly why that is not evidence: a pane's command does not
                // say WHICH session it is showing, so re-claiming on a process
                // name would hand this row a pane attached to another session
                // entirely, or a plain `claude` the operator started — and
                // would silently re-arm `R` on a pane the operator is driving
                // by hand. Such a pane stays the operator's; `Enter` opens a
                // fresh one, and double-attach is legal (PROBE §3).
                attached: !info.is_some_and(|i| i.detached),
            };
            // MY tab wins outright; only then does §5.5's numeric tie-break
            // decide. A session double-attached in two tabs has a pane sitting
            // beside THIS sidebar, and that is the one `Enter` must not travel
            // to, the one `x` must kill, and the one the badge must call
            // "here" — resolving it to whichever window happened to draw the
            // lower pane id made all three act on another tab's pane while the
            // badge said otherwise. With no own window (degraded, or a fixture
            // with no pane inventory) both sides are equally foreign and the
            // rule collapses to exactly the numeric one.
            let here = |o: &OpenPane| o.window.is_some() && o.window == mine;
            // ATTACHED OUTRANKS EVERYTHING, including "in my tab" and the
            // numeric tie-break. Verified live: detach from a pane, press
            // `Enter`, and the session has two mapped panes — the shell you
            // left and the fresh attach beside it. Resolving that by pane
            // number alone hands the row the shell, because it is the older
            // and therefore lower-numbered pane, and the sidebar then reports
            // a session it has just opened as not open, refuses to jump to it,
            // and points `x` at the wrong pane. A pane that is still showing
            // the session is the only honest answer to "where is this
            // session"; only when none is does a detached one represent it, so
            // that `x` can still close it.
            let rank = |o: &OpenPane| (o.attached, here(o));
            match open.get(&session_id) {
                Some(cur) => {
                    let (cur_rank, cand_rank) = (rank(cur), rank(&cand));
                    if cand_rank > cur_rank
                        || (cand_rank == cur_rank && cand.pane.num() < cur.pane.num())
                    {
                        open.insert(session_id, cand);
                    }
                }
                None => {
                    open.insert(session_id, cand);
                }
            }
        }
        self.open = open;
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

    /// `@ccmux_width` when set and sane, else this process's `--width`. Read
    /// from the `list_tabs` snapshot, so it costs no tmux call of its own.
    fn requested_width(&self) -> u16 {
        self.width_opt
            .map(|w| w.clamp(crate::WIDTH_MIN, crate::WIDTH_MAX))
            .unwrap_or(self.sidebar_width)
    }

    /// Spread the content panes of the sidebar's window evenly along one axis
    /// (SPEC §1.4). Errors are swallowed exactly as `pin_sidebar` swallows
    /// them: this is cosmetic geometry, and a window that refuses to be laid
    /// out is still a working window.
    ///
    /// CALLED ONLY FROM `act_open` AND `act_close_pane`, never from `tick`.
    /// The mouse is on, so the operator can drag a pane border at any time;
    /// evening on a timer would undo that every 2.5 seconds. Evening on the
    /// two verbs that made the layout uneven in the first place cannot.
    ///
    /// The width handed to `even_layout` is `pinned_width()` — the very number
    /// `pin_sidebar` re-asserts — so the layout this writes is a fixed point of
    /// the per-tick pin by construction, not by luck.
    fn even_content(&self, want: Option<tmux::ContentShape>, force: Force) {
        if self.degraded {
            return;
        }
        let (Some(sb), Some(cols)) = (&self.sidebar_pane, self.pinned_width()) else {
            return;
        };
        // §1.4 yields to the zoom for the same reason §1.3 does, and reads a
        // snapshot both call sites have just refreshed: a split un-zooms the
        // window before this runs, so `o`/`s` still even; a kill does not, so
        // closing one pane no longer un-zooms another. See `tmux::window_zoomed`.
        //
        // `Force::Yes` is `=` and only `=`: the gate exists to stop writes the
        // operator did not ask for, and a keypress is not one of those.
        if force == Force::No && tmux::window_zoomed(&self.panes, sb) {
            return;
        }
        let Some(win) = tmux::window_of(&self.panes, sb) else {
            return;
        };
        // Window-scoped, for the same reason `split_anchor` is: `pane_left` and
        // `pane_top` are per-window coordinates, and an unfiltered slice would
        // describe a geometry no window actually has.
        let scoped = tmux::panes_in_window(&self.panes, win);
        let Some(layout) = tmux::even_layout(&scoped, sb, cols, want) else {
            return;
        };
        let _ = tmux::apply_layout(&self.tmux_session, sb, &layout);
    }

    /// §1.3's width pin — re-asserted only when the sidebar is not already at
    /// the width it should be, and never through a zoom.
    ///
    /// THE COMPARISON IS AGAINST OBSERVED STATE, NOT AGAINST AN EVENT. §1.3
    /// forbids guarding the pin on "did the layout change", and it is right to:
    /// a change flag cannot see the operator drag a border, so the width would
    /// stop healing. `pane_width` is a fresh reading of the pane the pin is
    /// about to write, taken from the enumeration `tick` step 2 has just made,
    /// so a manual resize shows up as a mismatch on the very next tick and is
    /// healed exactly as before. What is dropped is only the `resize-pane` on
    /// the ticks that would have written the width it already had — every tick
    /// of an idle sidebar.
    ///
    /// The verb call sites read a snapshot taken before `even_content`'s
    /// `select-layout`, and that is sound in both directions: when a layout was
    /// applied it already carries the sidebar at exactly `cols` (§1.4), so a
    /// stale row that happens to match skips a pin that was a no-op anyway;
    /// when it was not, the snapshot is the fresh post-split one and the
    /// mismatch stands.
    ///
    /// A ZOOM outranks both. `resize-pane` clears `window_zoomed_flag`, so
    /// pinning through a zoom would hand the window back on the next tick; the
    /// pin resumes the moment the zoom is released. The width math is skipped
    /// with it, deliberately: a zoomed window reports the zoomed pane at the
    /// FULL window size and leaves the hidden panes on their old geometry, so
    /// `pinned_width_from` would be reading coordinates that describe no layout
    /// tmux will restore.
    fn pin_sidebar(&self) {
        self.pin_sidebar_with(Force::No);
    }

    /// `pin_sidebar`, with the two gates the tick needs and `=` does not.
    fn pin_sidebar_with(&self, force: Force) {
        if self.degraded {
            return;
        }
        let Some(sb) = self.sidebar_pane.as_ref() else {
            return;
        };
        if force == Force::No && tmux::window_zoomed(&self.panes, sb) {
            return;
        }
        let Some(cols) = self.pinned_width() else {
            return;
        };
        if force == Force::No && !Self::needs_repin(tmux::pane_width(&self.panes, sb), cols) {
            return;
        }
        tmux::pin_sidebar(&self.tmux_session, sb, cols);
    }

    /// The pin's whole decision, as a function of two numbers, so every case
    /// is reachable without a tmux.
    ///
    /// `observed` is `None` when the last enumeration does not carry the
    /// sidebar's row — a failed `list-panes` that left `panes_fresh` false, or
    /// a pane created since. That is NOT evidence the width is right, so it
    /// pins: the pre-existing behaviour, and the safe direction, because an
    /// unnecessary `resize-pane` costs one spawn while a skipped one leaves the
    /// sidebar at whatever width a manual split gave it.
    fn needs_repin(observed: Option<u16>, want: u16) -> bool {
        observed != Some(want)
    }

    /// Flush MY window's `@ccmux_tab_map`, addressed through my own pane.
    ///
    /// `map_dirty` is cleared on FAILURE too: tmux refuses a `set-option` value
    /// over ~16 KB, and a doomed write left dirty would be reissued every tick
    /// forever with nothing on screen to say the map had stopped persisting.
    /// The next map change retries once, and the operator is told.
    ///
    /// With no own pane there is no addressable target, so persistence behaves
    /// exactly as it does when degraded: in memory, dirty flag kept, nothing
    /// attempted and nothing said.
    fn save_map_now(&mut self) {
        if self.degraded || !self.map_dirty {
            return;
        }
        // Never write from a base this process has not actually read. Until
        // `adopt_own_state` succeeds, `self.map` is not this window's map — it
        // is an empty one — and flushing it would destroy the stored copy.
        // The dirty flag is KEPT, so the write happens on the tick adoption
        // lands, which merges rather than replaces.
        if !self.own_state_loaded {
            return;
        }
        let (Some(pane), Some(win)) = (self.own_pane.clone(), self.own_window.clone()) else {
            return;
        };
        match tmux::save_tab_map(&self.tmux_session, &pane, &win, &self.map) {
            Ok(()) => self.map_dirty = false,
            Err(e) => {
                self.map_dirty = false;
                self.flash(format!("pane map not saved: {}", tmux_msg(&e)), MsgLevel::Warn);
            }
        }
    }

    /// Flush `@ccmux_hidden`. Same contract as `save_map_now`, for the same
    /// reason: a value tmux refuses is not worth reissuing every tick, so the
    /// dirty flag is cleared on failure too and the operator is told once. The
    /// dismissal itself already took effect on screen — persistence failing
    /// costs it only its survival across a sidebar restart.
    fn save_hidden_now(&mut self) {
        if self.degraded || !self.hidden_dirty {
            return;
        }
        // Same rule as `save_map_now`: no write from an un-adopted base.
        if !self.own_state_loaded {
            return;
        }
        let (Some(pane), Some(win)) = (self.own_pane.clone(), self.own_window.clone()) else {
            return;
        };
        match tmux::save_tab_hidden(&self.tmux_session, &pane, &win, &self.hidden_log) {
            Ok(()) => self.hidden_dirty = false,
            Err(e) => {
                self.hidden_dirty = false;
                self.flash(
                    format!("hidden list not saved: {}", tmux_msg(&e)),
                    MsgLevel::Warn,
                );
            }
        }
    }

    /// Everything deferred that must still reach tmux before the process ends.
    ///
    /// `tick` is the only other flush site, which is exactly the problem this
    /// solves: a `d` or a `u` in the last poll interval before the operator
    /// quits has no next tick to be written by, and would silently revert on
    /// the next launch. The `u` direction is the one that stings — an undo the
    /// operator watched take effect on screen, gone, with the row hidden again
    /// and no message.
    ///
    /// Called from `main::run_sidebar` AFTER the event loop returns, so it
    /// covers `q`, `Ctrl-c` and an error return alike — no exit path can skip
    /// it by leaving the loop a different way. Deliberately NOT called from
    /// `act_quit`: the keypress path must stay free of tmux commands.
    pub fn shutdown(&mut self) {
        self.save_map_now();
        self.save_hidden_now();
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

    /// Move focus to a pane ccmux itself opened, in ANY tab. The only jump
    /// left: `pane` always comes from `pane_of`, i.e. from some tab's
    /// `@ccmux_tab_map` intersected with the live pane list, so
    /// `select_pane`'s R2 gate can never see a foreign target.
    ///
    /// Crossing tabs needs no new mechanism and gets none: `tmux::select_pane`
    /// has always issued `select-window -t <pane>` before `select-pane`, and a
    /// pane proven in-session proves the window it names is in-session too.
    /// Only the wording changes.
    fn jump_to_ccmux_pane(&mut self, pane: &PaneId) {
        let where_ = self.tab_suffix(pane);
        match tmux::select_pane(&self.tmux_session, pane) {
            Ok(()) => match self.pane_index_of(pane) {
                Some(i) => self.flash(format!("jumped to pane {i}{where_}"), MsgLevel::Info),
                None => self.flash(format!("jumped to pane {pane}{where_}"), MsgLevel::Info),
            },
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
        };
        // §8.6: the task field starts empty and is focused.
        let focus: usize = match kind {
            PromptKind::NewBackground => 1,
        };
        let cursor = fields.get(focus).map(|s| s.chars().count()).unwrap_or(0);
        self.prompt = Some(Prompt {
            kind,
            fields,
            focus,
            cursor,
            pending_create: None,
        });
        self.mode = Mode::Prompt(kind);
    }

    // ── per-mode key handlers ────────────────────────────────────────────────

    fn key_normal(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // §8.6: `n`'s pending cursor jump dies here, BEFORE the verb runs, the
        // moment the operator touches a key that is about the cursor. Before
        // the verb, because `Ctrl+X` reads the live selection: cancelling
        // afterwards would leave the very race this closes. See
        // `claims_the_cursor` and `PendingJump`.
        if claims_the_cursor(key) {
            self.cancel_pending_jump();
        }
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
            // MUST stay above the `_ if ctrl` catch-all on the next line: this
            // arm is matched top-down, and anything Ctrl placed below it is
            // silently dead. `'X'` is accepted beside `'x'` on the same
            // precedent `Ctrl-c`/`Ctrl-C` already set in `on_key` — a terminal
            // that reports the chord shifted must not find the key inert.
            KeyCode::Char('x') | KeyCode::Char('X') if ctrl => self.act_ctrl_x(),
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
            // `t` = a new TAB. Unbound until now; `c` stays deleted.
            KeyCode::Char('t') => {
                self.act_open_tab();
                Action::Redraw
            }
            KeyCode::Char('x') => {
                self.act_close_pane();
                Action::Redraw
            }
            KeyCode::Char('n') => {
                self.open_prompt(PromptKind::NewBackground);
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
            // Reachable only without Ctrl: the `Ctrl-d`/`Ctrl-u` arms above
            // match first, so half-page scrolling is untouched.
            KeyCode::Char('d') => {
                self.act_dismiss();
                Action::Redraw
            }
            KeyCode::Char('u') => {
                self.act_undo_dismiss();
                Action::Redraw
            }
            // `r` is BOTH halves of "put this back the way it should be": the
            // poll that refreshes the list, and the geometry §1.3 and §1.4 no
            // longer re-assert on a timer. Geometry first, so the frame this
            // keypress draws is already the right shape.
            //
            // The fold is HERE and not inside `act_force_refresh`, which three
            // post-verb refreshes also call. Forcing a layout from those would
            // put the timer-driven writes back — through a zoom — by the side
            // door, which is the whole thing the gates exist to stop.
            KeyCode::Char('r') => {
                self.act_repin();
                self.act_force_refresh();
                Action::Redraw
            }
            // Reachable: `R` arrives with SHIFT set, and no arm above it
            // requires empty modifiers. It sits BELOW `_ if ctrl`, which is
            // correct — `Ctrl-R` is not this binding and must stay inert.
            KeyCode::Char('R') => self.act_restart(),
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
                // Esc means cancel, and the most cancellable thing on screen is
                // an open delete window. It takes precedence over the filter:
                // one is a view, the other is a loaded verb.
                if self.stop_arm.is_some() || self.pending_delete.is_some() {
                    self.disarm_ctrl_x();
                    self.flash("delete window closed", MsgLevel::Info);
                } else if self.filter.is_empty() {
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
            // §8.6: in the cwd field `Tab` COMPLETES instead of cycling — the
            // task field stays one `BackTab` (or `Enter`) away. From any other
            // field `Tab` still cycles, which from the task field lands on the
            // cwd field in one press.
            KeyCode::Tab if p.focus == 0 => {
                // Trimmed exactly as Enter trims: a value Enter would accept
                // must also complete. On `Extend` the replacement is the
                // trimmed text (the stray whitespace dies, as it would at
                // submit); on `Stuck` nothing is written back, so a no-op Tab
                // mutates nothing.
                let cur = p.fields.first().map(|s| s.trim().to_string()).unwrap_or_default();
                match complete_dir(&cur, self.home.as_deref()) {
                    Completion::Extend(text) => {
                        if let Some(f) = p.fields.get_mut(0)
                            && *f != text
                        {
                            *f = text;
                            p.pending_create = None; // an edit, like any other
                        }
                        p.cursor = len(&p);
                    }
                    Completion::Stuck(n) if n > 1 => {
                        self.flash(format!("{n} matches"), MsgLevel::Info);
                    }
                    Completion::Stuck(_) => return Action::None,
                }
            }
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
                    p.pending_create = None; // §8.6: no mkdir offer survives an edit
                }
            }
            KeyCode::Delete => {
                // Mirror Backspace's `cursor > 0` guard: at end-of-field no
                // char is removed, so a no-op keypress must not disarm the
                // mkdir offer either.
                let at = p.cursor;
                if at < len(&p) {
                    if let Some(f) = p.fields.get_mut(p.focus) {
                        remove_char_at(f, at);
                    }
                    p.pending_create = None;
                }
            }
            KeyCode::Char(c) if !ctrl => {
                let at = p.cursor;
                if let Some(f) = p.fields.get_mut(p.focus) {
                    insert_char_at(f, at, c);
                }
                p.cursor = at + 1;
                p.pending_create = None;
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

/// Whether a geometry write is the TICK's or the OPERATOR's.
///
/// The two gates on §1.3 and §1.4 — "the width is already right" and "the
/// window is zoomed" — exist to stop ccmux writing geometry nobody asked for.
/// Neither is a reason to refuse `=`, which is the operator asking in as many
/// words, so the flag is threaded rather than the gates being softened: the
/// tick keeps every guarantee it had.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Force {
    No,
    Yes,
}

/// Does this Normal-mode key take the cursor away from `n`'s pending jump
/// (§8.6)? Two populations, and the second is the one that is easy to forget:
///
///   * THE MOVES — `j` `k` `↓` `↑` `g` `G` `Tab` `S-Tab` `Ctrl-d` `Ctrl-u`.
///     Answered on the KEY and not on the resulting move, because a move that
///     found no row still happened as far as the operator is concerned: `k` on
///     the top row and `j` on the bottom one never reach `set_selected`, and
///     `Tab` returns early when `rows` carries no headers at all.
///   * THE VERBS THAT ACT ON THE SELECTED ROW — `Enter` `o` `s` `t` `x`
///     `Ctrl-x` `L` `d` `u`. Pressing one is a decision about the row that is
///     under the cursor NOW, and a jump firing between that decision and the
///     next keypress makes ccmux act on a row the operator never chose.
///     `Ctrl+X` is the sharp end: its two presses straddle a forced poll.
///
/// NOT here, deliberately:
///   * `n` — it arms the next intent itself; a second `n` replaces the first,
///     and abandoning the prompt with `Esc` must not have cost the operator a
///     jump they were still waiting on.
///   * `r` — a manual refresh is the operator ASKING for the poll that lists
///     the new session. Cancelling on it would make the obvious way to hurry
///     the jump along the one way to lose it.
///   * `/` and `a` — view toggles, not cursor verbs. §8.6 answers those with
///     the filtered-out flash instead.
///   * `?` `q` `R` `Esc` and every unbound key — they touch no selection.
fn claims_the_cursor(key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Mirrors `key_normal`'s arm order, including its `_ if ctrl` catch-all:
    // any Ctrl chord not bound there is inert and claims nothing.
    match key.code {
        KeyCode::Char('d') | KeyCode::Char('u') if ctrl => true,
        KeyCode::Char('x') | KeyCode::Char('X') if ctrl => true,
        _ if ctrl => false,

        KeyCode::Char('j') | KeyCode::Down => true,
        KeyCode::Char('k') | KeyCode::Up => true,
        KeyCode::Char('g') | KeyCode::Char('G') => true,
        KeyCode::Tab | KeyCode::BackTab => true,

        KeyCode::Enter => true,
        KeyCode::Char('o') | KeyCode::Char('s') | KeyCode::Char('t') => true,
        KeyCode::Char('x') => true,
        KeyCode::Char('L') => true,
        KeyCode::Char('d') | KeyCode::Char('u') => true,

        _ => false,
    }
}

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

/// §8.6: input-side twin of `model::shorten_cwd`'s display rule. The detail
/// block renders `~/x`, so `~/x` must be typeable. Only a LEADING tilde is
/// path syntax: `~` and `~/...` expand against `home`; `/data/~backup` is a
/// literal file name and passes through untouched; `~user/...` is refused
/// rather than half-implemented (no passwd lookup in this crate).
fn expand_tilde(input: &str, home: Option<&str>) -> Result<String, String> {
    if input == "~" || input.starts_with("~/") {
        let Some(h) = home.filter(|h| !h.is_empty()) else {
            return Err("cannot expand ~ — home directory unknown".to_string());
        };
        let rest = input.strip_prefix('~').unwrap_or_default();
        return Ok(format!("{h}{rest}"));
    }
    if input.starts_with('~') {
        return Err("~user paths are not supported — use an absolute path".to_string());
    }
    Ok(input.to_string())
}

/// What one `Tab` in the cwd field resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Completion {
    /// Replace the field with this text (unique match completed to `.../`, or
    /// several matches extended to their longest common prefix).
    Extend(String),
    /// Nothing to extend: how many directories matched the prefix. `0` is a
    /// silent no-op (no match, unreadable dir, relative path — all degrade the
    /// same way); `>1` is worth telling the operator about.
    Stuck(usize),
}

/// §8.6: read-only prefix completion for the cwd field. Splits `input` at its
/// last `/`, `~`-expands the directory half, and matches the final component
/// against that directory's SUBDIRECTORY names (metadata queries only — one
/// `read_dir`, one `is_dir` per name that survives the prefix filter; a
/// symlink to a directory counts, exactly as dispatch would treat it).
/// Dot-directories only match when the typed prefix itself starts with `.`.
/// Every failure — no `/`, unexpandable `~`, unreadable directory, non-UTF-8
/// names — degrades to `Stuck(0)`, a no-op.
fn complete_dir(input: &str, home: Option<&str>) -> Completion {
    let Some(cut) = input.rfind('/') else {
        return Completion::Stuck(0);
    };
    let (head, prefix) = input.split_at(cut + 1);
    let dir = match expand_tilde(head, home) {
        Ok(d) if d.starts_with('/') => d,
        _ => return Completion::Stuck(0),
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Completion::Stuck(0);
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let visible = prefix.starts_with('.') || !name.starts_with('.');
            (visible && name.starts_with(prefix) && e.path().is_dir()).then_some(name)
        })
        .collect();
    names.sort();
    match names.as_slice() {
        [] => Completion::Stuck(0),
        [one] => Completion::Extend(format!("{head}{one}/")),
        many => {
            // Longest common prefix across the candidates; extend only if it
            // goes beyond what is already typed, else report the count.
            let mut lcp = many[0].as_str();
            for name in &many[1..] {
                let shared = lcp
                    .char_indices()
                    .find(|(i, c)| !name[*i..].starts_with(*c))
                    .map(|(i, _)| i)
                    .unwrap_or_else(|| lcp.len().min(name.len()));
                lcp = &lcp[..shared];
            }
            if lcp.len() > prefix.len() {
                Completion::Extend(format!("{head}{lcp}"))
            } else {
                Completion::Stuck(many.len())
            }
        }
    }
}

/// Footer label for a session: its name, or an id when the CLI handed us an
/// empty one. Bounded so `hidden <label> — u to undo` still fits a 34-column
/// footer with the part that says how to undo intact.
fn session_label(sess: &Session) -> String {
    let raw = match (sess.name.trim().is_empty(), sess.id.as_deref()) {
        (false, _) => sess.name.trim().to_string(),
        (true, Some(short)) => short.to_string(),
        (true, None) => short_id_of(&sess.session_id),
    };
    model::truncate_end(&raw, LABEL_MAX)
}

/// First 8 chars of a uuid — the short form `claude` itself prints.
fn short_id_of(session_id: &str) -> String {
    session_id.chars().take(8).collect()
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
            hidden: HiddenSet::new(),
            hidden_dirty: false,
            hidden_absent: std::collections::BTreeSet::new(),
            hidden_log: HiddenLog::new(),
            last_frags: BTreeMap::new(),
            seq_seen: 0,
            mode: Mode::Normal,
            prompt: None,
            logs: None,
            map: PaneMap::default(),
            map_dirty: false,
            sidebar_pane: None,
            panes: Vec::new(),
            // No own pane by default: a struct-literal fixture is not a tmux
            // pane, and persistence must behave as it does when degraded rather
            // than shelling out to the operator's live socket. The two flush
            // tests set both explicitly.
            own_pane: None,
            own_window: None,
            tabs: Vec::new(),
            open: BTreeMap::new(),
            width_opt: None,
            sidebar_cmd: None,
            own_state_loaded: true,
            migrated: true,
            // NOT degraded: the gates under test must fire on their own merits,
            // not because the degraded check short-circuited them.
            degraded: false,
            stop_arm: None,
            cx_last_press: None,
            pending_delete: None,
            pending_jump: None,
            message: None,
            msg_deadline: None,
            poll_error: None,
            fail_streak: 0,
            drift_seen: BTreeSet::new(),
            drift_pending: BTreeSet::new(),
            drift_flash: None,
            last_poll: Instant::now(),
            last_agents: Instant::now(),
            idle_streak: 0,
            payload_fp: None,
            // The fixture is NOT mid-startup: `force_poll` starts false so the
            // gate's tests see the gate, not the one-shot first-tick exemption.
            force_poll: false,
            was_watched: true,
            quiesced: false,
            // The fixture's inventory is whatever the test puts in `panes`,
            // and it counts as freshly enumerated. A test that wants a FAILED
            // enumeration clears this, the way `refresh_panes` does.
            panes_fresh: true,
            tabs_fresh: true,
            should_quit: false,
            pending_restart: None,
            // Same rule as `dispatch`: no hermetic test may respawn a pane on
            // the operator's live server. A test that exercises `R` installs
            // its own recording stub.
            respawn: |_, _, _| panic!("unit test reached respawn_pane"),
            // Same rule, same shape: the delete path kills panes, and a test
            // of which ones installs its own recording stub.
            kill_pane: |_, _| panic!("unit test reached kill_pane"),
            // The test harness is what argv[0] names here, and it answers no
            // `--version`. Tests that want the refusal path install their own.
            probe: |_| Ok(()),
            // STRUCTURAL, not disciplinary: no hermetic test may ever spawn the
            // real `claude` (a dispatch here would start a real background
            // session on the operator's daemon). Tests that exercise the submit
            // path install their own recording stub.
            dispatch: |_, _| panic!("unit test reached dispatch_background"),
            // STRUCTURAL, exactly like `dispatch` and `respawn`: `R`'s agent
            // pass STOPS live sessions, and the unit suite runs on the same
            // machine as the operator's real fleet. Neither seam may reach a
            // real `claude` from a hermetic test, so both panic until a test
            // installs its own recording stub.
            agents_poll: || panic!("unit test reached agents::poll"),
            agents_stop: |_| panic!("unit test reached agents::stop"),
            // Same rule again, and this one would SPAWN: `claude respawn`
            // starts a worker for a real session on the operator's daemon.
            agents_respawn: |_| panic!("unit test reached agents::respawn"),
            // "SCANNED, AND NOBODY IS ATTACHED" is the ambient truth of a unit
            // test — no `/proc`, no clients — so every test written before the
            // attach guard sees the population it always saw. A test whose
            // subject IS the guard says so by replacing this.
            agents_attached: || Some(BTreeSet::new()),
            agent_budget: AGENT_BUDGET,
        }
    }

    // ── live: the §1.4 even-layout verbs against a real tmux server ─────────

    /// Every tmux command this test issues, including the raw ones below,
    /// names socket `ccmux-evenfix` explicitly and runs with `$TMUX`/`$TMUX_PANE`
    /// cleared. PROBE-FINDINGS RULE T1 is therefore structural here and not a
    /// matter of discipline: there is no code path from this test to the
    /// operator's default server, where their live work lives.
    const LIVE_SOCKET: &str = "ccmux-evenfix";
    const LIVE_SESSION: &str = "ccmux-even-test";

    /// A raw tmux call, for the two things the crate deliberately has no public
    /// helper for: creating a server-side window at a fixed 120x40 (ccmux
    /// itself always inherits the client's size) and tearing the session down.
    /// It is NOT a back door around R1-R3 — production code cannot reach it,
    /// `-L LIVE_SOCKET` is hard-coded, and the session name is a constant.
    fn live_raw(args: &[&str]) -> String {
        let out = std::process::Command::new("tmux")
            .arg("-L")
            .arg(LIVE_SOCKET)
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .expect("tmux");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Widths of one window's panes, left to right — `#{pane_width}` in the
    /// order the operator sees them.
    fn live_widths(a: &mut App) -> Vec<u16> {
        a.refresh_panes();
        let mut p = a.panes.clone();
        p.sort_by_key(|i| i.left);
        p.iter().map(|i| i.width).collect()
    }

    /// The evenness the feature exists to produce: no two content panes may
    /// differ by more than a single cell.
    ///
    /// Written as a bare `max - min <= 1` on purpose. An earlier form allowed
    /// `max - min < content.len()` as a second chance for "the remainder", and
    /// that disjunct is TAUTOLOGICAL under any remainder policy — the leftover
    /// is `total % n`, which is always below `n` — so the assertion could not
    /// fail for any pane count and would not have noticed the division getting
    /// less even.
    fn assert_spread_is_at_most_one(what: &str, content: &[u16]) {
        let hi = *content.iter().max().expect("at least one content pane");
        let lo = *content.iter().min().expect("at least one content pane");
        assert!(hi - lo <= 1, "{what} are not even: {content:?}");
    }

    /// Heights of one window's panes, top to bottom.
    fn live_heights(a: &mut App) -> Vec<u16> {
        a.refresh_panes();
        let mut p = a.panes.clone();
        p.sort_by_key(|i| i.top);
        p.iter().map(|i| i.height).collect()
    }

    /// Build a fresh 120x40 session with a sidebar, and an `App` bound to it.
    fn live_app() -> App {
        live_raw(&["kill-session", "-t", "=ccmux-even-test:"]);
        live_raw(&[
            "new-session",
            "-d",
            "-s",
            LIVE_SESSION,
            "-x",
            "120",
            "-y",
            "40",
            "-n",
            "cc",
            "--",
            "sleep",
            "600",
        ]);
        let panes = tmux::list_panes_in_session(LIVE_SESSION).expect("list panes");
        let sb = panes.first().expect("sidebar pane").id.clone();
        tmux::configure_session(LIVE_SESSION, &sb, 34).expect("configure");

        let mut a = app();
        a.tmux_session = LIVE_SESSION.into();
        a.sidebar_pane = Some(sb);
        a.map = PaneMap::new();
        load(&mut a, vec![
            bg("aaaaaaaa", "one", State::Working),
            bg("bbbbbbbb", "two", State::Working),
            bg("cccccccc", "three", State::Working),
            bg("dddddddd", "four", State::Working),
            bg("eeeeeeee", "five", State::Working),
            bg("ffffffff", "six", State::Working),
        ]);
        // THE LIVE SOCKET IS THE POINT. `app()` installs a panicking
        // `kill_pane` so no hermetic test can ever kill a pane on the
        // operator's server; this harness has already pinned `tmux::socket()`
        // to the throwaway one, so `x` must reach the real call or the kill
        // phase cannot be exercised at all.
        a.kill_pane = tmux::kill_pane;
        a.refresh_panes();
        a
    }

    /// Row indices that actually carry a session. `App::rows` is
    /// Header | Spacer | Session, so `selected = 0` is a group header and every
    /// verb on it is a no-op — which is correct behaviour and useless here.
    fn live_session_rows(a: &App) -> Vec<usize> {
        (0..a.rows.len()).filter(|i| a.is_session_row(*i)).collect()
    }

    /// Drives the REAL `act_open` / `act_close_pane` against a real tmux 3.4
    /// server and prints the geometry after every step, so the numbers in the
    /// report are reproducible rather than transcribed.
    ///
    /// Run with:
    ///   cargo test -- --ignored --nocapture live_even_layout
    ///
    /// The name filter is REQUIRED. `tmux::socket()` is process-global (see the
    /// socket-selection note in `tmux.rs`) and `tmux::tests::live_round_trip`
    /// sets it to `ccmux`, so running every ignored test at once has the two
    /// racing for one global. The failure is loud and harmless — "can't find
    /// session" against the other socket, nothing mutated — but it is a
    /// failure, so run this one by name or with `--test-threads=1`.
    ///
    /// The last phase emulates idle poll ticks by their exact geometry
    /// sequence: `tick` touches pane geometry in step 2 (`refresh_panes`) and
    /// step 7 (`pin_sidebar`) and nowhere else — steps 1, 4, 4b, 5, 6 and 8 are
    /// the clock, `claude`, the row list and the option flush — so repeating
    /// those two is repeating everything a tick can do to the layout, without
    /// making a hermetic-by-default test depend on the `claude` CLI.
    #[test]
    #[ignore = "mutates a tmux server; run by name (see the doc comment)"]
    fn live_even_layout_spreads_panes_on_split_and_kill() {
        tmux::set_socket(Some(LIVE_SOCKET));
        assert_eq!(tmux::socket().as_deref(), Some(LIVE_SOCKET), "throwaway socket only");

        // ── `o`: four content panes, widths evened ──────────────────────────
        let mut a = live_app();
        println!("\n== `o` (SplitDir::Vertical) — widths, 120x40 window, sidebar 34 ==");
        println!("start           : {:?}", live_widths(&mut a));
        let rows = live_session_rows(&a);
        for (n, row) in rows.iter().take(4).enumerate() {
            a.selected = *row;
            a.act_open(SplitDir::Vertical);
            println!("after o #{}      : {:?}", n + 1, live_widths(&mut a));
        }
        let evened = live_widths(&mut a);
        assert_eq!(evened[0], 34, "sidebar keeps its pinned width");
        assert_spread_is_at_most_one("`o` widths", &evened[1..]);

        // ── `x`: kill one, survivors re-evened ──────────────────────────────
        a.selected = rows[1];
        a.act_close_pane();
        let after_kill = live_widths(&mut a);
        println!("after x         : {after_kill:?}");
        assert_eq!(after_kill[0], 34, "sidebar keeps its pinned width");
        assert_spread_is_at_most_one("widths after `x`", &after_kill[1..]);

        // ── idle ticks: the pin must not disturb the layout ─────────────────
        let before = live_widths(&mut a);
        for i in 1..=5 {
            a.refresh_panes();
            a.pin_sidebar();
            println!("after tick {i}    : {:?}", live_widths(&mut a));
        }
        assert_eq!(live_widths(&mut a), before, "the per-tick pin is a no-op on an even layout");
        // And it is a no-op because it WRITES NOTHING, not because the write
        // happens to land on the same number: on a settled layout the pin's
        // own decision is "already right", so an idle sidebar spawns no
        // `resize-pane` at all.
        a.refresh_panes();
        let sb = a.sidebar_pane.clone().expect("sidebar");
        assert!(
            !App::needs_repin(tmux::pane_width(&a.panes, &sb), 34),
            "an idle tick on an even layout must not issue resize-pane"
        );

        // ── `s`: four content panes, heights evened ─────────────────────────
        let mut a = live_app();
        println!("\n== `s` (SplitDir::Horizontal) — heights, 120x40 window ==");
        // One `o` first, because `s` pressed while the sidebar is ALONE splits
        // the sidebar itself (`split_anchor` step 3) and leaves it stacked, not
        // a full-height left edge. §1.4 correctly declines to lay that out —
        // see the mixed-tree phase below — and it is the same pane the operator
        // would open first in practice.
        let rows = live_session_rows(&a);
        a.selected = rows[0];
        a.act_open(SplitDir::Vertical);
        println!("start (one o)   : {:?}", live_heights(&mut a));
        for (n, row) in rows.iter().skip(1).take(4).enumerate() {
            a.selected = *row;
            a.act_open(SplitDir::Horizontal);
            println!("after s #{}      : {:?}", n + 1, live_heights(&mut a));
        }
        println!("widths          : {:?}", live_widths(&mut a));
        assert_spread_is_at_most_one("`s` heights", &live_heights(&mut a)[1..]);
        a.selected = rows[1];
        a.act_close_pane();
        let after_kill = live_heights(&mut a);
        println!("after x         : {after_kill:?}");
        assert_spread_is_at_most_one("heights after `x`", &after_kill[1..]);

        let before = live_heights(&mut a);
        for i in 1..=5 {
            a.refresh_panes();
            a.pin_sidebar();
            println!("after tick {i}    : {:?}", live_heights(&mut a));
        }
        assert_eq!(live_heights(&mut a), before, "the per-tick pin is a no-op on an even layout");
        assert_eq!(live_widths(&mut a)[0], 34, "sidebar keeps its pinned width");

        // ── mixed `o` + `s`: the tree is left exactly as the operator built it
        let mut a = live_app();
        println!("\n== mixed `o` then `s` — the tree is left alone ==");
        let rows = live_session_rows(&a);
        a.selected = rows[0];
        a.act_open(SplitDir::Vertical);
        a.selected = rows[1];
        a.act_open(SplitDir::Vertical);
        a.refresh_panes();
        let row = a.panes.iter().map(|p| (p.left, p.width, p.top, p.height)).collect::<Vec<_>>();
        println!("two o           : {row:?}");
        a.selected = rows[2];
        a.act_open(SplitDir::Horizontal);
        a.refresh_panes();
        let tree = a.panes.iter().map(|p| (p.left, p.width, p.top, p.height)).collect::<Vec<_>>();
        println!("then one s      : {tree:?}");
        let sb = a.sidebar_pane.clone().expect("sidebar");
        let win = tmux::window_of(&a.panes, &sb).expect("window");
        let scoped = tmux::panes_in_window(&a.panes, win);
        assert_eq!(
            tmux::content_shape(&scoped, &sb),
            tmux::ContentShape::Ragged,
            "a mix of o and s is a tree"
        );
        assert_eq!(tmux::even_layout(&scoped, &sb, 34, None), None, "and a tree is never re-laid out");
        let after_ticks = {
            for _ in 0..3 {
                a.refresh_panes();
                a.pin_sidebar();
            }
            a.refresh_panes();
            a.panes.iter().map(|p| (p.left, p.width, p.top, p.height)).collect::<Vec<_>>()
        };
        println!("after 3 ticks   : {after_ticks:?}");
        assert_eq!(after_ticks, tree, "ticks do not move a tree either");

        live_raw(&["kill-session", "-t", "=ccmux-even-test:"]);
    }

    /// The pin writes only when the width is actually wrong, and an unknown
    /// width counts as wrong — see `needs_repin`.
    #[test]
    fn the_pin_writes_only_when_the_width_is_wrong() {
        assert!(!App::needs_repin(Some(34), 34), "already right: no resize-pane");
        assert!(App::needs_repin(Some(20), 34), "a manual resize still heals");
        assert!(App::needs_repin(Some(60), 34), "so does a split that halved it");
        assert!(App::needs_repin(None, 34), "an unreadable snapshot pins, as it always did");
    }

    /// `r` IS THE WAY BACK. The tick pin stands down on a matching width and
    /// on a zoom, and the even pass never runs on a timer at all — so this
    /// asserts the other half of that bargain: a window the operator has
    /// wrecked with tmux's own keys, and one they have zoomed, both come back
    /// on one `r`.
    ///
    /// Run with:
    ///   cargo test -- --ignored --nocapture live_r_reasserts
    ///
    /// Same socket caveat as the other live tests: run it by name.
    #[test]
    #[ignore = "mutates a tmux server; run by name (see the doc comment)"]
    fn live_r_reasserts_a_layout_the_tick_will_not_touch() {
        tmux::set_socket(Some(LIVE_SOCKET));
        assert_eq!(tmux::socket().as_deref(), Some(LIVE_SOCKET), "throwaway socket only");

        let mut a = live_app();
        let rows = live_session_rows(&a);
        for row in rows.iter().take(3) {
            a.selected = *row;
            a.act_open(SplitDir::Vertical);
        }
        let even = live_widths(&mut a);
        println!("\n== `r` re-asserts — 120x40 window, sidebar 34 ==");
        println!("start           : {even:?}");
        assert_eq!(even[0], 34);
        assert_spread_is_at_most_one("start", &even[1..]);

        // ── the operator drags a border, the way tmux's own keys do ─────────
        let sb = a.sidebar_pane.clone().expect("sidebar");
        let victim = a.panes.iter().find(|p| p.id != sb).map(|p| p.id.clone()).expect("content pane");
        live_raw(&["resize-pane", "-t", victim.as_str(), "-x", "12"]);
        let dragged = live_widths(&mut a);
        println!("after a drag    : {dragged:?}");
        assert_ne!(dragged, even, "the drag actually moved something");

        // A tick alone leaves it: the sidebar is still on its width, and §1.4
        // does not run on a timer. That is the gap `r` fills.
        a.refresh_panes();
        a.pin_sidebar();
        assert_eq!(live_widths(&mut a), dragged, "a tick does not undo a drag");

        a.act_repin();
        let healed = live_widths(&mut a);
        println!("after r         : {healed:?}");
        assert_eq!(healed[0], 34, "sidebar back on its pinned width");
        assert_spread_is_at_most_one("after `r`", &healed[1..]);

        // ── and it forces through a zoom, which the tick must never do ──────
        live_raw(&["resize-pane", "-Z", "-t", victim.as_str()]);
        a.refresh_panes();
        assert!(tmux::window_zoomed(&a.panes, &sb), "zoomed");
        a.refresh_panes();
        a.pin_sidebar();
        a.refresh_panes();
        assert!(tmux::window_zoomed(&a.panes, &sb), "a tick still holds the zoom");
        a.act_repin();
        a.refresh_panes();
        assert!(!tmux::window_zoomed(&a.panes, &sb), "but `r` re-lays the window, which ends it");
        let after_zoom = live_widths(&mut a);
        println!("after r on zoom : {after_zoom:?}");
        assert_eq!(after_zoom[0], 34);
        assert_spread_is_at_most_one("after `r` on a zoom", &after_zoom[1..]);

        live_raw(&["kill-session", "-t", "=ccmux-even-test:"]);
    }

    /// A ZOOM MUST OUTLIVE THE TICK. `<prefix> z` is the operator asserting a
    /// geometry of their own, and `resize-pane` clears `window_zoomed_flag` on
    /// tmux 3.4 — so before the gate, §1.3's per-tick pin gave the window back
    /// within one `interval` and the zoom looked like it "wore off after a few
    /// seconds". This drives the same two calls a real tick makes and asserts
    /// the flag is still set afterwards, then that releasing the zoom restores
    /// the pinned width on the very next tick.
    ///
    /// Run with:
    ///   cargo test -- --ignored --nocapture live_zoom
    ///
    /// Same socket caveat as `live_even_layout_spreads_panes_on_split_and_kill`:
    /// run it by name.
    #[test]
    #[ignore = "mutates a tmux server; run by name (see the doc comment)"]
    fn live_zoom_survives_the_per_tick_pin() {
        tmux::set_socket(Some(LIVE_SOCKET));
        assert_eq!(tmux::socket().as_deref(), Some(LIVE_SOCKET), "throwaway socket only");

        let mut a = live_app();
        let rows = live_session_rows(&a);
        a.selected = rows[0];
        a.act_open(SplitDir::Vertical);
        a.selected = rows[1];
        a.act_open(SplitDir::Vertical);
        a.refresh_panes();
        let pinned = live_widths(&mut a);
        println!("\n== zoom hold — 120x40 window, sidebar 34 ==");
        println!("before zoom     : {pinned:?}");

        // Zoom a CONTENT pane, the way the operator does: the pane that is not
        // the sidebar, addressed by id so this cannot land on the wrong one.
        let sb = a.sidebar_pane.clone().expect("sidebar");
        let victim = a
            .panes
            .iter()
            .find(|p| p.id != sb)
            .map(|p| p.id.clone())
            .expect("a content pane");
        live_raw(&["resize-pane", "-Z", "-t", victim.as_str()]);
        a.refresh_panes();
        assert!(tmux::window_zoomed(&a.panes, &sb), "the window is zoomed to start with");

        for i in 1..=5 {
            a.refresh_panes();
            a.pin_sidebar();
            a.refresh_panes();
            println!("after tick {i}    : zoomed={}", tmux::window_zoomed(&a.panes, &sb));
            assert!(
                tmux::window_zoomed(&a.panes, &sb),
                "tick {i} un-zoomed the window — the pin ran through a zoom"
            );
        }

        // And the pin is only DEFERRED, never abandoned: the next tick after
        // the operator un-zooms puts the sidebar back on its width.
        live_raw(&["resize-pane", "-Z", "-t", victim.as_str()]);
        a.refresh_panes();
        assert!(!tmux::window_zoomed(&a.panes, &sb), "un-zoomed");
        live_raw(&["resize-pane", "-t", sb.as_str(), "-x", "20"]);
        a.refresh_panes();
        a.pin_sidebar();
        let healed = live_widths(&mut a);
        println!("after unzoom    : {healed:?}");
        assert_eq!(healed[0], 34, "the pin heals a manual resize as soon as the zoom is gone");

        live_raw(&["kill-session", "-t", "=ccmux-even-test:"]);
    }

    /// A background row with NO WORKER (`pid: null`) — DORMANT.
    ///
    /// Dormant is the default on purpose, and it is not laziness about a
    /// fixture. `R`'s headless population is exactly the rows that HAVE a
    /// worker, so a dormant default leaves that population empty and every
    /// test written before it existed still asserts what it was written to
    /// assert. A test that wants a live paneless agent says so, in one word,
    /// at the row it means: `live(bg(...))`.
    fn bg(short: &str, name: &str, state: State) -> Session {
        Session {
            id: Some(short.to_string()),
            pid: None,
            session_id: format!("{short}-uuid"),
            cwd: "/home/dev/projects".into(),
            kind: Kind::Background,
            started_at: 1_787_600_000_000,
            name: name.to_string(),
            status: Status::Busy,
            state: Some(state),
        }
    }

    /// THE SAME ROW WITH A LIVE WORKER. The value is never read — only its
    /// presence is (`Session::has_worker`) — so any number will do and this
    /// one is deliberately not a plausible pid, to make that obvious at the
    /// call site.
    fn live(s: Session) -> Session {
        Session { pid: Some(4242), ..s }
    }

    fn inter(uuid: &str, name: &str) -> Session {
        Session {
            id: None,
            pid: Some(4242),
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

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// A successful, COMPLETE poll — what `agents::poll` returns when nothing
    /// went wrong. Tests that want an under-reporting poll build the `Payload`
    /// themselves with `dropped > 0`.
    fn complete(sessions: Vec<Session>) -> Result<model::Payload, agents::AgentsError> {
        Ok(model::Payload { sessions, dropped: 0 })
    }

    /// Make the NEXT `save_hidden` for this app a no-op that still returns
    /// `Ok(())`, by pre-seeding `tmux`'s write-dedupe cache with the exact JSON
    /// it is about to serialize.
    ///
    /// This is how `shutdown` is tested without a tmux server, and the reason
    /// is a hard rule, not tidiness: `cargo test` must never reach tmux,
    /// because the default socket is the operator's live one. The value is
    /// serialized by the same call `save_hidden` makes, so it always matches
    /// and the spawn is always skipped.
    fn arm_hermetic_flush(a: &App) {
        let win = a.own_window.clone().expect("a flushing app owns a window");
        crate::tmux::seed_saved_value(
            &a.tmux_session,
            crate::tmux::OPT_TAB_HIDDEN,
            &win,
            &serde_json::to_string(&a.hidden_log).expect("HiddenLog serializes"),
        );
    }

    /// Give an app an addressable identity, the way a real sidebar gets one
    /// from `$TMUX_PANE`. Without it every window-scoped write is skipped.
    fn own(a: &mut App, pane: &str, window: &str) {
        a.own_pane = PaneId::parse(pane);
        a.own_window = WindowId::parse(window);
    }

    fn load(app: &mut App, sessions: Vec<Session>) {
        app.sessions = sessions;
        app.rebuild_rows();
        app.select_first();
    }

    fn pane(id: &str, window: u32, index: u32, left: u16, width: u16, active: bool) -> PaneInfo {
        PaneInfo {
            id: PaneId::parse(id).expect("pane id"),
            index,
            left,
            top: 0,
            width,
            height: 40,
            active,
            window_index: window,
            window_id: WindowId::parse(&format!("@{window}")).expect("window id"),
            // Watched by default: the pane fixtures predate the poll gate and
            // exist to exercise layout, not visibility. The gate's own tests
            // set these three explicitly.
            window_active: true,
            session_clients: 1,
            window_viewers: Some(1),
            window_zoomed: false,
            detached: false,
            shell: false,
        }
    }

    // ── `Ctrl+X` — stop, and again to delete (§8.2) ─────────────────────────

    /// Age the burst guard's clock so the NEXT press reads as a deliberate one.
    /// No test sleeps: they backdate.
    fn after(a: &mut App, gap: Duration) {
        a.cx_last_press = a.cx_last_press.and_then(|t| t.checked_sub(gap));
    }

    /// Age a settling delete past `CX_SETTLE` and run the event loop's tick.
    fn settle(a: &mut App) -> bool {
        if let Some(p) = a.pending_delete.as_mut() {
            p.at = p.at.checked_sub(CX_SETTLE).unwrap_or(p.at);
        }
        a.tick_stop_arm()
    }

    /// A DELIBERATE second press: clear of `CX_MIN_GAP` (750 ms), inside
    /// `CX_WINDOW` (2 s). The usable band is exactly that, and this sits in it.
    const BEAT: Duration = Duration::from_millis(1000);
    const _: () = assert!(BEAT.as_millis() > CX_MIN_GAP.as_millis());
    const _: () = assert!(BEAT.as_millis() < CX_WINDOW.as_millis());

    // ── `Ctrl+X` twice closes the session's panes ───────────────────────────

    thread_local! {
        /// Every pane `kill_pane` was asked to kill, in order, with the tmux
        /// session it was asked to kill it in.
        static KILLS: std::cell::RefCell<Vec<(String, String)>> =
            const { std::cell::RefCell::new(Vec::new()) };
        /// Panes the stub refuses to kill, the way R2 refuses a dead one.
        static KILL_REFUSED: std::cell::RefCell<Vec<String>> =
            const { std::cell::RefCell::new(Vec::new()) };
        /// When the LAST kill returned — the re-stamp must come after it.
        static LAST_KILL_AT: std::cell::RefCell<Option<Instant>> =
            const { std::cell::RefCell::new(None) };
    }

    fn recording_kill(session: &str, pane: &PaneId) -> Result<(), TmuxError> {
        KILLS.with(|k| k.borrow_mut().push((session.to_string(), pane.to_string())));
        LAST_KILL_AT.with(|t| *t.borrow_mut() = Some(Instant::now()));
        let refused = KILL_REFUSED.with(|r| r.borrow().iter().any(|p| p == pane.as_str()));
        if refused {
            return Err(TmuxError::BadTarget(format!("pane {pane} is not in tmux session")));
        }
        Ok(())
    }

    /// The pane ids handed to `kill_pane`, in order.
    fn kills() -> Vec<String> {
        KILLS.with(|k| k.borrow().iter().map(|(_, p)| p.clone()).collect())
    }

    fn refuse_kill(pane: &str) {
        KILL_REFUSED.with(|r| r.borrow_mut().push(pane.to_string()));
    }

    fn pid(s: &str) -> PaneId {
        PaneId::parse(s).expect("pane id")
    }

    /// The shape of the live session this exists for. Tab 1 is mine: `%1` my
    /// sidebar, `%2` beside it showing `1c45d64f`. Tab 2 has its own sidebar
    /// `%4` and a content pane `%5`, held in THAT window's map — which also
    /// shows `1c45d64f` when `double` is set (double-attach is legal, PROBE
    /// §3). `1c45d64f` is loaded and selected.
    fn deletable(a: &mut App, double: bool) {
        own(a, "%1", "@1");
        a.sidebar_pane = PaneId::parse("%1");
        a.panes = vec![
            pane("%1", 1, 0, 0, 34, false),
            pane("%2", 1, 1, 35, 85, true),
            pane("%4", 2, 0, 0, 34, false),
            pane("%5", 2, 1, 35, 85, false),
        ];
        a.map.insert(&pid("%2"), entry("1c45d64f-uuid"));
        let mut t1 = tab("@1", 1, Some("%1"));
        t1.map = a.map.clone();
        let mut t2 = tab("@2", 2, Some("%4"));
        if double {
            t2.map.insert(&pid("%5"), entry("1c45d64f-uuid"));
        }
        a.tabs = vec![t1, t2];
        a.rebuild_open();
        a.kill_pane = recording_kill;
        KILLS.with(|k| k.borrow_mut().clear());
        KILL_REFUSED.with(|r| r.borrow_mut().clear());
        LAST_KILL_AT.with(|t| *t.borrow_mut() = None);
        agents::test_spawn::reset();
        load(a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);
        assert_eq!(a.selected_key.as_deref(), Some("1c45d64f-uuid"));
    }

    /// The deliberate gesture: press, a clear beat, press, and the settle.
    fn delete_twice(a: &mut App) {
        a.on_key(ctrl('x'));
        after(a, BEAT);
        a.on_key(ctrl('x'));
        assert!(settle(a), "the settled delete must ask for a redraw");
    }

    /// THE bug. `claude rm` made the attach exit, the wrapper parked the pane
    /// on `enter=resume  s=shell  q=close pane`, and the operator had to press
    /// `q` in a pane with nothing left to resume. A successful delete now
    /// closes the pane — through the seam `x` kills through, in ccmux's own
    /// session, and unmaps it from my window's map.
    #[test]
    fn a_successful_delete_closes_the_sessions_pane() {
        let mut a = app();
        deletable(&mut a, false);

        delete_twice(&mut a);

        assert_eq!(agents::test_spawn::joined(), vec!["stop 1c45d64f", "rm 1c45d64f"]);
        assert_eq!(kills(), vec!["%2"]);
        KILLS.with(|k| {
            assert!(
                k.borrow().iter().all(|(s, _)| s == "ccmux-test"),
                "every kill names ccmux's own session: {:?}",
                k.borrow()
            );
        });
        assert!(a.map.get(&pid("%2")).is_none(), "an own-window pane is unmapped, as `x` does");
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "deleted bt/reg-update + worktree · closed 1 pane");
        assert_eq!(level, MsgLevel::Warn);
        assert!(a.pending_delete.is_none());
    }

    /// Double-attach across tabs: BOTH panes close. The one in tab 2 is
    /// killed but NOT unmapped from here — that window's option has one
    /// writer and it is not this process; its owner reconciles it away.
    #[test]
    fn a_session_open_in_two_tabs_has_both_panes_closed() {
        let mut a = app();
        deletable(&mut a, true);

        delete_twice(&mut a);

        assert_eq!(kills(), vec!["%2", "%5"]);
        assert!(a.map.get(&pid("%2")).is_none(), "mine: unmapped");
        assert!(
            a.tabs[1].map.get(&pid("%5")).is_some(),
            "tab 2's entry is its owner's to remove, never this process's"
        );
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "deleted bt/reg-update + worktree · closed 2 panes");
    }

    /// A refused `claude rm` — a worktree with unpushed work — closes NOTHING.
    /// The pane still shows a session that exists, plus the one line saying
    /// the work is safe, and killing it would hide that line.
    #[test]
    fn a_failed_delete_closes_nothing() {
        let mut a = app();
        deletable(&mut a, true);

        a.on_key(ctrl('x'));
        after(&mut a, BEAT);
        // Queued AFTER the first press: `fail_next` takes the next intercepted
        // call, and the first press's `claude stop` has already gone by.
        agents::test_spawn::fail_next(
            "Session 1c45d64f has a worktree with unpushed commits; not removing",
        );
        a.on_key(ctrl('x'));
        settle(&mut a);

        assert_eq!(agents::test_spawn::joined(), vec!["stop 1c45d64f", "rm 1c45d64f"]);
        assert!(kills().is_empty(), "a refused rm reaches no kill: {:?}", kills());
        assert!(a.map.get(&pid("%2")).is_some(), "the pane is still mapped");
        assert!(a.tabs[1].map.get(&pid("%5")).is_some());
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(
            text,
            "delete failed: Session 1c45d64f has a worktree with unpushed commits; not removing"
        );
        assert_eq!(level, MsgLevel::Error);
    }

    /// The FIRST press is a stop, and a stopped session is still there to
    /// resume, so its pane stays parked on the prompt where `enter` resumes
    /// it. Unchanged.
    #[test]
    fn the_first_ctrl_x_still_parks_the_pane_and_closes_nothing() {
        let mut a = app();
        deletable(&mut a, true);

        a.on_key(ctrl('x'));

        assert_eq!(agents::test_spawn::joined(), vec!["stop 1c45d64f"]);
        assert!(kills().is_empty(), "{:?}", kills());
        assert!(a.map.get(&pid("%2")).is_some());
        assert!(a.stop_arm.is_some(), "and the delete window is open");
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "stopped bt/reg-update");
    }

    /// A map that names a sidebar — the shape a corrupt or hand-edited option
    /// could produce — never turns the sidebar into a kill target. It is
    /// skipped, not counted, and left mapped for its owner to sort out.
    #[test]
    fn the_sidebar_is_never_a_delete_kill_target() {
        let mut a = app();
        deletable(&mut a, false);
        a.map.insert(&pid("%1"), entry("1c45d64f-uuid"));
        a.tabs[1].map.insert(&pid("%4"), entry("1c45d64f-uuid"));
        a.rebuild_open();

        delete_twice(&mut a);

        assert_eq!(kills(), vec!["%2"], "neither tab's sidebar is killed");
        assert!(a.map.get(&pid("%1")).is_some(), "a refused kill unmaps nothing");
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "deleted bt/reg-update + worktree · closed 1 pane");
    }

    /// A pane tmux refuses to kill — R2, or already gone — is left where it
    /// is, still mapped, and the count says only what actually closed.
    #[test]
    fn a_refused_kill_is_left_mapped_and_not_counted() {
        let mut a = app();
        deletable(&mut a, true);
        refuse_kill("%2");

        delete_twice(&mut a);

        assert_eq!(kills(), vec!["%2", "%5"], "the refusal does not stop the run");
        assert!(a.map.get(&pid("%2")).is_some(), "a pane that did not die stays mapped");
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "deleted bt/reg-update + worktree · closed 1 pane");
    }

    /// No pane at all: the flash is exactly what it was — the count is only
    /// appended when there is one to report.
    #[test]
    fn a_delete_with_no_pane_flashes_the_bare_line() {
        let mut a = app();
        deletable(&mut a, false);
        a.map = PaneMap::new();
        a.tabs[0].map = PaneMap::new();
        a.rebuild_open();

        delete_twice(&mut a);

        assert!(kills().is_empty());
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "deleted bt/reg-update + worktree");
    }

    /// A pane a map still names but the inventory no longer has is not a
    /// target: `x`'s R2 gate would only refuse it, at the cost of a spawn.
    #[test]
    fn a_dead_pane_in_a_map_is_not_a_kill_target() {
        let mut a = app();
        deletable(&mut a, true);
        a.panes.retain(|p| p.id.as_str() != "%5");
        a.rebuild_open();

        delete_twice(&mut a);

        assert_eq!(kills(), vec!["%2"]);
    }

    /// Mark a pane of the fixture the way the wrapper marks it: parked
    /// (`Ctrl+Z`, latch `1`) or handed to the operator's shell (`s`, latch
    /// `shell`, which reads as detached too).
    fn latch(a: &mut App, id: &str, shell: bool) {
        let p = a.panes.iter_mut().find(|p| p.id.as_str() == id).expect("fixture pane");
        p.detached = true;
        p.shell = shell;
    }

    /// THE PARKED PANE — the one the operator complained about. `Ctrl+Z` left
    /// it on `enter=resume  s=shell  q=close pane`, `claude rm` made that
    /// resume impossible, and it is closed with the rest.
    #[test]
    fn a_parked_pane_is_closed_by_the_delete() {
        let mut a = app();
        deletable(&mut a, true);
        latch(&mut a, "%2", false);
        a.rebuild_open();

        delete_twice(&mut a);

        assert_eq!(kills(), vec!["%2", "%5"]);
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "deleted bt/reg-update + worktree · closed 2 panes");
    }

    /// REGRESSION. A pane the operator answered `s` in is their shell now —
    /// with, measured live, a `sleep 600` or a hand-attached OTHER session in
    /// it — and the map still names it, because `reconcile` keeps an entry
    /// while its pane lives. Deleting the session it used to show killed it,
    /// foreground job and all. The wrapper's `shell` mark is what tells it
    /// from a parked pane: the parked one in the other tab still closes, the
    /// shell survives, stays mapped for `x`, and is not counted.
    #[test]
    fn a_pane_the_operator_took_to_a_shell_survives_the_delete() {
        let mut a = app();
        deletable(&mut a, true);
        latch(&mut a, "%2", true);
        latch(&mut a, "%5", false);
        a.rebuild_open();

        delete_twice(&mut a);

        assert_eq!(kills(), vec!["%5"], "the shell is not a kill target");
        assert!(a.map.get(&pid("%2")).is_some(), "still mapped, still `x`'s to close");
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "deleted bt/reg-update + worktree · closed 1 pane");
        assert!(a.pending_delete.is_none());
    }

    /// The same when the shell is the ONLY pane: nothing is killed and the
    /// flash is the bare line — the pane is on screen, saying for itself
    /// what is in it.
    #[test]
    fn a_delete_whose_only_pane_is_a_shell_closes_nothing() {
        let mut a = app();
        deletable(&mut a, false);
        latch(&mut a, "%2", true);
        a.rebuild_open();

        delete_twice(&mut a);

        assert!(kills().is_empty(), "{:?}", kills());
        assert!(a.map.get(&pid("%2")).is_some());
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "deleted bt/reg-update + worktree");
    }

    /// `x` is the operator's say-so, and it still closes a shell pane: the
    /// README's "it is still a pane ccmux opened, so `x` still closes it"
    /// holds for the shell exactly as it does for the parked prompt.
    #[test]
    fn x_still_closes_a_pane_the_operator_took_to_a_shell() {
        let mut a = app();
        deletable(&mut a, false);
        latch(&mut a, "%2", true);
        a.rebuild_open();

        a.on_key(press('x'));

        assert_eq!(kills(), vec!["%2"]);
        assert!(a.map.get(&pid("%2")).is_none());
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "closed pane 1 — agent still running");
    }

    /// The re-stamp of `cx_last_press` must survive, and it must come AFTER
    /// the kills: they are one more blocking shell-out on this path, and a
    /// `Ctrl+X` buffered through them has to read as the burst it is.
    #[test]
    fn the_re_stamp_after_a_delete_comes_after_the_kills() {
        let mut a = app();
        deletable(&mut a, true);

        delete_twice(&mut a);

        let killed_at = LAST_KILL_AT.with(|t| *t.borrow()).expect("something was killed");
        let stamped = a.cx_last_press.expect("run_delete re-stamps");
        assert!(stamped >= killed_at, "the re-stamp must follow the last kill");
        // And it does what it is for: a press dequeued now is a burst.
        assert_eq!(a.on_key(ctrl('x')), Action::Redraw);
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "too fast — press Ctrl+X again"
        );
        assert_eq!(agents::test_spawn::joined(), vec!["stop 1c45d64f", "rm 1c45d64f"]);
    }

    /// `x` goes through the SAME path, and is unchanged by it: one pane, the
    /// one beside this sidebar, unmapped and announced as before.
    #[test]
    fn x_closes_one_pane_through_the_shared_path() {
        let mut a = app();
        deletable(&mut a, true);

        a.on_key(press('x'));

        assert_eq!(kills(), vec!["%2"], "`x` closes the pane in MY tab, and only it");
        assert!(a.map.get(&pid("%2")).is_none());
        assert!(a.tabs[1].map.get(&pid("%5")).is_some(), "tab 2's pane is untouched");
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "closed pane 1 — agent still running");
        assert_eq!(level, MsgLevel::Info);
        assert!(agents::test_spawn::joined().is_empty(), "`x` never reaches `claude`");
    }

    /// `x` on a session open only in ANOTHER tab kills that pane and unmaps
    /// nothing here — the rule the shared path carries for the delete too.
    #[test]
    fn x_in_another_tab_kills_but_does_not_unmap() {
        let mut a = app();
        deletable(&mut a, true);
        a.map = PaneMap::new();
        a.tabs[0].map = PaneMap::new();
        a.rebuild_open();

        a.on_key(press('x'));

        assert_eq!(kills(), vec!["%5"]);
        assert!(a.tabs[1].map.get(&pid("%5")).is_some());
        assert!(!a.map_dirty, "nothing of mine changed");
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "closed pane 1 in tab 2 — agent still running");
    }

    /// THE placement test. `key_normal` matches top-down and ends its Ctrl
    /// block with `_ if ctrl => Action::None`; a `Ctrl+X` arm below that line
    /// compiles, reads correctly, and never fires. Only a press driven through
    /// `on_key` — the real entry point, mode dispatch and all — proves it.
    #[test]
    fn ctrl_x_reaches_the_keymap_and_is_not_swallowed_by_the_ctrl_catch_all() {
        agents::test_spawn::reset();
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);

        assert_eq!(a.on_key(ctrl('x')), Action::Redraw, "the catch-all swallowed it");
        assert_eq!(agents::test_spawn::joined(), vec!["stop 1c45d64f"]);
        assert_eq!(a.mode, Mode::Normal, "no modal: §8.2 stops immediately now");

        // The window is open, and the footer says what the next press does and
        // what it takes.
        let arm = a.stop_arm.clone().expect("the first press must arm");
        assert_eq!(arm.short_id, "1c45d64f");
        let hint = a.arm_hint().expect("armed");
        assert!(hint.contains("Ctrl+X again"), "{hint}");
        assert!(hint.contains("worktree"), "{hint}");
        assert!(hint.contains("cannot be undone"), "{hint}");

        // The Ctrl arms that already existed still work, above and below.
        a.disarm_ctrl_x();
        assert_eq!(a.on_key(ctrl('d')), Action::Redraw);
        assert_eq!(a.on_key(ctrl('u')), Action::Redraw);
        assert_eq!(a.on_key(ctrl('z')), Action::None, "unbound Ctrl chords stay inert");
    }

    #[test]
    fn a_second_ctrl_x_inside_the_window_deletes_the_captured_session() {
        agents::test_spawn::reset();
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);

        a.on_key(ctrl('x'));
        after(&mut a, BEAT);
        assert_eq!(a.on_key(ctrl('x')), Action::Redraw);

        // The press SCHEDULES; it does not delete. Nothing has reached the
        // boundary yet, and the window is consumed either way.
        assert_eq!(agents::test_spawn::joined(), vec!["stop 1c45d64f"]);
        assert!(a.stop_arm.is_none(), "the window closes on the second press");
        assert_eq!(
            a.pending_delete.as_ref().map(|p| p.target.short_id.clone()),
            Some("1c45d64f".to_string())
        );

        assert!(settle(&mut a), "the settled delete must ask for a redraw");
        assert_eq!(agents::test_spawn::joined(), vec!["stop 1c45d64f", "rm 1c45d64f"]);
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Warn);
        assert_eq!(text, "deleted bt/reg-update + worktree");
        assert!(a.pending_delete.is_none());
    }

    /// "Press again within two seconds" — after that it is a first press again,
    /// and a first press stops. It must never delete on the strength of a
    /// window that has closed.
    #[test]
    fn a_press_after_the_window_lapses_only_stops_again() {
        agents::test_spawn::reset();
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);

        a.on_key(ctrl('x'));
        let armed_at = a.stop_arm.as_ref().map(|arm| arm.at).expect("armed");
        // Three seconds later: past `CX_WINDOW`, and a clear gap.
        if let Some(arm) = a.stop_arm.as_mut() {
            arm.at = armed_at.checked_sub(Duration::from_secs(3)).unwrap_or(armed_at);
        }
        after(&mut a, Duration::from_secs(3));

        a.on_key(ctrl('x'));
        assert!(a.pending_delete.is_none(), "a lapsed window must not delete");
        assert_eq!(agents::test_spawn::joined(), vec!["stop 1c45d64f", "stop 1c45d64f"]);
        assert!(a.stop_arm.is_some(), "and it opens a fresh window");
        settle(&mut a);
        assert!(!agents::test_spawn::joined().iter().any(|c| c.starts_with("rm ")));
    }

    /// The window expires on its own, with no key pressed — the footer must
    /// stop promising a verb that is no longer loaded.
    #[test]
    fn the_window_expires_on_the_event_loop_tick() {
        agents::test_spawn::reset();
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);
        a.on_key(ctrl('x'));
        assert!(a.arm_hint().is_some());

        assert!(!a.tick_stop_arm(), "a live window is not expired");
        if let Some(arm) = a.stop_arm.as_mut() {
            arm.at = arm.at.checked_sub(Duration::from_secs(3)).unwrap_or(arm.at);
        }
        assert!(a.tick_stop_arm(), "an expired window must ask for a redraw");
        assert!(a.stop_arm.is_none());
        assert!(a.arm_hint().is_none());
    }

    /// Moving the cursor between the presses. The second press deletes nothing
    /// AND stops nothing: re-arming on the new row would stop an innocent agent
    /// on two deliberate presses whenever the just-stopped row moved out from
    /// under the cursor.
    #[test]
    fn a_second_ctrl_x_on_another_row_deletes_nothing_and_stops_nothing() {
        agents::test_spawn::reset();
        let mut a = app();
        load(
            &mut a,
            vec![
                bg("1c45d64f", "bt/reg-update", State::Working),
                bg("629da7fc", "kernel bugs", State::Working),
            ],
        );

        a.on_key(ctrl('x'));
        assert_eq!(agents::test_spawn::joined(), vec!["stop 1c45d64f"]);
        a.on_key(press('j'));
        assert_eq!(
            a.selected_session().map(|s| s.name.clone()),
            Some("kernel bugs".to_string())
        );

        after(&mut a, BEAT);
        a.on_key(ctrl('x'));
        assert!(a.pending_delete.is_none());
        assert!(a.stop_arm.is_none(), "the window closes rather than moving");
        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop 1c45d64f"],
            "the neighbour must not be stopped, and nothing deleted"
        );
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Warn);
        assert_eq!(text, "moved off bt/reg-update — nothing deleted");
    }

    /// A poll landing between the presses re-sorts the list. The cursor follows
    /// the session by key, and the delete runs against the id CAPTURED at the
    /// first press — never against whatever the cursor now indexes.
    #[test]
    fn a_poll_that_re_sorts_the_list_between_presses_still_deletes_the_captured_id() {
        agents::test_spawn::reset();
        let mut a = app();
        let target = bg("1c45d64f", "bt/reg-update", State::Working);
        let other = bg("629da7fc", "kernel bugs", State::Working);
        load(&mut a, vec![target.clone(), other.clone()]);
        assert_eq!(a.selected, 1, "row 0 is the group header");

        a.on_key(ctrl('x'));
        // The poll comes back with the rows the other way round.
        a.sessions = vec![other, target];
        a.rebuild_rows();
        assert_eq!(
            a.selected_session().map(|s| s.id.clone().unwrap_or_default()),
            Some("1c45d64f".to_string()),
            "the cursor follows the session by key"
        );

        after(&mut a, BEAT);
        a.on_key(ctrl('x'));
        settle(&mut a);
        assert_eq!(agents::test_spawn::joined(), vec!["stop 1c45d64f", "rm 1c45d64f"]);
    }

    /// The session goes away between the qualifying press and the settle. The
    /// captured id is re-validated against the current poll and the delete is
    /// abandoned — `claude rm` is never built.
    #[test]
    fn a_session_that_vanishes_before_the_settle_is_not_deleted() {
        agents::test_spawn::reset();
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);

        a.on_key(ctrl('x'));
        after(&mut a, BEAT);
        a.on_key(ctrl('x'));
        assert!(a.pending_delete.is_some());

        a.sessions.clear();
        a.rebuild_rows();
        settle(&mut a);

        assert_eq!(agents::test_spawn::joined(), vec!["stop 1c45d64f"]);
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Warn);
        assert_eq!(text, "session 1c45d64f is gone — not deleted");
    }

    /// A burst the tty had already buffered arrives as N events in the same
    /// instant. Exactly one stop comes out, and no delete — whatever N is.
    #[test]
    fn a_burst_of_buffered_ctrl_x_stops_once_and_deletes_nothing() {
        agents::test_spawn::reset();
        let mut a = app();
        load(
            &mut a,
            vec![
                bg("1c45d64f", "bt/reg-update", State::Working),
                bg("629da7fc", "kernel bugs", State::Working),
            ],
        );

        for _ in 0..8 {
            a.on_key(ctrl('x'));
        }
        settle(&mut a);
        assert_eq!(agents::test_spawn::joined(), vec!["stop 1c45d64f"]);
        assert!(a.pending_delete.is_none());
        // The window the first press opened survives the burst, so a human who
        // merely typed too fast still has it.
        assert!(a.stop_arm.is_some());
    }

    /// A held key, RELEASED just after its first auto-repeat — the stream a
    /// hold makes when the finger comes off inside the first repeat interval.
    /// There is no third event to cancel a settling delete, so `CX_SETTLE`
    /// cannot save this one and `CX_MIN_GAP` is the whole defence. At every
    /// stock auto-repeat delay the repeat must therefore not even QUALIFY.
    ///
    /// This is the shape that deleted a live session and its worktree while
    /// `CX_MIN_GAP` was 250 ms: `KeyEventKind::Press`-only events make "press,
    /// wait 660 ms, press, release" and "hold for 665 ms" the same stream, so
    /// nothing downstream of the gap can separate them.
    #[test]
    fn a_held_ctrl_x_released_after_its_first_repeat_deletes_nothing() {
        // GNOME's default, KDE's, and X11 `xset`'s.
        for delay in [500u64, 600, 660] {
            agents::test_spawn::reset();
            let mut a = app();
            load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);

            a.on_key(ctrl('x'));
            after(&mut a, Duration::from_millis(delay));
            a.on_key(ctrl('x'));
            assert!(
                a.pending_delete.is_none(),
                "a {delay} ms repeat delay must not qualify as a second press"
            );
            // The key comes up here: nothing else ever arrives.
            settle(&mut a);
            a.tick_stop_arm();
            assert_eq!(
                agents::test_spawn::joined(),
                vec!["stop 1c45d64f"],
                "a hold at a {delay} ms repeat delay must stop once and delete nothing"
            );
            // And the window is still open, so the operator who really did mean
            // to press twice has not lost it.
            assert!(a.stop_arm.is_some());
        }
    }

    /// A held key whose repeat delay was configured ABOVE `CX_MIN_GAP` — past
    /// what any stock setting does. The first repeat qualifies there, so the
    /// stream itself has to cancel it: that is what `CX_SETTLE` is for, and it
    /// is why the delete does not run inside the keypress.
    #[test]
    fn a_held_ctrl_x_stops_once_and_never_deletes() {
        agents::test_spawn::reset();
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);

        a.on_key(ctrl('x'));
        after(&mut a, Duration::from_millis(900));
        a.on_key(ctrl('x'));
        assert!(a.pending_delete.is_some(), "a 900 ms repeat delay does qualify");

        // The rest of the stream, 25–40 ms apart. The first one kills it.
        a.on_key(ctrl('x'));
        assert!(a.pending_delete.is_none(), "a repeat stream must cancel the settle");
        for _ in 0..40 {
            assert_eq!(a.on_key(ctrl('x')), Action::Redraw);
        }
        settle(&mut a);
        a.tick_stop_arm();
        assert_eq!(agents::test_spawn::joined(), vec!["stop 1c45d64f"]);
    }

    /// REGRESSION. Two `Ctrl+X` keystrokes ~100 ms apart deleted a live session
    /// and its git worktree, because the first press BLOCKED the UI inside
    /// `claude stop` (0.66 s) plus the forced refresh (0.19 s) while the burst
    /// guard's clock ran. The second press was dequeued ~0.85 s after the first
    /// was stamped, read a gap wider than `CX_MIN_GAP`, and qualified as the
    /// deliberate second press — before any frame carrying the warning had ever
    /// been drawn.
    ///
    /// This is the ONE test here that blocks for real: the whole bug is that
    /// the gap came from the shell-out rather than from the operator, and a
    /// backdated clock cannot express that. `slow_next` blocks for longer than
    /// `CX_MIN_GAP`, which is the worst case for the guard.
    #[test]
    fn a_burst_across_a_blocking_stop_deletes_nothing() {
        agents::test_spawn::reset();
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);

        // Press 1 lands, and `claude stop` freezes the UI for longer than the
        // guard's whole bar. Press 2 was already in the tty buffer.
        agents::test_spawn::slow_next(CX_MIN_GAP + Duration::from_millis(50));
        a.on_key(ctrl('x'));
        a.on_key(ctrl('x'));

        assert!(
            a.pending_delete.is_none(),
            "the buffered press must not qualify: it answered no warning"
        );
        settle(&mut a);
        a.tick_stop_arm();
        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop 1c45d64f"],
            "a burst across a blocking stop performs exactly one stop"
        );
        assert!(a.stop_arm.is_some(), "and the window is still the operator's");
    }

    /// REGRESSION, same class, the other blocking shell-out. `run_delete` runs
    /// `claude rm` plus a forced refresh from the event-loop TICK, where no
    /// keypress stamped anything. A `Ctrl+X` buffered during that freeze used
    /// to dequeue with a stale gap, read as a fresh FIRST press, and stop
    /// whatever row the cursor fell to when the deleted session left the list.
    #[test]
    fn a_ctrl_x_buffered_during_the_delete_stops_nothing() {
        agents::test_spawn::reset();
        let mut a = app();
        load(
            &mut a,
            vec![
                bg("1c45d64f", "bt/reg-update", State::Working),
                bg("629da7fc", "kernel bugs", State::Working),
            ],
        );

        a.on_key(ctrl('x'));
        after(&mut a, BEAT);
        a.on_key(ctrl('x'));

        // The delete blocks; the session it removed leaves the list, and the
        // cursor falls to the neighbour.
        agents::test_spawn::slow_next(CX_MIN_GAP + Duration::from_millis(50));
        settle(&mut a);
        a.sessions.retain(|s| s.id.as_deref() != Some("1c45d64f"));
        a.rebuild_rows();
        assert_eq!(
            a.selected_session().map(|s| s.name.clone()),
            Some("kernel bugs".to_string())
        );

        // The press the operator made at a frozen screen.
        a.on_key(ctrl('x'));
        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop 1c45d64f", "rm 1c45d64f"],
            "the neighbour must not be stopped by a press aimed at a frozen UI"
        );
        assert!(a.stop_arm.is_none());
    }

    /// A suppressed press must SAY it was suppressed. Before this it returned
    /// `Action::None`, so not even a redraw happened and the stale line from
    /// the press before it stayed on screen — the sidebar read as wedged.
    #[test]
    fn a_press_inside_the_burst_guard_says_so_instead_of_going_silent() {
        agents::test_spawn::reset();
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);

        // Arm, then close the window by moving off and pressing again.
        a.on_key(ctrl('x'));
        a.on_key(press('j'));
        after(&mut a, BEAT);
        a.on_key(ctrl('x'));
        assert!(a.stop_arm.is_none(), "the window is closed");

        // A press right behind it: suppressed, but not silent.
        assert_eq!(
            a.on_key(ctrl('x')),
            Action::Redraw,
            "a suppressed press must still ask for a redraw"
        );
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "too fast — press Ctrl+X again");
        assert_eq!(level, MsgLevel::Warn);
        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop 1c45d64f"],
            "and it still acts on nothing"
        );
        // While the window IS open the footer keeps the warning: the flash is
        // outranked, so it can never displace what the next press will do.
        a.on_key(ctrl('x'));
        assert!(a.arm_hint().is_none(), "nothing is armed here");
    }

    /// The refusal has to name the right cause. A row that LEFT the list — `a`
    /// hiding Completed, or a `/` filter matching the worktree path that
    /// `claude stop` reverts — is not the operator moving the cursor, and the
    /// recovery is different.
    #[test]
    fn a_row_that_leaves_the_list_refuses_differently_from_a_cursor_move() {
        agents::test_spawn::reset();
        let mut a = app();
        load(
            &mut a,
            vec![
                bg("1c45d64f", "bt/reg-update", State::Working),
                bg("629da7fc", "kernel bugs", State::Working),
            ],
        );

        a.on_key(ctrl('x'));
        // The row leaves the list under a standing filter, cursor untouched.
        a.filter = "kernel".to_string();
        a.rebuild_rows();
        assert!(!a.is_visible(
            &a.sessions
                .iter()
                .find(|s| s.id.as_deref() == Some("1c45d64f"))
                .map(|s| s.session_id.clone())
                .expect("target")
        ));

        after(&mut a, BEAT);
        a.on_key(ctrl('x'));
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "bt/reg-update left the list — nothing deleted");
        assert_eq!(level, MsgLevel::Warn);
        settle(&mut a);
        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop 1c45d64f"],
            "it still refuses: nothing stopped, nothing deleted"
        );
    }

    /// The window belongs to the list in Normal mode. Anything that leaves it
    /// closes the window, and so does quitting.
    #[test]
    fn the_delete_window_does_not_survive_a_mode_change_or_q() {
        for leave in [press('/'), press('n'), press('?'), press('q')] {
            agents::test_spawn::reset();
            let mut a = app();
            load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);
            a.on_key(ctrl('x'));
            assert!(a.stop_arm.is_some());

            a.on_key(leave);
            assert!(a.stop_arm.is_none(), "{leave:?} must close the window");
            assert!(a.pending_delete.is_none());

            // And a Ctrl+X arriving after that is a FIRST press, never a delete.
            a.mode = Mode::Normal;
            a.should_quit = false;
            after(&mut a, BEAT);
            a.on_key(ctrl('x'));
            settle(&mut a);
            assert!(!agents::test_spawn::joined().iter().any(|c| c.starts_with("rm ")));
        }

        // Ctrl-c quits from any mode and takes the window with it.
        agents::test_spawn::reset();
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);
        a.on_key(ctrl('x'));
        assert_eq!(a.on_key(ctrl('c')), Action::Quit);
        assert!(a.stop_arm.is_none());
    }

    #[test]
    fn esc_closes_the_delete_window() {
        agents::test_spawn::reset();
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);
        a.on_key(ctrl('x'));
        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(a.stop_arm.is_none());
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "delete window closed");
    }

    /// A stop that failed must not open a delete window: the escalation is only
    /// ever an escalation of a stop that happened.
    #[test]
    fn a_failed_stop_does_not_open_the_delete_window() {
        agents::test_spawn::reset();
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);
        agents::test_spawn::fail_next("daemon is not running");

        a.on_key(ctrl('x'));
        assert!(a.stop_arm.is_none());
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Error);
        assert_eq!(text, "stop failed: daemon is not running");
    }

    /// A row already in Completed has nothing to stop — and refusing there
    /// would strand the session the PREVIOUS press stopped, which is Completed
    /// by the time the window lapses. `claude rm` works on already-exited
    /// sessions, so the press arms without shelling out.
    #[test]
    fn ctrl_x_on_a_completed_row_arms_for_delete_without_stopping() {
        agents::test_spawn::reset();
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Stopped)]);
        assert_eq!(a.selected_session().map(|s| s.group()), Some(Group::Completed));

        a.on_key(ctrl('x'));
        assert!(agents::test_spawn::calls().is_empty(), "nothing to stop");
        assert!(a.stop_arm.is_some());
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "bt/reg-update is already stopped");

        after(&mut a, BEAT);
        a.on_key(ctrl('x'));
        settle(&mut a);
        assert_eq!(agents::test_spawn::joined(), vec!["rm 1c45d64f"]);
    }

    /// `S` is REMOVED, not redirected. Stop lives on `Ctrl+X` and `S` is an
    /// unbound key like any other letter: no modal, no capture, no `claude`
    /// call, and no message — pressing it must not even redraw, or it would
    /// still read as a key that means something.
    #[test]
    fn s_is_completely_unbound() {
        agents::test_spawn::reset();
        let mut a = app();
        load(&mut a, vec![bg("1c45d64f", "bt/reg-update", State::Working)]);

        // Uppercase arrives with SHIFT set — asserted so a future keymap change
        // cannot make `S` inert only because the modifier check is wrong.
        assert_eq!(
            a.on_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT)),
            Action::None
        );
        assert_eq!(a.mode, Mode::Normal, "no modal");
        assert!(a.stop_arm.is_none(), "no window");
        assert!(agents::test_spawn::calls().is_empty(), "no claude call");
        assert!(a.message.is_none(), "unbound keys say nothing");
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

    /// `is_attachable` is a LIVE gate, not a leftover of the interactive era:
    /// `parse_sessions` honours an explicit `kind: "background"` even when the
    /// CLI omits `id`, so such a row is listed and has no id to hand `claude`.
    /// `S`, `Enter` and `o` must all refuse it without shelling out.
    #[test]
    fn a_listed_row_with_no_short_id_refuses_every_id_verb() {
        let mut a = app();
        let mut no_id = bg("11111111", "half-parsed", State::Working);
        no_id.id = None;
        load(&mut a, vec![no_id]);

        let act = a.on_key(ctrl('x'));
        assert_eq!(act, Action::Redraw);
        assert_eq!(a.mode, Mode::Normal);
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Warn);
        assert_eq!(text, "no short id — cannot stop this session");
        // Refused BEFORE the boundary, and with no window opened: a session
        // `claude` cannot be told to stop is one it cannot be told to delete.
        assert!(a.stop_arm.is_none(), "a refused verb must not arm");
        assert!(agents::test_spawn::calls().is_empty());

        // `Enter` and `o` refuse BEFORE any tmux call: there is no map entry
        // and no pane, so a fallthrough would shell out at `split`.
        for k in [KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), press('o')] {
            a.message = None;
            assert_eq!(a.on_key(k), Action::Redraw);
            let (text, level) = a.message.clone().unwrap_or_default_msg();
            assert_eq!(level, MsgLevel::Warn);
            assert_eq!(text, "no short id — cannot open this session");
        }
        assert!(a.map.panes.is_empty());
        assert!(!a.map_dirty, "a refused verb must not dirty the map");
    }

    #[test]
    fn pane_of_resolves_through_the_map_only() {
        let mut a = app();
        let bg_pane = PaneId::parse("%25").expect("valid pane id");

        a.map.insert(
            &bg_pane,
            PaneEntry {
                session_id: "1c45d64f-uuid".into(),
                short_id: "1c45d64f".into(),
                name: "bt/reg-update".into(),
                opened_at: 0,
            },
        );

        a.rebuild_open();
        assert_eq!(a.pane_of("1c45d64f-uuid"), Some(bg_pane));
        assert_eq!(a.pane_of("nobody"), None);
        assert!(!a.is_open("nobody"));
    }

    // ── tabs ────────────────────────────────────────────────────────────────

    fn tab(window: &str, index: u32, sidebar: Option<&str>) -> TabInfo {
        TabInfo {
            window: WindowId::parse(window).expect("window id"),
            index,
            sidebar: sidebar.and_then(PaneId::parse),
            map: PaneMap::new(),
            hidden: HiddenLog::new(),
        }
    }

    /// What the tmux server holds after both sidebars have flushed: one
    /// fragment per window, each written by exactly one process.
    fn server(frags: &[(&str, u32, &str, &HiddenLog)]) -> Vec<TabInfo> {
        frags
            .iter()
            .map(|(w, i, sb, log)| {
                let mut t = tab(w, *i, Some(sb));
                t.hidden = (*log).clone();
                t
            })
            .collect()
    }

    fn entry(session_id: &str) -> PaneEntry {
        PaneEntry {
            session_id: session_id.into(),
            short_id: session_id.chars().take(8).collect(),
            name: "n".into(),
            opened_at: 0,
        }
    }

    /// `t` refuses exactly where `o`/`s` do, and refuses BEFORE issuing any
    /// tmux command — which is what lets this run hermetically.
    #[test]
    fn t_refuses_a_session_with_no_short_id_and_outside_tmux() {
        // §9.7: `claude attach` takes the 8-hex short id, so without one the
        // new tab's first pane would have nothing to run.
        let mut a = app();
        a.sidebar_cmd = Some("ccmux sidebar".into());
        let mut orphan = bg("aaaaaaaa", "no id here", State::Working);
        orphan.id = None;
        load(&mut a, vec![orphan]);
        a.on_key(press('t'));
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "no short id — cannot open this session"
        );
        assert!(a.panes.is_empty(), "nothing was created");

        let mut d = app();
        d.degraded = true;
        d.sidebar_cmd = Some("ccmux sidebar".into());
        load(&mut d, four());
        d.on_key(press('t'));
        assert_eq!(
            d.message.clone().unwrap_or_default_msg().0,
            "not inside tmux — tabs unavailable"
        );

        // And with no sidebar command there is nothing to run in the new tab's
        // sidebar, so the tab is not created half-built.
        let mut n = app();
        load(&mut n, four());
        n.on_key(press('t'));
        assert_eq!(
            n.message.clone().unwrap_or_default_msg().0,
            "sidebar command unknown — cannot open a tab"
        );
    }

    /// Per-window sidebar resolution, as a pure rule.
    #[test]
    fn each_tab_resolves_its_own_sidebar_and_registers_only_when_it_must() {
        let mut a = app();
        own(&mut a, "%2", "@2");
        a.panes = vec![pane("%1", 1, 1, 0, 34, false), pane("%2", 2, 1, 0, 34, false)];

        // Marker unset: I am the sidebar, and I must say so.
        a.tabs = vec![tab("@1", 1, Some("%1")), tab("@2", 2, None)];
        assert_eq!(a.sidebar_choice(), (PaneId::parse("%2"), true));

        // Marker already names me: nothing to write.
        a.tabs = vec![tab("@1", 1, Some("%1")), tab("@2", 2, Some("%2"))];
        assert_eq!(a.sidebar_choice(), (PaneId::parse("%2"), false));

        // Marker names ANOTHER tab's sidebar — the trap a single session-wide
        // `@ccmux_sidebar` fell into. It is not mine and must not be pinned by
        // me; I register instead.
        a.tabs = vec![tab("@1", 1, Some("%1")), tab("@2", 2, Some("%1"))];
        assert_eq!(a.sidebar_choice(), (PaneId::parse("%2"), true));

        // Marker names a dead pane in my window: register.
        a.tabs = vec![tab("@2", 2, Some("%99"))];
        assert_eq!(a.sidebar_choice(), (PaneId::parse("%2"), true));

        // A second sidebar hand-launched into a window that already has one
        // ADOPTS it rather than stealing the marker.
        let mut b = app();
        own(&mut b, "%3", "@2");
        b.panes = vec![
            pane("%2", 2, 1, 0, 34, false),
            pane("%3", 2, 2, 34, 34, false),
        ];
        b.tabs = vec![tab("@2", 2, Some("%2"))];
        assert_eq!(b.sidebar_choice(), (PaneId::parse("%2"), false));

        // No own pane at all: keep a live cached id, drop a dead one, never
        // invent one.
        let mut c = app();
        c.panes = vec![pane("%1", 1, 1, 0, 34, false)];
        c.sidebar_pane = PaneId::parse("%1");
        assert_eq!(c.sidebar_choice(), (PaneId::parse("%1"), false));
        c.sidebar_pane = PaneId::parse("%99");
        assert_eq!(c.sidebar_choice(), (None, false));
    }

    /// Pinning follows the sidebar, so with a sidebar per window it pins THIS
    /// window and no other. `split_anchor` already scoped itself the same way
    /// and is verified here rather than churned.
    #[test]
    fn layout_stays_inside_the_tab_that_owns_the_sidebar() {
        let mut a = app();
        own(&mut a, "%2", "@2");
        a.panes = vec![
            pane("%0", 1, 1, 0, 34, false),
            pane("%1", 1, 2, 34, 100, true),
            pane("%2", 2, 1, 0, 34, false),
            pane("%3", 2, 2, 34, 60, false),
        ];
        a.tabs = vec![tab("@1", 1, Some("%0")), tab("@2", 2, Some("%2"))];
        a.sidebar_pane = PaneId::parse("%2");

        // Tab 1's window is 134 columns wide and tab 2's is 94; pinning must
        // measure MINE.
        assert_eq!(a.pinned_width_from(34), Some(34));
        assert_eq!(a.pinned_width_from(200), Some(94 - MIN_CONTENT_COLS));
        // The anchor is a pane of my window. Unscoped, step 1 would return
        // `%1` — the active pane of a tab the operator is not looking at.
        assert_eq!(a.split_anchor(), PaneId::parse("%3"));
    }

    /// THE concurrency claim, through the real keypress path: two sidebars
    /// dismissing different rows in the same tick each write their OWN window
    /// option, so neither reverts the other.
    #[test]
    fn two_tabs_dismissing_in_one_tick_do_not_revert_each_other() {
        let mut a = app();
        own(&mut a, "%1", "@1");
        load(&mut a, four());
        let mut b = app();
        own(&mut b, "%2", "@2");
        load(&mut b, four());

        // Neither has seen the other: both are working from the same empty
        // shared set, which is exactly the race.
        a.select_first();
        let id_a = a.selected_key.clone().expect("a row");
        a.on_key(press('d'));
        b.select_first();
        b.select_next();
        let id_b = b.selected_key.clone().expect("another row");
        b.on_key(press('d'));
        assert_ne!(id_a, id_b);
        // Same wall-clock millisecond, so the stamps collide and the origin
        // window is what orders them — deterministically, for every reader.
        assert_eq!(a.hidden_log.ops[0].seq, b.hidden_log.ops[0].seq);

        // Both flush. Two different tmux options; both writes land.
        let tabs = server(&[
            ("@1", 1, "%1", &a.hidden_log),
            ("@2", 2, "%2", &b.hidden_log),
        ]);
        for app in [&mut a, &mut b] {
            app.panes = vec![pane("%1", 1, 1, 0, 34, false), pane("%2", 2, 1, 0, 34, false)];
            app.tabs = tabs.clone();
            app.refold_hidden();
            app.rebuild_rows();
        }

        for (who, app) in [("tab 1", &a), ("tab 2", &b)] {
            assert!(app.hidden.ids().contains(&id_a), "{who} lost tab 1's dismissal");
            assert!(app.hidden.ids().contains(&id_b), "{who} lost tab 2's dismissal");
            assert_eq!(session_rows(app).len(), 2, "{who} shows both rows hidden");
        }
        // Each still writes only its own fragment — nobody adopted the other's
        // op, so there is no key with two authors.
        assert_eq!(a.hidden_log.ops.len(), 1);
        assert_eq!(b.hidden_log.ops.len(), 1);
    }

    /// The undo stack is shared, and `u` in either tab pops the newest
    /// dismissal by identity — never by position, so nothing can misapply it.
    #[test]
    fn undo_in_one_tab_restores_the_newest_dismissal_from_either() {
        let mut a = app();
        own(&mut a, "%1", "@1");
        load(&mut a, four());
        let mut theirs = HiddenLog::new();
        theirs.push(HiddenOp {
            id: "bbbbbbbb-uuid".into(),
            add: true,
            seq: a.now_ms as u64 + 500,
            org: 2,
        });
        a.panes = vec![pane("%1", 1, 1, 0, 34, false), pane("%2", 2, 1, 0, 34, false)];
        a.tabs = server(&[("@1", 1, "%1", &a.hidden_log), ("@2", 2, "%2", &theirs)]);
        a.refold_hidden();
        a.rebuild_rows();
        assert_eq!(a.hidden.ids(), ["bbbbbbbb-uuid"]);

        a.on_key(press('u'));
        assert!(a.hidden.ids().is_empty(), "the other tab's dismissal came back");
        // Restored by a TOMBSTONE in my own fragment, not by editing theirs.
        let mine = a.hidden_log.ops.last().expect("a tombstone");
        assert_eq!((mine.id.as_str(), mine.add, mine.org), ("bbbbbbbb-uuid", false, 1));
        assert!(mine.seq > a.now_ms as u64 + 500, "a tombstone always outranks its target");
        // And it survives the fold against the fragment it undoes.
        a.tabs = server(&[("@1", 1, "%1", &a.hidden_log), ("@2", 2, "%2", &theirs)]);
        a.refold_hidden();
        assert!(a.hidden.ids().is_empty());
    }

    /// A window option dies with its window. Without adoption, closing a tab
    /// would silently un-hide every row it dismissed.
    #[test]
    fn an_orphaned_fragment_is_adopted_by_the_lowest_live_tab() {
        let mut a = app();
        own(&mut a, "%1", "@1");
        a.panes = vec![pane("%1", 1, 1, 0, 34, false), pane("%2", 2, 1, 0, 34, false)];
        let mut theirs = HiddenLog::new();
        theirs.push(HiddenOp { id: "gone-tab-uuid".into(), add: true, seq: 900, org: 2 });
        a.tabs = server(&[("@1", 1, "%1", &a.hidden_log), ("@2", 2, "%2", &theirs)]);
        a.adopt_orphan_fragments();
        a.refold_hidden();
        assert_eq!(a.hidden.ids(), ["gone-tab-uuid"]);
        assert!(a.hidden_log.ops.is_empty(), "nothing to adopt while tab 2 lives");

        // Tab 2 closes: its window, and its window option, are gone.
        a.panes = vec![pane("%1", 1, 1, 0, 34, false)];
        a.tabs = server(&[("@1", 1, "%1", &a.hidden_log)]);
        a.adopt_orphan_fragments();
        a.refold_hidden();
        assert_eq!(a.hidden.ids(), ["gone-tab-uuid"], "the dismissal outlived the tab");
        assert!(a.hidden_dirty, "the adopted op is owed to tmux");
        // Adopted VERBATIM: the stamp and origin are preserved, so adoption
        // changes where an op is stored and nothing about the fold.
        assert_eq!(a.hidden_log.ops, vec![HiddenOp {
            id: "gone-tab-uuid".into(),
            add: true,
            seq: 900,
            org: 2
        }]);

        // Only the lowest live tab adopts, so two survivors cannot both claim
        // it and diverge.
        let mut b = app();
        own(&mut b, "%2", "@2");
        b.panes = vec![pane("%1", 1, 1, 0, 34, false), pane("%2", 2, 1, 0, 34, false)];
        b.last_frags = [(3u64, theirs.clone())].into_iter().collect();
        b.tabs = server(&[("@1", 1, "%1", &HiddenLog::new()), ("@2", 2, "%2", &b.hidden_log)]);
        b.adopt_orphan_fragments();
        assert!(b.hidden_log.ops.is_empty(), "tab 2 is not the adopter");
    }

    /// REGRESSION. `adopt_own_state` used to latch `own_state_loaded` even when
    /// the window enumeration did not carry my window. `refresh_panes` swallows
    /// a failed `list-windows` while `own_window` still resolves from the
    /// successful `list-panes`, so ONE transient failure on the tick that first
    /// resolved identity left the sidebar running for its whole life on an
    /// empty map and an empty dismissal fragment — and then overwriting this
    /// window's durable options with those empties on the next dirty flush.
    #[test]
    fn a_failed_window_enumeration_never_latches_an_empty_state() {
        let mut a = app();
        a.tmux_session = "ccmux-test-adopt".into();
        own(&mut a, "%1", "@1");
        // The fixture starts adopted, because most tests want the steady
        // state; this one is about the very first tick.
        a.own_state_loaded = false;
        a.map.insert(&PaneId::parse("%2").expect("id"), entry("aaaaaaaa-uuid"));

        // The tick `list-windows` failed on: `tabs` empty, identity resolved.
        a.tabs.clear();
        a.adopt_own_state();
        assert!(!a.own_state_loaded, "an empty enumeration is not an adoption");

        // Nothing may be flushed from an un-adopted base. The dedupe cache is
        // pre-seeded with exactly the JSON an UNGUARDED flush would write, so
        // it would succeed and clear the flag without reaching tmux — the
        // guard is the only thing that can leave the flag standing.
        a.map_dirty = true;
        a.hidden_dirty = true;
        let win = WindowId::parse("@1").expect("window id");
        crate::tmux::seed_saved_value(
            &a.tmux_session,
            crate::tmux::OPT_TAB_MAP,
            &win,
            &serde_json::to_string(&a.map).expect("PaneMap serializes"),
        );
        arm_hermetic_flush(&a);
        a.save_map_now();
        a.save_hidden_now();
        assert!(a.map_dirty, "the map was written from an empty base");
        assert!(a.hidden_dirty, "the dismissal log was written from an empty base");

        // The next tick's `list-windows` succeeds. Adoption MERGES: the stored
        // state arrives, and the `o` and `d` pressed while it was failing are
        // still there.
        let mut stored = tab("@1", 1, Some("%1"));
        stored.map.insert(&PaneId::parse("%7").expect("id"), entry("bbbbbbbb-uuid"));
        let _ = stored.hidden.push(HiddenOp {
            id: "cccccccc-uuid".into(),
            add: true,
            seq: 5,
            org: 1,
        });
        a.tabs = vec![stored];
        a.adopt_own_state();
        assert!(a.own_state_loaded);
        assert_eq!(a.map.panes.len(), 2, "stored AND interim: {:?}", a.map.panes);
        assert!(a.map.get(&PaneId::parse("%2").expect("id")).is_some(), "the interim `o`");
        assert!(a.map.get(&PaneId::parse("%7").expect("id")).is_some(), "the stored entry");
        assert_eq!(a.map.v, 1, "the adopted map carries the current schema version");
        assert_eq!(a.hidden_log.ops.len(), 1, "the stored dismissal survived");
    }

    /// `open` is the union of every tab's map intersected with the live pane
    /// list. The intersection is what makes a cross-tab `x` correct in the same
    /// frame it happens, without anyone writing to a window they do not own.
    #[test]
    fn open_spans_every_tab_and_drops_a_pane_the_moment_it_dies() {
        let mut a = app();
        own(&mut a, "%1", "@1");
        a.panes = vec![
            pane("%1", 1, 1, 0, 34, false),
            pane("%5", 1, 2, 34, 60, false),
            pane("%2", 2, 1, 0, 34, false),
            pane("%9", 2, 2, 34, 60, false),
        ];
        a.map.insert(&PaneId::parse("%5").expect("id"), entry("aaaaaaaa-uuid"));
        let mut theirs = tab("@2", 2, Some("%2"));
        theirs.map.insert(&PaneId::parse("%9").expect("id"), entry("bbbbbbbb-uuid"));
        a.tabs = vec![tab("@1", 1, Some("%1")), theirs.clone()];
        a.rebuild_open();

        assert_eq!(a.pane_of("aaaaaaaa-uuid"), PaneId::parse("%5"));
        assert_eq!(a.pane_of("bbbbbbbb-uuid"), PaneId::parse("%9"), "a pane in another tab");
        assert_eq!(a.open["aaaaaaaa-uuid"].window_index, Some(1));
        assert_eq!(a.open["bbbbbbbb-uuid"].window_index, Some(2));
        // The flash names the tab only when it is not mine.
        assert_eq!(a.tab_suffix(&PaneId::parse("%5").expect("id")), "");
        assert_eq!(a.tab_suffix(&PaneId::parse("%9").expect("id")), " in tab 2");

        // Tab 2 kills %9 and writes NOTHING of tab 1's. The next pane listing
        // is all it takes for every process to agree it is gone.
        a.panes.retain(|p| p.id.as_str() != "%9");
        a.rebuild_open();
        assert_eq!(a.pane_of("bbbbbbbb-uuid"), None);
        assert_eq!(a.tabs[1].map.panes.len(), 1, "the stale entry was not ours to remove");

        // Two panes for one session, one of them MINE: `Enter` and `x` take
        // the one in this tab, even though the other's id is numerically
        // lower. See `a_session_open_in_two_tabs_resolves_to_the_one_in_mine`.
        a.panes.push(pane("%3", 2, 2, 34, 60, false));
        theirs.map.insert(&PaneId::parse("%3").expect("id"), entry("aaaaaaaa-uuid"));
        a.tabs = vec![tab("@1", 1, Some("%1")), theirs];
        a.rebuild_open();
        assert_eq!(a.pane_of("aaaaaaaa-uuid"), PaneId::parse("%5"));
    }

    /// THE CONSEQUENCE OF THE SHELL HANDOFF. `claude attach` used to end with
    /// the pane: `Ctrl+Z` exited it, the wrapper printed a notice and blocked on
    /// `read`, and the operator's Enter closed the pane, taking the map entry
    /// with it. Now the pane parks on a resume prompt instead — and holds the
    /// operator's shell if they ask for one — so it lives indefinitely and
    /// `@ccmux_tab_map` goes on naming a pane that stopped
    /// showing the session — and every claim built on that map is wrong:
    /// `is_open`'s marker, the tab badge, `Enter`'s jump, and `R`'s licence to
    /// respawn. `#{@ccmux_detached}` is the pane saying it made the transition.
    ///
    /// It is reconciled to MAPPED BUT NOT ATTACHED, not to unmapped: the pane is
    /// still one ccmux opened, and `x` is the only way ccmux can close what
    /// ccmux opened.
    #[test]
    fn a_detached_pane_stops_being_reported_open_but_stays_ccmuxs_to_close() {
        let mut a = app();
        own(&mut a, "%1", "@1");
        a.panes = vec![pane("%1", 1, 1, 0, 34, false), pane("%5", 1, 2, 34, 60, false)];
        a.map.insert(&PaneId::parse("%5").expect("id"), entry("aaaaaaaa-uuid"));
        a.tabs = vec![tab("@1", 1, Some("%1"))];
        a.rebuild_open();
        assert!(a.is_open("aaaaaaaa-uuid"), "a live attach is open");
        assert_eq!(a.pane_of("aaaaaaaa-uuid"), PaneId::parse("%5"));

        // Ctrl+Z: the attach exits, the pane latches and `exec`s a shell.
        a.panes[1].detached = true;
        a.rebuild_open();
        assert!(!a.is_open("aaaaaaaa-uuid"), "the map may not go on claiming a shell");
        assert_eq!(a.pane_of("aaaaaaaa-uuid"), None, "nothing jumps there any more");
        assert_eq!(
            a.pane_of_any("aaaaaaaa-uuid"),
            PaneId::parse("%5"),
            "`x` must still reach the pane ccmux opened"
        );
        assert!(
            !a.open["aaaaaaaa-uuid"].attached,
            "mapped but not attached — the entry is kept, the claim is not"
        );

        // `reconcile` drops DEAD panes and nothing else; a detached one is very
        // much alive, so the entry survives every tick until the pane does not.
        assert!(!a.map.reconcile(&a.panes), "a live pane is never reconciled away");
        assert!(a.map.get(&PaneId::parse("%5").expect("id")).is_some());

        // ccmux never clears the latch itself. Reattaching by hand in that
        // shell does not give the pane back: `#{pane_current_command}` would
        // report `claude`, and PROBE-FINDINGS §4 says a pane's command never
        // identifies WHICH session it is showing. `Enter` opens a fresh pane
        // instead, which is legal (PROBE §3) and cannot mis-attribute anything.
        assert_eq!(a.pane_of("aaaaaaaa-uuid"), None);

        // The pane itself may, and does: `attach_pane_cmd`'s resume path unsets
        // the option before re-running the SAME attach, so a pane detached and
        // resumed in place is `attached` again on the next tick, with its
        // marker, its badge, `Enter`'s jump and `R` all restored.
        a.panes[1].detached = false;
        a.rebuild_open();
        assert!(a.is_open("aaaaaaaa-uuid"));
        assert_eq!(a.pane_of("aaaaaaaa-uuid"), PaneId::parse("%5"));
    }

    /// `act_enter` is `match self.pane_of(id) { Some(p) => jump, None => open }`
    /// and `act_open` is the only branch that can reach a tmux server, so the
    /// branch itself is what a hermetic test can assert. A detached pane must
    /// take the OPEN arm: jumping the operator into the prompt they just left
    /// and calling it the session would be worse than the second pane it costs.
    #[test]
    fn enter_opens_a_fresh_pane_rather_than_jumping_into_the_shell() {
        let mut a = app();
        own(&mut a, "%1", "@1");
        a.panes = vec![pane("%1", 1, 1, 0, 34, false), pane("%5", 1, 2, 34, 60, false)];
        a.map.insert(&PaneId::parse("%5").expect("id"), entry("aaaaaaaa-uuid"));
        a.tabs = vec![tab("@1", 1, Some("%1"))];
        a.panes[1].detached = true;
        a.rebuild_open();
        assert!(a.pane_of("aaaaaaaa-uuid").is_none(), "the jump arm is unreachable");

        // And `Enter` on it reaches `act_open`, which refuses first when this
        // process has no tmux — proving which arm ran without a server.
        a.degraded = true;
        load(&mut a, vec![bg("aaaaaaaa", "a", State::Working)]);
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "not inside tmux — open unavailable"
        );
    }

    /// REGRESSION, measured live: after `Ctrl+Z`, `Enter` went on jumping into
    /// the shell for up to a whole `tick_interval` (2.5 s by default, up to
    /// 10 s on the idle ladder), because `open` is rebuilt by `tick` and a
    /// keypress is not a tick. Three runs of detach-then-Enter: the first
    /// opened a pane, the second and third jumped to the pane that had just
    /// latched `@ccmux_detached`. `Enter` re-reads the inventory first now, the
    /// way `finish_restart` does.
    ///
    /// Hermetic because an invalid session name fails every tmux call at the
    /// target check, before a process is spawned — and `panes_fresh` is exactly
    /// the flag `refresh_panes` clears when the read did not come back, so it
    /// is proof the verb tried to read.
    #[test]
    fn enter_re_reads_the_pane_inventory_before_it_decides() {
        let mut a = app();
        own(&mut a, "%1", "@1");
        a.panes = vec![pane("%1", 1, 1, 0, 34, false), pane("%5", 1, 2, 34, 60, false)];
        a.map.insert(&PaneId::parse("%5").expect("id"), entry("aaaaaaaa-uuid"));
        a.tabs = vec![tab("@1", 1, Some("%1"))];
        a.rebuild_open();
        load(&mut a, vec![bg("aaaaaaaa", "a", State::Working)]);
        assert!(a.pane_of("aaaaaaaa-uuid").is_some(), "the jump arm is the one under test");

        a.tmux_session = "not a session name".into();
        a.panes_fresh = true;
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!a.panes_fresh, "`Enter` decided on whatever `tick` last left behind");
    }

    /// REGRESSION, found live. Detach from a pane and press `Enter`: the
    /// session now has TWO mapped panes in one tab — the shell you left and the
    /// fresh attach beside it — and the shell is the OLDER, lower-numbered one.
    /// Resolving that by §5.5's numeric rule handed the row the shell, so the
    /// sidebar reported a session it had just opened as not open, would not
    /// jump to it, and pointed `x` at the wrong pane. Attached outranks
    /// everything; a detached pane only represents the session when nothing
    /// else does, and then only so `x` can reach it.
    #[test]
    fn a_live_attach_outranks_the_shell_left_behind_by_an_earlier_one() {
        let mut a = app();
        own(&mut a, "%1", "@1");
        a.panes = vec![
            pane("%1", 1, 1, 0, 34, false),
            pane("%24", 1, 2, 34, 60, false),
            pane("%25", 1, 3, 94, 60, false),
        ];
        a.map.insert(&PaneId::parse("%24").expect("id"), entry("aaaaaaaa-uuid"));
        a.map.insert(&PaneId::parse("%25").expect("id"), entry("aaaaaaaa-uuid"));
        a.tabs = vec![tab("@1", 1, Some("%1"))];

        // %24 detached; %25 is the fresh attach `Enter` just opened.
        a.panes[1].detached = true;
        a.rebuild_open();
        assert!(a.is_open("aaaaaaaa-uuid"), "the session IS on screen, in %25");
        assert_eq!(a.pane_of("aaaaaaaa-uuid"), PaneId::parse("%25"));
        assert_eq!(
            a.pane_of_any("aaaaaaaa-uuid"),
            PaneId::parse("%25"),
            "`x` closes the pane showing the session, not the shell beside it"
        );

        // Close the attach and the shell is what is left to represent the row.
        a.panes.retain(|p| p.id.as_str() != "%25");
        a.rebuild_open();
        assert!(!a.is_open("aaaaaaaa-uuid"));
        assert_eq!(a.pane_of_any("aaaaaaaa-uuid"), PaneId::parse("%24"));

        // A pane in ANOTHER tab that is still attached beats a detached one of
        // mine: "here" only decides between panes of equal standing.
        a.panes = vec![
            pane("%1", 1, 1, 0, 34, false),
            pane("%24", 1, 2, 34, 60, false),
            pane("%99", 2, 1, 0, 60, false),
        ];
        a.panes[1].detached = true;
        let mut theirs = tab("@2", 2, Some("%99"));
        theirs.map.insert(&PaneId::parse("%99").expect("id"), entry("aaaaaaaa-uuid"));
        a.tabs = vec![tab("@1", 1, Some("%1")), theirs];
        a.rebuild_open();
        assert_eq!(a.pane_of("aaaaaaaa-uuid"), PaneId::parse("%99"));
    }

    /// REGRESSION. A session double-attached in two tabs used to resolve to the
    /// globally lowest-numbered pane, so the tab whose pane happened to carry
    /// the higher id had its badge point AWAY from a pane sitting on its own
    /// screen, `Enter` switch the client out of the tab that already had it,
    /// and `x` — destructive — kill the OTHER tab's pane while leaving the
    /// visible one alive. The badge's contract ("blank means open HERE, a digit
    /// ALWAYS means somewhere else") said the opposite of all three.
    #[test]
    fn a_session_open_in_two_tabs_resolves_to_the_one_in_mine() {
        // %2 is in tab 1, %7 in tab 2, both attached to the same session.
        let panes = vec![
            pane("%1", 1, 1, 0, 34, false),
            pane("%2", 1, 2, 34, 60, false),
            pane("%6", 2, 1, 0, 34, false),
            pane("%7", 2, 2, 34, 60, false),
        ];
        let mut one = PaneMap::new();
        one.insert(&PaneId::parse("%2").expect("id"), entry("aaaaaaaa-uuid"));
        let mut two = PaneMap::new();
        two.insert(&PaneId::parse("%7").expect("id"), entry("aaaaaaaa-uuid"));

        // The tab holding the HIGHER pane id is the one the old rule betrayed.
        let mut b = app();
        own(&mut b, "%6", "@2");
        b.panes = panes.clone();
        b.map = two.clone();
        let mut t1 = tab("@1", 1, Some("%1"));
        t1.map = one.clone();
        let mut t2 = tab("@2", 2, Some("%6"));
        t2.map = two.clone();
        b.tabs = vec![t1.clone(), t2.clone()];
        b.rebuild_open();
        assert_eq!(b.pane_of("aaaaaaaa-uuid"), PaneId::parse("%7"), "x must kill MY pane");
        assert_eq!(b.open["aaaaaaaa-uuid"].window, WindowId::parse("@2"));
        assert_eq!(b.tab_suffix(&PaneId::parse("%7").expect("id")), "", "no jump away");

        // The other tab is symmetric — not merely lucky that its id is lower.
        let mut a = app();
        own(&mut a, "%1", "@1");
        a.panes = panes.clone();
        a.map = one;
        a.tabs = vec![t1, t2];
        a.rebuild_open();
        assert_eq!(a.pane_of("aaaaaaaa-uuid"), PaneId::parse("%2"));

        // No own window at all (degraded, or a fixture with no inventory): both
        // panes are equally foreign and §5.5's numeric rule is all that is
        // left, exactly as before.
        a.own_pane = None;
        a.own_window = None;
        a.rebuild_open();
        assert_eq!(a.pane_of("aaaaaaaa-uuid"), PaneId::parse("%2"));
    }

    /// `x` refuses ANY tab's sidebar, not only this one's: closing another
    /// tab's would leave it blind until the next `ccmux` launch.
    #[test]
    fn x_refuses_to_close_any_tabs_sidebar() {
        let mut a = app();
        own(&mut a, "%1", "@1");
        load(&mut a, four());
        a.panes = vec![pane("%1", 1, 1, 0, 34, false), pane("%2", 2, 1, 0, 34, false)];
        a.sidebar_pane = PaneId::parse("%1");
        a.tabs = vec![tab("@1", 1, Some("%1")), tab("@2", 2, Some("%2"))];
        // The selected session is "open" in the OTHER tab's sidebar pane — the
        // shape a corrupt or hand-edited map could produce.
        a.map.insert(&PaneId::parse("%2").expect("id"), entry("aaaaaaaa-uuid"));
        a.rebuild_open();
        a.select_first();
        assert_eq!(a.selected_key.as_deref(), Some("aaaaaaaa-uuid"));

        a.on_key(press('x'));
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "refusing to close the sidebar"
        );
        assert!(a.is_any_sidebar(&PaneId::parse("%2").expect("id")));
        assert!(!a.is_any_sidebar(&PaneId::parse("%9").expect("id")));
    }

    /// The two-strike absence rule must retire the OPS, not only the folded
    /// view: a surviving op would re-hide the id at the next fold.
    #[test]
    fn reconciliation_retires_the_ops_behind_a_dismissal() {
        let mut a = app();
        own(&mut a, "%1", "@1");
        load(&mut a, four());
        a.select_first();
        a.on_key(press('d'));
        let id = "aaaaaaaa-uuid";
        assert_eq!(a.hidden.ids(), [id]);

        // Two consecutive complete polls without it — the debounce, unchanged.
        let rest: Vec<Session> = four().into_iter().filter(|s| s.session_id != id).collect();
        a.apply_poll(complete(rest.clone()));
        assert_eq!(a.hidden.ids(), [id], "one absence concludes nothing");
        a.apply_poll(complete(rest));
        assert!(a.hidden.ids().is_empty());
        assert!(a.hidden_log.ops.is_empty(), "the op went with the dismissal");
        // Which is what makes it stick through the next fold.
        a.refold_hidden();
        assert!(a.hidden.ids().is_empty());
    }

    #[test]
    fn navigation_never_lands_on_a_header_and_survives_an_empty_list() {
        let mut a = app();
        let mut idle = bg("cccccccc", "three", State::Unknown("parked".into()));
        idle.status = Status::Idle;
        load(
            &mut a,
            vec![
                bg("aaaaaaaa", "one", State::Working),
                bg("bbbbbbbb", "two", State::Done),
                idle,
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
        ] {
            let mut a = app();
            a.mode = mode;
            let act = a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert_eq!(act, Action::Quit);
            assert!(a.should_quit);
        }
    }

    /// SPEC §8.7 is a tombstone: `c` used to open the interactive prompt and
    /// now binds nothing. "Nothing" is the assertion — no mode change, no
    /// prompt, no flash — because a key that merely stopped opening a pane but
    /// still flashed or still armed a mode would be a different bug. `Ctrl-c`
    /// is unaffected and keeps quitting; that is `ctrl_c_quits_from_every_mode`.
    #[test]
    fn the_c_key_is_inert_in_normal_mode() {
        for c in ['c', 'C'] {
            let mut a = app();
            load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);
            let before = a.selected;
            assert_eq!(a.on_key(press(c)), Action::None, "{c:?} did something");
            assert_eq!(a.mode, Mode::Normal, "{c:?} changed mode");
            assert!(a.prompt.is_none(), "{c:?} opened a prompt");
            assert!(a.message.is_none(), "{c:?} flashed a message");
            assert!(!a.should_quit, "{c:?} quit");
            assert_eq!(a.selected, before, "{c:?} moved the selection");
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

    // ── §8.6: tilde expansion, the mkdir offer, and cwd reachability ────────

    // Per-thread record of what the `dispatch` seam received. Tests run one
    // per thread, so a `thread_local!` needs no clearing between tests.
    thread_local! {
        static DISPATCHED: std::cell::RefCell<Vec<(String, String)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// The seam as the CLI behaved BEFORE it was parsed for an id — and as it
    /// behaves again whenever the banner changes shape. `Ok(None)` is the
    /// degraded case, and every pre-existing §8.6 test runs through it, which
    /// is what proves the degradation is exactly today's behaviour.
    fn recording_dispatch(cwd: &str, task: &str) -> Result<Option<String>, AgentsError> {
        DISPATCHED.with(|d| d.borrow_mut().push((cwd.to_string(), task.to_string())));
        Ok(None)
    }

    /// The seam as the real CLI behaves: it hands back the short id.
    fn recording_dispatch_with_id(cwd: &str, task: &str) -> Result<Option<String>, AgentsError> {
        DISPATCHED.with(|d| d.borrow_mut().push((cwd.to_string(), task.to_string())));
        Ok(Some("e44654bf".to_string()))
    }

    fn dispatched() -> Vec<(String, String)> {
        DISPATCHED.with(|d| d.borrow().clone())
    }

    /// A fresh, empty fixture directory under `~/.local/tmp` — the ONE place
    /// this suite is allowed to write. Unique per test name + pid so parallel
    /// test threads never share a path.
    fn fs_fixture(name: &str) -> std::path::PathBuf {
        let home = std::env::var("HOME").expect("HOME");
        let p = std::path::PathBuf::from(home)
            .join(".local/tmp")
            .join(format!("ccmux-ncwd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("fixture dir under ~/.local/tmp");
        p
    }

    /// `n`, then force the prompt's cwd field to `cwd` and type `task`.
    fn prompt_with(a: &mut App, cwd: &str, task: &str) {
        a.on_key(press('n'));
        let p = a.prompt.as_mut().expect("prompt");
        p.fields[0] = cwd.to_string();
        for c in task.chars() {
            a.on_key(press(c));
        }
    }

    fn enter(a: &mut App) {
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[test]
    fn tilde_expands_only_as_a_leading_path_component() {
        let home = Some("/home/dev");
        assert_eq!(expand_tilde("~", home), Ok("/home/dev".into()));
        assert_eq!(expand_tilde("~/projects/foo", home), Ok("/home/dev/projects/foo".into()));
        // A tilde that is merely CONTAINED is a file name, not syntax.
        assert_eq!(expand_tilde("/data/~backup", home), Ok("/data/~backup".into()));
        assert_eq!(expand_tilde("/a/b", home), Ok("/a/b".into()));
        // `~user` is refused, not misexpanded.
        assert!(expand_tilde("~root/x", home).is_err());
        // No home to expand against: a clear refusal, not a bogus path.
        assert!(expand_tilde("~", None).is_err());
        assert!(expand_tilde("~/x", Some("")).is_err());
    }

    #[test]
    fn a_tilde_cwd_is_expanded_before_dispatch() {
        let tmp = fs_fixture("tilde-dispatch");
        let sub = tmp.join("exists");
        std::fs::create_dir(&sub).expect("fixture subdir");

        let mut a = app();
        a.dispatch = recording_dispatch;
        // Point `~` at the fixture so the typed path exercises expansion while
        // every real directory involved stays under ~/.local/tmp.
        a.home = Some(tmp.to_string_lossy().into_owned());
        load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);
        prompt_with(&mut a, "~/exists", "do the thing");
        enter(&mut a);

        assert_eq!(a.mode, Mode::Normal, "dispatch must close the prompt");
        assert_eq!(dispatched(), vec![(sub.to_string_lossy().into_owned(), "do the thing".into())]);
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!((text.as_str(), level), ("dispatched background session", MsgLevel::Info));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The common case must stay: `n`, type the task, ONE Enter — the
    /// prefilled cwd (the selected row's) is honoured with no extra keystroke.
    #[test]
    fn the_common_case_is_still_one_enter_with_the_prefilled_cwd() {
        let tmp = fs_fixture("common-case");
        let mut a = app();
        a.dispatch = recording_dispatch;
        let mut sess = bg("aaaaaaaa", "one", State::Working);
        sess.cwd = tmp.to_string_lossy().into_owned();
        load(&mut a, vec![sess]);

        a.on_key(press('n'));
        for c in "run it".chars() {
            a.on_key(press(c));
        }
        enter(&mut a);

        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(dispatched(), vec![(tmp.to_string_lossy().into_owned(), "run it".into())]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_missing_dir_arms_the_offer_and_the_second_enter_creates_one_level() {
        let tmp = fs_fixture("arm-create");
        let target = tmp.join("newdir");
        let mut a = app();
        a.dispatch = recording_dispatch;
        a.home = None; // flashes carry the full path
        load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);
        prompt_with(&mut a, &target.to_string_lossy(), "build");

        // First Enter: arms, creates NOTHING, stays in the prompt.
        enter(&mut a);
        assert_eq!(a.mode, Mode::Prompt(PromptKind::NewBackground));
        assert!(!target.exists(), "first Enter must not mkdir");
        assert!(dispatched().is_empty(), "first Enter must not dispatch");
        let armed = a.prompt.as_ref().and_then(|p| p.pending_create.clone());
        assert_eq!(armed.as_deref(), Some(&*target.to_string_lossy()));
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Warn);
        assert!(text.contains("does not exist"), "{text}");

        // Second Enter: creates exactly this one directory and dispatches.
        enter(&mut a);
        assert!(target.is_dir(), "second Enter must create the directory");
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(dispatched(), vec![(target.to_string_lossy().into_owned(), "build".into())]);
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Info);
        assert!(
            text.contains(&*target.to_string_lossy()),
            "the flash must name the created path: {text}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_missing_parent_is_refused_with_no_mkdir() {
        let tmp = fs_fixture("deep-path");
        let deep = tmp.join("nope").join("deep");
        let mut a = app();
        a.home = None;
        load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);
        prompt_with(&mut a, &deep.to_string_lossy(), "t");

        enter(&mut a);
        assert_eq!(a.mode, Mode::Prompt(PromptKind::NewBackground));
        assert!(!tmp.join("nope").exists(), "no level of a typo'd tree may materialise");
        assert!(a.prompt.as_ref().is_some_and(|p| p.pending_create.is_none()), "must not arm");
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Warn);
        assert!(text.contains("parent does not exist"), "{text}");
        // Enter again: still refused — a repeat cannot escalate into a tree.
        enter(&mut a);
        assert!(!tmp.join("nope").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_file_or_dangling_symlink_at_the_target_is_refused() {
        let tmp = fs_fixture("not-a-dir");
        let file = tmp.join("plain");
        std::fs::write(&file, b"x").expect("fixture file");
        let dangling = tmp.join("dangling");
        std::os::unix::fs::symlink(tmp.join("void"), &dangling).expect("fixture symlink");

        for target in [&file, &dangling] {
            let mut a = app();
            a.home = None;
            load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);
            prompt_with(&mut a, &target.to_string_lossy(), "t");
            enter(&mut a);
            assert_eq!(a.mode, Mode::Prompt(PromptKind::NewBackground));
            assert!(a.prompt.as_ref().is_some_and(|p| p.pending_create.is_none()));
            let (text, level) = a.message.clone().unwrap_or_default_msg();
            assert_eq!(level, MsgLevel::Warn);
            assert!(text.contains("not a directory"), "{text}");
            // The entry is untouched: still a file / still a dangling link.
            assert!(std::fs::symlink_metadata(target).is_ok());
            assert!(!target.is_dir());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn tilde_user_and_empty_cwd_are_refused_cleanly() {
        let mut a = app();
        load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);
        prompt_with(&mut a, "~root/x", "t");
        enter(&mut a);
        assert_eq!(a.mode, Mode::Prompt(PromptKind::NewBackground));
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Warn);
        assert!(text.contains("~user paths are not supported"), "{text}");

        // Empty cwd — the degraded/empty-list corner: refuse, never arm.
        let p = a.prompt.as_mut().expect("prompt");
        p.fields[0].clear();
        enter(&mut a);
        assert_eq!(a.mode, Mode::Prompt(PromptKind::NewBackground));
        assert!(a.prompt.as_ref().is_some_and(|p| p.pending_create.is_none()));
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!((text.as_str(), level), ("cwd cannot be empty", MsgLevel::Warn));
    }

    #[test]
    fn the_mkdir_arm_survives_neither_esc_nor_edits() {
        let tmp = fs_fixture("arm-drops");
        let target = tmp.join("newdir");
        let mut a = app();
        a.home = None;
        load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);

        // Armed, then Esc: the prompt and the arm die together.
        prompt_with(&mut a, &target.to_string_lossy(), "t");
        enter(&mut a);
        assert!(a.prompt.as_ref().is_some_and(|p| p.pending_create.is_some()));
        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(a.prompt.is_none());
        a.on_key(press('n'));
        assert!(a.prompt.as_ref().is_some_and(|p| p.pending_create.is_none()));
        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        // Armed, then an edit (task field is focused): disarmed on the spot,
        // and the following Enter re-arms instead of creating.
        prompt_with(&mut a, &target.to_string_lossy(), "t");
        enter(&mut a);
        assert!(a.prompt.as_ref().is_some_and(|p| p.pending_create.is_some()));
        a.on_key(press('x'));
        assert!(a.prompt.as_ref().is_some_and(|p| p.pending_create.is_none()));
        enter(&mut a);
        assert!(!target.exists(), "an Enter after an edit must arm again, not create");
        assert!(a.prompt.as_ref().is_some_and(|p| p.pending_create.is_some()));

        // A paste is an edit too.
        a.on_paste("y");
        assert!(a.prompt.as_ref().is_some_and(|p| p.pending_create.is_none()));

        // Backspace as well.
        enter(&mut a);
        assert!(a.prompt.as_ref().is_some_and(|p| p.pending_create.is_some()));
        a.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(a.prompt.as_ref().is_some_and(|p| p.pending_create.is_none()));
        assert!(!target.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn tab_completes_directories_in_the_cwd_field_and_still_cycles_from_task() {
        let tmp = fs_fixture("completion");
        for d in ["alpha", "proj-one", "proj-two", ".hidden"] {
            std::fs::create_dir(tmp.join(d)).expect("fixture subdir");
        }
        std::fs::write(tmp.join("alfile"), b"x").expect("fixture file");
        let root = tmp.to_string_lossy().into_owned();
        let home = None;

        // Unique prefix: completed through to `.../alpha/` (the file `alfile`
        // is not a directory and must not compete).
        assert_eq!(
            complete_dir(&format!("{root}/al"), home),
            Completion::Extend(format!("{root}/alpha/"))
        );
        // Several matches: extended to their longest common prefix…
        assert_eq!(
            complete_dir(&format!("{root}/p"), home),
            Completion::Extend(format!("{root}/proj-"))
        );
        // …and once at that prefix, stuck with a count.
        assert_eq!(complete_dir(&format!("{root}/proj-"), home), Completion::Stuck(2));
        // Dot-directories require a typed dot.
        assert_eq!(
            complete_dir(&format!("{root}/.h"), home),
            Completion::Extend(format!("{root}/.hidden/"))
        );
        // Unreadable directory, no `/`, `~user`: all degrade to a no-op.
        assert_eq!(complete_dir("/ccmux-definitely-missing/x", home), Completion::Stuck(0));
        assert_eq!(complete_dir("relative", home), Completion::Stuck(0));
        assert_eq!(complete_dir("~root/x", home), Completion::Stuck(0));

        // Through the keymap: `Tab` from the task field still reaches the cwd
        // field; a second `Tab` completes IN PLACE instead of cycling away.
        let mut a = app();
        a.home = None;
        load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);
        a.on_key(press('n'));
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        {
            let p = a.prompt.as_mut().expect("prompt");
            assert_eq!(p.focus, 0, "Tab from the task field lands on cwd");
            p.fields[0] = format!("{root}/alp");
            p.cursor = p.fields[0].chars().count();
        }
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let p = a.prompt.clone().expect("prompt");
        assert_eq!(p.focus, 0, "completion must not move focus");
        assert_eq!(p.fields[0], format!("{root}/alpha/"));
        assert_eq!(p.cursor, p.fields[0].chars().count());

        // BackTab still returns to the task field.
        a.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert!(a.prompt.as_ref().is_some_and(|p| p.focus == 1));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The arm flash must survive the overflow carve — at most 4 rows of the
    /// default 34-column sidebar — with its actionable tail intact, so a long
    /// path is elided to `CWD_FLASH_MAX` instead of shoving `⏎ again to create
    /// it` off the bottom. The elision is display-only: dispatch still gets
    /// the full expanded path.
    #[test]
    fn the_arm_flash_elides_a_long_path_and_keeps_its_actionable_tail() {
        let tmp = fs_fixture("long-arm");
        let parent = tmp.join("a".repeat(40)).join("b".repeat(38));
        std::fs::create_dir_all(&parent).expect("fixture parents");
        let target = parent.join("newdir");
        let mut a = app();
        a.dispatch = recording_dispatch;
        a.home = None;
        load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);
        prompt_with(&mut a, &target.to_string_lossy(), "t");

        enter(&mut a);
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Warn);
        assert!(text.ends_with("⏎ again to create it"), "tail must survive: {text}");
        assert!(text.contains('…'), "a long path must be elided: {text}");
        assert!(text.contains("newdir"), "the identifying tail component must survive: {text}");
        let fixed = " does not exist — ⏎ again to create it";
        assert!(
            model::display_width(&text) <= CWD_FLASH_MAX + model::display_width(fixed),
            "flash wider than the elision budget allows: {text}"
        );

        // The elision changed nothing about WHAT is created or dispatched.
        enter(&mut a);
        assert!(target.is_dir(), "second Enter still creates the full path");
        assert_eq!(dispatched(), vec![(target.to_string_lossy().into_owned(), "t".into())]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `Delete` at end-of-field removes nothing, so — mirroring Backspace's
    /// `cursor > 0` guard — it must not disarm the mkdir offer either.
    #[test]
    fn a_noop_delete_at_end_of_field_keeps_the_mkdir_arm() {
        let tmp = fs_fixture("del-eof");
        let target = tmp.join("newdir");
        let mut a = app();
        a.home = None;
        load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);
        prompt_with(&mut a, &target.to_string_lossy(), "t");
        enter(&mut a);
        assert!(a.prompt.as_ref().is_some_and(|p| p.pending_create.is_some()));

        // Cursor sits at end-of-field after typing: Delete is a no-op.
        a.on_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        let p = a.prompt.as_ref().expect("prompt");
        assert_eq!(p.fields[1], "t", "no char removed");
        assert!(p.pending_create.is_some(), "a no-op Delete must keep the arm");

        // Home + Delete removes a char: that IS an edit, and disarms.
        a.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        a.on_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        let p = a.prompt.as_ref().expect("prompt");
        assert_eq!(p.fields[1], "", "the char under the cursor is removed");
        assert!(p.pending_create.is_none(), "a real Delete edit must disarm");
        assert!(!target.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `Tab` completion sees the field exactly as `Enter` will — trimmed — so
    /// a value Enter would accept cannot make Tab a silent no-op. On `Extend`
    /// the stray whitespace dies with the replacement; on `Stuck` the field is
    /// not touched at all.
    #[test]
    fn tab_completion_trims_the_field_exactly_as_enter_does() {
        let tmp = fs_fixture("trim-complete");
        std::fs::create_dir(tmp.join("proj-one")).expect("fixture subdir");
        let root = tmp.to_string_lossy().into_owned();
        let mut a = app();
        a.home = None;
        load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);
        a.on_key(press('n'));
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)); // to cwd
        {
            let p = a.prompt.as_mut().expect("prompt");
            p.fields[0] = format!("  {root}/proj-o ");
            p.cursor = p.fields[0].chars().count();
        }
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let p = a.prompt.as_ref().expect("prompt");
        assert_eq!(p.fields[0], format!("{root}/proj-one/"));
        assert_eq!(p.cursor, p.fields[0].chars().count());

        // A stuck completion writes nothing back — whitespace and all.
        let stuck = "  /ccmux-definitely-missing/x".to_string();
        {
            let p = a.prompt.as_mut().expect("prompt");
            p.fields[0] = stuck.clone();
            p.cursor = stuck.chars().count();
        }
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.prompt.as_ref().expect("prompt").fields[0], stuck);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A failed `create_dir` must say WHAT it could not create — the operator
    /// should not have to re-read the field to know which path failed and that
    /// nothing was made.
    #[test]
    fn a_failed_create_names_the_path_and_creates_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = fs_fixture("ro-parent");
        let ro = tmp.join("roparent");
        std::fs::create_dir(&ro).expect("fixture subdir");
        let mut perms = std::fs::metadata(&ro).expect("meta").permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&ro, perms).expect("chmod 555");
        let target = ro.join("kid");

        let mut a = app();
        a.dispatch = recording_dispatch;
        a.home = None;
        load(&mut a, vec![bg("aaaaaaaa", "one", State::Working)]);
        prompt_with(&mut a, &target.to_string_lossy(), "t");
        enter(&mut a); // arms
        enter(&mut a); // create fails: EACCES

        assert_eq!(a.mode, Mode::Prompt(PromptKind::NewBackground), "prompt must survive");
        assert!(!target.exists(), "nothing may be created");
        assert!(dispatched().is_empty(), "nothing may be dispatched");
        assert!(
            a.prompt.as_ref().is_some_and(|p| p.pending_create.is_none()),
            "a failed create must disarm"
        );
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(level, MsgLevel::Error);
        assert!(text.contains("could not create"), "{text}");
        // The path is named under the same elision rule as every §8.6 flash:
        // the identifying FINAL components must be there verbatim.
        assert!(
            text.contains("roparent/kid"),
            "the flash must name the path that failed: {text}"
        );

        let mut perms = std::fs::metadata(&ro).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&ro, perms).expect("chmod 755");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn backoff_widens_the_interval_after_three_failures() {
        let mut a = app();
        assert_eq!(a.agents_interval(), Duration::from_millis(2500));
        a.fail_streak = 3;
        assert_eq!(a.agents_interval(), Duration::from_secs(10));
        a.fail_streak = 0;
        assert_eq!(a.agents_interval(), Duration::from_millis(2500));
    }

    /// REGRESSION. The failure backoff belongs to the `claude agents` spawn
    /// and to nothing else. It used to be the TICK interval, which is also the
    /// poll gate's edge detector — and since only a successful poll clears
    /// `fail_streak`, and a shut gate takes no poll, a sidebar that quiesced
    /// while the CLI was failing kept a 10 s tick for as long as it stayed off
    /// screen. Coming back then took up to a backoff to be noticed instead of
    /// up to an interval, and the §1.3 width pin and the pane reconcile were
    /// slowed with it.
    #[test]
    fn the_failure_backoff_never_slows_the_tick() {
        let mut a = app();
        a.fail_streak = 9;
        assert_eq!(a.tick_interval(), Duration::from_millis(2500));
        assert_eq!(a.agents_interval(), Duration::from_secs(10));

        // And it cannot slow the wake-up either. Quiesce with the streak
        // latched — no poll can clear it from here — then come back.
        watched_by(&mut a, 1, false);
        a.observe_watchers();
        assert!(a.quiesced);
        assert_eq!(a.tick_interval(), Duration::from_millis(2500), "the tick took the backoff");

        watched_by(&mut a, 1, true);
        a.last_agents = Instant::now();
        assert!(
            gated_tick(&mut a, || complete(four())),
            "the backoff outranked the visibility edge"
        );
        assert_eq!(a.fail_streak, 0, "the forced poll did not clear the streak");
    }

    // ── §8.6: `n` moves the cursor onto the session it dispatched ───────────

    /// The three rows the fixture starts with, cursor on the first.
    fn three() -> Vec<Session> {
        vec![
            bg("aaaaaaaa", "one", State::Working),
            bg("bbbbbbbb", "two", State::Working),
            bg("cccccccc", "three", State::Working),
        ]
    }

    /// Those three plus the session `n` just dispatched, in `group`. It is the
    /// NEWEST, so within its group it sorts to the top — which is exactly why
    /// its row index is not something the dispatch could have predicted.
    fn three_plus_new(state: State) -> Vec<Session> {
        let mut fresh = bg("e44654bf", "fresh task", state);
        fresh.started_at = 1_787_700_000_000;
        let mut all = three();
        all.push(fresh);
        all
    }

    /// An app with `three()` on screen, the cursor on the first row, and `n`'s
    /// cursor intent armed for a session the daemon has not listed yet.
    ///
    /// Armed through the REAL keypath — `n`, the task text, `Enter` — against a
    /// seam that answers the way the real CLI does.
    fn armed(fixture: &str) -> (App, std::path::PathBuf) {
        let tmp = fs_fixture(fixture);
        let mut a = app();
        load(&mut a, three());
        a.dispatch = recording_dispatch_with_id;
        prompt_with(&mut a, &tmp.to_string_lossy(), "do it");
        enter(&mut a);
        assert_eq!(a.mode, Mode::Normal, "the dispatch closed the prompt");
        assert_eq!(
            a.pending_jump.as_ref().map(|j| j.short_id.as_str()),
            Some("e44654bf"),
            "the short id off `--bg` armed the intent"
        );
        assert_eq!(
            a.selected_key.as_deref(),
            Some("aaaaaaaa-uuid"),
            "`n` itself still never moves the cursor"
        );
        (a, tmp)
    }

    /// One tick's worth of the only three steps the cursor intent can see: the
    /// poll (4), the rebuild (5) and the settle (5b). Deliberately NOT `tick()`
    /// — that also shells out to tmux, and this suite may not touch the
    /// operator's live socket. The poll goes through `poll_step`, so `polled`
    /// is the real gate's answer and not a literal.
    fn poll_and_settle(a: &mut App, sessions: Vec<Session>) {
        age(a, Duration::from_secs(60));
        let polled = a.poll_step(|| complete(sessions));
        assert!(polled, "the fixture's gate must let this poll through");
        a.rebuild_rows();
        a.tick_pending_jump(polled);
    }

    /// A tick whose poll gate was SHUT — the quiesced sidebar. Rows still
    /// rebuild; nothing was learned, so nothing may be charged to the budget.
    fn settle_without_polling(a: &mut App) {
        a.rebuild_rows();
        a.tick_pending_jump(false);
    }

    /// THE CASE THE WHOLE DESIGN EXISTS FOR: the row does not exist on the poll
    /// the dispatch forces, so a one-shot "select it now" would miss it.
    #[test]
    fn the_cursor_follows_the_new_session_when_it_arrives_on_a_later_poll() {
        let (mut a, tmp) = armed("jump-later");

        // Poll 1 — the daemon has not listed it. The cursor must not wander,
        // and the intent must survive.
        poll_and_settle(&mut a, three());
        assert_eq!(a.selected_key.as_deref(), Some("aaaaaaaa-uuid"), "nothing moved");
        assert!(a.pending_jump.is_some(), "a poll that missed it is not a refusal");

        // Poll 2 — it appears, in the group that sorts FIRST.
        poll_and_settle(&mut a, three_plus_new(State::Blocked));
        assert_eq!(
            a.selected_key.as_deref(),
            Some("e44654bf-uuid"),
            "the cursor is keyed by the UUID, and `--bg` only gave a short id"
        );
        assert_eq!(
            a.selected_session().and_then(|s| s.id.clone()).as_deref(),
            Some("e44654bf"),
            "and it is the row for the session that was just dispatched"
        );
        assert!(
            matches!(a.rows.get(a.selected), Some(Row::Session { .. })),
            "selection landed on {:?}",
            a.rows.get(a.selected)
        );
        assert_eq!(a.pending_jump, None, "spent once it fires");

        // Still only the CURSOR: `n` opens nothing.
        assert!(a.map.pane_for_session("e44654bf-uuid").is_none(), "no pane was opened");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A `--bg` banner this build cannot parse degrades to exactly the
    /// behaviour that shipped before the jump existed — and the dispatch itself
    /// still succeeds, which is the half that matters.
    #[test]
    fn a_dispatch_with_no_parseable_id_still_dispatches_and_leaves_the_cursor() {
        let tmp = fs_fixture("jump-degraded");
        let mut a = app();
        load(&mut a, three());
        // `Ok(None)` — the seam as the CLI behaved before, and as it behaves
        // again the day the banner changes shape.
        a.dispatch = recording_dispatch;
        prompt_with(&mut a, &tmp.to_string_lossy(), "do it");
        enter(&mut a);

        assert_eq!(
            dispatched(),
            vec![(tmp.to_string_lossy().into_owned(), "do it".into())],
            "the session was dispatched"
        );
        assert_eq!(a.mode, Mode::Normal);
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!((text.as_str(), level), ("dispatched background session", MsgLevel::Info));
        assert_eq!(a.pending_jump, None, "no id, no intent");

        poll_and_settle(&mut a, three_plus_new(State::Working));
        assert_eq!(a.selected_key.as_deref(), Some("aaaaaaaa-uuid"), "today's behaviour, exactly");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The intent is bounded. It must not sit there for the life of the process
    /// waiting to re-arm on some unrelated future row.
    #[test]
    fn the_pending_jump_expires_after_a_bounded_number_of_polls() {
        let (mut a, tmp) = armed("jump-expiry");
        for i in 0..JUMP_POLLS {
            assert!(a.pending_jump.is_some(), "poll {i} is still inside the budget");
            poll_and_settle(&mut a, three());
        }
        assert_eq!(a.pending_jump, None, "the intent does not outlive its budget");

        // A session that finally lists long afterwards is NOT chased.
        poll_and_settle(&mut a, three_plus_new(State::Working));
        assert_eq!(a.selected_key.as_deref(), Some("aaaaaaaa-uuid"));

        // And the expiry was SILENT: the last thing said is still the flash the
        // dispatch itself put up, which was true — the agent is running.
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "dispatched background session");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The budget is counted in POLLS, not ticks and not seconds. A quiesced
    /// sidebar ticks on without spawning `claude agents`; those ticks learn
    /// nothing about the id and must refute nothing.
    #[test]
    fn a_tick_that_did_not_poll_spends_none_of_the_jumps_budget() {
        let (mut a, tmp) = armed("jump-quiesced");
        let before = a.pending_jump.clone();
        for _ in 0..(JUMP_POLLS * 4) {
            settle_without_polling(&mut a);
        }
        assert_eq!(a.pending_jump, before, "a shut gate refutes nothing");

        // And the intent is still good when the polls resume.
        poll_and_settle(&mut a, three_plus_new(State::Working));
        assert_eq!(a.selected_key.as_deref(), Some("e44654bf-uuid"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A jump that yanks the selection out from under a keypress is worse than
    /// no jump. Every deliberate cursor move cancels it.
    ///
    /// The `j` then `k` case is the one a `selected_key` snapshot compared at
    /// fire time would have got wrong: the cursor ends up back on the key it
    /// started from, and the operator has still had their say.
    #[test]
    fn a_jump_never_fires_after_the_operator_moved_the_cursor() {
        /// One named cursor move, as the operator would make it.
        type Move = (&'static str, fn(&mut App));
        let moves: [Move; 6] = [
            ("j", |a| {
                a.on_key(press('j'));
            }),
            ("j then k", |a| {
                a.on_key(press('j'));
                a.on_key(press('k'));
            }),
            ("G", |a| {
                a.on_key(press('G'));
            }),
            ("Tab", |a| {
                a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            }),
            ("d", |a| {
                a.on_key(press('d'));
            }),
            ("d then u", |a| {
                a.on_key(press('d'));
                a.on_key(press('u'));
            }),
        ];
        for (label, mv) in moves {
            let (mut a, tmp) = armed("jump-operator");
            mv(&mut a);
            assert_eq!(a.pending_jump, None, "{label} must cancel the intent on the spot");
            poll_and_settle(&mut a, three_plus_new(State::Blocked));
            assert_ne!(
                a.selected_key.as_deref(),
                Some("e44654bf-uuid"),
                "{label}: the cursor was the operator's, and stayed theirs"
            );
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }

    /// `/` is the operator's, not ccmux's. A new session the filter excludes is
    /// SAID, never chased by clearing the filter — the precedent `u` set with
    /// `restored X — filtered out`.
    #[test]
    fn a_filtered_out_new_session_flashes_and_leaves_the_filter_standing() {
        let (mut a, tmp) = armed("jump-filtered");
        a.filter = "one".into();
        a.rebuild_rows();
        let before = a.selected_key.clone();

        poll_and_settle(&mut a, three_plus_new(State::Working));

        assert_eq!(a.filter, "one", "the filter is not ccmux's to clear");
        assert_eq!(a.selected_key, before, "there is no row to move to");
        assert!(
            matches!(a.rows.get(a.selected), Some(Row::Session { .. })),
            "and the cursor is still on a session row"
        );
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!((text.as_str(), level), ("dispatched fresh task — filtered out", MsgLevel::Warn));
        assert_eq!(a.pending_jump, None, "said once, then spent");

        // Clearing the filter later does NOT resurrect the jump: it was spent,
        // and moving the cursor on a later, unrelated keystroke would be the
        // stale-intent bug in a different coat.
        a.filter.clear();
        poll_and_settle(&mut a, three_plus_new(State::Working));
        assert_eq!(a.selected_key, before);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `a` hides Completed. Confirming rather than assuming that a freshly
    /// dispatched session is visible under it — and that the one shape which is
    /// NOT (a task that finished before the next poll) takes the filtered-out
    /// path rather than moving the cursor to a row that is not on screen.
    #[test]
    fn a_freshly_dispatched_session_is_visible_with_completed_hidden() {
        // Working, and Idle — the two states a just-dispatched session can be
        // in — both have rows while `a` is off.
        let mut idle = bg("e44654bf", "fresh task", State::Working);
        idle.status = Status::Idle;
        idle.state = None;
        for (label, fresh) in [
            ("working", bg("e44654bf", "fresh task", State::Working)),
            ("blocked", bg("e44654bf", "fresh task", State::Blocked)),
            ("idle", idle),
        ] {
            let (mut a, tmp) = armed("jump-hide-completed");
            a.show_completed = false;
            a.rebuild_rows();
            let mut all = three();
            all.push(fresh);
            poll_and_settle(&mut a, all);
            assert_eq!(
                a.selected_key.as_deref(),
                Some("e44654bf-uuid"),
                "{label} is not Completed, so `a` does not hide it"
            );
            let _ = std::fs::remove_dir_all(&tmp);
        }

        // The exception, stated: a session that is already Done when it lands
        // has no row while `a` is off, so it is announced, not chased.
        let (mut a, tmp) = armed("jump-hide-completed-done");
        a.show_completed = false;
        a.rebuild_rows();
        let mut all = three();
        all.push(bg("e44654bf", "fresh task", State::Done));
        poll_and_settle(&mut a, all);
        assert_eq!(a.selected_key.as_deref(), Some("aaaaaaaa-uuid"));
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!((text.as_str(), level), ("dispatched fresh task — filtered out", MsgLevel::Warn));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The selection contract: only ever a `Row::Session`, never a Header and
    /// never a Spacer — whichever group the new session lands in, including the
    /// one that sits directly under a Header and the one that sits after a
    /// Spacer.
    #[test]
    fn the_jump_only_ever_lands_on_a_session_row() {
        for state in [State::Working, State::Blocked, State::Done] {
            let (mut a, tmp) = armed("jump-rowkind");
            poll_and_settle(&mut a, three_plus_new(state.clone()));
            assert_eq!(a.selected_key.as_deref(), Some("e44654bf-uuid"), "{state:?}");
            match a.rows.get(a.selected) {
                Some(Row::Session { idx }) => {
                    assert_eq!(a.sessions[*idx].id.as_deref(), Some("e44654bf"), "{state:?}");
                }
                other => panic!("{state:?}: selection landed on {other:?}"),
            }
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }

    /// Dispatch from an EMPTY list — the corner a `selected_key` snapshot could
    /// not survive. The cursor has no key to anchor to, so the first arriving
    /// poll parks it on some other session through `reanchor_selection`'s
    /// nearest-index fallback. That is the list changing under a stationary
    /// cursor, not the operator moving it, and the jump still lands.
    #[test]
    fn a_dispatch_from_an_empty_list_still_lands_on_its_row() {
        let tmp = fs_fixture("jump-empty");
        let mut a = app();
        a.rebuild_rows();
        assert_eq!(a.selected_key, None, "nothing to select yet");
        a.dispatch = recording_dispatch_with_id;
        prompt_with(&mut a, &tmp.to_string_lossy(), "do it");
        enter(&mut a);
        assert!(a.pending_jump.is_some());

        // Two unrelated sessions land first.
        poll_and_settle(&mut a, vec![
            bg("aaaaaaaa", "one", State::Blocked),
            bg("bbbbbbbb", "two", State::Working),
        ]);
        assert_eq!(a.selected_key.as_deref(), Some("aaaaaaaa-uuid"), "the fallback parked it");
        assert!(a.pending_jump.is_some(), "which is not the operator moving the cursor");

        poll_and_settle(&mut a, vec![
            bg("aaaaaaaa", "one", State::Blocked),
            bg("bbbbbbbb", "two", State::Working),
            bg("e44654bf", "fresh task", State::Working),
        ]);
        assert_eq!(a.selected_key.as_deref(), Some("e44654bf-uuid"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// THE HOLE THE `set_selected` CANCEL LEFT. A cursor key that finds no row
    /// to move to never reaches `set_selected`, and the two commonest presses
    /// there are — `k` on the top row and `j` on the bottom — are exactly that.
    /// The operator took the cursor either way, so the jump must not fire.
    #[test]
    fn a_cursor_key_that_moves_nothing_still_cancels_the_jump() {
        type Move = (&'static str, fn(&mut App));

        // The cursor is on the FIRST row: `k` and `Ctrl-u` have nowhere to go.
        let at_top: [Move; 2] = [
            ("k", |a| {
                a.on_key(press('k'));
            }),
            ("C-u", |a| {
                a.on_key(ctrl('u'));
            }),
        ];
        for (label, mv) in at_top {
            let (mut a, tmp) = armed("jump-top-boundary");
            let before = a.selected_key.clone();
            mv(&mut a);
            assert_eq!(a.selected_key, before, "{label} really did move nothing");
            assert_eq!(a.pending_jump, None, "{label} must cancel the intent anyway");
            poll_and_settle(&mut a, three_plus_new(State::Blocked));
            assert_eq!(a.selected_key, before, "{label}: the cursor was the operator's");
            let _ = std::fs::remove_dir_all(&tmp);
        }

        // The cursor is on the LAST row: `j` and `Ctrl-d` have nowhere to go.
        // `select_last` is called DIRECTLY rather than through `G`, so the
        // intent is still armed when the boundary key arrives — which is also
        // the proof that the cancel hangs off the KEYPRESS now and not off
        // `set_selected`, which `select_last` goes straight through.
        let at_bottom: [Move; 2] = [
            ("j", |a| {
                a.on_key(press('j'));
            }),
            ("C-d", |a| {
                a.on_key(ctrl('d'));
            }),
        ];
        for (label, mv) in at_bottom {
            let (mut a, tmp) = armed("jump-bottom-boundary");
            a.select_last();
            assert!(a.pending_jump.is_some(), "{label}: `set_selected` is not the cancel");
            let before = a.selected_key.clone();
            assert_eq!(before.as_deref(), Some("cccccccc-uuid"), "{label}: on the last row");
            mv(&mut a);
            assert_eq!(a.selected_key, before, "{label} really did move nothing");
            assert_eq!(a.pending_jump, None, "{label} must cancel the intent anyway");
            poll_and_settle(&mut a, three_plus_new(State::Blocked));
            assert_eq!(a.selected_key, before, "{label}: the cursor was the operator's");
            let _ = std::fs::remove_dir_all(&tmp);
        }

        // `Tab` with no rows at all: `cycle_group` returns before it can select
        // anything, because `rows` carries no headers to walk between.
        let tmp = fs_fixture("jump-tab-empty");
        let mut a = app();
        a.rebuild_rows();
        a.dispatch = recording_dispatch_with_id;
        prompt_with(&mut a, &tmp.to_string_lossy(), "do it");
        enter(&mut a);
        assert!(a.pending_jump.is_some(), "armed on an empty list");
        assert!(a.rows.is_empty(), "and there is nothing to Tab between");
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.pending_jump, None, "Tab must cancel the intent anyway");
        // The arriving list is shaped so the two outcomes are distinguishable:
        // `Blocked` sorts first, so `reanchor_selection`'s nearest-index
        // fallback parks the cursor on `one` and a jump that fired would be the
        // only thing that could put it on `fresh task`.
        poll_and_settle(&mut a, vec![
            bg("aaaaaaaa", "one", State::Blocked),
            bg("e44654bf", "fresh task", State::Working),
        ]);
        assert_eq!(a.selected_key.as_deref(), Some("aaaaaaaa-uuid"), "the fallback parked it");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The cancel table, stated once. It is WIDER than the moves on purpose: a
    /// verb that acts on the row under the cursor has committed to that row.
    #[test]
    fn the_keys_that_claim_the_cursor_are_the_moves_and_the_row_verbs() {
        for c in ['j', 'k', 'g', 'G', 'o', 's', 't', 'x', 'L', 'd', 'u'] {
            assert!(claims_the_cursor(press(c)), "{c} claims the cursor");
        }
        for code in [KeyCode::Down, KeyCode::Up, KeyCode::Tab, KeyCode::BackTab, KeyCode::Enter] {
            let k = KeyEvent::new(code, KeyModifiers::NONE);
            assert!(claims_the_cursor(k), "{code:?} claims the cursor");
        }
        for c in ['d', 'u', 'x', 'X'] {
            assert!(claims_the_cursor(ctrl(c)), "C-{c} claims the cursor");
        }
        // Uppercase bindings arrive with SHIFT set, exactly as `key_normal`
        // documents; the table must not require empty modifiers either.
        for c in ['G', 'L'] {
            let k = KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT);
            assert!(claims_the_cursor(k), "shifted {c} claims the cursor");
        }

        // `n` arms the next intent itself, `r` is the refresh that LANDS the
        // one already armed, `/` and `a` are view toggles that §8.6 answers
        // with the filtered-out flash, and the rest touch no selection at all.
        for c in ['n', 'r', '/', 'a', '?', 'q', 'R'] {
            assert!(!claims_the_cursor(press(c)), "{c} does not claim the cursor");
        }
        let shifted_r = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        assert!(!claims_the_cursor(shifted_r), "shifted R does not claim the cursor");
        for c in ['j', 'k', 'g', 'n', 'z'] {
            assert!(!claims_the_cursor(ctrl(c)), "C-{c} is unbound and inert");
        }
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!claims_the_cursor(esc), "Esc does not claim the cursor");
    }

    /// `Ctrl+X`'s two presses straddle a forced poll — and the poll the FIRST
    /// press forces is the one most likely to list the new session. A jump
    /// firing in between retargeted the cursor, `arm_second_press` refused with
    /// `moved off <name> — nothing deleted`, and the operator, who had moved
    /// nothing, lost their delete and was told they had done it.
    #[test]
    fn a_pending_jump_cannot_retarget_the_ctrl_x_delete() {
        let (mut a, tmp) = armed("jump-ctrl-x");
        agents::test_spawn::reset();

        // First press: stop the row under the cursor and open the window.
        a.on_key(ctrl('x'));
        assert_eq!(agents::test_spawn::joined(), vec!["stop aaaaaaaa"]);
        assert!(a.stop_arm.is_some(), "the delete window is open");
        assert_eq!(a.pending_jump, None, "and the press took the cursor with it");

        // The poll that press forced now lists the new session — the exact
        // moment the jump used to fire, inside the operator's own window.
        poll_and_settle(&mut a, three_plus_new(State::Blocked));
        assert_eq!(
            a.selected_session().map(|s| s.name.clone()).as_deref(),
            Some("one"),
            "the cursor is still on the row the first press stopped"
        );

        // Second press, well inside the window: it deletes what was stopped.
        after(&mut a, BEAT);
        a.on_key(ctrl('x'));
        assert!(settle(&mut a), "the settled delete must ask for a redraw");
        assert_eq!(agents::test_spawn::joined(), vec!["stop aaaaaaaa", "rm aaaaaaaa"]);
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "deleted one + worktree");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A poll that FAILED is not evidence. `poll_step` reports that
    /// `claude agents` RAN, and its `Err` path leaves `sessions` exactly as it
    /// found them — so charging the budget for it let a transiently failing CLI
    /// spend the whole jump before the sidebar had once seen a session list.
    #[test]
    fn a_poll_that_failed_spends_none_of_the_jumps_budget() {
        let (mut a, tmp) = armed("jump-failing-cli");
        for i in 0..(JUMP_POLLS * 2) {
            age(&mut a, Duration::from_secs(60));
            let polled = a.poll_step(|| {
                Err(AgentsError::Cmd {
                    code: 1,
                    stderr: "daemon is not running".into(),
                })
            });
            assert!(polled, "poll {i} ran");
            a.rebuild_rows();
            a.tick_pending_jump(polled);
            assert!(a.poll_error.is_some(), "poll {i} failed");
        }
        assert_eq!(
            a.pending_jump.as_ref().map(|j| j.polls_left),
            Some(JUMP_POLLS),
            "sixteen polls that never answered refuted nothing"
        );

        // And the jump still lands the moment the CLI answers.
        poll_and_settle(&mut a, three_plus_new(State::Working));
        assert_eq!(a.selected_key.as_deref(), Some("e44654bf-uuid"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The `— filtered out` flash is the design's alternative to dropping the
    /// jump silently, and it is the only thing the operator is owed. It must
    /// not be spent into a footer that cannot show it: `draw_footer` hands the
    /// line to the filter input while `/` is open and never consults
    /// `footer_message` at all, and `Mode::Help`, `Mode::Logs` and a prompt
    /// cover the sidebar rect outright. Hold the intent instead, exactly as THE
    /// DRIFT GUARD holds a warning it could not deliver.
    ///
    /// The wait is FREE, and it has to be: the budget's question — does this
    /// session exist — is already answered by the time we get here, and charging
    /// the wait made the hold worthless in the case it exists for. At the
    /// 2500ms default the whole budget is ~20s of thinking with `/` open, after
    /// which the operator got neither the move nor the reason.
    #[test]
    fn a_filtered_out_jump_waits_for_a_footer_that_can_carry_the_reason() {
        for (label, mode) in [("filter", Mode::Filter), ("help", Mode::Help), ("logs", Mode::Logs)] {
            let (mut a, tmp) = armed("jump-covered-footer");
            a.filter = "one".into();
            a.rebuild_rows();
            a.mode = mode;
            let before = a.selected_key.clone();

            // Far more polls than the budget would ever have allowed.
            for _ in 0..(JUMP_POLLS * 3) {
                poll_and_settle(&mut a, three_plus_new(State::Working));
            }

            let (text, _) = a.message.clone().unwrap_or_default_msg();
            assert_eq!(
                text, "dispatched background session",
                "{label}: nothing was said where nothing can be read"
            );
            assert_eq!(a.selected_key, before, "{label}: there is still no row to move to");
            assert_eq!(
                a.pending_jump.as_ref().map(|j| j.polls_left),
                Some(JUMP_POLLS),
                "{label}: the wait for a free footer costs no budget"
            );
            let _ = std::fs::remove_dir_all(&tmp);
        }

        // Held is not unbounded in the way that matters: a session that LEAVES
        // the list while the footer is covered puts the intent back on the
        // budget, so nothing is held forever on a row that no longer exists.
        let (mut a, tmp) = armed("jump-covered-then-gone");
        a.filter = "one".into();
        a.rebuild_rows();
        a.mode = Mode::Filter;
        poll_and_settle(&mut a, three_plus_new(State::Working));
        assert_eq!(a.pending_jump.as_ref().map(|j| j.polls_left), Some(JUMP_POLLS));
        for _ in 0..JUMP_POLLS {
            poll_and_settle(&mut a, three());
        }
        assert_eq!(a.pending_jump, None, "the budget resumed the moment it stopped being listed");
        let _ = std::fs::remove_dir_all(&tmp);

        // Commit the filter with `Enter` and the footer is the list's again:
        // the very next tick says it, without needing another poll.
        let (mut a, tmp) = armed("jump-covered-then-committed");
        a.filter = "one".into();
        a.rebuild_rows();
        a.mode = Mode::Filter;
        poll_and_settle(&mut a, three_plus_new(State::Working));
        enter(&mut a);
        assert_eq!(a.mode, Mode::Normal, "Enter commits the filter");
        settle_without_polling(&mut a);
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!((text.as_str(), level), ("dispatched fresh task — filtered out", MsgLevel::Warn));
        assert_eq!(a.filter, "one", "the filter is still not ccmux's to clear");
        assert_eq!(a.pending_jump, None, "said once, then spent");
        let _ = std::fs::remove_dir_all(&tmp);

        // The other way out: a filter edited into one the new row DOES match
        // lets the held intent simply land, with nothing to explain.
        let (mut a, tmp) = armed("jump-covered-then-matched");
        a.filter = "one".into();
        a.rebuild_rows();
        a.mode = Mode::Filter;
        poll_and_settle(&mut a, three_plus_new(State::Working));
        assert!(a.pending_jump.is_some(), "held while `/` owned the footer");
        a.filter = "fresh".into();
        a.rebuild_rows();
        settle_without_polling(&mut a);
        assert_eq!(a.selected_key.as_deref(), Some("e44654bf-uuid"));
        let (text, _) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "dispatched background session", "the jump landed; nothing to explain");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── the poll gate: nobody watching, nothing spawned (SPEC §4.2) ─────────

    /// Put the app in a tab whose visibility the test controls, exactly the
    /// way `refresh_panes` would: an own pane that appears in a freshly
    /// enumerated inventory, carrying the three fields `PANE_FMT` now reads
    /// off it. The viewer count is the one tmux would report for this pair —
    /// a client on my session, rendering my window.
    fn watched_by(a: &mut App, clients: u32, window_active: bool) {
        let viewers = u32::from(clients > 0 && window_active);
        watched_by_row(a, clients, window_active, Some(viewers));
    }

    /// The same, with the three fields set independently — for the states the
    /// pair alone cannot express: a grouped session (`sattach=0 wact=1 wac=1`)
    /// and a tmux too old to answer `#{window_active_clients}` (`None`).
    fn watched_by_row(a: &mut App, clients: u32, window_active: bool, viewers: Option<u32>) {
        own(a, "%1", "@1");
        let mut me = pane("%1", 1, 1, 0, 34, true);
        me.session_clients = clients;
        me.window_active = window_active;
        me.window_viewers = viewers;
        a.panes = vec![me];
        a.panes_fresh = true;
    }

    /// A `fetch` that fails the test if the gate ever calls it. This is the
    /// assertion — "no `claude agents` process was spawned" — expressed at the
    /// one seam where a spawn could happen.
    fn never_polls() -> Result<model::Payload, AgentsError> {
        panic!("the gate spawned `claude agents` with nobody watching");
    }

    /// One gated tick's worth of the poll path: what `tick` does at steps 3b
    /// and 4, minus the tmux reads and the row rebuild.
    fn gated_tick(
        a: &mut App,
        fetch: impl FnOnce() -> Result<model::Payload, AgentsError>,
    ) -> bool {
        a.observe_watchers();
        a.poll_step(fetch)
    }

    /// Age the gate's clock so the NEXT tick is due on the interval alone.
    /// No test sleeps: they backdate, like the `Ctrl+X` guard's tests.
    fn age(a: &mut App, by: Duration) {
        a.last_agents = a.last_agents.checked_sub(by).unwrap_or(a.last_agents);
    }

    #[test]
    fn a_detached_session_spawns_no_poll() {
        let mut a = app();
        watched_by(&mut a, 0, true);
        a.observe_watchers();
        assert_eq!(a.watchers(), Watchers::Detached);
        age(&mut a, Duration::from_secs(60));
        for _ in 0..20 {
            assert!(!gated_tick(&mut a, never_polls), "a detached session polled");
        }
        assert!(a.quiesced, "the operator gets no sign the sidebar is paused");
    }

    #[test]
    fn a_sidebar_in_a_background_tab_spawns_no_poll() {
        let mut a = app();
        watched_by(&mut a, 1, false);
        a.observe_watchers();
        assert_eq!(a.watchers(), Watchers::OtherTab);
        age(&mut a, Duration::from_secs(60));
        for _ in 0..20 {
            assert!(!gated_tick(&mut a, never_polls), "a background tab polled");
        }
        assert!(a.quiesced);
    }

    /// The other half of the contract: the gate closes only on POSITIVE
    /// evidence. No own pane, or an inventory that does not carry it, must
    /// keep polling exactly as today — that is the degraded and outside-tmux
    /// case, where the sidebar is presumably the thing on screen.
    #[test]
    fn an_unanswerable_inventory_keeps_polling() {
        let mut a = app();
        a.panes.clear();
        a.own_pane = None;
        assert_eq!(a.watchers(), Watchers::Unknown);
        age(&mut a, Duration::from_secs(60));
        assert!(gated_tick(&mut a, || complete(four())));
        assert!(!a.quiesced, "an unknown answer is not a quiesced sidebar");

        // Own pane set, but this tick's enumeration failed and left `panes`
        // without it: still unknown, still polling.
        own(&mut a, "%9", "@9");
        a.panes = vec![pane("%1", 1, 1, 0, 34, true)];
        assert_eq!(a.watchers(), Watchers::Unknown);
        age(&mut a, Duration::from_secs(60));
        assert!(gated_tick(&mut a, || complete(four())));
    }

    /// REGRESSION. A failed enumeration must not be answered out of the LAST
    /// good one. `refresh_panes` keeps `self.panes` on an error — every other
    /// reader wants the last good answer — so a sidebar that was in a
    /// background tab when `list-panes -t '=ccmux:'` began failing (the
    /// session was renamed, say) kept reading its own stale `wact=0` row as
    /// positive evidence that nobody was looking. The gate then never saw a
    /// false->true edge, and the sidebar never polled again — while being the
    /// on-screen, active pane of an attached session. Only `r` moved it, one
    /// poll at a time.
    #[test]
    fn a_failed_enumeration_cannot_latch_the_gate_shut() {
        let mut a = app();
        // In a background tab, correctly silent.
        watched_by(&mut a, 1, false);
        a.observe_watchers();
        assert_eq!(a.watchers(), Watchers::OtherTab);
        age(&mut a, Duration::from_secs(60));
        assert!(!gated_tick(&mut a, never_polls));

        // Now every enumeration fails. The rows are the same rows, and they
        // are now worth nothing as evidence about who is looking.
        a.panes_fresh = false;
        assert_eq!(
            a.watchers(),
            Watchers::Unknown,
            "a stale row was read as positive evidence that nobody is watching"
        );
        assert!(a.poll_due(), "the gate stayed shut on a stale inventory");
        assert!(gated_tick(&mut a, || complete(four())));
        assert!(!a.quiesced, "an unanswerable inventory is not a quiesced sidebar");

        // And it keeps polling, tick after tick, for as long as tmux cannot
        // answer — not once, the way `r` did.
        for _ in 0..5 {
            age(&mut a, Duration::from_secs(60));
            assert!(gated_tick(&mut a, || complete(four())), "the gate re-latched");
        }
    }

    /// REGRESSION. `#{session_attached}` counts clients whose session is MINE.
    /// A grouped session (`tmux new-session -t ccmux`) shares the window list,
    /// so an operator watching the sidebar through the group leaves my own
    /// session at zero attached clients while the sidebar is fully on screen.
    /// Reading the pair alone quiesced it there, stably — no visibility edge
    /// could ever fire, because nothing was changing.
    #[test]
    fn a_client_on_a_grouped_session_still_counts_as_watching() {
        let mut a = app();
        // The row tmux actually prints in that state, verified on 3.4:
        // `%0 wact=1 sattach=0 wac=1`.
        watched_by_row(&mut a, 0, true, Some(1));
        a.observe_watchers();
        assert_eq!(a.watchers(), Watchers::Onscreen, "a visible sidebar was called detached");
        assert!(!a.quiesced);
        for _ in 0..5 {
            age(&mut a, Duration::from_millis(2500));
            assert!(gated_tick(&mut a, || complete(four())), "a visible sidebar skipped a poll");
        }

        // The same client moves to another window of the group: my row keeps
        // `wact=1` (it is still my session's current window) but nothing is
        // rendering it any more, and the viewer count is what says so.
        watched_by_row(&mut a, 0, true, Some(0));
        age(&mut a, Duration::from_secs(60));
        assert!(!gated_tick(&mut a, never_polls), "an unwatched window polled");
        assert!(a.quiesced);
    }

    /// A tmux that does not know `#{window_active_clients}` answers with the
    /// empty string, and the gate falls back to the pair — exactly the
    /// behaviour that shipped before the field was added.
    #[test]
    fn without_a_viewer_count_the_gate_falls_back_to_the_pair() {
        let mut a = app();
        watched_by_row(&mut a, 1, true, None);
        assert_eq!(a.watchers(), Watchers::Onscreen);
        watched_by_row(&mut a, 1, false, None);
        assert_eq!(a.watchers(), Watchers::OtherTab);
        watched_by_row(&mut a, 0, true, None);
        assert_eq!(a.watchers(), Watchers::Detached);
    }

    /// Coming back to the tab must not mean waiting out whatever the ladder
    /// had grown to while nobody was looking.
    #[test]
    fn becoming_visible_again_polls_on_that_very_tick() {
        let mut a = app();
        watched_by(&mut a, 1, false);
        for _ in 0..10 {
            assert!(!gated_tick(&mut a, never_polls));
        }
        // The ladder is irrelevant and the clock has just been reset by
        // nothing at all — the poll is owed by the TRANSITION.
        a.idle_streak = 99;
        a.last_agents = Instant::now();
        watched_by(&mut a, 1, true);
        assert!(
            gated_tick(&mut a, || complete(four())),
            "switching back to the tab did not refresh it"
        );
        assert_eq!(a.sessions.len(), 4);
        assert!(!a.quiesced);
        assert_eq!(a.idle_streak, 0, "the transition must collapse the ladder");
    }

    /// A skipped poll is not a failed poll, and it is not a poll.
    #[test]
    fn a_skipped_poll_raises_no_error_and_ages_no_dismissal() {
        let mut a = app();
        load(&mut a, four());
        a.on_key(press('d')); // dismiss bt/reg-update
        assert_eq!(a.hidden.ids(), ["aaaaaaaa-uuid"]);
        a.hidden_dirty = false;

        // One real poll WITHOUT the dismissed session: strike one.
        let without: Vec<Session> =
            four().into_iter().filter(|s| s.name != "bt/reg-update").collect();
        a.apply_poll(complete(without.clone()));
        assert_eq!(a.hidden_absent.len(), 1, "strike one");

        // Now nobody is watching. However many ticks pass, the second strike
        // never lands, because a skip is not a poll.
        watched_by(&mut a, 0, true);
        age(&mut a, Duration::from_secs(600));
        for _ in 0..50 {
            assert!(!gated_tick(&mut a, never_polls));
        }
        assert_eq!(a.hidden.ids(), ["aaaaaaaa-uuid"], "a skip retired a dismissal");
        assert_eq!(a.hidden_absent.len(), 1, "a skip advanced the debounce");
        assert!(!a.hidden_dirty);

        // And none of it looks like a failure.
        assert!(a.poll_error.is_none(), "a skipped poll set a poll error");
        assert_eq!(a.fail_streak, 0, "a skipped poll bumped the failure streak");
        assert_eq!(a.tick_interval(), Duration::from_millis(2500));
    }

    /// `r` is the operator saying "now", and it outranks both the gate and the
    /// ladder. It is also the path every post-verb refresh takes, so a `Ctrl+X`
    /// in a tab that just lost focus still shows its result.
    #[test]
    fn force_refresh_polls_through_a_shut_gate_and_a_long_ladder() {
        for (clients, active) in [(0u32, true), (1, false), (1, true)] {
            let mut a = app();
            watched_by(&mut a, clients, active);
            a.observe_watchers();
            a.idle_streak = 99;
            a.last_agents = Instant::now();
            assert!(!a.poll_due(), "the gate or the ladder should be holding");

            a.on_key(press('r'));
            assert!(a.force_poll, "`r` did not owe a poll");
            assert!(a.poll_due(), "`r` did not get through: clients={clients} active={active}");
            assert!(a.poll_step(|| complete(four())));
            assert_eq!(a.sessions.len(), 4);
            assert!(!a.force_poll, "the forced poll was not consumed");
        }
    }

    /// The ladder: four identical payloads before anything widens, then
    /// doubling to the 30 s ceiling — and one changed row puts it straight
    /// back on `interval`.
    #[test]
    fn the_idle_ladder_grows_on_a_still_fleet_and_collapses_on_a_change() {
        let mut a = app();
        watched_by(&mut a, 1, true);
        let base = Duration::from_millis(2500);

        a.apply_poll(complete(four()));
        assert_eq!(a.idle_streak, 0, "the first payload is a change");
        assert_eq!(a.agents_interval(), base);

        // Three more identical payloads are still the fast path.
        for _ in 0..3 {
            a.apply_poll(complete(four()));
        }
        assert_eq!(a.idle_streak, 3);
        assert_eq!(a.agents_interval(), base, "the ladder started too early");

        let rungs = [
            Duration::from_secs(5),
            Duration::from_secs(10),
            Duration::from_secs(20),
            Duration::from_secs(30),
            Duration::from_secs(30),
        ];
        for want in rungs {
            a.apply_poll(complete(four()));
            assert_eq!(a.agents_interval(), want, "streak {}", a.idle_streak);
        }

        // A change of STATE alone — same ids, same count, same names.
        let mut moved = four();
        if let Some(s) = moved.first_mut() {
            s.state = Some(State::Done);
        }
        a.apply_poll(complete(moved));
        assert_eq!(a.idle_streak, 0, "a state change did not collapse the ladder");
        assert_eq!(a.agents_interval(), base);
    }

    /// The idle ladder is what decides when the next poll happens, and it only
    /// collapses when the payload fingerprint changes. A session becoming
    /// BLOCKED must be such a change — otherwise, at the top of the ladder, the
    /// one row that needs a human would not appear on screen for another 30
    /// seconds, and only then because something else moved.
    #[test]
    fn a_session_becoming_blocked_changes_the_fingerprint() {
        let mut a = app();
        watched_by(&mut a, 1, true);

        // Climb to the top rung on a still fleet.
        for _ in 0..9 {
            a.apply_poll(complete(four()));
        }
        assert_eq!(a.agents_interval(), Duration::from_secs(30), "the ladder never grew");
        let still = a.payload_fp;

        // Same ids, same count, same names, same order — only `state` and
        // `status` move, exactly as they do when a session hits a permission
        // prompt.
        // NOTE the status stays `Busy`: a live blocked row need not carry
        // `waiting` (one of the two in the fleet on 2026-08-31 said `idle`), so
        // the fingerprint has to move on the `state` change alone.
        let mut blocked = four();
        blocked[0].state = Some(State::Blocked);
        a.apply_poll(complete(blocked));

        assert_ne!(a.payload_fp, still, "a session becoming blocked hashed identical");
        assert_eq!(a.idle_streak, 0, "blocked did not collapse the ladder");
        assert_eq!(
            a.agents_interval(),
            Duration::from_millis(2500),
            "the ladder did not collapse to base — the next poll is still far away"
        );
        // And it is on screen, at the top.
        a.rebuild_rows();
        assert!(matches!(
            a.rows.first(),
            Some(Row::Header { group: Group::Blocked, count: 1 })
        ));
    }

    /// THE DRIFT GUARD, runtime half. An unrecognised value is announced ONCE,
    /// by name, without panicking and without hiding the row.
    #[test]
    fn an_unmodelled_state_or_status_says_so_once_and_keeps_the_row() {
        let mut a = app();
        let mut odd = four();
        odd[0].state = Some(State::Unknown("hibernating".into()));
        odd[0].status = Status::Idle;
        a.apply_poll(complete(odd.clone()));

        let (msg, level) = a.message.clone().expect("no warning for an unmodelled state");
        assert!(msg.contains("hibernating"), "the warning does not name the value: {msg:?}");
        assert!(msg.contains("unmodelled"), "{msg:?}");
        assert_eq!(level, MsgLevel::Warn);
        // The row is still there, and still grouped — never dropped, never fatal.
        assert_eq!(a.sessions.len(), 4);
        assert_eq!(a.sessions[0].group(), Group::Idle, "an unknown state is still filed under Idle");
        a.rebuild_rows();
        assert!(session_rows(&a).contains(&"bt/reg-update".to_string()));

        // Once. A warning on every poll is a warning nobody reads.
        a.message = None;
        a.apply_poll(complete(odd));
        assert_eq!(a.message, None, "the same value warned twice");

        // A NEW value warns again.
        let mut odder = four();
        odder[1].status = Status::Unknown("pondering".into());
        a.apply_poll(complete(odder));
        let (msg, _) = a.message.clone().expect("a new value did not warn");
        assert!(msg.contains("pondering"), "{msg:?}");
    }

    /// The one case that must NOT warn, and the reason the guard tests for an
    /// empty string: every `state: "done"` row omits `status` entirely, which
    /// parses to `Status::Unknown("")`. A guard that cried drift on those would
    /// warn on most of a real fleet and be muted within a day.
    #[test]
    fn an_absent_status_key_is_not_drift() {
        let mut a = app();
        let mut fleet = four();
        for s in &mut fleet {
            s.state = Some(State::Done);
            s.status = Status::Unknown(String::new());
        }
        a.apply_poll(complete(fleet));
        assert_eq!(a.message, None, "an absent `status` key was reported as drift");
        assert!(a.drift_seen.is_empty());

        // Nor do the four values this build models.
        let mut a = app();
        let mut fleet = four();
        fleet[0].state = Some(State::Blocked);
        fleet[0].status = Status::Waiting;
        fleet[1].state = Some(State::Stopped);
        fleet[2].status = Status::Idle;
        a.apply_poll(complete(fleet));
        assert_eq!(a.message, None, "a modelled value was reported as drift");
    }

    /// THE REGRESSION TEST for a guard that could be switched off by accident.
    ///
    /// The warning's only channel is the one shared `message` slot, and a value
    /// used to be marked "already announced" the instant it was POSTED there.
    /// So any flash landing inside the 4s window — a `d`, a `u`, an `x`, a
    /// failed verb, or `stop_and_arm`'s own "stopped …" in the very same tick —
    /// retired it for the life of the process, and the unmodelled row went back
    /// to being a silent purple `?` under Idle. A value is now retired only
    /// once its warning has been SEEN THROUGH.
    #[test]
    fn a_drift_warning_stomped_before_it_could_be_read_comes_back() {
        let mut a = app();
        let mut odd = four();
        odd[0].state = Some(State::Unknown("hibernating".into()));
        a.apply_poll(complete(odd.clone()));
        let (first, _) = a.message.clone().expect("no warning for an unmodelled state");
        assert!(first.contains("hibernating"), "{first:?}");

        // A keypress flash overwrites it well inside its window.
        a.flash("hidden bt/reg-update — u to undo", MsgLevel::Info);
        a.apply_poll(complete(odd.clone()));
        assert!(
            a.drift_seen.is_empty(),
            "a warning nobody could have read was retired anyway"
        );
        assert!(!a.drift_pending.is_empty(), "the value was dropped from the queue");

        // That message runs out; the warning must return on its own.
        a.message = None;
        a.apply_poll(complete(odd.clone()));
        let (again, level) = a.message.clone().expect("the stomped warning never came back");
        assert!(again.contains("hibernating"), "{again:?}");
        assert_eq!(level, MsgLevel::Warn);

        // And once it HAS run its window in an uncovered footer, it is done:
        // this is a guard, not a nag.
        a.message = None;
        a.apply_poll(complete(odd.clone()));
        assert_eq!(a.message, None, "the delivered warning warned again");
        assert!(a.drift_seen.iter().any(|v| v.contains("hibernating")));
        a.message = None;
        a.apply_poll(complete(odd));
        assert_eq!(a.message, None, "the delivered warning warned again");
    }

    /// The same hole, entered through the other door: `Mode::Help`, `Mode::Logs`
    /// and the prompts render over the WHOLE sidebar rect and `Mode::Filter`
    /// owns the footer line, so a warning posted while one of them is up is
    /// written somewhere nobody can see. It must wait, not be spent.
    #[test]
    fn a_drift_warning_waits_for_an_overlay_to_come_down() {
        for cover in [Mode::Help, Mode::Logs, Mode::Filter, Mode::Prompt(PromptKind::NewBackground)] {
            let mut a = app();
            a.mode = cover.clone();
            let mut odd = four();
            odd[0].state = Some(State::Unknown("hibernating".into()));
            a.apply_poll(complete(odd.clone()));
            assert_eq!(a.message, None, "{cover:?}: warned into a covered footer");
            assert!(a.drift_seen.is_empty(), "{cover:?}: retired an unseen warning");
            assert!(
                a.drift_pending.iter().any(|v| v.contains("hibernating")),
                "{cover:?}: the value was dropped instead of queued"
            );

            // Esc, and the next poll finally says it.
            a.mode = Mode::Normal;
            a.apply_poll(complete(odd));
            let (msg, _) = a.message.clone().expect("the overlay swallowed the warning");
            assert!(msg.contains("hibernating"), "{cover:?}: {msg:?}");
        }
    }

    /// The retry rides the TICK, not the poll. Polls quiesce — the visibility
    /// gate stops them when no client is attached and the idle ladder stretches
    /// them to 30s on a still fleet — so a warning stomped at the top of the
    /// ladder would otherwise wait half a minute, and one stomped after the
    /// last client detached would wait for a client to come back. Reproduced
    /// exactly that on `-L ccmux-blkfix`: with the payload frozen and no client
    /// attached, no further poll ever arrived and the stomped warning never
    /// returned until this hook existed.
    #[test]
    fn the_drift_retry_does_not_wait_for_the_next_poll() {
        let mut a = app();
        let mut odd = four();
        odd[0].state = Some(State::Unknown("hibernating".into()));
        a.apply_poll(complete(odd));
        assert!(a.message.as_ref().unwrap().0.contains("hibernating"));

        // A keypress flash takes the footer; then it expires. No poll happens
        // at all from here on.
        a.flash("hidden bt/reg-update — u to undo", MsgLevel::Info);
        assert!(!a.tick_drift(), "posted over a message still on screen");
        a.message = None;
        a.msg_deadline = None;

        assert!(a.tick_drift(), "the tick did not bring the warning back");
        let (msg, level) = a.message.clone().expect("no warning after the tick");
        assert!(msg.contains("hibernating"), "{msg:?}");
        assert_eq!(level, MsgLevel::Warn);

        // Delivered on a later tick, and then it stops for good.
        a.message = None;
        a.msg_deadline = None;
        assert!(!a.tick_drift(), "the delivered warning posted again");
        assert!(a.drift_seen.iter().any(|v| v.contains("hibernating")));
        assert_eq!(a.message, None);
    }

    /// The two halves of the guard must scan the SAME population. `note_drift`
    /// used to run after `apply_poll` had already dropped every interactive row,
    /// while the `#[ignore]`d live test iterates `poll()`'s raw payload — so the
    /// test could fail, naming a session, while the sidebar had nothing to say
    /// and never listed that session in the first place.
    #[test]
    fn the_drift_guard_reads_rows_the_list_never_shows() {
        let mut a = app();
        let mut fleet = four();
        let mut chat = inter("11111111-2222-3333-4444-555555555555", "desktop chat");
        chat.status = Status::Unknown("pondering".into());
        fleet.push(chat);

        a.apply_poll(complete(fleet));
        let (msg, _) = a.message.clone().expect("an interactive row's drift went unannounced");
        assert!(msg.contains("pondering"), "{msg:?}");

        // Announcing it must NOT list it: interactive sessions stay unlisted.
        assert_eq!(a.sessions.len(), 4);
        assert!(a.sessions.iter().all(|s| s.kind != Kind::Interactive));
    }

    /// The ladder must never outlive a human at the keyboard.
    #[test]
    fn a_keypress_collapses_the_idle_ladder() {
        let mut a = app();
        load(&mut a, four());
        watched_by(&mut a, 1, true);
        for _ in 0..9 {
            a.apply_poll(complete(four()));
        }
        assert_eq!(a.agents_interval(), Duration::from_secs(30));

        a.on_key(press('j'));
        assert_eq!(a.idle_streak, 0);
        assert_eq!(a.agents_interval(), Duration::from_millis(2500));
        // The gap since the last spawn is already past the collapsed interval,
        // so the very next tick polls. No forced poll is owed — a keypress
        // must not put a `claude` spawn behind every `j`.
        assert!(!a.force_poll);
        age(&mut a, Duration::from_millis(2500));
        assert!(a.poll_due());
    }

    /// The fast path is the fast path: onscreen, attached, with a fleet that
    /// keeps moving, every interval polls and nothing is skipped.
    #[test]
    fn the_visible_active_sidebar_polls_exactly_as_before() {
        let mut a = app();
        watched_by(&mut a, 1, true);
        a.observe_watchers();
        assert_eq!(a.watchers(), Watchers::Onscreen);

        let mut polls = 0;
        for n in 0..12 {
            // A fleet that changes every tick — the ladder never starts.
            let mut fleet = four();
            if let Some(s) = fleet.first_mut() {
                s.name = format!("bt/reg-update {n}");
            }
            age(&mut a, Duration::from_millis(2500));
            if gated_tick(&mut a, || complete(fleet)) {
                polls += 1;
            }
        }
        assert_eq!(polls, 12, "the fast path skipped a poll");
        assert_eq!(a.agents_interval(), Duration::from_millis(2500));
        assert!(!a.quiesced);
        assert!(a.poll_error.is_none());

        // And one interval that has NOT elapsed still waits, exactly as it
        // did before the gate existed.
        a.last_agents = Instant::now();
        assert!(!a.poll_due());
    }

    // ── `d` / `u`: dismissal is a VIEW filter ───────────────────────────────

    /// Three Working sessions and one Completed, in the order `build_rows`
    /// emits them: Working newest-first, then a Spacer, then Completed.
    fn four() -> Vec<Session> {
        let mut a = bg("aaaaaaaa", "bt/reg-update", State::Working);
        let mut b = bg("bbbbbbbb", "kernel bugs", State::Working);
        let mut c = bg("cccccccc", "ccmux scaffold", State::Working);
        let mut d = bg("dddddddd", "prediction run", State::Done);
        a.started_at = 300;
        b.started_at = 200;
        c.started_at = 100;
        d.started_at = 50;
        vec![a, b, c, d]
    }

    fn session_rows(a: &App) -> Vec<String> {
        a.rows
            .iter()
            .filter_map(|r| match r {
                Row::Session { idx } => a.sessions.get(*idx).map(|s| s.name.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn dismiss_removes_exactly_one_row_and_undo_puts_it_back() {
        let mut a = app();
        load(&mut a, four());
        let before = a.rows.len();
        let names_before = session_rows(&a);
        assert_eq!(names_before[0], "bt/reg-update");

        let act = a.on_key(press('d'));

        assert_eq!(act, Action::Redraw);
        assert_eq!(a.rows.len(), before - 1, "exactly one row goes");
        assert_eq!(
            session_rows(&a),
            ["kernel bugs", "ccmux scaffold", "prediction run"]
        );
        // The poll is untouched: the header's `total` still counts it, which is
        // what makes the count read 3/4 while one row is hidden.
        assert_eq!(a.sessions.len(), 4);
        assert_eq!(a.hidden.ids(), ["aaaaaaaa-uuid"]);
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "hidden bt/reg-update — u to undo");
        assert_eq!(level, MsgLevel::Info, "hiding a row is not a warning");

        a.on_key(press('u'));

        assert_eq!(a.rows.len(), before);
        assert_eq!(session_rows(&a), names_before);
        assert!(a.hidden.ids().is_empty());
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restored bt/reg-update"
        );
        // Undo leaves the cursor ON the row it brought back.
        assert_eq!(a.selected_key.as_deref(), Some("aaaaaaaa-uuid"));
        assert_eq!(a.selected_session().map(|s| s.name.as_str()), Some("bt/reg-update"));
    }

    /// Task item 4, as a test rather than as a claim.
    ///
    /// `d` may not stop, kill, detach or attach anything, and may not issue a
    /// tmux command at all — the `@ccmux_hidden` write is DEFERRED to the next
    /// `tick`, which is what `hidden_dirty` records. This test also runs with
    /// `degraded: false`, so a stray `set-option` here would shell out to a
    /// real tmux server; the whole suite staying hermetic is part of the proof.
    #[test]
    fn dismiss_never_touches_the_agent_or_tmux() {
        let mut a = app();
        load(&mut a, four());
        let ids_before: Vec<String> = a.sessions.iter().map(|s| s.session_id.clone()).collect();
        let map_before = a.map.clone();

        a.on_key(press('d'));

        let ids_after: Vec<String> = a.sessions.iter().map(|s| s.session_id.clone()).collect();
        assert_eq!(ids_after, ids_before, "the poll is not mutated");
        assert_eq!(a.map, map_before, "no pane map entry is added or removed");
        assert!(!a.map_dirty, "no tmux pane work was queued");
        assert_eq!(a.mode, Mode::Normal, "no mode change: this is not destructive");
        assert!(a.hidden_dirty, "the option write is queued for the next tick");
        assert!(!a.should_quit);

        // And it works with no tmux at all, unlike every pane verb.
        let mut d = app();
        d.degraded = true;
        load(&mut d, four());
        d.on_key(press('d'));
        assert_eq!(d.hidden.ids().len(), 1);
        assert!(
            d.message.clone().unwrap_or_default_msg().0.starts_with("hidden "),
            "degraded mode must not refuse a view filter"
        );
    }

    #[test]
    fn dismiss_and_undo_compose_with_an_active_filter() {
        let mut a = app();
        load(&mut a, four());
        // "n" hits "kernel bugs" (Working) and "prediction run" (Completed)
        // and nothing else — not the shared cwd, not a short id.
        a.filter = "n".into();
        a.rebuild_rows();
        a.select_first();
        assert_eq!(session_rows(&a), ["kernel bugs", "prediction run"]);

        a.on_key(press('d'));
        assert_eq!(session_rows(&a), ["prediction run"], "the filter still applies");
        assert_eq!(a.hidden.ids(), ["bbbbbbbb-uuid"]);
        // The Working group emptied under the filter, so its header went too.
        assert!(!a.rows.iter().any(|r| matches!(r, Row::Header { group: Group::Working, .. })));

        a.on_key(press('u'));
        assert_eq!(session_rows(&a), ["kernel bugs", "prediction run"]);
        assert!(a.hidden.ids().is_empty());
        assert_eq!(a.filter, "n", "undo must not clear the filter");
    }

    #[test]
    fn dismiss_and_undo_compose_with_the_completed_group_hidden() {
        let mut a = app();
        load(&mut a, four());
        a.on_key(press('a')); // hide Completed
        assert!(!a.show_completed);
        assert_eq!(
            session_rows(&a),
            ["bt/reg-update", "kernel bugs", "ccmux scaffold"]
        );

        a.on_key(press('d'));
        assert_eq!(session_rows(&a), ["kernel bugs", "ccmux scaffold"]);
        // `a` hides a GROUP, `d` hides a SESSION: the Completed row is still
        // filtered out and is not the row that was dismissed.
        assert_eq!(a.hidden.ids(), ["aaaaaaaa-uuid"]);

        a.on_key(press('u'));
        assert_eq!(
            session_rows(&a),
            ["bt/reg-update", "kernel bugs", "ccmux scaffold"]
        );
        assert!(!a.show_completed, "undo must not re-show the Completed group");
    }

    /// The sharpest edge in the feature: the row under the cursor is the one
    /// that disappears, so `reanchor_selection` cannot find `selected_key`.
    #[test]
    fn dismissing_the_cursor_row_lands_on_the_next_session() {
        let mut a = app();
        load(&mut a, four());

        // Top of the list: the cursor falls to the session BELOW.
        a.on_key(press('d'));
        assert!(matches!(a.rows.get(a.selected), Some(Row::Session { .. })));
        assert_eq!(a.selected_session().map(|s| s.name.as_str()), Some("kernel bugs"));

        // Bottom of the list: there is nothing below, so it rises to the one
        // ABOVE — never onto the Header or the Spacer that precede it.
        a.select_last();
        assert_eq!(a.selected_session().map(|s| s.name.as_str()), Some("prediction run"));
        a.on_key(press('d'));
        assert!(
            matches!(a.rows.get(a.selected), Some(Row::Session { .. })),
            "never a Header or a Spacer"
        );
        assert_eq!(a.selected_session().map(|s| s.name.as_str()), Some("ccmux scaffold"));
        assert!(a.selected < a.rows.len(), "never past the end");
        // The Completed group emptied, so its header and spacer went with it.
        assert!(!a.rows.iter().any(|r| matches!(r, Row::Header { group: Group::Completed, .. })));
        assert!(!a.rows.iter().any(|r| matches!(r, Row::Spacer)));
    }

    #[test]
    fn dismissing_the_last_visible_session_leaves_a_coherent_empty_list() {
        let mut a = app();
        load(&mut a, vec![bg("aaaaaaaa", "bt/reg-update", State::Working)]);

        a.on_key(press('d'));

        assert!(a.rows.is_empty(), "the header goes with its last row");
        assert_eq!(a.selected, a.rows.len(), "parked per the field contract");
        assert_eq!(a.selected_key, None);
        assert_eq!(a.scroll, 0);
        assert!(a.selected_session().is_none());
        // Every movement key is a no-op on an empty list, not a panic.
        for c in ['j', 'k', 'g', 'G'] {
            a.on_key(press(c));
            assert_eq!(a.selected, a.rows.len());
        }
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.selected, 0);

        a.on_key(press('u'));
        assert_eq!(session_rows(&a), ["bt/reg-update"]);
        assert!(matches!(a.rows.get(a.selected), Some(Row::Session { .. })));
    }

    #[test]
    fn undo_with_nothing_dismissed_says_so_and_changes_nothing() {
        let mut a = app();
        load(&mut a, four());
        let rows_before = a.rows.clone();
        let selected_before = a.selected;

        let act = a.on_key(press('u'));

        assert_eq!(act, Action::Redraw);
        assert_eq!(a.rows, rows_before);
        assert_eq!(a.selected, selected_before);
        assert!(!a.hidden_dirty, "a no-op must not queue an option write");
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "nothing to undo");
        assert_eq!(level, MsgLevel::Info);
    }

    /// A dismissal outlives polls (that is the whole point for a permanently
    /// dead row like a Claude Desktop session), but must be reconciled away
    /// once the session leaves the poll, so the set cannot grow without bound.
    #[test]
    fn a_dismissal_survives_polls_and_reconciles_away_when_the_session_ends() {
        let mut a = app();
        load(&mut a, four());
        a.on_key(press('d'));
        a.hidden_dirty = false;
        assert_eq!(a.hidden.ids(), ["aaaaaaaa-uuid"]);

        // A fresh poll returning the same four sessions: still hidden. Driven
        // through `apply_poll`, which is the whole of what `tick` decides —
        // calling `hidden.reconcile` by hand here would only re-test
        // `HiddenSet`, which `tmux.rs` already covers.
        a.apply_poll(complete(four()));
        assert_eq!(a.hidden.ids(), ["aaaaaaaa-uuid"]);
        assert!(!session_rows(&a).contains(&"bt/reg-update".to_string()));

        // Now the session ends and leaves the poll entirely. ONE poll is not
        // enough — a single under-reporting poll must never cost a dismissal.
        let gone: Vec<Session> = four().into_iter().filter(|s| s.name != "bt/reg-update").collect();
        a.apply_poll(complete(gone.clone()));
        assert_eq!(a.hidden.ids(), ["aaaaaaaa-uuid"], "one absence is a strike, not a verdict");

        // The second poll agrees, and only now is the id dropped.
        a.apply_poll(complete(gone));
        assert!(a.hidden.ids().is_empty(), "two agreeing polls drop the vanished id");
        assert!(a.hidden_dirty, "and the shrunken set is queued for the option");
        assert!(matches!(a.rows.get(a.selected), Some(Row::Session { .. })));
    }

    /// `u` must un-hide even when the row it restores is still off screen for
    /// an unrelated reason — and must say so, or it looks like it did nothing.
    #[test]
    fn undo_after_the_filter_changed_restores_and_says_it_is_filtered_out() {
        let mut a = app();
        load(&mut a, four());
        a.on_key(press('d')); // hides bt/reg-update

        a.filter = "kernel".into();
        a.rebuild_rows();
        a.on_key(press('u'));

        assert!(a.hidden.ids().is_empty(), "the dismissal is undone regardless");
        assert_eq!(session_rows(&a), ["kernel bugs"], "the filter still rules the view");
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "restored bt/reg-update — filtered out");
        assert_eq!(level, MsgLevel::Warn);

        // Clearing the filter shows it again, with no second `u`.
        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(session_rows(&a).contains(&"bt/reg-update".to_string()));
    }

    /// Interactive sessions are NEVER listed. ccmux is a background-agent
    /// explorer: there is no `claude attach` for an interactive session, and one
    /// owned by Claude Desktop has no tmux pane to jump to either, so such a row
    /// was permanently un-openable.
    ///
    /// Replaces `a_desktop_session_with_no_short_id_dismisses_and_stays_dismissed`.
    /// That test dismissed a Claude Desktop row; no such row can now reach the
    /// list to be dismissed at all, which is the stronger guarantee.
    #[test]
    fn interactive_sessions_never_reach_the_list() {
        let desktop = inter("9f2c1a44-1111-4038-8de7-d5f112c92360", "claude-a1");
        let terminal = inter("5b605ed1-2222-4038-8de7-d5f112c92362", "claude-3c");
        let mut a = app();

        // Through the REAL path — `apply_poll`, not the `load` helper, which
        // assigns `sessions` directly and would bypass the policy under test.
        for _ in 0..3 {
            a.apply_poll(complete(vec![
                desktop.clone(),
                terminal.clone(),
                bg("aaaaaaaa", "bt/reg-update", State::Working),
            ]));
            a.rebuild_rows();
            assert_eq!(session_rows(&a), ["bt/reg-update"]);
            // `total` counts what ccmux manages, so the header cannot show a
            // `matching/total` ratio for rows that were never listed.
            assert_eq!(a.sessions.len(), 1);
        }
        // Neither parentage matters: a terminal-hosted interactive session is
        // excluded exactly like a Desktop-hosted one.
        assert!(a.sessions.iter().all(|s| s.kind != Kind::Interactive));

        // The exclusion must NOT read as a lossy payload. `reconcile_hidden`
        // distrusts an incomplete poll, so if excluding rows poisoned that flag
        // a dismissed background session could never reconcile away.
        let mut b = app();
        b.apply_poll(complete(vec![
            desktop.clone(),
            bg("bbbbbbbb", "alpha/opt", State::Done),
        ]));
        b.rebuild_rows();
        b.select_first();
        b.on_key(press('d'));
        assert_eq!(b.hidden.ids(), ["bbbbbbbb-uuid"]);
        for _ in 0..2 {
            b.apply_poll(complete(vec![desktop.clone()]));
        }
        assert!(
            b.hidden.ids().is_empty(),
            "excluding interactive rows must not poison poll completeness"
        );
    }

    /// The dismiss flash names a session with an empty name by its short id
    /// rather than rendering a bare dash. Kept from the replaced test, re-cut
    /// onto a background session since interactive ones are no longer listed.
    #[test]
    fn a_nameless_session_is_still_nameable_in_the_dismiss_flash() {
        let mut b = app();
        let mut nameless = bg("7e0b33aa", "", State::Done);
        nameless.name = String::new();
        load(&mut b, vec![nameless]);
        b.on_key(press('d'));
        assert_eq!(
            b.message.clone().unwrap_or_default_msg().0,
            "hidden 7e0b33aa — u to undo"
        );
    }

    // ── the flush: `tick` is not the only way out of the event loop ─────────

    /// REGRESSION. Persistence used to happen ONLY in `tick`, so a `d` inside
    /// one poll interval of `q` was never written: `@ccmux_hidden` stayed
    /// unset and the relaunched sidebar showed the row again. At the default
    /// 2500ms interval that is most of the motivating flow — hide the
    /// permanently dead Claude Desktop row, quit — and it falsified the
    /// README's own "quit and relaunch and they are still there".
    #[test]
    fn a_dismissal_reaches_tmux_when_the_sidebar_quits_before_the_next_tick() {
        let mut a = app();
        a.tmux_session = "ccmux-test-flush-d".into();
        own(&mut a, "%3", "@1");
        load(&mut a, four());

        a.on_key(press('d'));
        assert_eq!(a.hidden.ids(), ["aaaaaaaa-uuid"]);
        assert!(a.hidden_dirty, "the keypress queues the write, it does not issue it");

        arm_hermetic_flush(&a);
        assert_eq!(a.on_key(press('q')), Action::Quit);
        a.shutdown();

        assert!(!a.hidden_dirty, "quitting inside one poll interval must not drop the write");
        assert!(
            !a.message.clone().unwrap_or_default_msg().0.contains("not saved"),
            "the flush must have succeeded, not been swallowed: {:?}",
            a.message
        );
    }

    /// REGRESSION, and the harmful direction of the same defect: an `u` the
    /// operator watched take effect could be thrown away by quitting, leaving
    /// the row hidden again on relaunch with nothing said. The value that must
    /// reach `@ccmux_hidden` is the EMPTY set, not the stale one.
    #[test]
    fn an_undo_reaches_tmux_when_the_sidebar_quits_before_the_next_tick() {
        let mut a = app();
        a.tmux_session = "ccmux-test-flush-u".into();
        own(&mut a, "%3", "@1");
        load(&mut a, four());

        a.on_key(press('d'));
        arm_hermetic_flush(&a);
        a.shutdown(); // stands in for the tick that persisted the dismissal
        assert!(!a.hidden_dirty);

        a.on_key(press('u'));
        assert!(a.hidden.ids().is_empty());
        assert!(a.hidden_dirty, "the undo is queued too");
        assert_eq!(
            serde_json::to_string(&a.hidden).expect("serialize"),
            r#"{"v":1,"ids":[]}"#,
            "the value the flush owes tmux is the empty set"
        );

        arm_hermetic_flush(&a);
        assert_eq!(a.on_key(press('q')), Action::Quit);
        a.shutdown();
        assert!(!a.hidden_dirty, "the undo must not be reverted by quitting");
    }

    /// `shutdown` is the only flush site outside `tick`, so it must also be a
    /// no-op when there is nothing owed — and must never fire in degraded mode,
    /// where the set is memory-only by design.
    #[test]
    fn shutdown_is_a_no_op_with_nothing_owed_and_in_degraded_mode() {
        let mut a = app();
        a.shutdown();
        assert!(a.message.is_none(), "a clean exit says nothing");

        let mut d = app();
        d.degraded = true;
        load(&mut d, four());
        d.on_key(press('d'));
        d.shutdown(); // must not reach tmux at all
        assert!(d.hidden_dirty, "degraded mode never writes the option");
        assert_eq!(d.hidden.ids(), ["aaaaaaaa-uuid"], "but the row still hides");
    }

    // ── reconciliation: a poll that under-reports is not a death notice ─────

    /// REGRESSION. `parse_sessions` drops individual malformed rows on purpose,
    /// so a poll can exit 0, parse, and still be missing sessions that are very
    /// much alive. Reconciling against it used to erase those dismissals AND
    /// the `u` that would undo them — unrecoverably, and with no message.
    #[test]
    fn a_lossy_poll_concludes_nothing_about_a_dismissed_session() {
        let mut a = app();
        load(&mut a, four());
        a.on_key(press('d'));
        a.hidden_dirty = false;

        // `claude agents` exits 0, the payload parses, but the dismissed row is
        // one of the ones `into_session` had to skip.
        let survivors: Vec<Session> =
            four().into_iter().filter(|s| s.name != "bt/reg-update").collect();
        a.apply_poll(Ok(model::Payload { sessions: survivors.clone(), dropped: 1 }));

        assert_eq!(a.hidden.ids(), ["aaaaaaaa-uuid"], "a lossy poll is not evidence");
        assert!(!a.hidden_dirty, "and nothing was written");
        assert!(a.hidden_absent.is_empty(), "it does not even count as a strike");

        // It never becomes evidence, either: repeat it and the dismissal holds.
        for _ in 0..5 {
            a.apply_poll(Ok(model::Payload { sessions: survivors.clone(), dropped: 1 }));
        }
        assert_eq!(a.hidden.ids(), ["aaaaaaaa-uuid"]);

        // Undo still works, which is the point — the row is recoverable.
        a.on_key(press('u'));
        assert!(a.hidden.ids().is_empty());
    }

    /// A strike must not outlive the dismissal that earned it. `d` -> absent
    /// complete poll (strike) -> `u` -> a LOSSY poll (which recomputes nothing)
    /// -> `d` again used to leave the old strike standing, so the re-dismissed
    /// id would be dropped after ONE absence instead of two.
    #[test]
    fn a_fresh_dismissal_starts_with_no_strike_against_it() {
        let mut a = app();
        load(&mut a, four());
        let without: Vec<Session> =
            four().into_iter().filter(|s| s.name != "bt/reg-update").collect();

        a.on_key(press('d'));
        a.apply_poll(complete(without.clone()));
        assert_eq!(a.hidden_absent.len(), 1, "one strike is on the books");

        a.on_key(press('u'));
        // A lossy poll that DOES carry the session: reconciliation is skipped
        // wholesale, so nothing recomputes the strike set.
        a.apply_poll(Ok(model::Payload { sessions: four(), dropped: 1 }));
        assert_eq!(a.hidden_absent.len(), 1, "the stale strike is still there");

        a.on_key(press('d'));
        a.apply_poll(complete(without));
        assert_eq!(
            a.hidden.ids(),
            ["aaaaaaaa-uuid"],
            "the re-dismissal is owed two absences of its own, not one"
        );
    }

    /// REGRESSION, degenerate case: `claude` hiccups and exits 0 with `[]`.
    /// That parses perfectly and cannot be told from a real empty list, so it
    /// is debounced instead — one such poll used to wipe the entire set and the
    /// whole undo stack in a single tick.
    #[test]
    fn one_empty_poll_never_erases_the_dismissed_set_but_two_do() {
        let mut a = app();
        load(&mut a, four());
        a.on_key(press('d')); // bt/reg-update
        a.on_key(press('d')); // kernel bugs
        assert_eq!(a.hidden.ids(), ["aaaaaaaa-uuid", "bbbbbbbb-uuid"]);
        a.hidden_dirty = false;

        a.apply_poll(complete(Vec::new()));
        assert_eq!(
            a.hidden.ids(),
            ["aaaaaaaa-uuid", "bbbbbbbb-uuid"],
            "one empty-but-successful poll must not wipe the set"
        );
        assert!(!a.hidden_dirty, "nor persist a wiped set");

        // Two consecutive polls agreeing is the bar, and `[]` really can mean
        // the sessions are gone — so the second one does drop them.
        a.apply_poll(complete(Vec::new()));
        assert!(a.hidden.ids().is_empty());
        assert!(a.hidden_dirty);
    }

    /// A session that misses ONE poll and comes back keeps its dismissal
    /// forever: the strike has to be consecutive, or a flaky CLI would erase
    /// dismissals a poll at a time.
    #[test]
    fn a_session_that_blinks_out_of_one_poll_keeps_its_dismissal() {
        let mut a = app();
        load(&mut a, four());
        a.on_key(press('d'));

        let without: Vec<Session> =
            four().into_iter().filter(|s| s.name != "bt/reg-update").collect();
        for _ in 0..4 {
            a.apply_poll(complete(without.clone())); // strike
            a.apply_poll(complete(four())); // and it is back: strike cleared
            assert_eq!(a.hidden.ids(), ["aaaaaaaa-uuid"]);
        }
        assert!(!session_rows(&a).contains(&"bt/reg-update".to_string()));
    }

    /// An `Err` poll keeps the last good list on screen, so it must conclude
    /// nothing at all — including nothing about the dismissed set.
    #[test]
    fn a_failed_poll_leaves_the_dismissed_set_and_the_last_good_list_alone() {
        let mut a = app();
        load(&mut a, four());
        a.on_key(press('d'));
        a.hidden_dirty = false;

        for _ in 0..3 {
            a.apply_poll(Err(agents::AgentsError::Cmd {
                code: 1,
                stderr: "claude: connection refused".into(),
            }));
        }

        assert_eq!(a.hidden.ids(), ["aaaaaaaa-uuid"]);
        assert!(!a.hidden_dirty);
        assert_eq!(a.sessions.len(), 4, "the last good list stays");
        assert_eq!(a.fail_streak, 3);
        assert!(a.poll_error.is_some());
    }

    // ── the cursor after `u` ────────────────────────────────────────────────

    /// INVARIANT PIN, not a regression: `u` on a row `/` still hides must leave
    /// the cursor exactly where it was.
    ///
    /// This held before `act_undo_dismiss` was reworked, but only by
    /// coincidence. `build_rows` returns byte-identical rows when the restored
    /// row stays filtered out, so `reanchor_selection`'s nearest-index fallback
    /// resolved to `abs_diff(target, target) == 0` — the row the cursor was
    /// already on. `act_undo_dismiss` now states the condition instead of
    /// leaning on that coincidence, and this test is what stops either half
    /// drifting.
    #[test]
    fn undo_of_a_filtered_out_row_leaves_the_cursor_where_it_was() {
        let mut a = app();
        load(&mut a, four());
        a.on_key(press('d')); // hides bt/reg-update

        // A filter that excludes the dismissed row, and a cursor deliberately
        // parked on a row that is NOT the nearest index to the restored one.
        a.filter = "n".into(); // kernel bugs, prediction run — never bt/reg-update
        a.rebuild_rows();
        a.select_last();
        let before = a.selected_key.clone();
        assert!(before.is_some(), "the cursor is on a real row to begin with");

        a.on_key(press('u'));

        assert!(a.hidden.ids().is_empty(), "the undo happened");
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "restored bt/reg-update — filtered out");
        assert_eq!(level, MsgLevel::Warn);
        assert_eq!(a.selected_key, before, "an invisible row is not worth moving the cursor for");

        // And it is still there once the filter is gone: the restored row is
        // back at the top, unselected, and the cursor has not been handed to
        // anything it was not already on.
        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(a.selected_key, before);
        assert!(session_rows(&a).contains(&"bt/reg-update".to_string()));
    }

    // ── `d` on a row that owns a pane ───────────────────────────────────────

    /// REGRESSION. The row is the only place `x` and `Enter` can be reached
    /// from, so dismissing a session ccmux opened a pane for leaves that pane
    /// on screen with no way to close or jump to it. Not killing it is correct;
    /// saying nothing was not. The `@ccmux_map` entry stays on purpose — the
    /// pane is live, and `u` then `x` is the recovery it makes possible.
    #[test]
    fn dismissing_a_session_with_an_open_pane_warns_and_keeps_the_map_entry() {
        let mut a = app();
        load(&mut a, four());
        let pane = PaneId::parse("%7").expect("valid pane id");
        a.map.insert(
            &pane,
            PaneEntry {
                session_id: "aaaaaaaa-uuid".into(),
                short_id: "aaaaaaaa".into(),
                name: "bt/reg-update".into(),
                opened_at: 0,
            },
        );

        a.on_key(press('d'));

        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(text, "hidden — pane open, u to undo");
        assert_eq!(level, MsgLevel::Warn, "yellow, because a pane is now unreachable");
        assert!(text.contains("u to undo"), "the recovery must survive every variant");
        assert!(
            model::display_width(&text) <= 34,
            "the warning still has to fit the narrowest sidebar: {text:?}"
        );
        assert_eq!(
            a.map.pane_for_session("aaaaaaaa-uuid"),
            Some(pane),
            "the map entry is what `u` then `x` needs; dismissal must not touch it"
        );

        // A row with no ccmux pane keeps the plain, named flash.
        a.on_key(press('u'));
        a.map.remove(&PaneId::parse("%7").expect("valid pane id"));
        a.on_key(press('d'));
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "hidden bt/reg-update — u to undo"
        );
    }

    /// `Ctrl-d` / `Ctrl-u` must keep scrolling: the ctrl arms are matched first.
    #[test]
    fn ctrl_d_and_ctrl_u_still_scroll_and_never_dismiss() {
        let mut a = app();
        load(&mut a, four());
        a.viewport = 4;

        a.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(a.hidden.ids().is_empty(), "Ctrl-d is half-page down, not dismiss");
        assert_ne!(a.selected, 1, "it moved the cursor");
        a.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(a.hidden.ids().is_empty());
        assert!(a.message.is_none(), "no 'nothing to undo' from Ctrl-u");
    }

    // ── `R` — restart in place (§8.11) ──────────────────────────────────────

    thread_local! {
        /// Every `(pane, command)` the respawn seam was handed, in order.
        static RESPAWNS: std::cell::RefCell<Vec<(String, String)>> =
            const { std::cell::RefCell::new(Vec::new()) };
        /// ONE TIMELINE across BOTH seams, which is the only way the ordering
        /// property can be asserted at all: "every stop lands before any
        /// respawn" is a statement about two recorders, and two separate lists
        /// cannot say anything about which came first.
        static ORDER: std::cell::RefCell<Vec<String>> =
            const { std::cell::RefCell::new(Vec::new()) };
        /// What `fixture_poll` answers, as a QUEUE: entry N answers poll N and
        /// the last entry repeats forever. A one-entry queue is a fleet that
        /// never changes; a longer one stages a fleet that changes BETWEEN two
        /// reads, which is the race the re-poll exists to narrow.
        static FLEET: std::cell::RefCell<Vec<Result<Vec<Session>, ()>>> =
            const { std::cell::RefCell::new(Vec::new()) };
        /// Panes whose respawn the seam must REFUSE. Empty by default, so
        /// every test that does not name a pane sees the seam it always saw.
        /// A respawn can genuinely fail — the pane was killed between the
        /// phases, tmux said no — and that failure is the difference between
        /// an agent that was restarted and one left halted.
        static RESPAWN_FAIL: std::cell::RefCell<BTreeSet<String>> =
            const { std::cell::RefCell::new(BTreeSet::new()) };
        /// Short ids whose paneless `claude respawn` must fail. Empty by
        /// default. A respawn can fail on its own — the daemon refused, the
        /// CLI wedged, `run_bounded` killed the client — and with no pane to
        /// fall back on, what that failure MEANT is the whole difference
        /// between an agent still on its old binary and one halted with
        /// nothing anywhere that would bring it back.
        static RESPAWN_AGENT_FAIL: std::cell::RefCell<BTreeSet<String>> =
            const { std::cell::RefCell::new(BTreeSet::new()) };
        /// What `agents_attached` answers: `Some(ids)` for a scan that ran,
        /// `None` for one that could not. Default `Some(empty)` — no clients —
        /// so a test that does not mention the guard sees the fleet it always
        /// saw.
        static ATTACHED: std::cell::RefCell<Option<BTreeSet<String>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// The respawn seam, recording instead of shelling out. Thread-local, so
    /// two `R` tests running in parallel cannot see each other's calls.
    fn recording_respawn(_session: &str, pane: &PaneId, cmd: &str) -> Result<(), TmuxError> {
        ORDER.with(|o| o.borrow_mut().push(format!("respawn {pane}")));
        RESPAWNS.with(|r| r.borrow_mut().push((pane.to_string(), cmd.to_string())));
        if RESPAWN_FAIL.with(|f| f.borrow().contains(pane.as_str())) {
            return Err(TmuxError::BadTarget(format!("{pane} is gone")));
        }
        Ok(())
    }

    /// Make the respawn of `pane` fail, as tmux does when the pane was killed
    /// between PHASE 1 and PHASE 2.
    fn respawn_fails(pane: &str) {
        RESPAWN_FAIL.with(|f| {
            f.borrow_mut().insert(pane.to_string());
        });
    }

    fn recorded() -> Vec<(String, String)> {
        RESPAWNS.with(|r| r.borrow().clone())
    }

    /// The poll seam for `R`'s agent pass. Records the read on the shared
    /// timeline so a test can count how many times the state was re-read.
    fn fixture_poll() -> Result<model::Payload, AgentsError> {
        ORDER.with(|o| o.borrow_mut().push("poll".into()));
        FLEET.with(|f| {
            let mut q = f.borrow_mut();
            let next = if q.len() > 1 {
                q.remove(0)
            } else {
                q.first().cloned().unwrap_or(Ok(Vec::new()))
            };
            match next {
                Ok(sessions) => Ok(model::Payload { sessions, dropped: 0 }),
                Err(()) => Err(AgentsError::Cmd {
                    code: 1,
                    stderr: "daemon is not running".into(),
                }),
            }
        })
    }

    /// The stop seam. It records on the shared timeline AND calls through to
    /// `agents::stop`, whose process boundary is closed in test builds — so the
    /// same call proves the ORDER here and the exact argv (`stop <id>`) in
    /// `agents::test_spawn`, and `fail_next` still works on it.
    fn recording_stop(id: &str) -> Result<(), AgentsError> {
        ORDER.with(|o| o.borrow_mut().push(format!("stop {id}")));
        agents::stop(id)
    }

    /// The paneless-restart seam, on the SAME timeline as the stops and the
    /// pane respawns — which is the only way the phase ordering can be
    /// asserted at all — and calling through to `agents::respawn`, whose
    /// process boundary is closed in test builds, so `agents::test_spawn`
    /// still proves the exact argv.
    ///
    /// Logged as `respawn-agent <id>`, not `respawn <id>`: the pane seam
    /// already owns `respawn %N` on this timeline, and an ordering assertion
    /// that could not tell a pane from an agent would pass for the wrong
    /// reason.
    fn recording_agent_respawn(id: &str) -> Result<(), AgentsError> {
        ORDER.with(|o| o.borrow_mut().push(format!("respawn-agent {id}")));
        if RESPAWN_AGENT_FAIL.with(|f| f.borrow().contains(id)) {
            return Err(AgentsError::Cmd {
                code: 1,
                stderr: "daemon refused the respawn".into(),
            });
        }
        agents::respawn(id)
    }

    /// Make the paneless restart of `id` fail, as it does when the daemon
    /// refuses — or when `run_bounded` kills a client that had already asked.
    fn agent_respawn_fails(id: &str) {
        RESPAWN_AGENT_FAIL.with(|f| {
            f.borrow_mut().insert(id.to_string());
        });
    }

    /// The attach-scan seam. `None` is a scan that could NOT run, which is a
    /// different thing from one that found nobody.
    fn fixture_attached() -> Option<BTreeSet<String>> {
        ATTACHED.with(|a| a.borrow().clone())
    }

    /// Say who holds a live `claude attach`, anywhere on the machine.
    fn attached(ids: &[&str]) {
        ATTACHED.with(|a| *a.borrow_mut() = Some(ids.iter().map(|s| (*s).to_string()).collect()));
    }

    /// Say the scan itself could not run — no readable `/proc`.
    fn attach_scan_fails() {
        ATTACHED.with(|a| *a.borrow_mut() = None);
    }

    fn order() -> Vec<String> {
        ORDER.with(|o| o.borrow().clone())
    }

    /// Script the fleet `fixture_poll` answers with, one `Vec` per poll.
    fn fleet(polls: Vec<Result<Vec<Session>, ()>>) {
        FLEET.with(|f| *f.borrow_mut() = polls);
    }

    /// One ccmux tab plus a second one, and — the point of the fixture — `%3`,
    /// an UNMANAGED pane in ccmux's own window. It is in no map and is no
    /// window's marker, which is exactly the state of the operator's
    /// `opencode` pane in the live session.
    fn restartable(a: &mut App) {
        own(a, "%1", "@1");
        a.panes = vec![
            pane("%1", 1, 0, 0, 34, false),
            pane("%2", 1, 1, 35, 40, false),
            pane("%3", 1, 2, 76, 40, true),
            pane("%4", 2, 0, 0, 34, false),
            pane("%5", 2, 1, 35, 80, false),
        ];
        let mut m1 = PaneMap::new();
        m1.insert(
            &PaneId::parse("%2").expect("pane id"),
            PaneEntry {
                session_id: "sid-a".into(),
                short_id: "aaaaaaaa".into(),
                name: "a".into(),
                opened_at: 0,
            },
        );
        let mut m2 = PaneMap::new();
        m2.insert(
            &PaneId::parse("%5").expect("pane id"),
            PaneEntry {
                session_id: "sid-b".into(),
                short_id: "bbbbbbbb".into(),
                name: "b".into(),
                opened_at: 0,
            },
        );
        // My own window's map is `self.map` — the record this process writes —
        // and `tabs[0].map` is the stored shadow of it that `list-windows`
        // reads back. In the ordinary case they agree, and the fixture says so.
        a.map = m1.clone();
        a.tabs = vec![
            TabInfo {
                window: WindowId::parse("@1").expect("window id"),
                index: 1,
                sidebar: PaneId::parse("%1"),
                map: m1,
                hidden: HiddenLog::new(),
            },
            TabInfo {
                window: WindowId::parse("@2").expect("window id"),
                index: 2,
                sidebar: PaneId::parse("%4"),
                map: m2,
                hidden: HiddenLog::new(),
            },
        ];
        a.respawn = recording_respawn;
        a.agents_poll = fixture_poll;
        a.agents_stop = recording_stop;
        a.agents_respawn = recording_agent_respawn;
        a.agents_attached = fixture_attached;
        // Both mapped panes hold a session that is finished and idle, so the
        // fixture's default is "everything in scope is restartable" and a test
        // that cares about a busy agent says so by scripting its own fleet.
        //
        // `cccccccc` has no pane, and `bg` makes it DORMANT — so the headless
        // population is empty here and the fixture's default behaviour is the
        // pane-only behaviour every test written before it still expects. A
        // test that wants a live paneless agent wraps the row in `live`.
        fleet(vec![Ok(vec![
            bg("aaaaaaaa", "a", State::Done),
            bg("bbbbbbbb", "b", State::Done),
            bg("cccccccc", "c", State::Done),
        ])]);
        RESPAWNS.with(|r| r.borrow_mut().clear());
        ORDER.with(|o| o.borrow_mut().clear());
        RESPAWN_FAIL.with(|f| f.borrow_mut().clear());
        RESPAWN_AGENT_FAIL.with(|f| f.borrow_mut().clear());
        // The fixture's ambient truth: the scan RAN and nobody is attached.
        attached(&[]);
        agents::test_spawn::reset();
    }

    /// THE SAFETY TEST: the panes `R` respawns are exactly the panes ccmux can
    /// prove it created. Never `%3`, the operator's own pane sitting in ccmux's
    /// own window — the state of the live session's `opencode` pane. Never
    /// `%1` either, this process's own, which restarted by `exec`.
    #[test]
    fn r_restarts_only_the_panes_ccmux_owns() {
        let mut a = app();
        restartable(&mut a);

        a.restart_others();

        let got = recorded();
        let panes: Vec<&str> = got.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(panes, vec!["%2", "%4", "%5"]);
        assert!(
            !panes.contains(&"%3"),
            "an unmanaged pane must never be respawned: {panes:?}"
        );
        assert!(
            !panes.contains(&"%1"),
            "my own pane restarts by exec, never by respawn: {panes:?}"
        );

        // The commands are the ones that rebuild each role, not a guess.
        assert!(got[0].1.contains("attach aaaaaaaa;"), "{:?}", got[0].1);
        // The other tab's sidebar is respawned with a command built from the
        // FRESHLY RESOLVED binary. The fixture's `sidebar_cmd` is `None`, so
        // the only way this string exists is that `R` rebuilt it rather than
        // reusing the one the launcher computed from a `current_exe()` that a
        // `cargo install` may since have turned into a deleted inode.
        assert!(a.sidebar_cmd.is_none(), "nothing stale was available to reuse");
        let exe = restart::exe_path().expect("the test binary is on disk");
        let want = restart::sidebar_command(&exe).expect("build");
        assert_eq!(got[1].1, want);
        assert!(got[2].1.contains("attach bbbbbbbb;"), "{:?}", got[2].1);

        // The summary counts this process too — it restarted first, by the
        // better mechanism, which is why it is here to restart the rest.
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 2 agents"
        );
    }

    /// THE KEYPRESS, and the ordering that keeps a bad binary survivable.
    ///
    /// `R` is reachable in the Normal keymap, and it authorises a restart
    /// WITHOUT killing anything: the respawns belong to the image it execs
    /// into. The recording stub is the proof — it saw no calls.
    #[test]
    fn r_authorises_the_restart_and_kills_nothing_yet() {
        let mut a = app();
        restartable(&mut a);

        let act = a.on_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT));

        assert_eq!(act, Action::Restart, "`R` is reachable in the Normal keymap");
        assert!(
            recorded().is_empty(),
            "no pane may be killed before the new binary is proven to run: {:?}",
            recorded()
        );
        let p = a.pending_restart.take().expect("a restart was authorised");
        assert!(p.exe.is_file(), "the binary is resolved before the exec");
    }

    /// THE REGRESSION TEST for a pre-check that only looked at a file mode.
    ///
    /// A candidate that cannot be run must stop the restart DEAD — no pending
    /// exec, and above all no respawn. Before the `exec`-first ordering, this
    /// case had already killed every other tab's sidebar by the time `execve`
    /// returned ENOENT, and the survivor could only say "restart failed".
    #[test]
    fn a_binary_that_does_not_run_stops_the_restart_before_any_pane_dies() {
        let mut a = app();
        restartable(&mut a);
        a.probe = |_| Err("the new ccmux will not start: bad interpreter".into());

        let act = a.on_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT));

        assert_eq!(act, Action::Redraw, "the sidebar stays where it is");
        assert!(a.pending_restart.is_none(), "nothing to exec");
        assert!(
            recorded().is_empty(),
            "no pane is killed on the strength of a binary that cannot run: {:?}",
            recorded()
        );
        let (text, level) = a.message.clone().unwrap_or_default_msg();
        assert_eq!(
            text,
            "the new ccmux will not start: bad interpreter — not restarting"
        );
        assert_eq!(level, MsgLevel::Error);
    }

    /// THE REGRESSION TEST for planning off a stale snapshot.
    ///
    /// `%6` was opened moments ago: `act_open` put it in `self.map` and the
    /// `@ccmux_tab_map` snapshot in `self.tabs` has not caught up — which is
    /// the guaranteed state right after an `o`, since `act_open` refreshes
    /// before it saves. Planning from the snapshot left `%6` running the
    /// `claude` binary the operator had just replaced while the footer counted
    /// it as restarted. The in-memory map is the authoritative record and is
    /// what decides.
    #[test]
    fn a_pane_opened_since_the_last_snapshot_is_still_restarted() {
        let mut a = app();
        restartable(&mut a);
        // Live, and in MY map, but not yet in the stored snapshot of it.
        a.panes.push(pane("%6", 1, 3, 116, 40, false));
        a.map = a.tabs[0].map.clone();
        a.map.insert(
            &PaneId::parse("%6").expect("pane id"),
            PaneEntry {
                session_id: "sid-c".into(),
                short_id: "cccccccc".into(),
                name: "c".into(),
                opened_at: 0,
            },
        );
        assert!(
            a.tabs[0].map.get(&PaneId::parse("%6").expect("pane id")).is_none(),
            "the snapshot is one write behind, which is the bug"
        );

        a.restart_others();

        let panes: Vec<String> = recorded().into_iter().map(|(p, _)| p).collect();
        assert!(panes.contains(&"%6".to_string()), "the new pane was skipped: {panes:?}");
        assert!(!panes.contains(&"%3".to_string()), "still no unmanaged pane: {panes:?}");
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 3 panes, 3 agents",
            "and it is counted, so the footer is not a lie"
        );
    }

    /// `R` end to end, on the pane the operator is actually sitting in. The map
    /// still names `%2` — correctly, ccmux opened it — but its attach is gone
    /// and the operator's shell is in there, so the respawn that would blow it
    /// away must not be issued, and the footer must own up to the pane it left
    /// alone.
    #[test]
    fn r_leaves_a_pane_that_fell_through_to_the_shell_alone() {
        let mut a = app();
        restartable(&mut a);
        if let Some(p) = a.panes.iter_mut().find(|p| p.id.as_str() == "%2") {
            p.detached = true;
        }

        a.restart_others();

        let panes: Vec<String> = recorded().into_iter().map(|(p, _)| p).collect();
        assert!(!panes.contains(&"%2".to_string()), "the shell was respawned: {panes:?}");
        assert!(panes.contains(&"%5".to_string()), "a live attach is still restarted: {panes:?}");
        assert!(!panes.contains(&"%3".to_string()), "still no unmanaged pane: {panes:?}");
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 1 pane, 1 agent (1 skipped)"
        );
    }

    /// The authoritative map is my own window's, and only my own window's. A
    /// pane another tab dropped from ITS map is not resurrected by mine.
    #[test]
    fn my_map_overrides_only_my_own_window() {
        let mut a = app();
        restartable(&mut a);
        a.map = PaneMap::new(); // I have closed everything I opened.

        a.restart_others();

        let panes: Vec<String> = recorded().into_iter().map(|(p, _)| p).collect();
        assert_eq!(panes, vec!["%4".to_string(), "%5".to_string()], "{panes:?}");
        assert!(
            !panes.contains(&"%2".to_string()),
            "my window's stale snapshot must not outvote my own map"
        );
    }

    /// `R` is a single keypress that kills processes, and deliberately has no
    /// arm: it must not open one, borrow `Ctrl+X`'s, or leave a mode behind.
    #[test]
    fn r_needs_no_arm_and_opens_none() {
        let mut a = app();
        restartable(&mut a);
        load(&mut a, four());

        a.on_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT));

        assert!(a.stop_arm.is_none(), "`R` does not arm a delete window");
        assert!(a.pending_delete.is_none());
        assert_eq!(a.mode, Mode::Normal, "no modal, no prompt: the confirm mode is gone");
        assert!(!a.should_quit, "a restart is not a quit");
        assert!(a.arm_hint().is_none(), "the footer promises no second press");
    }

    /// A dismissal made just before `R` is still in memory AND still owed to
    /// tmux afterwards, so `main`'s flush step writes it to
    /// `@ccmux_tab_hidden` — the window option that carries it across the
    /// restart. The pane map is likewise untouched.
    #[test]
    fn a_dismissal_and_the_pane_map_survive_a_restart() {
        let mut a = app();
        restartable(&mut a);
        load(&mut a, four());
        a.map = a.tabs[0].map.clone();

        a.on_key(press('d'));
        let hidden_before = a.hidden.ids().to_vec();
        let log_before = a.hidden_log.clone();
        let map_before = a.map.clone();
        assert_eq!(hidden_before.len(), 1, "the fixture dismissed a row");

        a.on_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT));

        assert_eq!(a.hidden.ids(), hidden_before.as_slice(), "`R` un-hides nothing");
        assert_eq!(a.hidden_log, log_before, "the dismissal log is not rewritten");
        assert_eq!(a.map, map_before, "the pane map is not rewritten");
        assert!(
            a.hidden_dirty,
            "still owed to tmux, so `shutdown` writes it before the exec"
        );
    }

    /// Outside tmux there is nothing to respawn and nothing to exec into.
    /// The panicking respawn stub is the proof that no pane was touched.
    #[test]
    fn r_refuses_outside_tmux() {
        let mut a = app();
        a.degraded = true;

        let act = a.on_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT));

        assert_eq!(act, Action::Redraw);
        assert!(a.pending_restart.is_none(), "nothing to exec");
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "not inside tmux — restart unavailable"
        );
    }

    // ── `R` — the agents (§8.11) ────────────────────────────────────────────

    /// THE SAFETY TEST FOR THE AGENT PASS. A working agent is mid-task, with
    /// half-applied edits on disk and a tool call possibly in flight, and
    /// `claude stop` would take all of it. `R` must stay safe to press at any
    /// moment, so a Working row is left on its old binary and the footer says
    /// so — the trade, stated.
    #[test]
    fn a_working_agent_is_never_stopped() {
        let mut a = app();
        restartable(&mut a);
        fleet(vec![Ok(vec![
            bg("aaaaaaaa", "a", State::Working),
            bg("bbbbbbbb", "b", State::Done),
        ])]);

        a.restart_others();

        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop bbbbbbbb"],
            "a working agent must never be stopped"
        );
        // Its PANE still restarts — the attach client is disposable, and the
        // agent behind it is untouched.
        let panes: Vec<String> = recorded().into_iter().map(|(p, _)| p).collect();
        assert_eq!(panes, vec!["%2", "%4", "%5"], "{panes:?}");
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 1 agent (1 busy)"
        );
    }

    /// A blocked agent is stopped at a permission prompt WAITING ON A HUMAN.
    /// Stopping it discards the question, and the operator comes back to a
    /// halted session with no record of what it was about to ask.
    ///
    /// Both routes into `Group::Blocked` are pinned, because they disagree in
    /// the live fleet: `state: "blocked"`, and a `status: "waiting"` under a
    /// state that says nothing of the kind. A rule that read `state` alone
    /// would have stopped the second one.
    #[test]
    fn a_blocked_agent_is_never_stopped_by_either_route() {
        for blocked in [
            bg("aaaaaaaa", "a", State::Blocked),
            Session { status: Status::Waiting, ..bg("aaaaaaaa", "a", State::Working) },
            // The shape that shipped a bug once already: the CLI stops saying
            // `blocked` and the row arrives as an unmodelled state plus the
            // waiting status.
            Session {
                status: Status::Waiting,
                state: Some(State::Unknown("halted".into())),
                ..bg("aaaaaaaa", "a", State::Working)
            },
        ] {
            let mut a = app();
            restartable(&mut a);
            let label = format!("{:?}/{:?}", blocked.state, blocked.status);
            fleet(vec![Ok(vec![blocked, bg("bbbbbbbb", "b", State::Done)])]);

            a.restart_others();

            assert_eq!(
                agents::test_spawn::joined(),
                vec!["stop bbbbbbbb"],
                "a blocked agent was stopped ({label})"
            );
            assert_eq!(
                a.message.clone().unwrap_or_default_msg().0,
                "restarted 2 sidebars, 2 panes, 1 agent (1 busy)",
                "({label})"
            );
        }
    }

    /// Idle and done are what the operator asked for: a worker that is up with
    /// nothing in flight, under whichever of the two quiet headings the CLI's
    /// vocabulary lands it in.
    #[test]
    fn an_idle_or_done_agent_is_restarted() {
        for state in [
            Some(State::Done),
            // "idle": no state this build models, and nothing busy about it.
            None,
            Some(State::Unknown("napping".into())),
        ] {
            let mut a = app();
            restartable(&mut a);
            let label = format!("{state:?}");
            let mut idle = bg("aaaaaaaa", "a", State::Done);
            idle.state = state;
            idle.status = Status::Idle;
            fleet(vec![Ok(vec![idle, bg("bbbbbbbb", "b", State::Done)])]);

            a.restart_others();

            assert_eq!(
                agents::test_spawn::joined(),
                vec!["stop aaaaaaaa", "stop bbbbbbbb"],
                "an idle agent was left on the old binary ({label})"
            );
            assert_eq!(
                a.message.clone().unwrap_or_default_msg().0,
                "restarted 2 sidebars, 2 panes, 2 agents",
                "({label})"
            );
        }
    }

    /// THE REGRESSION TEST for `R` un-stopping a session the operator stopped
    /// on purpose.
    ///
    /// `Session::group` files `done` AND `stopped` under **Completed**, both
    /// correctly — they are both terminal — and the state rule used to read
    /// only the group. So a single `Ctrl+X` was undone by the next `R`: the
    /// operator's stop was reversed by a keypress that exists to upgrade
    /// binaries, on a session it should never have named. A stopped session
    /// is not running an old `claude`; it is not running.
    ///
    /// Nothing is reported about it either. `busy` means a RUNNING worker left
    /// on its old version, and a halted session is not that; the pane it sits
    /// in is what the footer accounts for, and here that pane is parked.
    #[test]
    fn a_stopped_agent_is_left_stopped_and_its_pane_is_not_re_attached() {
        let mut a = app();
        restartable(&mut a);
        // The live shape, and the one the interaction turns on: `claude stop`
        // kills the attach (PROBE-FINDINGS §2), so the pane has already fallen
        // through to its own post-attach path and latched `@ccmux_detached`.
        // `state: "stopped"` and a live `claude attach` cannot coexist.
        if let Some(p) = a.panes.iter_mut().find(|p| p.id.as_str() == "%2") {
            p.detached = true;
        }
        fleet(vec![Ok(vec![
            bg("aaaaaaaa", "a", State::Stopped),
            bg("bbbbbbbb", "b", State::Done),
        ])]);

        a.restart_others();

        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop bbbbbbbb"],
            "a session the operator stopped must stay stopped"
        );
        let panes: Vec<String> = recorded().into_iter().map(|(p, _)| p).collect();
        assert!(
            !panes.contains(&"%2".to_string()),
            "nothing may re-attach a stopped session's pane — skipping the stop \
             would achieve nothing if it did: {panes:?}"
        );
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 1 pane, 1 agent (1 skipped)",
            "a halted agent is not `busy` — it is not running anything"
        );
    }

    /// THE BELT, for the shape the pane cannot cover. If a stopped session's
    /// pane were somehow still a live attach target — a `stopped` row whose
    /// pane has not yet latched — the state rule is the only thing standing
    /// between `Ctrl+X` and a restart that undoes it. It refuses, and it
    /// refuses from `State` because `Group` puts this row beside `done`.
    #[test]
    fn a_stopped_agent_is_refused_even_when_its_pane_is_still_a_target() {
        let mut a = app();
        restartable(&mut a);
        fleet(vec![Ok(vec![
            bg("aaaaaaaa", "a", State::Stopped),
            bg("bbbbbbbb", "b", State::Done),
        ])]);

        a.restart_others();

        assert_eq!(agents::test_spawn::joined(), vec!["stop bbbbbbbb"]);
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 1 agent"
        );
    }

    /// THE SCOPE TEST, and the guard the whole headless population stands on.
    ///
    /// A DORMANT session ccmux never opened is none of `R`'s business, however
    /// idle it is: there is no worker, so there is nothing on an old binary to
    /// bring forward and "restarting" it would START it. On the operator's
    /// fleet 9 of 16 rows look exactly like `zzzzzzzz` here, so getting this
    /// wrong turns one keypress into nine background workers nobody asked for.
    #[test]
    fn a_dormant_session_ccmux_never_opened_is_never_stopped() {
        let mut a = app();
        restartable(&mut a);
        fleet(vec![Ok(vec![
            bg("aaaaaaaa", "a", State::Done),
            bg("bbbbbbbb", "b", State::Done),
            bg("zzzzzzzz", "not ours", State::Done),
            bg("yyyyyyyy", "also not ours", State::Stopped),
        ])]);

        a.restart_others();

        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop aaaaaaaa", "stop bbbbbbbb"],
            "a session with no worker must never be restarted, which would START it"
        );
        assert!(
            !order().iter().any(|e| e.starts_with("respawn-agent ")),
            "nothing may be respawned that was not running: {:?}",
            order()
        );
    }

    /// THE FEATURE. A session with a LIVE WORKER and no client used to be out
    /// of reach — the only resume `R` had was a pane's `claude attach`, so a
    /// session ccmux never opened kept the `claude` it was dispatched with for
    /// ever. `claude respawn <id>` needs no pane, so it comes back on the
    /// binary that is installed now.
    ///
    /// ONE VERB, and no `claude stop` of its own: the pair it replaced could
    /// leave a stop without its partner, and its resume had a documented fork
    /// branch that exited 0 while starting a copy of the conversation.
    #[test]
    fn a_live_session_ccmux_never_opened_is_restarted_headlessly() {
        let mut a = app();
        restartable(&mut a);
        fleet(vec![Ok(vec![
            bg("aaaaaaaa", "a", State::Done),
            bg("bbbbbbbb", "b", State::Done),
            live(bg("zzzzzzzz", "live and paneless", State::Done)),
            // Dormant, and beside a live one so the two cannot be confused.
            bg("yyyyyyyy", "dormant", State::Done),
        ])]);

        a.restart_others();

        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop aaaaaaaa", "stop bbbbbbbb", "respawn zzzzzzzz"],
            "the live paneless agent is respawned; the dormant one is not touched"
        );
        // Its pane pass is untouched: the headless session has no pane, and no
        // pane was invented for it.
        let panes: Vec<String> = recorded().into_iter().map(|(p, _)| p).collect();
        assert_eq!(panes, vec!["%2", "%4", "%5"], "{panes:?}");
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 3 agents",
            "the headless restart counts beside the paned ones, in one number"
        );
    }

    /// NO CASCADE, on the population that has no pane behind it. A respawn
    /// that failed costs THAT session and nothing else; the next one is still
    /// attempted, and so are all the panes.
    ///
    /// It is also the regression test for reading a failure as a fact about
    /// the worker. `run_bounded` SIGKILLs the client at `RESPAWN_TIMEOUT`
    /// while the daemon carries out the restart regardless, so the error alone
    /// says nothing. The poll that follows is what decides, and here it finds
    /// `yyyyyyyy` HALTED — the outcome the operator has to act on, and the one
    /// the old code filed under `not restarted`, whose documented meaning is
    /// "this agent is still running its old binary".
    #[test]
    fn a_failed_paneless_respawn_is_counted_by_what_the_poll_finds() {
        let mut a = app();
        restartable(&mut a);
        let both = || {
            Ok(vec![
                live(bg("yyyyyyyy", "live 1", State::Done)),
                live(bg("zzzzzzzz", "live 2", State::Done)),
            ])
        };
        fleet(vec![
            // 1-2: sub-pass A's re-polls. Neither paned session is in this
            // fleet, so both are `not restarted` and the log below is the
            // paneless pass alone.
            both(),
            both(),
            // 3: the enumeration. 4: the re-poll before `yyyyyyyy`.
            both(),
            both(),
            // 5: THE VERIFYING POLL after its respawn failed — the daemon
            // carried out the halt and the worker is gone.
            Ok(vec![
                Session { pid: None, ..bg("yyyyyyyy", "halted by the respawn", State::Done) },
                live(bg("zzzzzzzz", "live 2", State::Done)),
            ]),
            // 6: the re-poll before `zzzzzzzz`, which is still fine.
            both(),
        ]);
        agent_respawn_fails("yyyyyyyy");

        a.restart_others();

        assert_eq!(
            order()
                .into_iter()
                .filter(|e| e.starts_with("respawn-agent "))
                .collect::<Vec<_>>(),
            vec!["respawn-agent yyyyyyyy", "respawn-agent zzzzzzzz"],
            "one failure ended the pass instead of costing one session"
        );
        let panes: Vec<String> = recorded().into_iter().map(|(p, _)| p).collect();
        assert_eq!(panes, vec!["%2", "%4", "%5"], "the panes still restart: {panes:?}");
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 1 agent (2 not restarted, 1 left stopped)",
            "a halted agent must not be reported as one still running its old binary"
        );
    }

    /// THE OTHER SIDE OF THE SAME POLL: the worker is still there, so nothing
    /// was halted and the agent is still running whatever it was running.
    /// `not restarted` says exactly that, and the pass must not reach for
    /// `left stopped` on the strength of an error code.
    #[test]
    fn a_failed_paneless_respawn_that_halted_nothing_is_not_restarted() {
        let mut a = app();
        restartable(&mut a);
        let full = || {
            Ok(vec![
                bg("aaaaaaaa", "a", State::Done),
                bg("bbbbbbbb", "b", State::Done),
                live(bg("zzzzzzzz", "z", State::Done)),
            ])
        };
        // Polls 1-2 are sub-pass A, 3 the enumeration, 4 the re-poll, and 5 —
        // the last, so it repeats — the VERIFYING poll: the worker is untouched.
        fleet(vec![full(), full(), full(), full(), full()]);
        agent_respawn_fails("zzzzzzzz");

        a.restart_others();

        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 2 agents (1 not restarted)"
        );
    }

    /// THE STATE RULE, applied to the population that has no pane to soften
    /// it. A stopped session stays stopped here too — otherwise `R` would
    /// revive one when it happened to have a pane and not otherwise — and a
    /// working or blocked one keeps its version, from either axis.
    #[test]
    fn a_stopped_or_busy_paneless_agent_is_never_touched() {
        for (session, want_busy) in [
            (live(bg("zzzzzzzz", "stopped on purpose", State::Stopped)), false),
            (live(bg("zzzzzzzz", "mid task", State::Working)), true),
            (live(bg("zzzzzzzz", "at a prompt", State::Blocked)), true),
            (
                Session {
                    status: Status::Waiting,
                    ..live(bg("zzzzzzzz", "waiting, unmodelled state", State::Unknown("halted".into())))
                },
                true,
            ),
        ] {
            let mut a = app();
            restartable(&mut a);
            let label = format!("{:?}/{:?}", session.state, session.status);
            fleet(vec![Ok(vec![
                bg("aaaaaaaa", "a", State::Done),
                bg("bbbbbbbb", "b", State::Done),
                session,
            ])]);

            a.restart_others();

            assert_eq!(
                agents::test_spawn::joined(),
                vec!["stop aaaaaaaa", "stop bbbbbbbb"],
                "a paneless agent that is not idle was stopped ({label})"
            );
            let want = if want_busy {
                "restarted 2 sidebars, 2 panes, 2 agents (1 busy)"
            } else {
                // A halted session is not on the old binary; it is on no
                // binary. There is no exception to report about it.
                "restarted 2 sidebars, 2 panes, 2 agents"
            };
            assert_eq!(a.message.clone().unwrap_or_default_msg().0, want, "({label})");
        }
    }

    /// A worker that exits between the enumeration and its own turn is DORMANT
    /// by the time the respawn would land, and the re-poll is what catches it.
    /// `claude respawn` on a session with no worker STARTS one — the
    /// dormant-resurrection bug arriving through the race instead of through
    /// the rule.
    #[test]
    fn a_headless_target_whose_worker_exited_in_the_window_is_left_alone() {
        let mut a = app();
        restartable(&mut a);
        fleet(vec![
            // Polls 1 and 2 are the two paned candidates; poll 3 enumerates and
            // finds `zzzzzzzz` live.
            Ok(vec![
                bg("aaaaaaaa", "a", State::Done),
                bg("bbbbbbbb", "b", State::Done),
                live(bg("zzzzzzzz", "live", State::Done)),
            ]),
            Ok(vec![
                bg("aaaaaaaa", "a", State::Done),
                bg("bbbbbbbb", "b", State::Done),
                live(bg("zzzzzzzz", "live", State::Done)),
            ]),
            Ok(vec![
                bg("aaaaaaaa", "a", State::Done),
                bg("bbbbbbbb", "b", State::Done),
                live(bg("zzzzzzzz", "live", State::Done)),
            ]),
            // Poll 4 — the re-poll immediately before its stop. The worker has
            // gone in the meantime.
            Ok(vec![
                bg("aaaaaaaa", "a", State::Done),
                bg("bbbbbbbb", "b", State::Done),
                bg("zzzzzzzz", "its worker exited", State::Done),
            ]),
        ]);

        a.restart_others();

        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop aaaaaaaa", "stop bbbbbbbb"],
            "the enumeration's snapshot authorised the act instead of the re-poll"
        );
        assert!(
            !order().iter().any(|e| e.starts_with("respawn-agent ")),
            "a dormant session was respawned: {:?}",
            order()
        );
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 2 agents"
        );
    }

    /// The disjointness rule, end to end. `bbbbbbbb` is live AND held by `%5`,
    /// so it belongs to the pane population — one stop, resumed by its pane,
    /// and no `claude respawn` on top. Two acts for one restart would be a
    /// second destructive call and a double count.
    #[test]
    fn a_live_session_ccmux_has_open_is_restarted_by_its_pane_and_not_twice() {
        let mut a = app();
        restartable(&mut a);
        fleet(vec![Ok(vec![
            live(bg("aaaaaaaa", "a", State::Done)),
            live(bg("bbbbbbbb", "b", State::Done)),
        ])]);

        a.restart_others();

        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop aaaaaaaa", "stop bbbbbbbb"],
            "a session a pane holds must not also be respawned headlessly"
        );
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 2 agents"
        );
    }

    /// And the same for a pane ccmux is deliberately LEAVING ALONE. A parked
    /// pane still holds its session, so the headless pass may not adopt it:
    /// the operator parked that pane, `Enter` resumes into it, and being
    /// stopped and restarted behind their back is not what `skipped` means.
    #[test]
    fn a_parked_panes_session_is_not_adopted_by_the_headless_pass() {
        let mut a = app();
        restartable(&mut a);
        if let Some(p) = a.panes.iter_mut().find(|p| p.id.as_str() == "%2") {
            p.detached = true;
        }
        fleet(vec![Ok(vec![
            live(bg("aaaaaaaa", "parked", State::Done)),
            live(bg("bbbbbbbb", "b", State::Done)),
        ])]);

        a.restart_others();

        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop bbbbbbbb"],
            "the parked pane's agent must be left running"
        );
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 1 pane, 1 agent (1 skipped)"
        );
    }

    /// THE REGRESSION TEST for a population scoped to ccmux's own tmux session
    /// and subtracted from the whole machine's fleet.
    ///
    /// `Plan::held` is built from `list-windows`/`list-panes -s` inside
    /// `cli.session` (R2, R3). A session held by a live pane of a SECOND ccmux
    /// workspace — `--session NAME` and `-L socket` are both documented — or by
    /// a `claude attach` the operator ran by hand in any window is in no map
    /// this instance can read, so it used to read as paneless. `claude respawn`
    /// KILLS that client: the other window printed `Session <id> has exited.`
    /// and latched `@ccmux_detached`, while this footer counted the session
    /// `restarted`.
    ///
    /// The client process is what dies, so the client process is the evidence.
    #[test]
    fn a_session_attached_outside_this_ccmux_is_never_restarted_headlessly() {
        let mut a = app();
        restartable(&mut a);
        fleet(vec![Ok(vec![
            bg("aaaaaaaa", "a", State::Done),
            bg("bbbbbbbb", "b", State::Done),
            live(bg("zzzzzzzz", "open in another ccmux window", State::Done)),
        ])]);
        // No ccmux pane of THIS session names it, and there never could be one.
        attached(&["zzzzzzzz"]);

        a.restart_others();

        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop aaaaaaaa", "stop bbbbbbbb"],
            "another window's live attach was about to be killed"
        );
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 2 agents",
            "and it is not counted: this pass never had any business with it"
        );
    }

    /// THE REGRESSION TEST for reading "the scan found nobody" out of "the scan
    /// could not run".
    ///
    /// The attach evidence is the only thing standing between this pass and
    /// every attach client on the machine, so an unavailable scan must stop
    /// the paneless pass dead rather than authorise it. The pane pass is
    /// untouched — that is what `R` was always for — and the footer says which
    /// half did not happen, instead of a silence that reads as "there were
    /// none".
    #[test]
    fn an_attach_scan_that_cannot_run_stops_the_paneless_pass() {
        let mut a = app();
        restartable(&mut a);
        fleet(vec![Ok(vec![
            bg("aaaaaaaa", "a", State::Done),
            bg("bbbbbbbb", "b", State::Done),
            live(bg("zzzzzzzz", "live and paneless", State::Done)),
        ])]);
        attach_scan_fails();

        a.restart_others();

        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop aaaaaaaa", "stop bbbbbbbb"],
            "absent evidence authorised the act"
        );
        let panes: Vec<String> = recorded().into_iter().map(|(p, _)| p).collect();
        assert_eq!(panes, vec!["%2", "%4", "%5"], "the pane pass is untouched: {panes:?}");
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 2 agents (paneless pass skipped)"
        );
    }

    /// THE REGRESSION TEST for the fleet's fork branch, at the call site.
    ///
    /// `claude --bg --resume <sessionId>` exits 0 while starting a COPY of the
    /// conversation when the daemon still calls the session running — which is
    /// precisely the state ccmux left it in by resuming immediately after its
    /// own stop. Every occurrence was invisible: an extra live agent, an extra
    /// row in the fleet, counted as a success. `claude respawn` has no such
    /// branch, and no argv this pass builds may reintroduce one.
    #[test]
    fn the_agent_pass_builds_only_stop_and_respawn_argvs() {
        let mut a = app();
        restartable(&mut a);
        fleet(vec![Ok(vec![
            live(bg("aaaaaaaa", "a", State::Done)),
            live(bg("bbbbbbbb", "b", State::Done)),
            live(bg("zzzzzzzz", "live and paneless", State::Done)),
        ])]);

        a.restart_others();

        let argv = agents::test_spawn::calls();
        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop aaaaaaaa", "stop bbbbbbbb", "respawn zzzzzzzz"]
        );
        for c in &argv {
            assert!(
                !c.iter().any(|f| f == "--resume" || f == "--bg" || f == "--all"),
                "a forking or fleet-wide argv came back: {c:?}"
            );
        }
    }

    /// A pane parked after `Ctrl+Z` is the operator's shell now, so `R` does
    /// not respawn it — and therefore nothing would re-attach its session.
    /// Stopping it would leave that agent halted with no resume, which is a
    /// stop wearing a restart's name. Out of scope, and its pane-level skip is
    /// already reported.
    #[test]
    fn a_pane_parked_after_ctrl_z_leaves_its_agent_alone() {
        let mut a = app();
        restartable(&mut a);
        if let Some(p) = a.panes.iter_mut().find(|p| p.id.as_str() == "%2") {
            p.detached = true;
        }

        a.restart_others();

        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop bbbbbbbb"],
            "the parked pane's agent must be left running"
        );
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 1 pane, 1 agent (1 skipped)"
        );
    }

    /// THE REGRESSION TEST for absent evidence authorising an act.
    ///
    /// `refresh_panes` keeps the previous inventory when a tmux read fails —
    /// right for every drawing reader — but the process that runs this pass is
    /// a FRESH image whose previous inventory is EMPTY. So a failed
    /// `list-panes` or `list-windows` left `plan` with no targets AND no
    /// `held`, and the paneless pass then read the whole machine's fleet as
    /// nobody's: it halted and restarted the very sessions this window had
    /// open in panes, respawned none of them, and reported success. Renaming
    /// the tmux session out from under a running ccmux is enough to produce it.
    ///
    /// `plan` used missing evidence to REFUSE to act and the paneless pass
    /// used it to AUTHORISE acting; the asymmetry is the bug. Now neither
    /// runs, and the operator is told the reads failed.
    #[test]
    fn a_tmux_read_that_failed_stops_the_whole_pass_rather_than_widening_it() {
        for (why, panes_fresh, tabs_fresh) in [
            ("list-panes failed", false, true),
            ("list-windows failed", true, false),
            ("both failed", false, false),
        ] {
            let mut a = app();
            restartable(&mut a);
            fleet(vec![Ok(vec![
                live(bg("aaaaaaaa", "open in %2 — this window", State::Done)),
                live(bg("bbbbbbbb", "open in %5 — the other tab", State::Done)),
            ])]);
            // What the fresh post-`exec` image is left holding when the read
            // fails: nothing, and a flag that says so.
            a.panes_fresh = panes_fresh;
            a.tabs_fresh = tabs_fresh;
            if !panes_fresh {
                a.panes.clear();
            }
            if !tabs_fresh {
                a.tabs.clear();
            }

            a.restart_others();

            assert!(
                agents::test_spawn::calls().is_empty(),
                "an agent was touched on no evidence at all ({why}): {:?}",
                agents::test_spawn::joined()
            );
            assert!(
                recorded().is_empty(),
                "a pane was respawned from a blank inventory ({why}): {:?}",
                recorded()
            );
            assert_eq!(
                a.message.clone().unwrap_or_default_msg().0,
                "cannot read this session's panes — restarted this sidebar only",
                "({why})"
            );
        }
    }

    /// THE ORDERING PROPERTY, on one timeline. `claude attach` is what resumes
    /// a stopped session, so a stop that landed after its pane had already
    /// re-attached would leave the operator looking at a halted session with
    /// nothing to restart it. Every stop goes first — not merely each stop
    /// before its own pane, because two panes may hold one session.
    #[test]
    fn every_stop_lands_before_every_respawn() {
        let mut a = app();
        restartable(&mut a);

        a.restart_others();

        let log = order();
        let last_stop = log.iter().rposition(|e| e.starts_with("stop "));
        let first_respawn = log.iter().position(|e| e.starts_with("respawn "));
        assert!(last_stop.is_some() && first_respawn.is_some(), "{log:?}");
        assert!(
            last_stop < first_respawn,
            "a stop landed after a pane had already re-attached: {log:?}"
        );
        // And specifically: the session in `%2` is stopped before `%2` is
        // respawned with the `claude attach` that resumes it.
        let stop_a = log.iter().position(|e| e == "stop aaaaaaaa");
        let respawn_2 = log.iter().position(|e| e == "respawn %2");
        assert!(stop_a < respawn_2, "{log:?}");
    }

    /// THE SAME PROPERTY WITH A HEADLESS AGENT IN THE PASS — which is where it
    /// could have been lost, since those sessions touch no pane and so have no
    /// ordering constraint of their own to keep them honest.
    ///
    /// The whole of PHASE 1 stays before the whole of PHASE 2: every halt,
    /// paned and paneless, then every pane respawn. The paned stops go FIRST
    /// within phase 1, because their resume is phase 2 and every second spent
    /// elsewhere is a second longer they sit halted; the paneless restarts
    /// follow, each one whole in a single call.
    #[test]
    fn every_stop_lands_before_every_respawn_with_a_headless_agent_too() {
        let mut a = app();
        restartable(&mut a);
        fleet(vec![Ok(vec![
            bg("aaaaaaaa", "a", State::Done),
            bg("bbbbbbbb", "b", State::Done),
            live(bg("zzzzzzzz", "live and paneless", State::Done)),
        ])]);

        a.restart_others();

        let log: Vec<String> = order().into_iter().filter(|e| e != "poll").collect();
        assert_eq!(
            log,
            vec![
                "stop aaaaaaaa",
                "stop bbbbbbbb",
                "respawn-agent zzzzzzzz",
                "respawn %2",
                "respawn %4",
                "respawn %5",
            ],
            "the phase boundary moved: {log:?}"
        );
        // A paneless respawn is a stop-and-start inside the daemon, so it
        // belongs on the same side of the boundary as every other halt.
        let last_agent = log.iter().rposition(|e| {
            e.starts_with("stop ") || e.starts_with("respawn-agent ")
        });
        let first_respawn = log.iter().position(|e| e.starts_with("respawn %"));
        assert!(
            last_agent < first_respawn,
            "an agent was halted after a pane had already re-attached: {log:?}"
        );
    }

    /// THE RACE NARROWER. `App::sessions` is up to a poll interval old — and in
    /// the process that runs this pass it is empty — so the state is re-read
    /// immediately before each stop. A single snapshot taken at the top would
    /// age by one `claude stop` per session ahead of it, and this fleet is the
    /// consequence: `bbbbbbbb` is idle when the pass starts and working by the
    /// time its own turn comes. It must not be stopped.
    #[test]
    fn the_state_is_re_read_immediately_before_every_stop() {
        let mut a = app();
        restartable(&mut a);
        fleet(vec![
            // Poll 1, for `aaaaaaaa`: both idle.
            Ok(vec![
                bg("aaaaaaaa", "a", State::Done),
                bg("bbbbbbbb", "b", State::Done),
            ]),
            // Poll 2, for `bbbbbbbb`: it picked up work in between.
            Ok(vec![
                bg("aaaaaaaa", "a", State::Stopped),
                bg("bbbbbbbb", "b", State::Working),
            ]),
        ]);

        a.restart_others();

        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop aaaaaaaa"],
            "the second stop was authorised by a snapshot the first stop had aged"
        );
        // ONE READ PER STOP, not one read per pass — asserted as the property
        // rather than as a total, so it stays true of every population: EVERY
        // stop, paned or headless, is immediately preceded by a poll.
        let log = order();
        for (i, e) in log.iter().enumerate() {
            if e.starts_with("stop ") {
                assert_eq!(
                    log.get(i.wrapping_sub(1)).map(String::as_str),
                    Some("poll"),
                    "a stop was authorised by a read that was not the last one: {log:?}"
                );
            }
        }
        assert_eq!(
            log.iter().filter(|e| *e == "poll").count(),
            3,
            "two re-polls for the two paned candidates, plus one that \
             enumerates the headless population — which has no tmux record to \
             be read from: {log:?}"
        );
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 1 agent (1 busy)"
        );
    }

    /// NO CASCADE. One `claude stop` failing — a wedged daemon, a session the
    /// CLI refuses — must cost that agent its upgrade and nothing else: the
    /// remaining agents are still restarted, every pane is still respawned, and
    /// the operator is told a number rather than left to notice.
    #[test]
    fn a_failed_stop_does_not_cascade_and_is_reported() {
        let mut a = app();
        restartable(&mut a);
        agents::test_spawn::fail_next("daemon is not running");

        a.restart_others();

        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop aaaaaaaa", "stop bbbbbbbb"],
            "the failure stopped the pass instead of costing one agent"
        );
        let panes: Vec<String> = recorded().into_iter().map(|(p, _)| p).collect();
        assert_eq!(panes, vec!["%2", "%4", "%5"], "the panes must still restart: {panes:?}");
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 1 agent (1 not restarted)"
        );
    }

    /// A poll that fails is not one session's problem: without it ccmux cannot
    /// tell working from idle, and every stop after it would be a guess. The
    /// pass ends, the rest are counted as not restarted, and — the part that
    /// matters — the pane pass still runs, because that is the job `R` had
    /// before agents were part of it.
    #[test]
    fn a_poll_that_fails_ends_the_agent_pass_and_the_panes_still_restart() {
        let mut a = app();
        restartable(&mut a);
        fleet(vec![Err(())]);

        a.restart_others();

        assert!(
            agents::test_spawn::joined().is_empty(),
            "nothing may be stopped on a state that could not be read: {:?}",
            agents::test_spawn::joined()
        );
        assert_eq!(
            order().iter().filter(|e| *e == "poll").count(),
            1,
            "a CLI that just failed is not asked again for every remaining agent"
        );
        let panes: Vec<String> = recorded().into_iter().map(|(p, _)| p).collect();
        assert_eq!(panes, vec!["%2", "%4", "%5"], "{panes:?}");
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 0 agents (2 not restarted)"
        );
    }

    /// A session ccmux has open but that the fleet no longer lists cannot be
    /// shown to be idle, so it is not stopped — and, unlike a poll failure,
    /// that says nothing about the NEXT session, so the pass continues.
    #[test]
    fn a_session_that_left_the_fleet_is_not_stopped_and_does_not_end_the_pass() {
        let mut a = app();
        restartable(&mut a);
        fleet(vec![Ok(vec![bg("bbbbbbbb", "b", State::Done)])]);

        a.restart_others();

        assert_eq!(agents::test_spawn::joined(), vec!["stop bbbbbbbb"]);
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 1 agent (1 not restarted)"
        );
    }

    /// The whole-pass budget. Per-call bounds multiply — seven agents against a
    /// wedged `claude` is minutes of a sidebar that has not drawn its first
    /// frame — so the pass has a ceiling of its own, and crossing it must never
    /// cost the panes their respawn.
    #[test]
    fn an_exhausted_budget_ends_the_agent_pass_and_the_panes_still_restart() {
        let mut a = app();
        restartable(&mut a);
        a.agent_budget = Duration::ZERO;

        a.restart_others();

        assert!(
            !order().iter().any(|e| e == "poll" || e.starts_with("stop ")),
            "an exhausted budget must not even read the fleet: {:?}",
            order()
        );
        let panes: Vec<String> = recorded().into_iter().map(|(p, _)| p).collect();
        assert_eq!(panes, vec!["%2", "%4", "%5"], "{panes:?}");
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 0 agents (2 not restarted)"
        );
    }

    /// ONE BUDGET FOR THE WHOLE PASS, and the headless half is inside it.
    ///
    /// The half that has no pane behind it is the one an unbounded pass would
    /// hurt most — a stop with its resume still to come is a halted session —
    /// so the deadline is checked before the enumeration and again before every
    /// pair. An exhausted budget therefore stops and resumes NOTHING, and it
    /// still costs the panes nothing.
    #[test]
    fn an_exhausted_budget_reaches_no_headless_agent_either() {
        let mut a = app();
        restartable(&mut a);
        a.agent_budget = Duration::ZERO;
        fleet(vec![Ok(vec![live(bg("zzzzzzzz", "live and paneless", State::Done))])]);

        a.restart_others();

        assert!(
            order().iter().all(|e| e.starts_with("respawn ")),
            "an exhausted budget reached the fleet: {:?}",
            order()
        );
        assert!(
            agents::test_spawn::joined().is_empty(),
            "nothing may be stopped, and above all nothing resumed, past the deadline"
        );
        let panes: Vec<String> = recorded().into_iter().map(|(p, _)| p).collect();
        assert_eq!(panes, vec!["%2", "%4", "%5"], "{panes:?}");
    }

    /// THE REGRESSION TEST for a footer that reported an upgrade for a worker
    /// that was not running.
    ///
    /// `restarted` used to be incremented the moment `claude stop` returned Ok,
    /// which is HALF the operation. The resume is the pane's respawned
    /// `claude attach`, and it can fail on its own — kill that pane in the
    /// seconds between the phases (reproduced live with `tmux kill-pane`) and
    /// the agent is stopped with nothing left to bring it back, while the
    /// footer counted it among the successes and no word in `note` covered the
    /// state it was actually in.
    ///
    /// Now the tally waits for PHASE 2: the stop that got its pane back is
    /// `restarted`, the one that did not is `left stopped` — its own word,
    /// because it is the only outcome of `R` that asks the operator to act.
    #[test]
    fn an_agent_whose_pane_never_came_back_is_reported_as_left_stopped() {
        let mut a = app();
        restartable(&mut a);
        // `%2` holds `aaaaaaaa`; it is gone by the time PHASE 2 reaches it.
        respawn_fails("%2");

        a.restart_others();

        // BOTH were stopped — the agent pass is unchanged and unaware.
        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop aaaaaaaa", "stop bbbbbbbb"],
            "the agent pass must not be steered by a pane failure that happens later"
        );
        // But only `bbbbbbbb` was resumed, and only it is claimed.
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 2 panes, 1 agent (1 failed, 1 left stopped)"
        );
    }

    /// The same rule from the other side: one session in two panes is resumed
    /// if EITHER pane comes back. Double-attach is legal (PROBE-FINDINGS §3),
    /// so one surviving `claude attach` is one running worker, and calling that
    /// `left stopped` would send the operator to fix a session that is fine.
    #[test]
    fn one_surviving_pane_is_enough_to_resume_a_doubly_attached_session() {
        let mut a = app();
        restartable(&mut a);
        // `%6` in my window holds the same session as `%5` in the other tab.
        a.panes.push(pane("%6", 1, 3, 116, 40, false));
        a.map.insert(
            &PaneId::parse("%6").expect("pane id"),
            PaneEntry {
                session_id: "sid-b".into(),
                short_id: "bbbbbbbb".into(),
                name: "b".into(),
                opened_at: 0,
            },
        );
        respawn_fails("%5");

        a.restart_others();

        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 3 panes, 2 agents (1 failed)",
            "a session whose second pane came back is running, not stranded"
        );
    }

    /// Double-attach is legal, so one session can sit in two panes. It is
    /// stopped ONCE — a second `claude stop` for one restart is a second
    /// destructive call, and it would be counted twice in the footer — while
    /// both panes still re-attach.
    #[test]
    fn a_session_open_in_two_panes_is_stopped_once() {
        let mut a = app();
        restartable(&mut a);
        // `%6` in my window holds the same session as `%5` in the other tab.
        a.panes.push(pane("%6", 1, 3, 116, 40, false));
        a.map.insert(
            &PaneId::parse("%6").expect("pane id"),
            PaneEntry {
                session_id: "sid-b".into(),
                short_id: "bbbbbbbb".into(),
                name: "b".into(),
                opened_at: 0,
            },
        );

        a.restart_others();

        assert_eq!(
            agents::test_spawn::joined(),
            vec!["stop aaaaaaaa", "stop bbbbbbbb"],
            "one session, one stop"
        );
        let panes: Vec<String> = recorded().into_iter().map(|(p, _)| p).collect();
        assert_eq!(panes, vec!["%2", "%6", "%4", "%5"], "both panes re-attach: {panes:?}");
        assert_eq!(
            a.message.clone().unwrap_or_default_msg().0,
            "restarted 2 sidebars, 3 panes, 2 agents"
        );
    }

    /// `Ctrl-R` is not this binding. It falls to the `_ if ctrl` catch-all
    /// above the char arms and must stay completely inert.
    #[test]
    fn ctrl_r_is_not_the_restart_binding() {
        let mut a = app();
        restartable(&mut a);

        let act = a.on_key(ctrl('r'));

        assert_eq!(act, Action::None);
        assert!(recorded().is_empty(), "Ctrl-R respawns nothing");
        assert!(a.pending_restart.is_none());
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
