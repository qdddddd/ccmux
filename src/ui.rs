//! STUB — owner: UI lane. SPEC §3.4.
//!
//! **PURE RENDERING.** Takes `&App` and a `&mut Frame`, writes cells. It spawns
//! no process, opens no file, reads no clock beyond what `App` already carries,
//! and mutates nothing. Any `Command`, `std::fs`, or `&mut App` appearing in
//! this file is a spec violation.
//!
//! Consumes `app::{App, Mode, Confirm, Prompt, PromptKind, MsgLevel, LogsView}`,
//! `model::{Group, Row, Session, Kind, Status, format_age, shorten_cwd,
//! truncate_end}`, `tmux::PaneId` (for `Display` only).

use ratatui::{Frame, style::Color};

use crate::app::App;

/// Gruvbox, matching ~/projects/slurm-tui/src/palette.rs.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub fg: Color,
    pub gray: Color,
    pub dim: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub blue: Color,
    pub purple: Color,
    pub aqua: Color,
    pub orange: Color,
    pub sel_bg: Color,
}

impl Palette {
    pub fn dark() -> Self {
        todo!()
    }

    pub fn light() -> Self {
        todo!()
    }

    /// `app.dark ? dark() : light()`
    pub fn for_app(_app: &App) -> Self {
        todo!()
    }
}

/// THE entry point. Everything else in this module is private.
pub fn draw(_f: &mut Frame, _app: &App) {
    todo!()
}

/// Number of session rows the list viewport can show at `total_height`.
/// The single source of truth for viewport arithmetic. `main.rs` calls it once
/// per frame and writes the result into `App::viewport`, so `app.rs` never
/// imports `ui` and the dependency DAG stays acyclic.
/// Returns 0 when the area is degenerate.
pub fn list_viewport_rows(_total_height: u16) -> u16 {
    todo!()
}
