//! Fixtures follow the measured 0.153.4 response subset in PROBE-FINDINGS §9.
//! Tests use fake IO, test-owned loopback listeners and temporary fixture tokens.
//! Only ignored live tests can contact a daemon or read an external credential.

use super::*;
use std::{cell::{Cell, RefCell}, collections::VecDeque, rc::Rc};
use crate::model::Group;

const SECRET: &str = "fixture-capability-do-not-log";
const NOW: i64 = 1_800_000_000_000;
const CUTOFF: i64 = NOW - HISTORY_DAYS * 86_400_000;

fn id(n: u64) -> String { format!("01a0a609-12a6-7000-8000-{n:012x}") }

fn config(url: &str) -> CodexConfig {
    CodexConfig { url: url.into(), token_file: "/unused/fixture-token".into(), bin: "codex".into() }
}

fn client() -> CodexClient {
    prepare_with(&config("ws://127.0.0.1:8965"),
        |_, _| panic!("literal addresses must not resolve"),
        |_| Ok(format!("{SECRET}\n\n"))).ok().unwrap()
}

fn thread(n: u64) -> Value {
    json!({
        "id":id(n), "sessionId":id(999), "name":"fixture task", "preview":"first prompt",
        "cwd":"/fixture/work", "createdAt":NOW/1000-100, "updatedAt":NOW/1000,
        "ephemeral":false, "parentThreadId":null, "forkedFromId":null,
        "source":"cli", "status":{"type":"idle"}, "turns":[],
        "cliVersion":"0.153.4", "modelProvider":"openai", "projectId":null
    })
}

fn eligible(value: &Value) -> Session {
    match parse_thread(value) {
        Ok(Parsed::Eligible(row)) => *row,
        _ => panic!("expected eligible fixture"),
    }
}

fn response_page(data: Vec<Value>, next: Option<&str>) -> Value {
    json!({"data":data,"nextCursor":next})
}

#[derive(Clone, Default)]
struct FakeClock {
    time: Rc<Cell<Duration>>,
    step: Rc<Cell<Duration>>,
}

impl FakeClock {
    fn advance(&self, duration: Duration) { self.time.set(self.time.get() + duration); }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        let now = self.time.get();
        self.advance(self.step.get());
        now
    }
}

struct Script {
    loaded: BTreeMap<String, Value>,
    history: BTreeMap<String, Value>,
    reads: BTreeMap<String, Value>,
    rpc_errors: BTreeSet<String>,
    response_ids: BTreeMap<String, Value>,
    read_costs: BTreeMap<String, Duration>,
    archive_result: Value,
    archive_ids: Vec<String>,
    archive_cost: Duration,
    incoming: VecDeque<(Duration, Value)>,
    ahead: Vec<Value>,
    sent: Vec<Value>,
    read_ids: Vec<String>,
    connect_cost: Duration,
    send_cost: Duration,
    receive_cost: Duration,
    read_cost: Duration,
    close_cost: Duration,
    closes: usize,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            loaded: BTreeMap::from([(String::new(), response_page(vec![], None))]),
            history: BTreeMap::from([(String::new(), response_page(vec![], None))]),
            reads: BTreeMap::new(), rpc_errors: BTreeSet::new(),
            response_ids: BTreeMap::new(), read_costs: BTreeMap::new(),
            archive_result:json!({}), archive_ids:Vec::new(), archive_cost:Duration::ZERO,
            incoming: VecDeque::new(), ahead: Vec::new(), sent: Vec::new(), read_ids: Vec::new(),
            connect_cost: Duration::ZERO, send_cost: Duration::ZERO, receive_cost: Duration::ZERO,
            read_cost: Duration::ZERO, close_cost: Duration::ZERO, closes: 0,
        }
    }
}

struct FakeConnector {
    script: Rc<RefCell<Script>>,
    clock: FakeClock,
}

impl FakeConnector {
    fn new(clock: &FakeClock, script: Script) -> Self {
        Self { script: Rc::new(RefCell::new(script)), clock: clock.clone() }
    }
}

struct FakeTransport {
    script: Rc<RefCell<Script>>,
    clock: FakeClock,
}

impl Connector for FakeConnector {
    fn connect<'a>(&mut self, _: &Prepared, _: Deadline<'a>)
        -> Result<Box<dyn Transport + 'a>, CodexError>
    {
        self.clock.advance(self.script.borrow().connect_cost);
        Ok(Box::new(FakeTransport { script: self.script.clone(), clock: self.clock.clone() }))
    }
}

impl Transport for FakeTransport {
    fn send(&mut self, value: Value) -> Result<(), CodexError> {
        let mut script = self.script.borrow_mut();
        self.clock.advance(script.send_cost);
        script.sent.push(value.clone());
        let method = value["method"].as_str().unwrap();
        if method == "initialized" { return Ok(()); }
        let before = std::mem::take(&mut script.ahead);
        for value in before { script.incoming.push_back((Duration::from_millis(1), value)); }
        let cursor = value["params"]["cursor"].as_str().unwrap_or("");
        let mut cost = script.receive_cost;
        let data = match method {
            "initialize" => Some(json!({"userAgent":"codex_cli_rs/0.153.4 (Linux)",
                "platformFamily":"unix","platformOs":"linux","codexHome":"/fixture"})),
            "thread/loaded/list" => script.loaded.get(cursor).cloned(),
            "thread/list" => script.history.get(cursor).cloned(),
            "thread/read" => {
                let id = value["params"]["threadId"].as_str().unwrap();
                script.read_ids.push(id.into());
                cost += script.read_costs.get(id).copied().unwrap_or(script.read_cost);
                script.reads.get(id).cloned().map(|thread| json!({"thread":thread}))
            }
            "thread/archive" => {
                let id = value["params"]["threadId"].as_str().unwrap();
                script.archive_ids.push(id.into());
                cost += script.archive_cost;
                Some(script.archive_result.clone())
            }
            _ => panic!("request outside RPC allowlist"),
        };
        let response_id = script.response_ids.get(method).unwrap_or(&value["id"]);
        let response = match data {
            Some(data) if !script.rpc_errors.contains(method) =>
                json!({"id":response_id,"result":data}),
            _ => json!({"id":response_id,"error":{"code":-32601,"message":SECRET,"data":SECRET}}),
        };
        script.incoming.push_back((cost, response));
        Ok(())
    }

    fn receive(&mut self) -> Result<Value, CodexError> {
        let (cost, value) = self.script.borrow_mut().incoming.pop_front().unwrap();
        self.clock.advance(cost);
        Ok(value)
    }

    fn close(&mut self) -> Result<(), CodexError> {
        let mut script = self.script.borrow_mut();
        script.closes += 1;
        self.clock.advance(script.close_cost);
        Ok(())
    }
}

fn poll(script: Script) -> (CodexObservation, Rc<RefCell<Script>>) {
    let clock = FakeClock::default();
    let mut connector = FakeConnector::new(&clock, script);
    let result = client().poll_with(NOW, &clock, &mut connector);
    (result, connector.script)
}

fn archive(script: Script, thread_id: &str, expected: &CodexStatus)
    -> (Result<ArchiveOutcome, CodexError>, Rc<RefCell<Script>>)
{
    let clock = FakeClock::default();
    let mut connector = FakeConnector::new(&clock, script);
    let result = client().archive_with(thread_id, expected, &clock, &mut connector);
    (result, connector.script)
}

fn fleet(rows: Vec<Value>, loaded: Vec<String>) -> Script {
    Script {
        loaded: BTreeMap::from([(String::new(),
            response_page(loaded.into_iter().map(Value::String).collect(), None))]),
        history: BTreeMap::from([(String::new(), response_page(rows, None))]),
        ..Script::default()
    }
}

fn assert_kind(observation: &CodexObservation, kind: CodexFailureKind) {
    assert!(!observation.complete);
    let diagnostic = observation.diagnostic.as_ref().unwrap();
    assert_eq!(diagnostic.kind, kind);
    assert!(!format!("{diagnostic:?}").contains(SECRET));
    assert!(diagnostic.message.chars().count() <= 120);
}

// ── Identity, mapping, and source branches ─────────────────────────────────

#[test]
fn full_thread_identity_and_tail_display_never_use_session_id() {
    let first = eligible(&thread(0x12345678));
    let second = eligible(&thread(0xABCDEF90));
    assert_eq!(first.session_id, id(0x12345678));
    assert_eq!(first.id.as_deref(), Some("12345678"));
    assert_eq!(second.id.as_deref(), Some("abcdef90"));
    assert_eq!(first.provider, Provider::Codex);
    assert_eq!(first.kind, Kind::Background);
    assert_eq!(first.pid, None);
    assert!(!first.has_worker());
    assert_eq!(first.started_at, NOW-100_000);
    assert_eq!(first.codex.unwrap().updated_at, NOW);
}

#[test]
fn name_fallback_and_control_stripping() {
    let mut row = thread(1);
    row["name"] = json!("\x1b[31mred\x1b[0m\x07\nname");
    assert_eq!(eligible(&row).name, "red name");
    row["name"] = json!("\x1b]0;title\x07  ");
    row["preview"] = json!("\n \n\x1b[32museful\x1b[0m\nlater");
    assert_eq!(eligible(&row).name, "useful");
    row.as_object_mut().unwrap().remove("name");
    assert_eq!(eligible(&row).name, "useful");
    row["preview"] = json!("\x1bPdiscard\x1b\\\n \t");
    assert_eq!(eligible(&row).name, "Codex 00000001");
    assert_eq!(clean_text("a\x1b(Bb\x00c\x7f\u{009f}"), "abc");
}

