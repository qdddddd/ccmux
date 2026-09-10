//! Sidebar rendering. SPEC §3.4 (signatures) and §6 (the rendering spec).
//!
//! **PURE RENDERING.** Takes `&App` and a `&mut Frame`, writes cells. It spawns
//! no process, opens no file, reads no clock beyond what `App` already carries,
//! and mutates nothing. Any `Command`, `std::fs`, or `&mut App` appearing in
//! this file is a spec violation.
//!
//! Consumes `app::{App, Mode, Prompt, PromptKind, MsgLevel, LogsView}`,
//! `model::{Group, Row, Session, Kind, Status, format_age, shorten_cwd,
//! truncate_end}`, `tmux::PaneId` (for `Display` only).
//!
//! Two implementation notes for the Integrator:
//!
//! 1. This module resolves the selected session, the "is open" flag, and the
//!    pane badge from `App`'s **public fields** rather than through
//!    `App::selected_session` / `is_open` / `pane_of` / `pane_index_of`. The
//!    logic mirrors §3.5's documented behaviour exactly (the reconciled
//!    `@ccmux_map`). Reading fields keeps `ui::draw`
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

use crate::app::{App, LogsView, Mode, MsgLevel, Prompt, PromptKind};
use crate::model::{
    Group, Kind, Row, Session, State, Status, char_width, display_width, format_age,
    shorten_cwd, truncate_end,
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
    /// The open marker's OTHER-TAB ink, for a session parked in a tab you are
    /// not looking at (§6.4). Named for the role, and deliberately NOT a second
    /// aqua: it is a gruvbox NEUTRAL, so the two markers separate by HUE —
    /// green against warm grey — rather than by depth. Depth was tried and
    /// failed; see `the_two_open_marker_shades_never_collapse`. The tone still
    /// inverts with the ground, darker on the light theme and lighter on the
    /// dark one, because "more prominent" does. `aqua` keeps the current tab.
    pub aqua_elsewhere: Color,
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
            aqua: Color::Rgb(0x8e, 0xc0, 0x7c), // 7.01:1 ground, 5.51:1 band
            // gruvbox fg2, a neutral. Lighter than `aqua` here: on a dark
            // ground prominence is height, not depth. The darkest neutral that
            // still out-contrasts `aqua` — one step down, fg3 `#bdae93`, is
            // 6.77:1 on a ground where `aqua` is already 7.01:1.
            // 8.59:1 ground, 6.76:1 band, ΔE 31.9 from `aqua`.
            aqua_elsewhere: Color::Rgb(0x7c, 0x6f, 0x64),
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
            aqua: Color::Rgb(0x3d, 0x71, 0x51),   // 5.03:1 ground, 4.16:1 band
            orange: Color::Rgb(0xaf, 0x3a, 0x03), // 5.40:1
            // gruvbox fg3, a neutral, and DARKER than `aqua` — the opposite
            // of the dark theme's move, because this ground has no headroom
            // left going lighter (`#427b58` is already 3.64:1 on `sel_bg`,
            // under the floor). Its predecessor `#1d3a2a` went further down
            // still and lost the hue doing it: at 10.95:1 it read as ordinary
            // dark text rather than as a coloured marker. This is the LIGHTEST
            // gruvbox neutral that clears the floor — one step up, fg4
            // `#7c6f64`, is 3.55:1 on the band. That makes it the same ink as
            // `dim`, and that is the price of a neutral on this ground: the
            // gruvbox neutrals ARE the text ramp, so any of them is some tier
            // of text. `dim` is the LIGHTEST of this theme's three text inks,
            // so the marker lands at the shallow end of that ramp and not the
            // deep end that broke it. But the collision is TOTAL, not
            // glancing: column 1's badge takes this same ink by construction,
            // and a Completed row paints its name and age in `dim`, so an
            // UNSELECTED other-tab row of that group renders marker, badge,
            // name and age in one RGB, with only the glyph shapes separating
            // them. Nothing here is three columns clear of anything.
            //
            // What IS bounded is the SELECTED row, the one actually being
            // read: `session_line` forces its name to `p.fg` and promotes its
            // age dim -> gray, two gruvbox ramp steps off this ink and one
            // (ΔE 16.5 and 8.6). Pinned by
            // `the_neutral_marker_never_half_matches_the_text_ramp`.
            // 5.74:1 ground, 4.75:1 band, ΔE 28.8 from `aqua`.
            aqua_elsewhere: Color::Rgb(0xa8, 0x99, 0x84),
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

/// The rect an over-wide footer message wraps into, or `None` when the sidebar
/// is too short to give it more than the footer row it already has.
///
/// With a detail block it is exactly that block plus the footer, which is what
/// §6.8's amendment always described. WITHOUT one — every height below 12 — it
/// used to be nothing at all, so the message fell back to `draw_footer`'s
/// one-line `truncate_end`. That is survivable for a flash; it is not
/// survivable for `Ctrl+X`'s armed warning, the longest line ccmux produces and
/// the only one whose TAIL carries the consequence ("and its worktree — cannot
/// be undone"). Below 12 rows the block is carved out of the bottom of the
/// list instead, capped so the header and at least one session row always
/// survive. The carve is transient — it stands only while a message is too wide
/// to fit, which for the armed warning is at most `CX_WINDOW` — and it changes
/// nothing about `list_viewport_rows`, so the row-index-to-screen-line mapping
/// underneath it is untouched.
fn overflow_rect(area: Rect, s: &Slots) -> Option<Rect> {
    let ft = s.footer?;
    let bottom = ft.y.saturating_add(ft.height);
    if let Some(d) = s.detail {
        return Some(Rect {
            x: d.x,
            y: d.y,
            width: d.width,
            height: bottom.saturating_sub(d.y),
        });
    }
    // Header + one list row are never carved away. h == 3 leaves height 1,
    // which is the footer alone and no better than truncating, so it declines.
    let max_h = area.height.saturating_sub(2);
    if max_h < 2 {
        return None;
    }
    let height = max_h.min(4);
    Some(Rect {
        x: area.x,
        y: bottom.saturating_sub(height),
        width: area.width,
        height,
    })
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
    // §8.5's "closed pane N — agent still running" (35 columns) and §9.7's
    // "no short id — cannot stop this session" (38) do not fit a 34-column
    // sidebar, and they are the ones the operator most needs to read in full.
    match (
        s.footer,
        overflow_message(app, area.width as usize, &p),
        overflow_rect(area, &s),
    ) {
        (Some(_), Some((text, color)), Some(rect)) => {
            // The carved rect can overlap rows the list has already drawn (it
            // does whenever there is no detail block), so clear it first.
            f.render_widget(Clear, rect);
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
        Mode::Prompt(kind) => draw_prompt(f, area, *kind, app.prompt.as_ref(), &p),
        Mode::Normal | Mode::Filter => {}
    }
}

// ── Header (§6.2) ───────────────────────────────────────────────────────────

/// `tab N` — which tab this sidebar is in, in tmux's own `#{window_index}`.
///
/// It is needed because ccmux runs its session with `status off`, so tmux's own
/// window list is not on screen: the sidebar is the ONLY place tab identity can
/// appear. The number is tmux's own index, so `prefix-3` goes exactly where a
/// `tab 3` chip — or a `3` badge — points.
///
/// **There is deliberately no denominator.** The chip used to read `tab N/M`
/// with N a window INDEX and M a COUNT of windows, two quantities that agree
/// only while the indices happen to be a contiguous `1..M`. tmux's
/// `renumber-windows` defaults to OFF and `configure_session` never turns it
/// on, so closing a middle tab leaves a permanent gap and the chip rendered
/// impossible headers like `tab 4/2`. A count cannot be reconciled with an
/// index — one of them had to go, and the index is the half that is
/// actionable, so the count is what went.
///
/// The two-tab gate counts ccmux TABS — windows carrying `@ccmux_tab_sidebar`,
/// the same sole criterion `heal_sidebar` uses — not windows that merely have
/// panes. A bare `prefix-c` window of the operator's own is not a tab: it used
/// to summon the chip and inflate its count even though heal correctly leaves
/// it alone.
///
/// `None` below two tabs. With one tab there is no digit anywhere to explain,
/// `tab 1` would be pure noise, and the header stays byte-identical to what it
/// rendered before tabs existed — all tab UI is invisible until a second tab
/// exists.
fn tab_chip(app: &App) -> Option<String> {
    if app.tabs.iter().filter(|t| t.sidebar.is_some()).count() < 2 {
        return None;
    }
    let me = app
        .own_pane
        .as_ref()
        .and_then(|pane| app.panes.iter().find(|i| &i.id == pane))?
        .window_index;
    Some(format!("tab {me}"))
}

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
        let wide = format!("  {count} session{}", if total == 1 { "" } else { "s" });
        let narrow = format!("  {count}");
        // Three rungs, resolved against the rail's budget so no session count
        // can push the poll dot off column W-1: chip + wide count, chip +
        // narrow count, then no chip at all — which is today's header, byte for
        // byte. The per-row badges still carry tab identity when the chip goes.
        let chip = tab_chip(app).map(|c| format!("  {c}"));
        // The dot renders only while `used < w - 2`, so content that reaches
        // exactly `w - 2` costs the rail its dot. The budget is therefore
        // `w - 3`: the last column the header text may occupy. (Reachable
        // before the chip lost its denominator too — at W=22 a `tab N/M` chip
        // plus a `  N/M` count landed exactly on `w - 2`.)
        let budget = w.saturating_sub(3);
        let fits = |a: &str, b: &str| used + display_width(a) + display_width(b) <= budget;
        let (chip, text) = match &chip {
            Some(c) if w >= 26 && fits(c, &wide) => (Some(c.clone()), wide),
            Some(c) if fits(c, &narrow) => (Some(c.clone()), narrow),
            _ if w >= 26 => (None, wide),
            _ => (None, narrow),
        };
        if let Some(c) = chip {
            push(&mut spans, &mut used, &c, Style::default().fg(p.aqua));
        }
        push(&mut spans, &mut used, &text, Style::default().fg(p.gray));
    }

    // Poll indicator, right-aligned at column W-2. Priority: a live poll error
    // outranks the structural "outside tmux" degradation, because the error is
    // the transient, actionable condition, and both outrank a quiesced gate,
    // which is neither a failure nor a degradation. Dropped below W=14 to
    // match §6.2's third sample line, which renders the wordmark alone.
    //
    // The quiesced dot is dim and HOLLOW, deliberately not the yellow of a
    // degradation: polling paused because nothing was on screen is normal
    // operation, not a fault. There is exactly one situation in which it can
    // be seen, and that situation is the point — tmux replays the last frame
    // this pane drew when the operator switches back to the tab, so the frame
    // they land on says the list was paused, one tick before the forced poll
    // refreshes it.
    if w >= 14 {
        let (glyph, style) = if app.poll_error.is_some() {
            ("●", Style::default().fg(p.red))
        } else if app.degraded {
            ("○", Style::default().fg(p.yellow))
        } else if app.quiesced {
            ("○", Style::default().fg(p.dim))
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
/// top.
fn name_tier(sess: &Session, p: &Palette) -> Color {
    match sess.group() {
        // Blocked shares the top rung with Working. There is nothing above
        // `p.fg` to promote it to, and it does not need one: it is already the
        // first group on screen, under a yellow rule, behind a yellow `▲`.
        Group::Blocked | Group::Working => p.fg,
        Group::Idle => p.gray,
        Group::Completed => p.dim,
    }
}

fn group_accent(g: Group, p: &Palette) -> Color {
    match g {
        // Yellow is the one accent this list did not already spend, and it is
        // the colour the header dot already uses for "look at this": 8.69:1 on
        // the dark ground, 5.04:1 on the light one, both over the 4.0 floor on
        // `sel_bg` too. Red was not an option — blocked is not a failure.
        Group::Blocked => p.yellow,
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
/// SPEC NOTE: `Done` and `Stopped` are matched BEFORE the `Status::Unknown` /
/// `State::Unknown` fallback. `claude agents --json` omits `status` on most
/// `state: "done"` rows (9 of 14 done rows on 2026-08-31; the other 5 carried
/// `status: "idle"`), and an absent key parses to `Status::Unknown("")`, so
/// checking unknown first would make the table's `Completed → ✓` row
/// unreachable for the majority of real completed sessions and paint them
/// purple `?`. `state: "done"` is definitive knowledge, not forward-compat
/// territory; the `?` fallback keeps its job for a status or state value the
/// CLI invents that this build does not recognize.
///
/// `done`, `stopped` and `blocked` are ALL resolved before that fallback for
/// the same reason. Every one of them was `Unknown` once, and each time the
/// symptom was identical: a purple `?` filed under Idle. `App::note_drift` is
/// what now makes the next one announce itself instead of hiding in plain
/// sight — see the note there.
fn status_glyph(sess: &Session, p: &Palette) -> (&'static str, Color) {
    if matches!(sess.state, Some(State::Done)) {
        return ("✓", p.green);
    }
    if matches!(sess.state, Some(State::Stopped)) {
        return ("■", p.gray);
    }
    let unknown_status = matches!(sess.status, Status::Unknown(_));
    let unknown_state = matches!(sess.state, Some(State::Unknown(_)));
    // ONE match, in precedence order, and the blocked verdict is ASKED OF
    // `group()` rather than re-derived from `state`/`status` here.
    //
    // It was re-derived once, and the two rules promptly disagreed: this arm
    // read `state == Blocked || status == Waiting` while `Session::group` only
    // honoured `Waiting` under an absent or unmodelled `state`, so a
    // `state: "working", status: "waiting"` row drew the yellow `▲` that means
    // "answer me" while sitting under the **Working** heading, out of `Tab`'s
    // reach. A mark and a heading that contradict each other are worse than
    // either alone. There is now one rule, in `group()`, and this reads it.
    match sess.group() {
        // Before the `?` fallback, deliberately: a row this build cannot fully
        // name but that `group()` has placed in Blocked is still a row waiting
        // on a human, and purple `?` is not what that should look like.
        Group::Blocked => ("▲", p.yellow),
        _ if unknown_status || unknown_state => ("?", p.purple),
        Group::Working => match sess.status {
            Status::Busy => ("●", p.orange),
            _ => ("◐", p.blue),
        },
        Group::Idle => ("○", p.gray),
        // Unreachable in practice — `group()` returns Completed only for
        // `Done`/`Stopped`, and both returned above — but kept as a real arm
        // rather than an `unreachable!`, because `draw` must never panic.
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
    // A window index of ten or more is exactly where a number stops being
    // something you can eyeball anyway, so the badge degrades to `+` — "open,
    // further along than you want to count" — rather than taking a second
    // column and shifting the row off the grid. tmux's `renumber-windows`
    // defaults to OFF and ccmux never turns it on, so indices are NOT a
    // contiguous 1..M: `+` needs ten windows to have existed at once, not ten
    // to be alive now.
    let badge = tab_badge_for(app, &sess.session_id).map(|i| char::from_digit(i, 10).unwrap_or('+'));
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

    // The gutter's ONE ink, shared by both its columns. `badge.is_some()` is
    // already the "not in the tab you are looking at" test — it is what makes
    // column 2 a digit rather than a blank — so reading the shade off it, and
    // not off a second copy of the window comparison, is what stops the colour
    // and the digit from ever disagreeing about where a session is.
    let ink = if badge.is_some() { p.aqua_elsewhere } else { p.aqua };

    let mut spans: Vec<Span> = Vec::with_capacity(8);
    // Column 1. Open wins over selected: the aqua `▌` is the one thing that
    // says "this session is on screen", and the selected row is already carried
    // by the band, the BOLD name and the promoted age. `▏` (U+258F) is a
    // hairline where `▌` (U+258C) is a thick bar — different weight, not just
    // a different colour.
    //
    // WHICH ink says where. A session in this tab is already in front of you,
    // so it keeps the established aqua; one parked in another tab takes
    // `aqua_elsewhere`, a neutral, because that is the row you have to go
    // somewhere to see. The two differ in HUE and not merely in depth, which is
    // what lets the second one read as a colour at all on the light ground.
    // It is the same fact the digit carries, said in the channel you read
    // without counting.
    if open {
        spans.push(Span::styled("▌".to_string(), base.fg(ink)));
    } else if selected {
        spans.push(Span::styled("▏".to_string(), base.fg(p.fg)));
    } else {
        spans.push(Span::styled(" ".to_string(), base));
    }
    // Column 2. Blank means "open, and open HERE"; a digit sends you to a tab.
    // It takes `ink`, the same colour as the `▌` beside it, so `▌3` still
    // reads as one two-cell token — and adds no contrast surface, because that
    // colour is already painted in column 1 on both grounds and on `sel_bg`.
    match badge {
        Some(b) => spans.push(Span::styled(b.to_string(), base.fg(ink))),
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
    // Most `state: "done"` rows carry no `status` key (9 of 14 done rows on
    // 2026-08-31; the rest said `idle`), and an absent key parses to
    // `Status::Unknown("")`, so the state is resolved FIRST here for the same
    // reason `status_glyph` resolves it first — otherwise the list says
    // "Completed" while the detail block says "?".
    let status = if matches!(sess.state, Some(State::Done)) {
        "done".to_string()
    } else if matches!(sess.state, Some(State::Stopped)) {
        "stopped".to_string()
    } else if matches!(sess.state, Some(State::Blocked)) {
        "blocked".to_string()
    } else {
        match &sess.status {
            Status::Busy => "busy".to_string(),
            Status::Idle => "idle".to_string(),
            Status::Waiting => "waiting".to_string(),
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
/// What the footer is saying, in priority order, with no clock read anywhere:
/// `app.rs` expires both the delete window and the flashed message.
///
/// `Ctrl+X`'s open window outranks a flashed message on purpose. It is the one
/// line here that describes what the NEXT keypress will do, and it must not be
/// possible for an unrelated flash landing during those two seconds to take the
/// warning off the screen while the verb stays loaded (§8.2).
fn footer_message(app: &App, p: &Palette) -> Option<(String, Color)> {
    if let Some(hint) = app.arm_hint() {
        return Some((hint, p.red));
    }
    match (&app.message, &app.poll_error) {
        (Some((text, level)), _) => Some((
            text.clone(),
            match level {
                MsgLevel::Info => p.green,
                MsgLevel::Warn => p.yellow,
                MsgLevel::Error => p.red,
            },
        )),
        (None, Some(err)) => Some((format!("agents: {}", err.lines().next().unwrap_or("")), p.red)),
        (None, None) => None,
    }
}

fn overflow_message(app: &App, w: usize, p: &Palette) -> Option<(String, Color)> {
    if w == 0 || app.mode == Mode::Filter {
        return None;
    }
    let (text, color) = footer_message(app, p)?;
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
/// priority order. `d/u hide` sits fourth — above `C-x stop`, which announces
/// its own second half in the footer the moment it fires — because `d` is the
/// one key here that makes a row vanish on a single press, and the pair names
/// its own undo. At the default 34 columns the budget runs out after `x close`
/// and no fourth pair renders at all, so a narrow sidebar discovers `d` through
/// `?` and the README, exactly as it already discovers `C-x`.
///
/// `t tab` is inserted FOURTH rather than beside `o/s split`, so the 34-column
/// footer is unchanged: `x close` is the verb that acts on what is already on
/// screen, and evicting it to advertise a new one would be a bad trade at the
/// default width. `t` is discovered through `?` and the README, on the same
/// precedent `d/u hide` and `C-x stop` already set.
const HINTS: &[(&str, &str)] = &[
    ("⏎", "open"),
    ("o/s", "split"),
    ("x", "close"),
    ("t", "tab"),
    ("d/u", "hide"),
    ("C-x", "stop"),
    ("n", "new"),
    ("L", "logs"),
    ("/", "filter"),
    // LAST, deliberately. `R` is an upgrade verb pressed once after a
    // `cargo install`, not a working verb, so it must not evict a pair an
    // operator uses every minute. At the 34-column default it never renders,
    // and is discovered through `?` and the README exactly as `t` and `C-x`
    // already are.
    ("R", "restart"),
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

    // 2. `Ctrl+X`'s open delete window, else a flashed message, else a standing
    //    poll error. Their 2s and 4s lifetimes are `app.rs`'s business:
    //    rendering never reads a clock.
    if let Some((text, color)) = footer_message(app, p) {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_end(&text, w),
                Style::default().fg(color),
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
    ("t", "open in a new tab"),
    ("x", "close pane (agent lives)"),
    ("Ctrl-x", "stop session"),
    // Sized to survive the 34-column overlay uncut: the escalation's
    // CONSEQUENCE is the half an operator must be able to read, and the
    // two-second window is stated where it is actionable — the footer warning
    // that stands while it is open.
    ("Ctrl-x ×2", "delete it + worktree"),
    ("n", "new background session"),
    ("L", "logs for this session"),
    ("d", "hide row (pane stays)"),
    ("u", "undo the last hide"),
    ("/", "filter"),
    ("a", "toggle Completed group"),
    ("r", "refresh + re-even panes"),
    ("R", "restart ccmux + agents"),
    ("?", "this help"),
    ("q", "quit sidebar"),
    ("Esc", "cancel window/filter"),
    ("Ctrl-c", "quit from any mode"),
    ("▌N", "open in tab N (blank: here)"),
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

fn draw_prompt(f: &mut Frame, area: Rect, kind: PromptKind, prompt: Option<&Prompt>, p: &Palette) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let title = match kind {
        PromptKind::NewBackground => " new background session ",
    };
    let labels: &[&str] = match kind {
        PromptKind::NewBackground => &["cwd ", "task"],
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
            // Keep the cursor inside the visible window on a long value. The
            // window start is walked BACK from the cursor in display columns
            // (§6.6), not chars: subtracting a char count from a column budget
            // let a CJK value overrun the budget, and `truncate_end` then cut
            // the window's TAIL — the cursor and the text being typed — off
            // the screen. `char_width` is per-char, so a narrow-base VS16 pair
            // may be charged one column short — `truncate_end` below still
            // bounds the render, and the CJK case this fixes is exact.
            let chars: Vec<char> = value.chars().collect();
            let win = budget.saturating_sub(1);
            let mut start = cursor;
            let mut used = 0usize;
            while start > 0 {
                let cw = char_width(chars[start - 1]);
                if used + cw > win {
                    break;
                }
                used += cw;
                start -= 1;
            }
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
    // §8.6: the hint tracks the prompt's state. An armed mkdir offer outlives
    // the 4s footer flash, so the standing hint is what says the next ⏎
    // creates; otherwise the hint names where Tab goes — to the cwd field from
    // the task field, completing a directory prefix once there.
    let armed = prompt.map(|pr| pr.pending_create.is_some()).unwrap_or(false);
    let on_cwd = prompt.map(|pr| pr.focus == 0).unwrap_or(false);
    let hint = if armed {
        " ⏎ create cwd + run   Esc: cancel"
    } else if on_cwd {
        " Tab: complete dir   ⏎ run"
    } else {
        " Tab: cwd   ⏎ run   Esc: cancel"
    };
    lines.push(Line::from(Span::styled(
        truncate_end(hint, w),
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

/// The pane id string showing `session_id`, from `App::open` — the union of
/// every tab's `@ccmux_tab_map` intersected with the live pane list. Ties
/// already broke on the numeric part of `%N` when `open` was built, so this
/// module and `app.rs` can no longer disagree about which pane a row names.
#[cfg(test)]
fn pane_key_for(app: &App, session_id: &str) -> Option<String> {
    app.open.get(session_id).map(|o| o.pane.as_str().to_string())
}

/// Membership is not enough: `OpenPane::attached` is false for a pane whose
/// attach exited and left an interactive shell behind, and a shell is not the
/// session being "on screen". The `▌` would otherwise stay lit forever on a
/// pane the operator has since been using for something else entirely.
fn is_open(app: &App, session_id: &str) -> bool {
    app.open.get(session_id).is_some_and(|o| o.attached)
}

/// Gutter column 2: the TAB the session's pane lives in, inked only when that
/// is not the tab you are looking at. `None` renders a blank column.
///
/// The badge used to be `#{pane_index}`, which is per-window: with tabs, "pane
/// 2" exists in every one of them, so the number stopped naming anything. The
/// question a row must answer once `t` exists is "where is this session", and
/// the answer that lets you act is the tab — it is what `Enter` switches to and
/// what tmux's own `prefix-N` takes. Inside a tab the pane is already on screen
/// in front of you, and its exact index survives where it is actionable: the
/// `opened <name> in pane N` flash.
///
/// Blank for the current tab is the other half of that. A digit that could mean
/// "here" would have to be read against a tab number the operator must
/// remember; with this rule a digit ALWAYS means "somewhere else", and the
/// common single-tab session renders exactly the ink it rendered before —
/// marker, blank column 2 — instead of a constant column of identical digits.
/// The aqua `▌` in column 1 already says "this session is on screen".
///
/// When the sidebar cannot resolve its own window (degraded, or a no-panic
/// fixture that sets no pane inventory) the digit is shown unconditionally —
/// and so, since the shade follows the digit, is the neutral marker. That is
/// not the marker over-reaching. `App::resolve_identity` accepts `$TMUX_PANE`
/// only when it names a pane `list_panes_in_session` returned, and that listing
/// covers every window of the managed session, so a failure to resolve proves
/// this sidebar is being drawn somewhere the inventory does not reach —
/// `ccmux sidebar --session X` run by hand from another tmux session — and
/// every pane in it really is a tab away. With no inventory at all the question
/// does not arise: `rebuild_open` stamps no `window_index`, so this returns
/// `None` and the marker keeps today's `aqua` — and degraded mode does not even
/// get that far, since `refresh_panes` returns before `adopt_own_state` can put
/// a pane in the map for a row to be open by.
///
/// This is also THE "is it elsewhere?" test for the whole gutter: `session_line`
/// picks the open marker's shade from `is_some()` rather than repeating the
/// window comparison, so whatever this returns, the colour and the digit agree
/// by construction. Change the rule here and both cells follow.
fn tab_badge_for(app: &App, session_id: &str) -> Option<u32> {
    let open = app.open.get(session_id).filter(|o| o.attached)?;
    if open.window.is_some() && open.window == app.own_window {
        return None;
    }
    open.window_index
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
    use std::time::{Duration, Instant};

    fn sess(n: usize, kind: Kind, status: Status, state: Option<State>) -> Session {
        Session {
            id: match kind {
                Kind::Background => Some(format!("{n:08x}")),
                Kind::Interactive => None,
            },
            // Nothing the sidebar DRAWS reads the worker — `R`'s scope does,
            // and that is tested where the scope lives.
            pid: None,
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
            hidden_log: crate::tmux::HiddenLog::new(),
            last_frags: std::collections::BTreeMap::new(),
            seq_seen: 0,
            mode: Mode::Normal,
            prompt: None,
            logs: None,
            map: PaneMap::default(),
            map_dirty: false,
            sidebar_pane: None,
            panes: Vec::new(),
            // No own window: these fixtures render an `App` built by struct
            // literal, so the badge falls back to showing the tab digit for
            // every open row — the documented degraded rendering.
            own_pane: None,
            own_window: None,
            tabs: Vec::new(),
            open: std::collections::BTreeMap::new(),
            width_opt: None,
            sidebar_cmd: None,
            own_state_loaded: true,
            migrated: true,
            degraded: false,
            stop_arm: None,
            cx_last_press: None,
            pending_delete: None,
            pending_jump: None,
            message: None,
            msg_deadline: None,
            poll_error: None,
            fail_streak: 0,
            drift_seen: std::collections::BTreeSet::new(),
            drift_pending: std::collections::BTreeSet::new(),
            drift_flash: None,
            last_poll: Instant::now(),
            last_agents: Instant::now(),
            idle_streak: 0,
            payload_fp: None,
            force_poll: false,
            was_watched: true,
            quiesced: false,
            panes_fresh: true,
            tabs_fresh: true,
            should_quit: false,
            // Rendering never dispatches; a panic here is a rendering test
            // reaching into `agents`, which must be impossible.
            pending_restart: None,
            respawn: |_, _, _| panic!("ui test reached respawn_pane"),
            kill_pane: |_, _| panic!("ui test reached kill_pane"),
            probe: |_| panic!("ui test reached restart::probe"),
            dispatch: |_, _| panic!("ui test reached dispatch_background"),
            // `ui` draws; it never restarts anything. Both `R` seams panic so
            // a rendering test that somehow reached the agent pass would say
            // so rather than shelling out to the operator's live `claude`.
            agents_poll: || panic!("ui test reached agents::poll"),
            agents_stop: |_| panic!("ui test reached agents::stop"),
            agents_respawn: |_| panic!("ui test reached agents::respawn"),
            agents_attached: || panic!("ui test reached agents::attached_ids"),
            agent_budget: Duration::from_secs(60),
        }
    }

    /// A live pane in tab `window`. The badge reads `window_index`, so this is
    /// what a badge fixture varies.
    fn pane_in(id: &str, index: u32, left: u16, window: u32) -> PaneInfo {
        PaneInfo {
            id: PaneId::parse(id).expect("pane id"),
            index,
            left,
            top: 0,
            width: 60,
            height: 24,
            active: false,
            window_index: window,
            window_id: crate::tmux::WindowId::parse(&format!("@{window}")).expect("window id"),
            window_active: true,
            session_clients: 1,
            window_viewers: Some(1),
            window_zoomed: false,
            detached: false,
            shell: false,
        }
    }

    /// A ccmux tab: a window carrying `@ccmux_tab_sidebar`. `sidebar: None` is
    /// the operator's own window — a bare `prefix-c` — which is not a tab.
    fn win(window: u32, index: u32, sidebar: Option<&str>) -> crate::tmux::TabInfo {
        crate::tmux::TabInfo {
            window: crate::tmux::WindowId::parse(&format!("@{window}")).expect("window id"),
            index,
            sidebar: sidebar.and_then(PaneId::parse),
            map: PaneMap::new(),
            hidden: crate::tmux::HiddenLog::new(),
        }
    }

    /// The fixture behind every `draw_never_panics_*` sweep.
    ///
    /// The cycle is 7 long, not 4, and the last three rungs are why: it used to
    /// emit only Working/Busy, Working/Idle, Done and an unknown status, so no
    /// panic sweep ever rendered a `Group::Blocked` header, a `▲`, a `■`, or a
    /// `Status::Waiting` row at any of the hostile sizes. A guard test that
    /// cannot reach the newest code is not guarding it. Every glyph
    /// `status_glyph` can return is now reachable from here, and `many(40)` —
    /// the widest sweep, over every size and every mode — hits all seven.
    fn many(n: usize) -> Vec<Session> {
        (0..n)
            .map(|i| {
                let (status, state) = match i % 7 {
                    0 => (Status::Busy, Some(State::Working)),
                    1 => (Status::Idle, Some(State::Working)),
                    2 => (Status::Idle, Some(State::Done)),
                    3 => (Status::Unknown("weird".into()), None),
                    4 => (Status::Waiting, Some(State::Blocked)),
                    5 => (Status::Idle, Some(State::Stopped)),
                    _ => (Status::Waiting, Some(State::Unknown("halted".into()))),
                };
                let mut s = sess(i, Kind::Background, status, state);
                // Every fifth row has no short id. `parse_sessions` honours an
                // explicit `kind: "background"` even when the CLI omits `id`,
                // so the `—` fallback in the detail block is still reachable.
                if i % 5 == 0 {
                    s.id = None;
                }
                s
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
            Mode::Prompt(PromptKind::NewBackground),
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
                    pending_create: None,
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
        let mut app = app_with(many(7));
        app.degraded = true;
        app.poll_error = Some("claude: command not found\nsecond line".into());
        app.message = Some(("stopped bt/reg-update".into(), MsgLevel::Info));
        render(&app);
        app.message = Some(("stop failed: no such session".into(), MsgLevel::Error));
        app.dark = false;
        render(&app);
        app.message = Some(("no short id — cannot stop this session".into(), MsgLevel::Warn));
        render(&app);
    }

    #[test]
    fn draw_never_panics_with_out_of_range_selection_and_scroll() {
        let mut app = app_with(many(7));
        app.selected = 9_999;
        app.scroll = 9_999;
        render(&app);
        app.selected = 0; // a Row::Header index — must not be treated as a session
        app.scroll = 0;
        render(&app);
    }

    #[test]
    fn draw_never_panics_on_filtered_and_unicode_content() {
        let mut sessions = many(7);
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

    /// The open marker and the pane badge, driven by `@ccmux_map` — now the
    /// only thing that resolves a session to a pane.
    #[test]
    fn open_marker_renders_for_a_mapped_background_session() {
        let sessions = vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))];
        let sid = sessions[0].session_id.clone();
        let mut app = app_with(sessions);
        app.selected = 1;
        app.rebuild_open();
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
        app.rebuild_open();
        assert!(is_open(&app, &sid));
        assert_eq!(pane_key_for(&app, &sid).as_deref(), Some("%7"));

        let mut term = Terminal::new(TestBackend::new(40, 24)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let dump = term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>();
        assert!(dump.contains('▌'), "open marker must be drawn");
    }

    /// The pane outlives the attach now: `Ctrl+Z` exits `claude attach` and the
    /// pane command `exec`s a shell in its place. The `▌` and the tab badge are
    /// the sidebar's two claims that a session is ON SCREEN, and a shell is not
    /// the session — so both must go dark the moment the pane latches
    /// `@ccmux_detached`, while the row itself is otherwise unchanged.
    #[test]
    fn a_pane_that_fell_through_to_the_shell_is_no_longer_marked_open() {
        let sessions = vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))];
        let sid = sessions[0].session_id.clone();
        let mut app = app_with(sessions);
        app.selected = 1;
        app.own_window = crate::tmux::WindowId::parse("@1");
        app.panes = vec![pane_in("%1", 1, 0, 1), pane_in("%7", 2, 34, 2)];
        app.map.panes.insert(
            "%7".into(),
            PaneEntry {
                session_id: sid.clone(),
                short_id: "00000001".into(),
                name: "n".into(),
                opened_at: 0,
            },
        );
        app.rebuild_open();
        assert!(is_open(&app, &sid), "a live attach is open");
        assert_eq!(tab_badge_for(&app, &sid), Some(2), "and it is in another tab");
        let lit = rows_at(&app, 34, 8);
        assert!(lit.iter().any(|r| r.contains('▌')), "{lit:?}");

        // The attach exits; the pane is now the operator's shell.
        app.panes[1].detached = true;
        app.rebuild_open();
        assert!(!is_open(&app, &sid), "a shell is not the session being on screen");
        assert_eq!(tab_badge_for(&app, &sid), None, "and the badge must not point at it");
        let dark = rows_at(&app, 34, 8);
        assert!(!dark.iter().any(|r| r.contains('▌')), "{dark:?}");
        assert!(!dark.iter().any(|r| r.contains('2')), "the tab digit is gone too: {dark:?}");
        // Still ours, still in the map — `x` reaches it through `pane_of_any`.
        assert_eq!(pane_key_for(&app, &sid).as_deref(), Some("%7"));
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
        app.panes = vec![pane_in("%7", 3, 35, 3)];
        app.rebuild_open();

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
    /// The `?` overlay is where the binding is discovered, and the 34-column
    /// sidebar is the width it is discovered at. Both halves must render uncut
    /// there — a clipped `delete it + workt…` is the one truncation in this
    /// overlay that costs the operator the consequence.
    #[test]
    fn the_help_overlay_documents_ctrl_x_and_what_the_second_press_takes() {
        let mut app = app_with(many(3));
        app.mode = Mode::Help;
        app.help_lines = help_line_count();
        for w in [60u16, 34] {
            let rows = rows_at(&app, w, 40).join("\n");
            assert!(rows.contains("Ctrl-x"), "w={w}: {rows}");
            assert!(rows.contains("stop session"), "w={w}: {rows}");
            assert!(rows.contains("delete it + worktree"), "w={w}: {rows}");
            assert!(!rows.contains("stop session (confirm)"), "w={w}: {rows}");
        }
    }

    /// §8.2: while `Ctrl+X`'s window is open the footer must say what the next
    /// press does AND what it takes, and it must outrank the flashed message —
    /// nothing may take the warning off the screen while the verb is loaded.
    /// It is over-width by design, so it wraps into the detail block the same
    /// way an over-width refusal does, and every word survives.
    #[test]
    fn the_open_delete_window_owns_the_footer_and_says_what_it_takes() {
        let mut app = app_with(many(3));
        app.stop_arm = Some(crate::app::StopArm {
            session_id: "uuid-0001".into(),
            short_id: "1c45d64f".into(),
            name: "bt/reg-update".into(),
            at: Instant::now(),
        });
        // A flash from the stop that opened the window. The warning wins.
        app.message = Some(("stopped bt/reg-update".into(), MsgLevel::Info));

        let rows = rows_at(&app, 34, 24);
        let flat = rows[18..].join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("Ctrl+X again: delete bt/reg-update"), "{flat:?}");
        assert!(flat.contains("worktree"), "{flat:?}");
        assert!(flat.contains("cannot be undone"), "{flat:?}");
        assert!(!flat.contains("stopped bt/reg-update"), "the flash outranked the warning: {flat:?}");

        // Closed window: the flash comes back, unchanged.
        app.stop_arm = None;
        let rows = rows_at(&app, 34, 24);
        assert!(rows[23].contains("stopped bt/reg-update"), "{:?}", rows[23]);
    }

    /// REGRESSION. §6.8's overflow wrap used to need the detail block, which
    /// `slots` only allocates at height >= 12. Below that the armed warning —
    /// the longest line ccmux produces, and the only one whose TAIL carries the
    /// consequence — fell back to a one-line `truncate_end` and the operator
    /// was left looking at a loaded irreversible verb whose warning said
    /// neither what it takes nor that it cannot be undone.
    #[test]
    fn the_armed_warning_survives_a_sidebar_too_short_for_a_detail_block() {
        let mut app = app_with(many(3));
        app.stop_arm = Some(crate::app::StopArm {
            session_id: "uuid-0001".into(),
            short_id: "1c45d64f".into(),
            name: "bt/reg-update".into(),
            at: Instant::now(),
        });

        // Every height that can hold the warning at all but has no detail
        // block, plus the first height that has one. The message is 70 columns
        // and wraps to three lines at 34, so it needs three carved rows on top
        // of the header — h >= 5.
        for h in 5u16..=12 {
            let rows = rows_at(&app, 34, h);
            let flat = rows.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(flat.contains("Ctrl+X again: delete bt/reg-update"), "h={h}: {flat:?}");
            assert!(flat.contains("worktree"), "h={h}: {flat:?}");
            assert!(flat.contains("cannot be undone"), "h={h}: {flat:?}");
            // The header is never carved away, and neither is the last list row
            // above the block.
            assert!(rows[0].contains("sessions"), "h={h}: header lost: {:?}", rows[0]);
        }

        // h == 4 cannot hold all three lines without carving the list away
        // entirely, which the cap forbids. It still says what delete TAKES,
        // which the one-line truncation it replaced did not.
        let flat = rows_at(&app, 34, 4).join(" ");
        assert!(flat.contains("worktree"), "{flat:?}");

        // Nothing is carved when there is nothing over-wide to carve for: the
        // list keeps every row it had.
        app.stop_arm = None;
        app.message = Some(("stopped it".into(), MsgLevel::Info));
        let rows = rows_at(&app, 34, 10);
        assert!(rows[1].contains("Working"), "{:?}", rows[1]);
        assert!(rows[9].contains("stopped it"), "{:?}", rows[9]);
    }

    /// §8.2: `Esc`'s FIRST meaning is closing the delete window — `key_normal`
    /// gives the arm precedence over the filter — so the overlay that is the
    /// only place a narrow sidebar discovers keys must not still call it the
    /// filter key and nothing else.
    #[test]
    fn the_help_overlay_says_esc_closes_the_delete_window() {
        let mut app = app_with(many(3));
        app.mode = Mode::Help;
        app.help_lines = help_line_count();
        for w in [34u16, 60] {
            let rows = rows_at(&app, w, 40).join("\n");
            assert!(rows.contains("cancel window/filter"), "w={w}: {rows}");
            assert!(!rows.contains("Esc       clear filter"), "w={w}: {rows}");
        }
    }

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
        // Quoted verbatim from `App::stop_and_arm` — a refusal is the class
        // of message the operator must read in full, and this one is 38 display
        // columns, so a 34-column sidebar cannot show it without wrapping.
        const REFUSAL: &str = "no short id — cannot stop this session";
        assert!(
            display_width(REFUSAL) > 34,
            "fixture is no longer over-width, so it proves nothing: {REFUSAL:?}"
        );
        app.message = Some((REFUSAL.into(), MsgLevel::Warn));
        let rows = rows_at(&app, 34, 24);
        let tail = rows[20..].join(" ");
        // Whitespace-normalised, because the wrap lands mid-sentence and each
        // row is padded to the full width. Every word must survive the wrap.
        let flat = tail.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("no short id"), "{tail:?}");
        assert!(
            flat.contains(REFUSAL),
            "the message was clipped instead of wrapped: {tail:?}"
        );

        // A message that fits leaves the detail block alone.
        app.message = Some(("stopped af/reg".into(), MsgLevel::Info));
        let rows = rows_at(&app, 34, 24);
        assert!(rows[23].contains("stopped af/reg"), "{:?}", rows[23]);
        assert!(rows[20].contains("name"), "detail block lost: {:?}", rows[20]);
    }

    /// `Ctrl+X`'s delete flash grew a pane count (§8.2). With it the line is
    /// ~50 columns, over the 34-column default, so it has to wrap through the
    /// same overflow path as the arm hint — and every word has to survive,
    /// because the tail is the half that says what happened to the panes.
    #[test]
    fn the_counted_delete_flash_wraps_at_the_default_width() {
        let mut app = app_with(many(3));
        // Verbatim from `App::run_delete`.
        const FLASH: &str = "deleted bt/reg-update + worktree · closed 2 panes";
        assert!(
            display_width(FLASH) > 34,
            "fixture is no longer over-width, so it proves nothing: {FLASH:?}"
        );
        app.message = Some((FLASH.into(), MsgLevel::Warn));
        let rows = rows_at(&app, 34, 24);
        let tail = rows[20..].join(" ");
        let flat = tail.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains(FLASH),
            "the message was clipped instead of wrapped: {tail:?}"
        );
        // The count is the tail, and the tail is what the wrap exists for.
        assert!(flat.ends_with("closed 2 panes"), "{flat:?}");
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

    /// The detail block must not contradict the row above it. It said `?` for
    /// a blocked session while the list said Idle; now both say the same word.
    #[test]
    fn detail_block_says_blocked_for_a_blocked_session() {
        let app = app_with(vec![sess(
            1,
            Kind::Background,
            Status::Waiting,
            Some(State::Blocked),
        )]);
        let rows = rows_at(&app, 40, 24);
        let id_line = rows
            .iter()
            .find(|r| r.trim_start().starts_with("id"))
            .cloned()
            .unwrap_or_default();
        assert!(id_line.contains("blocked"), "{id_line:?}");
        assert!(!id_line.contains('?'), "{id_line:?}");
        // And the `waiting` status word survives on its own, for a row whose
        // `state` this build cannot name.
        let app = app_with(vec![sess(2, Kind::Background, Status::Waiting, None)]);
        let rows = rows_at(&app, 40, 24);
        let id_line = rows
            .iter()
            .find(|r| r.trim_start().starts_with("id"))
            .cloned()
            .unwrap_or_default();
        assert!(id_line.contains("waiting"), "{id_line:?}");
    }

    /// `draw_detail`'s `Kind` match is kept TOTAL on purpose, and this is the
    /// test that keeps the `Interactive` arm honest. `Kind::Interactive`
    /// survives in `model`, `parse_sessions` still emits it, and the arm is
    /// what a change to §3.5's listing policy would land on — but nothing
    /// reachable through `apply_poll` renders one, so the row is built here
    /// directly. That is the point, not a shortcut: delete this test only if
    /// the arm itself goes.
    #[test]
    fn the_detail_block_still_names_an_interactive_kind() {
        let app = app_with(vec![sess(1, Kind::Interactive, Status::Busy, None)]);
        let rows = rows_at(&app, 40, 24);
        let id_line = rows
            .iter()
            .find(|r| r.trim_start().starts_with("id"))
            .cloned()
            .unwrap_or_default();
        assert!(id_line.contains("interactive"), "{id_line:?}");
        assert!(!id_line.contains("background"), "{id_line:?}");
        // No short id, so the id column falls back to the em dash.
        assert!(id_line.contains('\u{2014}'), "{id_line:?}");
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

    /// The header's one-character health report, across all four states it can
    /// be in. The quiesced dot has to be distinguishable from the healthy one
    /// (the operator learns the pane was paused) and from the error one (it is
    /// not a fault) — so it is hollow, and it is dim rather than yellow.
    #[test]
    fn the_header_dot_tells_paused_apart_from_healthy_and_from_failed() {
        let dot = |app: &App| {
            let rows = rows_at(app, 40, 24);
            rows.first()
                .and_then(|r| r.chars().nth(38))
                .unwrap_or(' ')
                .to_string()
        };

        let mut app = app_with(many(3));
        assert_eq!(dot(&app), "●", "a healthy sidebar");

        app.quiesced = true;
        assert_eq!(dot(&app), "○", "nobody watching: the dot goes hollow");

        // A failure still outranks it: a quiesced gate must never be able to
        // hide a `claude` that is broken.
        app.poll_error = Some("claude: connection refused".into());
        assert_eq!(dot(&app), "●", "the error dot lost to the quiesced one");
        app.poll_error = None;
        app.degraded = true;
        assert_eq!(dot(&app), "○");
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
        let idle_no_state = sess(4, Kind::Background, Status::Idle, None);
        assert_eq!(status_glyph(&idle_no_state, &p), ("○", p.gray));

        // `?` still means "this build does not recognize the value", and such a
        // row is still filed under Idle. Neither may change: `?` is the honest
        // answer for a value this build has never seen, and the drift guard —
        // not a re-grouping — is what makes it loud.
        let odd_state = sess(5, Kind::Background, Status::Idle, Some(State::Unknown("hibernating".into())));
        assert_eq!(status_glyph(&odd_state, &p), ("?", p.purple));
        assert_eq!(odd_state.group(), Group::Idle);
        let odd_status = sess(6, Kind::Background, Status::Unknown("thinking".into()), None);
        assert_eq!(status_glyph(&odd_status, &p), ("?", p.purple));
        assert_eq!(odd_status.group(), Group::Idle);
    }

    /// The bug, at the glyph: a blocked session used to render purple `?`.
    /// It renders `▲` in `p.yellow`, on either signal, on both palettes, and
    /// the glyph is unique in the table — no two rows may share a mark.
    #[test]
    fn a_blocked_session_gets_its_own_glyph_and_not_the_unknown_one() {
        for p in [Palette::light(), Palette::dark()] {
            let blocked = sess(1, Kind::Background, Status::Waiting, Some(State::Blocked));
            assert_eq!(status_glyph(&blocked, &p), ("▲", p.yellow));
            assert_ne!(status_glyph(&blocked, &p), ("?", p.purple), "the old bug");

            // The status alone is enough, even when the state is unmodelled —
            // otherwise the same bug just moves one field over.
            let renamed = sess(2, Kind::Background, Status::Waiting, Some(State::Unknown("halted".into())));
            assert_eq!(status_glyph(&renamed, &p), ("▲", p.yellow));

            // Distinct from every other glyph in the table.
            let others = [
                sess(3, Kind::Background, Status::Busy, Some(State::Working)),
                sess(4, Kind::Background, Status::Idle, Some(State::Working)),
                sess(5, Kind::Background, Status::Idle, None),
                sess(6, Kind::Background, Status::Idle, Some(State::Done)),
                sess(7, Kind::Background, Status::Idle, Some(State::Stopped)),
                sess(8, Kind::Background, Status::Unknown("thinking".into()), None),
            ];
            for o in &others {
                assert_ne!(status_glyph(o, &p).0, "▲", "{:?} stole the Blocked glyph", o.state);
            }
        }
        // One display column, like every other glyph on the grid. Measured in
        // tmux 3.4 with `#{cursor_x}`: U+25B2 is one cell.
        assert_eq!(display_width("▲"), 1);
    }

    /// THE REGRESSION TEST for the two rules that drifted apart.
    ///
    /// `status_glyph` used to re-derive "is this blocked?" from `state` and
    /// `status` instead of asking `group()`, and the two answers disagreed on
    /// exactly one input: `state: "working", status: "waiting"` drew the yellow
    /// `▲` — the mark that means "answer me" — under the **Working** heading,
    /// where `Tab`'s Blocked stop cannot reach it and where it is not first on
    /// screen. The mark said one thing and the sort said another.
    ///
    /// Exhaustive over every state x status pair this build can hold, both
    /// palettes: the `▲` appears if and only if the row is in `Group::Blocked`.
    /// Any future edit that gives either rule a clause the other lacks fails
    /// here, naming the pair.
    #[test]
    fn the_blocked_glyph_and_the_blocked_group_never_disagree() {
        let states = [
            None,
            Some(State::Working),
            Some(State::Done),
            Some(State::Stopped),
            Some(State::Blocked),
            Some(State::Unknown("halted".into())),
        ];
        let statuses = [
            Status::Busy,
            Status::Idle,
            Status::Waiting,
            Status::Unknown(String::new()),
            Status::Unknown("pondering".into()),
        ];
        for p in [Palette::light(), Palette::dark()] {
            for state in &states {
                for status in &statuses {
                    let mut s = sess(1, Kind::Background, status.clone(), state.clone());
                    s.name = "n".into();
                    let (glyph, colour) = status_glyph(&s, &p);
                    let blocked = s.group() == Group::Blocked;
                    assert_eq!(
                        glyph == "▲",
                        blocked,
                        "state={state:?} status={status:?}: glyph {glyph:?} vs group {:?}",
                        s.group()
                    );
                    if blocked {
                        assert_eq!(colour, p.yellow, "state={state:?} status={status:?}");
                    }
                }
            }
        }
    }

    /// The specific pair that was wrong, stated on its own so a failure names
    /// the scenario and not just a matrix cell: the likeliest way the CLI
    /// breaks this build a third time is to stop emitting `state: "blocked"`
    /// and leave a parked session reading `working` + `waiting`. That row must
    /// reach the TOP of the list, not merely draw a yellow mark in the middle
    /// of it.
    #[test]
    fn a_working_row_that_is_waiting_on_a_human_is_still_hoisted() {
        let p = Palette::dark();
        let renamed = sess(1, Kind::Background, Status::Waiting, Some(State::Working));
        assert_eq!(renamed.group(), Group::Blocked, "a waiting row stayed under Working");
        assert_eq!(status_glyph(&renamed, &p), ("▲", p.yellow));

        // And it really is first on screen, above a genuinely working row.
        let mut working = sess(2, Kind::Background, Status::Busy, Some(State::Working));
        working.started_at = 9_999_999;
        let app = app_with(vec![working, renamed]);
        assert_eq!(app.rows[0], Row::Header { group: Group::Blocked, count: 1 });
        assert!(matches!(app.rows[1], Row::Session { idx: 1 }));

        // A FINISHED session is waiting on nobody, whatever `status` says, and
        // 5 of 14 live `done` rows do carry a status word. Terminal wins.
        for st in [State::Done, State::Stopped] {
            let fin = sess(3, Kind::Background, Status::Waiting, Some(st.clone()));
            assert_eq!(fin.group(), Group::Completed, "{st:?} + waiting was hoisted");
            assert_ne!(status_glyph(&fin, &p).0, "▲", "{st:?} + waiting drew the blocked mark");
        }
    }

    /// The row that most deserves attention is the first one on screen, its
    /// header is the first header, and at the default 34 columns the grid it
    /// sits on is byte-for-byte the grid every other row sits on.
    #[test]
    fn a_blocked_row_leads_the_list_and_keeps_the_thirty_four_column_grid() {
        let mut blocked = sess(1, Kind::Background, Status::Waiting, Some(State::Blocked));
        blocked.name = "client statement automation".into();
        blocked.started_at = 0; // OLDEST: the group must outrank the age
        let mut working = sess(2, Kind::Background, Status::Busy, Some(State::Working));
        working.started_at = 4_000_000;
        let app = app_with(vec![working, blocked]);

        assert_eq!(app.rows[0], Row::Header { group: Group::Blocked, count: 1 });
        assert!(matches!(app.rows[1], Row::Session { idx: 1 }));

        let p = Palette::dark();
        let cols = line_cols(&session_line(&app, &app.sessions[1], false, 34, &p));
        assert_eq!(cols.len(), 34, "the row is not 34 columns wide");
        // The gutter is unchanged: marker, badge, glyph, space.
        assert_eq!(cols[2], '▲', "the glyph must sit on column 3 like every other");
        assert_eq!(cols[3], ' ');
        assert_eq!(cols[4], 'c', "the name still starts on column 5");
        assert_eq!(cols[33], ' ', "column W is still the margin");
        assert_eq!(cols[31..33].iter().collect::<String>(), "1h", "the age still ends on W-1");
        // The header above it is 34 columns too, and says Blocked.
        let h = line_cols(&group_header_line(Group::Blocked, 1, 34, &p));
        assert_eq!(h.len(), 34);
        assert_eq!(h[0..11].iter().collect::<String>(), " ── Blocked");

        // The glyph is painted yellow, and nothing else on the row is.
        let line = session_line(&app, &app.sessions[1], false, 34, &p);
        let yellow: Vec<String> = line
            .spans
            .iter()
            .filter(|sp| sp.style.fg == Some(p.yellow))
            .map(|sp| sp.content.to_string())
            .collect();
        assert_eq!(yellow, vec!["▲".to_string()]);
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

    /// §8.6: the prompt's hint line is the standing statement of what `Tab`
    /// and `⏎` do RIGHT NOW — where Tab goes from the task field, that it
    /// completes in the cwd field, and (outliving the 4s footer flash) that an
    /// armed mkdir offer makes the next Enter create.
    #[test]
    fn the_prompt_hint_tracks_focus_and_the_armed_mkdir_offer() {
        let mut app = app_with(vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))]);
        app.mode = Mode::Prompt(PromptKind::NewBackground);
        let prompt = |focus: usize, pending: Option<&str>| Prompt {
            kind: PromptKind::NewBackground,
            fields: vec!["/home/dev/projects".into(), "task text".into()],
            focus,
            cursor: 0,
            pending_create: pending.map(str::to_string),
        };
        let dump = |app: &App| {
            let mut term = Terminal::new(TestBackend::new(40, 24)).unwrap();
            term.draw(|f| draw(f, app)).unwrap();
            term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>()
        };

        app.prompt = Some(prompt(1, None));
        let d = dump(&app);
        assert!(d.contains("Tab: cwd"), "task focus must advertise Tab → cwd: {d:?}");

        app.prompt = Some(prompt(0, None));
        let d = dump(&app);
        assert!(d.contains("Tab: complete"), "cwd focus must advertise completion: {d:?}");

        app.prompt = Some(prompt(0, Some("/home/dev/projects")));
        let d = dump(&app);
        assert!(d.contains("create cwd"), "an armed offer must be visible in the hint: {d:?}");
    }

    /// The focused field's visible window is a display-column walk back from
    /// the cursor. Charging a CJK char one column (a char-count subtraction)
    /// overfilled the window and `truncate_end` then cut its TAIL — the very
    /// characters being typed — off the screen.
    #[test]
    fn the_focused_field_window_keeps_a_cjk_tail_visible() {
        let mut app = app_with(vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))]);
        app.mode = Mode::Prompt(PromptKind::NewBackground);
        let value = format!("/tmp/{}尾巴", "汉".repeat(30));
        let cursor = value.chars().count();
        app.prompt = Some(Prompt {
            kind: PromptKind::NewBackground,
            fields: vec![value, "task".into()],
            focus: 0,
            cursor,
            pending_create: None,
        });
        let mut term = Terminal::new(TestBackend::new(40, 24)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        // Wide glyphs leave continuation cells behind them; squash blanks so
        // the tail can be matched as a contiguous string.
        let squashed: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .chars()
            .filter(|c| *c != ' ')
            .collect();
        assert!(
            squashed.contains("尾巴"),
            "the tail at the cursor must be inside the visible window: {squashed:?}"
        );
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
    /// indices that clamp the badge to `+` (12, 999) and both palettes.
    ///
    /// The row this sweeps is Working/Busy throughout; every OTHER glyph is
    /// swept by `every_glyph_holds_the_grid_at_every_width` below.
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
                app.panes = vec![pane_in("%7", 1, 0, i)];
            }
            app.rebuild_open();
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

    /// The same invariant, swept across EVERY row of the glyph table instead of
    /// across hostile names.
    ///
    /// `every_session_line_is_exactly_the_sidebar_width` drives a single
    /// Working/Busy session, so `●` was the only mark it ever measured: `✓`,
    /// `■`, `○`, `?` and the new `▲` were pinned at exactly one width — 34 — in
    /// one bespoke test each. Every glyph is one display column, so the grid
    /// cannot break today; this is here so that a two-column mark slipped into
    /// the table later fails a width sweep rather than shipping a torn
    /// selection band. Split out from the sweep above rather than multiplied
    /// into it, because the name and age dimensions do not interact with the
    /// gutter and the cross-product costs seconds.
    #[test]
    fn every_glyph_holds_the_grid_at_every_width() {
        let variants: &[(Status, Option<State>)] = &[
            (Status::Busy, Some(State::Working)),                        // ●
            (Status::Idle, Some(State::Working)),                        // ◐
            (Status::Idle, None),                                        // ○
            (Status::Unknown(String::new()), Some(State::Done)),         // ✓
            (Status::Idle, Some(State::Stopped)),                        // ■
            (Status::Waiting, Some(State::Blocked)),                     // ▲
            (Status::Idle, Some(State::Blocked)),                        // ▲, state only
            (Status::Waiting, Some(State::Working)),                     // ▲, status only
            (Status::Waiting, Some(State::Unknown("halted".into()))),    // ▲, renamed state
            (Status::Unknown("pondering".into()), None),                 // ?
        ];
        let mut app = app_with(vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))]);
        for (status, state) in variants {
            app.sessions[0].status = status.clone();
            app.sessions[0].state = state.clone();
            // The mark itself must be one cell, or the grid arithmetic is a lie.
            let (glyph, _) = status_glyph(&app.sessions[0], &Palette::dark());
            assert_eq!(display_width(glyph), 1, "{glyph:?} is not one display column");
            for name in ["", "alpha/opt", "回归模型数据清洗与因子测试流水线重构任务"] {
                app.sessions[0].name = name.to_string();
                for selected in [false, true] {
                    for w in 0..=120usize {
                        for pal in [Palette::light(), Palette::dark()] {
                            let l = session_line(&app, &app.sessions[0], selected, w, &pal);
                            assert_eq!(
                                line_w(&l),
                                w,
                                "w={w} sel={selected} name={name:?} \
                                 status={status:?} state={state:?} glyph={glyph:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// SPEC §6.3 prints a sample of the group headers at W=34. It went stale —
    /// it showed a full-width `── Blocked (2) ─────` rule that
    /// `group_header_line` stopped drawing, with parentheses it stopped
    /// emitting — and nothing caught it, because no test read the block. This
    /// reads it: every fenced line in §6.3 must be a line the renderer actually
    /// produces. A doc that disagrees with the code is worse than no doc, and
    /// §6.3 is where the next implementer looks before touching the header.
    #[test]
    fn the_spec_group_header_sample_is_what_the_renderer_draws() {
        const SPEC: &str = include_str!("../SPEC.md");
        let start = SPEC.find("### 6.3 Group headers").expect("SPEC §6.3 is gone");
        let body = &SPEC[start..];
        let open = start + body.find("```").expect("§6.3 has no sample block") + 3;
        let close = open + SPEC[open..].find("```").expect("§6.3's sample block is unterminated");
        let sample: Vec<&str> = SPEC[open..close].lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(sample.len(), 4, "§6.3 should sample all four groups: {sample:?}");

        let p = Palette::dark();
        let drawn: Vec<String> = [
            (Group::Blocked, 2usize),
            (Group::Working, 3),
            (Group::Idle, 1),
            (Group::Completed, 12),
        ]
        .iter()
        .map(|&(g, c)| {
            group_header_line(g, c, 34, &p)
                .spans
                .iter()
                .flat_map(|sp| sp.content.chars())
                .collect::<String>()
        })
        .collect();

        for (want, got) in sample.iter().zip(&drawn) {
            // Trailing spaces are the margin column; a Markdown block cannot be
            // trusted to keep them, so compare the inked part.
            assert_eq!(
                want.trim_end(),
                got.trim_end(),
                "SPEC §6.3 shows {want:?} but the renderer draws {got:?}"
            );
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
        app.panes = vec![pane_in("%7", 1, 0, 2)];
        app.rebuild_open();

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

    /// The badge names a TAB, and says nothing when the answer is "you are
    /// already looking at it". At the default 34 columns, with the whole grid
    /// asserted column by column.
    #[test]
    fn the_tab_badge_is_blank_here_and_a_digit_elsewhere() {
        let p = Palette::light();
        let mut app = app_with(vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))]);
        app.sessions[0].name = "alpha/opt".into();
        let sid = app.sessions[0].session_id.clone();
        app.own_pane = PaneId::parse("%1");
        app.own_window = crate::tmux::WindowId::parse("@2");
        app.map.panes.insert("%7".into(), PaneEntry {
            session_id: sid,
            short_id: "00000001".into(),
            name: "n".into(),
            opened_at: 0,
        });

        // Open in MY tab: marker, blank badge — exactly the ink a single-tab
        // sidebar rendered before tabs existed.
        app.panes = vec![pane_in("%1", 1, 0, 2), pane_in("%7", 2, 34, 2)];
        app.rebuild_open();
        let here = line_cols(&session_line(&app, &app.sessions[0], false, 34, &p));
        assert_eq!(here[0..4].iter().collect::<String>(), "▌ ● ", "open, and open here");

        // Open in tab 5: the digit that tells you where to go.
        app.panes = vec![pane_in("%1", 1, 0, 2), pane_in("%7", 2, 34, 5)];
        app.rebuild_open();
        let there = line_cols(&session_line(&app, &app.sessions[0], false, 34, &p));
        assert_eq!(there[0..4].iter().collect::<String>(), "▌5● ", "open over in tab 5");

        // The badge is the ONLY thing that differs: the name column's left
        // edge and the whole rest of the row are byte-identical.
        assert_eq!(
            here[2..].iter().collect::<String>(),
            there[2..].iter().collect::<String>(),
            "the badge moved the rest of the row"
        );
        assert_eq!(line_w(&session_line(&app, &app.sessions[0], false, 34, &p)), 34);

        // Not open at all: both gutter columns stay quiet.
        app.map.panes.clear();
        app.rebuild_open();
        let closed = line_cols(&session_line(&app, &app.sessions[0], false, 34, &p));
        assert_eq!(closed[0..4].iter().collect::<String>(), "  ● ");
    }

    /// `tab N` is the only wayfinding the operator gets, because ccmux runs its
    /// session with `status off` and tmux's own window list is not on screen.
    /// It stays out of the way until there is a second tab to name.
    #[test]
    fn the_header_chip_appears_only_once_a_second_tab_exists() {
        let mut app = app_with(many(7));
        app.own_pane = PaneId::parse("%1");
        app.own_window = crate::tmux::WindowId::parse("@2");

        // One tab: the header is byte-identical to what it rendered before.
        app.panes = vec![pane_in("%1", 1, 0, 2)];
        app.tabs = vec![win(2, 2, Some("%1"))];
        let solo = rows_at(&app, 34, 24)[0].clone();
        assert!(!solo.contains("tab"), "one tab needs no chip: {solo:?}");

        // Three tabs, this sidebar in the one at index 2.
        app.panes = vec![pane_in("%0", 1, 0, 1), pane_in("%1", 1, 0, 2), pane_in("%3", 1, 0, 5)];
        app.tabs = vec![win(1, 1, Some("%0")), win(2, 2, Some("%1")), win(5, 5, Some("%3"))];
        let row = rows_at(&app, 34, 24)[0].clone();
        assert!(row.starts_with(" ccmux  tab 2  7 sessions"), "{row:?}");
        let cols: Vec<char> = row.chars().collect();
        assert_eq!(cols[32], '●', "the poll dot keeps the rail at column W-1");
        assert_eq!(cols[33], ' ', "column W stays the margin");

        // A count wide enough to crowd the chip degrades the COUNT, never the
        // rail: the dot is still on column W-1.
        app.filter = "session number 1".into();
        app.rows = build_rows(&app.sessions, &app.filter, true, &[]);
        for w in [20u16, 26, 34, 40] {
            let row = rows_at(&app, w, 24)[0].clone();
            let cols: Vec<char> = row.chars().collect();
            assert_eq!(cols[w as usize - 2], '●', "w={w}: {row:?}");
            assert_eq!(cols[w as usize - 1], ' ', "w={w}: {row:?}");
        }
    }

    /// REGRESSION. The chip used to read `tab {window_index}/{window count}`,
    /// which are only reconcilable while the indices are a contiguous `1..M`.
    /// tmux's `renumber-windows` defaults to OFF and `configure_session` never
    /// turns it on, so closing a middle tab left a gap and the header claimed
    /// `tab 4/2` — tab four of two. And the count came from windows that merely
    /// had panes, so the operator's own `prefix-c` window summoned a chip and
    /// inflated it, while `heal_sidebar` correctly refused to treat that window
    /// as a tab at all.
    #[test]
    fn the_header_chip_never_claims_a_tab_number_larger_than_the_count() {
        let mut app = app_with(many(4));
        app.own_pane = PaneId::parse("%9");
        app.own_window = crate::tmux::WindowId::parse("@4");

        // Two tabs left at indices 1 and 4 — the middle two were closed.
        app.panes = vec![pane_in("%0", 1, 0, 1), pane_in("%9", 1, 0, 4)];
        app.tabs = vec![win(1, 1, Some("%0")), win(4, 4, Some("%9"))];
        let row = rows_at(&app, 34, 24)[0].clone();
        assert!(row.starts_with(" ccmux  tab 4  4 sessions"), "{row:?}");
        assert!(!row.contains('/'), "no denominator to disagree with: {row:?}");

        // One ccmux tab plus a window the operator made themselves: not two
        // tabs, so no chip at all.
        app.panes = vec![pane_in("%9", 1, 0, 4), pane_in("%5", 1, 0, 7)];
        app.tabs = vec![win(4, 4, Some("%9")), win(7, 7, None)];
        let row = rows_at(&app, 34, 24)[0].clone();
        assert!(!row.contains("tab"), "a bare prefix-c window is not a tab: {row:?}");
    }

    #[test]
    fn the_help_overlay_and_footer_document_the_tab_key() {
        assert!(KEYS.iter().any(|(k, a)| *k == "t" && a.contains("tab")));
        assert!(KEYS.iter().any(|(k, _)| k.contains('▌')), "the badge is explained");
        assert!(HINTS.iter().any(|(k, a)| *k == "t" && *a == "tab"));
        assert!(!KEYS.iter().any(|(k, _)| *k == "c"), "`c` stays deleted");
        assert_eq!(help_line_count(), KEYS.len());

        let mut app = app_with(many(3));
        app.mode = Mode::Help;
        app.help_lines = help_line_count();
        let dump = rows_at(&app, 40, 30).join("\n");
        assert!(dump.contains("open in a new tab"), "{dump}");
    }

    /// `R` is documented in both places an operator looks, and the 34-column
    /// footer is unchanged by it — the pair sits last, below every working verb.
    #[test]
    fn the_help_overlay_and_footer_document_the_restart_key() {
        assert!(KEYS.iter().any(|(k, a)| *k == "R" && a.contains("restart")));
        // The agents are the population `R` used to miss, and the overlay is
        // where an operator finds out that pressing it touches them at all.
        assert!(
            KEYS.iter().any(|(k, a)| *k == "R" && a.contains("agents")),
            "the help overlay does not say `R` restarts agents"
        );
        assert!(HINTS.iter().any(|(k, a)| *k == "R" && *a == "restart"));
        assert_eq!(HINTS.last().map(|(k, _)| *k), Some("R"), "`R` is the last pair");

        let mut app = app_with(many(3));
        app.mode = Mode::Help;
        app.help_lines = help_line_count();
        let dump = rows_at(&app, 40, 34).join("\n");
        assert!(dump.contains("restart ccmux + agents"), "{dump}");

        // The label survives the 34-column overlay uncut: the key column is 10
        // wide and the body 32, which leaves 22 for a 22-column label.
        app.mode = Mode::Help;
        let narrow = rows_at(&app, 34, 34).join("\n");
        assert!(narrow.contains("restart ccmux + agents"), "{narrow}");
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
        app.rebuild_open();
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
        fn tab(app: &mut App, window: u32) {
            app.panes = vec![pane_in("%7", 1, 0, window)];
            app.rebuild_open();
        }

        for w in [20usize, 24, 28, 34, 44] {
            tab(&mut app, 3);
            let single = line_cols(&session_line(&app, &app.sessions[0], false, w, &p));
            for index in [10u32, 12, 99, 999] {
                tab(&mut app, index);
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
        let blocked = sess(4, Kind::Background, Status::Waiting, Some(State::Blocked));
        assert_eq!(name_tier(&working, &p), p.fg);
        assert_eq!(name_tier(&idle, &p), p.gray);
        assert_eq!(name_tier(&done, &p), p.dim);
        // Blocked shares Working's rung: there is nothing above `p.fg`, and the
        // ladder must not be inverted to make room.
        assert_eq!(name_tier(&blocked, &p), p.fg);
    }

    /// The new group's accent is subject to the same floor as every colour that
    /// carries a mark, on BOTH grounds and on the selection band — a glyph that
    /// means "answer me" is worthless if it cannot be seen.
    #[test]
    fn the_blocked_accent_clears_the_contrast_floor_on_both_grounds() {
        fn lum(c: Color) -> f64 {
            let Color::Rgb(r, g, b) = c else { panic!("not true-colour: {c:?}") };
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
        for (name, p, bg) in [
            ("dark", Palette::dark(), Color::Rgb(0x28, 0x28, 0x28)),
            ("light", Palette::light(), Color::Rgb(0xfb, 0xf1, 0xc7)),
        ] {
            let g = group_accent(Group::Blocked, &p);
            assert_eq!(g, p.yellow);
            let on_bg = ratio(g, bg);
            let on_sel = ratio(g, p.sel_bg);
            assert!(on_bg >= 4.0, "{name}: blocked accent is {on_bg:.2}:1 on the ground");
            assert!(on_sel >= 4.0, "{name}: blocked accent is {on_sel:.2}:1 on sel_bg");
            // Distinct from every accent the list already spends.
            for (other, c) in [
                ("working", p.orange),
                ("idle", p.blue),
                ("completed", p.gray),
                ("done", p.green),
                ("unknown", p.purple),
            ] {
                assert_ne!(g, c, "{name}: the Blocked accent collides with {other}");
            }
        }
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
            (44u16, "⏎ open  o/s split  x close  t tab"),
            // `t tab` sits FOURTH, so the 34-column footer is unchanged by it.
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

    /// The sRGB transfer curve, undone: a palette entry's three 8-bit channels
    /// as linear light in 0..1. One copy, shared by `lum` and `lab` below, so
    /// the two ways this module measures colour cannot disagree about what the
    /// bytes mean.
    fn linear(c: Color) -> (f64, f64, f64) {
        let Color::Rgb(r, g, b) = c else {
            panic!("palette entries must be true-colour: {c:?}")
        };
        let f = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        (f(r), f(g), f(b))
    }

    /// WCAG relative luminance and the contrast ratio between two palette
    /// entries. One copy, shared by the three colour tests below, so they
    /// cannot drift into measuring different things.
    fn lum(c: Color) -> f64 {
        let (r, g, b) = linear(c);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    fn ratio(a: Color, b: Color) -> f64 {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// CIE L*a*b* under D65, the white point sRGB is defined against. Unlike
    /// `lum` it keeps the two chromatic axes, so it can see a difference of HUE
    /// at equal brightness — precisely what a contrast ratio is blind to.
    /// (The luminance row here is the full-precision sRGB matrix; `lum` above
    /// uses the coefficients WCAG rounds it to, which is what WCAG specifies.)
    fn lab(c: Color) -> (f64, f64, f64) {
        let (r, g, b) = linear(c);
        // sRGB -> CIEXYZ, each axis normalised by the D65 white point.
        let x = (0.412_456_4 * r + 0.357_576_1 * g + 0.180_437_5 * b) / 0.950_47;
        let y = 0.212_672_9 * r + 0.715_152_2 * g + 0.072_175_0 * b;
        let z = (0.019_333_9 * r + 0.119_192_0 * g + 0.950_304_1 * b) / 1.088_83;
        let f = |t: f64| {
            if t > 216.0 / 24389.0 { t.cbrt() } else { (841.0 / 108.0) * t + 4.0 / 29.0 }
        };
        let (fx, fy, fz) = (f(x), f(y), f(z));
        (116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz))
    }

    /// CIE76 ΔE — plain Euclidean distance in Lab. Crude next to CIEDE2000, and
    /// entirely good enough for the only question asked of it: are these two
    /// inks the same colour, or two colours?
    fn delta_e(a: Color, b: Color) -> f64 {
        let (l1, a1, b1) = lab(a);
        let (l2, a2, b2) = lab(b);
        ((l1 - l2).powi(2) + (a1 - a2).powi(2) + (b1 - b2).powi(2)).sqrt()
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
    ///
    /// `aqua_elsewhere` is held to both halves like any other accent: the open
    /// marker it paints is drawn on the band whenever its row is selected, so
    /// clearing the ground alone would not be enough.
    #[test]
    /// NOTE: `aqua_elsewhere` is deliberately NOT in these loops. It is the
    /// only palette entry exempt from the 4.0 floor, because it paints a
    /// one-column block whose meaning is duplicated by the tab digit beside it
    /// — see `the_two_open_marker_shades_never_collapse`, which holds it to a
    /// non-vanishing bound and to a hue distance from `aqua` instead.
    fn light_accents_are_readable_and_the_band_is_safe() {
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
            // Everything `session_line` can paint on a selected row. `yellow`
            // joined this list with the Blocked glyph: before that it only ever
            // appeared on the header's poll dot, which is never on the band.
            for (field, c) in [
                ("fg", p.fg),
                ("gray", p.gray),
                ("aqua", p.aqua),
                ("green", p.green),
                ("blue", p.blue),
                ("purple", p.purple),
                ("orange", p.orange),
                ("yellow", p.yellow),
            ] {
                let r = ratio(c, p.sel_bg);
                assert!(r >= 4.0, "{name}.{field} is {r:.2}:1 on sel_bg, needs >= 4.0:1");
            }
        }
    }

    /// The two open-marker inks must stay two inks. Collapsing them — by
    /// pointing `aqua_elsewhere` back at `aqua`, or by nudging one until the
    /// pair is indistinguishable — would silently delete the whole signal while
    /// every contrast assertion above still passed.
    ///
    /// SEPARATION IS MEASURED IN LAB, NOT AS A LUMINANCE RATIO. This test used
    /// to demand the pair sit >= 1.5:1 apart in WCAG contrast, and that metric
    /// was the wrong axis — it is what broke the marker. A contrast ratio can
    /// only see one ink being DARKER than the other, so the only way to satisfy
    /// it was to keep pushing `aqua_elsewhere` down, until at `#1d3a2a` and
    /// 10.95:1 on the light ground the marker had stopped reading as a colour
    /// and read as ordinary dark text. It had cleared every number and lost the
    /// thing the numbers were standing in for. Two inks that differ by HUE at
    /// comparable brightness are obviously distinguishable and score barely
    /// 1.14:1, which that assertion would have rejected.
    ///
    /// CIE76 ΔE sees all three axes. >= 20 is well clear of "the same colour",
    /// and it is a RE-POINTING and not a loosening: the neutrals measure 28.8
    /// light and 31.9 dark, but the aqua pair this replaced already scored 24.5
    /// and 27.8, so the new floor would have passed the old palette too. Every
    /// other assertion here is unchanged.
    ///
    /// The direction is asserted per theme because it INVERTS. "More
    /// prominent" on the light ground means darker and on the dark ground
    /// means lighter, so the portable statement is the one made against the
    /// ground itself: whatever the hex, `aqua_elsewhere` must out-contrast
    /// `aqua` there. That is why the field is named for its role and not for
    /// its shade — `aqua_dark` would be a lie in one of the two themes.
    #[test]
    fn the_two_open_marker_shades_never_collapse() {
        let dark_bg = Color::Rgb(0x28, 0x28, 0x28);
        let light_bg = Color::Rgb(0xfb, 0xf1, 0xc7);

        for (name, p, bg) in [
            ("dark", Palette::dark(), dark_bg),
            ("light", Palette::light(), light_bg),
        ] {
            assert_ne!(p.aqua, p.aqua_elsewhere, "{name}: the two marker inks collapsed");
            // Far enough apart to read as two colours side by side, not as a
            // rounding error. Measured: ΔE 28.8 light, 31.9 dark.
            let apart = delta_e(p.aqua, p.aqua_elsewhere);
            assert!(apart >= 20.0, "{name}: the inks are only ΔE {apart:.1} apart, needs >= 20");
            // DELIBERATELY EXEMPT from the 4.0 text floor the other accents
            // obey. `aqua_elsewhere` is gruvbox bg4: a one-column solid block,
            // not text — WCAG's applicable rule is SC 1.4.11 (non-text, 3.0:1),
            // and even that is a comfort bound rather than a correctness one
            // here, because the marker's meaning is ALSO carried by the tab
            // digit printed immediately beside it. Nothing is lost if the block
            // is faint; the operator reads the digit. Two darker candidates
            // that DID clear 4.0 (fg3 #665c54, fg2 #d5c4a1) were rejected in
            // use: at that depth the marker reads as body text rather than as a
            // cue, which is the defect this shade exists to fix.
            //
            // The bound below only stops it vanishing into the ground entirely.
            let faint = ratio(p.aqua_elsewhere, bg);
            assert!(
                faint >= 2.0,
                "{name}: aqua_elsewhere at {faint:.2}:1 would disappear into the ground"
            );
            let faint_band = ratio(p.aqua_elsewhere, p.sel_bg);
            assert!(
                faint_band >= 2.0,
                "{name}: aqua_elsewhere at {faint_band:.2}:1 would disappear into the band"
            );
        }
        // bg4 is LIGHTER than `aqua` on the light theme and DARKER on the dark
        // one — the opposite of the fg3/fg2 pair it replaces. Pinned so a future
        // edit cannot quietly walk it back toward the text ramp.
        assert!(lum(Palette::light().aqua_elsewhere) > lum(Palette::light().aqua));
        assert!(lum(Palette::dark().aqua_elsewhere) < lum(Palette::dark().aqua));
    }

    /// The price of a NEUTRAL marker on the light ground, bounded and pinned.
    /// `p.aqua_elsewhere` there is `#665c54`, byte-identical to `p.dim`: the
    /// gruvbox neutrals ARE that theme's text ramp, so a neutral clearing the
    /// 4.0:1 band floor has nowhere to stand that is not already a text tier
    /// (fg4, the one step lighter, is 3.55:1 on the band). The consequence is
    /// real and is not a rounding error — an UNSELECTED other-tab Completed row
    /// paints marker, badge, name and age in one RGB, and only the glyph shapes
    /// separate them. This test does not pretend otherwise. It pins the two
    /// bounds that make the trade survivable, neither of which any other test
    /// covers.
    ///
    /// ONE: the collision must be EXACT or CLEAR, never in between. An ink a
    /// few ΔE off a text tier is the one outcome nobody would choose — it reads
    /// as a rendering fault rather than as either a colour or a tier. gruvbox's
    /// own neighbouring text tiers sit ~8 ΔE apart (light fg->gray 7.9,
    /// gray->dim 8.6), so one ramp step is the natural floor: ΔE 0, or >= 7.5.
    ///
    /// TWO: the SELECTED row — the one actually being read — must keep the
    /// marker off every ink beside it, and does, but not by luck. It holds only
    /// because `session_line` forces the selected name to `p.fg` and promotes
    /// the selected age dim -> gray, two promotions made for contrast reasons of
    /// their own (`p.dim` is 3.16:1 on the dark band). Drop either and the light
    /// theme's selected row goes flat too, which is the failure this bounds; the
    /// palette-level assertions above would all still pass.
    ///
    /// Asserted on the rendered spans, not on the palette, because what matters
    /// is the ink that actually reaches the row.
    #[test]
    fn the_neutral_marker_never_half_matches_the_text_ramp() {
        // One gruvbox ramp step, the smallest gap the theme itself ever asks a
        // reader to see between two text tiers.
        const STEP: f64 = 7.5;

        for (name, p) in [("dark", Palette::dark()), ("light", Palette::light())] {
            for (field, c) in [("fg", p.fg), ("gray", p.gray), ("dim", p.dim)] {
                let d = delta_e(p.aqua_elsewhere, c);
                assert!(
                    d == 0.0 || d >= STEP,
                    "{name}: aqua_elsewhere is ΔE {d:.1} from {field} — neither the same \
                     ink nor a distinguishable one"
                );
            }
        }

        // Completed is the worst case on purpose: its name tier IS `p.dim`, so
        // on the light theme every ink in the unselected row is the marker's.
        let mut app = app_with(vec![sess(1, Kind::Background, Status::Idle, Some(State::Done))]);
        let sid = app.sessions[0].session_id.clone();
        app.own_pane = PaneId::parse("%1");
        app.own_window = crate::tmux::WindowId::parse("@2");
        app.map.panes.insert("%7".into(), PaneEntry {
            session_id: sid,
            short_id: "00000001".into(),
            name: "n".into(),
            opened_at: 0,
        });
        // Open in tab 5, i.e. NOT the tab being drawn: the elsewhere ink.
        app.panes = vec![pane_in("%1", 1, 0, 2), pane_in("%7", 2, 34, 5)];
        app.rebuild_open();

        for (name, p) in [("dark", Palette::dark()), ("light", Palette::light())] {
            for selected in [false, true] {
                let l = session_line(&app, &app.sessions[0], selected, 34, &p);
                assert_eq!(line_cols(&l)[1], '5', "the fixture must be an other-tab row");
                let ink = l.spans[0].style.fg.expect("the marker is inked");
                assert_eq!(ink, p.aqua_elsewhere, "{name}: the fixture must take the neutral");

                // Span 4 is the name (marker, badge, glyph, space, name); the
                // age is the last span carrying text. Both are checked rather
                // than trusted, so a layout change fails here loudly instead of
                // silently measuring a pad span.
                assert!(
                    l.spans[4].content.starts_with("session"),
                    "span 4 is no longer the name: {:?}",
                    l.spans[4].content
                );
                let name_ink = l.spans[4].style.fg.expect("the name is inked");
                let age_ink = l
                    .spans
                    .iter()
                    .rev()
                    .find(|s| !s.content.trim().is_empty())
                    .and_then(|s| s.style.fg)
                    .expect("the age is inked at 34 columns");

                for (what, c) in [("name", name_ink), ("age", age_ink)] {
                    let d = delta_e(ink, c);
                    if selected {
                        assert!(
                            d >= STEP,
                            "{name}: the SELECTED row's {what} is only ΔE {d:.1} from the \
                             marker — the promotion that keeps them apart is gone"
                        );
                    } else {
                        assert!(
                            d == 0.0 || d >= STEP,
                            "{name}: the unselected row's {what} is ΔE {d:.1} from the marker"
                        );
                    }
                }
                // The reason the selected row can promote at all: `p.gray` is
                // safe on the band where `p.dim` is not. Asserted where the
                // promotion is relied on, not only in the palette sweep.
                if selected {
                    let r = ratio(age_ink, p.sel_bg);
                    assert!(r >= 4.0, "{name}: the promoted age is {r:.2}:1 on sel_bg");
                }
            }
        }
    }

    /// The gutter's two columns answer the same question, so they must never
    /// answer it differently: a blank badge takes today's `aqua` and a digit
    /// takes `aqua_elsewhere`, in column 1 and column 2 alike. Asserted on the
    /// rendered spans, selected and unselected, in both themes — the marker is
    /// painted on the band when its row is selected, which is the case the
    /// shade is easiest to lose.
    #[test]
    fn the_marker_shade_and_the_tab_badge_always_agree() {
        let mut app = app_with(vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))]);
        app.sessions[0].name = "alpha/opt".into();
        let sid = app.sessions[0].session_id.clone();
        app.own_pane = PaneId::parse("%1");
        app.own_window = crate::tmux::WindowId::parse("@2");
        app.map.panes.insert("%7".into(), PaneEntry {
            session_id: sid,
            short_id: "00000001".into(),
            name: "n".into(),
            opened_at: 0,
        });

        for p in [Palette::light(), Palette::dark()] {
            for selected in [false, true] {
                // Open in MY tab (window @2): today's aqua, blank column 2.
                app.panes = vec![pane_in("%1", 1, 0, 2), pane_in("%7", 2, 34, 2)];
                app.rebuild_open();
                let l = session_line(&app, &app.sessions[0], selected, 34, &p);
                let cols = line_cols(&l);
                assert_eq!(cols[0], '▌');
                assert_eq!(cols[1], ' ', "the current tab shows no digit");
                assert_eq!(l.spans[0].style.fg, Some(p.aqua), "selected={selected}");

                // Open in tab 5: the neutral shade, and the digit that
                // names where. Both gutter columns carry the same ink.
                app.panes = vec![pane_in("%1", 1, 0, 2), pane_in("%7", 2, 34, 5)];
                app.rebuild_open();
                let l = session_line(&app, &app.sessions[0], selected, 34, &p);
                let cols = line_cols(&l);
                assert_eq!(cols[0], '▌');
                assert_eq!(cols[1], '5', "another tab shows its digit");
                assert_eq!(l.spans[0].style.fg, Some(p.aqua_elsewhere), "selected={selected}");
                assert_eq!(
                    l.spans[1].style.fg, l.spans[0].style.fg,
                    "`▌5` must read as one two-cell token"
                );

                // The shade is the ONLY thing that changed: the grid is
                // untouched from column 2 rightwards.
                app.panes = vec![pane_in("%1", 1, 0, 2), pane_in("%7", 2, 34, 2)];
                app.rebuild_open();
                let here = line_cols(&session_line(&app, &app.sessions[0], selected, 34, &p));
                assert_eq!(here[2..], cols[2..], "the shade moved the rest of the row");
            }
        }
    }

    /// The gutter with NO `own_window`, pinned deliberately rather than left to
    /// drift. The shade reads off `tab_badge_for`, so it inherits whatever that
    /// helper answers when the sidebar cannot place itself — two states, which
    /// behave differently, and both are asserted because only the pairing of
    /// them says the shade is doing the right thing.
    ///
    /// With no pane inventory the row is open but placed nowhere:
    /// `rebuild_open` stamps neither window nor index, `tab_badge_for` has no
    /// index to return, and the row renders exactly what it rendered before the
    /// second shade existed — blank column 2, today's `aqua`. This is the shape
    /// every no-panic fixture in this module has, and the shape of the first
    /// tick before any enumeration has landed. The neutral stays out of a
    /// question the process cannot answer. (Degraded mode proper is quieter
    /// still: `refresh_panes` returns early, so `adopt_own_state` never runs and
    /// there is no map to make the row open at all — no marker, no digit.)
    ///
    /// With an inventory but still no `own_window`, every open row shows its
    /// digit, and the neutral marker goes with it. That is not the marker
    /// over-reaching: `App::resolve_identity` accepts `$TMUX_PANE` only if it
    /// appears in `list_panes_in_session`, which lists every pane of every
    /// window of the managed session, so a sidebar that fails to place itself is
    /// one drawn outside that listing — `ccmux sidebar --session X` run by hand
    /// from another tmux session — and every pane in the inventory really is a
    /// tab away. What is asserted here is the weaker, unconditional promise:
    /// whatever `tab_badge_for` answers, both gutter cells answer it together. A
    /// shade that went neutral while the digit stayed would be exactly the drift
    /// the single `ink` exists to make impossible.
    #[test]
    fn the_degraded_gutter_never_says_two_things_at_once() {
        let mut app = app_with(vec![sess(1, Kind::Background, Status::Busy, Some(State::Working))]);
        let sid = app.sessions[0].session_id.clone();
        app.map.panes.insert("%7".into(), PaneEntry {
            session_id: sid,
            short_id: "00000001".into(),
            name: "n".into(),
            opened_at: 0,
        });
        // The state under test: this process cannot say which tab it is in.
        app.own_pane = None;
        app.own_window = None;

        for p in [Palette::light(), Palette::dark()] {
            for selected in [false, true] {
                // Degraded: no inventory. Open, but placed nowhere.
                app.panes = Vec::new();
                app.rebuild_open();
                let l = session_line(&app, &app.sessions[0], selected, 34, &p);
                assert_eq!(line_cols(&l)[0], '▌', "the row is still open");
                assert_eq!(line_cols(&l)[1], ' ', "no inventory names no tab");
                assert_eq!(
                    l.spans[0].style.fg,
                    Some(p.aqua),
                    "an unplaceable pane must not take the elsewhere ink (selected={selected})"
                );

                // Inventory, still no own window: the documented digit, and
                // the shade that goes with it.
                app.panes = vec![pane_in("%7", 2, 34, 6)];
                app.rebuild_open();
                let l = session_line(&app, &app.sessions[0], selected, 34, &p);
                assert_eq!(line_cols(&l)[1], '6', "the documented degraded digit");
                assert_eq!(l.spans[0].style.fg, Some(p.aqua_elsewhere));
                assert_eq!(
                    l.spans[1].style.fg,
                    l.spans[0].style.fg,
                    "the two gutter cells must never disagree, degraded or not"
                );
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

