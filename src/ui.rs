//! Sidebar rendering. SPEC §3.4 (signatures) and §6 (the rendering spec).
//!
//! **PURE RENDERING.** Takes `&App` and a `&mut Frame`, writes cells. It spawns
//! no process, opens no file, reads no clock beyond what `App` already carries,
//! and mutates nothing. Any `Command`, `std::fs`, or `&mut App` appearing in
//! this file is a spec violation.
//!
//! Consumes `app::{App, Mode, Confirm, Prompt, PromptKind, MsgLevel, LogsView}`,
//! `model::{Group, Row, Session, Kind, Status, format_age, shorten_cwd,
//! truncate_end}`, `tmux::PaneId` (for `Display` only).
//!
//! Two implementation notes for the Integrator:
//!
//! 1. This module resolves the selected session, the "is open" flag, and the
//!    pane badge from `App`'s **public fields** rather than through
//!    `App::selected_session` / `is_open` / `pane_of` / `pane_index_of`. The
//!    logic mirrors §3.5's documented behaviour exactly (map first, then the
//!    transient `interactive_panes` cache). Reading fields keeps `ui::draw`
//!    testable — §10.1 mandates a no-panic matrix for this module, and that
//!    matrix cannot run while the accessors are `todo!()` in another lane.
//!    Swapping these private helpers for the accessors after integration is a
//!    behaviour-preserving change.
//! 2. `Mode::Help` scrolling reads `App::help_scroll`, a field separate from
//!    the list's `scroll` because `App::clamp_scroll` pins the latter to the
//!    session list's bounds every frame.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Confirm, LogsView, Mode, MsgLevel, Prompt, PromptKind};
use crate::model::{
    Group, Kind, Row, Session, State, Status, display_width, format_age, shorten_cwd,
    truncate_end,
};

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
        Self {
            fg: Color::Rgb(0xeb, 0xdb, 0xb2),
            // Contrast on the gruvbox dark ground (#282828), WCAG AA needs 4.5:1
            // for body text and 3:1 for secondary. The originals were gray 4.02:1
            // and dim 2.26:1 — dim was unreadable, so both moved up one gruvbox
            // step: fg4 and gray. See `palette_contrast_is_readable`.
            gray: Color::Rgb(0xa8, 0x99, 0x84), // 5.30:1
            dim: Color::Rgb(0x92, 0x83, 0x74),  // 4.02:1
            red: Color::Rgb(0xfb, 0x49, 0x34),
            green: Color::Rgb(0xb8, 0xbb, 0x26),
            yellow: Color::Rgb(0xfa, 0xbd, 0x2f),
            blue: Color::Rgb(0x83, 0xa5, 0x98),
            purple: Color::Rgb(0xd3, 0x86, 0x9b),
            aqua: Color::Rgb(0x8e, 0xc0, 0x7c),
            orange: Color::Rgb(0xfe, 0x80, 0x19),
            sel_bg: Color::Rgb(0x3c, 0x38, 0x36),
        }
    }

    pub fn light() -> Self {
        Self {
            fg: Color::Rgb(0x3c, 0x38, 0x36), // gruvbox fg1, 10.22:1
            // On the gruvbox light ground (#fbf1c7) the originals were gray
            // 3.24:1 and dim 4.29:1 — both below AA for body text, and the
            // ladder was inverted (dim darker than gray). Now fg > gray > dim.
            gray: Color::Rgb(0x50, 0x49, 0x45), // 7.78:1
            dim: Color::Rgb(0x66, 0x5c, 0x54),  // 5.74:1
            // The accents used to be gruvbox's DARK-theme hues, which fail
            // the house 4.0:1 floor on the light ground (#fbf1c7) — the theme
            // that ships by default: green 2.73, aqua 2.80, orange 3.41,
            // blue 3.73, purple 3.73, yellow 2.19. Six of seven were below the
            // floor, which is why every status glyph read as decoration. These
            // are gruvbox's light-theme "faded" hues; each clears 5.0:1 on the
            // ground and 4.1:1 on `sel_bg`. See `light_accents_are_readable`.
            red: Color::Rgb(0x9d, 0x00, 0x06),    // 7.60:1
            green: Color::Rgb(0x6d, 0x68, 0x0d),  // 5.09:1
            yellow: Color::Rgb(0x8d, 0x5c, 0x10), // 5.04:1
            blue: Color::Rgb(0x07, 0x66, 0x78),   // 5.82:1
            purple: Color::Rgb(0x8f, 0x3f, 0x71), // 5.94:1
            aqua: Color::Rgb(0x3d, 0x71, 0x51),   // 5.03:1
            orange: Color::Rgb(0xaf, 0x3a, 0x03), // 5.40:1
            sel_bg: Color::Rgb(0xeb, 0xdb, 0xb2),
        }
    }

    /// `app.dark ? dark() : light()`
    pub fn for_app(app: &App) -> Self {
        if app.dark { Self::dark() } else { Self::light() }
    }
}

// ── Layout (§6.1) ───────────────────────────────────────────────────────────

/// Number of session rows the list viewport can show at `total_height`.
/// The single source of truth for viewport arithmetic. `main.rs` calls it once
/// per frame and writes the result into `App::viewport`, so `app.rs` never
/// imports `ui` and the dependency DAG stays acyclic.
/// Returns 0 when the area is degenerate.
///
/// Implements §6.1's degradation table exactly:
///   H == 0 | H == 1        -> 0     (header only, or nothing)
///   H == 2                 -> 1     (header + list; no footer)
///   3 <= H < 12            -> H-2   (header + list + footer)
///   H >= 12                -> H-6   (also separator + 3-line detail block)
pub fn list_viewport_rows(total_height: u16) -> u16 {
    match total_height {
        0 | 1 => 0,
        2 => 1,
        3..=11 => total_height.saturating_sub(2),
        _ => total_height.saturating_sub(6),
    }
}

/// The resolved vertical slots for one frame. Every field is `None` when the
/// area is too small to carry it (§6.1).
#[derive(Debug, Clone, Copy, Default)]
struct Slots {
    header: Option<Rect>,
    list: Option<Rect>,
    sep: Option<Rect>,
    detail: Option<Rect>,
    footer: Option<Rect>,
}

fn slots(area: Rect) -> Slots {
    let mut s = Slots::default();
    if area.width == 0 || area.height == 0 {
        return s;
    }
    let h = area.height;
    let row = |y_off: u16, height: u16| Rect {
        x: area.x,
        y: area.y.saturating_add(y_off),
        width: area.width,
        height,
    };

    s.header = Some(row(0, 1));
    // The list always occupies rows 1..; its height is the single source of
    // truth shared with `App::viewport`.
    let list_h = list_viewport_rows(h);
    if list_h > 0 {
        s.list = Some(row(1, list_h));
    }
    if h >= 3 {
        s.footer = Some(row(h.saturating_sub(1), 1));
    }
    if h >= 12 {
        s.sep = Some(row(h.saturating_sub(5), 1));
        s.detail = Some(row(h.saturating_sub(4), 3));
    }
    s
}

// ── Entry point ─────────────────────────────────────────────────────────────

/// THE entry point. Everything else in this module is private.
pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let p = Palette::for_app(app);
    let s = slots(area);

    if let Some(r) = s.header {
        draw_header(f, r, app, &p);
    }
    if let Some(r) = s.list {
        draw_list(f, r, app, &p);
    }
    if let Some(r) = s.sep {
        // Inset by the margin so the sidebar's ONE rule terminates on the same
        // two columns every other line does.
        let w = r.width as usize;
        let m = margin(w);
        let rule = format!("{0}{1}{0}", " ".repeat(m), "─".repeat(w.saturating_sub(2 * m)));
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(rule, Style::default().fg(p.dim)))),
            r,
        );
    }
    // §6.8 AMENDMENT: a message wider than the sidebar takes over the detail
    // block and wraps, instead of losing its tail to a one-line truncation.
    // §8.5's refusal and "agent still running" wordings do not fit 34 columns,
    // and they are the two the operator most needs to read in full.
    match (s.detail, s.footer, overflow_message(app, area.width as usize, &p)) {
        (Some(d), Some(ft), Some((text, color))) => {
            let rect = Rect {
                x: d.x,
                y: d.y,
                width: d.width,
                height: ft.y.saturating_add(ft.height).saturating_sub(d.y),
            };
            f.render_widget(
                Paragraph::new(text)
                    .style(Style::default().fg(color))
                    .wrap(Wrap { trim: true }),
                rect,
            );
        }
        _ => {
            if let Some(r) = s.detail {
                draw_detail(f, r, app, &p);
            }
            if let Some(r) = s.footer {
                draw_footer(f, r, app, &p);
            }
        }
    }

    // Overlays always render INSIDE the sidebar rect — ccmux never draws over a
    // Claude pane (§6.7).
    match &app.mode {
        Mode::Help => draw_help(f, area, app, &p),
        Mode::Logs => draw_logs(f, area, app.logs.as_ref(), &p),
        Mode::Confirm(c) => draw_confirm(f, area, c, &p),
        Mode::Prompt(kind) => draw_prompt(f, area, *kind, app.prompt.as_ref(), &p),
        Mode::Normal | Mode::Filter => {}
    }
}

// ── Header (§6.2) ───────────────────────────────────────────────────────────