#[test]
fn all_status_rows_and_flags_map_without_worker_inference() {
    let cases = [
        (json!({"type":"notLoaded"}), Status::Idle, Some(State::Unloaded), Group::Codex, CodexStatus::NotLoaded),
        (json!({"type":"idle"}), Status::Idle, None, Group::Codex, CodexStatus::Idle),
        (json!({"type":"systemError"}), Status::Unknown("systemError".into()), None, Group::Codex, CodexStatus::SystemError),
        (json!({"type":"active","activeFlags":[]}), Status::Busy, Some(State::Working), Group::Codex,
            CodexStatus::Active { flags: vec![] }),
        (json!({"type":"active","activeFlags":["waitingOnApproval"]}),
            Status::Waiting, Some(State::Blocked), Group::Codex,
            CodexStatus::Active { flags: vec!["waitingOnApproval".into()] }),
        (json!({"type":"active","activeFlags":["waitingOnUserInput"]}),
            Status::Waiting, Some(State::Blocked), Group::Codex,
            CodexStatus::Active { flags: vec!["waitingOnUserInput".into()] }),
        (json!({"type":"active","activeFlags":["zFuture","waitingOnApproval","aFuture","zFuture"]}),
            Status::Waiting, Some(State::Blocked), Group::Codex,
            CodexStatus::Active { flags: vec!["aFuture".into(), "waitingOnApproval".into(), "zFuture".into()] }),
        (json!({"type":"active","activeFlags":["zFuture","aFuture","zFuture"]}),
            Status::Unknown("aFuture, zFuture".into()), Some(State::Working), Group::Codex,
            CodexStatus::Active { flags: vec!["aFuture".into(), "zFuture".into()] }),
        (json!({"type":"future"}), Status::Unknown("future".into()), None, Group::Codex,
            CodexStatus::Unknown("future".into())),
    ];
    for (runtime, status, state, group, expected) in cases {
        let mut row = thread(1);
        row["status"] = runtime;
        let parsed = eligible(&row);
        assert_eq!(parsed.status, status);
        assert_eq!(parsed.state, state);
        assert_eq!(parsed.group(), group);
        assert_eq!(parsed.codex.as_ref().unwrap().runtime, expected);
        assert!(!parsed.has_worker());
    }
    let mut row = thread(1);
    row["status"] = json!({"type":"active","activeFlags":["z","a","z"]});
    assert_eq!(eligible(&row).codex.unwrap().runtime,
        CodexStatus::Active { flags: vec!["a".into(), "z".into()] });
}

#[test]
fn malformed_eligible_fields_are_loss_not_idle() {
    for field in ["id", "cwd", "createdAt", "updatedAt", "ephemeral", "source", "status", "preview"] {
        let mut row = thread(1);
        row.as_object_mut().unwrap().remove(field);
        assert!(parse_thread(&row).is_err(), "{field}");
    }
    for (field, value) in [
        ("id",json!("short")), ("id",json!(4)), ("cwd",json!("relative")),
        ("cwd",json!(null)), ("createdAt",json!(i64::MAX)), ("updatedAt",json!(1.5)),
        ("ephemeral",json!("false")), ("name",json!([])),
        ("status",json!("idle")), ("status",json!({})),
        ("status",json!({"type":1})), ("status",json!({"type":"active"})),
        ("status",json!({"type":"active","activeFlags":null})),
        ("status",json!({"type":"active","activeFlags":[1]})),
    ] {
        let mut row = thread(1);
        row[field] = value;
        assert!(parse_thread(&row).is_err(), "{field}");
    }
}

#[test]
fn unknown_extra_fields_and_an_ordinary_fork_are_eligible() {
    let mut row = thread(1);
    row["forkedFromId"] = json!(id(2));
    row["futureField"] = json!({"ignored":true});
    row["status"]["futureField"] = json!(12);
    assert_eq!(eligible(&row).session_id, id(1));
}

#[test]
fn all_eligible_source_strings_are_accepted() {
    for source in ["cli","vscode","exec","appServer","unknown"] {
        let mut row = thread(1);
        row["source"] = json!(source);
        assert!(matches!(parse_thread(&row), Ok(Parsed::Eligible(_))));
    }
}

#[test]
fn every_sole_subagent_payload_and_string_custom_are_deliberate_exclusions() {
    for source in [
        json!({"subAgent":"memory_consolidation"}), json!({"subAgent":"other"}),
        json!({"subAgent":{"thread_spawn":{"parent_thread_id":id(8),"depth":1}}}),
        json!({"subAgent":"future"}), json!({"subAgent":null}), json!({"subAgent":3}),
        json!({"subAgent":[]}), json!({"subAgent":{"future":true}}), json!({"custom":"x"}),
    ] {
        let row = json!({"id":id(1),"source":source});
        assert!(matches!(parse_thread(&row), Ok(Parsed::Excluded(_))));
        let mut script = fleet(vec![], vec![id(1)]);
        script.reads.insert(id(1), row);
        let (result, _) = poll(script);
        assert!(result.complete);
        assert!(result.sessions.is_empty());
        assert!(result.source_drift.is_empty());
    }
}

#[test]
fn independent_exclusions_precede_unfamiliar_sources_in_both_halves() {
    for source in [json!("future"), json!({"custom":1}), Value::Null] {
        for field in ["ephemeral", "parentThreadId"] {
            let mut row = json!({"id":id(1),"source":source,"updatedAt":NOW/1000});
            row[field] = if field == "ephemeral" { json!(true) } else { json!(id(8)) };
            assert!(matches!(parse_thread(&row), Ok(Parsed::Excluded(_))));
            for history in [true, false] {
                let mut script = fleet(if history { vec![row.clone()] } else { vec![] }, vec![id(1)]);
                script.reads.insert(id(1), row.clone());
                let (result, _) = poll(script);
                assert!(result.complete);
                assert!(result.source_drift.is_empty());
                assert!(result.sessions.is_empty());
            }
        }
    }
    assert!(parse_thread(&json!({"id":"bad","ephemeral":true})).is_err());
}

#[test]
fn unrecognized_sources_remain_drift_loss_and_uncached() {
    for source in [
        json!("future"), json!(SECRET), json!(1), json!(null), json!({"custom":1}),
        json!({"custom":"x","extra":1}), json!({"subAgent":"review","extra":1}),
        json!({"different":"x"}),
    ] {
        let clock = FakeClock::default();
        let mut client = client();
        let mut row = thread(1);
        row["source"] = source;
        for _ in 0..2 {
            let mut script = fleet(vec![], vec![id(1)]);
            script.reads.insert(id(1), row.clone());
            let mut connector = FakeConnector::new(&clock, script);
            let result = client.poll_with(NOW, &clock, &mut connector);
            assert_kind(&result, CodexFailureKind::Incomplete);
            assert_eq!(result.source_drift.len(), 1);
            assert!(!format!("{:?}", result.source_drift).contains(SECRET));
            assert!(result.sessions.is_empty());
            assert_eq!(connector.script.borrow().read_ids, vec![id(1)]);
            assert!(client.exclusions.is_empty());
        }
    }
}

// ── Pagination, union, and partial observations ────────────────────────────

#[test]
fn loaded_cursors_history_cutoff_and_metadata_reuse_form_one_union() {
    let mut old = thread(2);
    old["updatedAt"] = json!(CUTOFF/1000-1);
    let mut equal = thread(3);
    equal["updatedAt"] = json!(CUTOFF/1000);
    let mut unloaded = thread(4);
    unloaded["status"] = json!({"type":"notLoaded"});
    unloaded["updatedAt"] = json!(CUTOFF/1000-2);
    let mut script = fleet(vec![thread(1), equal.clone()], vec![id(1)]);
    script.loaded.insert("".into(), response_page(vec![json!(id(1))], Some("loaded-2")));
    script.loaded.insert("loaded-2".into(), response_page(vec![json!(id(2)),json!(id(4))], None));
    script.history.insert("".into(), response_page(vec![thread(1),equal], Some("history-2")));
    script.history.insert("history-2".into(), response_page(vec![old], Some("must-not-fetch")));
    script.reads.insert(id(4), unloaded);
    let (result, script) = poll(script);
    assert!(result.complete);
    assert_eq!(result.cutoff_ms, CUTOFF);
    assert_eq!(result.sessions.iter().map(|s| s.session_id.clone()).collect::<Vec<_>>(),
        vec![id(1),id(2),id(3),id(4)]);
    assert_eq!(result.sessions[3].state, Some(State::Unloaded)); // membership isn't status
    assert_eq!(result.history_metadata_ids, BTreeSet::from([id(1),id(2),id(3)]));
    assert_eq!(script.borrow().read_ids, vec![id(4)]);
    assert!(!script.borrow().sent.iter().any(|r| r["params"]["cursor"] == "must-not-fetch"));
}

#[test]
fn an_old_unloaded_history_row_is_not_in_the_union() {
    let mut row = thread(1);
    row["updatedAt"] = json!(CUTOFF/1000-1);
    let (result, _) = poll(fleet(vec![row], vec![]));
    assert!(result.complete);
    assert!(result.sessions.is_empty());
    assert_eq!(result.history_metadata_ids, BTreeSet::from([id(1)]));
}

#[test]
fn duplicate_rows_coalesce_but_conflicting_rows_are_unusable() {
    let (same, _) = poll(fleet(vec![thread(1),thread(1)], vec![]));
    assert!(same.complete);
    assert_eq!(same.sessions.len(), 1);
    let mut changed = thread(1);
    changed["name"] = json!("different");
    let (conflict, _) = poll(fleet(vec![thread(1),thread(2),changed], vec![]));
    assert_kind(&conflict, CodexFailureKind::Incomplete);
    assert_eq!(conflict.sessions.len(), 1);
    assert_eq!(conflict.sessions[0].session_id, id(2));
}

#[test]
fn invalid_cursors_keep_valid_page_rows_but_cannot_prove_absence() {
    for cursor in [json!(""),json!(false),json!(12)] {
        let mut script = fleet(vec![thread(1)], vec![]);
        script.history.get_mut("").unwrap()["nextCursor"] = cursor;
        let (result, _) = poll(script);
        assert_kind(&result, CodexFailureKind::Incomplete);
        assert_eq!(result.sessions.len(), 1);
    }
}

#[test]
fn cursor_cycles_and_bad_loaded_ids_are_incomplete() {
    let mut script = fleet(vec![thread(1)], vec![]);
    script.loaded.insert("".into(), response_page(vec![json!("bad"),json!(id(1))], Some("again")));
    script.loaded.insert("again".into(), response_page(vec![json!(id(1))], Some("again")));
    let (result, _) = poll(script);
    assert_kind(&result, CodexFailureKind::Incomplete);
    assert_eq!(result.sessions.len(), 1);
    let mut script = fleet(vec![thread(1)], vec![]);
    script.history.insert("".into(), response_page(vec![thread(1)], Some("again")));
    script.history.insert("again".into(), response_page(vec![thread(1)], Some("again")));
    assert_kind(&poll(script).0, CodexFailureKind::Incomplete);
}

#[test]
fn order_violations_within_and_across_pages_prevent_cutoff_termination() {
    for split in [true, false] {
        let mut old = thread(1);
        old["updatedAt"] = json!(CUTOFF/1000-1);
        let mut script = fleet(vec![], vec![]);
        if split {
            // First page doesn't cross; second moves back above its timestamp.
            old["updatedAt"] = json!(CUTOFF/1000);
            script.history.insert("".into(), response_page(vec![old.clone()], Some("second")));
            old["id"] = json!(id(3));
            old["updatedAt"] = json!(CUTOFF/1000-1);
            script.history.insert("second".into(), response_page(vec![thread(2),old], Some("last")));
        } else {
            script.history.insert("".into(), response_page(vec![old,thread(2)], Some("last")));
        }
        script.history.insert("last".into(), response_page(vec![], None));
        let (result, script) = poll(script);
        assert_kind(&result, CodexFailureKind::Incomplete);
        assert!(script.borrow().sent.iter().any(|r| r["params"]["cursor"] == "last"));
    }
}

