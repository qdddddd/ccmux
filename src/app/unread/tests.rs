//! The unread mark: detection, the stamp rule across tabs, and the seen rule.
//! Hermetic: no own pane, so nothing is ever flushed to tmux.
use super::*;
use crate::app::tests::app;
use crate::model::{CodexMeta, Kind};
use crate::tmux::{PaneEntry, TabInfo};

fn claude(id: &str, state: Option<State>, status: Status) -> Session {
    Session {
        provider: Provider::Claude, codex: None, id: Some(id[..8].to_string()), pid: None,
        session_id: id.into(), cwd: "/w".into(), kind: Kind::Background, started_at: 0,
        name: id.into(), status, state,
    }
}

fn busy(id: &str) -> Session { claude(id, Some(State::Working), Status::Busy) }
fn done(id: &str) -> Session { claude(id, Some(State::Done), Status::Idle) }

fn codex(runtime: CodexStatus, state: Option<State>, status: Status) -> Session {
    Session {
        provider: Provider::Codex, codex: Some(CodexMeta { runtime, updated_at: 0 }),
        ..claude("01a00000-0000-7000-8000-000000000001", state, status)
    }
}

fn poll(a: &mut App, now: i64, rows: Vec<Session>) {
    a.now_ms = now;
    a.apply_poll(Ok(model::Payload { sessions: rows, dropped: 0 }));
}

fn window(n: u32) -> WindowId { WindowId::parse(&format!("@{n}")).unwrap() }

fn tab(n: u32) -> TabInfo {
    TabInfo { window: window(n), index: n, sidebar: None, map: PaneMap::new(),
        hidden: HiddenLog::new(), unread: Default::default() }
}

/// A sidebar in tab `n` of a two-tab session (tabs 1 and 2).
fn sidebar(n: u32) -> App {
    let mut a = app();
    a.own_window = Some(window(n));
    a.tabs = vec![tab(1), tab(2)];
    a.panes_fresh = true;
    a
}

/// What a tick's `list-windows` does between two sidebars: each reads the
/// other's stored fragment, then refolds (which also prunes its own).
fn sync(a: &mut App, b: &mut App) {
    let (na, nb) = (a.own_org() as usize, b.own_org() as usize);
    a.tabs[nb - 1].unread = b.unread.log.clone();
    b.tabs[na - 1].unread = a.unread.log.clone();
    a.refold_unread();
    b.refold_unread();
}

/// `id`'s pane, in tab 1's map and on screen (or not).
fn show(a: &mut App, id: &str, on_screen: bool) {
    let pane = PaneId::parse("%9").unwrap();
    a.map.insert(&pane, PaneEntry { provider: Provider::Claude, session_id: id.into(),
        short_id: id[..8].into(), name: "n".into(), opened_at: 0 });
    a.panes = vec![PaneInfo {
        id: pane, index: 2, left: 35, top: 0, width: 60, height: 40, active: false,
        window_index: 1, window_id: window(1), window_active: on_screen, session_clients: 1,
        window_viewers: Some(u32::from(on_screen)), window_zoomed: false, detached: false, shell: false, jump_from: None,
    }];
}

const X: &str = "aaaaaaaa-0000-0000-0000-000000000001";
const Y: &str = "bbbbbbbb-0000-0000-0000-000000000002";

