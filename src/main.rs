//! ccmux — a tmux-backed frontend for Claude Code sessions.
//!
//! One binary, two roles (SPEC §1):
//!
//! - `ccmux` — the launcher. Creates or heals the ccmux tmux session, pins the
//!   sidebar pane, then attaches. Never enters raw mode.
//! - `ccmux sidebar` — the ratatui session explorer that runs inside the left
//!   pane. Raw mode + alternate screen + §4.1's event loop.

mod agents;
mod app;
mod model;
mod restart;
mod tmux;
mod ui;

use std::io::{self, Stdout};
use std::os::unix::process::CommandExt;
use std::time::Duration;

use anyhow::{Context, anyhow};
use clap::Parser;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};

use crate::app::{Action, MsgLevel};
use crate::tmux::PaneId;

#[derive(clap::Parser)]
// `version` is not decoration: `restart::probe` runs `<candidate> --version`
// to prove the binary `R` is about to `exec` actually starts, and reads the
// name it prints back to prove it is ccmux and not a `$PATH` namesake.
#[command(name = "ccmux", version, about = "tmux-backed frontend for Claude Code sessions")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,

    /// tmux session name to create/attach
    #[arg(long, default_value = "ccmux", global = true, value_parser = validate_session_name)]
    pub session: String,

    /// Pinned sidebar width in columns
    #[arg(long, default_value_t = 34, global = true)]
    pub width: u16,

    /// Use the light palette (the default)
    #[arg(long, global = true, conflicts_with = "dark")]
    pub light: bool,

    /// Use the dark palette (for a dark terminal background)
    #[arg(long, global = true)]
    pub dark: bool,

    /// tmux socket name (`tmux -L <name>`); default is tmux's own socket.
    ///
    /// PROBE-FINDINGS §8 requires this: it is how ccmux is exercised against a
    /// throwaway tmux server that cannot reach the operator's live panes.
    #[arg(short = 'L', long, global = true, value_parser = validate_socket_name)]
    pub socket: Option<String>,
}

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Run the session-explorer sidebar (normally launched by `ccmux` itself)
    Sidebar {
        /// Poll interval for `claude agents --json`, milliseconds
        #[arg(long, default_value_t = 2500)]
        interval: u64,
    },
}

/// Reject anything tmux cannot address: `:` and `.` are tmux target separators.
pub fn validate_session_name(s: &str) -> Result<String, String> {
    let ok = (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(s.to_string())
    } else {
        Err("session name must match [A-Za-z0-9_-]{1,64}".to_string())
    }
}

/// Mirrors `tmux::valid_socket_name`. Rejected here so a bad `-L` is a clap
/// error the operator can read, not a `BadTarget` on every later tmux call.
pub fn validate_socket_name(s: &str) -> Result<String, String> {
    let ok = (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.');
    if ok {
        Ok(s.to_string())
    } else {
        Err("socket name must match [A-Za-z0-9_.-]{1,64}".to_string())
    }
}

/// `--width` is clamped to 20..=120 before use, so a typo cannot produce an
/// unusable sidebar (SPEC §1.1).
pub const WIDTH_MIN: u16 = 20;
pub const WIDTH_MAX: u16 = 120;

/// `--interval` floor and ceiling. Not in SPEC §1.1, but `--interval 0` would
/// spawn `claude agents --json` in a tight loop against the operator's live
/// daemon; same class of guard as the width clamp.
const INTERVAL_MIN_MS: u64 = 250;
const INTERVAL_MAX_MS: u64 = 600_000;

/// `event::poll` slice (SPEC §4.2).
const TICK_MS: u64 = 120;

/// A `tick()` at least this slow is treated as a freeze: input typed during it
/// is discarded rather than replayed. Normal ticks cost ~0.21s.
const SLOW_TICK: Duration = Duration::from_secs(1);
/// A keypress that blocked for longer than this froze the UI, so whatever the
/// tty buffered during it is type-ahead aimed at a screen that was never drawn.
/// `SLOW_TICK`'s rule, at the shorter threshold a single blocking `claude` call
/// needs: a `claude stop` plus the forced refresh behind `Ctrl+X` measures
/// ~0.85 s, comfortably under `SLOW_TICK` and comfortably over this.
const SLOW_KEY: Duration = Duration::from_millis(300);

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // MUST be the first thing that happens: every tmux call reads the
    // process-global socket. `set_socket` is called only when `-L` was given,
    // because `set_socket(None)` pins the DEFAULT socket and would defeat the
    // `$CCMUX_TMUX_SOCKET` seeding a sidebar pane may rely on.
    if let Some(name) = cli.socket.as_deref() {
        tmux::set_socket(Some(name));
    }

    match cli.cmd {
        Some(Cmd::Sidebar { interval }) => run_sidebar(&cli, interval),
        None => run_launcher(&cli),
    }
}

/// Which palette to use: an explicit flag, then `CCMUX_THEME=light|dark`, then
/// the **light** default.
///
/// Light is the default because the palette is not auto-detected and a wrong
/// guess is not symmetric: dark `fg` (#ebdbb2) on a light ground is 1.21:1 —
/// invisible — whereas light `fg` (#3c3836) on a dark ground still reads.
/// The safer default is the one whose failure mode is merely ugly.
///
/// There is deliberately NO terminal auto-detection. `COLORFGBG` is unset under
/// kitty and most modern terminals, and while tmux does emit an OSC 11
/// background query on client attach, a response could not be verified
/// end-to-end headlessly — shipping it would mean shipping an unverified
/// startup path that can hang the TUI waiting on a reply that never comes.
fn resolve_dark(cli: &Cli) -> bool {
    if cli.dark {
        return true;
    }
    if cli.light {
        return false;
    }
    matches!(std::env::var("CCMUX_THEME"), Ok(v) if v.eq_ignore_ascii_case("dark"))
}

// ── Launcher ────────────────────────────────────────────────────────────────