#[test]
fn parse_loss_cannot_authorize_early_cutoff_or_hide_good_rows() {
    let mut bad = thread(1);
    bad["updatedAt"] = json!(CUTOFF/1000-1);
    bad.as_object_mut().unwrap().remove("cwd");
    let mut script = fleet(vec![], vec![]);
    script.history.insert("".into(), response_page(vec![bad], Some("last")));
    script.history.insert("last".into(), response_page(vec![], None));
    let (result, script) = poll(script);
    assert_kind(&result, CodexFailureKind::Incomplete);
    assert!(script.borrow().sent.iter().any(|r| r["params"]["cursor"] == "last"));
}

#[test]
fn excluded_rows_without_timestamps_do_not_authorize_a_cutoff() {
    let mut old = thread(2);
    old["updatedAt"] = json!(CUTOFF/1000-1);
    let mut script = fleet(vec![], vec![]);
    script.history.insert("".into(), response_page(
        vec![json!({"id":id(1),"source":{"custom":"x"}}),old], Some("last")));
    script.history.insert("last".into(), response_page(vec![], None));
    let (result, script) = poll(script);
    assert!(result.complete);
    assert!(script.borrow().sent.iter().any(|r| r["params"]["cursor"] == "last"));
}

#[test]
fn failed_or_mismatched_reads_never_supply_coverage() {
    let mut script = fleet(vec![thread(1)], vec![id(2),id(3)]);
    script.reads.insert(id(2), thread(999)); // wrong full address in response
    let (result, script) = poll(script);
    assert!(!result.complete);
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(script.borrow().read_ids, vec![id(2),id(3)]);
}

#[test]
fn expiry_mid_page_retains_only_already_validated_rows() {
    let clock = FakeClock::default();
    clock.step.set(Duration::from_millis(10));
    let script = fleet((1..=100).map(thread).collect(), vec![]);
    let mut connector = FakeConnector::new(&clock, script);
    let result = client().poll_with(NOW, &clock, &mut connector);
    assert_kind(&result, CodexFailureKind::Timeout);
    assert!(!result.sessions.is_empty());
    assert!(result.sessions.len() < 100);
    assert_eq!(connector.script.borrow().closes, 0);
}

// ── Fair reads, immutable coverage, and fingerprint provenance ────────────

fn excluded_fleet(count: u64) -> Script {
    let mut script = fleet(vec![], (1..=count).map(id).collect());
    for n in 1..=count {
        let mut row = json!({"id":id(n)});
        match n % 4 {
            0 => row["ephemeral"] = json!(true),
            1 => row["parentThreadId"] = json!(id(100)),
            2 => row["source"] = json!({"subAgent":{"future":[]}}),
            _ => row["source"] = json!({"custom":"fixture"}),
        }
        script.reads.insert(id(n), row);
    }
    script
}

#[test]
fn exclusions_converge_in_ceil_n_over_k_attempts_and_survive_reload() {
    let clock = FakeClock::default();
    let mut client = client();
    let mut covered = BTreeSet::new();
    for _ in 0..3 { // n=6, k=2
        let mut script = excluded_fleet(6);
        script.read_cost = Duration::from_millis(400);
        let mut connector = FakeConnector::new(&clock, script);
        let result = client.poll_with(NOW, &clock, &mut connector);
        covered.extend(client.exclusions.keys().cloned());
        assert!(result.sessions.is_empty());
        let before = client.attempted.clone();
        client.replace_prepared(super::tests::client());
        assert_eq!(client.attempted, before);
    }
    assert_eq!(covered.len(), 6);
    let mut connector = FakeConnector::new(&clock, excluded_fleet(6));
    let result = client.poll_with(NOW, &clock, &mut connector);
    assert!(result.complete);
    assert!(connector.script.borrow().read_ids.is_empty());
}

#[test]
fn eligible_reads_rotate_but_never_reuse_old_metadata_as_coverage() {
    let clock = FakeClock::default();
    let mut client = client();
    let mut observed = BTreeSet::new();
    for _ in 0..3 {
        let mut script = fleet(vec![], (1..=5).map(id).collect());
        script.reads = (1..=5).map(|n| (id(n),thread(n))).collect();
        script.read_cost = Duration::from_millis(400);
        let mut connector = FakeConnector::new(&clock, script);
        let result = client.poll_with(NOW, &clock, &mut connector);
        assert_kind(&result, CodexFailureKind::Timeout);
        assert_eq!(result.sessions.len(), 2);
        observed.extend(result.sessions.into_iter().map(|r| r.session_id));
        assert!(client.exclusions.is_empty());
    }
    assert_eq!(observed.len(), 5);
}

#[test]
fn read_errors_advance_order_and_new_ids_go_first() {
    let clock = FakeClock::default();
    let mut client = client();
    let mut first = FakeConnector::new(&clock, fleet(vec![], vec![id(1),id(2)]));
    assert!(!client.poll_with(NOW, &clock, &mut first).complete);
    assert!(client.attempted[&id(1)] < client.attempted[&id(2)]);
    let mut second = FakeConnector::new(&clock, excluded_fleet(3));
    client.poll_with(NOW, &clock, &mut second);
    assert_eq!(second.script.borrow().read_ids, vec![id(3),id(1),id(2)]);
}

#[test]
fn cache_eviction_requires_a_complete_loaded_census_and_reentry_reads_again() {
    let clock = FakeClock::default();
    let mut client = client();
    let mut connector = FakeConnector::new(&clock, excluded_fleet(1));
    assert!(client.poll_with(NOW, &clock, &mut connector).complete);
    for bad in [json!("bad"),json!(null)] {
        let mut script = Script::default();
        script.loaded.insert("".into(), response_page(vec![bad], None));
        let mut connector = FakeConnector::new(&clock, script);
        assert!(!client.poll_with(NOW, &clock, &mut connector).complete);
        assert!(client.exclusions.contains_key(&id(1)));
        assert!(client.attempted.contains_key(&id(1)));
    }
    let mut connector = FakeConnector::new(&clock, Script::default());
    assert!(client.poll_with(NOW, &clock, &mut connector).complete);
    assert!(client.exclusions.is_empty());
    assert!(client.attempted.is_empty());
    let mut connector = FakeConnector::new(&clock, excluded_fleet(1));
    assert!(client.poll_with(NOW, &clock, &mut connector).complete);
    assert_eq!(connector.script.borrow().read_ids, vec![id(1)]);
}

#[test]
fn contradictory_fresh_metadata_invalidates_cached_exclusion() {
    let clock = FakeClock::default();
    let mut client = client();
    let mut connector = FakeConnector::new(&clock, excluded_fleet(4));
    assert!(client.poll_with(NOW, &clock, &mut connector).complete);
    let mut connector = FakeConnector::new(&clock, fleet((1..=4).map(thread).collect(), (1..=4).map(id).collect()));
    let result = client.poll_with(NOW, &clock, &mut connector);
    assert_kind(&result, CodexFailureKind::Incomplete);
    assert_eq!(result.sessions.len(), 4);
    assert!(client.exclusions.is_empty());
}

#[test]
fn unknown_nested_subagent_and_independently_excluded_source_changes_are_not_conflicts() {
    let clock = FakeClock::default();
    let mut client = client();
    let mut connector = FakeConnector::new(&clock, excluded_fleet(4));
    assert!(client.poll_with(NOW, &clock, &mut connector).complete);
    let rows = vec![
        json!({"id":id(2),"source":{"subAgent":null},"updatedAt":NOW/1000}),
        json!({"id":id(4),"ephemeral":true,"source":"future","updatedAt":NOW/1000}),
    ];
    let mut connector = FakeConnector::new(&clock, fleet(rows, (1..=4).map(id).collect()));
    assert!(client.poll_with(NOW, &clock, &mut connector).complete);
}

#[test]
fn timestamp_free_projection_and_history_provenance_allow_a_stable_start_anchor() {
    let mut first = thread(1);
    first["status"] = json!({"type":"active","activeFlags":["z","a"]});
    let a = eligible(&first);
    first["createdAt"] = json!(NOW/1000);
    first["updatedAt"] = json!(NOW/1000+10);
    first["status"]["activeFlags"] = json!(["a","z","a"]);
    let b = eligible(&first);
    assert_eq!(fingerprint(std::slice::from_ref(&a)), fingerprint(std::slice::from_ref(&b)));
    assert_ne!(a.started_at, b.started_at);
    assert_eq!(fingerprint(&[a.clone(),eligible(&thread(2))]),
        fingerprint(&[eligible(&thread(2)),b.clone()]));
    for field in ["name","cwd","status"] {
        let mut changed = first.clone();
        changed[field] = match field {
            "name" => json!("new name"), "cwd" => json!("/elsewhere"),
            _ => json!({"type":"idle"}),
        };
        assert_ne!(fingerprint(std::slice::from_ref(&b)), fingerprint(&[eligible(&changed)]));
    }
    let mut script = fleet(vec![], vec![id(1)]);
    script.reads.insert(id(1), first.clone());
    let (fallback, _) = poll(script);
    assert!(fallback.history_metadata_ids.is_empty());
    assert_eq!(fallback.sessions[0].started_at, b.started_at);
    let (history, _) = poll(fleet(vec![first], vec![id(1)]));
    assert_eq!(history.history_metadata_ids, BTreeSet::from([id(1)]));
}

// ── RPC allowlist, dispatch, errors, and the shared budget ──────────────────

#[test]
fn initialize_order_and_exact_read_only_method_parameters() {
    let mut script = fleet(vec![], vec![id(1)]);
    script.reads.insert(id(1), thread(1));
    let (result, script) = poll(script);
    assert!(result.complete);
    assert_eq!(script.borrow().sent, vec![
        json!({"id":0,"method":"initialize","params":{
            "clientInfo":{"name":"ccmux","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":true}}}),
        json!({"method":"initialized","params":{}}),
        json!({"id":"ccmux-1","method":"thread/loaded/list","params":{"limit":100,"cursor":null}}),
        json!({"id":"ccmux-2","method":"thread/list","params":{
            "limit":100,"cursor":null,"sortKey":"updated_at","sortDirection":"desc",
            "archived":false,"sourceKinds":["cli","vscode","exec","appServer","unknown"],
            "modelProviders":[],"useStateDbOnly":true}}),
        json!({"id":"ccmux-3","method":"thread/read","params":{"threadId":id(1),"includeTurns":false}}),
    ]);
}