fn draw_header(f: &mut Frame, area: Rect, app: &App, p: &Palette) {
    let w = area.width as usize;
    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0usize;

    push(&mut spans, &mut used, " ", Style::default());
    push(
        &mut spans,
        &mut used,
        "ccmux",
        Style::default().fg(p.aqua).add_modifier(Modifier::BOLD),
    );

    if w >= 14 {
        let total = app.sessions.len();
        let matching = app
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Session { .. }))
            .count();
        // §6.2 AMENDMENT: the ratio shows whenever the list is showing fewer
        // than all sessions, not only when `/` is active — `a` (hide Completed)
        // also hides rows, and a bare total then contradicts the visible list.
        let count = if matching == total {
            format!("{total}")
        } else {
            format!("{matching}/{total}")
        };
        let text = if w >= 26 {
            format!("  {count} session{}", if total == 1 { "" } else { "s" })
        } else {
            format!("  {count}")
        };
        push(&mut spans, &mut used, &text, Style::default().fg(p.gray));
    }

    // Poll indicator, right-aligned at column W-2. Priority: a live poll error
    // outranks the structural "outside tmux" degradation, because the error is
    // the transient, actionable condition. Dropped below W=14 to match §6.2's
    // third sample line, which renders the wordmark alone.
    if w >= 14 {
        let (glyph, style) = if app.poll_error.is_some() {
            ("●", Style::default().fg(p.red))
        } else if app.degraded {
            ("○", Style::default().fg(p.yellow))
        } else {
            ("●", Style::default().fg(p.dim))
        };
        let target = w.saturating_sub(2);
        if used < target {
            let pad = " ".repeat(target - used);
            push(&mut spans, &mut used, &pad, Style::default());
            push(&mut spans, &mut used, glyph, style);
        }
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn push(spans: &mut Vec<Span<'static>>, used: &mut usize, text: &str, style: Style) {
    *used += display_width(text);
    spans.push(Span::styled(text.to_string(), style));
}

// ── List (§6.3, §6.4) ───────────────────────────────────────────────────────

// ── The one system rule (§0) ────────────────────────────────────────────────
//
// **Column W-1 is the rail; column W is the margin.** Every line that carries
// one small fact puts that fact's LAST CELL on column W-1 and leaves column W
// blank: the session age, the group count, the detail block's status word, the
// header's poll dot, the footer's help key. Nothing else touches those two
// columns. `draw_header` already right-aligns its poll dot at `w - 2`, i.e.
// column W-1, which is why it needs no edit.

/// The fixed left gutter: cap/marker(1) + pane badge(1) + glyph(1) + space(1).
/// It is FOUR columns at every width that has a gutter and never varies, which
/// is what puts the status glyph on column 3 and the name on column 5 of every
/// row alike. See `session_line` for why the badge cannot be allowed to grow.
const GUTTER: usize = 4;
/// One blank column between the name field and the rail.
const GAP: usize = 1;
/// Nominal rail field. A wider age (`9999d`) spills into that row's own name.
const NUM_W: usize = 3;
/// The gutter and the right margin exist at or above this width.
const MARGIN_MIN: usize = 20;
/// The age rail exists at or above this width.
const RAIL_MIN: usize = 28;
/// Detail labels are right-flushed into columns 1..4.
const LABEL_W: usize = 4;
/// `LABEL_W + 2` — detail values start on column 7.
const VALUE_COL: usize = LABEL_W + 2;
/// An 8-column short id plus an ellipsis. Below this the detail block's status
/// word yields the rail: the glyph on the row already states the status, while
/// the short id is stated nowhere else and is what you type into `claude`.
const MIN_BODY: usize = 9;

/// 1 when the layout has a right margin (and therefore a gutter), else 0.
fn margin(w: usize) -> usize {
    if w >= MARGIN_MIN { 1 } else { 0 }
}

/// Group -> name weight. The ladder `palette_contrast_is_readable` already
/// guarantees (fg > gray > dim), spent on the one axis the list is sorted by,
/// so the weight still names the group when its header has scrolled off the
/// top. Interactive sessions keep their hue: a kind is not a tier.
fn name_tier(sess: &Session, p: &Palette) -> Color {
    if sess.kind == Kind::Interactive {
        return p.purple;
    }
    match sess.group() {
        Group::Working => p.fg,
        Group::Idle => p.gray,
        Group::Completed => p.dim,
    }
}

fn group_accent(g: Group, p: &Palette) -> Color {
    match g {
        Group::Working => p.orange,
        Group::Idle => p.blue,
        Group::Completed => p.gray,
    }
}

/// `── Title` from column 5, the count's last cell on the rail (column W-1).
///
/// The full-width `────────` rule this used to draw is gone: `Row::Spacer`
/// already separates one group from the next, and the rule was doing that job a
/// second time, louder. The accent survives as a two-cell chip in columns 2..3,
/// so per-group hue identity is kept while the readable text moves to `p.gray`
/// (7.78:1 light) — painting the title itself in the accent put the Working
/// header at 3.41:1 on the default ground. The title starts on column 5, the
/// same column the session names start on, so it reads as a column heading.
fn group_header_line(g: Group, count: usize, w: usize, p: &Palette) -> Line<'static> {
    if w == 0 {
        return Line::from(Vec::<Span>::new());
    }
    let cs = count.to_string();
    if w < MARGIN_MIN {
        // The narrow rung, with the parens dropped: the count is a bare number
        // at every width, and `Completed 12` fits 12 columns where
        // `Completed (12)` did not.
        return Line::from(Span::styled(
            truncate_end(&format!("{} {}", g.title(), count), w),
            Style::default().fg(group_accent(g, p)),
        ));
    }
    let m = margin(w);
    let cw = display_width(&cs);
    // " " + "──" + " " => the label starts on column 5.
    const USED0: usize = 4;
    let title = truncate_end(g.title(), w.saturating_sub(USED0 + m + cw + 1));
    let used = USED0 + display_width(&title);
    let stop = w.saturating_sub(m + cw); // the count's first column - 1
    let mut spans = vec![
        Span::styled(" ".to_string(), Style::default()),
        Span::styled("──".to_string(), Style::default().fg(group_accent(g, p))),
        Span::styled(" ".to_string(), Style::default()),
        Span::styled(title, Style::default().fg(p.gray).add_modifier(Modifier::BOLD)),
    ];
    if stop > used {
        spans.push(Span::styled(" ".repeat(stop - used), Style::default()));
        spans.push(Span::styled(cs, Style::default().fg(p.dim)));
    }
    let used: usize = spans.iter().map(|sp| display_width(&sp.content)).sum();
    pad_to(&mut spans, used, w, Style::default());
    Line::from(spans)
}

/// §6.4's status glyph table.
/// SPEC §6.4's glyph table, resolved top-down.
///
/// SPEC NOTE: `Completed` is matched BEFORE the `Status::Unknown` /
/// `State::Unknown` fallback. `claude agents --json` omits `status` on every
/// `state: "done"` row (verified against the live payload: 10 of 10), so
/// checking unknown first would make the table's `Completed → ✓` row
/// unreachable for every real completed session and paint the whole group
/// purple `?`. `state: "done"` is definitive knowledge, not forward-compat
/// territory; the `?` fallback keeps its job for a status or state value the
/// CLI invents that this build does not recognize (e.g. `state: "stopped"`).
fn status_glyph(sess: &Session, p: &Palette) -> (&'static str, Color) {
    if matches!(sess.state, Some(State::Done)) {
        return ("✓", p.green);
    }
    if matches!(sess.state, Some(State::Stopped)) {
        return ("■", p.gray);
    }
    let unknown_status = matches!(sess.status, Status::Unknown(_));
    let unknown_state = matches!(sess.state, Some(State::Unknown(_)));
    if unknown_status || unknown_state {
        return ("?", p.purple);
    }
    match sess.group() {
        Group::Working => match sess.status {
            Status::Busy => ("●", p.orange),
            _ => ("◐", p.blue),
        },
        Group::Idle => ("○", p.gray),
        Group::Completed => ("✓", p.green),
    }
}

/// One list row, on the one grid: gutter(4) | name | gap(1) | rail(3) | margin(1).
///
/// The pane badge lives in gutter column 2, immediately right of the marker,
/// so the name field's right edge no longer moves when a session is opened and
/// every line in the sidebar carries exactly ONE token on the rail. ONE
/// documented spill, the policy the age field already used: an age of `100d`
/// or more takes one to three columns from that row's own name. It never
/// shifts a neighbouring row, because the age's last cell is anchored to
/// column W-1 and grows leftward into a field only that row owns.
///
/// The badge takes no such licence. It is one column wide always — the gutter
/// is the row's LEFT edge, so a badge that grew would push this row's glyph
/// and name right while its neighbours stayed put, and the eye reads a broken
/// left edge as broken far more readily than a short name.
fn session_line(app: &App, sess: &Session, selected: bool, w: usize, p: &Palette) -> Line<'static> {
    if w == 0 {
        return Line::from(Vec::<Span>::new());
    }
    let (glyph, glyph_color) = status_glyph(sess, p);
    let base = if selected {
        Style::default().bg(p.sel_bg)
    } else {
        Style::default()
    };
    // Selection forces `p.fg`: `p.dim` is 3.16:1 on the dark `sel_bg`, so a
    // selected Completed row would otherwise be dim-on-band.
    let name_style = if selected {
        base.fg(p.fg).add_modifier(Modifier::BOLD)
    } else {
        base.fg(name_tier(sess, p))
    };

    // W < 6: glyph only.
    if w < 6 {
        let mut spans = vec![Span::styled(glyph.to_string(), base.fg(glyph_color))];
        pad_to(&mut spans, 1, w, base);
        return Line::from(spans);
    }

    // 6..=19: glyph + space + name (truncated to W-2), no gutter, no margin.
    if w < MARGIN_MIN {
        let mut spans = vec![
            Span::styled(glyph.to_string(), base.fg(glyph_color)),
            Span::styled(" ".to_string(), base),
        ];
        let name = truncate_end(&sess.name, w.saturating_sub(2));
        let used = 2 + display_width(&name);
        spans.push(Span::styled(name, name_style));
        pad_to(&mut spans, used, w, base);
        return Line::from(spans);
    }

    let m = margin(w);
    let open = is_open(app, &sess.session_id);
    let idx = if open {
        pane_index_for(app, &sess.session_id)
    } else {
        None
    };
    // Ten or more panes in one tmux window is exactly where a per-window index
    // stops being something you can eyeball anyway, so the badge degrades to
    // `+` — "open, somewhere further down this window" — rather than taking a
    // second column and shifting the row off the grid. The exact index is
    // still stated where it is actionable: the `opened <name> in pane N` flash.
    let badge = idx.map(|i| char::from_digit(i, 10).unwrap_or('+'));
    let (age, field) = if w >= RAIL_MIN {
        let a = format_age(sess.started_at, app.now_ms);
        let f = display_width(&a).max(NUM_W);
        (Some(a), f)
    } else {
        (None, 0)
    };
    let right = if age.is_some() { GAP + field } else { 0 };
    let name_budget = w.saturating_sub(GUTTER + right + m);
    let name = truncate_end(&sess.name, name_budget);

    let mut spans: Vec<Span> = Vec::with_capacity(8);
    // Column 1. Open wins over selected: the aqua `▌` is the one thing that
    // says "this session is on screen", and the selected row is already carried
    // by the band, the BOLD name and the promoted age. `▏` (U+258F) is a
    // hairline where `▌` (U+258C) is a thick bar — different weight, not just
    // a different colour.
    if open {
        spans.push(Span::styled("▌".to_string(), base.fg(p.aqua)));
    } else if selected {
        spans.push(Span::styled("▏".to_string(), base.fg(p.fg)));
    } else {
        spans.push(Span::styled(" ".to_string(), base));
    }
    // Column 2. An open row whose index does not resolve (no pane inventory
    // yet) keeps the marker and leaves this column blank.
    match badge {
        Some(b) => spans.push(Span::styled(b.to_string(), base.fg(p.fg))),
        None => spans.push(Span::styled(" ".to_string(), base)),
    }
    spans.push(Span::styled(glyph.to_string(), base.fg(glyph_color)));
    spans.push(Span::styled(" ".to_string(), base));
    let mut used = GUTTER + display_width(&name);
    spans.push(Span::styled(name, name_style));

    // Pad out to the rail's lead so the age lands flush on column W-1.
    let rail_start = w.saturating_sub(m + right);
    if used < rail_start {
        spans.push(Span::styled(" ".repeat(rail_start - used), base));
        used = rail_start;
    }
    if let Some(a) = age {
        let aw = display_width(&a);
        let lead = (GAP + field).saturating_sub(aw);
        spans.push(Span::styled(" ".repeat(lead), base));
        // `p.dim` on `sel_bg` is 3.16:1 in the dark theme, so the selected
        // row's age promotes one rung. This is a contrast fix, not a flourish.
        spans.push(Span::styled(
            a,
            base.fg(if selected { p.gray } else { p.dim }),
        ));
        used += lead + aw;
    }

    // The selection bar must be solid across the full width, margin included.
    pad_to(&mut spans, used, w, base);
    Line::from(spans)
}