/// Only a turn this process SAW running, and then saw end on its own, marks
/// the row — stamped with the last-running ms. Every other transition, and
/// every non-observation, leaves no mark.
#[test]
fn only_an_observed_running_to_finished_transition_marks_the_row() {
    use Turn::*;
    let unknown = || Status::Unknown("pondering".into());
    let cases = [
        (Some(State::Working), Status::Busy, Running),
        (None, Status::Busy, Running),
        (Some(State::Unknown("new".into())), Status::Busy, Running),
        (Some(State::Working), Status::Idle, Finished), // group Working, but between turns
        (None, Status::Idle, Finished),
        (Some(State::Done), Status::Unknown(String::new()), Finished),
        (Some(State::Done), Status::Busy, Finished), // a stale status on a terminal state
        (Some(State::Stopped), Status::Idle, Other),
        (Some(State::Blocked), Status::Idle, Other),
        (Some(State::Working), Status::Waiting, Other),
        (None, unknown(), Other),
    ];
    for (state, status, want) in cases {
        assert_eq!(turn_of(&claude(X, state.clone(), status.clone())), want, "{state:?}/{status:?}");
    }
    let active = |flags: &[&str]| CodexStatus::Active { flags: flags.iter().map(|f| f.to_string()).collect() };
    for (runtime, state, status, want) in [
        (active(&[]), Some(State::Working), Status::Busy, Running),
        (active(&["somethingNew"]), Some(State::Working), unknown(), Running),
        (active(&["waitingOnApproval"]), Some(State::Blocked), Status::Waiting, Other),
        (CodexStatus::Idle, None, Status::Idle, Finished),
        (CodexStatus::NotLoaded, Some(State::Unloaded), Status::Idle, Finished),
        (CodexStatus::SystemError, None, Status::Unknown("systemError".into()), Finished),
        (CodexStatus::Unknown("x".into()), None, unknown(), Other),
    ] {
        assert_eq!(turn_of(&codex(runtime.clone(), state, status)), want, "{runtime:?}");
    }

    let mut a = app();
    // Startup: a finished row with no running observation behind it.
    poll(&mut a, 1_000, vec![done(X), busy(Y)]);
    assert!(a.unread.set.is_empty(), "no mark is invented at startup");
    // A failed poll and a poll that does not list Y leave Y's stamp alone.
    a.now_ms = 2_000;
    a.apply_poll(Err(AgentsError::NotFound("x".into())));
    poll(&mut a, 3_000, vec![done(X)]);
    assert_eq!(a.unread.last_running.get(Y), Some(&(Provider::Claude, 1_000)));
    poll(&mut a, 4_000, vec![done(X), done(Y)]);
    assert!(a.is_unread(Provider::Claude, Y));
    let add = a.unread.log.ops.iter().find(|o| o.id == Y).unwrap();
    assert_eq!((add.add, add.seq), (true, 1_000), "stamped with the last-running ms, not now");
    assert!(a.unread.last_running.is_empty(), "the stamp is spent");
    // Running -> stopped / blocked: the turn ends without a mark.
    for end in [claude(X, Some(State::Stopped), Status::Idle), claude(X, Some(State::Blocked), Status::Waiting)] {
        poll(&mut a, 5_000, vec![busy(X), done(Y)]);
        poll(&mut a, 6_000, vec![end, done(Y)]);
        assert!(!a.is_unread(Provider::Claude, X));
        assert!(a.unread.last_running.is_empty());
    }
}

/// The stamp rule across tabs: tab B last saw X running BEFORE the operator
/// looked, and comes back on screen afterwards. Its late add must lose to the
/// seen already in the log — through real refolds, which prune, in between.
#[test]
fn a_stale_sidebar_cannot_re_mark_a_row_already_read() {
    let (mut a, mut b) = (sidebar(1), sidebar(2));
    poll(&mut b, 1_000, vec![busy(X)]); // t0: B's last look, then B leaves the screen
    poll(&mut a, 2_000, vec![busy(X)]);
    poll(&mut a, 3_000, vec![done(X)]); // A mints add@t1
    sync(&mut a, &mut b);
    assert!(a.is_unread(Provider::Claude, X) && b.is_unread(Provider::Claude, X));

    show(&mut a, X, true);
    a.now_ms = 4_000;
    a.tick_unread_seen(); // seen@t3
    for _ in 0..3 {
        sync(&mut a, &mut b);
    }
    assert!(!a.is_unread(Provider::Claude, X) && !b.is_unread(Provider::Claude, X));
    assert!(a.unread.log.ops.iter().any(|o| o.id == X && !o.add), "the tombstone is kept, not GC'd");

    poll(&mut b, 5_000, vec![done(X)]); // B back on screen, stale stamp t0
    sync(&mut a, &mut b);
    assert!(!a.is_unread(Provider::Claude, X) && !b.is_unread(Provider::Claude, X), "stays READ");
}

/// A turn the operator watched end leaves no mark AND a watermark: the same
/// stale sidebar must not mark it afterwards either.
#[test]
fn a_turn_watched_to_its_end_is_never_marked_by_another_tab() {
    let (mut a, mut b) = (sidebar(1), sidebar(2));
    poll(&mut b, 1_000, vec![busy(X)]);
    show(&mut a, X, true);
    poll(&mut a, 2_000, vec![busy(X)]);
    poll(&mut a, 3_000, vec![done(X)]);
    sync(&mut a, &mut b);
    assert!(!a.is_unread(Provider::Claude, X));
    poll(&mut b, 5_000, vec![done(X)]);
    sync(&mut a, &mut b);
    assert!(!a.is_unread(Provider::Claude, X) && !b.is_unread(Provider::Claude, X));

    // B never saw this turn running, but the operator reads it there; A's
    // older stamp must not mark it afterwards.
    show(&mut a, X, false);
    poll(&mut a, 5_500, vec![busy(X)]);
    show(&mut b, X, true);
    poll(&mut b, 5_600, vec![done(X)]);
    let stamp = b.unread.log.max_seq();
    poll(&mut b, 5_700, vec![done(X)]);
    assert_eq!(b.unread.log.max_seq(), stamp, "one seen per look, not one per poll");
    sync(&mut a, &mut b);
    poll(&mut a, 5_800, vec![done(X)]);
    sync(&mut a, &mut b);
    assert!(!a.is_unread(Provider::Claude, X) && !b.is_unread(Provider::Claude, X));

    // Off screen, the same transition marks.
    poll(&mut a, 6_000, vec![busy(X)]);
    poll(&mut a, 7_000, vec![done(X)]);
    assert!(a.is_unread(Provider::Claude, X));

    // A look that begins and ends between two of A's polls, at a row A last
    // polled finished, still leaves a watermark for B's older stamp to lose to.
    let (mut c, mut d) = (sidebar(1), sidebar(2));
    poll(&mut d, 9_000, vec![busy(X)]); // D: stamp t0, then off screen
    poll(&mut c, 9_500, vec![done(X)]); // C: snapshot finished, X off screen
    show(&mut c, X, true);
    c.now_ms = 10_000;
    c.tick_unread_seen(); // the look starts — no poll in it
    show(&mut c, X, false);
    c.tick_unread_seen();
    sync(&mut c, &mut d);
    poll(&mut d, 11_000, vec![done(X)]); // D back on screen with t0
    sync(&mut c, &mut d);
    assert!(!c.is_unread(Provider::Claude, X) && !d.is_unread(Provider::Claude, X));
}