#[test]
fn archive_uses_one_fresh_read_and_exactly_one_mutation() {
    for (status, expected) in [(json!({"type":"idle"}), CodexStatus::Idle),
        (json!({"type":"notLoaded"}), CodexStatus::NotLoaded)]
    {
        let mut row = thread(1);
        row["status"] = status;
        let mut script = Script::default();
        script.reads.insert(id(1), row);
        let (result, script) = archive(script, &id(1), &expected);
        assert_eq!(result.unwrap(), ArchiveOutcome::Archived { updated_at: NOW / 1000 * 1000 });
        let script = script.borrow();
        assert_eq!(script.archive_ids, vec![id(1)]);
        assert_eq!(script.closes, 1);
        assert_eq!(script.sent, vec![
            json!({"id":0,"method":"initialize","params":{
                "clientInfo":{"name":"ccmux","version":env!("CARGO_PKG_VERSION")},
                "capabilities":{"experimentalApi":true}}}),
            json!({"method":"initialized","params":{}}),
            json!({"id":"ccmux-1","method":"thread/read",
                "params":{"threadId":id(1),"includeTurns":false}}),
            json!({"id":"ccmux-2","method":"thread/archive",
                "params":{"threadId":id(1)}}),
        ]);
    }
}

#[test]
fn every_non_idle_fresh_status_refuses_before_archive() {
    for status in [
        json!({"type":"active","activeFlags":[]}),
        json!({"type":"active","activeFlags":["waitingOnApproval"]}),
        json!({"type":"active","activeFlags":["waitingOnUserInput"]}),
        json!({"type":"active","activeFlags":["futureFlag"]}),
        json!({"type":"systemError"}),
        json!({"type":"future"}),
        json!({}),
    ] {
        for expected in [CodexStatus::Idle, CodexStatus::NotLoaded] {
            let mut row = thread(1);
            row["status"] = status.clone();
            let mut script = Script::default();
            script.reads.insert(id(1), row);
            let (result, script) = archive(script, &id(1), &expected);
            assert_eq!(result.unwrap(), ArchiveOutcome::StateChanged, "{status}");
            assert!(script.borrow().archive_ids.is_empty(), "{status}");
            assert_eq!(script.borrow().closes, 1);
        }
    }
}

#[test]
fn archive_errors_timeouts_and_invalid_success_are_never_success() {
    let mut rpc_error = Script::default();
    rpc_error.reads.insert(id(1), thread(1));
    rpc_error.rpc_errors.insert("thread/archive".into());
    let (result, script) = archive(rpc_error, &id(1), &CodexStatus::Idle);
    assert_eq!(result.unwrap_err().diagnostic.kind, CodexFailureKind::Protocol);
    assert_eq!(script.borrow().archive_ids, vec![id(1)]);

    let mut timeout = Script::default();
    timeout.reads.insert(id(1), thread(1));
    timeout.archive_cost = POLL_TIMEOUT;
    let (result, script) = archive(timeout, &id(1), &CodexStatus::Idle);
    assert_eq!(result.unwrap_err().diagnostic.kind, CodexFailureKind::Timeout);
    assert_eq!(script.borrow().archive_ids, vec![id(1)]);

    let mut malformed = Script::default();
    malformed.reads.insert(id(1), thread(1));
    malformed.archive_result = json!({"unexpected":true});
    assert_eq!(archive(malformed, &id(1), &CodexStatus::Idle).0.unwrap_err().diagnostic.kind,
        CodexFailureKind::Protocol);
}

#[test]
fn archive_connection_ignores_requests_and_broadcast_notifications() {
    let mut script = Script::default();
    script.reads.insert(id(1), thread(1));
    script.ahead = vec![
        json!({"id":"approval","method":"item/commandExecution/requestApproval",
            "params":{"command":SECRET}}),
        json!({"method":"thread/archived","params":{"threadId":id(1)}}),
        json!({"method":"thread/status/changed","params":{"threadId":id(1),
            "status":{"type":"notLoaded"}}}),
    ];
    let (result, script) = archive(script, &id(1), &CodexStatus::Idle);
    assert!(matches!(result.unwrap(), ArchiveOutcome::Archived { .. }));
    let sent = &script.borrow().sent;
    assert_eq!(sent.len(), 4);
    assert!(sent.iter().all(|value| value.get("method").is_some()));
    assert!(!serde_json::to_string(sent).unwrap().contains(SECRET));
}

#[test]
fn archive_requires_the_fresh_status_to_match_the_row() {
    // notLoaded -> idle is a client attaching outside ccmux since the poll;
    // idle -> notLoaded is the server's idle unload. Both refuse.
    for (fresh, row_showed) in [("idle", CodexStatus::NotLoaded), ("notLoaded", CodexStatus::Idle)] {
        let mut row = thread(1);
        row["status"] = json!({"type":fresh});
        let mut script = Script::default();
        script.reads.insert(id(1), row);
        let (result, script) = archive(script, &id(1), &row_showed);
        assert_eq!(result.unwrap(), ArchiveOutcome::StateChanged, "{fresh}");
        let script = script.borrow();
        assert_eq!(script.read_ids, vec![id(1)]);
        assert!(script.archive_ids.is_empty(), "{fresh}: archive sent against a changed row");
        assert_eq!(script.closes, 1);
    }
}

#[test]
fn a_fresh_read_of_another_thread_is_a_state_change_not_an_archive() {
    let mut script = Script::default();
    script.reads.insert(id(1), thread(2));
    let (result, script) = archive(script, &id(1), &CodexStatus::Idle);
    assert_eq!(result.unwrap(), ArchiveOutcome::StateChanged);
    assert!(script.borrow().archive_ids.is_empty());
}

#[test]
fn a_fresh_read_without_updated_at_is_never_archived() {
    let mut row = thread(1);
    row.as_object_mut().unwrap().remove("updatedAt");
    let mut script = Script::default();
    script.reads.insert(id(1), row);
    let (result, script) = archive(script, &id(1), &CodexStatus::Idle);
    assert_eq!(result.unwrap_err().diagnostic.kind, CodexFailureKind::Protocol);
    assert!(script.borrow().archive_ids.is_empty());
}

#[test]
fn the_fresh_read_and_the_archive_share_one_deadline() {
    // Each half fits a fresh 1000 ms budget; together they do not.
    let mut split = Script::default();
    split.reads.insert(id(1), thread(1));
    split.read_cost = Duration::from_millis(600);
    split.archive_cost = Duration::from_millis(600);
    let (result, script) = archive(split, &id(1), &CodexStatus::Idle);
    assert_eq!(result.unwrap_err().diagnostic.kind, CodexFailureKind::Timeout);
    assert_eq!(script.borrow().archive_ids, vec![id(1)], "the late answer is never success");

    // A read that uses the budget up, or a slow connect plus a read that
    // fits alone, never sends the archive.
    for (connect, read) in [(0, 1000), (500, 600)] {
        let mut slow = Script::default();
        slow.reads.insert(id(1), thread(1));
        slow.connect_cost = Duration::from_millis(connect);
        slow.read_cost = Duration::from_millis(read);
        let (result, script) = archive(slow, &id(1), &CodexStatus::Idle);
        assert_eq!(result.unwrap_err().diagnostic.kind, CodexFailureKind::Timeout);
        assert!(script.borrow().archive_ids.is_empty(), "archive sent after the deadline");
    }
}

#[test]
fn archive_rejects_invalid_addresses_without_connecting() {
    let clock = FakeClock::default();
    let mut connector = FakeConnector::new(&clock, Script::default());
    let error = client().archive_with("not-a-thread", &CodexStatus::Idle, &clock, &mut connector)
        .unwrap_err();
    assert_eq!(error.diagnostic.kind, CodexFailureKind::Protocol);
    assert!(connector.script.borrow().sent.is_empty());
}

#[test]
fn forgetting_an_archived_id_clears_only_its_poll_bookkeeping() {
    let mut client = client();
    client.exclusions.insert(id(1), Exclusion::Custom);
    client.exclusions.insert(id(2), Exclusion::SubAgent);
    client.attempted.insert(id(1), 10);
    client.attempted.insert(id(2), 20);

    client.forget(&id(1));

    assert!(!client.exclusions.contains_key(&id(1)));
    assert!(!client.attempted.contains_key(&id(1)));
    assert_eq!(client.exclusions.get(&id(2)), Some(&Exclusion::SubAgent));
    assert_eq!(client.attempted.get(&id(2)), Some(&20));
}

#[test]
fn server_requests_and_notifications_get_no_reply_and_do_not_degrade() {
    let mut script = fleet(vec![thread(1)], vec![]);
    script.ahead = vec![
        json!({"id":0,"method":"item/commandExecution/requestApproval","params":{"command":SECRET}}),
        json!({"id":"tool-id","method":"item/tool/call","params":{"name":"must-not-run"}}),
        json!({"id":"auth-id","method":"account/chatgptAuthTokens/refresh","params":{}}),
        json!({"id":null,"method":"future/request","params":{}}),
        json!({"method":"thread/started","params":{"thread":thread(999)}}),
    ];
    let (result, script) = poll(script);
    assert!(result.complete);
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(script.borrow().sent.len(), 4); // only our initialize/notification/lists
    assert!(script.borrow().sent.iter().all(|v| v.get("method").is_some()));
    assert!(!serde_json::to_string(&script.borrow().sent).unwrap().contains(SECRET));
}

#[test]
fn error_responses_are_not_data_and_do_not_trigger_fallbacks() {
    for method in ["initialize","thread/loaded/list","thread/list"] {
        let mut script = Script::default();
        script.rpc_errors.insert(method.into());
        let (result, script) = poll(script);
        assert_kind(&result, CodexFailureKind::Protocol);
        assert!(result.sessions.is_empty());
        assert_eq!(script.borrow().sent.last().unwrap()["method"], method);
        assert_eq!(script.borrow().closes, 0);
    }
}

#[test]
fn malformed_envelopes_and_unmatched_response_ids_are_protocol_errors() {
    for value in [
        json!([]), json!({}), json!({"method":null,"id":0}),
        json!({"id":999,"result":{"userAgent":"fixture"}}), json!({"id":"0","result":{"userAgent":"fixture"}}),
        json!({"id":0,"result":{},"error":{}}),
        json!({"id":0,"error":{"code":"bad","message":SECRET}}),
        json!({"id":0,"error":{"code":-32601}}),
    ] {
        let mut script = Script::default();
        script.ahead.push(value);
        let (result, script) = poll(script);
        assert_kind(&result, CodexFailureKind::Protocol);
        assert_eq!(script.borrow().sent.len(), 1);
    }
}

