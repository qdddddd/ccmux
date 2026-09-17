use super::*;
use crate::app::tests::{app, watched_by};
use crate::model::{CodexMeta, Kind, State};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

#[derive(Default)]
struct Script {
    calls: Vec<&'static str>,
    archive_ids: Vec<String>,
    archive_expected: Vec<CodexStatus>,
    archive_delay: Duration,
    archive_results: VecDeque<Result<codex::ArchiveOutcome, CodexDiagnostic>>,
    observations: VecDeque<CodexObservation>,
    prepare_error: Option<CodexDiagnostic>,
    reload_error: Option<CodexDiagnostic>,
}

thread_local! {
    static SCRIPT: Rc<RefCell<Script>> = Rc::new(RefCell::new(Script::default()));
}

struct Fake(Rc<RefCell<Script>>);
impl PollClient for Fake {
    fn poll(&mut self, _: i64) -> CodexObservation {
        let mut script = self.0.borrow_mut();
        script.calls.push("codex poll");
        script.observations.pop_front().unwrap_or_else(|| observation(vec![], true))
    }
    fn reload(&mut self, _: &CodexConfig) -> Result<(), CodexDiagnostic> {
        let mut script = self.0.borrow_mut();
        script.calls.push("reload");
        script.reload_error.clone().map_or(Ok(()), Err)
    }
    fn forget(&mut self, _: &str) {
        self.0.borrow_mut().calls.push("forget");
    }
}

fn fake_prepare(_: &CodexConfig) -> Result<Box<dyn PollClient>, CodexDiagnostic> {
    SCRIPT.with(|state| {
        let mut script = state.borrow_mut();
        script.calls.push("prepare");
        match script.prepare_error.clone() {
            Some(error) => Err(error),
            None => Ok(Box::new(Fake(state.clone())) as Box<dyn PollClient>),
        }
    })
}

fn fake_archive(_: &CodexConfig, id: &str, expected: &CodexStatus)
    -> Result<codex::ArchiveOutcome, CodexDiagnostic>
{
    let delay = SCRIPT.with(|state| {
        let mut script = state.borrow_mut();
        script.calls.push("archive");
        script.archive_ids.push(id.into());
        script.archive_expected.push(expected.clone());
        script.archive_delay
    });
    std::thread::sleep(delay); // a real block, like the RPC it stands in for
    SCRIPT.with(|state| state.borrow_mut().archive_results.pop_front()
        .unwrap_or(Ok(codex::ArchiveOutcome::Archived { updated_at: 20 })))
}

fn configured() -> App {
    SCRIPT.with(|s| *s.borrow_mut() = Script::default());
    let mut a = app();
    a.codex.settings.url = "ws://127.0.0.1:8965".into();
    a.codex.settings.token_file = "/never-read-test-token".into();
    a.codex.prepare = fake_prepare;
    a.codex_archive = fake_archive;
    a.agents_poll = || {
        SCRIPT.with(|s| s.borrow_mut().calls.push("claude poll"));
        Ok(payload(vec![claude()]))
    };
    a
}

fn row(n: u64, runtime: CodexStatus) -> Session {
    let (status, state) = match runtime {
        CodexStatus::NotLoaded => (Status::Idle, Some(State::Unloaded)),
        CodexStatus::Idle => (Status::Idle, None),
        CodexStatus::SystemError => (Status::Unknown("systemError".into()), None),
        CodexStatus::Active { .. } => (Status::Busy, Some(State::Working)),
        CodexStatus::Unknown(ref raw) => (Status::Unknown(raw.clone()), None),
    };
    Session {
        provider: Provider::Codex, codex: Some(CodexMeta { runtime, updated_at: 20 }),
        session_id: format!("01a00000-0000-7000-8000-{n:012x}"), id: Some(format!("{n:08x}")),
        name: format!("task {n}"), cwd: "/remote/codex-cwd".into(),
        kind: Kind::Background, pid: None, started_at: 10, status, state,
    }
}

fn claude() -> Session {
    Session { provider: Provider::Claude, codex: None, session_id: "claude-id".into(),
        id: Some("12345678".into()), name: "Claude task".into(), ..row(1, CodexStatus::Idle) }
}

fn payload(sessions: Vec<Session>) -> model::Payload {
    model::Payload { sessions, dropped: 0 }
}

fn observation(sessions: Vec<Session>, complete: bool) -> CodexObservation {
    CodexObservation {
        sessions, complete, cutoff_ms: 0, history_metadata_ids: BTreeSet::new(),
        diagnostic: if complete { None } else { Some(error(CodexFailureKind::Incomplete)) },
        source_drift: BTreeSet::new(),
    }
}

fn error(kind: CodexFailureKind) -> CodexDiagnostic {
    CodexDiagnostic { kind, message: format!("test {kind:?} failure") }
}

fn queue(observation: CodexObservation) {
    SCRIPT.with(|s| s.borrow_mut().observations.push_back(observation));
}

fn calls() -> Vec<&'static str> { SCRIPT.with(|s| s.borrow().calls.clone()) }

fn poll(a: &mut App, rows: Vec<Session>, complete: bool) {
    queue(observation(rows, complete));
    a.codex.force = true;
    assert!(a.poll_codex());
}

fn key(a: &mut App, code: KeyCode) {
    a.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn ctrl_x(a: &mut App) {
    a.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
}

fn age_ctrl_x(a: &mut App) {
    a.cx_last_press = a.cx_last_press.and_then(|at| at.checked_sub(Duration::from_secs(1)));
}

fn settle_ctrl_x(a: &mut App) {
    if let Some(pending) = &mut a.pending_delete {
        pending.at = pending.at.checked_sub(CX_SETTLE).unwrap_or(pending.at);
    }
    assert!(with_inventory(|_| Ok(String::new()), || a.tick_stop_arm()));
}

/// The settle re-reads the pane inventory. `answer` gets the verb and is
/// the whole session as far as the refresh can tell; anything else is a bug.
fn with_inventory<T>(answer: fn(&str) -> Result<String, crate::tmux::TmuxError>,
    test: impl FnOnce() -> T) -> T
{
    crate::tmux::test_commands::with(move |argv| {
        let args = if argv.first().is_some_and(|a| a == "-L") { &argv[2..] } else { argv };
        match args[0].as_str() {
            verb @ ("list-panes" | "list-windows") => answer(verb),
            other => panic!("unscripted tmux command {other}"),
        }
    }, test)
}

fn show(a: &mut App, rows: Vec<Session>) {
    a.sessions = rows;
    a.rebuild_rows();
}

/// A launch record for `target` in some other tab's stored map.
fn mapped_in_another_tab(a: &mut App, target: &Session) {
    let mut remote = PaneMap::new();
    remote.insert(&PaneId::parse("%9").unwrap(), PaneEntry {
        provider:Provider::Codex, session_id:target.session_id.clone(),
        short_id:target.id.clone().unwrap(), name:target.name.clone(), opened_at:1,
    });
    a.tabs.push(TabInfo { window:WindowId::parse("@9").unwrap(), index:9,
        sidebar:None, map:remote, hidden:HiddenLog::new() });
}

/// A Working Claude row above `codex`, with the cursor on `codex`.
fn beside_working_claude(codex: Session) -> App {
    let mut a = configured();
    let mut working = claude();
    working.status = Status::Busy;
    working.state = Some(State::Working);
    a.apply_poll(Ok(payload(vec![working])));
    poll(&mut a, vec![codex], true);
    a.rebuild_rows();
    key(&mut a, KeyCode::Char('G'));
    assert_eq!(a.selected_session().unwrap().provider, Provider::Codex);
    agents::test_spawn::reset();
    a
}

/// Everything drawn, at a width where no flash has to wrap.
fn screen(a: &App) -> String {
    use ratatui::{Terminal, backend::TestBackend};
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::ui::draw(f, a)).unwrap();
    let buf = term.backend().buffer();
    (0..24).map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>().join("\n")
}

fn sleep_until(at: Instant) {
    std::thread::sleep(at.saturating_duration_since(Instant::now()));
}

fn claude_error(a: &mut App, stderr: &str) {
    a.apply_poll(Err(AgentsError::Cmd { code: 1, stderr: stderr.into() }));
}

#[test]
fn off_never_prepares_or_polls_even_when_forced() {
    let mut a = configured();
    a.codex.settings.url.clear();
    a.codex.prepare = |_| panic!("OFF prepared");
    a.codex.force = true;
    a.codex.reload = true;
    assert!(!a.poll_codex());
    assert!(calls().is_empty());
    assert_eq!(a.diagnostics.codex.health, Health::NotYetObserved);
    assert!(a.codex.last_attempt.is_none());
}

#[test]
fn hidden_startup_keeps_codex_pending_but_claude_takes_first_poll() {
    let mut a = configured();
    watched_by(&mut a, 1, false);
    a.was_watched = false;
    a.force_poll = true;
    a.observe_watchers();
    assert!(a.poll_providers());
    assert_eq!(calls(), ["claude poll"]);
    assert!(a.codex.startup && a.codex.last_attempt.is_none());
    assert!(!a.poll_codex());
    watched_by(&mut a, 1, true);
    a.observe_watchers();
    assert!(a.poll_providers());
    assert_eq!(calls(), ["claude poll", "claude poll", "prepare", "codex poll"]);
    assert!(!a.codex.startup);
}

#[test]
fn explicit_refresh_and_post_verb_force_cross_the_gate_and_ladders() {
    for explicit in [false, true] {
        let mut a = configured();
        watched_by(&mut a, 0, false);
        a.codex.idle_streak = 99;
        a.codex.fail_streak = 99;
        a.codex.last_attempt = Some(Instant::now());
        if explicit {
            a.degraded = true; // layout half of r cannot call tmux
            key(&mut a, KeyCode::Char('r'));
        } else { a.act_force_refresh(); }
        assert!(calls().is_empty(), "r does no inline preparation");
        assert!(a.poll_providers());
        assert_eq!(calls(), ["claude poll", "prepare", "codex poll"]);
        assert!(!a.codex.startup && !a.codex.force && !a.codex.reload);
        assert!(!a.poll_codex());
    }
}

