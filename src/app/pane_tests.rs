//! Key-to-tmux fixtures: no tmux process, agent subprocess, credential IO or RPC.
use super::*;
use crate::model::{CodexMeta, CodexStatus, State, Status};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

const ID: &str = "01a0a609-12a6-7000-8000-123456789abc";

fn row() -> Session {
    let mut row = model::parse_sessions(
        r#"[{"id":"display-only","sessionId":"fixture","name":"Codex task","cwd":"/remote/only"}]"#
    ).unwrap().sessions.remove(0);
    row.provider = Provider::Codex;
    row.session_id = ID.into();
    row.codex = Some(CodexMeta { runtime: CodexStatus::Idle, updated_at: 1 });
    row.status = Status::Idle;
    row
}

fn record(row: &Session) -> PaneEntry {
    PaneEntry { provider: row.provider, session_id: row.session_id.clone(),
        short_id: row.id.clone().unwrap_or_default(), name: row.name.clone(), opened_at: 1 }
}

fn pid(id: &str) -> PaneId { PaneId::parse(id).unwrap() }
fn wid(id: u32) -> WindowId { WindowId::parse(&format!("@{id}")).unwrap() }
fn key(a: &mut App, code: KeyCode) -> Action { a.on_key(KeyEvent::new(code, KeyModifiers::NONE)) }
fn press(a: &mut App, code: char) -> Action { key(a, KeyCode::Char(code)) }

struct Pane { id: String, window: u32, index: u32, latch: String }

struct Server {
    session: String,
    panes: Vec<Pane>,
    tabs: BTreeMap<u32, TabInfo>,
    calls: Vec<Vec<String>>,
    next: u32,
}

impl Server {
    fn new(session: &str) -> Self {
        Self {
            session: session.into(), panes: vec![Pane { id: "%1".into(), window: 1, index: 1, latch: String::new() }],
            tabs: BTreeMap::from([(1, TabInfo { window: wid(1), index: 1, sidebar: Some(pid("%1")),
                map: PaneMap::new(), hidden: HiddenLog::new() })]), calls: vec![], next: 10,
        }
    }

    fn add_pane(&mut self, window: u32) -> String {
        let id = format!("%{}", self.next);
        self.next += 1;
        let index = self.panes.iter().filter(|p| p.window == window).count() as u32 + 1;
        self.panes.push(Pane { id: id.clone(), window, index, latch: String::new() });
        id
    }

    fn command(&mut self, argv: &[String]) -> Result<String, TmuxError> {
        let args = if argv.first().is_some_and(|s| s == "-L") { &argv[2..] } else { argv };
        self.calls.push(args.to_vec());
        let target = || args.windows(2).find(|w| w[0] == "-t").map(|w| w[1].as_str()).unwrap();
        match args[0].as_str() {
            "list-panes" => {
                assert_eq!(target(), format!("={}:", self.session));
                Ok(self.panes.iter().map(|p| format!(
                    "{}\t{}\t{}\t0\t{}\t40\t0\t{}\t@{}\t1\t1\t1\t0\t{}",
                    p.id, p.index, if p.index == 1 { 0 } else { 35 }, if p.index == 1 { 34 } else { 60 },
                    p.window, p.window, p.latch)).collect::<Vec<_>>().join("\n"))
            }
            "list-windows" => {
                assert_eq!(target(), format!("={}:", self.session));
                Ok(self.tabs.values().map(|t| format!("{}\t{}\t{}\t34\t{}\t{}",
                    t.window, t.index, t.sidebar.as_ref().map(PaneId::as_str).unwrap_or(""),
                    serde_json::to_string(&t.map).unwrap(), serde_json::to_string(&t.hidden).unwrap(),
                )).collect::<Vec<_>>().join("\n"))
            }
            "split-window" => {
                let window = self.panes.iter().find(|p| p.id == target()).unwrap().window;
                if args.iter().any(|a| a == "-b") {
                    assert!(!self.tabs[&window].map.panes.is_empty(), "seed must exist before sidebar starts");
                }
                Ok(self.add_pane(window))
            }
            "new-window" => {
                assert_eq!(target(), format!("={}:", self.session));
                let window = self.tabs.keys().max().copied().unwrap() + 1;
                self.tabs.insert(window, TabInfo { window: wid(window), index: window, sidebar: None,
                    map: PaneMap::new(), hidden: HiddenLog::new() });
                let pane = self.add_pane(window);
                Ok(format!("@{window}\t{window}\t{pane}"))
            }
            "set-option" => {
                let window = self.panes.iter().find(|p| p.id == target()).unwrap().window;
                let key = &args[args.len()-2];
                let value = args.last().unwrap();
                let tab = self.tabs.get_mut(&window).unwrap();
                match key.as_str() {
                    tmux::OPT_TAB_MAP => tab.map = serde_json::from_str(value).unwrap(),
                    tmux::OPT_TAB_SIDEBAR => tab.sidebar = Some(pid(value)),
                    tmux::OPT_TAB_HIDDEN => tab.hidden = serde_json::from_str(value).unwrap(),
                    _ => panic!("unexpected option {key}"),
                }
                Ok(String::new())
            }
            "kill-pane" => {
                let id = target().to_owned();
                self.panes.retain(|p| p.id != id);
                Ok(String::new())
            }
            "select-window" | "select-pane" | "select-layout" | "resize-pane" | "respawn-pane" => {
                assert!(self.panes.iter().any(|p| p.id == target()), "target must be live in the fixture");
                Ok(String::new())
            }
            _ => panic!("unscripted tmux command: {args:?}"),
        }
    }