#[test]
fn initialize_identity_is_validated_but_not_echoed_in_diagnostics() {
    let clock = FakeClock::default();
    let mut client = client();
    let mut script = Script::default();
    script.ahead.push(json!({"id":0,"result":{"userAgent":format!("fixture {SECRET}\x1b[31m")}}));
    let mut connector = FakeConnector::new(&clock, script);
    let result = client.poll_with(NOW, &clock, &mut connector);
    assert_kind(&result, CodexFailureKind::Protocol);
    assert!(!result.diagnostic.unwrap().message.contains(SECRET));
}

#[test]
fn connect_and_each_rpc_phase_share_the_same_deadline() {
    let mut script = fleet(vec![thread(1)], vec![]);
    script.connect_cost = Duration::from_millis(200);
    script.send_cost = Duration::from_millis(50); // 4 sends = 200
    script.receive_cost = Duration::from_millis(100); // 3 responses = 300
    script.close_cost = Duration::from_millis(350); // total 1050
    let (result, script) = poll(script);
    assert_kind(&result, CodexFailureKind::Timeout);
    assert_eq!(result.sessions.len(), 1); // retain rows even when close exhausts budget
    assert_eq!(script.borrow().closes, 1);

    let mut script = Script { connect_cost: POLL_TIMEOUT, ..Script::default() };
    let (result, recorded) = poll(script);
    assert_kind(&result, CodexFailureKind::Timeout);
    assert!(recorded.borrow().sent.is_empty());
    assert_eq!(recorded.borrow().closes, 0);

    script = Script { send_cost: POLL_TIMEOUT, ..Script::default() };
    let (result, recorded) = poll(script);
    assert_kind(&result, CodexFailureKind::Timeout);
    assert_eq!(recorded.borrow().sent.len(), 1);
    assert_eq!(recorded.borrow().closes, 0);
}

#[test]
fn a_request_flood_expires_without_reply_or_close_bytes() {
    let script = Script {
        ahead: (0..1100).map(|id| json!({"method":"approval","id":id,"params":{}})).collect(),
        ..Script::default()
    };
    let clock = FakeClock::default();
    let mut connector = FakeConnector::new(&clock, script);
    let result = client().poll_with(NOW, &clock, &mut connector);
    assert_kind(&result, CodexFailureKind::Timeout);
    assert_eq!(clock.time.get(), POLL_TIMEOUT);
    assert_eq!(connector.script.borrow().sent.len(), 1);
    assert_eq!(connector.script.borrow().closes, 0);
}

#[test]
fn expired_deadlines_start_no_request_and_do_not_advance_read_attempts() {
    let clock = FakeClock::default();
    let mut connector = FakeConnector::new(&clock, Script::default());
    let prepared = client();
    let deadline = Deadline::new(&clock);
    let transport = connector.connect(&prepared.prepared, deadline).ok().unwrap();
    let mut rpc = Rpc { transport, deadline, next_id:0, attempted:false };
    clock.advance(POLL_TIMEOUT);
    assert_eq!(rpc.request(Method::Read(&id(1))).unwrap_err().diagnostic.kind, CodexFailureKind::Timeout);
    assert!(!rpc.attempted);
    assert!(connector.script.borrow().sent.is_empty());
}

#[test]
fn address_attempts_get_only_the_remaining_budget() {
    let clock = FakeClock::default();
    let deadline = Deadline::new(&clock);
    let addresses = ["127.0.0.1:8965","127.0.0.2:8965","[::1]:8965"]
        .map(|s| s.parse().unwrap());
    let mut budgets = Vec::new();
    let result: Result<(), _> = connect_addresses(&addresses, deadline, |_, remaining| {
        budgets.push(remaining);
        clock.advance(Duration::from_millis(600));
        Err(io::Error::other(SECRET))
    });
    assert_eq!(result.unwrap_err().diagnostic.kind, CodexFailureKind::Timeout);
    assert_eq!(budgets, vec![POLL_TIMEOUT, Duration::from_millis(400)]);
}

// ── Actual tungstenite over fake, fragmented IO ────────────────────────────

struct Wire {
    input: VecDeque<u8>,
    output: Rc<RefCell<Vec<u8>>>,
    timeouts: Rc<RefCell<Vec<(bool, Duration, Duration)>>>,
    clock: FakeClock,
    read_chunk: usize,
    write_chunk: usize,
    read_cost: Duration,
    write_cost: Duration,
    flush_cost: Duration,
    read_errors: VecDeque<io::ErrorKind>,
    write_errors: VecDeque<io::ErrorKind>,
    flush_errors: VecDeque<io::ErrorKind>,
}

impl Wire {
    fn new(clock: &FakeClock, bytes: Vec<u8>) -> Self {
        Self {
            input: bytes.into(), output: Rc::new(RefCell::new(Vec::new())),
            timeouts: Rc::new(RefCell::new(Vec::new())), clock: clock.clone(),
            read_chunk: usize::MAX, write_chunk: usize::MAX,
            read_cost: Duration::ZERO, write_cost: Duration::ZERO, flush_cost: Duration::ZERO,
            read_errors: VecDeque::new(), write_errors: VecDeque::new(), flush_errors: VecDeque::new(),
        }
    }
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.clock.advance(self.read_cost);
        if let Some(error) = self.read_errors.pop_front() { return Err(error.into()); }
        let len = buf.len().min(self.read_chunk).min(self.input.len());
        for byte in buf.iter_mut().take(len) { *byte = self.input.pop_front().unwrap(); }
        Ok(len)
    }
}

impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.clock.advance(self.write_cost);
        if let Some(error) = self.write_errors.pop_front() { return Err(error.into()); }
        let len = buf.len().min(self.write_chunk);
        self.output.borrow_mut().extend_from_slice(&buf[..len]);
        Ok(len)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.clock.advance(self.flush_cost);
        if let Some(error) = self.flush_errors.pop_front() { return Err(error.into()); }
        Ok(())
    }
}

impl SocketIo for Wire {
    fn read_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.timeouts.borrow_mut().push((false, self.clock.time.get(), timeout));
        Ok(())
    }
    fn write_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.timeouts.borrow_mut().push((true, self.clock.time.get(), timeout));
        Ok(())
    }
}

fn frame_bytes(text: &str) -> Vec<u8> {
    let mut socket = WebSocket::from_raw_socket(io::Cursor::new(Vec::new()),
        tungstenite::protocol::Role::Server, Some(websocket_config()));
    socket.send(Message::Text(text.to_owned().into())).unwrap();
    socket.get_ref().get_ref().clone()
}

fn raw_transport<'a>(wire: Wire, deadline: Deadline<'a>) -> WsTransport<'a, Wire> {
    let stream = TimedStream { inner: wire, deadline };
    WsTransport { socket: WebSocket::from_raw_socket(stream,
        tungstenite::protocol::Role::Client, Some(websocket_config())), deadline }
}

const UPGRADE: &str = "HTTP/1.1 101 Switching Protocols\r\n\
Connection: Upgrade\r\nUpgrade: websocket\r\n\
Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";

fn upgrade_request() -> tungstenite::handshake::client::Request {
    let mut request = "ws://127.0.0.1:8965/".into_client_request().unwrap();
    request.headers_mut().insert("Sec-WebSocket-Key",
        HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="));
    let mut auth = HeaderValue::from_str(&format!("Bearer {SECRET}")).unwrap();
    auth.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, auth);
    request
}

#[test]
fn fragmented_upgrade_checks_deadline_inside_library_read_loop() {
    let clock = FakeClock::default();
    let deadline = Deadline::new(&clock);
    let mut wire = Wire::new(&clock, UPGRADE.as_bytes().to_vec());
    wire.read_chunk = 1;
    wire.read_cost = Duration::from_millis(40);
    let timeouts = wire.timeouts.clone();
    let stream = TimedStream { inner: wire, deadline };
    let error = match client_with_config(upgrade_request(), stream, Some(websocket_config())) {
        Err(HandshakeError::Failure(error)) => ws_error(error),
        _ => panic!("fragmented handshake should exhaust the deadline"),
    };
    assert_eq!(error.diagnostic.kind, CodexFailureKind::Timeout);
    assert!(!format!("{error:?} {error}").contains(SECRET));
    assert_eq!(clock.time.get(), POLL_TIMEOUT);
    assert_eq!(timeouts.borrow().last().unwrap().2, Duration::from_millis(40));
}

#[test]
fn upgrade_frame_decode_and_partial_writes_do_not_renew_deadline() {
    let clock = FakeClock::default();
    let deadline = Deadline::new(&clock);
    clock.advance(Duration::from_millis(200)); // pretend TCP connect used this
    let mut bytes = UPGRADE.as_bytes().to_vec();
    bytes.extend(frame_bytes(&json!({"id":0,"result":{"padding":"x".repeat(100)}}).to_string()));
    let mut wire = Wire::new(&clock, bytes);
    wire.read_chunk = 8;
    wire.read_cost = Duration::from_millis(32);
    let timeouts = wire.timeouts.clone();
    let stream = TimedStream { inner: wire, deadline };
    let socket = match client_with_config(upgrade_request(), stream, Some(websocket_config())) {
        Ok((socket, _)) => socket,
        _ => panic!("upgrade should finish inside the budget"),
    };
    let mut transport = WsTransport { socket, deadline };
    assert_eq!(transport.receive().unwrap_err().diagnostic.kind, CodexFailureKind::Timeout);
    assert_eq!(clock.time.get(), POLL_TIMEOUT);
    assert!(timeouts.borrow().iter().any(|(write, _, _)| *write));
    for (_, at, timeout) in timeouts.borrow().iter() {
        assert_eq!(*timeout, (POLL_TIMEOUT - *at).min(IO_SLICE));
    }

    let clock = FakeClock::default();
    let deadline = Deadline::new(&clock);
    let mut wire = Wire::new(&clock, vec![]);
    wire.write_chunk = 1;
    wire.write_cost = Duration::from_millis(40);
    let timeouts = wire.timeouts.clone();
    let output = wire.output.clone();
    let mut transport = raw_transport(wire, deadline);
    assert_eq!(transport.send(json!({"method":"initialize","params":{}}))
        .unwrap_err().diagnostic.kind, CodexFailureKind::Timeout);
    let length = output.borrow().len();
    assert_eq!(length, 25);
    for (write, at, timeout) in timeouts.borrow().iter() {
        assert!(*write);
        assert_eq!(*timeout, (POLL_TIMEOUT - *at).min(IO_SLICE));
    }
    assert!(transport.close().is_err());
    assert_eq!(output.borrow().len(), length); // no close bytes after expiry
}