#[test]
fn keypress_and_wake_reset_both_idle_ladders_without_extra_polling() {
    let mut a = configured();
    a.codex.startup = false;
    a.codex.last_attempt = Some(Instant::now());
    a.idle_streak = 99;
    a.codex.idle_streak = 99;
    key(&mut a, KeyCode::Char('j'));
    assert_eq!((a.idle_streak, a.codex.idle_streak), (0, 0));
    assert!(!a.force_poll && !a.codex.force && calls().is_empty());
    watched_by(&mut a, 1, false);
    a.observe_watchers();
    a.idle_streak = 99;
    a.codex.idle_streak = 99;
    watched_by(&mut a, 1, true);
    a.observe_watchers();
    assert!(a.force_poll && a.codex.force);
    assert_eq!((a.idle_streak, a.codex.idle_streak), (0, 0));
}

#[test]
fn both_due_run_in_order_even_after_claude_failure() {
    let mut a = configured();
    a.agents_poll = || {
        SCRIPT.with(|s| s.borrow_mut().calls.push("claude failure"));
        Err(AgentsError::NotFound("missing".into()))
    };
    a.force_poll = true;
    queue(observation(vec![row(1, CodexStatus::Idle)], true));
    assert!(a.poll_providers());
    assert_eq!(calls(), ["claude failure", "prepare", "codex poll"]);
    assert!(a.poll_error.is_some() && a.codex_error().is_none());
    assert_eq!(a.sessions.len(), 1);
    assert_eq!((a.fail_streak, a.codex.fail_streak), (1, 0));
}

#[test]
fn preparation_failure_is_memoized_until_r() {
    let mut a = configured();
    SCRIPT.with(|s| s.borrow_mut().prepare_error = Some(error(CodexFailureKind::Credential)));
    for _ in 0..4 {
        a.codex.force = true;
        assert!(a.poll_codex());
    }
    assert_eq!(calls(), ["prepare"]);
    assert_eq!(a.codex.fail_streak, 4);
    assert_eq!(a.codex.interval(a.interval), BACKOFF);
    SCRIPT.with(|s| s.borrow_mut().prepare_error = None);
    a.degraded = true;
    key(&mut a, KeyCode::Char('r'));
    assert!(a.poll_codex());
    assert_eq!(calls(), ["prepare", "prepare", "codex poll"]);
    assert!(a.codex_error().is_none());
}

#[test]
fn invalid_url_never_reaches_prepare_and_is_provider_local() {
    let mut a = configured();
    a.codex.settings.url = "ws://192.0.2.1:8965".into();
    a.force_poll = true;
    assert!(a.poll_providers());
    assert_eq!(calls(), ["claude poll"]);
    assert!(a.codex.open_rejected);
    assert!(a.codex_error().is_some() && a.poll_error.is_none());
    assert_eq!(a.sessions[0].provider, Provider::Claude);
}

#[test]
fn failed_reload_keeps_old_client_and_rows_but_skips_that_poll() {
    let mut a = configured();
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    a.codex.idle_streak = 6;
    SCRIPT.with(|s| s.borrow_mut().reload_error = Some(error(CodexFailureKind::Configuration)));
    a.degraded = true;
    key(&mut a, KeyCode::Char('r'));
    assert!(a.poll_codex());
    assert_eq!(calls(), ["prepare", "codex poll", "reload"]);
    assert_eq!(a.sessions.len(), 1);
    assert!(a.codex.open_rejected && a.codex_error().is_some());
    assert_eq!((a.codex.fail_streak, a.codex.idle_streak), (1, 0));
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    assert_eq!(a.codex.fail_streak, 0);
    assert_eq!(calls().last(), Some(&"codex poll"));
    assert!(a.codex.open_rejected, "automatic success cannot clear the address latch");
    SCRIPT.with(|s| s.borrow_mut().reload_error = None);
    let fp = a.codex.fingerprint;
    key(&mut a, KeyCode::Char('r'));
    queue(observation(vec![row(1, CodexStatus::Idle)], true));
    assert!(a.poll_codex());
    assert_eq!(a.codex.fingerprint, fp);
    assert!(!a.codex.open_rejected);
    assert_eq!(calls().iter().filter(|c| **c == "prepare").count(), 1);
}

#[test]
fn provider_rows_replace_only_on_their_own_complete_observation() {
    let mut a = configured();
    a.apply_poll(Ok(payload(vec![claude()])));
    let one = row(1, CodexStatus::Idle);
    let two = row(2, CodexStatus::NotLoaded);
    poll(&mut a, vec![one.clone(), two.clone()], true);
    poll(&mut a, vec![row(1, CodexStatus::Active { flags: vec![] })], false);
    assert_eq!(a.sessions.len(), 3);
    assert_eq!(a.codex.rows[&one.session_id].group(), model::Group::Codex);
    assert!(a.codex.rows.contains_key(&two.session_id));
    assert_eq!(a.fail_streak, 0);
    a.apply_poll(Ok(payload(vec![])));
    assert_eq!(a.sessions.len(), 2);
    poll(&mut a, vec![], false);
    assert_eq!(a.sessions.len(), 2, "absence from an incomplete result is not deletion");
    a.apply_poll(Ok(payload(vec![claude()])));
    poll(&mut a, vec![], true);
    assert_eq!(a.sessions.len(), 1);
    assert_eq!(a.sessions[0].provider, Provider::Claude);
}

#[test]
fn codex_archive_local_refusals_never_arm_or_call_rpc() {
    let mut cases = Vec::new();
    cases.push((row(1, CodexStatus::Active { flags: vec![] }), "running — not archived"));
    let mut blocked = row(1, CodexStatus::Active { flags: vec!["waitingOnApproval".into()] });
    blocked.status = Status::Waiting;
    blocked.state = Some(State::Blocked);
    cases.push((blocked, "running — not archived"));
    cases.push((row(1, CodexStatus::SystemError), "state unknown — not archived"));
    cases.push((row(1, CodexStatus::Unknown("future".into())), "state unknown — not archived"));
    for (row, message) in cases {
        let mut a = configured();
        show(&mut a, vec![row]);
        ctrl_x(&mut a);
        assert!(a.stop_arm.is_none() && a.pending_delete.is_none());
        assert_eq!(a.message.as_ref().map(|m| (&*m.0, m.1)), Some((message, MsgLevel::Warn)));
        assert!(!calls().contains(&"archive"));
    }

    let mut a = configured();
    a.codex.settings.url.clear();
    show(&mut a, vec![row(1, CodexStatus::Idle)]);
    ctrl_x(&mut a);
    assert_eq!(a.message.as_ref().map(|m| (&*m.0, m.1)),
        Some(("codex not configured — archive unavailable", MsgLevel::Warn)));
    assert!(a.stop_arm.is_none() && !calls().contains(&"archive"));

    let mut a = configured();
    let target = row(1, CodexStatus::Idle);
    mapped_in_another_tab(&mut a, &target);
    show(&mut a, vec![target]);
    ctrl_x(&mut a);
    assert_eq!(a.message.as_ref().map(|m| (&*m.0, m.1)),
        Some(("close its pane first (x) — not archived", MsgLevel::Warn)));
    assert!(a.stop_arm.is_none() && !calls().contains(&"archive"));
}

#[test]
fn codex_archive_needs_two_presses_then_removes_and_suppresses_the_row() {
    let mut a = configured();
    let target = row(1, CodexStatus::Idle);
    let id = target.session_id.clone();
    show(&mut a, vec![target.clone()]);
    assert_eq!(a.selected_session().map(|s| s.provider), Some(Provider::Codex));

    ctrl_x(&mut a);
    assert_eq!(a.stop_arm.as_ref().map(|arm| (arm.provider, arm.session_id.as_str())),
        Some((Provider::Codex, id.as_str())), "message: {:?}", a.message);
    assert!(!calls().contains(&"archive"));
    assert_eq!(a.arm_hint().as_deref(), Some("Ctrl-x again to archive task 1"));

    age_ctrl_x(&mut a);
    ctrl_x(&mut a);
    assert!(a.stop_arm.is_none() && a.pending_delete.is_some());
    assert!(!calls().contains(&"archive"));
    settle_ctrl_x(&mut a);
    assert_eq!(SCRIPT.with(|s| s.borrow().archive_ids.clone()), vec![id.clone()]);
    assert!(a.sessions.is_empty() && a.rows.is_empty());
    assert_eq!(a.message.as_ref().map(|m| (&*m.0, m.1)),
        Some(("archived task 1", MsgLevel::Info)));
    assert!(a.codex.force);

    // A lagging loaded/read path may still return the archived Thread. The
    // local tombstone keeps it out until a complete authoritative union omits
    // it, after which a later unarchive can be rediscovered.
    poll(&mut a, vec![target.clone()], false);
    assert!(a.sessions.is_empty());
    poll(&mut a, vec![target.clone()], true);
    assert!(a.sessions.is_empty());
    poll(&mut a, vec![target.clone()], true);
    assert!(a.sessions.is_empty(), "a second lagging complete poll resurrected the archive");
    poll(&mut a, vec![], true);
    poll(&mut a, vec![target], true);
    assert_eq!(a.sessions.len(), 1);
}

#[test]
fn successful_archive_forgets_metadata_in_the_prepared_poll_client() {
    let mut a = configured();
    let target = row(1, CodexStatus::Idle);
    poll(&mut a, vec![target], true);
    a.rebuild_rows();

    ctrl_x(&mut a);
    age_ctrl_x(&mut a);
    ctrl_x(&mut a);
    settle_ctrl_x(&mut a);

    assert_eq!(calls(), ["prepare", "codex poll", "archive", "forget"]);
}

