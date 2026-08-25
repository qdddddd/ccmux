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
            gray: Color::Rgb(0x92, 0x83, 0x74),
            dim: Color::Rgb(0x66, 0x5c, 0x54),
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
            fg: Color::Rgb(0x3c, 0x37, 0x35),
            gray: Color::Rgb(0x92, 0x83, 0x73),
            dim: Color::Rgb(0x7c, 0x6f, 0x64),
            red: Color::Rgb(0xcc, 0x23, 0x1c),
            green: Color::Rgb(0x98, 0x97, 0x19),
            yellow: Color::Rgb(0xd7, 0x99, 0x20),
            blue: Color::Rgb(0x45, 0x85, 0x88),
            purple: Color::Rgb(0xb1, 0x62, 0x86),
            aqua: Color::Rgb(0x68, 0x9d, 0x69),
            orange: Color::Rgb(0xd6, 0x5d, 0x0e),
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
        let rule: String = "─".repeat(r.width as usize);
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

fn group_accent(g: Group, p: &Palette) -> Color {
    match g {
        Group::Working => p.orange,
        Group::Idle => p.blue,
        Group::Completed => p.gray,
    }
}

fn group_header_line(g: Group, count: usize, w: usize, p: &Palette) -> Line<'static> {
    let title = format!("{} ({})", g.title(), count);
    if w < 20 {
        return Line::from(Span::styled(
            truncate_end(&title, w),
            Style::default().fg(group_accent(g, p)),
        ));
    }
    let lead = "── ";
    let title_w = display_width(&title);
    let tail = w.saturating_sub(display_width(lead) + title_w + 1);
    let mut spans = vec![
        Span::styled(lead.to_string(), Style::default().fg(p.dim)),
        Span::styled(title, Style::default().fg(group_accent(g, p))),
    ];
    if tail > 0 {
        spans.push(Span::styled(
            format!(" {}", "─".repeat(tail)),
            Style::default().fg(p.dim),
        ));
    }
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