    fn calls(&self, verb: &str) -> Vec<&Vec<String>> {
        self.calls.iter().filter(|args| args[0] == verb).collect()
    }
}

fn with_sidebar(test: impl FnOnce(&mut App, &Rc<RefCell<Server>>)) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let mut a = super::tests::app();
    a.tmux_session = format!("ccmux-pane-fixture-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    a.own_pane = Some(pid("%1"));
    a.kill_pane = tmux::kill_pane;
    a.codex.settings = crate::settings::CodexSettings {
        url: "ws://127.0.0.1:8965".into(), token_file: "/missing/fixture/token file".into(),
        bin: "/fixture/codex bin".into(),
    };
    a.codex.prepare = |_| panic!("pane key must not prepare or call RPC");
    a.sidebar_cmd = a.codex.settings.command(Path::new("/fixture/ccmux"),
        ["sidebar", "--socket", "ccmux-smoke", "--session", &a.tmux_session].map(Into::into));
    a.sessions = vec![row()];
    a.rebuild_rows();
    let server = Rc::new(RefCell::new(Server::new(&a.tmux_session)));
    let handler = server.clone();
    tmux::test_commands::with(move |args| handler.borrow_mut().command(args), || {
        a.refresh_panes();
        server.borrow_mut().calls.clear();
        test(&mut a, &server);
    });
}

fn map_pane(a: &mut App, server: &Rc<RefCell<Server>>, window: u32, row: &Session, latch: &str) -> String {
    let mut s = server.borrow_mut();
    s.tabs.entry(window).or_insert_with(|| TabInfo { window: wid(window), index: window, sidebar: None,
        map: PaneMap::new(), hidden: HiddenLog::new() });
    let pane = s.add_pane(window);
    s.panes.last_mut().unwrap().latch = latch.into();
    s.tabs.get_mut(&window).unwrap().map.insert(&pid(&pane), record(row));
    if window == 1 { a.map.insert(&pid(&pane), record(row)); }
    drop(s);
    a.refresh_panes();
    server.borrow_mut().calls.clear();
    pane
}

#[test]
fn enter_opens_and_then_jumps_while_o_and_s_always_split() {
    with_sidebar(|a, server| {
        assert_eq!(key(a, KeyCode::Enter), Action::Redraw);
        {
            let s = server.borrow();
            let split = s.calls("split-window");
            assert_eq!(split.len(), 1);
            assert_eq!(split[0][1], "-h");
            assert!(split[0].iter().any(|s| s == "-d"));
            assert!(s.calls("select-pane").is_empty(), "opening retains sidebar focus");
            let cmd = split[0].last().unwrap();
            assert!(cmd.contains(&format!("resume {ID}; rc=$?")));
            assert!(!cmd.contains("display-only") && !cmd.contains("claude attach"));
        }
        let pane = a.pane_of(Provider::Codex, ID).unwrap();
        let entry = a.map.get(&pane).unwrap();
        assert_eq!((entry.provider, entry.session_id.as_str(), entry.name.as_str()),
            (Provider::Codex, ID, "Codex task"));
        assert_eq!(serde_json::to_value(&a.map).unwrap()["v"], 2);
        server.borrow_mut().calls.clear();
        key(a, KeyCode::Enter);
        assert!(server.borrow().calls("split-window").is_empty());
        assert_eq!(server.borrow().calls("select-pane")[0].last().unwrap(), pane.as_str());
        for (key, flag) in [('o', "-h"), ('s', "-v")] {
            server.borrow_mut().calls.clear();
            press(a, key);
            assert_eq!(server.borrow().calls("split-window").len(), 1);
            assert_eq!(server.borrow().calls("split-window")[0][1], flag);
        }
        assert_eq!(a.map.panes.len(), 3);
    });
}