/// The shell-command string tmux runs (via `/bin/sh -c`) in the sidebar pane.
///
/// SPEC §1.2 builds this from `current_exe()` so a sidebar launched from a
/// non-`PATH` build still works, with every component `sh_quote`d (§7).
///
/// SPEC NOTE: the socket is propagated as a `--socket` argument rather than the
/// `CCMUX_TMUX_SOCKET=` env-assignment prefix the Tmux lane proposed. Same
/// effect, one quoting rule instead of two: the whole command stays a plain
/// `sh_join` of quoted argv words. Env seeding still works and is untouched.
fn sidebar_command(cli: &Cli, width: u16) -> anyhow::Result<String> {
    let exe = std::env::current_exe().context("cannot resolve the ccmux executable path")?;
    let exe = exe
        .to_str()
        .ok_or_else(|| anyhow!("the ccmux executable path is not valid UTF-8"))?;

    let width = width.to_string();
    let mut parts: Vec<&str> = vec![exe, "sidebar", "--session", &cli.session, "--width", &width];
    // Resolve here, in the launcher, and forward the ANSWER rather than the
    // flag: the sidebar runs in a tmux pane that may not inherit CCMUX_THEME,
    // so re-resolving there could disagree with what the operator asked for.
    if resolve_dark(cli) {
        parts.push("--dark");
    }
    if let Some(sock) = cli.socket.as_deref() {
        parts.push("--socket");
        parts.push(sock);
    }
    Ok(tmux::sh_join(&parts))
}

/// SPEC §1.2. Never enters raw mode.
fn run_launcher(cli: &Cli) -> anyhow::Result<()> {
    // 1 — tmux binary missing.
    if !tmux::server_available() {
        eprintln!("ccmux: tmux not found on PATH");
        std::process::exit(1);
    }

    // 2 — §9.6: re-running ccmux inside ccmux is a no-op, never a second
    // new-session. `current_session_name` answers only for the server this
    // process is actually a pane of, so `--socket` cannot short-circuit the
    // launcher with another server's session name.
    if tmux::current_session_name().as_deref() == Some(cli.session.as_str()) {
        println!("ccmux: already inside session '{}'", cli.session);
        return Ok(());
    }

    let width = cli.width.clamp(WIDTH_MIN, WIDTH_MAX);
    let sidebar_cmd = sidebar_command(cli, width)?;

    // 3 — a dead server also exits non-zero, which reads correctly as "absent".
    if tmux::has_session(&cli.session) {
        // A name collision must never turn into a layout change in someone
        // else's session. `@ccmux_map` is written by `configure_session` and by
        // nothing else, so its absence proves this session is not ours —
        // healing it would split a pane into a live window and then hold that
        // pane at ccmux's width forever (PROBE-FINDINGS §8's incident).
        if tmux::get_user_option(&cli.session, tmux::OPT_MAP).is_none() {
            return Err(anyhow!(
                "tmux session '{}' exists but is not a ccmux session — refusing to modify it",
                cli.session
            ));
        }
        heal_sidebar(cli, width, &sidebar_cmd)?;
    } else {
        // 4
        let pane = tmux::create_session(&cli.session, &sidebar_cmd)
            .with_context(|| format!("cannot create tmux session '{}'", cli.session))?;
        tmux::configure_session(&cli.session, &pane, width)
            .with_context(|| format!("cannot configure tmux session '{}'", cli.session))?;
        // Mark the first window as a ccmux tab straight away, so `heal_sidebar`
        // recognises it on the next launch without the legacy fallback.
        tmux::set_tab_sidebar(&cli.session, &pane)
            .with_context(|| format!("cannot record the sidebar of '{}'", cli.session))?;
        // 4g — a no-op returning exit 0 while the sidebar is the only pane.
        tmux::pin_sidebar(&cli.session, &pane, width);
    }

    // 6 — replaces this process's terminal view when attaching.
    tmux::attach_or_switch(&cli.session)
        .with_context(|| format!("cannot attach tmux session '{}'", cli.session))?;
    Ok(())
}

/// What the launcher must do to ONE window of ccmux's session. Pure, so §1.2
/// step 5's rule is unit-testable without a tmux server.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HealAction {
    /// The marker names a live pane of this window: re-pin it, nothing else.
    Repin(PaneId),
    /// An established ccmux tab whose sidebar was quit with `q` or crashed.
    Insert,
    /// Not a ccmux tab, or one still being built. Leave it completely alone.
    Skip,
}

/// `@ccmux_tab_sidebar` is the SOLE test for "this window is a ccmux tab".
///
/// The marker is a WINDOW option, so it outlives the process it names: it is
/// still set after a `q`, after a crash, and after the pane it named is gone.
/// That makes it a complete test for an ESTABLISHED tab all by itself.
///
/// It used to also count a window whose `@ccmux_tab_map` named a live pane, so
/// that a window `t` had just created — map seeded, sidebar not yet marked —
/// healed too. But that is exactly the interval in which `t` is still building
/// the tab, and healing it split a SECOND sidebar into the window: two ccmux
/// processes writing one window's `@ccmux_tab_map` and `@ccmux_tab_hidden`,
/// which is the one state the single-writer design exists to make unreachable,
/// and which never converges and never self-heals. Dropping the map branch
/// closes it BY CONSTRUCTION — at every instant of `t` the new window is either
/// unidentifiable (skip: bare window, then map seeded but unmarked) or marked
/// (re-pin), and never identifiable-but-unmarked. `t` now writes the marker
/// itself the moment the split returns, so nothing waits on the child process.
///
/// The cost is deliberate: a window whose `t` failed at the split keeps its
/// seeded map and gets no sidebar from a later launch. The seed stays anyway —
/// it is what keeps that live, attached Claude pane visible to `Enter` and
/// closable by `x` in every other tab.
fn heal_action(tab: &tmux::TabInfo, scoped: &[tmux::PaneInfo]) -> HealAction {
    match &tab.sidebar {
        Some(pane) if scoped.iter().any(|q| &q.id == pane) => HealAction::Repin(pane.clone()),
        Some(_) => HealAction::Insert,
        None => HealAction::Skip,
    }
}