fn session_line(app: &App, sess: &Session, selected: bool, w: usize, p: &Palette) -> Line<'static> {
    if w == 0 {
        return Line::from(Vec::<Span>::new());
    }
    let (glyph, glyph_color) = status_glyph(sess, p);
    let name_color = if sess.kind == Kind::Interactive { p.purple } else { p.fg };
    let base = if selected {
        Style::default().bg(p.sel_bg)
    } else {
        Style::default()
    };
    let name_style = if selected {
        base.fg(p.fg)
    } else {
        base.fg(name_color)
    };

    // W < 6: glyph only.
    if w < 6 {
        let mut spans = vec![Span::styled(glyph.to_string(), base.fg(glyph_color))];
        pad_to(&mut spans, 1, w, base);
        return Line::from(spans);
    }

    // 6..=19: glyph + space + name (truncated to W-2), no open marker.
    if w < 20 {
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

    let open = is_open(app, &sess.session_id);
    let show_age = w >= 28;
    let show_badge = w >= 34;

    // Right-hand segment, laid out left to right: [badge] [age].
    let badge = if show_badge && open {
        pane_index_for(app, &sess.session_id).map(|i| i.to_string())
    } else {
        None
    };
    let age = if show_age {
        let a = format_age(sess.started_at, app.now_ms);
        // The field is 4 columns wide; a degenerate "9999d" is allowed to spill
        // into the name budget rather than be truncated into nonsense.
        let width = display_width(&a).max(4);
        Some((a, width))
    } else {
        None
    };

    let badge_w = badge.as_ref().map(|b| 1 + display_width(b)).unwrap_or(0);
    let age_w = age.as_ref().map(|(_, wd)| 1 + wd).unwrap_or(0);

    // marker(1) + glyph(1) + space(1) = 3 fixed left columns.
    let name_budget = w.saturating_sub(3 + badge_w + age_w);
    let name = truncate_end(&sess.name, name_budget);

    let mut spans: Vec<Span> = Vec::with_capacity(8);
    let marker = if open { "▌" } else { " " };
    let marker_style = if open { base.fg(p.aqua) } else { base };
    spans.push(Span::styled(marker.to_string(), marker_style));
    spans.push(Span::styled(glyph.to_string(), base.fg(glyph_color)));
    spans.push(Span::styled(" ".to_string(), base));
    let mut used = 3 + display_width(&name);
    spans.push(Span::styled(name, name_style));

    // Pad so the right-hand segment lands flush with the row's right edge.
    let right_w = badge_w + age_w;
    let right_start = w.saturating_sub(right_w);
    if used < right_start {
        let pad = right_start - used;
        spans.push(Span::styled(" ".repeat(pad), base));
        used += pad;
    }
    if let Some(b) = badge {
        spans.push(Span::styled(" ".to_string(), base));
        let bw = display_width(&b);
        spans.push(Span::styled(
            b,
            if selected { base.fg(p.fg) } else { base.fg(p.aqua) },
        ));
        used += 1 + bw;
    }
    if let Some((a, width)) = age {
        let lead = width.saturating_sub(display_width(&a));
        spans.push(Span::styled(" ".repeat(1 + lead), base));
        let aw = display_width(&a);
        spans.push(Span::styled(
            a,
            if selected { base.fg(p.fg) } else { base.fg(p.dim) },
        ));
        used += 1 + lead + aw;
    }

    // The selection bar must be solid across the full width (§6.4).
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

fn draw_detail(f: &mut Frame, area: Rect, app: &App, p: &Palette) {
    let w = area.width as usize;
    if w == 0 || area.height == 0 {
        return;
    }
    let label = Style::default().fg(p.gray);

    let Some(sess) = selected_session(app) else {
        // Empty selection renders three blank lines.
        f.render_widget(Paragraph::new(vec![Line::from(""), Line::from(""), Line::from("")]), area);
        return;
    };

    // " name    " — the value column starts at char 9.
    const VALUE_COL: usize = 9;
    let budget = w.saturating_sub(VALUE_COL);

    let name_color = if sess.kind == Kind::Interactive { p.purple } else { p.fg };
    let l1 = Line::from(vec![
        Span::styled(" name    ", label),
        Span::styled(truncate_end(&sess.name, budget), Style::default().fg(name_color)),
    ]);

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
    } else {
        match &sess.status {
            Status::Busy => "busy".to_string(),
            Status::Idle => "idle".to_string(),
            Status::Unknown(s) if s.is_empty() => "?".to_string(),
            Status::Unknown(s) => s.clone(),
        }
    };
    let pane_suffix = pane_index_for(app, &sess.session_id).map(|i| format!(" · pane {i}"));
    let body = format!("{short}  {kind}  {status}");
    let suffix_w = pane_suffix.as_ref().map(|s| display_width(s)).unwrap_or(0);
    let mut l2_spans = vec![
        Span::styled(" id      ", label),
        Span::styled(
            truncate_end(&body, budget.saturating_sub(suffix_w)),
            Style::default().fg(p.fg),
        ),
    ];
    if let Some(suffix) = pane_suffix
        && budget > suffix_w
    {
        l2_spans.push(Span::styled(suffix, Style::default().fg(p.aqua)));
    }

    let cwd = shorten_cwd(&sess.cwd, app.home.as_deref(), budget);
    let l3 = Line::from(vec![
        Span::styled(" cwd     ", label),
        Span::styled(cwd, Style::default().fg(p.fg)),
    ]);

    f.render_widget(Paragraph::new(vec![l1, Line::from(l2_spans), l3]), area);
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

const HINT: &str = "j/k move  ⏎ open  o/s split  x close  S stop  n new  ? help";

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

    // 4. The hint line, truncated from the right.
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_end(HINT, w),
            Style::default().fg(p.dim),
        ))),
        area,
    );
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
        let rows = build_rows(&sessions, "", true);
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
        app.rows = build_rows(&app.sessions, &app.filter, true);
        render(&app);
        app.filter = "zzz-nothing-matches".into();
        app.rows = build_rows(&app.sessions, &app.filter, true);
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
        app.rows = build_rows(&app.sessions, "", true);
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
        app.rows = build_rows(&app.sessions, "", false);
        let rows = rows_at(&app, 40, 24);
        assert!(
            rows[0].contains("/6"),
            "`a` hid rows but the header still claims all 6: {:?}",
            rows[0]
        );

        // Nothing hidden: the bare total, not a ratio.
        app.show_completed = true;
        app.rows = build_rows(&app.sessions, "", true);
        let rows = rows_at(&app, 40, 24);
        assert!(rows[0].contains("6 sessions"), "{:?}", rows[0]);
        assert!(!rows[0].contains("/6"), "{:?}", rows[0]);
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
        app.rows = build_rows(&app.sessions, &app.filter, true);
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

}