#[test]
fn every_codex_runtime_can_attach_without_a_display_id_or_successful_poll() {
    for (runtime, status, state) in [
        (CodexStatus::NotLoaded, Status::Idle, Some(State::Unloaded)),
        (CodexStatus::SystemError, Status::Unknown("systemError".into()), None),
        (CodexStatus::Unknown("future".into()), Status::Unknown("future".into()), None),
        (CodexStatus::Active { flags: vec![] }, Status::Busy, Some(State::Working)),
        (CodexStatus::Active { flags: vec!["waitingOnApproval".into()] }, Status::Waiting, Some(State::Blocked)),
    ] {
        with_sidebar(|a, server| {
            let r = &mut a.sessions[0];
            r.id = None;
            r.status = status;
            r.state = state;
            r.codex.as_mut().unwrap().runtime = runtime;
            a.rebuild_rows();
            press(a, 'o');
            assert_eq!(server.borrow().calls("split-window").len(), 1);
            assert!(a.map.panes.values().all(|e| e.session_id == ID && e.short_id.is_empty()));
        });
    }
}

#[test]
fn codex_tab_seeds_v2_before_sidebar_and_propagates_only_nonsecret_settings() {
    with_sidebar(|a, server| {
        press(a, 't');
        let s = server.borrow();
        let new = s.calls.iter().position(|a| a[0] == "new-window").unwrap();
        let seed = s.calls.iter().position(|a| a[0] == "set-option" && a.contains(&tmux::OPT_TAB_MAP.into())).unwrap();
        let sidebar = s.calls.iter().position(|a| a[0] == "split-window").unwrap();
        assert!(new < seed && seed < sidebar);
        let wire: serde_json::Value = serde_json::from_str(s.calls[seed].last().unwrap()).unwrap();
        assert_eq!(wire["v"], 2);
        assert_eq!(wire["panes"]["%10"]["provider"], "codex");
        assert_eq!(wire["panes"]["%10"]["session_id"], ID);
        let cmd = s.calls[sidebar].last().unwrap();
        assert!(cmd.contains("'CCMUX_CODEX_BIN=/fixture/codex bin'"));
        assert!(cmd.contains("--codex-url=ws://127.0.0.1:8965"));
        assert!(cmd.contains("'--codex-token-file=/missing/fixture/token file'"));
        assert!(!cmd.contains("CODEX_REMOTE_TOKEN="));
        assert_eq!(s.calls("select-pane").last().unwrap().last().unwrap(), "%10");
        assert!(a.map.panes.is_empty(), "new tab has its own writer");
        assert_eq!(s.tabs[&2].map.panes["%10"].provider, Provider::Codex);
        assert_eq!(a.message.as_ref().unwrap().0, "opened Codex task in tab 2");
    });
}

#[test]
fn x_closes_live_parked_and_shell_codex_panes_without_touching_the_thread() {
    for latch in ["", "1", "shell"] {
        with_sidebar(|a, server| {
            let pane = map_pane(a, server, 1, &row(), latch);
            agents::test_spawn::reset();
            press(a, 'x');
            let s = server.borrow();
            assert_eq!(s.calls("kill-pane").len(), 1);
            assert_eq!(s.calls("kill-pane")[0].last().unwrap(), &pane);
            let kill = s.calls.iter().position(|a| a[0] == "kill-pane").unwrap();
            assert_eq!(s.calls[kill-1][0], "list-panes", "real R2 gate precedes kill");
            assert!(a.map.panes.is_empty());
            assert_eq!(a.message.as_ref().unwrap().0, "closed pane 2 — Codex work stays on server");
            assert!(agents::test_spawn::calls().is_empty());
        });
    }
}

#[test]
fn cross_tab_enter_and_x_keep_the_foreign_map_owned_by_its_sidebar() {
    with_sidebar(|a, server| {
        let pane = map_pane(a, server, 2, &row(), "");
        let original = server.borrow().tabs[&2].map.clone();
        key(a, KeyCode::Enter);
        assert_eq!(server.borrow().calls("select-pane")[0].last().unwrap(), &pane);
        assert_eq!(a.message.as_ref().unwrap().0, "jumped to pane 1 in tab 2");
        server.borrow_mut().calls.clear();
        press(a, 'x');
        let s = server.borrow();
        assert_eq!(s.tabs[&2].map, original);
        assert!(s.calls("set-option").is_empty());
        assert_eq!(a.message.as_ref().unwrap().0, "closed pane 1 in tab 2 — Codex work stays on server");
    });
}