#[test]
fn fragmented_websocket_message_and_malformed_json_use_real_frame_decoder() {
    let clock = FakeClock::default();
    let deadline = Deadline::new(&clock);
    // Two unmasked server frames: non-final text "[", final continuation "]".
    let wire = Wire::new(&clock, vec![0x01,1,b'[',0x80,1,b']']);
    let mut transport = raw_transport(wire, deadline);
    assert_eq!(transport.receive().unwrap(), json!([]));

    let clock = FakeClock::default();
    let mut wire = Wire::new(&clock, vec![0x01,1,b'[',0x80,1,b']']);
    wire.read_chunk = 1;
    wire.read_cost = Duration::from_millis(200);
    let mut transport = raw_transport(wire, Deadline::new(&clock));
    assert_eq!(transport.receive().unwrap_err().diagnostic.kind, CodexFailureKind::Timeout);

    let clock = FakeClock::default();
    let wire = Wire::new(&clock, frame_bytes(&format!("not-json-{SECRET}")));
    let mut transport = raw_transport(wire, Deadline::new(&clock));
    let error = transport.receive().unwrap_err();
    assert_eq!(error.diagnostic.kind, CodexFailureKind::Protocol);
    assert!(!format!("{error} {error:?}").contains(SECRET));
}

#[test]
fn frame_and_aggregate_message_sizes_are_bounded() {
    let clock = FakeClock::default();
    let wire = Wire::new(&clock, frame_bytes(&"x".repeat(MAX_MESSAGE+1)));
    let mut transport = raw_transport(wire, Deadline::new(&clock));
    assert_eq!(transport.receive().unwrap_err().diagnostic.kind, CodexFailureKind::Protocol);
    let config = websocket_config();
    assert_eq!(config.max_frame_size, Some(MAX_MESSAGE));
    assert_eq!(config.max_message_size, Some(MAX_MESSAGE));

    use tungstenite::protocol::frame::{Frame, coding::{Data, OpCode}};
    let mut socket = WebSocket::from_raw_socket(io::Cursor::new(Vec::new()),
        tungstenite::protocol::Role::Server, Some(websocket_config()));
    for (kind, last) in [(Data::Text, false),(Data::Continue, true)] {
        socket.send(Message::Frame(Frame::message("x".repeat(MAX_MESSAGE/2+1),
            OpCode::Data(kind), last))).unwrap();
    }
    let wire = Wire::new(&clock, socket.get_ref().get_ref().clone());
    let mut transport = raw_transport(wire, Deadline::new(&clock));
    assert_eq!(transport.receive().unwrap_err().diagnostic.kind, CodexFailureKind::Protocol);
}

#[test]
fn actual_wire_server_requests_produce_only_our_requests() {
    let clock = FakeClock::default();
    let deadline = Deadline::new(&clock);
    let messages = [
        json!({"id":0,"method":"item/commandExecution/requestApproval","params":{}}),
        json!({"id":"tool","method":"item/tool/call","params":{}}),
        json!({"id":"auth","method":"account/chatgptAuthTokens/refresh","params":{}}),
        json!({"id":0,"result":{"data":[]}}),
    ];
    let wire = Wire::new(&clock, messages.iter().flat_map(|m| frame_bytes(&m.to_string())).collect());
    let output = wire.output.clone();
    let mut rpc = Rpc { transport:Box::new(raw_transport(wire, deadline)),
        deadline, next_id:0, attempted:false };
    assert_eq!(rpc.request(Method::Loaded(None)).unwrap(), json!({"data":[]}));
    let mut reader = WebSocket::from_raw_socket(io::Cursor::new(output.borrow().clone()),
        tungstenite::protocol::Role::Server, Some(websocket_config()));
    let message = reader.read().unwrap().into_text().unwrap();
    let value: Value = serde_json::from_str(&message).unwrap();
    assert_eq!(value["method"], "thread/loaded/list");
    assert!(reader.read().is_err()); // no approval/tool/auth reply frame follows
}

#[test]
fn upgrade_auth_and_transport_errors_are_redacted_finite_categories() {
    for code in [401,403,404,500] {
        let response = tungstenite::http::Response::builder().status(code)
            .header("x-echo", SECRET).body(Some(SECRET.as_bytes().to_vec())).unwrap();
        let error = ws_error(tungstenite::Error::Http(Box::new(response)));
        let expected = if code == 401 || code == 403 {
            CodexFailureKind::Authentication
        } else { CodexFailureKind::Protocol };
        assert_eq!(error.diagnostic.kind, expected);
        assert!(!format!("{error:?} {error} {:?}",error.diagnostic()).contains(SECRET));
    }
    let error = io_error(io::Error::other(SECRET));
    assert_eq!(error.diagnostic.kind, CodexFailureKind::Connection);
    assert!(!format!("{error:?} {error}").contains(SECRET));
    // tungstenite logs raw handshake bytes and server frame text internally.
    assert_eq!(log::STATIC_MAX_LEVEL, log::LevelFilter::Off);
    assert!(!format!("{:?}",upgrade_request().headers()).contains(SECRET));
}

// ── Preparation is injectable and opens no connection ─────────────────────

#[test]
fn url_validation_accepts_only_cli_supported_loopback_forms() {
    for url in [
        "ws://localhost:8965", "ws://LOCALHOST:8965", "ws://127.0.0.2:8965",
        "ws://127.255.255.254", "ws://[::1]:8965", "ws://localhost/path",
    ] {
        assert!(validate_url(url).is_ok(), "{url}");
    }
    for url in [
        "ws://192.168.1.1:8965", "ws://100.64.0.1:8965", "ws://host:8965",
        "ws://localhost.:8965", "wss://localhost:8965", "http://localhost:8965",
        "ws://user@localhost:8965", "ws://localhost:8965?token=x",
        "ws://localhost:8965#fragment", "ws://[::2]:8965", "ws://[::ffff:127.0.0.1]:8965",
        "ws://2130706433:8965", "ws://127.1:8965", "ws://localhost:",
        "ws://[127.0.0.1]:8965", "ws://[localhost]:8965", "ws://localhost:+8965",
        "ws://[127.0.0.1]:+8965", "ws://localhost:89x5",
        "ws://localhost:65536", "ws://localhost:0", "ws://local%68ost:8965",
    ] {
        let error = validate_url(url).unwrap_err();
        assert_eq!(error.diagnostic.kind, CodexFailureKind::Configuration, "{url}");
        assert_eq!(error.to_string(), URL_ERROR);
    }
}

#[test]
fn invalid_url_is_rejected_before_dns_or_credential_io() {
    let config = config(&format!("ws://{SECRET}@localhost:8965"));
    let result = prepare_with(&config, |_, _| panic!("must not resolve"), |_| panic!("must not read"));
    let error = match result { Err(error) => error, _ => panic!("invalid URL accepted") };
    assert!(!format!("{error} {error:?}").contains(SECRET));
}

#[test]
fn localhost_resolution_is_checked_before_token_read() {
    for addresses in [
        vec![], vec!["127.0.0.1:8965".parse().unwrap(), "192.168.1.2:8965".parse().unwrap()],
    ] {
        let result = prepare_with(&config("ws://localhost:8965"), |_, _| Ok(addresses),
            |_| panic!("must reject before credential IO"));
        let error = match result { Err(error) => error, _ => panic!("bad DNS accepted") };
        assert_eq!(error.diagnostic.kind, CodexFailureKind::Configuration);
    }
    let client = prepare_with(&config("ws://LOCALHOST:8965"), |host, port| {
        assert!(host.eq_ignore_ascii_case("localhost"));
        assert_eq!(port, 8965);
        Ok(vec!["127.0.0.1:8965".parse().unwrap(),"[::1]:8965".parse().unwrap()])
    }, |_| Ok(SECRET.into())).ok().unwrap();
    assert_eq!(client.prepared.addresses.len(), 2);
}

#[test]
fn failed_reload_leaves_the_old_prepared_client_usable() {
    let clock = FakeClock::default();
    let mut existing = client();
    let replacement = prepare_with(&config("ws://localhost:8965"),
        |_, _| Ok(vec!["192.168.0.1:8965".parse().unwrap()]),
        |_| panic!("bad reload must not read new token"));
    assert!(replacement.is_err());
    assert_eq!(existing.prepared.addresses, vec!["127.0.0.1:8965".parse().unwrap()]);
    let mut connector = FakeConnector::new(&clock, Script::default());
    assert!(existing.poll_with(NOW, &clock, &mut connector).complete);
    // The future app owns the separate attach-refusal latch, not poll().
}

#[test]
fn credential_and_resolver_failures_never_echo_io_text() {
    let result = prepare_with(&config("ws://localhost:8965"),
        |_, _| Err(io::Error::other(SECRET)), |_| panic!("must not read after failed DNS"));
    let error = match result { Err(error) => error, _ => panic!("failed DNS accepted") };
    assert_eq!(error.diagnostic.kind, CodexFailureKind::Connection);
    assert!(!format!("{error:?} {error}").contains(SECRET));

    for token in [
        String::new(), "\n\n".into(), format!("{SECRET}\r\n"),
        format!("{SECRET}\nsecond"), format!("{SECRET}\0"), "x".repeat(MAX_TOKEN as usize+1),
        format!("{SECRET}{}", "\n".repeat(MAX_TOKEN as usize)),
    ] {
        let result = prepare_with(&config("ws://127.0.0.1:8965"),
            |_, _| panic!("literal"), |_| Ok(token));
        let error = match result { Err(error) => error, _ => panic!("invalid token accepted") };
        assert_eq!(error.diagnostic.kind, CodexFailureKind::Credential);
        assert!(!format!("{error:?} {error}").contains(SECRET));
    }
    let result = prepare_with(&config("ws://127.0.0.1:8965"),
        |_, _| panic!("literal"), |_| Err(io::Error::other(SECRET)));
    let error = match result { Err(error) => error, _ => panic!("IO error accepted") };
    assert!(!format!("{error:?} {error}").contains(SECRET));
    assert_eq!(client().prepared.token, SECRET); // shell-compatible trailing LF removal
}