#[test]
fn codex_archive_revalidates_and_reports_failure_without_success() {
    for outcome in [
        Ok(codex::ArchiveOutcome::StateChanged),
        Err(CodexDiagnostic { kind:CodexFailureKind::Timeout,
            message:"codex archive timed out after 1000 ms".into() }),
    ] {
        let mut a = configured();
        let target = row(1, CodexStatus::NotLoaded);
        show(&mut a, vec![target]);
        SCRIPT.with(|s| s.borrow_mut().archive_results.push_back(outcome.clone()));
        ctrl_x(&mut a);
        age_ctrl_x(&mut a);
        ctrl_x(&mut a);
        settle_ctrl_x(&mut a);
        assert_eq!(SCRIPT.with(|s| s.borrow().archive_ids.len()), 1);
        assert_eq!(a.sessions.len(), 1);
        let (message, level) = a.message.clone().unwrap();
        assert_eq!(level, MsgLevel::Warn);
        match outcome {
            Ok(_) => assert_eq!(message, "state changed — not archived"),
            Err(_) => assert_eq!(message, "archive failed: codex archive timed out after 1000 ms"),
        }
        assert!(a.codex.force);
    }
}

#[test]
fn codex_archive_window_disarms_on_retarget_modes_and_repeat_bursts() {
    // Row disappearance followed by a Claude neighbour must never reach stop.
    agents::test_spawn::reset();
    let mut a = configured();
    let codex = row(1, CodexStatus::Idle);
    let claude = claude();
    show(&mut a, vec![claude.clone(), codex]);
    while a.selected_session().is_some_and(|s| s.provider != Provider::Codex) {
        key(&mut a, KeyCode::Char('j'));
    }
    ctrl_x(&mut a);
    a.sessions.retain(|s| s.provider == Provider::Claude);
    a.rebuild_rows();
    assert!(a.stop_arm.is_none(), "a vanished Codex row disarms immediately");
    age_ctrl_x(&mut a);
    ctrl_x(&mut a);
    assert!(a.stop_arm.is_none() && a.pending_delete.is_none());
    assert!(agents::test_spawn::calls().is_empty());
    assert!(!calls().contains(&"archive"));

    // Mode departure uses the shared disarm path.
    let mut a = configured();
    show(&mut a, vec![row(1, CodexStatus::Idle)]);
    ctrl_x(&mut a);
    key(&mut a, KeyCode::Char('/'));
    assert!(a.stop_arm.is_none() && a.pending_delete.is_none());

    // A held/repeated third event cancels the settling operation.
    let mut a = configured();
    show(&mut a, vec![row(1, CodexStatus::Idle)]);
    ctrl_x(&mut a);
    age_ctrl_x(&mut a);
    ctrl_x(&mut a);
    ctrl_x(&mut a);
    assert!(a.pending_delete.is_none());
    assert!(!calls().contains(&"archive"));

    // Vanishing during the settle interval cancels before the RPC seam.
    let mut a = configured();
    show(&mut a, vec![row(1, CodexStatus::Idle)]);
    ctrl_x(&mut a);
    age_ctrl_x(&mut a);
    ctrl_x(&mut a);
    assert!(a.pending_delete.is_some());
    a.sessions.clear();
    a.rebuild_rows();
    assert!(a.pending_delete.is_none());
    assert!(!a.tick_stop_arm());
    assert!(!calls().contains(&"archive"));
}

#[test]
fn codex_archive_window_expires_and_rechecks_row_and_pane_state() {
    let mut a = configured();
    show(&mut a, vec![row(1, CodexStatus::Idle), row(2, CodexStatus::NotLoaded)]);
    ctrl_x(&mut a);
    key(&mut a, KeyCode::Char('j'));
    assert!(a.stop_arm.is_none(), "moving to another Codex row disarms");
    assert!(!calls().contains(&"archive"));

    let mut a = configured();
    show(&mut a, vec![row(1, CodexStatus::Idle)]);
    ctrl_x(&mut a);
    a.stop_arm.as_mut().unwrap().at -= CX_WINDOW + Duration::from_millis(1);
    assert!(a.tick_stop_arm());
    assert!(a.stop_arm.is_none() && !calls().contains(&"archive"));

    let mut a = configured();
    let mut target = row(1, CodexStatus::Idle);
    show(&mut a, vec![target.clone()]);
    ctrl_x(&mut a);
    target.codex.as_mut().unwrap().runtime = CodexStatus::Active { flags:vec![] };
    target.status = Status::Busy;
    target.state = Some(State::Working);
    show(&mut a, vec![target]);
    age_ctrl_x(&mut a);
    ctrl_x(&mut a);
    assert_eq!(a.message.as_ref().unwrap().0, "running — not archived");
    assert!(a.pending_delete.is_none() && !calls().contains(&"archive"));

    let mut a = configured();
    let target = row(1, CodexStatus::NotLoaded);
    show(&mut a, vec![target.clone()]);
    ctrl_x(&mut a);
    a.map.insert(&PaneId::parse("%8").unwrap(), PaneEntry {
        provider:Provider::Codex, session_id:target.session_id,
        short_id:target.id.unwrap(), name:target.name, opened_at:1,
    });
    age_ctrl_x(&mut a);
    ctrl_x(&mut a);
    assert_eq!(a.message.as_ref().unwrap().0, "close its pane first (x) — not archived");
    assert!(a.pending_delete.is_none() && !calls().contains(&"archive"));
}

#[test]
fn unmaterialized_timestamps_anchor_without_freezing_metadata_or_idle_ladder() {
    let mut a = configured();
    let mut r = row(1, CodexStatus::Idle);
    for time in 10..18 {
        r.started_at = time;
        r.codex.as_mut().unwrap().updated_at = time + 100;
        poll(&mut a, vec![r.clone()], true);
        assert_eq!(a.sessions[0].started_at, 10);
        assert_eq!(a.sessions[0].codex.as_ref().unwrap().updated_at, time + 100);
    }
    assert_eq!(a.codex.idle_streak, 7);
    assert_eq!(a.codex.interval(a.interval), IDLE_MAX);
    let mut obs = observation(vec![r.clone()], true);
    obs.history_metadata_ids.insert(r.session_id.clone());
    queue(obs);
    a.codex.force = true;
    a.poll_codex();
    assert_eq!(a.sessions[0].started_at, 17, "history metadata replaces the anchor");
    assert_eq!(a.codex.idle_streak, 8, "provenance is not a payload change");
    r.name = "new name".into();
    poll(&mut a, vec![r], true);
    assert_eq!(a.codex.idle_streak, 0);
}

#[test]
fn scheduler_uses_one_provider_clock_and_the_longer_backoff() {
    let mut a = configured();
    let now = Instant::now();
    a.codex.startup = false;
    a.codex.last_attempt = Some(now);
    for (idle, fail, seconds) in [(0, 0, 2.5), (4, 0, 5.0), (5, 0, 10.0),
        (6, 0, 20.0), (7, 0, 30.0), (0, 3, 10.0)]
    {
        a.codex.idle_streak = idle;
        a.codex.fail_streak = fail;
        let interval = Duration::from_secs_f64(seconds);
        assert_eq!(a.codex.interval(a.interval), interval);
        assert!(!a.codex.due(true, a.interval, now + interval - Duration::from_nanos(1)));
        assert!(a.codex.due(true, a.interval, now + interval));
        assert!(!a.codex.due(false, a.interval, now + interval));
    }
    a.codex.force = true;
    assert!(a.codex.due(false, a.interval, now));
}

#[test]
fn claude_fingerprint_ignores_codex_changes() {
    let mut a = configured();
    a.apply_poll(Ok(payload(vec![claude()])));
    let fingerprint = a.payload_fp;
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    a.apply_poll(Ok(payload(vec![claude()])));
    assert_eq!(a.payload_fp, fingerprint);
    assert_eq!(a.idle_streak, 1);
    poll(&mut a, vec![row(2, CodexStatus::NotLoaded)], false);
    a.apply_poll(Ok(payload(vec![claude()])));
    assert_eq!(a.idle_streak, 2);
}

#[test]
fn restart_summary_keeps_full_ttl_before_first_failure_diagnostic() {
    let mut a = configured();
    let now = Instant::now();
    let summary = "restarted 2 sidebars; Codex panes skipped: 3";
    a.flash(summary, MsgLevel::Info);
    let deadline = a.msg_deadline;
    SCRIPT.with(|s| s.borrow_mut().prepare_error = Some(error(CodexFailureKind::Credential)));
    a.poll_codex();
    for offset in [0, 1, 3] {
        assert!(!a.deliver_diagnostics(now + Duration::from_secs(offset)));
        assert_eq!(a.message.as_ref().unwrap().0, summary);
        assert_eq!(a.msg_deadline, deadline);
    }
    a.message = None;
    a.msg_deadline = None;
    assert!(a.deliver_diagnostics(now + MSG_TTL));
    assert!(a.message.as_ref().unwrap().0.starts_with("codex degraded:"));
    assert_eq!(a.msg_deadline, Some(now + MSG_TTL + MSG_TTL));
}

#[test]
fn diagnostics_wait_for_overlays_and_armed_warning_then_coalesce() {
    let mut a = configured();
    let now = Instant::now();
    claude_error(&mut a, "failure A");
    SCRIPT.with(|s| s.borrow_mut().prepare_error = Some(error(CodexFailureKind::Credential)));
    a.poll_codex();
    for mode in [Mode::Help, Mode::Logs, Mode::Filter, Mode::Prompt(PromptKind::NewBackground)] {
        a.mode = mode;
        assert!(!a.deliver_diagnostics(now));
        assert!(a.message.is_none());
    }
    a.mode = Mode::Normal;
    a.stop_arm = Some(StopArm { provider:Provider::Claude, session_id:"claude-id".into(),
        short_id:"12345678".into(), name:"task".into(), at:now });
    assert!(!a.deliver_diagnostics(now));
    a.stop_arm = None;
    assert!(a.deliver_diagnostics(now));
    let text = &a.message.as_ref().unwrap().0;
    assert!(text.starts_with("agents: failure A; codex degraded:"));
    let deadline = a.msg_deadline;
    claude_error(&mut a, "failure B");
    assert!(!a.deliver_diagnostics(now + Duration::from_secs(1)));
    assert_eq!(a.msg_deadline, deadline);
}