fn pad_to(spans: &mut Vec<Span<'static>>, used: usize, w: usize, base: Style) {
    if used < w {
        spans.push(Span::styled(" ".repeat(w - used), base));
    }
}

fn draw_list(f: &mut Frame, area: Rect, app: &App, p: &Palette) {
    let w = area.width as usize;
    let height = area.height as usize;
    if w == 0 || height == 0 {
        return;
    }

    let start = app.scroll.min(app.rows.len());
    let end = start.saturating_add(height).min(app.rows.len());
    let mut lines: Vec<Line> = Vec::with_capacity(height);

    for (i, row) in app.rows[start..end].iter().enumerate() {
        let abs = start + i;
        match row {
            Row::Header { group, count } => lines.push(group_header_line(*group, *count, w, p)),
            Row::Spacer => lines.push(Line::from("")),
            Row::Session { idx } => match app.sessions.get(*idx) {
                Some(sess) => lines.push(session_line(app, sess, abs == app.selected, w, p)),
                None => lines.push(Line::from(Span::styled(
                    truncate_end("  <stale row>", w),
                    Style::default().fg(p.dim),
                ))),
            },
        }
    }

    if lines.is_empty() {
        // §9.3: centred in the list area, vertically and horizontally.
        let text = if app.sessions.is_empty() {
            "no sessions"
        } else {
            "no matches"
        };
        for _ in 0..(height / 2) {
            lines.push(Line::from(""));
        }
        lines.push(
            Line::from(Span::styled(
                truncate_end(text, w),
                Style::default().fg(p.dim),
            ))
            .alignment(Alignment::Center),
        );
    }

    f.render_widget(Paragraph::new(lines), area);
}

// ── Detail block (§6.5) ─────────────────────────────────────────────────────

/// Labels right-flushed into columns 1..4, values from column 7, the status
/// word right-flushed to the rail.
///
/// The `· pane N` suffix is gone: it cost 9 columns to restate what the row's
/// own `▌`+digit gutter says at every width the gutter exists, and reclaiming
/// them is what stops the id line truncating at the default 34 columns.
fn draw_detail(f: &mut Frame, area: Rect, app: &App, p: &Palette) {
    let w = area.width as usize;
    if w == 0 || area.height == 0 {
        return;
    }

    let Some(sess) = selected_session(app) else {
        // Empty selection renders three blank lines.
        f.render_widget(Paragraph::new(vec![Line::from(""), Line::from(""), Line::from("")]), area);
        return;
    };

    let short = sess.id.clone().unwrap_or_else(|| "—".to_string());
    let kind = match sess.kind {
        Kind::Background => "background",
        Kind::Interactive => "interactive",
    };
    // `state: "done"` rows carry no `status` key (verified: 10 of 12 live
    // completed sessions), so the group is resolved FIRST here for the same
    // reason `status_glyph` resolves it first — otherwise the list says
    // "Completed" while the detail block says "?".
    let status = if matches!(sess.state, Some(State::Done)) {
        "done".to_string()
    } else if matches!(sess.state, Some(State::Stopped)) {
        "stopped".to_string()
    } else {
        match &sess.status {
            Status::Busy => "busy".to_string(),
            Status::Idle => "idle".to_string(),
            Status::Unknown(s) if s.is_empty() => "?".to_string(),
            Status::Unknown(s) => s.clone(),
        }
    };

    // Below 10 columns the label rail would leave nothing for a value, so the
    // labels drop and the three bare values render at column 1.
    if w < 10 {
        let lines = vec![
            Line::from(Span::styled(
                truncate_end(&sess.name, w),
                Style::default().fg(p.fg).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                truncate_end(&short, w),
                Style::default().fg(p.gray),
            )),
            Line::from(Span::styled(
                shorten_cwd(&sess.cwd, app.home.as_deref(), w),
                Style::default().fg(p.gray),
            )),
        ];
        f.render_widget(Paragraph::new(lines), area);
        return;
    }

    let m = margin(w);
    let budget = w.saturating_sub(VALUE_COL + m);
    let label = |t: &str| {
        Span::styled(
            format!("{t:>LABEL_W$}  "),
            Style::default().fg(p.dim),
        )
    };

    let l1 = Line::from(vec![
        label("name"),
        Span::styled(
            truncate_end(&sess.name, budget),
            Style::default().fg(p.fg).add_modifier(Modifier::BOLD),
        ),
    ]);

    // The status word is redundant with the row's own status glyph; the 8-hex
    // short id is stated nowhere else in the UI. So when the two cannot both
    // fit, the status yields and the id survives.
    let body = format!("{short} {kind}");
    let st = truncate_end(&status, 8);
    let sw = display_width(&st);
    let mut l2 = vec![label("id")];
    if budget >= sw + 1 + MIN_BODY {
        let left = truncate_end(&body, budget - sw - 1);
        let used = VALUE_COL + display_width(&left);
        l2.push(Span::styled(left, Style::default().fg(p.gray)));
        let stop = w.saturating_sub(m + sw);
        if stop > used {
            l2.push(Span::styled(" ".repeat(stop - used), Style::default()));
        }
        l2.push(Span::styled(st, Style::default().fg(p.gray)));
    } else {
        l2.push(Span::styled(
            truncate_end(&body, budget),
            Style::default().fg(p.gray),
        ));
    }

    let l3 = Line::from(vec![
        label("cwd"),
        Span::styled(
            shorten_cwd(&sess.cwd, app.home.as_deref(), budget),
            Style::default().fg(p.gray),
        ),
    ]);

    f.render_widget(Paragraph::new(vec![l1, Line::from(l2), l3]), area);
}

// ── Footer (§6.8) ───────────────────────────────────────────────────────────

/// The footer text that does NOT fit on one row, with its colour — `None` when
/// the footer has nothing to say, when what it says fits, or when `Mode::Filter`
/// owns the footer (§6.8 priority 1).
fn overflow_message(app: &App, w: usize, p: &Palette) -> Option<(String, Color)> {
    if w == 0 || app.mode == Mode::Filter {
        return None;
    }
    let (text, color) = match (&app.message, &app.poll_error) {
        (Some((text, level)), _) => (
            text.clone(),
            match level {
                MsgLevel::Info => p.green,
                MsgLevel::Warn => p.yellow,
                MsgLevel::Error => p.red,
            },
        ),
        (None, Some(err)) => (format!("agents: {}", err.lines().next().unwrap_or("")), p.red),
        (None, None) => return None,
    };
    if display_width(&text) <= w {
        return None;
    }
    Some((text, color))
}

/// Whole key/label pairs, greedily filled from column 1. A pair is never split,
/// which is what stops the footer clipping mid-word to `x cl…`. `j/k move` is
/// deliberately absent — it is the one hint a TUI user never needs told, and
/// dropping it is what makes three whole pairs fit at the default 34 columns.
/// Footer hint pairs, most-wanted first: `draw_footer` fits whole pairs from
/// the left and stops at the first one that will not fit, so the order is the
/// priority order. `d/u hide` sits fourth — above `S stop`, which is behind a
/// confirm modal and cannot fire by accident — because `d` is the one key here
/// that makes a row vanish on a single press, and the pair names its own undo.
/// At the default 34 columns the budget runs out after `x close` and no fourth
/// pair renders at all, so a narrow sidebar discovers `d` through `?` and the
/// README, exactly as it already discovers `S`.
const HINTS: &[(&str, &str)] = &[
    ("⏎", "open"),
    ("o/s", "split"),
    ("x", "close"),
    ("d/u", "hide"),
    ("S", "stop"),
    ("n", "new"),
    ("L", "logs"),
    ("/", "filter"),
];
/// Pinned to the rail at every width that can hold it.
const HELP_HINT: &str = "? help";

