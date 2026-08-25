//! ccmux — a tmux-backed frontend for Claude Code sessions.
//!
//! SCAFFOLD: the CLI surface (SPEC §1.1) is real; `run_launcher` (§1.2),
//! `run_sidebar` and `event_loop` (§3.6, §4.1) are the Integrator lane's.

#![allow(unused)]

mod agents;
mod app;
mod model;
mod tmux;
mod ui;

use clap::Parser;

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

/// `--width` is clamped to 20..=120 before use, so a typo cannot produce an
/// unusable sidebar (SPEC §1.1).
pub const WIDTH_MIN: u16 = 20;
pub const WIDTH_MAX: u16 = 120;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Some(Cmd::Sidebar { interval }) => run_sidebar(&cli, interval),
        None => run_launcher(&cli),
    }
}

/// §1.2. Never enters raw mode.
fn run_launcher(_cli: &Cli) -> anyhow::Result<()> {
    todo!()
}

/// Raw mode + alternate screen + the event loop; teardown runs on every exit
/// path including a panic-free error return.
fn run_sidebar(_cli: &Cli, _interval_ms: u64) -> anyhow::Result<()> {
    todo!()
}

fn event_loop(
    _terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    _app: &mut app::App,
) -> anyhow::Result<()> {
    todo!()
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
}