/// Viewed = a live mapped pane, in ANY tab's map, in a rendered window — and
/// in a zoomed window only the zoomed pane. A detached (parked) pane does not
/// count: it shows the resume prompt or a shell, not the turn.
#[test]
fn the_seen_rule_reads_the_inventory_including_zoom_and_detached_panes() {
    let base = PaneInfo {
        id: PaneId::parse("%9").unwrap(), index: 2, left: 35, top: 0, width: 60, height: 40,
        active: false, window_index: 2, window_id: window(2), window_active: false, session_clients: 1,
        window_viewers: Some(1), window_zoomed: false, detached: false, shell: false, jump_from: None,
    };
    assert!(pane_on_screen(&base));
    assert!(!pane_on_screen(&PaneInfo { window_viewers: Some(0), ..base.clone() }));
    assert!(!pane_on_screen(&PaneInfo { window_viewers: None, ..base.clone() }), "old tmux: not current");
    assert!(pane_on_screen(&PaneInfo { window_viewers: None, window_active: true, ..base.clone() }));
    assert!(!pane_on_screen(&PaneInfo { window_zoomed: true, ..base.clone() }), "hidden by a zoom");
    assert!(pane_on_screen(&PaneInfo { window_zoomed: true, active: true, ..base.clone() }));

    // Tab 2's map holds X's pane. Parked with `Ctrl+Z` and on screen while X
    // runs on: its end is still marked, and the parked pane does not read it.
    let mut a = sidebar(1);
    a.tabs[1].map.insert(&base.id, PaneEntry { provider: Provider::Claude, session_id: X.into(),
        short_id: X[..8].into(), name: "n".into(), opened_at: 0 });
    a.panes = vec![PaneInfo { detached: true, ..base.clone() }];
    poll(&mut a, 1_000, vec![busy(X), busy(Y)]);
    poll(&mut a, 2_000, vec![done(X), done(Y)]);
    assert!(a.is_unread(Provider::Claude, X) && a.is_unread(Provider::Claude, Y));
    a.tick_unread_seen();
    assert!(a.is_unread(Provider::Claude, X), "a parked pane shows no turn");
    let seen = |a: &App| a.unread.log.ops.iter().filter(|o| !o.add).count();
    a.panes_fresh = false;
    a.panes = vec![base.clone()];
    a.tick_unread_seen();
    assert!(a.is_unread(Provider::Claude, X), "a stale inventory is no evidence of a look");
    a.panes_fresh = true;
    a.tick_unread_seen();
    assert!(!a.is_unread(Provider::Claude, X), "seen through another tab's resumed pane");
    assert!(a.is_unread(Provider::Claude, Y), "Y has no pane");
    assert_eq!(seen(&a), 1, "one seen, for X only");
    a.tick_unread_seen();
    assert_eq!(seen(&a), 1, "nothing minted for a row already read");
}

/// Degraded (no tmux): the set lives in memory, and Enter — refused there —
/// is still the look that clears the selected row's mark.
#[test]
fn enter_clears_the_mark_even_when_degraded() {
    let mut a = app();
    poll(&mut a, 1_000, vec![busy(X)]);
    poll(&mut a, 2_000, vec![done(X)]);
    a.rebuild_rows();
    a.select_first();
    a.degraded = true;
    assert!(a.is_unread(Provider::Claude, X));
    a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!a.is_unread(Provider::Claude, X));
}