#[test]
fn flapping_and_varying_stderr_cannot_reflash_inside_thirty_seconds() {
    let mut a = app(); // Also proves Claude-only delivery.
    let now = Instant::now();
    claude_error(&mut a, "attempt 1 failed");
    assert!(a.deliver_diagnostics(now));
    a.message = None;
    for i in 1..6 {
        a.apply_poll(Ok(payload(vec![])));
        claude_error(&mut a, &format!("attempt {i} failed"));
        assert!(!a.deliver_diagnostics(now + Duration::from_secs(i * 5)));
        assert_eq!(a.poll_error.as_deref(), Some(format!("attempt {i} failed").as_str()));
    }
    assert!(a.deliver_diagnostics(now + DIAGNOSTIC_COOLDOWN));
    assert!(a.message.as_ref().unwrap().0.contains("attempt 5"));
    a.message = None;
    claude_error(&mut a, "different raw stderr, same category");
    assert!(!a.deliver_diagnostics(now + Duration::from_secs(60)));
}

#[test]
fn category_changes_queue_and_explicit_r_bypasses_cooldown_but_not_guard() {
    let mut a = app();
    let now = Instant::now();
    claude_error(&mut a, "first");
    a.deliver_diagnostics(now);
    a.message = None;
    a.apply_poll(Err(AgentsError::NotFound("not found".into())));
    assert!(!a.deliver_diagnostics(now + Duration::from_secs(1)));
    a.diagnostics.claude.explicit_refresh = true;
    claude_error(&mut a, "requested retry");
    a.flash("user operation", MsgLevel::Info);
    assert!(!a.deliver_diagnostics(now + Duration::from_secs(2)));
    a.apply_poll(Ok(payload(vec![])));
    claude_error(&mut a, "later automatic failure");
    a.message = None;
    assert!(a.deliver_diagnostics(now + Duration::from_secs(3)));
    assert_eq!(a.message.as_ref().unwrap().0, "agents refresh failed: requested retry");
    a.message = None;
    a.apply_poll(Ok(payload(vec![])));
    claude_error(&mut a, "next");
    assert!(!a.deliver_diagnostics(now + Duration::from_secs(31)));
    assert!(a.deliver_diagnostics(now + Duration::from_secs(33)));
}

#[test]
fn recovery_cancels_automatic_diagnostic_without_periodic_reminders() {
    let mut a = configured();
    let now = Instant::now();
    claude_error(&mut a, "queued");
    a.apply_poll(Ok(payload(vec![])));
    assert!(!a.deliver_diagnostics(now + Duration::from_secs(60)));
    a.diagnostics.codex.failure(FailureCategory::Codex(CodexFailureKind::Connection), "refused".into());
    a.deliver_diagnostics(now);
    a.message = None;
    assert!(!a.deliver_diagnostics(now + Duration::from_secs(60)));
}

#[test]
fn runtime_warning_is_once_per_observed_episode_and_full_id() {
    let mut a = configured();
    let now = Instant::now();
    let r = row(1, CodexStatus::SystemError);
    poll(&mut a, vec![r.clone()], true);
    assert!(a.codex_error().is_none());
    assert!(a.drift_pending.is_empty());
    a.announce_warnings(now);
    let deadline = a.msg_deadline;
    poll(&mut a, vec![r.clone()], true);
    a.announce_warnings(now + Duration::from_secs(1));
    assert_eq!(a.msg_deadline, deadline);
    a.message = None;
    a.announce_warnings(now + MSG_TTL);
    assert!(a.diagnostics.runtime[&r.session_id].seen);
    poll(&mut a, vec![r.clone()], false);
    a.announce_warnings(now + Duration::from_secs(10));
    assert!(a.message.is_none());
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    assert!(!a.diagnostics.runtime.contains_key(&r.session_id));
    poll(&mut a, vec![r.clone()], true);
    a.announce_warnings(now + Duration::from_secs(11));
    assert_eq!(a.message.as_ref().unwrap().0, "codex runtime error: 00000001");
    let mut another = row(2, CodexStatus::SystemError);
    another.id = r.id.clone(); // Same display suffix, different warning identity.
    poll(&mut a, vec![r, another.clone()], true);
    a.message = None;
    a.announce_warnings(now + Duration::from_secs(15));
    assert!(!a.diagnostics.runtime[&another.session_id].seen);
    assert_eq!(a.diagnostics.runtime_flash.as_ref().unwrap().id, another.session_id);
}

#[test]
fn runtime_coverage_absence_and_recovery_do_not_mark_new_episode_seen() {
    let mut a = configured();
    let now = Instant::now();
    let r = row(1, CodexStatus::SystemError);
    poll(&mut a, vec![r.clone()], true);
    a.announce_warnings(now);
    a.mode = Mode::Help;
    a.announce_warnings(now + Duration::from_secs(1));
    a.message = None;
    a.mode = Mode::Normal;
    poll(&mut a, vec![], true);
    a.announce_warnings(now + Duration::from_secs(5));
    assert!(a.message.is_none());
    assert!(!a.diagnostics.runtime[&r.session_id].seen);
    poll(&mut a, vec![r.clone()], true);
    a.announce_warnings(now + Duration::from_secs(6));
    a.flash("unrelated message", MsgLevel::Info);
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    assert_eq!(a.message.as_ref().unwrap().0, "unrelated message");
    poll(&mut a, vec![r.clone()], true);
    a.message = None;
    a.announce_warnings(now + Duration::from_secs(10));
    assert!(!a.diagnostics.runtime[&r.session_id].seen);
}

#[test]
fn mixed_flags_and_source_drift_keep_provider_labels_and_unknown_values() {
    let mut a = configured();
    a.mode = Mode::Help;
    let r = row(1, CodexStatus::Active { flags: vec!["waitingOnApproval".into(), "futureFlag".into()] });
    let mut obs = observation(vec![r], false);
    obs.source_drift.insert("futureSource".into());
    queue(obs);
    a.poll_codex();
    assert!(a.drift_pending.iter().any(|s| s.contains("codex active flags") && s.contains("futureFlag")));
    assert!(a.drift_pending.contains("codex source futureSource"));
    assert!(!a.drift_pending.iter().any(|s| s.contains("waitingOnApproval")));
}

#[test]
fn codex_navigation_filter_completed_dismiss_undo_and_new_are_local() {
    let mut a = configured();
    let r = row(1, CodexStatus::NotLoaded);
    poll(&mut a, vec![r.clone()], true);
    a.rebuild_rows();
    assert_eq!(a.selected_session().unwrap().session_id, r.session_id);
    for filter in ["codex", &r.session_id, r.id.as_deref().unwrap()] {
        a.filter = filter.into();
        a.rebuild_rows();
        assert!(a.selected_session().is_some());
    }
    a.filter.clear();
    key(&mut a, KeyCode::Char('a'));
    assert!(a.selected_session().is_none());
    key(&mut a, KeyCode::Char('a'));
    key(&mut a, KeyCode::Char('d'));
    assert_eq!(a.hidden.provider_of(&r.session_id), Some(Provider::Codex));
    key(&mut a, KeyCode::Char('u'));
    assert!(a.selected_session().is_some());
    key(&mut a, KeyCode::Char('n'));
    let prompt = a.prompt.as_ref().unwrap();
    assert_eq!(prompt.kind, PromptKind::NewBackground);
    assert_eq!(prompt.fields[0], std::env::current_dir().unwrap().to_string_lossy());
    assert!(prompt.pending_create.is_none());
    assert_eq!(calls(), ["prepare", "codex poll"]);
}

#[test]
fn logs_refuses_while_ctrl_x_only_arms_without_external_io() {
    let mut a = configured();
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    a.rebuild_rows();
    key(&mut a, KeyCode::Char('L'));
    assert_eq!(a.message.as_ref().unwrap().0, "Codex logs unavailable in v1 — use the Codex TUI");
    a.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert_eq!(a.message.as_ref().unwrap().0, "Ctrl-x again to archive task 1");
    assert!(a.stop_arm.is_some() && a.pending_delete.is_none() && a.logs.is_none());
    assert_eq!(calls(), ["prepare", "codex poll"]);
    a.degraded = true;
    for (code, text) in [(KeyCode::Enter, "open"), (KeyCode::Char('t'), "tabs"), (KeyCode::Char('x'), "close")] {
        key(&mut a, code);
        assert_eq!(a.message.as_ref().unwrap().0, format!("not inside tmux — {text} unavailable"));
    }
}

#[test]
fn provider_cooldowns_are_independent_and_only_posted_candidates_are_consumed() {
    let mut a = configured();
    let now = Instant::now();
    claude_error(&mut a, "first failure");
    assert!(a.deliver_diagnostics(now));
    a.message = None;
    a.apply_poll(Ok(payload(vec![])));
    claude_error(&mut a, "next failure");
    a.diagnostics.codex.failure(FailureCategory::Codex(CodexFailureKind::Timeout), "timed out".into());
    assert!(a.deliver_diagnostics(now + Duration::from_secs(1)));
    assert_eq!(a.message.as_ref().unwrap().0, "codex degraded: timed out");
    a.message = None;
    assert!(!a.deliver_diagnostics(now + Duration::from_secs(29)));
    assert!(a.deliver_diagnostics(now + Duration::from_secs(30)));
    assert_eq!(a.message.as_ref().unwrap().0, "agents: next failure");
}

#[test]
fn codex_complete_poll_never_retires_a_claude_absence_strike() {
    let mut a = configured();
    let gone = claude();
    a.sessions = vec![gone.clone()];
    a.rebuild_rows();
    key(&mut a, KeyCode::Char('d'));
    a.apply_poll(Ok(payload(vec![])));
    assert!(a.hidden_absent.contains(&gone.session_id));
    for _ in 0..3 { poll(&mut a, vec![row(1, CodexStatus::Idle)], true); }
    assert!(a.hidden.ids().contains(&gone.session_id));
    assert!(a.hidden_absent.contains(&gone.session_id));
    a.apply_poll(Ok(payload(vec![])));
    assert!(!a.hidden.ids().contains(&gone.session_id));
}