#[test]
fn validated_urls_build_real_upgrade_requests_without_dns_or_tls() {
    for url in ["ws://127.0.0.1:8965","ws://LOCALHOST:8965","ws://[::1]:8965"] {
        let endpoint = endpoint(url).ok().unwrap();
        let mut request = endpoint.uri.into_client_request().unwrap();
        request.headers_mut().insert("Sec-WebSocket-Key",
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="));
        request.headers_mut().insert(AUTHORIZATION, client().prepared.authorization);
        assert_eq!(request.uri().path(), "/");
        assert!(!format!("{request:?}").contains(SECRET));
        let clock = FakeClock::default();
        let deadline = Deadline::new(&clock);
        let wire = Wire::new(&clock, UPGRADE.as_bytes().to_vec());
        let output = wire.output.clone();
        let stream = TimedStream { inner: wire, deadline };
        assert!(client_with_config(request, stream, Some(websocket_config())).is_ok());
        let output = output.borrow();
        let text = std::str::from_utf8(&output).unwrap();
        assert!(text.starts_with("GET / HTTP/1.1\r\n"));
        assert_eq!(text.matches(SECRET).count(), 1);
        assert!(text.contains(&format!("authorization: Bearer {SECRET}\r\n")));
    }
}


// ── Review regressions: framing, fair reads and immutable cache coverage ────

struct WireConnector(Option<Wire>);

impl Connector for WireConnector {
    fn connect<'a>(&mut self, _: &Prepared, deadline: Deadline<'a>)
        -> Result<Box<dyn Transport + 'a>, CodexError>
    {
        Ok(Box::new(raw_transport(self.0.take().unwrap(), deadline)))
    }
}

fn empty_poll_wire(clock: &FakeClock) -> Wire {
    let replies = [
        json!({"id":0,"result":{"userAgent":"fixture"}}),
        json!({"id":"ccmux-1","result":response_page(vec![], None)}),
        json!({"id":"ccmux-2","result":response_page(vec![], None)}),
    ];
    Wire::new(clock, replies.iter().flat_map(|value| frame_bytes(&value.to_string())).collect())
}

#[test]
fn interrupted_and_slice_timed_out_io_is_retried_without_degrading_the_poll() {
    for kind in [io::ErrorKind::Interrupted, io::ErrorKind::WouldBlock, io::ErrorKind::TimedOut] {
        let clock = FakeClock::default();
        let mut wire = empty_poll_wire(&clock);
        wire.read_errors.push_back(kind);
        wire.write_errors.push_back(kind);
        wire.flush_errors.push_back(kind);
        wire.read_cost = Duration::from_millis(1);
        wire.write_cost = Duration::from_millis(1);
        wire.flush_cost = Duration::from_millis(1);
        let result = client().poll_with(NOW, &clock, &mut WireConnector(Some(wire)));
        assert!(result.complete, "{kind:?}: {:?}", result.diagnostic);
        assert!(clock.time.get() < POLL_TIMEOUT);
    }
}

#[test]
fn repeated_interruption_and_slice_timeouts_expire_the_original_deadline() {
    for kind in [io::ErrorKind::Interrupted, io::ErrorKind::WouldBlock, io::ErrorKind::TimedOut] {
        for operation in ["read", "write", "flush"] {
            let clock = FakeClock::default();
            let mut wire = empty_poll_wire(&clock);
            let errors = VecDeque::from(vec![kind; 20]);
            let cost = Duration::from_millis(100);
            match operation {
                "read" => { wire.read_errors = errors; wire.read_cost = cost; }
                "write" => { wire.write_errors = errors; wire.write_cost = cost; }
                "flush" => { wire.flush_errors = errors; wire.flush_cost = cost; }
                _ => unreachable!(),
            }
            let timeouts = wire.timeouts.clone();
            let result = client().poll_with(NOW, &clock, &mut WireConnector(Some(wire)));
            assert_kind(&result, CodexFailureKind::Timeout);
            assert_eq!(clock.time.get(), POLL_TIMEOUT, "{kind:?} {operation}");
            assert!(timeouts.borrow().len() >= 10, "every retry must re-arm");
            for (_, at, timeout) in timeouts.borrow().iter() {
                assert_eq!(*timeout, (POLL_TIMEOUT - *at).min(IO_SLICE));
            }
        }
    }
}

#[test]
fn write_and_flush_timeouts_use_the_remaining_budget_even_below_one_slice() {
    for flush in [false, true] {
        let clock = FakeClock::default();
        let deadline = Deadline::new(&clock);
        clock.advance(POLL_TIMEOUT - Duration::from_millis(35));
        let mut wire = Wire::new(&clock, vec![]);
        if flush {
            wire.flush_cost = Duration::from_millis(10);
            wire.flush_errors.push_back(io::ErrorKind::Interrupted);
        } else {
            wire.write_cost = Duration::from_millis(10);
            wire.write_errors.push_back(io::ErrorKind::Interrupted);
        }
        let timeouts = wire.timeouts.clone();
        let mut stream = TimedStream { inner: wire, deadline };
        if flush { stream.flush().unwrap(); } else { assert_eq!(stream.write(b"x").unwrap(), 1); }
        assert_eq!(*timeouts.borrow(), vec![
            (true, Duration::from_millis(965), Duration::from_millis(35)),
            (true, Duration::from_millis(975), Duration::from_millis(25)),
        ]);
    }
}

#[test]
fn otherwise_valid_responses_require_the_exact_id_and_id_type() {
    let clock = FakeClock::default();
    for wrong in [json!(999), json!("0"), json!("ccmux-0")] {
        let mut client = client();
        let mut script = Script::default();
        script.response_ids.insert("initialize".into(), wrong);
        let mut connector = FakeConnector::new(&clock, script);
        assert_kind(&client.poll_with(NOW, &clock, &mut connector), CodexFailureKind::Protocol);
        assert_eq!(connector.script.borrow().sent.len(), 1);
    }
    for wrong in [json!("ccmux-9"), json!(999), json!("1"), json!(null)] {
        let mut client = client();
        let mut first = FakeConnector::new(&clock, excluded_fleet(1));
        assert!(client.poll_with(NOW, &clock, &mut first).complete);
        let exclusions = client.exclusions.clone();
        let attempted = client.attempted.clone();
        let mut script = Script::default();
        script.response_ids.insert("thread/loaded/list".into(), wrong);
        let mut connector = FakeConnector::new(&clock, script);
        assert_kind(&client.poll_with(NOW, &clock, &mut connector), CodexFailureKind::Protocol);
        assert_eq!(client.exclusions, exclusions);
        assert_eq!(client.attempted, attempted);
    }
    let mut script = fleet(vec![], vec![id(1)]);
    script.reads.insert(id(1), thread(1));
    script.response_ids.insert("thread/read".into(), json!("wrong-read-id"));
    let (result, _) = poll(script);
    assert_kind(&result, CodexFailureKind::Protocol);
    assert!(result.sessions.is_empty());
}

#[test]
fn an_expired_read_moves_behind_other_loaded_threads() {
    let clock = FakeClock::default();
    let mut client = client();
    for attempt in 1..=3 {
        let mut script = fleet(vec![], vec![id(1), id(2)]);
        script.reads = (1..=2).map(|n| (id(n), thread(n))).collect();
        script.read_costs.insert(id(1), POLL_TIMEOUT);
        let mut connector = FakeConnector::new(&clock, script);
        let previous = client.attempted.get(&id(1)).copied().unwrap_or(0);
        let result = client.poll_with(NOW, &clock, &mut connector);
        assert_kind(&result, CodexFailureKind::Timeout);
        assert!(client.attempted[&id(1)] > previous);
        if attempt == 1 {
            assert_eq!(connector.script.borrow().read_ids, vec![id(1)]);
        } else {
            assert_eq!(connector.script.borrow().read_ids, vec![id(2), id(1)]);
            assert_eq!(result.sessions[0].session_id, id(2));
        }
    }
}

#[test]
fn reload_between_partial_polls_preserves_read_counter_and_fairness() {
    let clock = FakeClock::default();
    let mut client = client();
    let mut observed = BTreeSet::new();
    for _ in 0..3 {
        let mut script = fleet(vec![], (1..=3).map(id).collect());
        script.reads = (1..=3).map(|n| (id(n), thread(n))).collect();
        script.read_cost = Duration::from_millis(600);
        let mut connector = FakeConnector::new(&clock, script);
        let result = client.poll_with(NOW, &clock, &mut connector);
        assert_kind(&result, CodexFailureKind::Timeout);
        assert_eq!(result.sessions.len(), 1);
        observed.insert(result.sessions[0].session_id.clone());
        let before = (client.sequence, client.attempted.clone());
        client.replace_prepared(super::tests::client());
        assert_eq!((client.sequence, client.attempted.clone()), before);
    }
    assert_eq!(observed, (1..=3).map(id).collect());
}

#[test]
fn partial_loaded_cursors_preserve_unseen_exclusions_and_read_order() {
    let clock = FakeClock::default();
    for tail in [json!("p2"), json!(12), json!(""), json!(false)] {
        let mut client = client();
        let mut connector = FakeConnector::new(&clock, excluded_fleet(3));
        assert!(client.poll_with(NOW, &clock, &mut connector).complete);
        let before = (client.exclusions.clone(), client.attempted.clone());
        let mut script = Script::default();
        script.loaded.insert("".into(), response_page(vec![json!(id(1))], Some("p2")));
        script.loaded.insert("p2".into(), json!({"data":[id(2)], "nextCursor":tail}));
        let mut connector = FakeConnector::new(&clock, script);
        assert_kind(&client.poll_with(NOW, &clock, &mut connector), CodexFailureKind::Incomplete);
        assert_eq!((client.exclusions.clone(), client.attempted.clone()), before);
        let mut connector = FakeConnector::new(&clock, excluded_fleet(3));
        assert!(client.poll_with(NOW, &clock, &mut connector).complete);
        assert!(connector.script.borrow().read_ids.is_empty());
    }
}

#[test]
fn independent_exclusions_with_unfamiliar_sources_do_not_contradict_cached_source_exclusions() {
    let clock = FakeClock::default();
    for source in [json!("future"), json!({"custom":1}), json!({"subAgent":"x","extra":1})] {
        for parent in [false, true] {
            let mut client = client();
            let mut connector = FakeConnector::new(&clock, excluded_fleet(3));
            assert!(client.poll_with(NOW, &clock, &mut connector).complete);
            assert_eq!(client.exclusions[&id(2)], Exclusion::SubAgent);
            assert_eq!(client.exclusions[&id(3)], Exclusion::Custom);
            let rows = [2,3].into_iter().map(|n| {
                let mut row = json!({"id":id(n),"source":source,"updatedAt":NOW/1000});
                if parent { row["parentThreadId"] = json!(id(100)); }
                else { row["ephemeral"] = json!(true); }
                row
            }).collect();
            let mut connector = FakeConnector::new(&clock, fleet(rows, (1..=3).map(id).collect()));
            let result = client.poll_with(NOW, &clock, &mut connector);
            assert!(result.complete, "{:?}", result.diagnostic);
            assert!(result.source_drift.is_empty());
            for n in [2,3] { assert!(client.exclusions.contains_key(&id(n))); }
        }
    }
}