/// SPEC §1.2 step 5, now once per TAB. Re-pins every intact sidebar and
/// re-inserts any that was quit with `q` or crashed.
///
/// Healing is the launcher's job EXCLUSIVELY. A running sidebar never lays out
/// another tab: a peer that inserted a pane into another window and wrote that
/// window's `@ccmux_tab_sidebar` would be the second writer this whole design
/// exists to make unrepresentable.
///
/// Which window gets what is decided by `heal_action`, which is pure.
fn heal_sidebar(cli: &Cli, width: u16, sidebar_cmd: &str) -> anyhow::Result<()> {
    let panes = tmux::list_panes_in_session(&cli.session)
        .with_context(|| format!("cannot list panes of tmux session '{}'", cli.session))?;

    // `@ccmux_width` is what every RUNNING sidebar re-pins from on every tick,
    // so a relaunch that only resized the pane would be reverted within one
    // poll. Write it first; the pins below then agree with those processes.
    tmux::set_user_option(&cli.session, tmux::OPT_WIDTH, &width.to_string())
        .with_context(|| format!("cannot record the sidebar width of '{}'", cli.session))?;

    let (tabs, _) = tmux::list_tabs(&cli.session)
        .with_context(|| format!("cannot list windows of tmux session '{}'", cli.session))?;

    let mut known_tab = false;
    for tab in &tabs {
        let scoped: Vec<tmux::PaneInfo> = panes
            .iter()
            .filter(|p| p.window_id == tab.window)
            .cloned()
            .collect();
        if scoped.is_empty() {
            continue;
        }
        match heal_action(tab, &scoped) {
            HealAction::Skip => continue,
            HealAction::Repin(pane) => {
                // 5c — harmless re-pin.
                known_tab = true;
                tmux::pin_sidebar(&cli.session, &pane, width);
                continue;
            }
            // 5b, per tab: the sidebar was quit with `q` or crashed while its
            // Claude panes lived on.
            HealAction::Insert => known_tab = true,
        }
        let leftmost = tmux::leftmost_pane(&scoped).ok_or_else(|| {
            anyhow!("tmux session '{}' has no panes to anchor a sidebar", cli.session)
        })?;
        let pane = tmux::split_left_of(&cli.session, &leftmost, sidebar_cmd)
            .context("cannot re-insert the sidebar pane")?;
        tmux::set_tab_sidebar(&cli.session, &pane)
            .context("cannot record the new sidebar pane")?;
        tmux::pin_sidebar(&cli.session, &pane, width);
    }

    if !known_tab {
        heal_legacy_session(cli, width, sidebar_cmd, &panes)?;
    }
    Ok(())
}

/// A session created by a build that had no tabs: no window carries a marker or
/// a tab map, and the sidebar (if any) is named by the session-scoped
/// `@ccmux_sidebar`. Heal exactly the one window it describes, then give that
/// window a proper marker so this path can never fire twice.
///
/// The marker must be read the way `list_tabs` reads it — as a WINDOW option
/// under a name no session value can shadow. A format lookup of the legacy
/// `@ccmux_sidebar` would report every window as marked, because `#{@name}`
/// falls back window -> session -> global (verified on tmux 3.4), and heal
/// would then inject a sidebar into each of the operator's own windows.
fn heal_legacy_session(
    cli: &Cli,
    width: u16,
    sidebar_cmd: &str,
    panes: &[tmux::PaneInfo],
) -> anyhow::Result<()> {
    let recorded = tmux::get_user_option(&cli.session, tmux::OPT_SIDEBAR)
        .as_deref()
        .and_then(PaneId::parse)
        .filter(|p| panes.iter().any(|q| &q.id == p));

    let pane = match recorded {
        // Alive: adopt it, do not split a second one in beside it.
        Some(pane) => {
            tmux::pin_sidebar(&cli.session, &pane, width);
            pane
        }
        None => {
            // Window-scoped: `pane_left` is per-window, so the leftmost pane of
            // the whole session can live in a window that has nothing to do
            // with the ccmux layout. Prefer the window ccmux created by name.
            let window = tmux::window_index_named(&cli.session, tmux::WINDOW_NAME)
                .filter(|w| panes.iter().any(|p| p.window_index == *w))
                .or_else(|| tmux::lowest_window(panes))
                .ok_or_else(|| {
                    anyhow!("tmux session '{}' has no windows to host a sidebar", cli.session)
                })?;
            let scoped = tmux::panes_in_window(panes, window);
            let leftmost = tmux::leftmost_pane(&scoped).ok_or_else(|| {
                anyhow!("tmux session '{}' has no panes to anchor a sidebar", cli.session)
            })?;
            let pane = tmux::split_left_of(&cli.session, &leftmost, sidebar_cmd)
                .context("cannot re-insert the sidebar pane")?;
            tmux::pin_sidebar(&cli.session, &pane, width);
            pane
        }
    };
    tmux::set_tab_sidebar(&cli.session, &pane).context("cannot record the new sidebar pane")?;
    Ok(())
}

// ── Sidebar ─────────────────────────────────────────────────────────────────

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Undo everything `run_sidebar` did to the terminal. Idempotent and
/// error-swallowing on purpose: it runs from the panic hook, where returning an
/// error is not an option and a half-restored terminal is worse than none.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

/// A panic inside raw mode + the alternate screen leaves the operator with an
/// unusable terminal (no echo, no newline translation, garbage screen). Restore
/// first, then let the original hook print the panic message onto a sane
/// terminal where it is actually readable.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original(info);
    }));
}