#[test]
fn incomplete_observations_back_off_reset_idle_and_complete_recovers() {
    let mut a = configured();
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    a.codex.idle_streak = 6;
    for _ in 0..3 {
        let mut obs = observation(vec![row(1, CodexStatus::Idle)], false);
        obs.diagnostic = Some(error(CodexFailureKind::Timeout));
        queue(obs);
        a.codex.force = true;
        assert!(a.poll_codex());
    }
    assert_eq!(a.codex.fail_streak, 3);
    assert_eq!(a.codex.idle_streak, 0);
    assert_eq!(a.codex.interval(a.interval), BACKOFF);
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    assert_eq!(a.codex.fail_streak, 0);
    assert_eq!(a.codex.interval(a.interval), a.interval);
}

#[test]
fn post_verb_wake_and_backoff_never_reload_a_prepared_client() {
    let mut a = configured();
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    for _ in 0..4 { poll(&mut a, vec![row(1, CodexStatus::Idle)], false); }
    assert!(a.codex.fail_streak >= FAIL_BACKOFF_AT);
    a.codex.last_attempt = Some(Instant::now() - BACKOFF);
    assert!(a.poll_codex());
    assert!(!calls().contains(&"reload"), "automatic backoff reloaded");
    a.act_force_refresh();
    assert!(a.poll_codex());
    assert!(!calls().contains(&"reload"), "post-verb reloaded");
    watched_by(&mut a, 1, false);
    a.observe_watchers();
    watched_by(&mut a, 1, true);
    a.observe_watchers();
    assert!(a.poll_codex());
    assert!(!calls().contains(&"reload"), "wake reloaded");
    assert_eq!(calls().iter().filter(|c| **c == "prepare").count(), 1);
    assert!(!a.diagnostics.codex.explicit_refresh);
}

#[test]
fn explicit_codex_r_marks_the_attempt_and_bypasses_diagnostic_cooldown() {
    let mut a = configured();
    let now = Instant::now();
    poll(&mut a, vec![], false);
    assert!(a.deliver_diagnostics(now));
    assert!(a.message.as_ref().unwrap().0.starts_with("codex degraded:"));
    a.message = None;
    a.degraded = true;
    key(&mut a, KeyCode::Char('r'));
    assert!(a.codex.reload && a.codex.force);
    queue(observation(vec![], false));
    assert!(a.poll_codex());
    assert_eq!(calls(), ["prepare", "codex poll", "reload", "codex poll"]);
    assert!(a.deliver_diagnostics(now + Duration::from_secs(1)));
    assert!(a.message.as_ref().unwrap().0.starts_with("codex refresh failed:"));
    assert!(!a.codex.reload && !a.diagnostics.codex.explicit_refresh);
}

#[test]
fn explicit_claude_only_r_marks_the_attempt_and_bypasses_diagnostic_cooldown() {
    let mut a = app();
    let now = Instant::now();
    a.agents_poll = || Err(AgentsError::Cmd { code: 1, stderr: "boom".into() });
    a.force_poll = true;
    assert!(a.poll_providers());
    assert!(a.deliver_diagnostics(now));
    assert_eq!(a.message.as_ref().unwrap().0, "agents: boom");
    a.message = None;
    a.degraded = true;
    key(&mut a, KeyCode::Char('r'));
    assert!(a.diagnostics.claude.explicit_refresh);
    assert!(a.poll_providers());
    assert!(a.deliver_diagnostics(now + Duration::from_secs(1)));
    assert_eq!(a.message.as_ref().unwrap().0, "agents refresh failed: boom");
    assert!(!a.diagnostics.claude.explicit_refresh);
}

#[test]
fn post_verb_claude_failure_never_becomes_an_explicit_refresh() {
    let mut a = app();
    let now = Instant::now();
    a.agents_poll = || Err(AgentsError::Cmd { code: 1, stderr: "boom".into() });
    a.force_poll = true;
    assert!(a.poll_providers());
    assert!(a.deliver_diagnostics(now));
    a.message = None;
    a.act_force_refresh();
    assert!(!a.diagnostics.claude.explicit_refresh);
    assert!(a.poll_providers());
    assert!(!a.deliver_diagnostics(now + Duration::from_secs(1)));
    a.agents_poll = || Err(AgentsError::NotFound("missing".into()));
    a.act_force_refresh();
    assert!(a.poll_providers());
    assert!(!a.deliver_diagnostics(now + Duration::from_secs(2)));
    assert!(a.deliver_diagnostics(now + DIAGNOSTIC_COOLDOWN));
    assert!(a.message.as_ref().unwrap().0.starts_with("agents:"));
}

#[test]
fn codex_polls_on_its_own_clock_when_claude_is_not_due() {
    let mut a = configured();
    a.idle_streak = 99;
    a.last_agents = Instant::now();
    a.codex.startup = false;
    a.codex.last_attempt = Instant::now().checked_sub(Duration::from_secs(60));
    assert!(!a.poll_due());
    assert!(!a.poll_providers());
    assert_eq!(calls(), ["prepare", "codex poll"]);
}

#[test]
fn a_codex_arm_stamps_the_burst_guard_before_its_row_disappears() {
    let mut a = configured();
    let mut working = claude();
    working.status = Status::Busy;
    working.state = Some(State::Working);
    a.apply_poll(Ok(payload(vec![working])));
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    a.rebuild_rows();
    key(&mut a, KeyCode::Char('G'));
    assert_eq!(a.selected_session().unwrap().provider, Provider::Codex);
    agents::test_spawn::reset();
    let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
    a.on_key(ctrl_x);
    assert!(a.cx_last_press.is_some());
    assert!(a.stop_arm.as_ref().is_some_and(|arm| arm.provider == Provider::Codex));
    poll(&mut a, vec![], true);
    a.rebuild_rows();
    assert_eq!(a.selected_session().unwrap().provider, Provider::Claude);
    for _ in 0..3 { a.on_key(ctrl_x); }
    assert_eq!(a.message.as_ref().unwrap().0, CODEX_CLOSED);
    assert!(agents::test_spawn::joined().is_empty(), "a buffered refusal stopped a neighbour");
    assert!(a.stop_arm.is_none() && a.pending_delete.is_none());
}

#[test]
fn codex_ctrl_x_closes_a_claude_delete_window_before_returning_to_claude() {
    let mut a = configured();
    let mut working = claude();
    working.status = Status::Busy;
    working.state = Some(State::Working);
    a.apply_poll(Ok(payload(vec![working])));
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    a.rebuild_rows();
    key(&mut a, KeyCode::Char('g'));
    assert_eq!(a.selected_session().unwrap().provider, Provider::Claude);
    agents::test_spawn::reset();
    let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
    a.on_key(ctrl_x);
    assert!(a.stop_arm.is_some());
    assert_eq!(agents::test_spawn::joined(), ["stop 12345678"]);
    key(&mut a, KeyCode::Char('j'));
    assert_eq!(a.selected_session().unwrap().provider, Provider::Codex);
    a.cx_last_press = Some(Instant::now() - Duration::from_secs(1));
    a.on_key(ctrl_x);
    assert_eq!(a.message.as_ref().unwrap().0, "Ctrl-x again to archive task 1");
    assert!(a.stop_arm.as_ref().is_some_and(|arm| arm.provider == Provider::Codex));
    assert!(a.pending_delete.is_none());
    key(&mut a, KeyCode::Char('k'));
    assert_eq!(a.selected_session().unwrap().provider, Provider::Claude);
    assert!(a.stop_arm.is_none(), "crossing back to Claude disarms the archive");
    a.cx_last_press = Some(Instant::now() - Duration::from_secs(1));
    a.on_key(ctrl_x);
    assert_eq!(agents::test_spawn::joined(), ["stop 12345678", "stop 12345678"]);
    assert!(!agents::test_spawn::joined().iter().any(|call| call.starts_with("rm ")));
    assert_eq!(calls(), ["prepare", "codex poll"]);
}