/// `/clear` re-keys the session: its mark (with its original stamp) and its
/// running stamp follow the agent to the new id; absence then retires the old
/// id after two complete polls.
#[test]
fn clear_carries_the_mark_and_absence_retires_the_old_id() {
    let mut a = app();
    let cleared = |id: &str, state: Option<State>, status: Status| {
        let mut s = claude(X, state, status);
        s.session_id = id.into();
        s
    };
    poll(&mut a, 1_000, vec![busy(X)]);
    poll(&mut a, 2_000, vec![done(X)]);
    poll(&mut a, 3_000, vec![cleared(Y, Some(State::Done), Status::Idle)]);
    assert!(a.is_unread(Provider::Claude, Y), "the mark follows the agent");
    assert!(a.unread.log.ops.iter().any(|o| o.id == Y && o.add && o.seq == 1_000));
    poll(&mut a, 4_000, vec![cleared(Y, Some(State::Done), Status::Idle)]);
    assert!(!a.unread.log.ops.iter().any(|o| o.id == X), "two complete polls retire the old id");

    // `/clear` mid-turn: the running stamp moves too, and the new id's end
    // is marked with it.
    poll(&mut a, 5_000, vec![cleared(Y, Some(State::Working), Status::Busy)]);
    poll(&mut a, 6_000, vec![cleared(X, Some(State::Done), Status::Idle)]);
    assert!(a.unread.log.ops.iter().any(|o| o.id == X && o.add && o.seq == 5_000));
}

/// A look at a marked row that is RUNNING AGAIN reads the earlier turn's
/// mark, not this one: the turn in progress is still marked when it ends off
/// screen, whichever tab minted the look's seen. A tab whose snapshot is not
/// trusted raises nothing, so its old running stamp still cannot re-mark a
/// row already read.
#[test]
fn a_look_at_a_running_row_does_not_swallow_the_end_of_its_turn() {
    // This sidebar mints the seen.
    let mut a = app();
    poll(&mut a, 1_000, vec![busy(X)]);
    poll(&mut a, 2_000, vec![done(X)]); // add@1000
    poll(&mut a, 3_000, vec![busy(X)]); // running again
    show(&mut a, X, true);
    a.now_ms = 4_000;
    a.tick_unread_seen(); // seen@4000 reads the earlier turn
    assert!(!a.is_unread(Provider::Claude, X));
    show(&mut a, X, false);
    poll(&mut a, 5_000, vec![done(X)]);
    assert!(a.is_unread(Provider::Claude, X), "the turn running during the look is marked");

    // The same through `Enter`, with no pane coming on screen.
    let mut a = app();
    poll(&mut a, 1_000, vec![busy(X)]);
    poll(&mut a, 2_000, vec![done(X)]);
    poll(&mut a, 3_000, vec![busy(X)]);
    a.rebuild_rows();
    a.select_first();
    a.degraded = true;
    a.now_ms = 4_000;
    a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!a.is_unread(Provider::Claude, X));
    poll(&mut a, 5_000, vec![done(X)]);
    assert!(a.is_unread(Provider::Claude, X), "Enter read the earlier turn only");

    // Another tab's sidebar mints the look's seen first.
    let (mut a, mut b) = (sidebar(1), sidebar(2));
    poll(&mut a, 1_000, vec![busy(X)]);
    poll(&mut a, 2_000, vec![done(X)]);
    sync(&mut a, &mut b);
    poll(&mut a, 3_000, vec![busy(X)]);
    b.quiesced = true;
    show(&mut b, X, true);
    b.now_ms = 4_000;
    b.tick_unread_seen();
    sync(&mut a, &mut b);
    show(&mut a, X, true);
    a.now_ms = 4_100;
    a.tick_unread_seen(); // nothing left to mint; the look still counts
    show(&mut a, X, false);
    poll(&mut a, 5_000, vec![done(X)]);
    sync(&mut a, &mut b);
    assert!(a.is_unread(Provider::Claude, X) && b.is_unread(Provider::Claude, X));

    // An off-screen sidebar holding an older running stamp mints the look's
    // seen itself: it must not lift its own stamp over it.
    let (mut a, mut b) = (sidebar(1), sidebar(2));
    poll(&mut b, 1_000, vec![busy(X)]);
    b.quiesced = true;
    poll(&mut a, 2_000, vec![busy(X)]);
    poll(&mut a, 3_000, vec![done(X)]);
    sync(&mut a, &mut b);
    show(&mut b, X, true);
    b.now_ms = 4_000;
    b.tick_unread_seen();
    show(&mut b, X, false);
    sync(&mut a, &mut b);
    b.quiesced = false;
    poll(&mut b, 5_000, vec![done(X)]);
    sync(&mut a, &mut b);
    assert!(!a.is_unread(Provider::Claude, X) && !b.is_unread(Provider::Claude, X), "stays READ");
}