fn draw_footer(f: &mut Frame, area: Rect, app: &App, p: &Palette) {
    let w = area.width as usize;
    if w == 0 {
        return;
    }

    // 1. Filter mode owns the footer.
    if app.mode == Mode::Filter {
        let text = format!("/{}▏", app.filter);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_end(&text, w),
                Style::default().fg(p.yellow),
            ))),
            area,
        );
        return;
    }

    // 2. A flashed message. Its 4s lifetime is `app.rs`'s business: rendering
    //    never reads a clock, only `app.message`.
    if let Some((text, level)) = &app.message {
        let color = match level {
            MsgLevel::Info => p.green,
            MsgLevel::Warn => p.yellow,
            MsgLevel::Error => p.red,
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_end(text, w),
                Style::default().fg(color),
            ))),
            area,
        );
        return;
    }

    // 3. A standing poll error.
    if let Some(err) = &app.poll_error {
        let text = format!("agents: {}", err.lines().next().unwrap_or(""));
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_end(&text, w),
                Style::default().fg(p.red),
            ))),
            area,
        );
        return;
    }

    // 4. The hint line: whole pairs from column 1, `? help` pinned to the rail.
    let hw = display_width(HELP_HINT);
    if w < hw + 2 {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_end(HELP_HINT, w),
                Style::default().fg(p.dim),
            ))),
            area,
        );
        return;
    }
    let m = margin(w);
    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0usize;
    // The pairs only start appearing once the layout has a gutter and a margin
    // at all; below that the pinned help key is the whole footer.
    if w >= MARGIN_MIN {
        let budget = w - m - hw - 1; // the rail, plus at least one blank column
        for (key, text) in HINTS {
            let pair = display_width(key) + 1 + display_width(text);
            let want = if used == 0 { pair } else { used + 2 + pair };
            if want > budget {
                break;
            }
            if used > 0 {
                spans.push(Span::styled("  ".to_string(), Style::default()));
            }
            spans.push(Span::styled((*key).to_string(), Style::default().fg(p.gray)));
            spans.push(Span::styled(format!(" {text}"), Style::default().fg(p.dim)));
            used = want;
        }
    }
    spans.push(Span::styled(
        " ".repeat(w.saturating_sub(m + hw + used)),
        Style::default(),
    ));
    spans.push(Span::styled("?".to_string(), Style::default().fg(p.gray)));
    spans.push(Span::styled(" help".to_string(), Style::default().fg(p.dim)));
    if m > 0 {
        spans.push(Span::styled(" ".repeat(m), Style::default()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── Overlays (§6.7) ─────────────────────────────────────────────────────────

/// A centred rect that can never exceed `area` and never has a negative origin.
fn centered(area: Rect, want_w: u16, want_h: u16) -> Rect {
    let w = want_w.min(area.width);
    let h = want_h.min(area.height);
    Rect {
        x: area.x.saturating_add(area.width.saturating_sub(w) / 2),
        y: area.y.saturating_add(area.height.saturating_sub(h) / 2),
        width: w,
        height: h,
    }
}

fn overlay_block(title: &str, border: Color, p: &Palette) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(p.fg).add_modifier(Modifier::BOLD),
        ))
}

/// `area` minus the block's border, saturating so a 1x1 or 2x2 rect yields an
/// empty inner rect instead of panicking.
fn inner(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

/// §8.1's binding table, rendered by the `?` overlay.
const KEYS: &[(&str, &str)] = &[
    ("j / Down", "next session"),
    ("k / Up", "previous session"),
    ("g / G", "first / last"),
    ("Ctrl-d/u", "half page down / up"),
    ("Tab", "next group"),
    ("S-Tab", "previous group"),
    ("Enter", "open or jump to pane"),
    ("o", "open in vertical split"),
    ("s", "open in horizontal split"),
    ("x", "close pane (agent lives)"),
    ("S", "stop session (confirm)"),
    ("n", "new background session"),
    ("c", "new interactive session"),
    ("L", "logs for this session"),
    ("d", "hide row (pane stays)"),
    ("u", "undo the last hide"),
    ("/", "filter"),
    ("a", "toggle Completed group"),
    ("r", "force refresh"),
    ("?", "this help"),
    ("q", "quit sidebar"),
    ("Esc", "clear filter"),
    ("Ctrl-c", "quit from any mode"),
];

/// Number of lines the `?` overlay renders. `main.rs` copies it into
/// `App::help_lines` each frame so `app.rs` can clamp `help_scroll` against the
/// real content without importing `ui` (the DAG stays acyclic).
pub fn help_line_count() -> usize {
    KEYS.len()
}

fn draw_help(f: &mut Frame, area: Rect, app: &App, p: &Palette) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    f.render_widget(Clear, area);
    let block = overlay_block(" keys ", p.aqua, p);
    let body = inner(area);
    f.render_widget(block, area);
    if body.width == 0 || body.height == 0 {
        return;
    }

    let w = body.width as usize;
    let two_col = w >= 30;
    let key_w = 10usize;
    let all: Vec<Line> = KEYS
        .iter()
        .map(|(k, a)| {
            if two_col {
                let key = truncate_end(k, key_w);
                let pad = key_w.saturating_sub(display_width(&key));
                Line::from(vec![
                    Span::styled(key, Style::default().fg(p.yellow)),
                    Span::styled(" ".repeat(pad), Style::default()),
                    Span::styled(
                        truncate_end(a, w.saturating_sub(key_w)),
                        Style::default().fg(p.fg),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled((*k).to_string(), Style::default().fg(p.yellow)),
                    Span::styled(
                        truncate_end(&format!(" {a}"), w.saturating_sub(display_width(k))),
                        Style::default().fg(p.fg),
                    ),
                ])
            }
        })
        .collect();

    // `App` caps `help_scroll` without knowing the overlay's height; clamp to
    // the real content here so an out-of-range value can never blank the
    // overlay or index out of bounds.
    let visible = body.height as usize;
    let max_scroll = all.len().saturating_sub(visible);
    let start = app.help_scroll.min(max_scroll);
    let end = start.saturating_add(visible).min(all.len());
    f.render_widget(Paragraph::new(all[start..end].to_vec()), body);
}

fn draw_logs(f: &mut Frame, area: Rect, logs: Option<&LogsView>, p: &Palette) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    f.render_widget(Clear, area);
    let title = match logs {
        Some(l) if !l.title.is_empty() => format!(" {} ", truncate_end(&l.title, 24)),
        _ => " logs ".to_string(),
    };
    let body = inner(area);
    f.render_widget(overlay_block(&title, p.blue, p), area);
    if body.width == 0 || body.height == 0 {
        return;
    }

    let Some(l) = logs else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_end("no log output", body.width as usize),
                Style::default().fg(p.dim),
            ))),
            body,
        );
        return;
    };

    let w = body.width as usize;
    let visible = body.height as usize;
    let max_scroll = l.lines.len().saturating_sub(visible);
    let start = l.scroll.min(max_scroll);
    let end = start.saturating_add(visible).min(l.lines.len());
    let lines: Vec<Line> = l.lines[start..end]
        .iter()
        .map(|s| Line::from(Span::styled(truncate_end(s, w), Style::default().fg(p.fg))))
        .collect();
    f.render_widget(Paragraph::new(lines), body);
}

fn draw_confirm(f: &mut Frame, area: Rect, confirm: &Confirm, p: &Palette) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // §8.2: the modal renders the payload CAPTURED when `S` was pressed, never
    // the live selection — a poll landing between `S` and `y` must not change
    // what the operator is being asked about.
    let Confirm::StopSession { short_id, name, .. } = confirm;

    let rect = centered(area, area.width.min(34), 13);
    f.render_widget(Clear, rect);
    let body = inner(rect);
    f.render_widget(overlay_block(" stop session ", p.red, p), rect);
    if body.width == 0 || body.height == 0 {
        return;
    }

    let w = body.width as usize;
    let id = if short_id.is_empty() { "—" } else { short_id.as_str() };
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            truncate_end(&format!(" {name}"), w),
            Style::default().fg(p.fg).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            truncate_end(&format!(" {id}"), w),
            Style::default().fg(p.gray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " Stops the agent. The".to_string(),
            Style::default().fg(p.gray),
        )),
        Line::from(Span::styled(
            " conversation is kept;".to_string(),
            Style::default().fg(p.gray),
        )),
        Line::from(Span::styled(
            " resume with Enter later.".to_string(),
            Style::default().fg(p.gray),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" y".to_string(), Style::default().fg(p.red).add_modifier(Modifier::BOLD)),
            Span::styled(": stop     ".to_string(), Style::default().fg(p.gray)),
            Span::styled("n/Esc".to_string(), Style::default().fg(p.fg)),
            Span::styled(": cancel".to_string(), Style::default().fg(p.gray)),
        ]),
    ];
    let take = lines.len().min(body.height as usize);
    f.render_widget(
        Paragraph::new(lines[..take].to_vec()).wrap(Wrap { trim: false }),
        body,
    );
}

fn draw_prompt(f: &mut Frame, area: Rect, kind: PromptKind, prompt: Option<&Prompt>, p: &Palette) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let title = match kind {
        PromptKind::NewBackground => " new background session ",
        PromptKind::NewInteractive => " new interactive session ",
    };
    let labels: &[&str] = match kind {
        PromptKind::NewBackground => &["cwd ", "task"],
        PromptKind::NewInteractive => &["cwd "],
    };

    let want_h = (labels.len() as u16).saturating_add(5);
    let rect = centered(area, area.width.min(36), want_h);
    f.render_widget(Clear, rect);
    let body = inner(rect);
    f.render_widget(overlay_block(title, p.yellow, p), rect);
    if body.width == 0 || body.height == 0 {
        return;
    }

    let w = body.width as usize;
    let mut lines: Vec<Line> = vec![Line::from("")];
    for (i, label) in labels.iter().enumerate() {
        let focused = prompt.map(|pr| pr.focus == i).unwrap_or(i == 0);
        let value = prompt
            .and_then(|pr| pr.fields.get(i))
            .cloned()
            .unwrap_or_default();
        let prefix = if focused { "> " } else { "  " };
        let label_style = if focused {
            Style::default().fg(p.aqua)
        } else {
            Style::default().fg(p.dim)
        };
        let value_style = if focused {
            Style::default().fg(p.fg)
        } else {
            Style::default().fg(p.dim)
        };
        // prefix(2) + label + space
        let head = display_width(prefix) + display_width(label) + 1;
        let budget = w.saturating_sub(head);

        let mut spans = vec![
            Span::styled(prefix.to_string(), label_style),
            Span::styled((*label).to_string(), label_style),
            Span::styled(" ".to_string(), Style::default()),
        ];

        if focused {
            let cursor = prompt.map(|pr| pr.cursor).unwrap_or(0).min(value.chars().count());
            // Keep the cursor inside the visible window on a long value.
            let chars: Vec<char> = value.chars().collect();
            let win = budget.saturating_sub(1);
            let start = if win == 0 { 0 } else { cursor.saturating_sub(win) };
            let before: String = chars[start..cursor].iter().collect();
            let at: String = chars.get(cursor).map(|c| c.to_string()).unwrap_or_else(|| " ".into());
            let after: String = chars.get(cursor.saturating_add(1)..).map(|s| s.iter().collect()).unwrap_or_default();
            if budget > 0 {
                spans.push(Span::styled(truncate_end(&before, budget), value_style));
                let left = budget.saturating_sub(display_width(&before));
                if left > 0 {
                    spans.push(Span::styled(at, value_style.add_modifier(Modifier::REVERSED)));
                    let left = left.saturating_sub(1);
                    if left > 0 {
                        spans.push(Span::styled(truncate_end(&after, left), value_style));
                    }
                }
            }
        } else {
            spans.push(Span::styled(truncate_end(&value, budget), value_style));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        truncate_end(" Tab: field   ⏎ run   Esc: cancel", w),
        Style::default().fg(p.gray),
    )));

    let take = lines.len().min(body.height as usize);
    f.render_widget(Paragraph::new(lines[..take].to_vec()), body);
}

