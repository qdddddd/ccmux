use super::*;
use crate::app::tests::{app, watched_by};
use crate::model::{CodexMeta, Kind, State};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

#[derive(Default)]
struct Script {
    calls: Vec<&'static str>,
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

fn configured() -> App {
    SCRIPT.with(|s| *s.borrow_mut() = Script::default());
    let mut a = app();
    a.codex.settings.url = "ws://127.0.0.1:8965".into();
    a.codex.settings.token_file = "/never-read-test-token".into();
    a.codex.prepare = fake_prepare;
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
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
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
    assert_eq!(a.codex.rows[&one.session_id].group(), model::Group::Working);
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
    a.stop_arm = Some(StopArm { session_id: "claude-id".into(), short_id: "12345678".into(), name: "task".into(), at: now });
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
    a.announce_runtime(now);
    let deadline = a.msg_deadline;
    poll(&mut a, vec![r.clone()], true);
    a.announce_runtime(now + Duration::from_secs(1));
    assert_eq!(a.msg_deadline, deadline);
    a.message = None;
    a.announce_runtime(now + MSG_TTL);
    assert!(a.diagnostics.runtime[&r.session_id].seen);
    poll(&mut a, vec![r.clone()], false);
    a.announce_runtime(now + Duration::from_secs(10));
    assert!(a.message.is_none());
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    assert!(!a.diagnostics.runtime.contains_key(&r.session_id));
    poll(&mut a, vec![r.clone()], true);
    a.announce_runtime(now + Duration::from_secs(11));
    assert_eq!(a.message.as_ref().unwrap().0, "codex runtime error: 00000001");
    let mut another = row(2, CodexStatus::SystemError);
    another.id = r.id.clone(); // Same display suffix, different warning identity.
    poll(&mut a, vec![r, another.clone()], true);
    a.message = None;
    a.announce_runtime(now + Duration::from_secs(15));
    assert!(!a.diagnostics.runtime[&another.session_id].seen);
    assert_eq!(a.diagnostics.runtime_flash.as_ref().unwrap().id, another.session_id);
}

#[test]
fn runtime_coverage_absence_and_recovery_do_not_mark_new_episode_seen() {
    let mut a = configured();
    let now = Instant::now();
    let r = row(1, CodexStatus::SystemError);
    poll(&mut a, vec![r.clone()], true);
    a.announce_runtime(now);
    a.mode = Mode::Help;
    a.announce_runtime(now + Duration::from_secs(1));
    a.message = None;
    a.mode = Mode::Normal;
    poll(&mut a, vec![], true);
    a.announce_runtime(now + Duration::from_secs(5));
    assert!(a.message.is_none());
    assert!(!a.diagnostics.runtime[&r.session_id].seen);
    poll(&mut a, vec![r.clone()], true);
    a.announce_runtime(now + Duration::from_secs(6));
    a.flash("unrelated message", MsgLevel::Info);
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    assert_eq!(a.message.as_ref().unwrap().0, "unrelated message");
    poll(&mut a, vec![r.clone()], true);
    a.message = None;
    a.announce_runtime(now + Duration::from_secs(10));
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
fn unsupported_codex_verbs_refuse_before_any_claude_or_tmux_call() {
    let mut a = configured();
    poll(&mut a, vec![row(1, CodexStatus::Idle)], true);
    a.rebuild_rows();
    key(&mut a, KeyCode::Char('L'));
    assert_eq!(a.message.as_ref().unwrap().0, "Codex logs unavailable in v1 — use the Codex TUI");
    a.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert_eq!(a.message.as_ref().unwrap().0, "Codex stop/delete unavailable in v1 — use the Codex TUI");
    assert!(a.stop_arm.is_none() && a.pending_delete.is_none() && a.logs.is_none());
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