#[test]
fn fingerprint_changes_when_only_normalized_runtime_flags_change() {
    for (before, after) in [
        (json!(["waitingOnApproval"]), json!(["waitingOnApproval","futureFlag"])),
        (json!(["a, b"]), json!(["a","b"])),
    ] {
        let mut row = thread(1);
        row["status"] = json!({"type":"active","activeFlags":before});
        let before = eligible(&row);
        row["status"]["activeFlags"] = after;
        let after = eligible(&row);
        assert_eq!((&before.status, &before.state), (&after.status, &after.state));
        assert_ne!(fingerprint(&[before]), fingerprint(&[after]));
    }
}


// ── Production connector against test-owned loopback sockets only ──────────

fn loopback_client(address: SocketAddr) -> CodexClient {
    prepare_with(&config(&format!("ws://{address}")),
        |_, _| panic!("loopback fixture must not resolve"),
        |_| Ok(SECRET.into())).ok().unwrap()
}

fn local_server(
    serve: impl FnOnce(TcpStream) + Send + 'static,
) -> (SocketAddr, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let worker = std::thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                    serve(stream);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < end => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("test listener accept: {error}"),
            }
        }
    });
    (address, worker)
}

/// A broken deadline must fail the test instead of hanging the entire suite.
/// Server helpers also have bounded waits and own every socket they touch.
fn bounded_poll(mut client: CodexClient) -> (CodexClient, CodexObservation, Duration) {
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let start = Instant::now();
        let result = client.poll(NOW);
        let _ = send.send((client, result, start.elapsed()));
    });
    let result = receive.recv_timeout(Duration::from_secs(2)).expect("poll exceeded test watchdog");
    worker.join().unwrap();
    result
}

fn read_upgrade(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 8192, "unbounded test request head");
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).unwrap()
}

#[test]
fn production_connector_sends_auth_and_classifies_upgrade_denials() {
    for code in [401, 403] {
        let (address, server) = local_server(move |mut stream| {
            let head = read_upgrade(&mut stream);
            assert!(head.contains(&format!("authorization: Bearer {SECRET}\r\n")),
                "production connector omitted the fixture Authorization header");
            let reply = format!("HTTP/1.1 {code} Denied\r\nContent-Length: {}\r\n\r\n{SECRET}", SECRET.len());
            stream.write_all(reply.as_bytes()).unwrap();
        });
        let (_, result, _) = bounded_poll(loopback_client(address));
        server.join().unwrap();
        assert_kind(&result, CodexFailureKind::Authentication);
    }
}

#[test]
fn stalled_upgrade_and_rpc_obey_the_one_second_wall_budget() {
    for after_upgrade in [false, true] {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (address, server) = local_server(move |mut stream| {
            if after_upgrade {
                let mut socket = tungstenite::accept(stream).unwrap();
                assert!(socket.read().unwrap().is_text()); // initialize, never answered
                let _ = wait.recv_timeout(Duration::from_secs(3));
            } else {
                read_upgrade(&mut stream); // never finish the HTTP upgrade
                let _ = wait.recv_timeout(Duration::from_secs(3));
            }
        });
        let (_, result, elapsed) = bounded_poll(loopback_client(address));
        let _ = release.send(());
        server.join().unwrap();
        assert_kind(&result, CodexFailureKind::Timeout);
        assert!(elapsed >= POLL_TIMEOUT, "{elapsed:?}");
        // SO_RCVTIMEO wake-up and scheduler latency may land just after the deadline.
        assert!(elapsed < POLL_TIMEOUT + Duration::from_millis(100), "{elapsed:?}");
    }
}

#[test]
fn production_connector_enforces_frame_and_aggregate_message_limits() {
    use tungstenite::protocol::frame::{Frame, coding::{Data, OpCode}};
    for fragmented in [false, true] {
        let (address, server) = local_server(move |stream| {
            let mut socket = tungstenite::accept(stream).unwrap();
            let request: Value = serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "initialize");
            let reply = json!({"id":request["id"],"result":{"userAgent":"x".repeat(MAX_MESSAGE)}}).to_string();
            if fragmented {
                let split = reply.len()/2;
                for (text, opcode, last) in [(&reply[..split], Data::Text, false), (&reply[split..], Data::Continue, true)] {
                    // A correct client may close as soon as the oversized length is known.
                    if socket.send(Message::Frame(Frame::message(text.to_owned(), OpCode::Data(opcode), last))).is_err() {
                        break;
                    }
                }
            } else {
                let _ = socket.send(Message::Text(reply.into()));
            }
        });
        let (_, result, _) = bounded_poll(loopback_client(address));
        server.join().unwrap();
        assert_kind(&result, CodexFailureKind::Protocol);
    }
}

#[test]
fn production_connector_tries_the_next_prepared_address_after_refusal() {
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    // Keep the port reserved on .1; no suite listener binds .2. Dropping a
    // listener to fabricate refusal lets another parallel test reuse its port.
    let refused = SocketAddr::from(([127, 0, 0, 2], reserved.local_addr().unwrap().port()));
    let (address, server) = local_server(|mut stream| {
        read_upgrade(&mut stream);
        stream.write_all(b"HTTP/1.1 401 Denied\r\nContent-Length: 0\r\n\r\n").unwrap();
    });
    let mut client = loopback_client(address);
    client.prepared.addresses.insert(0, refused);
    let (_, result, _) = bounded_poll(client);
    server.join().unwrap();
    drop(reserved);
    assert_kind(&result, CodexFailureKind::Authentication);
}

#[cfg(target_os = "linux")]
#[test]
fn production_tcp_connect_charges_time_waiting_for_a_full_accept_queue() {
    use std::os::fd::AsRawFd;
    unsafe extern "C" { fn listen(fd: std::ffi::c_int, backlog: std::ffi::c_int) -> std::ffi::c_int; }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    // Re-listen on this test's own fd with a tiny backlog, not the host default.
    assert_eq!(unsafe { listen(listener.as_raw_fd(), 1) }, 0);
    let address = listener.local_addr().unwrap();
    let mut queued = Vec::new();
    let mut full = false;
    for _ in 0..8 {
        match TcpStream::connect_timeout(&address, Duration::from_millis(20)) {
            Ok(stream) => queued.push(stream),
            Err(error) if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => {
                full = true;
                break;
            }
            Err(error) => panic!("queue fixture failed: {error}"),
        }
    }
    assert!(full && !queued.is_empty(), "fixture did not saturate its accept queue");
    let (_, result, elapsed) = bounded_poll(loopback_client(address));
    drop(listener);
    drop(queued);
    assert_kind(&result, CodexFailureKind::Timeout);
    assert!(elapsed >= POLL_TIMEOUT && elapsed < POLL_TIMEOUT + Duration::from_millis(100), "{elapsed:?}");
}

// ── Real credential IO, exclusively in private temporary fixture dirs ─────

struct TokenDir(PathBuf);

impl TokenDir {
    fn new() -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/tmp");
        std::fs::create_dir_all(&root).unwrap();
        loop {
            let path = root.join(format!("ccmux-token-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture dir: {error}"),
            }
        }
    }

    fn config(&self, file: &str) -> CodexConfig {
        CodexConfig { token_file: self.0.join(file), ..config("ws://127.0.0.1:8965") }
    }
}

impl Drop for TokenDir {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

fn credential_error(result: Result<CodexClient, CodexError>) {
    let error = match result { Err(error) => error, Ok(_) => panic!("invalid fixture token accepted") };
    assert_eq!(error.diagnostic.kind, CodexFailureKind::Credential);
    assert!(!format!("{error} {error:?}").contains(SECRET));
}

#[test]
fn real_token_reader_rejects_invalid_files_and_trims_only_trailing_lf() {
    let dir = TokenDir::new();
    for value in [SECRET.to_owned(), format!("{SECRET}\n"), format!("{SECRET}\n\n")] {
        std::fs::write(dir.0.join("token"), value).unwrap();
        let client = prepare(&dir.config("token")).expect("regular fixture token accepted");
        assert_eq!(client.prepared.token, SECRET);
        assert!(!format!("{:?}", client.prepared.authorization).contains(SECRET));
    }
    std::os::unix::fs::symlink(dir.0.join("token"), dir.0.join("symlink")).unwrap();
    assert!(prepare(&dir.config("symlink")).is_ok());
    for value in [String::new(), "\n".into(), format!("{SECRET}\r\n"),
        format!("{SECRET}\r"), format!("{SECRET}\nsecond"), "x".repeat(MAX_TOKEN as usize + 1)]
    {
        std::fs::write(dir.0.join("token"), value).unwrap();
        credential_error(prepare(&dir.config("token")));
    }
    credential_error(prepare(&dir.config("")));
    credential_error(prepare(&dir.config("missing")));
}

#[test]
fn real_token_reader_rejects_fifo_and_symlink_without_waiting_for_a_writer() {
    let dir = TokenDir::new();
    assert!(std::process::Command::new("mkfifo").arg(dir.0.join("fifo")).status().unwrap().success());
    std::os::unix::fs::symlink(dir.0.join("fifo"), dir.0.join("symlink")).unwrap();
    for name in ["fifo", "symlink"] {
        let config = dir.config(name);
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || { let _ = send.send(prepare(&config)); });
        credential_error(receive.recv_timeout(Duration::from_secs(2)).expect("FIFO blocked credential preparation"));
        worker.join().unwrap();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn real_token_reader_bounds_sparse_file_reads_before_allocating_its_size() {
    // A child with a memory ceiling makes an unbounded-read regression fail
    // without allocating a GiB in the operator's test process.
    if std::env::var("CCMUX_TOKEN_READER_CHILD").as_deref() == Ok("1") {
        let config = CodexConfig {
            token_file: std::env::current_dir().unwrap().join("sparse-token"),
            ..config("ws://127.0.0.1:8965")
        };
        credential_error(prepare(&config));
        return;
    }
    let dir = TokenDir::new();
    File::create(dir.0.join("sparse-token")).unwrap().set_len(1024 * 1024 * 1024).unwrap();
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", "ulimit -c 0; ulimit -v 262144; exec \"$@\"", "ccmux-token-limit"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "codex::tests::real_token_reader_bounds_sparse_file_reads_before_allocating_its_size"])
        .env("CCMUX_TOKEN_READER_CHILD", "1").current_dir(&dir.0).output().unwrap();
    assert!(output.status.success(), "bounded reader failed: {}", String::from_utf8_lossy(&output.stderr));
}