/// Raw mode + alternate screen + the event loop; teardown runs on every exit
/// path including a panic-free error return.
fn run_sidebar(cli: &Cli, interval_ms: u64) -> anyhow::Result<()> {
    let width = cli.width.clamp(WIDTH_MIN, WIDTH_MAX);
    let interval = Duration::from_millis(interval_ms.clamp(INTERVAL_MIN_MS, INTERVAL_MAX_MS));
    let mut app = app::App::new(cli.session.clone(), width, interval, resolve_dark(cli));
    // `t` gives the tab it creates its own sidebar, and the command that starts
    // one is built here, from the same `current_exe()` + `sh_join` builder the
    // launcher uses. `app` never imports `main`; the string is handed down, so
    // the module DAG stays acyclic.
    app.sidebar_cmd = sidebar_command(cli, width).ok();

    install_panic_hook();
    enable_raw_mode().context("cannot enter raw mode")?;
    let mut stdout = io::stdout();
    // Bracketed paste is MANDATORY, not a nicety: without it a paste into the
    // focused sidebar is delivered as individual key events and executed as
    // §8.1 keymap verbs — `S`+`y` alone stops a running agent.
    if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste) {
        let _ = disable_raw_mode();
        return Err(e).context("cannot enter the alternate screen");
    }

    let result = Terminal::new(CrosstermBackend::new(stdout))
        .context("cannot initialize the terminal backend")
        .and_then(|mut terminal| {
            let _ = terminal.hide_cursor();
            // §9.5: outside tmux this sets degraded mode; it never fails.
            app.init();
            // THE SECOND HALF OF AN `R`. The image that pressed it killed
            // nothing: it resolved this binary, proved it runs, and `exec`d.
            // This process running at all is the proof that licenses the
            // respawns, so they happen HERE — and only when the handoff token
            // is this process's own pid, which only an `exec` can arrange.
            if restart::is_handoff() {
                app.finish_restart();
            }
            event_loop(&mut terminal, &mut app)
        });

    // Teardown on BOTH the Ok and the Err path — an error return that leaves
    // the alternate screen up hides its own message.
    restore_terminal();
    // The deferred `@ccmux_map` / `@ccmux_hidden` writes have no next `tick` to
    // land in. This is the one place every exit path passes through — `q`,
    // `Ctrl-c` and an error return alike — so it is where they get written.
    // After `restore_terminal` so anything tmux prints lands on a real screen.
    app.shutdown();
    result
}

/// The terminal-derived geometry `app.rs` reads but never computes.
///
/// Runs on every iteration whether or not a frame follows: `on_key` pages by
/// `viewport`, so a stale one is a wrong `Ctrl-d`.
///
/// Generic over the backend for one reason: so `first_frame` is, and so the
/// thing `first_frame` exists to guarantee is a test and not a comment.
fn measure<B: Backend>(terminal: &Terminal<B>, app: &mut app::App) -> anyhow::Result<()> {
    let height = terminal.size()?.height;
    app.viewport = ui::list_viewport_rows(height);
    app.overlay_viewport = height.saturating_sub(2);
    app.help_lines = ui::help_line_count();
    app.clamp_scroll();
    Ok(())
}

/// ONE FRAME BEFORE THE FIRST TICK, and the whole reason it is a function.
///
/// `last_poll` is backdated in `App::new`, so the loop's first act is a
/// `tick()` that blocks on `claude agents --json` — up to
/// `agents::POLL_TIMEOUT`, and slowest of all on the cold start straight after
/// an upgrade, which is precisely when `R` was pressed. Drawing only after it
/// costs two things: the pane stays blank for that whole wait, and a message
/// armed before the loop can reach `MSG_TTL` with no frame ever having existed
/// — which is how `R`'s summary, the only feedback the verb has, was lost
/// outright. This frame makes both impossible, and it is the frame the operator
/// is looking at while the tick blocks.
fn first_frame<B: Backend>(terminal: &mut Terminal<B>, app: &mut app::App) -> anyhow::Result<()> {
    measure(terminal, app)?;
    terminal.draw(|f| ui::draw(f, app))?;
    Ok(())
}

/// SPEC §4.1, verbatim.
fn event_loop(terminal: &mut Tui, app: &mut app::App) -> anyhow::Result<()> {
    first_frame(terminal, app)?;
    let mut needs_draw = false;

    loop {
        if app.check_message_timeout() {
            needs_draw = true;
        }
        // THE DRIFT GUARD's retry. `App::note_drift` posts an unmodelled
        // `state`/`status` into the same one-line footer every flash uses, so a
        // keypress message or a full-screen overlay can take it away before it
        // has been read. This is the moment the footer frees up again — a tick,
        // not a poll, because polls quiesce behind the visibility gate and
        // stretch to 30s on a still fleet, and the warning must not wait on
        // either.
        if app.tick_drift() {
            needs_draw = true;
        }
        // §8.2: expires `Ctrl+X`'s delete window, and runs a delete that has
        // settled. It lives here rather than in the keypress so a held key's
        // repeat stream gets its chance to cancel one, and so the footer stops
        // promising a window that has closed even if no key is pressed again.
        if app.tick_stop_arm() {
            needs_draw = true;
        }

        if app.last_poll.elapsed() >= app.tick_interval() {
            let started = std::time::Instant::now();
            app.tick();
            needs_draw = true;
            // A tick that blocked long enough to be felt swallowed every key
            // pressed during it, and the terminal will now replay them all at
            // once against whatever is on screen. Those keystrokes were aimed
            // at a frozen UI; execute none of them.
            if started.elapsed() >= SLOW_TICK {
                drain_pending_input()?;
            }
        }

        measure(terminal, app)?;

        if needs_draw {
            terminal.draw(|f| ui::draw(f, app))?;
            needs_draw = false;
        }

        if event::poll(Duration::from_millis(TICK_MS))? {
            match event::read()? {
                // §4.1: filtering on Press is mandatory — without it a terminal
                // that reports key repeat/release doubles every keystroke.
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    let started = std::time::Instant::now();
                    let action = app.on_key(k);
                    // Same rule as the slow tick below, for the other place the
                    // UI can freeze: `Ctrl+X`'s first press shells out to
                    // `claude stop` and then to `claude agents --json`, ~0.85 s
                    // in which every keystroke is buffered and then replayed
                    // against a screen the operator never saw. `act_ctrl_x`
                    // re-stamps its own clock so a replayed `Ctrl+X` reads as
                    // the burst it is; this drops the whole replay, including
                    // the keys that are not `Ctrl+X`.
                    if started.elapsed() >= SLOW_KEY {
                        drain_pending_input()?;
                    }
                    match action {
                        Action::Quit => break,
                        Action::Restart => {
                            // `exec` does not return on success, so everything
                            // after this line is the FAILURE path.
                            if let Some(p) = app.pending_restart.take() {
                                let err = restart_now(
                                    &p,
                                    &mut |step| match step {
                                        // Whatever the tty buffered while the
                                        // respawns ran was aimed at a frozen
                                        // screen, and it would survive the
                                        // `exec` to be executed as keymap
                                        // verbs by the NEW image.
                                        RestartStep::DrainInput => {
                                            let _ = drain_pending_input();
                                        }
                                        RestartStep::RestoreTerminal => restore_terminal(),
                                        RestartStep::FlushTmuxState => app.shutdown(),
                                    },
                                    &mut exec_pending,
                                );
                                resume_terminal(terminal)?;
                                restart_failed(app, &err);
                            }
                            needs_draw = true;
                        }
                        Action::Redraw => needs_draw = true,
                        Action::None => {}
                    }
                }
                // Only reachable because `EnableBracketedPaste` is set above.
                Event::Paste(text) => {
                    if app.on_paste(&text) == Action::Redraw {
                        needs_draw = true;
                    }
                }
                Event::Resize(..) => needs_draw = true,
                _ => {}
            }
        }
    }

    Ok(())
}

