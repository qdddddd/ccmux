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

struct Pane { id: String, window: u32, index: u32, latch: String, pick: String }

struct Server {
    session: String,
    panes: Vec<Pane>,
    tabs: BTreeMap<u32, TabInfo>,
    /// Window-LEVEL values of the options the picker reads and writes; an
    /// absent key is a window with no override, which inherits `GLOBAL`.
    wopts: BTreeMap<u32, BTreeMap<String, String>>,
    calls: Vec<Vec<String>>,
    next: u32,
    /// `#{window_zoomed_flag}` for every window.
    zoomed: bool,
}

/// The operator's global `pane-border-status`: tmux's default, which is what
/// their config leaves it at.
const GLOBAL_STATUS: &str = "off";

impl Server {
    fn new(session: &str) -> Self {
        Self {
            session: session.into(),
            panes: vec![Pane { id: "%1".into(), window: 1, index: 1, latch: String::new(), pick: String::new() }],
            tabs: BTreeMap::from([(1, TabInfo { window: wid(1), index: 1, sidebar: Some(pid("%1")),
                map: PaneMap::new(), hidden: HiddenLog::new() })]), wopts: BTreeMap::new(), calls: vec![], next: 10, zoomed: false,
        }
    }

    fn add_pane(&mut self, window: u32) -> String {
        let id = format!("%{}", self.next);
        self.next += 1;
        let index = self.panes.iter().filter(|p| p.window == window).count() as u32 + 1;
        self.panes.push(Pane { id: id.clone(), window, index, latch: String::new(), pick: String::new() });
        id
    }