#[test]
fn parked_launch_loses_jump_eligibility_and_sidebar_enter_opens_another_pane() {
    with_sidebar(|a, server| {
        let parked = map_pane(a, server, 1, &row(), "1");
        assert_eq!(a.pane_of(Provider::Codex, ID), None);
        assert_eq!(a.pane_of_any(Provider::Codex, ID), Some(pid(&parked)));
        key(a, KeyCode::Enter);
        assert_eq!(server.borrow().calls("split-window").len(), 1);
        assert!(server.borrow().calls("select-pane").is_empty());
        assert_eq!(a.map.panes.len(), 2);
        assert!(a.map.panes.contains_key(&parked));
    });
}

#[test]
fn codex_close_keeps_sidebar_protection_and_live_r2_checks() {
    with_sidebar(|a, server| {
        a.map.insert(&pid("%1"), record(&row()));
        a.rebuild_open();
        press(a, 'x');
        assert_eq!(a.message.as_ref().unwrap().0, "refusing to close the sidebar");
        assert!(server.borrow().calls("kill-pane").is_empty());
        a.map.remove(&pid("%1"));
        let pane = map_pane(a, server, 1, &row(), "1");
        server.borrow_mut().panes.retain(|p| p.id != pane); // vanished after last inventory
        press(a, 'x');
        assert!(a.message.as_ref().unwrap().0.starts_with("close failed:"));
        assert!(server.borrow().calls("kill-pane").is_empty());
    });
}

#[test]
fn foreign_provider_records_cannot_jump_or_close_and_a_missing_map_says_not_open() {
    with_sidebar(|a, server| {
        let mut wrong = row();
        wrong.provider = Provider::Claude;
        map_pane(a, server, 2, &wrong, "");
        key(a, KeyCode::Enter);
        press(a, 'x');
        assert_eq!(a.message.as_ref().unwrap().0, "not open");
        let s = server.borrow();
        assert!(s.calls("select-pane").is_empty() && s.calls("split-window").is_empty() && s.calls("kill-pane").is_empty());
    });
}

#[test]
fn enter_jumps_to_an_attached_codex_pane_after_a_rejected_reload() {
    with_sidebar(|a, server| {
        let pane = map_pane(a, server, 1, &row(), "");
        a.codex.open_rejected = true;
        assert_eq!(key(a, KeyCode::Enter), Action::Redraw);
        assert_eq!(a.message.as_ref().unwrap().0, "jumped to pane 2");
        let s = server.borrow();
        assert!(!s.calls("list-panes").is_empty(), "jump requires fresh inventory");
        assert_eq!(s.calls("select-pane").len(), 1);
        assert_eq!(s.calls("select-pane")[0].last().unwrap(), &pane);
        assert!(s.calls("split-window").is_empty() && s.calls("new-window").is_empty());
    });
}

#[test]
fn configuration_and_resolution_refuse_new_panes_but_x_needs_no_codex_config() {
    for bad in ["off", "syntax", "unsafe-resolution"] {
        with_sidebar(|a, server| {
            let pane = map_pane(a, server, 1, &row(), "1");
            match bad {
                "off" => a.codex.settings.url.clear(),
                "syntax" => a.codex.settings.url = "ws://example.com:8965".into(),
                _ => a.codex.open_rejected = true,
            }
            let text = if bad == "unsafe-resolution" {
                "codex localhost resolved outside loopback — open unavailable"
            } else { "codex not configured — open unavailable" };
            for code in [KeyCode::Enter, KeyCode::Char('o'), KeyCode::Char('s'), KeyCode::Char('t')] {
                server.borrow_mut().calls.clear();
                key(a, code);
                assert_eq!(a.message.as_ref().unwrap().0, text);
                if code == KeyCode::Enter {
                    assert!(server.borrow().calls.iter().all(|args|
                        matches!(args[0].as_str(), "list-panes" | "list-windows")));
                } else {
                    assert!(server.borrow().calls.is_empty());
                }
            }
            press(a, 'x');
            assert_eq!(server.borrow().calls("kill-pane")[0].last().unwrap(), &pane);
        });
    }
}