/// The steps `R` runs before it stops being this process, in the order they
/// MUST happen. Named so the order is testable without a terminal and without
/// a process to replace: `restart_now` is what the event loop calls, and its
/// test drives the same function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartStep {
    /// Type-ahead typed at the frozen UI would otherwise survive the `exec`.
    DrainInput,
    /// MANDATORY, and mandatory FIRST: leave the alternate screen, drop raw
    /// mode, disable bracketed paste. The new image sets all three up again.
    /// Skip it and a failed `exec` leaves the operator with a wedged terminal —
    /// no echo, no newline translation — and no sidebar to fix it from.
    RestoreTerminal,
    /// `App::shutdown` — the deferred `@ccmux_tab_map` / `@ccmux_tab_hidden`
    /// writes. There is no next `tick` to land them in, exactly as at `q`, so a
    /// dismissal made since the last tick would not survive the restart.
    FlushTmuxState,
}

/// `R`'s tail: the three steps, then the `exec` that does not return.
///
/// The steps are a parameter rather than three inlined calls so that "the
/// terminal teardown runs before the exec" is a property of a function a test
/// can run, not of a comment. Returns the `exec` error, because an `exec` that
/// returns at all has failed and the sidebar must go on living.
fn restart_now(
    p: &restart::Pending,
    step: &mut dyn FnMut(RestartStep),
    exec: &mut dyn FnMut(&restart::Pending) -> io::Error,
) -> io::Error {
    step(RestartStep::DrainInput);
    step(RestartStep::RestoreTerminal);
    step(RestartStep::FlushTmuxState);
    exec(p)
}

/// Replace this process image with `p.exe`, keeping this pane, its geometry and
/// the window layout by construction — tmux is never told anything happened.
///
/// The argv is THIS process's own, minus argv[0]: same subcommand, same
/// `--session`, `--width`, `--socket`, `--interval`, same palette. argv[0]
/// becomes the freshly resolved path, which is what makes a SECOND restart find
/// the binary the same way, and what lets the new image rebuild the command the
/// other tabs' sidebars are respawned with.
///
/// `HANDOFF_ENV` carries this process's pid, which `exec` preserves: it is how
/// the image that comes up knows it owes the rest of the session a restart, and
/// — because only an `exec` keeps a pid — how a sidebar that merely inherited
/// the variable knows it does not.
fn exec_pending(p: &restart::Pending) -> io::Error {
    std::process::Command::new(&p.exe)
        .args(std::env::args_os().skip(1))
        .env(restart::HANDOFF_ENV, restart::handoff_token())
        .exec()
}

/// Undo `restore_terminal`. Reachable only when the `exec` failed, which is the
/// one case where the operator must be left with a working sidebar rather than
/// a dead pane.
fn resume_terminal(terminal: &mut Tui) -> anyhow::Result<()> {
    enable_raw_mode().context("cannot re-enter raw mode after a failed restart")?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)
        .context("cannot re-enter the alternate screen after a failed restart")?;
    let _ = terminal.hide_cursor();
    terminal.clear().context("cannot redraw after a failed restart")?;
    Ok(())
}

/// What the operator is told when the `exec` did not happen. The panes that
/// were already respawned are running the new binary; this one is not, and
/// saying so is the difference between a visible half-restart and a silent one.
fn restart_failed(app: &mut app::App, err: &io::Error) {
    app.flash(format!("restart failed: {err}"), MsgLevel::Error);
}