    fn command(&mut self, argv: &[String]) -> Result<String, TmuxError> {
        let args = if argv.first().is_some_and(|s| s == "-L") { &argv[2..] } else { argv };
        self.calls.push(args.to_vec());
        let target = || args.windows(2).find(|w| w[0] == "-t").map(|w| w[1].as_str()).unwrap();
        let window_of = |s: &Self, pane: &str| s.panes.iter().find(|p| p.id == pane).map(|p| p.window);
        match args[0].as_str() {
            "show-options" => {
                let window = window_of(self, target()).ok_or_else(|| tmux_err(args, "no such window"))?;
                let name = args.last().unwrap();
                let set = self.wopts.get(&window).and_then(|w| w.get(name)).cloned();
                if args.contains(&"-wA".to_string()) {
                    assert_eq!(name, "pane-border-status", "only the status is read effective");
                    return Ok(set.unwrap_or_else(|| GLOBAL_STATUS.into()));
                }
                Ok(match (set, args.contains(&"-qv".to_string())) {
                    (Some(v), true) => v,
                    (Some(v), false) => format!("{name} {v}"),
                    (None, _) => String::new(),
                })
            }
            "set-option" if args.contains(&"-p".to_string()) => {
                let id = target().to_owned();
                let pane = self.panes.iter_mut().find(|p| p.id == id).ok_or_else(|| tmux_err(args, "no such pane"))?;
                if args.contains(&"-u".to_string()) {
                    assert_eq!(args.last().unwrap(), tmux::OPT_PANE_PICK);
                    pane.pick.clear();
                } else {
                    assert_eq!(&args[args.len() - 2], tmux::OPT_PANE_PICK);
                    pane.pick = args.last().unwrap().clone();
                }
                Ok(String::new())
            }
            "set-option" if args.contains(&"-u".to_string()) => {
                let window = window_of(self, target()).ok_or_else(|| tmux_err(args, "no such window"))?;
                let key = args.last().unwrap();
                assert!(matches!(key.as_str(), "pane-border-status" | "pane-border-format" | tmux::OPT_PICK_SAVED),
                    "unexpected unset {key}");
                self.wopts.entry(window).or_default().remove(key);
                Ok(String::new())
            }
            "set-option" if matches!(args[args.len() - 2].as_str(),
                "pane-border-status" | "pane-border-format" | tmux::OPT_PICK_SAVED) =>
            {
                assert!(args.contains(&"-w".to_string()), "border options are window options");
                let window = window_of(self, target()).ok_or_else(|| tmux_err(args, "no such window"))?;
                self.wopts.entry(window).or_default().insert(args[args.len() - 2].clone(), args.last().unwrap().clone());
                Ok(String::new())
            }
            "list-panes" => {
                assert_eq!(target(), format!("={}:", self.session));
                Ok(self.panes.iter().map(|p| format!(
                    "{}\t{}\t{}\t0\t{}\t40\t0\t{}\t@{}\t1\t1\t1\t{}\t{}",
                    p.id, p.index, if p.index == 1 { 0 } else { 35 }, if p.index == 1 { 34 } else { 60 },
                    p.window, p.window, u8::from(self.zoomed), p.latch)).collect::<Vec<_>>().join("\n"))
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

    /// `(pane, label)` for every pane wearing one.
    fn labels(&self) -> Vec<(String, String)> {
        self.panes.iter().filter(|p| !p.pick.is_empty()).map(|p| (p.id.clone(), p.pick.clone())).collect()
    }

    /// Window 1's option overrides, as the operator would see `show -w`.
    fn window_opts(&self) -> BTreeMap<String, String> {
        self.wopts.get(&1).cloned().unwrap_or_default()
    }
}

fn tmux_err(args: &[String], msg: &str) -> TmuxError {
    TmuxError::Cmd { args: args.to_vec(), code: 1, stderr: msg.into() }
}

fn with_sidebar(test: impl FnOnce(&mut App, &Rc<RefCell<Server>>)) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let mut a = super::tests::app();
    a.tmux_session = format!("ccmux-pane-fixture-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    a.own_pane = Some(pid("%1"));
    a.kill_pane = tmux::kill_pane;
    a.codex.settings = crate::settings::CodexSettings {
        url: "ws://127.0.0.1:8965".into(), token_file: "/missing/fixture/token file".into(),
        bin: "/fixture/codex bin".into(), show_imports: false,
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
fn enter_opens_and_then_jumps_while_v_and_s_split_or_pick() {
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
        // One pane beside the sidebar: `v` splits it with no picker, exactly
        // as `act_open` would.
        server.borrow_mut().calls.clear();
        press(a, 'v');
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(server.borrow().calls("split-window").len(), 1);
        assert_eq!(server.borrow().calls("split-window")[0][1], "-h");
        assert!(server.borrow().labels().is_empty());
        // Two: `s` picks, and the label it is answered with is the pane split.
        server.borrow_mut().calls.clear();
        press(a, 's');
        assert_eq!(a.mode, Mode::PickPane);
        assert!(server.borrow().calls("split-window").is_empty());
        press(a, 'b');
        let s = server.borrow();
        assert_eq!(s.calls("split-window").len(), 1);
        assert_eq!(s.calls("split-window")[0][1], "-v");
        assert_eq!(s.calls("split-window")[0][3], "%11");
        assert_eq!(a.map.panes.len(), 3);
    });
}

/// Puts `n` more panes beside the fixture's sidebar, unmapped — the
/// operator's own panes, which the picker offers like any other.
fn add_panes(a: &mut App, server: &Rc<RefCell<Server>>, n: usize) -> Vec<String> {
    let ids = (0..n).map(|_| server.borrow_mut().add_pane(1)).collect();
    a.refresh_panes();
    server.borrow_mut().calls.clear();
    ids
}

fn position(server: &Rc<RefCell<Server>>, test: impl Fn(&Vec<String>) -> bool) -> usize {
    server.borrow().calls.iter().position(test).expect("call issued")
}

/// §8.4, the whole round trip: labels dealt in `#{pane_index}` order, the
/// window's originals recorded BEFORE the first border write, a lowercase
/// label answered, the labels and border options gone BEFORE the split, and
/// the split aimed at the labelled pane.
#[test]
fn a_label_splits_its_pane_after_the_labels_come_down() {
    with_sidebar(|a, server| {
        let ids = add_panes(a, server, 3);
        assert_eq!(press(a, 'v'), Action::Redraw);
        assert_eq!(a.mode, Mode::PickPane);
        assert!(a.message.is_none(), "the hint owns the footer");
        let want: Vec<(String, String)> =
            ids.iter().zip(["A", "B", "C"]).map(|(id, l)| (id.clone(), l.to_string())).collect();
        assert_eq!(server.borrow().labels(), want);
        let opts = server.borrow().window_opts();
        assert_eq!(opts["pane-border-status"], "top");
        assert!(opts["pane-border-format"].contains("@ccmux_pick"));
        let saved: serde_json::Value = serde_json::from_str(&opts[tmux::OPT_PICK_SAVED]).unwrap();
        assert_eq!(saved, serde_json::json!({"v": 1, "status_touched": true, "status": null, "format": null}));
        let save = position(server, |c| c[0] == "set-option" && c.contains(&tmux::OPT_PICK_SAVED.to_string()));
        let border = position(server, |c| c[0] == "set-option" && c.contains(&"pane-border-status".to_string()));
        assert!(save < border, "the originals are recorded before anything changes");
        assert!(server.borrow().calls("split-window").is_empty());

        server.borrow_mut().calls.clear();
        assert_eq!(press(a, 'c'), Action::Redraw);
        assert_eq!(a.mode, Mode::Normal);
        assert!(a.pick.is_none());
        let s = server.borrow();
        assert!(s.labels().is_empty());
        assert!(s.window_opts().is_empty(), "no override left behind: {:?}", s.window_opts());
        let split = s.calls("split-window");
        assert_eq!(split.len(), 1);
        assert_eq!((split[0][1].as_str(), split[0][3].as_str()), ("-h", ids[2].as_str()));
        drop(s);
        let unset = position(server, |c| c[0] == "set-option" && c.contains(&"-u".to_string()));
        let split = position(server, |c| c[0] == "split-window");
        assert!(unset < split, "labels come down before the split");
        assert!(a.message.as_ref().unwrap().0.starts_with("opened Codex task in pane"));
        assert!(a.map.panes.contains_key(&format!("%{}", server.borrow().next - 1)));
    });
}

#[test]
fn esc_and_any_other_key_cancel_and_clear_the_labels() {
    for code in [KeyCode::Esc, KeyCode::Char('z'), KeyCode::Char('j'), KeyCode::Char('v'), KeyCode::Enter] {
        with_sidebar(|a, server| {
            add_panes(a, server, 2);
            press(a, 's');
            assert_eq!(a.mode, Mode::PickPane);
            assert_eq!(key(a, code), Action::Redraw);
            assert_eq!(a.mode, Mode::Normal, "{code:?}");
            assert!(a.pick.is_none());
            assert_eq!(a.message.as_ref().unwrap().0, "pick cancelled");
            let s = server.borrow();
            assert!(s.labels().is_empty() && s.window_opts().is_empty(), "{code:?}");
            assert!(s.calls("split-window").is_empty(), "{code:?} opened something");
        });
    }
}

/// The operator's own window-level border settings come back EXACTLY: a
/// window already drawing a bottom border keeps its status untouched, and a
/// window-level format is re-set, not unset.
#[test]
fn window_level_border_options_are_restored_exactly() {
    with_sidebar(|a, server| {
        add_panes(a, server, 2);
        let mine = BTreeMap::from([
            ("pane-border-status".to_string(), "bottom".to_string()),
            ("pane-border-format".to_string(), "#{pane_title}".to_string()),
        ]);
        server.borrow_mut().wopts.insert(1, mine.clone());
        press(a, 'v');
        let s = server.borrow();
        assert_eq!(s.window_opts()["pane-border-status"], "bottom", "an existing border line is reused");
        assert!(!s.calls.iter().any(|c| c[0] == "set-option" && c.contains(&"pane-border-status".to_string())));
        drop(s);
        key(a, KeyCode::Esc);
        assert_eq!(server.borrow().window_opts(), mine);
    });
}

#[test]
fn a_refused_open_never_shows_a_picker() {
    with_sidebar(|a, server| {
        add_panes(a, server, 2);
        a.codex.settings.url.clear();
        for c in ['v', 's'] {
            press(a, c);
            assert_eq!(a.mode, Mode::Normal);
            assert_eq!(a.message.as_ref().unwrap().0, "codex not configured — open unavailable");
        }
        assert!(server.borrow().calls.is_empty());
    });
    with_sidebar(|a, server| {
        add_panes(a, server, 2);
        a.degraded = true;
        press(a, 'v');
        assert_eq!(a.mode, Mode::Normal);
        assert!(server.borrow().calls.is_empty());
    });
}

/// A zoomed window draws no pane borders, so `v`/`s` split as `Enter`
/// does rather than wait on labels nobody can see.
#[test]
fn a_zoomed_window_splits_without_a_picker() {
    with_sidebar(|a, server| {
        add_panes(a, server, 2);
        server.borrow_mut().zoomed = true;
        press(a, 'v');
        assert_eq!(a.mode, Mode::Normal);
        let s = server.borrow();
        assert_eq!(s.calls("split-window").len(), 1);
        assert!(s.labels().is_empty() && s.window_opts().is_empty());
    });
}

/// The session is re-resolved, and every guard re-run, when the label lands.
#[test]
fn a_session_that_left_the_list_while_picking_opens_nothing() {
    with_sidebar(|a, server| {
        add_panes(a, server, 2);
        press(a, 'v');
        a.sessions.clear();
        a.rebuild_rows();
        press(a, 'a');
        assert_eq!(a.message.as_ref().unwrap().0, "session left the list — nothing opened");
        let s = server.borrow();
        assert!(s.calls("split-window").is_empty());
        assert!(s.labels().is_empty() && s.window_opts().is_empty());
    });
}

/// `tick`'s step 3a: a labelled pane closing, or an unlabelled one opening,
/// cancels the pick; an unchanged pane set does not.
#[test]
fn the_tick_cancels_a_pick_whose_panes_changed() {
    for change in ["close", "open"] {
        with_sidebar(|a, server| {
            let ids = add_panes(a, server, 2);
            press(a, 'v');
            a.refresh_panes();
            a.tick_pick();
            assert_eq!(a.mode, Mode::PickPane, "nothing changed");
            match change {
                "close" => server.borrow_mut().panes.retain(|p| p.id != ids[0]),
                _ => { server.borrow_mut().add_pane(1); }
            }
            a.refresh_panes();
            a.tick_pick();
            assert_eq!(a.mode, Mode::Normal, "{change}");
            assert_eq!(a.message.as_ref().unwrap().0, "panes changed — pick cancelled");
            let s = server.borrow();
            assert!(s.labels().is_empty() && s.window_opts().is_empty(), "{change}");
        });
    }
}

#[test]
fn shutdown_takes_the_labels_down() {
    with_sidebar(|a, server| {
        add_panes(a, server, 2);
        press(a, 'v');
        // `Ctrl-c` quits from any mode without passing `key_pick`.
        assert_eq!(a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), Action::Quit);
        assert!(!server.borrow().labels().is_empty());
        a.shutdown();
        assert!(a.pick.is_none());
        let s = server.borrow();
        assert!(s.labels().is_empty() && s.window_opts().is_empty());
    });
}

/// The clear goes by the pick's WINDOW and label list, never by the pane the
/// labels were shown through: the sidebar moving to another window, the
/// anchor dying, and a labelled pane moving out each still leave the labelled
/// window, and every labelled pane, exactly as before.
#[test]
fn a_cancelled_pick_restores_its_own_window_whatever_moved() {
    for change in ["sidebar moved", "anchor died", "label moved"] {
        with_sidebar(|a, server| {
            let ids = add_panes(a, server, 3);
            press(a, 'v');
            assert_eq!(a.mode, Mode::PickPane);
            {
                let mut s = server.borrow_mut();
                s.tabs.entry(2).or_insert_with(|| TabInfo { window: wid(2), index: 2, sidebar: None,
                    map: PaneMap::new(), hidden: HiddenLog::new() });
                let moved = match change {
                    "sidebar moved" => "%1",
                    "label moved" => ids[1].as_str(),
                    _ => "",
                };
                if change == "anchor died" {
                    s.panes.retain(|p| p.id != "%1");
                } else {
                    s.panes.iter_mut().find(|p| p.id == moved).unwrap().window = 2;
                }
            }
            a.refresh_panes();
            a.tick_pick();
            assert_eq!(a.mode, Mode::Normal, "{change}");
            let s = server.borrow();
            assert!(s.labels().is_empty(), "{change}: {:?}", s.labels());
            assert!(s.window_opts().is_empty(), "{change}: {:?}", s.window_opts());
            assert!(s.wopts.get(&2).is_none_or(|w| w.is_empty()), "{change}: window 2 untouched");
        });
    }
}

/// The Codex refusal checks the PICKED session, not the row the cursor is on
/// when the label lands, and a picked session a tick hid from the rows is not
/// opened.
#[test]
fn a_pick_guards_its_own_session_not_the_cursor_row() {
    let mut claude = row();
    claude.provider = Provider::Claude;
    claude.session_id = "claude-fixture".into();
    claude.id = Some("abcd1234".into());
    claude.codex = None;
    // Codex picked, cursor moved to a Claude row, Codex now refused.
    with_sidebar(|a, server| {
        a.sessions.push(claude.clone());
        a.rebuild_rows();
        add_panes(a, server, 2);
        a.selected = (0..a.rows.len()).find(|&i| a.key_at(i).as_deref() == Some(ID)).unwrap();
        press(a, 'v');
        assert_eq!(a.pick.as_ref().unwrap().provider, Provider::Codex);
        a.selected = (0..a.rows.len()).find(|&i| a.key_at(i).as_deref() == Some("claude-fixture")).unwrap();
        a.codex.open_rejected = true;
        press(a, 'a');
        assert_eq!(a.message.as_ref().unwrap().0, "codex localhost resolved outside loopback — open unavailable");
        assert!(server.borrow().calls("split-window").is_empty());
    });
    // Claude picked, cursor moved to a Codex row whose config is unusable.
    with_sidebar(|a, server| {
        a.sessions.push(claude.clone());
        a.rebuild_rows();
        add_panes(a, server, 2);
        a.selected = (0..a.rows.len()).find(|&i| a.key_at(i).as_deref() == Some("claude-fixture")).unwrap();
        press(a, 'v');
        assert_eq!(a.pick.as_ref().unwrap().provider, Provider::Claude);
        a.selected = (0..a.rows.len()).find(|&i| a.key_at(i).as_deref() == Some(ID)).unwrap();
        a.codex.settings.url.clear();
        press(a, 'a');
        assert_eq!(server.borrow().calls("split-window").len(), 1, "{:?}", a.message);
    });
    // Picked session still in `sessions` but gone from the visible rows.
    with_sidebar(|a, server| {
        add_panes(a, server, 2);
        press(a, 'v');
        a.filter = "no such session".into();
        a.rebuild_rows();
        press(a, 'a');
        assert_eq!(a.message.as_ref().unwrap().0, "session left the list — nothing opened");
        assert!(server.borrow().calls("split-window").is_empty());
    });
}

/// A sidebar that died mid-pick left labels and its saved record behind; the
/// window's own next sidebar heals it once, and a second sidebar adopting the
/// real one does not.
#[test]
fn startup_heals_a_window_a_dead_sidebar_left_labelled() {
    with_sidebar(|a, server| {
        let ids = add_panes(a, server, 2);
        press(a, 'v');
        a.pick = None; // the process "died": nothing in memory, all of it in tmux
        a.mode = Mode::Normal;
        assert_eq!(server.borrow().labels().len(), 2);

        a.own_pane = Some(pid(&ids[0])); // a hand-launched second sidebar
        a.pick_healed = false;
        a.heal_pick_once();
        assert!(a.pick_healed);
        assert_eq!(server.borrow().labels().len(), 2, "not this process's window to heal");

        a.own_pane = Some(pid("%1"));
        a.pick_healed = false;
        a.heal_pick_once();
        let s = server.borrow();
        assert!(s.labels().is_empty());
        assert!(s.window_opts().is_empty(), "{:?}", s.window_opts());
    });
}

#[test]
fn more_panes_than_labels_is_refused() {
    with_sidebar(|a, server| {
        add_panes(a, server, PICK_CHARS.len() + 1);
        press(a, 'v');
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(a.message.as_ref().unwrap().0, "37 panes — too many to label");
        assert!(server.borrow().calls.iter().all(|c| c[0] == "list-panes" || c[0] == "list-windows"));
    });
}

#[test]
fn labels_are_letters_then_digits_in_candidate_order() {
    let ids: Vec<PaneId> = (0..36).map(|i| pid(&format!("%{}", 100 - i))).collect();
    let labels = pick_labels(&ids).unwrap();
    assert_eq!(labels[0], ('A', pid("%100")));
    assert_eq!(labels[25].0, 'Z');
    assert_eq!(labels[26].0, '1');
    assert_eq!(labels[35], ('0', pid("%65")));
    assert!(pick_labels(&[ids.clone(), vec![pid("%1")]].concat()).is_none());
    assert_eq!(pick_labels(&[]).unwrap(), vec![]);
}

/// The scope is `split_anchor`'s: the sidebar's window only, sidebar excluded,
/// in `#{pane_index}` order whatever order tmux listed them in.
#[test]
fn pick_scope_is_the_sidebars_window_in_pane_index_order() {
    with_sidebar(|a, server| {
        let ids = add_panes(a, server, 2);
        map_pane(a, server, 2, &row(), ""); // another tab's pane is never a candidate
        server.borrow_mut().panes.reverse();
        a.refresh_panes();
        let (anchor, window, candidates) = a.pick_scope().unwrap();
        assert_eq!((anchor, window), (pid("%1"), wid(1)));
        assert_eq!(candidates, ids.iter().map(|i| pid(i)).collect::<Vec<_>>());
    });
}

/// `v` replaced `o`, which is now unbound: it does nothing at all.
#[test]
fn o_is_unbound() {
    with_sidebar(|a, server| {
        add_panes(a, server, 2);
        assert_eq!(press(a, 'o'), Action::None);
        assert_eq!(a.mode, Mode::Normal);
        assert!(a.message.is_none());
        assert!(server.borrow().calls.is_empty());
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
            press(a, 'v');
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

/// The open flashes name the session the way every other flash does,
/// through `session_label`: an unbounded name pushed `in pane N` / `in tab N`
/// off the default sidebar.
#[test]
fn the_open_flashes_bound_a_long_name_to_the_label_width() {
    const LONG: &str = "Update Clash and Sync Codex profiles across machines";
    let label = model::truncate_end(LONG, LABEL_MAX);
    assert!(label.ends_with('…'), "the fixture must be over-long: {label}");
    with_sidebar(|a, _| {
        a.sessions[0].name = LONG.into();
        a.rebuild_rows();
        press(a, 'v');
        let msg = a.message.as_ref().unwrap().0.clone();
        assert!(msg.starts_with(&format!("opened {label}")), "{msg}");
        assert!(!msg.contains(LONG), "{msg}");
    });
    with_sidebar(|a, _| {
        a.sessions[0].name = LONG.into();
        a.rebuild_rows();
        press(a, 't');
        assert_eq!(a.message.as_ref().unwrap().0, format!("opened {label} in tab 2"));
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
            for code in [KeyCode::Enter, KeyCode::Char('v'), KeyCode::Char('s'), KeyCode::Char('t')] {
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
        for (code, verb) in [(KeyCode::Enter, "open"), (KeyCode::Char('v'), "open"),
            (KeyCode::Char('s'), "open"), (KeyCode::Char('t'), "tabs"), (KeyCode::Char('x'), "close")]
        {
            key(a, code);
            assert_eq!(a.message.as_ref().unwrap().0, format!("not inside tmux — {verb} unavailable"));
        }
        a.codex_delete = |_, _, _| panic!("degraded delete reached the RPC");
        a.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert_eq!(a.message.as_ref().unwrap().0, "not inside tmux — delete unavailable");
        assert!(a.stop_arm.is_none());
        assert!(server.borrow().calls.is_empty());
    });
}

#[test]
fn codex_delete_arm_and_unbound_keys_have_no_external_effects() {
    with_sidebar(|a, server| {
        agents::test_spawn::reset();
        for short in [None, Some("display-only")] {
            a.sessions[0].id = short.map(str::to_owned);
            // Each loop iteration is an independent first press. The previous
            // Codex disarm intentionally guards its unexpired window.
            a.cx_last_press = None;
            a.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
            assert_eq!(a.message.as_ref().unwrap().0, "Ctrl-x again to delete Codex task");
            assert!(a.stop_arm.is_some());
            a.disarm_ctrl_x();
            press(a, 'L');
            assert_eq!(a.message.as_ref().unwrap().0, "Codex logs unavailable in v1 — use the Codex TUI");
        }
        for c in ['c', 'S'] { assert_eq!(press(a, c), Action::None); }
        assert!(a.stop_arm.is_none() && a.pending_delete.is_none() && a.logs.is_none());
        assert_eq!(a.mode, Mode::Normal);
        assert!(server.borrow().calls.is_empty() && agents::test_spawn::calls().is_empty());
    });
}

static DELETES: AtomicUsize = AtomicUsize::new(0);

#[test]
fn delete_closes_the_codex_pane_in_another_tab() {
    with_sidebar(|a, server| {
        DELETES.store(0, Ordering::SeqCst);
        a.codex_delete = |_, _, _| {
            DELETES.fetch_add(1, Ordering::SeqCst);
            Ok(crate::codex::DeleteOutcome::Deleted)
        };
        let row = a.sessions[0].clone();
        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        a.on_key(ctrl_x);
        assert!(a.stop_arm.is_some(), "{:?}", a.message);
        // Another tab's sidebar opens the thread; this one has not ticked.
        map_in_tab_without_refresh(server, 2, &row);
        a.cx_last_press = a.cx_last_press.and_then(|t| t.checked_sub(Duration::from_secs(1)));
        a.on_key(ctrl_x);
        a.pending_delete.as_mut().expect("the stale inventory let the second press through").at -= CX_SETTLE;
        assert!(a.tick_stop_arm());
        assert_eq!(DELETES.load(Ordering::SeqCst), 1);
        assert_eq!(a.message.as_ref().unwrap().0, "deleted Codex task · closed 1 pane");
        assert_eq!(server.borrow().calls("kill-pane").len(), 1);
    });
}

fn map_in_tab_without_refresh(server: &Rc<RefCell<Server>>, window: u32, row: &Session) {
    let mut s = server.borrow_mut();
    s.tabs.insert(window, TabInfo { window: wid(window), index: window, sidebar: None,
        map: PaneMap::new(), hidden: HiddenLog::new() });
    let pane = s.add_pane(window);
    s.tabs.get_mut(&window).unwrap().map.insert(&pid(&pane), record(row));
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
        a.select_first(); // Completed now precedes the final Codex group.
        assert_eq!(a.selected_session().unwrap().provider, Provider::Claude);
        a.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(a.stop_arm.is_some());
        press(a, 'G');
        assert_eq!(a.selected_session().unwrap().provider, Provider::Codex);
        assert!(a.stop_arm.is_none() && a.pending_delete.is_none());
        assert!(server.borrow().calls.is_empty());
    });
}

#[test]
fn n_with_only_codex_rows_submits_codex_from_the_selected_directory() {
    with_sidebar(|a, server| {
        a.sessions[0].cwd = std::env::current_dir().unwrap().to_string_lossy().into_owned();
        a.codex_start = |_, cwd, task| {
            assert_eq!(cwd, std::env::current_dir().unwrap().to_str().unwrap());
            assert_eq!(task, "fixture task");
            Ok(row().session_id)
        };
        press(a, 'n');
        assert_eq!(a.prompt.as_ref().unwrap().kind, PromptKind::NewCodex);
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