#[test]
fn a_single_codex_poll_counts_source_and_flag_drift_in_the_same_flash() {
    let mut a = configured();
    let mut obs = observation(vec![row(1, CodexStatus::Active { flags: vec!["futureFlag".into()] })], false);
    obs.source_drift.insert(r#"{"custom":1}"#.into());
    queue(obs);
    assert!(a.poll_codex());
    let text = &a.message.as_ref().unwrap().0;
    assert!(text.contains("futureFlag") && text.contains("+1 more"), "{text}");
    let announced = a.drift_pending.clone();
    assert_eq!(announced.len(), 2);
    a.message = None;
    a.msg_deadline = None;
    a.tick_drift();
    assert_eq!(a.drift_seen, announced);
    assert!(a.drift_pending.is_empty() && a.message.is_none());
}

#[test]
fn drift_arriving_during_another_providers_flash_is_not_retired_unseen() {
    let mut a = configured();
    let mut c = claude();
    c.status = Status::Unknown("futureStatus".into());
    a.apply_poll(Ok(payload(vec![c])));
    let first = a.message.clone();
    poll(&mut a, vec![row(1, CodexStatus::Active { flags: vec!["futureFlag".into()] })], true);
    assert_eq!(a.message, first);
    a.message = None;
    a.msg_deadline = None;
    assert!(a.tick_drift());
    assert_eq!(a.drift_seen, BTreeSet::from([r#"status "futureStatus""#.into()]));
    assert!(a.message.as_ref().unwrap().0.contains("futureFlag"));
    assert_eq!(a.drift_pending.len(), 1);
    a.message = None;
    a.msg_deadline = None;
    assert!(!a.tick_drift());
    assert_eq!(a.drift_seen.len(), 2);
    assert!(a.drift_pending.is_empty());
}

#[test]
fn drift_delivery_settles_before_a_runtime_warning_takes_the_slot() {
    let mut a = configured();
    let now = Instant::now();
    let runtime = row(1, CodexStatus::SystemError);
    poll(&mut a, vec![runtime.clone(), row(2, CodexStatus::Unknown("futureStatus".into()))], true);
    assert!(a.message.as_ref().unwrap().0.starts_with("unmodelled"));
    a.message = None;
    a.msg_deadline = None;
    assert!(a.announce_warnings(now + MSG_TTL));
    assert!(a.drift_seen.iter().any(|value| value.contains("futureStatus")));
    assert_eq!(a.message.as_ref().unwrap().0, "codex runtime error: 00000001");
    a.message = None;
    a.msg_deadline = None;
    assert!(!a.announce_warnings(now + MSG_TTL + MSG_TTL));
    assert!(a.diagnostics.runtime[&runtime.session_id].seen);
    assert!(a.drift_pending.is_empty() && a.message.is_none());
}

#[test]
fn runtime_delivery_settles_before_a_pending_drift_warning_takes_the_slot() {
    let mut a = configured();
    let now = Instant::now();
    let runtime = row(1, CodexStatus::SystemError);
    poll(&mut a, vec![runtime.clone()], true);
    assert!(a.announce_warnings(now));
    let first = a.message.clone();
    poll(&mut a, vec![runtime.clone(), row(2, CodexStatus::Unknown("futureStatus".into()))], true);
    assert_eq!(a.message, first);
    a.message = None;
    a.msg_deadline = None;
    assert!(a.announce_warnings(now + MSG_TTL));
    assert!(a.diagnostics.runtime[&runtime.session_id].seen);
    assert!(a.message.as_ref().unwrap().0.contains("futureStatus"));
    a.message = None;
    a.msg_deadline = None;
    assert!(!a.announce_warnings(now + MSG_TTL + MSG_TTL));
    assert!(a.drift_pending.is_empty() && a.message.is_none());
}

#[test]
fn tab_and_backtab_cycle_five_groups_and_skip_hidden_empty_groups() {
    let mut a = configured();
    a.sessions = [Some(State::Blocked), Some(State::Working), None, Some(State::Done)]
        .into_iter().enumerate().map(|(i, state)| Session {
            session_id: format!("claude-{i}"), state, ..claude()
        }).collect();
    let unloaded = row(2, CodexStatus::NotLoaded);
    let mut blocked = row(1, CodexStatus::Active { flags: vec!["waitingOnApproval".into()] });
    blocked.status = Status::Waiting;
    blocked.state = Some(State::Blocked);
    a.sessions.extend([unloaded.clone(), blocked.clone()]);
    a.rebuild_rows();
    a.select_first();
    assert_eq!(a.selected_session().unwrap().group(), model::Group::Blocked);
    for target in ["claude-1", "claude-2", "claude-3", &blocked.session_id, "claude-0"] {
        key(&mut a, KeyCode::Tab);
        assert_eq!(a.selected_session().unwrap().session_id, target);
    }
    for target in [&blocked.session_id, "claude-3", "claude-2", "claude-1", "claude-0"] {
        key(&mut a, KeyCode::BackTab);
        assert_eq!(a.selected_session().unwrap().session_id, target);
    }
    key(&mut a, KeyCode::Char('a'));
    for target in ["claude-1", "claude-2", &blocked.session_id, "claude-0"] {
        key(&mut a, KeyCode::Tab);
        assert_eq!(a.selected_session().unwrap().session_id, target);
    }
    key(&mut a, KeyCode::BackTab);
    assert_eq!(a.selected_session().unwrap().session_id, blocked.session_id);
    assert!(a.rows.iter().any(|r| matches!(r, model::Row::Header { group: model::Group::Codex, count: 1 })));
    key(&mut a, KeyCode::Char('d')); // Only an unloaded Codex row remains, hidden by a.
    assert!(!a.rows.iter().any(|r| matches!(r, model::Row::Header { group: model::Group::Codex, .. })));
    a.select_first();
    key(&mut a, KeyCode::BackTab);
    assert_eq!(a.selected_session().unwrap().session_id, "claude-2");
    key(&mut a, KeyCode::Char('u'));
    a.select_first();
    key(&mut a, KeyCode::BackTab);
    assert_eq!(a.selected_session().unwrap().session_id, blocked.session_id);
    key(&mut a, KeyCode::Char('a'));
    assert!(a.rows.iter().any(|r| matches!(r, model::Row::Header { group: model::Group::Codex, count: 2 })));
    a.filter = unloaded.session_id;
    a.rebuild_rows();
    for code in [KeyCode::Tab, KeyCode::BackTab] {
        key(&mut a, code);
        assert_eq!(a.selected_session().unwrap().state, Some(State::Unloaded));
    }
    key(&mut a, KeyCode::Char('a'));
    assert!(a.rows.is_empty());
    for code in [KeyCode::Tab, KeyCode::BackTab] { key(&mut a, code); }
    assert!(a.selected_session().is_none());
    assert!(calls().is_empty(), "navigation must not poll or prepare");
}

#[test]
fn a_codex_refusal_stamps_the_burst_guard_before_its_row_disappears() {
    let mut blocked = row(1, CodexStatus::Active { flags: vec!["waitingOnApproval".into()] });
    blocked.status = Status::Waiting;
    blocked.state = Some(State::Blocked);
    type Setup = fn(&mut App, &Session);
    let cases: [(Session, Setup, &str); 7] = [
        (row(1, CodexStatus::Active { flags: vec![] }), |_, _| {}, "running — not archived"),
        (blocked, |_, _| {}, "running — not archived"),
        (row(1, CodexStatus::SystemError), |_, _| {}, "state unknown — not archived"),
        (row(1, CodexStatus::Unknown("future".into())), |_, _| {}, "state unknown — not archived"),
        (row(1, CodexStatus::Idle), mapped_in_another_tab, "close its pane first (x) — not archived"),
        (row(1, CodexStatus::Idle), |a, _| a.degraded = true, "not inside tmux — archive unavailable"),
        (row(1, CodexStatus::NotLoaded), |a, _| a.tabs_fresh = false, "pane map unavailable — not archived"),
    ];
    for (target, setup, refusal) in cases {
        let mut a = beside_working_claude(target.clone());
        setup(&mut a, &target);
        ctrl_x(&mut a);
        assert_eq!(a.message.as_ref().map(|m| (&*m.0, m.1)), Some((refusal, MsgLevel::Warn)));
        assert!(a.stop_arm.is_none() && a.pending_delete.is_none(), "{refusal}");
        poll(&mut a, vec![], true);
        a.rebuild_rows();
        assert_eq!(a.selected_session().unwrap().provider, Provider::Claude);
        ctrl_x(&mut a);
        assert!(agents::test_spawn::joined().is_empty(), "{refusal}: a double-tapped refusal stopped a neighbour");
        // Never "press Ctrl+X again": obeying that once the row had left is
        // what stopped the neighbour.
        assert_eq!(a.message.as_ref().unwrap().0, CODEX_CLOSED, "{refusal}");
    }
}

/// The confirming press and the retry both land inside the original two
/// seconds, CX_MIN_GAP apart, after the window closed under the cursor.
fn closed_window_refuses_paced_presses(close: fn(&mut App), presses: &[u64]) {
    let mut a = beside_working_claude(row(1, CodexStatus::Idle));
    ctrl_x(&mut a);
    let armed_at = a.stop_arm.as_ref().unwrap().at;
    assert!(screen(&a).contains("again to archive"));
    close(&mut a);
    assert!(a.stop_arm.is_none() && a.pending_delete.is_none());
    assert_eq!(a.selected_session().unwrap().provider, Provider::Claude);
    assert!(!screen(&a).contains("again to archive"), "{}", screen(&a));
    for &offset in presses {
        sleep_until(armed_at + Duration::from_millis(offset));
        ctrl_x(&mut a);
        assert!(armed_at.elapsed() < CX_WINDOW, "the press at {offset} ms left the window");
        assert!(agents::test_spawn::joined().is_empty(), "the press at {offset} ms stopped Claude");
        assert_eq!(a.message.as_ref().unwrap().0, CODEX_CLOSED);
        assert!(!screen(&a).contains("again to archive"));
    }
    assert!(!calls().contains(&"archive"));
}

fn vanish(a: &mut App) {
    poll(a, vec![], true);
    a.rebuild_rows();
}

#[test]
fn a_refused_press_cannot_shorten_a_vanished_codex_window() {
    closed_window_refuses_paced_presses(vanish, &[800, 1600]);
}

#[test]
fn a_vanished_codex_window_refuses_a_paced_press_without_an_earlier_one() {
    closed_window_refuses_paced_presses(vanish, &[1600]);
}

#[test]
fn a_codex_window_closed_by_a_burst_protects_the_neighbour_after_its_row_vanishes() {
    closed_window_refuses_paced_presses(|a| { ctrl_x(a); vanish(a); }, &[800, 1600]);
}

#[test]
fn a_refused_press_cannot_shorten_a_codex_window_closed_by_crossing() {
    closed_window_refuses_paced_presses(|a| key(a, KeyCode::Char('k')), &[800, 1600]);
}

#[test]
fn no_closed_codex_window_leaves_its_invitation_on_screen() {
    let expire = |a: &mut App| {
        a.stop_arm.as_mut().unwrap().at -= CX_WINDOW + Duration::from_millis(1);
        assert!(a.tick_stop_arm());
    };
    type Close = fn(&mut App);
    let closes: [(&str, Close); 5] = [
        ("crossing", |a| key(a, KeyCode::Char('k'))),
        ("vanish", vanish),
        ("expiry", expire),
        ("burst", ctrl_x),
        ("mode change", |a| { key(a, KeyCode::Char('/')); key(a, KeyCode::Esc); }),
    ];
    for (name, close) in closes {
        let mut a = beside_working_claude(row(1, CodexStatus::Idle));
        ctrl_x(&mut a);
        assert!(screen(&a).contains("Ctrl-x again to archive task 1"), "{name}");
        close(&mut a);
        assert_eq!(a.mode, Mode::Normal, "{name}");
        assert!(a.stop_arm.is_none() && a.pending_delete.is_none(), "{name}");
        let drawn = screen(&a);
        assert!(!drawn.contains("again to archive"), "{name}: {drawn}");
        assert!(drawn.contains(CODEX_CLOSED), "{name}: {drawn}");
        assert!(agents::test_spawn::joined().is_empty() && !calls().contains(&"archive"), "{name}");
    }
}

#[test]
fn a_repeat_burst_closes_a_codex_window_for_its_remainder() {
    let mut a = configured();
    show(&mut a, vec![row(1, CodexStatus::Idle)]);
    ctrl_x(&mut a);
    let armed_at = a.stop_arm.as_ref().unwrap().at;
    ctrl_x(&mut a);
    assert!(a.stop_arm.is_none() && a.pending_delete.is_none(), "a burst kept the archive window");
    assert_eq!(a.message.as_ref().unwrap().0, CODEX_CLOSED);
    for offset in [800, 1600] {
        sleep_until(armed_at + Duration::from_millis(offset));
        ctrl_x(&mut a);
        assert!(armed_at.elapsed() < CX_WINDOW, "the press at {offset} ms left the window");
        assert!(a.stop_arm.is_none() && a.pending_delete.is_none(),
            "the press at {offset} ms after a burst reopened or confirmed the archive");
        assert_eq!(a.message.as_ref().unwrap().0, CODEX_CLOSED);
    }
    assert!(!calls().contains(&"archive") && agents::test_spawn::joined().is_empty());
}

fn history(sessions: Vec<Session>, complete: bool) -> CodexObservation {
    let mut observation = observation(sessions, complete);
    observation.history_metadata_ids = observation.sessions.iter()
        .map(|row| row.session_id.clone()).collect();
    observation
}

fn poll_history(a: &mut App, rows: Vec<Session>, complete: bool) {
    queue(history(rows, complete));
    a.codex.force = true;
    assert!(a.poll_codex());
}

fn archive_selected(a: &mut App) {
    ctrl_x(a);
    age_ctrl_x(a);
    ctrl_x(a);
    settle_ctrl_x(a);
    assert!(a.message.as_ref().unwrap().0.starts_with("archived "), "{:?}", a.message);
}

fn unarchived(n: u64) -> Session {
    let mut row = row(n, CodexStatus::NotLoaded);
    row.codex.as_mut().unwrap().updated_at = 99;
    row
}

fn codex_names(a: &App) -> Vec<String> {
    a.sessions.iter().filter(|s| s.provider == Provider::Codex).map(|s| s.name.clone()).collect()
}

#[test]
fn codex_archive_fails_closed_without_a_readable_pane_inventory() {
    type Break = fn(&mut App);
    let cases: [(Break, &str); 3] = [
        (|a| a.degraded = true, "not inside tmux — archive unavailable"),
        (|a| a.panes_fresh = false, "pane map unavailable — not archived"),
        (|a| a.tabs_fresh = false, "pane map unavailable — not archived"),
    ];
    for (broken, refusal) in cases {
        // First press.
        let mut a = configured();
        show(&mut a, vec![row(1, CodexStatus::Idle)]);
        broken(&mut a);
        ctrl_x(&mut a);
        assert_eq!(a.message.as_ref().map(|m| (&*m.0, m.1)), Some((refusal, MsgLevel::Warn)));
        assert!(a.stop_arm.is_none());

        // Second press.
        let mut a = configured();
        show(&mut a, vec![row(1, CodexStatus::Idle)]);
        ctrl_x(&mut a);
        assert!(a.stop_arm.is_some());
        broken(&mut a);
        age_ctrl_x(&mut a);
        ctrl_x(&mut a);
        assert_eq!(a.message.as_ref().unwrap().0, refusal);
        assert!(a.pending_delete.is_none());
        assert!(!calls().contains(&"archive"), "{refusal}");
    }

    // Settle: outside tmux the refresh reads nothing at all.
    let mut a = configured();
    show(&mut a, vec![row(1, CodexStatus::Idle)]);
    ctrl_x(&mut a);
    age_ctrl_x(&mut a);
    ctrl_x(&mut a);
    a.degraded = true;
    settle_ctrl_x(&mut a);
    assert_eq!(a.message.as_ref().unwrap().0, "not inside tmux — archive unavailable");
    assert!(!calls().contains(&"archive"));
}

#[test]
fn a_failed_settle_inventory_refresh_refuses_the_archive() {
    type Answer = fn(&str) -> Result<String, crate::tmux::TmuxError>;
    let failures: [Answer; 2] = [
        |_| Err(crate::tmux::TmuxError::NotFound("list-panes failed".into())),
        |verb| if verb == "list-windows" {
            Err(crate::tmux::TmuxError::NotFound("list-windows failed".into()))
        } else { Ok(String::new()) },
    ];
    for answer in failures {
        let mut a = configured();
        show(&mut a, vec![row(1, CodexStatus::Idle)]);
        ctrl_x(&mut a);
        age_ctrl_x(&mut a);
        ctrl_x(&mut a);
        assert!(a.pending_delete.is_some());
        a.pending_delete.as_mut().unwrap().at -= CX_SETTLE;
        assert!(with_inventory(answer, || a.tick_stop_arm()));
        assert_eq!(a.message.as_ref().unwrap().0, "pane map unavailable — not archived");
        assert!(!calls().contains(&"archive"));
        assert_eq!(a.sessions.len(), 1);
    }
}

#[test]
fn the_archive_seam_receives_the_runtime_of_the_row_rechecked_at_settle() {
    for (armed, settled) in [
        (CodexStatus::NotLoaded, CodexStatus::NotLoaded),
        (CodexStatus::Idle, CodexStatus::Idle),
        // A poll between the press and the settle: the fresh read must match
        // the row as it is now, not as it was confirmed.
        (CodexStatus::Idle, CodexStatus::NotLoaded),
    ] {
        let mut a = configured();
        show(&mut a, vec![row(1, armed.clone())]);
        ctrl_x(&mut a);
        age_ctrl_x(&mut a);
        ctrl_x(&mut a);
        show(&mut a, vec![row(1, settled.clone())]);
        settle_ctrl_x(&mut a);
        assert_eq!(SCRIPT.with(|s| s.borrow().archive_expected.clone()), vec![settled]);
    }
}

#[test]
fn the_settle_rechecks_the_row_before_the_rpc() {
    let mut a = configured();
    show(&mut a, vec![row(1, CodexStatus::Idle)]);
    ctrl_x(&mut a);
    age_ctrl_x(&mut a);
    ctrl_x(&mut a);
    assert!(a.pending_delete.is_some());
    show(&mut a, vec![row(1, CodexStatus::Active { flags: vec![] })]);
    settle_ctrl_x(&mut a);
    assert_eq!(a.message.as_ref().unwrap().0, "running — not archived");
    assert!(!calls().contains(&"archive"));
    assert_eq!(a.sessions.len(), 1);
}

#[test]
fn a_press_buffered_during_a_slow_archive_reads_as_a_burst() {
    let mut a = beside_working_claude(row(1, CodexStatus::Idle));
    SCRIPT.with(|s| s.borrow_mut().archive_delay = Duration::from_millis(900));
    ctrl_x(&mut a);
    age_ctrl_x(&mut a);
    ctrl_x(&mut a);
    settle_ctrl_x(&mut a);
    assert_eq!(a.message.as_ref().unwrap().0, "archived task 1");
    assert_eq!(a.selected_session().unwrap().provider, Provider::Claude);
    // The press the operator made at the frozen screen.
    ctrl_x(&mut a);
    assert!(agents::test_spawn::joined().is_empty(), "a press buffered during the RPC stopped Claude");
    assert_eq!(a.message.as_ref().unwrap().0, "too fast — press Ctrl+X again");
}

#[test]
fn an_unarchive_after_an_incomplete_forced_poll_is_rediscovered() {
    let mut a = configured();
    show(&mut a, vec![row(1, CodexStatus::Idle)]);
    archive_selected(&mut a);
    poll(&mut a, vec![], false);
    poll_history(&mut a, vec![unarchived(1)], true);
    assert_eq!(codex_names(&a), ["task 1"]);
    poll_history(&mut a, vec![unarchived(1)], true);
    assert_eq!(codex_names(&a), ["task 1"]);
}

#[test]
fn a_persistently_incomplete_fleet_still_rediscovers_an_unarchived_thread() {
    let mut a = configured();
    let other = row(2, CodexStatus::Idle);
    show(&mut a, vec![row(1, CodexStatus::Idle), other.clone()]);
    archive_selected(&mut a);
    assert_eq!(codex_names(&a), ["task 2"]);
    for _ in 0..3 { poll_history(&mut a, vec![other.clone()], false); }
    poll_history(&mut a, vec![other.clone(), unarchived(1)], false);
    assert_eq!(codex_names(&a), ["task 1", "task 2"]);
    for _ in 0..2 { poll_history(&mut a, vec![other.clone(), unarchived(1)], false); }
    assert_eq!(codex_names(&a), ["task 1", "task 2"]);
}

#[test]
fn a_lagging_copy_of_an_archived_thread_stays_hidden() {
    let mut a = configured();
    let target = row(1, CodexStatus::Idle);
    show(&mut a, vec![target.clone()]);
    archive_selected(&mut a);
    // The loaded-list/read path, and even a history row, still carrying the
    // archived `updatedAt`.
    poll(&mut a, vec![row(1, CodexStatus::NotLoaded)], true);
    poll(&mut a, vec![row(1, CodexStatus::NotLoaded)], true);
    poll_history(&mut a, vec![target.clone()], true);
    poll_history(&mut a, vec![target.clone()], false);
    assert!(a.sessions.is_empty());
    // Only a COMPLETE union may retire the tombstone by omission.
    poll(&mut a, vec![], false);
    poll(&mut a, vec![row(1, CodexStatus::NotLoaded)], true);
    assert!(a.sessions.is_empty(), "an incomplete poll's omission retired the tombstone");
    poll(&mut a, vec![], true);
    poll(&mut a, vec![target], true);
    assert_eq!(a.sessions.len(), 1, "a complete omission did not retire the tombstone");
}

#[test]
fn only_a_confirmed_archive_hides_a_thread() {
    for outcome in [
        Ok(codex::ArchiveOutcome::StateChanged),
        Err(error(CodexFailureKind::Timeout)),
        Err(error(CodexFailureKind::Protocol)),
    ] {
        let mut a = configured();
        show(&mut a, vec![row(1, CodexStatus::Idle)]);
        SCRIPT.with(|s| s.borrow_mut().archive_results.push_back(outcome.clone()));
        ctrl_x(&mut a);
        age_ctrl_x(&mut a);
        ctrl_x(&mut a);
        settle_ctrl_x(&mut a);
        assert_eq!(SCRIPT.with(|s| s.borrow().archive_ids.len()), 1);
        for _ in 0..2 {
            poll(&mut a, vec![row(1, CodexStatus::Active { flags: vec![] })], true);
            assert_eq!(codex_names(&a), ["task 1"], "{outcome:?} hid a thread that was not archived");
        }
    }
}

// ── Every Codex window ending that archived nothing protects the neighbour ──
//
// A second review of the fixes found the same class again: the window could
// also end through a refused second press, a settle-time StateChanged or
// failure, or a row that left — and each of those left the rest of the two
// seconds unguarded, so a paced retry after the forced poll dropped the row
// stopped the Claude neighbour. Real sleeps; every press is >= CX_MIN_GAP
// after the last deliberate one and inside the original window.

fn message(a: &App) -> String { a.message.as_ref().map(|m| m.0.clone()).unwrap_or_default() }

fn press_inside_the_window(a: &mut App, armed_at: Instant, offset_ms: u64) {
    sleep_until(armed_at + Duration::from_millis(offset_ms));
    ctrl_x(a);
    assert!(armed_at.elapsed() < CX_WINDOW, "the press left the original window");
}

#[test]
fn a_refused_second_press_protects_the_rest_of_the_codex_window() {
    let mut a = beside_working_claude(row(1, CodexStatus::Idle));
    ctrl_x(&mut a);
    let armed_at = a.stop_arm.as_ref().unwrap().at;
    poll(&mut a, vec![row(1, CodexStatus::Active { flags: vec![] })], true);
    a.rebuild_rows();
    assert!(a.stop_arm.is_some(), "a row that stays visible does not disarm");
    press_inside_the_window(&mut a, armed_at, 800);
    assert_eq!(message(&a), "running — not archived");
    assert!(a.stop_arm.is_none() && a.pending_delete.is_none());
    vanish(&mut a);
    assert_eq!(a.selected_session().unwrap().provider, Provider::Claude);
    press_inside_the_window(&mut a, armed_at, 1600);
    assert!(agents::test_spawn::joined().is_empty(), "stopped {:?}", agents::test_spawn::joined());
    assert!(!calls().contains(&"archive"));
}

#[test]
fn a_settle_that_archives_nothing_protects_the_rest_of_the_codex_window() {
    for outcome in ["state-changed", "timeout", "protocol"] {
        let mut a = beside_working_claude(row(1, CodexStatus::Idle));
        SCRIPT.with(|s| s.borrow_mut().archive_results.push_back(match outcome {
            "state-changed" => Ok(codex::ArchiveOutcome::StateChanged),
            "timeout" => Err(error(CodexFailureKind::Timeout)),
            _ => Err(error(CodexFailureKind::Protocol)),
        }));
        ctrl_x(&mut a);
        let armed_at = a.stop_arm.as_ref().unwrap().at;
        press_inside_the_window(&mut a, armed_at, 780);
        assert!(a.pending_delete.is_some(), "{outcome}");
        sleep_until(armed_at + Duration::from_millis(780) + CX_SETTLE + Duration::from_millis(5));
        assert!(with_inventory(|_| Ok(String::new()), || a.tick_stop_arm()));
        assert!(message(&a).contains("not archived") || message(&a).starts_with("archive failed: "),
            "{outcome}: {}", message(&a));
        vanish(&mut a);
        assert_eq!(a.selected_session().unwrap().provider, Provider::Claude);
        press_inside_the_window(&mut a, armed_at, 1780);
        assert!(agents::test_spawn::joined().is_empty(),
            "{outcome}: a retry inside the window stopped {:?}", agents::test_spawn::joined());
        assert!(!message(&a).contains("press Ctrl+X again"), "{outcome}: {}", message(&a));
    }
}

#[test]
fn a_state_change_then_a_hidden_unload_stops_nothing() {
    let mut a = beside_working_claude(row(1, CodexStatus::Idle));
    key(&mut a, KeyCode::Char('a'));
    assert_eq!(a.selected_session().unwrap().provider, Provider::Codex);
    SCRIPT.with(|s| s.borrow_mut().archive_results.push_back(Ok(codex::ArchiveOutcome::StateChanged)));
    ctrl_x(&mut a);
    let armed_at = a.stop_arm.as_ref().unwrap().at;
    press_inside_the_window(&mut a, armed_at, 800);
    settle_ctrl_x(&mut a);
    assert_eq!(message(&a), "state changed — not archived");
    // The thread unloaded; `a` hides it and the cursor falls to Claude.
    poll(&mut a, vec![row(1, CodexStatus::NotLoaded)], true);
    a.rebuild_rows();
    assert_eq!(a.selected_session().unwrap().provider, Provider::Claude);
    press_inside_the_window(&mut a, armed_at, 1700);
    assert!(agents::test_spawn::joined().is_empty(), "stopped {:?}", agents::test_spawn::joined());
}

#[test]
fn a_double_tapped_codex_refusal_never_invites_another_press() {
    let mut a = beside_working_claude(row(1, CodexStatus::Active { flags: vec![] }));
    ctrl_x(&mut a);
    let t0 = Instant::now();
    assert_eq!(message(&a), "running — not archived");
    sleep_until(t0 + Duration::from_millis(200));
    ctrl_x(&mut a);
    assert_eq!(message(&a), CODEX_CLOSED);
    vanish(&mut a);
    assert_eq!(a.selected_session().unwrap().provider, Provider::Claude);
    assert!(!screen(&a).contains("press Ctrl+X again"));
    assert!(agents::test_spawn::joined().is_empty());
}

#[test]
fn the_closed_wording_lasts_as_long_as_the_guard_a_refused_press_set() {
    let mut a = beside_working_claude(row(1, CodexStatus::Idle));
    ctrl_x(&mut a);
    let armed_at = a.stop_arm.as_ref().unwrap().at;
    vanish(&mut a);
    assert_eq!(a.selected_session().unwrap().provider, Provider::Claude);
    press_inside_the_window(&mut a, armed_at, 1900);
    assert_eq!(message(&a), CODEX_CLOSED);
    // Past the original window, but inside the guard that 1.9 s press set.
    sleep_until(armed_at + Duration::from_millis(2500));
    ctrl_x(&mut a);
    assert_eq!(message(&a), CODEX_CLOSED);
    assert!(!screen(&a).contains("press Ctrl+X again"));
    assert!(agents::test_spawn::joined().is_empty());
}

#[test]
fn a_slow_failed_archive_keeps_a_buffered_press_uninvited() {
    let mut a = beside_working_claude(row(1, CodexStatus::Idle));
    SCRIPT.with(|s| {
        let mut s = s.borrow_mut();
        s.archive_results.push_back(Err(error(CodexFailureKind::Timeout)));
        s.archive_delay = Duration::from_millis(1000);
    });
    ctrl_x(&mut a);
    let armed_at = a.stop_arm.as_ref().unwrap().at;
    press_inside_the_window(&mut a, armed_at, 800);
    settle_ctrl_x(&mut a);
    assert!(armed_at.elapsed() >= Duration::from_millis(1800), "the RPC ate most of the window");
    vanish(&mut a);
    sleep_until(armed_at + CX_WINDOW + Duration::from_millis(50));
    ctrl_x(&mut a); // typed during the RPC, read just after the window ended
    assert_eq!(message(&a), CODEX_CLOSED);
    assert!(agents::test_spawn::joined().is_empty());
}

#[test]
fn a_tap_buffered_behind_a_blocking_tick_is_not_a_second_press() {
    let mut a = beside_working_claude(row(1, CodexStatus::Idle));
    ctrl_x(&mut a);
    let armed_at = a.stop_arm.as_ref().unwrap().at;
    // A 900 ms tick: under SLOW_TICK, so nothing drains, but long enough that
    // a double tap's second half typed at +50 ms is dequeued >= CX_MIN_GAP on.
    std::thread::sleep(Duration::from_millis(900));
    a.note_blocking_tick(Instant::now());
    ctrl_x(&mut a);
    assert!(a.pending_delete.is_none(), "a double tap scheduled the archive");
    sleep_until(armed_at + Duration::from_millis(900) + CX_SETTLE + Duration::from_millis(5));
    with_inventory(|_| Ok(String::new()), || a.tick_stop_arm());
    assert!(!calls().contains(&"archive"));
    assert_eq!(message(&a), CODEX_CLOSED);
}

#[test]
fn a_blocking_tick_without_a_codex_window_stamps_nothing() {
    let mut a = beside_working_claude(row(1, CodexStatus::Idle));
    assert!(a.cx_last_press.is_none());
    a.note_blocking_tick(Instant::now());
    assert!(a.cx_last_press.is_none(), "Claude timing must not change");
}

#[test]
fn a_lagging_loaded_read_with_a_new_updated_at_is_not_a_rediscovery() {
    let mut a = configured();
    show(&mut a, vec![row(1, CodexStatus::Idle)]);
    archive_selected(&mut a);
    let mut lagging = row(1, CodexStatus::NotLoaded);
    lagging.codex.as_mut().unwrap().updated_at = 99;
    // Plain `poll`: the row is not in the archived:false history.
    poll(&mut a, vec![lagging.clone()], false);
    assert!(codex_names(&a).is_empty(), "an incomplete lagging read resurrected the row");
    poll(&mut a, vec![lagging.clone()], true);
    assert!(codex_names(&a).is_empty(), "a complete lagging read resurrected the row");
    poll(&mut a, vec![lagging], true);
    assert!(codex_names(&a).is_empty());
}

#[test]
fn an_archived_polled_row_stays_gone_after_an_incomplete_poll() {
    let mut a = beside_working_claude(row(1, CodexStatus::Idle));
    archive_selected(&mut a);
    assert!(codex_names(&a).is_empty());
    poll(&mut a, vec![], false);
    assert!(codex_names(&a).is_empty(), "the cached row came back after an incomplete poll");
}