/// Discard every input event already queued. Called after a tick or keypress
/// that blocked long enough for the tty to buffer input aimed at a frozen UI.
fn drain_pending_input() -> anyhow::Result<()> {
    while event::poll(Duration::ZERO)? {
        let _ = event::read()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_name_validation() {
        assert!(validate_session_name("ccmux").is_ok());
        assert!(validate_session_name("ccmux-test_1").is_ok());
        assert!(validate_session_name("a").is_ok());
        assert!(validate_session_name(&"a".repeat(64)).is_ok());

        assert!(validate_session_name("").is_err());
        assert!(validate_session_name(&"a".repeat(65)).is_err());
        assert!(validate_session_name("agents:2").is_err());
        assert!(validate_session_name("agents.1").is_err());
        assert!(validate_session_name("my session").is_err());
    }

    #[test]
    fn socket_name_validation() {
        assert!(validate_socket_name("ccmux").is_ok());
        assert!(validate_socket_name("ccmux-test.1").is_ok());

        assert!(validate_socket_name("").is_err());
        assert!(validate_socket_name(&"a".repeat(65)).is_err());
        assert!(validate_socket_name("../../etc/passwd").is_err());
        assert!(validate_socket_name("has space").is_err());
    }

    #[test]
    fn cli_parses_the_documented_surface() {
        let cli = Cli::try_parse_from(["ccmux"]).expect("bare launcher");
        assert_eq!(cli.session, "ccmux");
        assert_eq!(cli.width, 34);
        assert!(!cli.light);
        assert!(cli.socket.is_none());
        assert!(cli.cmd.is_none());

        let cli = Cli::try_parse_from([
            "ccmux", "sidebar", "--session", "ccmux-test-a", "--width", "40", "--light", "-L",
            "ccmux", "--interval", "1000",
        ])
        .expect("sidebar with every global flag");
        assert_eq!(cli.session, "ccmux-test-a");
        assert_eq!(cli.width, 40);
        assert!(cli.light);
        assert_eq!(cli.socket.as_deref(), Some("ccmux"));
        match cli.cmd {
            Some(Cmd::Sidebar { interval }) => assert_eq!(interval, 1000),
            None => panic!("expected the sidebar subcommand"),
        }

        assert!(Cli::try_parse_from(["ccmux", "--session", "bad:name"]).is_err());
        assert!(Cli::try_parse_from(["ccmux", "--socket", "bad/name"]).is_err());
    }

    #[test]
    fn sidebar_command_is_a_quoted_argv_line() {
        let cli = Cli::try_parse_from(["ccmux", "--session", "ccmux-test-a", "--width", "40"])
            .expect("parse");
        let cmd = sidebar_command(&cli, 40).expect("build");
        assert!(cmd.contains(" sidebar --session ccmux-test-a --width 40"));
        // Light is the default, so nothing is forwarded for it.
        assert!(!cmd.contains("--light"));
        assert!(!cmd.contains("--dark"));
        assert!(!cmd.contains("--socket"));

        let cli = Cli::try_parse_from(["ccmux", "--dark", "-L", "ccmux"]).expect("parse");
        let cmd = sidebar_command(&cli, 34).expect("build");
        assert!(cmd.ends_with(" sidebar --session ccmux --width 34 --dark --socket ccmux"));
    }

    #[test]
    fn width_clamp_bounds_match_the_spec() {
        assert_eq!(1u16.clamp(WIDTH_MIN, WIDTH_MAX), 20);
        assert_eq!(34u16.clamp(WIDTH_MIN, WIDTH_MAX), 34);
        assert_eq!(9_999u16.clamp(WIDTH_MIN, WIDTH_MAX), 120);
    }

    #[test]
    fn interval_clamp_refuses_a_busy_loop() {
        assert_eq!(0u64.clamp(INTERVAL_MIN_MS, INTERVAL_MAX_MS), INTERVAL_MIN_MS);
        assert_eq!(2500u64.clamp(INTERVAL_MIN_MS, INTERVAL_MAX_MS), 2500);
        assert_eq!(
            u64::MAX.clamp(INTERVAL_MIN_MS, INTERVAL_MAX_MS),
            INTERVAL_MAX_MS
        );
    }

    /// `--light`/`--dark` are mutually exclusive, and the launcher forwards the
    /// RESOLVED answer to the sidebar rather than the raw flag — the pane may
    /// not inherit `CCMUX_THEME`, so re-resolving there could disagree.
    #[test]
    fn theme_flags_resolve_and_forward() {
        let light = Cli::try_parse_from(["ccmux", "--light"]).expect("parse");
        assert!(!resolve_dark(&light));
        let dark = Cli::try_parse_from(["ccmux", "--dark"]).expect("parse");
        assert!(resolve_dark(&dark));
        assert!(Cli::try_parse_from(["ccmux", "--light", "--dark"]).is_err());

        // The non-default is the one that travels to the pane.
        assert!(sidebar_command(&dark, 34).expect("cmd").contains("--dark"));
        assert!(!sidebar_command(&light, 34).expect("cmd").contains("--dark"));

        // Guarded: an operator running the suite with CCMUX_THEME exported
        // would otherwise see this fail for a reason that is not a defect.
        if std::env::var_os("CCMUX_THEME").is_none() {
            let bare = Cli::try_parse_from(["ccmux"]).expect("parse");
            assert!(!resolve_dark(&bare), "the default palette is light");
        }
    }

    fn heal_pane(id: &str, window: u32) -> tmux::PaneInfo {
        tmux::PaneInfo {
            id: PaneId::parse(id).expect("pane id"),
            index: 1,
            left: 0,
            top: 0,
            width: 34,
            height: 40,
            active: false,
            window_index: window,
            window_id: tmux::WindowId::parse(&format!("@{window}")).expect("window id"),
            window_active: true,
            session_clients: 1,
            window_viewers: Some(1),
            detached: false,
        }
    }

    fn heal_tab(window: u32, sidebar: Option<&str>, mapped: &[&str]) -> tmux::TabInfo {
        let mut map = tmux::PaneMap::new();
        for m in mapped {
            map.insert(
                &PaneId::parse(m).expect("pane id"),
                tmux::PaneEntry {
                    session_id: "aaaaaaaa-uuid".into(),
                    short_id: "aaaaaaaa".into(),
                    name: "n".into(),
                    opened_at: 0,
                },
            );
        }
        tmux::TabInfo {
            window: tmux::WindowId::parse(&format!("@{window}")).expect("window id"),
            index: window,
            sidebar: sidebar.and_then(PaneId::parse),
            map,
            hidden: tmux::HiddenLog::new(),
        }
    }

    /// REGRESSION. `@ccmux_tab_sidebar` is the SOLE test for "this window is a
    /// ccmux tab". Heal used to also adopt a window whose `@ccmux_tab_map`
    /// named a live pane — which is precisely the state `t` leaves behind while
    /// it is still building a tab, between seeding that map and the sidebar
    /// being marked. A launcher run in that ~30-40 ms gap split a SECOND
    /// sidebar into the window, and the two processes then wrote one window's
    /// `@ccmux_tab_map` and `@ccmux_tab_hidden` forever, diverging on screen
    /// and never converging. The marker branch alone is complete, because a
    /// window option outlives the process it names.
    #[test]
    fn heal_identifies_a_tab_by_its_marker_and_nothing_else() {
        // A tab being built by `t`: map seeded through the Claude pane, marker
        // not written yet, and — worst case — the sidebar pane already split
        // in. Heal must not touch it.
        let building = heal_tab(4, None, &["%9"]);
        let panes = vec![heal_pane("%9", 4), heal_pane("%10", 4)];
        assert_eq!(heal_action(&building, &panes), HealAction::Skip);

        // The operator's own `prefix-c` window: nothing at all. Unchanged.
        assert_eq!(heal_action(&heal_tab(7, None, &[]), &[heal_pane("%5", 7)]), HealAction::Skip);

        // An established tab whose sidebar was quit with `q`: the marker
        // survives its process and still names %1, which is gone. Heal it.
        let quit = heal_tab(1, Some("%1"), &["%2"]);
        assert_eq!(heal_action(&quit, &[heal_pane("%2", 1)]), HealAction::Insert);

        // A tab whose sidebar is alive: re-pin, never a second split.
        let live = heal_tab(1, Some("%1"), &["%2"]);
        let panes = vec![heal_pane("%1", 1), heal_pane("%2", 1)];
        assert_eq!(heal_action(&live, &panes), HealAction::Repin(PaneId::parse("%1").expect("id")));
    }

    /// SPEC.md is the design contract, and `src/` cites it by section number in
    /// ~20 comments. When the interactive machinery was deleted the spec was
    /// briefly left declaring nine functions and four struct fields that no
    /// longer exist, which silently turned those citations into lies. This
    /// pins the half a test can actually check: no removed item may reappear
    /// as a DECLARATION. Prose that says a thing is gone is fine and expected
    /// — the tombstoned §5.4 and §8.7 are full of it — so every needle below
    /// is a signature or a field, never a bare name.
    /// THE ORDER. The terminal teardown MUST precede the `exec`: an `exec`
    /// that fails from inside raw mode + the alternate screen leaves the
    /// operator a wedged terminal, and the new image sets both up again anyway.
    /// The tmux flush must also precede it, or a dismissal made since the last
    /// tick dies with the process image.
    #[test]
    fn the_restart_tears_the_terminal_down_before_it_execs() {
        let p = restart::Pending { exe: std::path::PathBuf::from("/nonexistent/ccmux") };
        let seen: std::cell::RefCell<Vec<RestartStep>> = std::cell::RefCell::new(Vec::new());
        let execs = std::cell::Cell::new(0usize);

        let err = restart_now(
            &p,
            &mut |s| seen.borrow_mut().push(s),
            &mut |got| {
                execs.set(execs.get() + 1);
                // The exec is LAST: every step has already run by the time it
                // is reached, which is the property this test exists for.
                assert_eq!(
                    *seen.borrow(),
                    vec![
                        RestartStep::DrainInput,
                        RestartStep::RestoreTerminal,
                        RestartStep::FlushTmuxState
                    ]
                );
                assert_eq!(got.exe, p.exe, "it execs the resolved binary");
                io::Error::from(io::ErrorKind::NotFound)
            },
        );

        assert_eq!(execs.get(), 1);
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "the failure is returned, not swallowed");
        assert_eq!(
            seen.borrow().iter().position(|s| *s == RestartStep::RestoreTerminal),
            Some(1)
        );
    }

    /// THE REGRESSION TEST for a restart note nobody ever saw.
    ///
    /// `R`'s summary is armed before the event loop, and the loop's first act
    /// is a `tick()` that blocks on `claude agents --json` for as long as
    /// `agents::POLL_TIMEOUT`. With the first `draw` behind that tick, a poll
    /// of four seconds or more expired the message before a single frame had
    /// existed and the operator saw nothing at all. The frame comes first.
    #[test]
    fn the_first_frame_carries_a_message_armed_before_the_loop() {
        let mut app = app::App::new("ccmux-test".into(), 34, Duration::from_millis(2500), true);
        app.flash("restarted 2 sidebars, 3 panes", MsgLevel::Info);
        let mut term = Terminal::new(ratatui::backend::TestBackend::new(34, 20))
            .expect("test backend");

        // No tick, no key, nothing else: exactly what `event_loop` does first.
        first_frame(&mut term, &mut app).expect("the first frame draws");

        let dump: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            dump.contains("restarted 2 sidebars, 3 panes"),
            "the note must be on screen before anything can block: {dump:?}"
        );
    }

    /// `exec` only returns when it FAILED, and a failed restart must leave a
    /// live sidebar that says so — never a dead pane. The recovery path is
    /// reachable because `restart_now` returns rather than diverging.
    #[test]
    fn a_failed_exec_keeps_the_sidebar_alive_and_says_so() {
        let mut app = app::App::new("ccmux-test".into(), 34, Duration::from_millis(2500), true);
        restart_failed(&mut app, &io::Error::from(io::ErrorKind::NotFound));

        assert!(!app.should_quit, "a failed restart is not a quit");
        let (text, level) = app.message.clone().expect("the operator is told");
        assert!(text.starts_with("restart failed: "), "{text:?}");
        assert_eq!(level, MsgLevel::Error);
    }

    #[test]
    fn the_spec_declares_nothing_the_crate_no_longer_has() {
        const SPEC: &str = include_str!("../SPEC.md");
        for needle in [
            // tmux.rs: the /proc ppid walk and the server-wide enumeration
            "pub fn ppid_of",
            "pub fn ancestry",
            "pub fn resolve_pane_for_pid",
            "pub fn list_panes_all",
            // tmux.rs: the one call that ever targeted a foreign session
            "pub fn focus_foreign_pane",
            // agents.rs: the `c` binding's command template
            "pub fn interactive_pane_cmd",
            // struct fields
            "pub interactive_panes",
            "pub pid: i32",
            "pub session_name: String",
            // the `NewInteractive` prompt variant, as a variant and not as prose
            "    NewInteractive,",
            // the confirm-modal machinery, deleted outright once `Ctrl+X`
            // replaced `S`: no declaration of it may come back
            "pub enum Confirm",
            "    Confirm(Confirm),",
            "pub fn act_request_stop",
            "pub fn act_confirm_stop",
            "fn key_confirm",
            "fn draw_confirm",
            "pub confirm_armed_at",
            "const CONFIRM_ARM_DELAY",
            // The group arity. `Group` gained `Blocked`, so a spec that still
            // declares three is promising an `all()` the crate does not have —
            // and the three-variant `Group` is what filed a blocked session
            // under Idle in the first place.
            "[Group; 3]",
            "Working = 0",
            // The attach pane's old trailer. The pane no longer waits to be
            // closed; it hands itself to a shell, and a spec that still quotes
            // the prompt is promising a keypress that does nothing.
            "press enter to close pane",
            "; read _",
        ] {
            assert!(
                !SPEC.contains(needle),
                "SPEC.md still declares {needle:?}, which the crate does not have"
            );
        }
        // The `c` row of the §8.1 keymap table, and the `-a` enumeration R3
        // used to permit. Both are behaviour the spec would be promising.
        assert!(!SPEC.contains("| `c` |"), "SPEC.md still lists a `c` keybinding");
        // `S` stops nothing now (§8.2). The spec must not promise that it does.
        assert!(
            !SPEC.contains("| `S` | **stop the session**"),
            "SPEC.md still binds stop to `S`"
        );
        assert!(
            SPEC.contains("| `Ctrl-x` | **stop the session**"),
            "SPEC.md does not document the `Ctrl-x` binding"
        );
        assert!(
            SPEC.contains("| `R` | **restart ccmux in place**"),
            "SPEC.md does not document the `R` binding"
        );
        assert!(
            !SPEC.contains("permitted in exactly one place"),
            "R3 still carves out an exception for `list-panes -a`"
        );
        // The shell handoff and the latch that keeps the map honest about it.
        assert!(
            SPEC.contains("exec \"$SHELL\" -l"),
            "SPEC.md does not declare the shell the attach pane hands over to"
        );
        assert!(
            SPEC.contains("@ccmux_detached"),
            "SPEC.md does not declare the pane option that retires a mapping"
        );
        // The other direction, for the two values that shipped unmodelled: the
        // spec must NAME them, because the glyph table and the state list are
        // where the next implementer looks before touching `model::State`.
        for needle in ["[Group; 4]", "Blocked = 0", "Blocked", "Stopped", "Waiting"] {
            assert!(
                SPEC.contains(needle),
                "SPEC.md does not declare {needle:?}, which the crate has"
            );
        }
    }

    /// The README's glyph table is what an operator reads to find out what a
    /// mark means, and a mark missing from it reads as noise on the screen.
    /// Every glyph `ui::status_glyph` can return must appear there.
    #[test]
    fn the_readme_glyph_table_names_every_glyph_the_list_can_draw() {
        const README: &str = include_str!("../README.md");
        for glyph in ["▲", "●", "◐", "○", "✓", "■", "?"] {
            assert!(
                README.contains(&format!("| `{glyph}`")),
                "the README glyph table is missing {glyph:?}"
            );
        }
        assert!(
            README.contains("Blocked"),
            "the README does not mention the Blocked group"
        );
    }

    /// The same rule for the README, which is what an operator reads before the
    /// spec: it must not promise a key the build does not bind.
    #[test]
    fn the_readme_documents_no_c_keybinding() {
        const README: &str = include_str!("../README.md");
        assert!(!README.contains("| `c` |"), "README lists a `c` keybinding");
        assert!(
            !README.contains("`n`, `c`"),
            "README still pairs `c` with `n` in the mode table"
        );
    }

    /// The same rule for the binding that moved: the README must not promise a
    /// stop on `S`, and must document both halves of `Ctrl-x` — including what
    /// the second press takes, which is the half that cannot be undone.
    #[test]
    fn the_docs_bind_stop_to_ctrl_x_and_not_to_s() {
        const README: &str = include_str!("../README.md");
        const SPEC: &str = include_str!("../SPEC.md");
        assert!(
            !README.contains("| `S` | **Stop the session.**"),
            "README still binds stop to `S`"
        );
        for (doc, name) in [(README, "README.md"), (SPEC, "SPEC.md")] {
            assert!(doc.contains("Ctrl-x"), "{name} does not mention Ctrl-x");
            assert!(
                doc.contains("worktree"),
                "{name} does not say what the second press takes"
            );
            assert!(
                doc.contains("claude rm"),
                "{name} does not name the verb the second press runs"
            );
        }
        // The Safety section used to close the delete window on a cursor move.
        // It does not: `arm_hint` names the captured row precisely BECAUSE the
        // cursor is free to move while the window is open, and the move is
        // answered at the next press, which refuses. SPEC had this right and
        // the README did not, which made the README the only doc misstating
        // the lifetime of the one irreversible verb.
        assert!(
            README.contains("Moving the cursor does **not** close it"),
            "README does not say the delete window survives a cursor move"
        );
        assert!(
            !README.contains("`q`, moving the cursor\n  to another row"),
            "README still closes the delete window on a cursor move"
        );
    }

}
