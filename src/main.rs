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
mod tmux;
mod ui;

use std::io::{self, Stdout};
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
use ratatui::backend::CrosstermBackend;

use crate::app::{Action, Mode};
use crate::tmux::PaneId;

#[derive(clap::Parser)]
#[command(name = "ccmux", about = "tmux-backed frontend for Claude Code sessions")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,

    /// tmux session name to create/attach
    #[arg(long, default_value = "ccmux", global = true, value_parser = validate_session_name)]
    pub session: String,

    /// Pinned sidebar width in columns
    #[arg(long, default_value_t = 34, global = true)]
    pub width: u16,

    /// Use the light palette (default: dark)
    #[arg(long, global = true)]
    pub light: bool,

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
    if cli.light {
        parts.push("--light");
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
        // 4g — a no-op returning exit 0 while the sidebar is the only pane.
        tmux::pin_sidebar(&cli.session, &pane, width);
    }

    // 6 — replaces this process's terminal view when attaching.
    tmux::attach_or_switch(&cli.session)
        .with_context(|| format!("cannot attach tmux session '{}'", cli.session))?;
    Ok(())
}

/// SPEC §1.2 step 5. Re-pins an intact sidebar; re-inserts one that was quit
/// with `q` or crashed.
fn heal_sidebar(cli: &Cli, width: u16, sidebar_cmd: &str) -> anyhow::Result<()> {
    let panes = tmux::list_panes_in_session(&cli.session)
        .with_context(|| format!("cannot list panes of tmux session '{}'", cli.session))?;

    let recorded = tmux::get_user_option(&cli.session, tmux::OPT_SIDEBAR)
        .as_deref()
        .and_then(PaneId::parse);
    // Present in the option AND still alive, or it does not count.
    let live = recorded.filter(|p| panes.iter().any(|q| &q.id == p));

    // `@ccmux_width` is what the RUNNING sidebar re-pins from every tick, so a
    // relaunch that only resized the pane would be reverted within one poll.
    // Write it first; the pin below then agrees with the sidebar process.
    tmux::set_user_option(&cli.session, tmux::OPT_WIDTH, &width.to_string())
        .with_context(|| format!("cannot record the sidebar width of '{}'", cli.session))?;

    match live {
        // 5c — harmless re-pin.
        Some(pane) => tmux::pin_sidebar(&cli.session, &pane, width),
        // 5b
        None => {
            // Window-scoped: `pane_left` is per-window, so the leftmost pane of
            // the whole session can live in a window that has nothing to do
            // with the ccmux layout. Prefer the window ccmux created by name.
            let window = tmux::window_index_named(&cli.session, tmux::WINDOW_NAME)
                .filter(|w| panes.iter().any(|p| p.window_index == *w))
                .or_else(|| tmux::lowest_window(&panes))
                .ok_or_else(|| {
                    anyhow!("tmux session '{}' has no windows to host a sidebar", cli.session)
                })?;
            let scoped = tmux::panes_in_window(&panes, window);
            let leftmost = tmux::leftmost_pane(&scoped).ok_or_else(|| {
                anyhow!("tmux session '{}' has no panes to anchor a sidebar", cli.session)
            })?;
            let pane = tmux::split_left_of(&cli.session, &leftmost, sidebar_cmd)
                .context("cannot re-insert the sidebar pane")?;
            tmux::set_user_option(&cli.session, tmux::OPT_SIDEBAR, pane.as_str())
                .context("cannot record the new sidebar pane")?;
            tmux::pin_sidebar(&cli.session, &pane, width);
        }
    }
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
    let mut app = app::App::new(cli.session.clone(), width, interval, !cli.light);

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
            event_loop(&mut terminal, &mut app)
        });

    // Teardown on BOTH the Ok and the Err path — an error return that leaves
    // the alternate screen up hides its own message.
    restore_terminal();
    result
}

/// SPEC §4.1, verbatim.
fn event_loop(terminal: &mut Tui, app: &mut app::App) -> anyhow::Result<()> {
    let mut needs_draw = true;

    loop {
        if app.check_message_timeout() {
            needs_draw = true;
        }

        if app.last_poll.elapsed() >= app.effective_interval() {
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

        // `app.rs` reads these but never computes them; this is the only place
        // they are written. `overlay_viewport` is the body height of a bordered
        // full-area overlay, which is what `Mode::Logs`/`Mode::Help` scroll.
        let height = terminal.size()?.height;
        app.viewport = ui::list_viewport_rows(height);
        app.overlay_viewport = height.saturating_sub(2);
        app.help_lines = ui::help_line_count();
        app.clamp_scroll();

        if needs_draw {
            terminal.draw(|f| ui::draw(f, app))?;
            needs_draw = false;
        }

        if event::poll(Duration::from_millis(TICK_MS))? {
            match event::read()? {
                // §4.1: filtering on Press is mandatory — without it a terminal
                // that reports key repeat/release doubles every keystroke.
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    let was_confirm = matches!(app.mode, Mode::Confirm(_));
                    let action = app.on_key(k);
                    // A confirmation must be answered by a keystroke made AFTER
                    // it was drawn. Anything already sitting in the tty buffer
                    // when `S` opened the modal is type-ahead aimed at the list,
                    // so drop it; `App::key_confirm`'s arming delay is the
                    // second, unit-testable half of the same guard.
                    if !was_confirm && matches!(app.mode, Mode::Confirm(_)) {
                        drain_pending_input()?;
                    }
                    match action {
                        Action::Quit => break,
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

/// Discard every input event already queued. Called the instant a destructive
/// confirmation opens.
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
        assert!(!cmd.contains("--light"));
        assert!(!cmd.contains("--socket"));

        let cli = Cli::try_parse_from(["ccmux", "--light", "-L", "ccmux"]).expect("parse");
        let cmd = sidebar_command(&cli, 34).expect("build");
        assert!(cmd.ends_with(" sidebar --session ccmux --width 34 --light --socket ccmux"));
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
}