#[test]
fn degraded_tmux_refusals_win_over_codex_configuration_errors() {
    with_sidebar(|a, server| {
        a.degraded = true;
        a.codex.settings.url.clear();
        for (code, verb) in [(KeyCode::Enter, "open"), (KeyCode::Char('o'), "open"),
            (KeyCode::Char('s'), "open"), (KeyCode::Char('t'), "tabs"), (KeyCode::Char('x'), "close")]
        {
            key(a, code);
            assert_eq!(a.message.as_ref().unwrap().0, format!("not inside tmux — {verb} unavailable"));
        }
        assert!(server.borrow().calls.is_empty());
    });
}

#[test]
fn codex_refusals_and_unbound_keys_have_no_tmux_rpc_or_claude_effects() {
    with_sidebar(|a, server| {
        agents::test_spawn::reset();
        for short in [None, Some("display-only")] {
            a.sessions[0].id = short.map(str::to_owned);
            a.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
            assert_eq!(a.message.as_ref().unwrap().0, "Codex stop/delete unavailable in v1 — use the Codex TUI");
            press(a, 'L');
            assert_eq!(a.message.as_ref().unwrap().0, "Codex logs unavailable in v1 — use the Codex TUI");
        }
        for c in ['c', 'S'] { assert_eq!(press(a, c), Action::None); }
        assert!(a.stop_arm.is_none() && a.pending_delete.is_none() && a.logs.is_none());
        assert_eq!(a.mode, Mode::Normal);
        assert!(server.borrow().calls.is_empty() && agents::test_spawn::calls().is_empty());
    });
}

#[test]
fn moving_from_a_claude_delete_window_to_codex_disarms_it() {
    with_sidebar(|a, server| {
        let mut claude = row();
        claude.provider = Provider::Claude;
        claude.session_id = "claude-uuid".into();
        claude.id = Some("deadbeef".into());
        claude.state = Some(State::Stopped);
        a.sessions.push(claude);
        a.rebuild_rows();
        a.select_last();
        a.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(a.stop_arm.is_some());
        press(a, 'g');
        assert_eq!(a.selected_session().unwrap().provider, Provider::Codex);
        assert!(a.stop_arm.is_none() && a.pending_delete.is_none());
        assert!(server.borrow().calls.is_empty());
    });
}

#[test]
fn n_with_only_codex_rows_submits_claude_from_the_local_directory() {
    with_sidebar(|a, server| {
        a.dispatch = |provider, cwd, task| {
            assert_eq!(provider, Provider::Claude);
            assert_eq!(cwd, std::env::current_dir().unwrap().to_str().unwrap());
            assert_eq!(task, "fixture task");
            Ok(None)
        };
        press(a, 'n');
        assert_eq!(a.prompt.as_ref().unwrap().kind, PromptKind::NewBackground);
        a.on_paste("fixture task");
        key(a, KeyCode::Enter);
        assert_eq!(a.mode, Mode::Normal);
        assert!(a.message.as_ref().unwrap().0.contains("dispatched background session"));
        assert!(server.borrow().calls.is_empty());
    });
}

#[test]
fn r_from_a_codex_row_restarts_only_sidebars_and_claude_and_reports_each_skip_once() {
    with_sidebar(|a, server| {
        map_pane(a, server, 1, &row(), "");
        map_pane(a, server, 1, &row(), "1");
        let mut claude = row();
        claude.provider = Provider::Claude;
        claude.session_id = "claude-uuid".into();
        claude.id = Some("deadbeef".into());
        map_pane(a, server, 1, &claude, "");
        a.respawn = tmux::respawn_pane;
        a.agents_poll = || Ok(model::Payload { sessions: vec![], dropped: 0 });
        a.agents_stop = agents::stop;
        a.agents_respawn = agents::respawn;
        agents::test_spawn::reset();
        assert_eq!(press(a, 'R'), Action::Restart);
        assert!(a.pending_restart.is_some() && server.borrow().calls.is_empty());
        a.finish_restart(); // emulate the fresh image; never exec the test process
        let s = server.borrow();
        assert_eq!(s.calls("respawn-pane").len(), 1);
        assert!(s.calls("respawn-pane")[0].last().unwrap().contains("claude attach deadbeef"));
        assert!(!s.calls("respawn-pane")[0].last().unwrap().contains("CODEX_REMOTE_TOKEN"));
        let note = &a.message.as_ref().unwrap().0;
        assert_eq!(note, "restarted 1 sidebar, 1 pane, 0 agents (1 not restarted); Codex panes skipped: 2");
        assert!(agents::test_spawn::calls().is_empty());
    });
}