// ── Private read-only views over `App` (see the module note) ────────────────

fn selected_session(app: &App) -> Option<&Session> {
    match app.rows.get(app.selected) {
        Some(Row::Session { idx }) => app.sessions.get(*idx),
        _ => None,
    }
}

/// The pane id string showing `session_id`, mirroring §3.5's documented order:
/// the reconciled `@ccmux_map` first, then the transient `interactive_panes`
/// cache. Ties inside the map break on the numeric part of `%N`.
fn pane_key_for(app: &App, session_id: &str) -> Option<String> {
    let mut best: Option<(u64, &String)> = None;
    for (pane, entry) in app.map.panes.iter() {
        if entry.session_id != session_id {
            continue;
        }
        let n = pane
            .strip_prefix('%')
            .and_then(|d| d.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        if best.map(|(bn, _)| n < bn).unwrap_or(true) {
            best = Some((n, pane));
        }
    }
    if let Some((_, pane)) = best {
        return Some(pane.clone());
    }
    app.interactive_panes
        .get(session_id)
        .map(|p| p.as_str().to_string())
}

fn is_open(app: &App, session_id: &str) -> bool {
    pane_key_for(app, session_id).is_some()
}

fn pane_index_for(app: &App, session_id: &str) -> Option<u32> {
    let key = pane_key_for(app, session_id)?;
    app.panes
        .iter()
        .find(|i| i.id.as_str() == key)
        .map(|i| i.index)
}

// ── Tests (§10.1: the no-panic matrix) ──────────────────────────────────────
//
// `App` is built by struct literal from its public fields, NOT via `App::new`,
// and rendering resolves selection/open-state through this module's private
// helpers. Both choices keep these tests independent of the Integrator lane's
// in-flight `todo!()` bodies, which is the only way the mandated no-panic
// matrix can actually run.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Session, State, Status, build_rows};
    use crate::tmux::{PaneEntry, PaneId, PaneInfo, PaneMap};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    fn sess(n: usize, kind: Kind, status: Status, state: Option<State>) -> Session {
        Session {
            pid: 1000 + n as i32,
            id: match kind {
                Kind::Background => Some(format!("{n:08x}")),
                Kind::Interactive => None,
            },
            session_id: format!("uuid-{n:04}"),
            cwd: "/home/dev/projects/shared/Foundation/price-sanitize".into(),
            kind,
            started_at: 1_000_000 + n as i64 * 1000,
            name: format!("session number {n} with a long-ish name"),
            status,
            state,
        }
    }

    fn app_with(sessions: Vec<Session>) -> App {
        let rows = build_rows(&sessions, "", true, &[]);
        App {
            tmux_session: "ccmux".into(),
            sidebar_width: 34,
            interval: Duration::from_millis(2500),
            dark: true,
            home: Some("/home/dev".into()),
            sessions,
            rows,
            now_ms: 5_000_000,
            selected: 1,
            selected_key: None,
            scroll: 0,
            viewport: 10,
            help_scroll: 0,
            overlay_viewport: 20,
            help_lines: help_line_count(),
            filter: String::new(),
            show_completed: true,
            hidden: crate::tmux::HiddenSet::new(),
            hidden_dirty: false,
            hidden_absent: std::collections::BTreeSet::new(),
            mode: Mode::Normal,
            prompt: None,
            logs: None,
            map: PaneMap::default(),
            map_dirty: false,
            interactive_panes: BTreeMap::new(),
            sidebar_pane: None,
            panes: Vec::new(),
            degraded: false,
            confirm_armed_at: None,
            message: None,
            msg_deadline: None,
            poll_error: None,
            fail_streak: 0,
            last_poll: Instant::now(),
            should_quit: false,
        }
    }

    fn many(n: usize) -> Vec<Session> {
        (0..n)
            .map(|i| {
                let (status, state) = match i % 4 {
                    0 => (Status::Busy, Some(State::Working)),
                    1 => (Status::Idle, Some(State::Working)),
                    2 => (Status::Idle, Some(State::Done)),
                    _ => (Status::Unknown("weird".into()), None),
                };
                let kind = if i % 5 == 0 { Kind::Interactive } else { Kind::Background };
                sess(i, kind, status, state)
            })
            .collect()
    }

    const SIZES: &[(u16, u16)] = &[
        (20, 8),
        (30, 24),
        (34, 76),
        (40, 76),
        (5, 3),
        (1, 1),
        (0, 0),
        // extra hostile sizes beyond §10.1's list
        (2, 2),
        (34, 12),
        (34, 11),
        (19, 6),
        (6, 4),
        (120, 1),
    ];

    fn render(app: &App) {
        for &(w, h) in SIZES {
            let mut term = Terminal::new(TestBackend::new(w, h)).expect("test backend");
            term.draw(|f| draw(f, app)).expect("draw must not fail");
        }
    }

    fn all_modes(base: &mut App) {
        for mode in [
            Mode::Normal,
            Mode::Filter,
            Mode::Help,
            Mode::Logs,
            Mode::Confirm(Confirm::StopSession {
                session_id: "uuid-0001".into(),
                short_id: "1c45d64f".into(),
                name: "bt/reg-update".into(),
            }),
            Mode::Prompt(PromptKind::NewBackground),
            Mode::Prompt(PromptKind::NewInteractive),
        ] {
            base.mode = mode.clone();
            base.prompt = match &mode {
                Mode::Prompt(PromptKind::NewBackground) => Some(Prompt {
                    kind: PromptKind::NewBackground,
                    fields: vec![
                        "/home/dev/projects/shared/Foundation".into(),
                        "regenerate the regression tables for run-a".into(),
                    ],
                    focus: 1,
                    cursor: 11,
                }),
                Mode::Prompt(PromptKind::NewInteractive) => Some(Prompt {
                    kind: PromptKind::NewInteractive,
                    fields: vec!["/home/dev".into()],
                    focus: 0,
                    cursor: 0,
                }),
                _ => None,
            };
            base.logs = if mode == Mode::Logs {
                Some(LogsView {
                    title: "prediction analysis ab_1_2_cd".into(),
                    lines: (0..200).map(|i| format!("log line {i} ── with unicode ✓")).collect(),
                    scroll: 150,
                })
            } else {
                None
            };
            render(base);
        }
        base.mode = Mode::Normal;
        base.prompt = None;
        base.logs = None;
    }

    #[test]
    fn viewport_rows_matches_the_spec_table() {
        let expected = [0u16, 0, 1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 6];
        for (h, want) in expected.iter().enumerate() {
            assert_eq!(list_viewport_rows(h as u16), *want, "height {h}");
        }
        // beyond the table: full layout keeps H-6
        assert_eq!(list_viewport_rows(76), 70);
        assert_eq!(list_viewport_rows(u16::MAX), u16::MAX - 6);
    }

    #[test]
    fn draw_never_panics_with_forty_sessions_in_every_mode() {
        let mut app = app_with(many(40));
        all_modes(&mut app);
    }

    #[test]
    fn draw_never_panics_with_no_sessions_in_every_mode() {
        let mut app = app_with(Vec::new());
        app.selected = 0;
        all_modes(&mut app);
    }

    #[test]
    fn draw_never_panics_in_degraded_and_error_states() {
        let mut app = app_with(many(3));
        app.degraded = true;
        app.poll_error = Some("claude: command not found\nsecond line".into());
        app.message = Some(("stopped bt/reg-update".into(), MsgLevel::Info));
        render(&app);
        app.message = Some(("stop failed: no such session".into(), MsgLevel::Error));
        app.dark = false;
        render(&app);
        app.message = Some(("cannot stop an interactive session".into(), MsgLevel::Warn));
        render(&app);
    }

    #[test]
    fn draw_never_panics_with_out_of_range_selection_and_scroll() {
        let mut app = app_with(many(5));
        app.selected = 9_999;
        app.scroll = 9_999;
        render(&app);
        app.selected = 0; // a Row::Header index — must not be treated as a session
        app.scroll = 0;
        render(&app);
    }

    #[test]
    fn draw_never_panics_on_filtered_and_unicode_content() {
        let mut sessions = many(4);
        sessions[0].name = "日本語のセッション名前テキスト🙂🙂🙂".into();
        sessions[0].cwd = "/home/dev/プロジェクト/とても長いディレクトリ名".into();
        sessions[1].name = String::new();
        sessions[1].cwd = String::new();
        sessions[2].started_at = 0; // exercises a stale timestamp in the age column
        let mut app = app_with(sessions);
        app.filter = "session".into();
        app.rows = build_rows(&app.sessions, &app.filter, true, &[]);
        render(&app);
        app.filter = "zzz-nothing-matches".into();
        app.rows = build_rows(&app.sessions, &app.filter, true, &[]);
        render(&app);
    }

    /// The open marker and the pane badge for a BACKGROUND session, driven by
    /// `@ccmux_map`. Interactive sessions resolve through `interactive_panes`,
    /// whose values need a `PaneId` — not constructible while `PaneId::parse`
    /// is another lane's `todo!()` — so that path is covered only by the
    /// no-panic matrix for now.
    #[test]
    fn open_marker_renders_for_a_mapped_background_session() {
        let sessions = vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))];
        let sid = sessions[0].session_id.clone();
        let mut app = app_with(sessions);
        app.selected = 1;
        assert!(!is_open(&app, &sid));

        app.map.panes.insert(
            "%25".into(),
            PaneEntry {
                session_id: sid.clone(),
                short_id: "00000001".into(),
                name: "n".into(),
                opened_at: 0,
            },
        );
        // A lower-numbered pane for the same session wins the badge.
        app.map.panes.insert(
            "%7".into(),
            PaneEntry {
                session_id: sid.clone(),
                short_id: "00000001".into(),
                name: "n".into(),
                opened_at: 0,
            },
        );
        assert!(is_open(&app, &sid));
        assert_eq!(pane_key_for(&app, &sid).as_deref(), Some("%7"));

        let mut term = Terminal::new(TestBackend::new(40, 24)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let dump = term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>();
        assert!(dump.contains('▌'), "open marker must be drawn");
    }

    /// One string per terminal row, so a test can say "the age column is on the
    /// row" instead of grepping the whole screen.
    fn rows_at(app: &App, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("test backend");
        term.draw(|f| draw(f, app)).expect("draw");
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn a_wide_name_keeps_the_age_and_the_pane_badge() {
        let mut app = app_with(vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))]);
        // A name Claude really produces from a Chinese conversation: 20 chars,
        // 40 display columns — wider than the whole sidebar.
        if let Some(s) = app.sessions.get_mut(0) {
            s.name = "回归模型数据清洗与因子测试流水线重构任务".into();
            s.started_at = 0;
        }
        app.now_ms = 3_600_000;
        app.rows = build_rows(&app.sessions, "", true, &[]);
        let sid = app.sessions[0].session_id.clone();
        app.map.panes.insert(
            "%7".into(),
            PaneEntry {
                session_id: sid,
                short_id: "00000001".into(),
                name: "n".into(),
                opened_at: 0,
            },
        );
        app.panes = vec![PaneInfo {
            id: PaneId::parse("%7").expect("pane id"),
            pid: 0,
            index: 3,
            left: 35,
            top: 0,
            width: 60,
            height: 24,
            active: false,
            session_name: "ccmux".into(),
            window_index: 1,
        }];

        let rows = rows_at(&app, 34, 24);
        let row = rows
            .iter()
            .find(|r| r.contains('回'))
            .cloned()
            .unwrap_or_default();
        assert!(row.contains("1h"), "wide name ate the age column: {row:?}");
        assert!(row.contains('3'), "wide name ate the pane badge: {row:?}");
        assert!(row.contains('▌'), "wide name ate the open marker: {row:?}");
    }

    #[test]
    fn header_count_reflects_a_hidden_group_not_only_a_filter() {
        let mut app = app_with(many(6));
        app.show_completed = false;
        app.rows = build_rows(&app.sessions, "", false, &[]);
        let rows = rows_at(&app, 40, 24);
        assert!(
            rows[0].contains("/6"),
            "`a` hid rows but the header still claims all 6: {:?}",
            rows[0]
        );

        // Nothing hidden: the bare total, not a ratio.
        app.show_completed = true;
        app.rows = build_rows(&app.sessions, "", true, &[]);
        let rows = rows_at(&app, 40, 24);
        assert!(rows[0].contains("6 sessions"), "{:?}", rows[0]);
        assert!(!rows[0].contains("/6"), "{:?}", rows[0]);
    }

    /// A dismissed session must stay in `total`. The count is the only thing
    /// on screen that still says the row exists, so `5/6` is what tells the
    /// operator that `d` hid something rather than that it ended.
    #[test]
    fn header_count_still_counts_a_dismissed_session_in_the_total() {
        let mut app = app_with(many(6));
        let victim = app
            .sessions
            .first()
            .map(|s| s.session_id.clone())
            .unwrap_or_default();
        app.hidden = crate::tmux::HiddenSet::new();
        app.hidden.dismiss(&victim);
        app.rows = build_rows(&app.sessions, "", true, app.hidden.ids());

        let rows = rows_at(&app, 40, 24);
        assert!(
            rows[0].contains("5/6"),
            "a dismissed row must leave the total alone: {:?}",
            rows[0]
        );
        assert_eq!(
            app.rows.iter().filter(|r| matches!(r, Row::Session { .. })).count(),
            5
        );
        assert_eq!(app.sessions.len(), 6, "the poll is untouched");
    }

    /// The `?` overlay is where a narrow sidebar discovers `d`, so both halves
    /// of the pair have to be in it.
    #[test]
    fn the_help_overlay_documents_dismiss_and_undo() {
        let mut app = app_with(many(3));
        app.mode = Mode::Help;
        app.help_lines = help_line_count();
        let rows = rows_at(&app, 60, 40).join("\n");
        assert!(rows.contains("hide row (pane stays)"), "{rows}");
        assert!(rows.contains("undo the last hide"), "{rows}");

        // The `d` line has to survive the narrow overlay too: at the default
        // 34-column sidebar the action column is `w - key_w`, and losing the
        // "(pane stays)" half is losing the only warning that `d` on a session
        // with an open pane leaves that pane with no row to reach it from.
        let narrow = rows_at(&app, 34, 40).join("\n");
        assert!(narrow.contains("hide row (pane stays)"), "{narrow}");
    }

    #[test]
    fn a_message_too_wide_for_the_footer_wraps_instead_of_clipping() {
        let mut app = app_with(many(3));
        // §8.5's refusal — the one message the operator must read in full.
        app.message = Some((
            "refusing: closing this pane would end the interactive session — exit Claude inside the pane instead".into(),
            MsgLevel::Warn,
        ));
        let rows = rows_at(&app, 34, 24);
        let tail = rows[20..].join(" ");
        assert!(tail.contains("refusing"), "{tail:?}");
        assert!(
            tail.contains("exit Claude inside the pane"),
            "the actionable half was clipped: {tail:?}"
        );

        // A message that fits leaves the detail block alone.
        app.message = Some(("stopped af/reg".into(), MsgLevel::Info));
        let rows = rows_at(&app, 34, 24);
        assert!(rows[23].contains("stopped af/reg"), "{:?}", rows[23]);
        assert!(rows[20].contains("name"), "detail block lost: {:?}", rows[20]);
    }

    #[test]
    fn detail_block_says_done_for_a_completed_session() {
        let app = app_with(vec![sess(
            1,
            Kind::Background,
            // The live payload omits `status` on every `state: "done"` row.
            Status::Unknown(String::new()),
            Some(State::Done),
        )]);
        let rows = rows_at(&app, 40, 24);
        let id_line = rows
            .iter()
            .find(|r| r.trim_start().starts_with("id"))
            .cloned()
            .unwrap_or_default();
        assert!(id_line.contains("done"), "{id_line:?}");
        assert!(!id_line.contains('?'), "{id_line:?}");
    }

    #[test]
    fn the_empty_placeholder_is_centred_in_the_list() {
        let app = app_with(Vec::new());
        let rows = rows_at(&app, 34, 24);
        let at = rows
            .iter()
            .position(|r| r.contains("no sessions"))
            .expect("placeholder");
        assert!(at > 4, "placeholder is not centred, it is on row {at}");
        let row = &rows[at];
        let lead = row.len() - row.trim_start().len();
        assert!(lead > 4, "placeholder is not centred horizontally: {row:?}");
    }

    #[test]
    fn header_shows_filter_ratio_and_group_titles() {
        let mut app = app_with(many(6));
        app.filter = "session number 1".into();
        app.rows = build_rows(&app.sessions, &app.filter, true, &[]);
        let mut term = Terminal::new(TestBackend::new(40, 24)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let dump = term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>();
        assert!(dump.contains("ccmux"));
        assert!(dump.contains("/6"), "filter ratio missing: {dump:?}");
        assert!(dump.contains("Working"));
    }

    #[test]
    fn status_glyph_resolves_completed_before_the_unknown_fallback() {
        let p = Palette::dark();

        // The shape `claude agents --json` actually emits for a finished
        // background session: state=done, `status` key absent entirely.
        let done = sess(1, Kind::Background, Status::Unknown(String::new()), Some(State::Done));
        assert_eq!(status_glyph(&done, &p), ("✓", p.green));

        // The three fully-known cases keep their glyphs.
        let busy = sess(2, Kind::Background, Status::Busy, Some(State::Working));
        assert_eq!(status_glyph(&busy, &p), ("●", p.orange));
        let waiting = sess(3, Kind::Background, Status::Idle, Some(State::Working));
        assert_eq!(status_glyph(&waiting, &p), ("◐", p.blue));
        let idle_interactive = sess(4, Kind::Interactive, Status::Idle, None);
        assert_eq!(status_glyph(&idle_interactive, &p), ("○", p.gray));

        // `?` still means "this build does not recognize the value".
        let odd_state = sess(5, Kind::Background, Status::Idle, Some(State::Unknown("stopped".into())));
        assert_eq!(status_glyph(&odd_state, &p), ("?", p.purple));
        let odd_status = sess(6, Kind::Interactive, Status::Unknown("thinking".into()), None);
        assert_eq!(status_glyph(&odd_status, &p), ("?", p.purple));
    }

    #[test]
    fn help_overlay_scrolls_with_help_scroll_not_list_scroll() {
        let mut app = app_with((0..40).map(|n| sess(n, Kind::Background, Status::Busy, Some(State::Working))).collect());
        app.mode = Mode::Help;
        // The list scroll is pinned to the session list every frame; it must
        // have no effect on the overlay.
        app.scroll = 9_999;
        app.help_scroll = 0;
        // Short enough that §8.1's binding list overflows the overlay body;
        // with no overflow there is nothing to scroll and the test is vacuous.
        let mut term = Terminal::new(TestBackend::new(40, 14)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let top = term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>();
        assert!(top.contains("next session"), "help overlay did not start at the top: {top:?}");

        app.help_scroll = 4;
        term.draw(|f| draw(f, &app)).unwrap();
        let scrolled = term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>();
        assert!(!scrolled.contains("next session"), "help overlay did not scroll: {scrolled:?}");
        assert!(scrolled.contains("next group"), "scrolled overlay lost its body: {scrolled:?}");

        // An out-of-range value clamps instead of blanking the overlay.
        app.help_scroll = 9_999;
        term.draw(|f| draw(f, &app)).unwrap();
        let clamped = term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>();
        assert!(clamped.contains("quit from any mode"), "clamped overlay went blank: {clamped:?}");
    }

    #[test]
    fn palettes_differ_and_track_the_dark_flag() {
        let mut app = app_with(Vec::new());
        assert_eq!(Palette::for_app(&app).fg, Palette::dark().fg);
        app.dark = false;
        assert_eq!(Palette::for_app(&app).fg, Palette::light().fg);
        assert_ne!(Palette::dark().sel_bg, Palette::light().sel_bg);
    }


    // ── The one grid (§0): column W-1 is the rail, column W is the margin ───

    /// Sum of a built line's span widths — what `draw_list` actually asks for,
    /// before ratatui pads or clips it to the buffer. A `TestBackend` buffer is
    /// always exactly W wide, so a buffer-based check of this invariant would
    /// be vacuous; this is the only place it can be tested.
    fn line_w(l: &Line<'static>) -> usize {
        l.spans.iter().map(|s| display_width(&s.content)).sum()
    }

    fn line_cols(l: &Line<'static>) -> Vec<char> {
        l.spans
            .iter()
            .flat_map(|s| s.content.chars())
            .collect()
    }

    /// THE invariant the whole layout rests on: one `Row` is one `Line`, and
    /// that line is exactly W display columns — otherwise the selection band
    /// has a hole in it and the rail stops being a rail. Swept over hostile
    /// names (empty, CJK, 80 columns), hostile ages (`0s` .. `9999d`), pane
    /// pane indices that clamp the badge to `+` (12, 999) and both palettes.
    #[test]
    fn every_session_line_is_exactly_the_sidebar_width() {
        let long = "x".repeat(80);
        let names: Vec<&str> = vec![
            "",
            "alpha/opt",
            "Neovim-style TUI with split sessions",
            "回归模型数据清洗与因子测试流水线重构任务",
            // Wide symbols below U+1F300 and a variation-selector pair: both
            // used to be charged one column and drawn in two.
            "emoji ✅ name test",
            "build ⚠\u{fe0f} failing",
            long.as_str(),
        ];
        // now_ms is 5_000_000: "0s", "4m", "7h", "15d" and a "9999d" spill.
        let ages: &[i64] = &[5_000_000, 4_760_000, 4_975_000, -1_290_000_000, i64::MIN / 4];
        let idxs: &[Option<u32>] = &[None, Some(0), Some(3), Some(12), Some(999)];

        for &idx in idxs {
            let mut app = app_with(vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))]);
            let sid = app.sessions[0].session_id.clone();
            app.map.panes.insert(
                "%7".into(),
                PaneEntry {
                    session_id: sid,
                    short_id: "00000001".into(),
                    name: "n".into(),
                    opened_at: 0,
                },
            );
            if let Some(i) = idx {
                app.panes = vec![PaneInfo {
                    id: PaneId::parse("%7").expect("pane id"),
                    pid: 0,
                    index: i,
                    left: 0,
                    top: 0,
                    width: 60,
                    height: 24,
                    active: false,
                    session_name: "ccmux".into(),
                    window_index: 1,
                }];
            }
            for name in &names {
                app.sessions[0].name = (*name).to_string();
                for &age in ages {
                    app.sessions[0].started_at = age;
                    for selected in [false, true] {
                        for w in 0..=120usize {
                            for pal in [Palette::light(), Palette::dark()] {
                                let l = session_line(&app, &app.sessions[0], selected, w, &pal);
                                assert_eq!(
                                    line_w(&l),
                                    w,
                                    "w={w} idx={idx:?} sel={selected} age={age} name={name:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_group_header_line_fits_and_fills_its_width() {
        let p = Palette::light();
        for g in Group::all() {
            for count in [0usize, 2, 12, 999, 1_000_000] {
                for w in 0..=120usize {
                    let l = group_header_line(g, count, w, &p);
                    let got = line_w(&l);
                    assert!(got <= w, "{g:?} count={count} w={w} overflowed to {got}");
                    if w >= MARGIN_MIN {
                        assert_eq!(got, w, "{g:?} count={count} w={w}");
                    }
                }
            }
        }
        // Below the gutter threshold the parens are gone, which is what lets
        // `Completed 12` fit 12 columns where `Completed (12)` did not.
        let l = group_header_line(Group::Completed, 12, 12, &p);
        assert_eq!(line_cols(&l).iter().collect::<String>(), "Completed 12");
    }

    /// The age, the group count and the footer's help key all terminate on the
    /// same column, and column W is always blank.
    #[test]
    fn one_rail_carries_the_age_the_count_and_the_help_key() {
        let mut app = app_with(vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))]);
        app.sessions[0].name = "Neovim-style TUI with split sessions".into();
        app.sessions[0].started_at = app.now_ms - 4 * 60_000; // "4m"
        let p = Palette::light();

        for w in [28usize, 34, 44, 80] {
            let cols = line_cols(&session_line(&app, &app.sessions[0], false, w, &p));
            assert_eq!(cols[w - 1], ' ', "column W must be the margin at w={w}");
            assert_eq!(
                cols[w - 3..w - 1].iter().collect::<String>(),
                "4m",
                "the age must end on column W-1 at w={w}"
            );
        }
        for w in [20usize, 28, 34, 44] {
            let cols = line_cols(&group_header_line(Group::Completed, 12, w, &p));
            assert_eq!(cols[w - 1], ' ', "column W must be the margin at w={w}");
            assert_eq!(cols[w - 3..w - 1].iter().collect::<String>(), "12", "w={w}");
            // The title starts on column 5, aligned with the names below it.
            assert_eq!(cols[0..4].iter().collect::<String>(), " ── ", "w={w}");
        }
    }

    /// The badge lives in gutter column 2 now, so it survives at 28 columns —
    /// strictly more widths than the old right-hand badge, which needed 34 —
    /// and the name field's right edge no longer moves when a session opens.
    #[test]
    fn the_pane_badge_sits_in_the_gutter_from_twenty_columns_up() {
        let mut app = app_with(vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))]);
        app.sessions[0].name = "alpha/opt".into();
        let sid = app.sessions[0].session_id.clone();
        let closed_25 = line_cols(&session_line(&app, &app.sessions[0], false, 34, &Palette::light()));

        app.map.panes.insert(
            "%7".into(),
            PaneEntry {
                session_id: sid,
                short_id: "00000001".into(),
                name: "n".into(),
                opened_at: 0,
            },
        );
        app.panes = vec![PaneInfo {
            id: PaneId::parse("%7").expect("pane id"),
            pid: 0,
            index: 2,
            left: 0,
            top: 0,
            width: 60,
            height: 24,
            active: false,
            session_name: "ccmux".into(),
            window_index: 1,
        }];

        for w in [20usize, 28, 34, 44] {
            let cols = line_cols(&session_line(&app, &app.sessions[0], false, w, &Palette::light()));
            assert_eq!(
                cols[0..4].iter().collect::<String>(),
                "▌2● ",
                "marker, badge, glyph, space at w={w}"
            );
        }

        // Opening the session must not move the name column's right edge.
        app.sessions[0].name = "a name that is far too long for this sidebar".into();
        let open_34 = line_cols(&session_line(&app, &app.sessions[0], false, 34, &Palette::light()));
        app.map.panes.clear();
        app.panes.clear();
        let closed_34 = line_cols(&session_line(&app, &app.sessions[0], false, 34, &Palette::light()));
        assert_eq!(
            open_34[4..29].iter().collect::<String>(),
            closed_34[4..29].iter().collect::<String>(),
            "the name field moved when the session opened"
        );
        assert_eq!(closed_25[0], ' ', "a closed row leaves column 1 blank");
    }

    /// Column 1 is the marker's, not the selection's: an open row keeps its
    /// aqua `▌` even while selected, because the band, the BOLD name and the
    /// promoted age already carry the selection.
    #[test]
    fn the_selection_cap_yields_column_one_to_an_open_marker() {
        let mut app = app_with(vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))]);
        let p = Palette::light();
        let cols = line_cols(&session_line(&app, &app.sessions[0], true, 34, &p));
        assert_eq!(cols[0], '▏', "a selected closed row caps column 1");

        let sid = app.sessions[0].session_id.clone();
        app.map.panes.insert(
            "%7".into(),
            PaneEntry {
                session_id: sid,
                short_id: "00000001".into(),
                name: "n".into(),
                opened_at: 0,
            },
        );
        let l = session_line(&app, &app.sessions[0], true, 34, &p);
        assert_eq!(line_cols(&l)[0], '▌', "the open marker outranks the cap");
        // `p.dim` is 3.16:1 on the dark band, so a selected row never paints it.
        let dark = Palette::dark();
        let l = session_line(&app, &app.sessions[0], true, 34, &dark);
        assert!(
            l.spans.iter().all(|s| s.style.fg != Some(dark.dim)),
            "a selected row must not paint p.dim on sel_bg"
        );
    }

    /// A pane index of ten or more must NOT widen the badge. It used to render
    /// as `12`, which pushed that row's status glyph from column 3 to column 4
    /// and its name from column 5 to column 6 while every neighbouring row kept
    /// 3 and 5 — the row still measured W, so nothing overflowed, but the one
    /// grid the redesign rests on was broken on the left edge, where it is most
    /// visible. Ten panes in one window is reachable: ccmux opens every session
    /// as a split of the same window.
    #[test]
    fn a_two_digit_pane_index_never_widens_the_gutter() {
        let p = Palette::light();
        let mut app = app_with(vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))]);
        app.sessions[0].name = "Kernel bugs investigation".into();
        let sid = app.sessions[0].session_id.clone();
        app.map.panes.insert(
            "%7".into(),
            PaneEntry {
                session_id: sid,
                short_id: "00000001".into(),
                name: "n".into(),
                opened_at: 0,
            },
        );
        fn pane(app: &mut App, index: u32) {
            app.panes = vec![PaneInfo {
                id: PaneId::parse("%7").expect("pane id"),
                pid: 0,
                index,
                left: 0,
                top: 0,
                width: 60,
                height: 24,
                active: false,
                session_name: "ccmux".into(),
                window_index: 1,
            }];
        }

        for w in [20usize, 24, 28, 34, 44] {
            pane(&mut app, 3);
            let single = line_cols(&session_line(&app, &app.sessions[0], false, w, &p));
            for index in [10u32, 12, 99, 999] {
                pane(&mut app, index);
                let cols = line_cols(&session_line(&app, &app.sessions[0], false, w, &p));
                assert_eq!(cols[0], '▌', "w={w} index={index}: the marker holds column 1");
                assert_eq!(cols[1], '+', "w={w} index={index}: the badge clamps to one column");
                assert_eq!(cols[2], '●', "w={w} index={index}: the glyph stays on column 3");
                assert_eq!(cols[3], ' ', "w={w} index={index}");
                // Byte-for-byte the same row as a single-digit index, badge aside.
                assert_eq!(
                    cols[2..].iter().collect::<String>(),
                    single[2..].iter().collect::<String>(),
                    "w={w} index={index}: the gutter moved the rest of the row"
                );
                assert_eq!(line_w(&session_line(&app, &app.sessions[0], false, w, &p)), w);
            }
        }
    }

    /// The width oracle itself, measured against the terminal rather than
    /// against itself. `every_session_line_is_exactly_the_sidebar_width` sums a
    /// row with `display_width`, so it cannot catch `display_width` being wrong
    /// — and it was: the Wide symbol blocks below U+1F300 were charged one
    /// column and drawn in two, so a name holding `✅` ran its row to W+1 and
    /// pushed the age onto the margin column. This test measures with numbers
    /// tmux 3.4 reported for these exact chars (`printf` + `#{cursor_x}`).
    #[test]
    fn an_emoji_name_still_ends_the_row_on_the_rail() {
        /// NOT `display_width` — that is the thing under test.
        fn tmux_cols(s: &str) -> usize {
            let mut n = 0;
            let mut it = s.chars().peekable();
            while let Some(c) = it.next() {
                let vs16 = it.peek() == Some(&'\u{fe0f}');
                n += match c {
                    '\u{fe0f}' | '\u{fe0e}' => 0,
                    '✅' | '⭐' | '⌚' | '❌' | '⏳' | '❓' | '🀄' => 2,
                    c if ('\u{4e00}'..='\u{9fff}').contains(&c) => 2,
                    _ if vs16 => 2,
                    _ => 1,
                };
            }
            n
        }
        assert_eq!(tmux_cols("emoji ✅ name"), 13, "the test's own oracle");

        let p = Palette::light();
        let mut app = app_with(vec![sess(1, Kind::Background, Status::Idle, None)]);
        app.sessions[0].started_at = app.now_ms - 3 * 3_600_000; // "3h"
        for name in [
            "emoji ✅ name test",
            "⭐⭐⭐",
            "build ⚠\u{fe0f} failing",
            "❌ ⏳ ❓ 🀄 一二三",
            "✅",
        ] {
            app.sessions[0].name = name.to_string();
            for w in 6..=60usize {
                for selected in [false, true] {
                    let l = session_line(&app, &app.sessions[0], selected, w, &p);
                    let text: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
                    assert_eq!(
                        tmux_cols(&text),
                        w,
                        "the terminal draws {text:?} in {} columns, not {w}",
                        tmux_cols(&text)
                    );
                    if w >= RAIL_MIN {
                        // Column W is the margin, and the age's last cell is on W-1.
                        assert!(text.ends_with("3h "), "the age left the rail: {text:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_name_weight_ladder_is_keyed_to_the_group() {
        let p = Palette::light();
        let working = sess(1, Kind::Background, Status::Busy, Some(State::Working));
        let idle = sess(2, Kind::Background, Status::Idle, None);
        let done = sess(3, Kind::Background, Status::Unknown(String::new()), Some(State::Done));
        let interactive = sess(4, Kind::Interactive, Status::Idle, None);
        assert_eq!(name_tier(&working, &p), p.fg);
        assert_eq!(name_tier(&idle, &p), p.gray);
        assert_eq!(name_tier(&done, &p), p.dim);
        assert_eq!(name_tier(&interactive, &p), p.purple, "kind is a hue, not a tier");
    }

    /// The id line's two facts, and which one loses. The status word is
    /// redundant with the row's own glyph; the 8-hex short id is stated nowhere
    /// else, so below `MIN_BODY` the status yields and the id survives. HEAD
    /// truncated the status away at the default 34 columns.
    #[test]
    fn the_detail_id_line_keeps_the_status_word_on_the_rail() {
        let mut app = app_with(vec![sess(
            1,
            Kind::Background,
            Status::Unknown(String::new()),
            Some(State::Stopped),
        )]);
        app.selected = 1;

        let id = rows_at(&app, 34, 24)
            .into_iter()
            .find(|r| r.trim_start().starts_with("id"))
            .expect("id line");
        assert!(id.contains("00000001 background"), "{id:?}");
        let cols: Vec<char> = id.chars().collect();
        assert_eq!(cols[33], ' ', "column W must be the margin: {id:?}");
        assert_eq!(
            cols[26..33].iter().collect::<String>(),
            "stopped",
            "the status word must end on column W-1: {id:?}"
        );

        let id = rows_at(&app, 20, 24)
            .into_iter()
            .find(|r| r.trim_start().starts_with("id"))
            .expect("id line");
        assert!(id.contains("00000001"), "the short id must survive: {id:?}");
        assert!(!id.contains("stopped"), "the status word must yield: {id:?}");

        // The `· pane N` suffix is gone; the gutter states the pane index.
        let all = rows_at(&app, 34, 24).join(" ");
        assert!(!all.contains("pane"), "the detail pane suffix is gone: {all:?}");
    }

    /// A pair is never split. HEAD clipped mid-word to `x cl…` at the default
    /// width; every fill below ends on a whole word with `? help` on the rail.
    #[test]
    fn the_footer_fills_whole_pairs_and_pins_the_help_key() {
        let app = app_with(many(3));
        for (w, want) in [
            (44u16, "⏎ open  o/s split  x close  d/u hide"),
            (34, "⏎ open  o/s split  x close"),
            (28, "⏎ open  o/s split"),
            (20, "⏎ open"),
        ] {
            let f = rows_at(&app, w, 24)[23].clone();
            assert!(f.starts_with(want), "w={w}: {f:?}");
            assert_eq!(f[want.len()..].trim(), "? help", "w={w}: {f:?}");
            let cols: Vec<char> = f.chars().collect();
            let w = w as usize;
            assert_eq!(cols[w - 1], ' ', "column W must be the margin: {f:?}");
            assert_eq!(cols[w - 7..w - 1].iter().collect::<String>(), "? help", "{f:?}");
        }
        // Below the gutter threshold only the pinned key renders.
        for w in [19u16, 12, 8] {
            let f = rows_at(&app, w, 24)[23].clone();
            assert_eq!(f.trim(), "? help", "w={w}: {f:?}");
        }
        for (w, want) in [(6u16, "? help"), (3, "? …"), (1, "…")] {
            let f = rows_at(&app, w, 24)[23].clone();
            assert_eq!(f.trim_end(), want, "w={w}: {f:?}");
        }
    }

    /// The accents carry facts — the status glyph, the open marker, the message
    /// level — and on the LIGHT ground, which ships by default, six of the seven
    /// used to fail the same 4.0:1 floor `palette_contrast_is_readable` enforces
    /// for the greys: green 2.73, aqua 2.80, orange 3.41, blue 3.73, purple
    /// 3.73, yellow 2.19. The whole colour channel barely functioned in the
    /// theme most users see. This guards the fix.
    ///
    /// The second half guards the selection band: every colour a SELECTED row
    /// can paint must clear the floor against `sel_bg` too. `p.dim` (3.16:1 on
    /// the dark band) and `p.red` (3.37:1) do not — which is exactly why
    /// `session_line` promotes the selected row's age dim -> gray and forces the
    /// selected name to `p.fg`, and why red never appears on a list row.
    #[test]
    fn light_accents_are_readable_and_the_band_is_safe() {
        fn lum(c: Color) -> f64 {
            let Color::Rgb(r, g, b) = c else {
                panic!("palette entries must be true-colour: {c:?}")
            };
            let f = |v: u8| {
                let v = v as f64 / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
        }
        fn ratio(a: Color, b: Color) -> f64 {
            let (x, y) = (lum(a), lum(b));
            (x.max(y) + 0.05) / (x.min(y) + 0.05)
        }
        let dark_bg = Color::Rgb(0x28, 0x28, 0x28);
        let light_bg = Color::Rgb(0xfb, 0xf1, 0xc7);

        for (name, p, bg) in [
            ("dark", Palette::dark(), dark_bg),
            ("light", Palette::light(), light_bg),
        ] {
            for (field, c) in [
                ("red", p.red),
                ("green", p.green),
                ("yellow", p.yellow),
                ("blue", p.blue),
                ("purple", p.purple),
                ("aqua", p.aqua),
                ("orange", p.orange),
            ] {
                let r = ratio(c, bg);
                assert!(r >= 4.0, "{name}.{field} is {r:.2}:1 on the ground, needs >= 4.0:1");
            }
            // Everything `session_line` can paint on a selected row.
            for (field, c) in [
                ("fg", p.fg),
                ("gray", p.gray),
                ("aqua", p.aqua),
                ("green", p.green),
                ("blue", p.blue),
                ("purple", p.purple),
                ("orange", p.orange),
            ] {
                let r = ratio(c, p.sel_bg);
                assert!(r >= 4.0, "{name}.{field} is {r:.2}:1 on sel_bg, needs >= 4.0:1");
            }
        }
    }

    /// Guards the fix for "the grey in the session list is too light". Every
    /// palette colour that carries text must clear a readable ratio against its
    /// OWN ground, and the ladder must descend fg > gray > dim. Before this,
    /// dark `dim` sat at 2.26:1 (unreadable) and light `gray` at 3.24:1, with
    /// the light ladder inverted — `dim` was darker than `gray`.
    #[test]
    fn palette_contrast_is_readable() {
        fn lum(c: Color) -> f64 {
            let Color::Rgb(r, g, b) = c else {
                panic!("palette entries must be true-colour: {c:?}")
            };
            let f = |v: u8| {
                let v = v as f64 / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
        }
        fn ratio(a: Color, b: Color) -> f64 {
            let (x, y) = (lum(a), lum(b));
            (x.max(y) + 0.05) / (x.min(y) + 0.05)
        }
        // The grounds these palettes are actually drawn on.
        let dark_bg = Color::Rgb(0x28, 0x28, 0x28);
        let light_bg = Color::Rgb(0xfb, 0xf1, 0xc7);

        for (name, p, bg) in [
            ("dark", Palette::dark(), dark_bg),
            ("light", Palette::light(), light_bg),
        ] {
            for (field, c) in [("fg", p.fg), ("gray", p.gray), ("dim", p.dim)] {
                let r = ratio(c, bg);
                assert!(r >= 4.0, "{name}.{field} is {r:.2}:1, needs >= 4.0:1");
            }
            assert!(
                ratio(p.fg, bg) > ratio(p.gray, bg),
                "{name}: fg must be more prominent than gray"
            );
            assert!(
                ratio(p.gray, bg) > ratio(p.dim, bg),
                "{name}: gray must be more prominent than dim"
            );
        }
    }

}
