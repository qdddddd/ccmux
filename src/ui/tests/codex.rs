use super::*;

fn codex_row(runtime: crate::model::CodexStatus, status: Status, state: Option<State>) -> Session {
    let mut s = sess(17, Kind::Background, status, state);
    s.provider = Provider::Codex;
    s.codex = Some(crate::model::CodexMeta { runtime, updated_at: 10 });
    s.session_id = "01a00000-1111-7222-8333-000000000011".into();
    s.name = "Codex task name".into();
    s
}

fn codex_degraded(app: &mut App) {
    use crate::app::providers::{FailureCategory, Health};
    app.codex.settings.url = "ws://localhost:8965".into();
    app.diagnostics.codex.health = Health::Degraded(
        FailureCategory::Codex(crate::codex::CodexFailureKind::Connection),
        "connection refused, server unavailable".into(),
    );
}

#[test]
fn codex_states_keep_their_glyphs_in_the_provider_group() {
    use crate::model::CodexStatus;
    #[derive(Clone, Copy, Debug)]
    enum Ink { Gray, Purple, Orange, Yellow }
    let cases = [
        (CodexStatus::NotLoaded, Status::Idle, Some(State::Unloaded), "◇", Ink::Gray, "unloaded", Group::Codex),
        (CodexStatus::Idle, Status::Idle, None, "○", Ink::Gray, "idle", Group::Codex),
        (CodexStatus::SystemError, Status::Unknown("systemError".into()), None, "?", Ink::Purple, "systemError", Group::Codex),
        (CodexStatus::Unknown("future".into()), Status::Unknown("future".into()), None, "?", Ink::Purple, "unknown", Group::Codex),
        (CodexStatus::Active { flags: vec![] }, Status::Busy, Some(State::Working), "●", Ink::Orange, "working", Group::Codex),
        (CodexStatus::Active { flags: vec!["newFlag".into()] }, Status::Unknown("newFlag".into()), Some(State::Working), "?", Ink::Purple, "unknown", Group::Codex),
        (CodexStatus::Active { flags: vec!["newFlag".into(), "waitingOnApproval".into()] }, Status::Waiting, Some(State::Blocked), "▲", Ink::Yellow, "blocked", Group::Codex),
        (CodexStatus::Active { flags: vec!["waitingOnUserInput".into()] }, Status::Waiting, Some(State::Blocked), "▲", Ink::Yellow, "blocked", Group::Codex),
    ];
    for (runtime, status, state, glyph, ink, detail, group) in cases {
        let row = codex_row(runtime, status, state);
        let mut app = app_with(vec![row.clone()]);
        app.codex.settings.url = "ws://localhost".into();
        assert_eq!(row.group(), group);
        for dark in [false, true] {
            app.dark = dark;
            let p = Palette::for_app(&app);
            let color = match ink {
                Ink::Gray => p.gray,
                Ink::Purple => p.purple,
                Ink::Orange => p.orange,
                Ink::Yellow => p.yellow,
            };
            assert_eq!(status_glyph(&row, &p), (glyph, color));
            for &(w, _) in SIZES {
                let line = session_line(&app, &row, true, w as usize, &p);
                assert_eq!(line_w(&line), w as usize);
                if w >= 6 {
                    assert!(!line.spans.iter().any(|s| s.content == "> "));
                }
            }
            let rendered = rows_at(&app, 60, 24).join("\n");
            assert!(rendered.contains(&format!("00000011 codex {detail}")), "{rendered}");
            all_modes(&mut app);
        }
        assert!(!row.name.starts_with("> "));
        assert!(row.filter_haystack().contains(&row.session_id));
    }
}

fn codex_fleet() -> Vec<Session> {
    use crate::model::CodexStatus;
    vec![
        codex_row(CodexStatus::NotLoaded, Status::Idle, Some(State::Unloaded)),
        codex_row(CodexStatus::Idle, Status::Idle, None),
        codex_row(CodexStatus::SystemError, Status::Unknown("systemError".into()), None),
        codex_row(CodexStatus::Unknown("future".into()), Status::Unknown("future".into()), None),
        codex_row(CodexStatus::Active { flags: vec![] }, Status::Busy, Some(State::Working)),
        codex_row(CodexStatus::Active { flags: vec!["newFlag".into()] },
            Status::Unknown("newFlag".into()), Some(State::Working)),
        codex_row(CodexStatus::Active { flags: vec!["newFlag".into(), "waitingOnApproval".into()] },
            Status::Waiting, Some(State::Blocked)),
        codex_row(CodexStatus::Active { flags: vec!["waitingOnUserInput".into()] },
            Status::Waiting, Some(State::Blocked)),
    ].into_iter().enumerate().map(|(i, mut s)| {
        s.session_id = format!("01a00000-1111-7222-8333-{:012x}", i + 100);
        s.id = Some(format!("{:08x}", i + 100));
        s.name = format!("Codex task {i}");
        s
    }).collect()
}

#[test]
fn mixed_provider_render_matrix_covers_standing_errors_flashes_and_drift() {
    let mut sessions = many(20);
    sessions.extend(codex_fleet());
    for dark in [false, true] {
        let mut app = app_with(sessions.clone());
        app.dark = dark;
        app.codex.settings.url = "ws://localhost".into();
        all_modes(&mut app);
        codex_degraded(&mut app);
        all_modes(&mut app);
        app.poll_error = Some("Claude failure with a reason much wider than the sidebar".into());
        all_modes(&mut app);
        app.flash("unmodelled codex active flags [futureFlag] — update ccmux", MsgLevel::Warn);
        all_modes(&mut app);
        app.flash("agents refresh failed: long command failure; codex refresh failed: authentication failed", MsgLevel::Error);
        all_modes(&mut app);
    }
}

#[test]
fn standing_errors_are_one_line_and_never_carve_the_details_or_list() {
    for claude in [false, true] {
        for codex in [false, true] {
            if !claude && !codex { continue; }
            let mut app = app_with(many(5));
            if claude { app.poll_error = Some("very long Claude poll error that cannot fit in the footer".into()); }
            if codex { codex_degraded(&mut app); }
            let p = Palette::for_app(&app);
            for &(w, h) in SIZES {
                assert!(overflow_message(&app, w as usize, &p).is_none());
                let before = rows_at(&app, w, h);
                let mut clean = app_with(app.sessions.clone());
                clean.selected = app.selected;
                let after = rows_at(&clean, w, h);
                if h > 2 {
                    assert_eq!(&before[1..h as usize - 1], &after[1..h as usize - 1]);
                }
            }
            let footer = rows_at(&app, 34, 24).pop().unwrap();
            if codex { assert!(footer.starts_with("[codex!] ")); }
            else { assert!(footer.starts_with("agents: ")); }
        }
    }
}

#[test]
fn degraded_prefix_is_red_once_in_wrapped_flash_and_filter() {
    let mut app = app_with(many(3));
    codex_degraded(&mut app);
    app.flash("long operator message whose final words must remain visible", MsgLevel::Warn);
    let p = Palette::for_app(&app);
    let text = rows_at(&app, 34, 24).join("\n");
    assert_eq!(text.matches("[codex!]").count(), 1);
    assert!(text.split_whitespace().collect::<Vec<_>>().join(" ").contains("remain visible"), "{text}");
    let line = footer_spans(&app, "warning", p.yellow, 34, &p);
    assert_eq!(line.spans[0].style.fg, Some(p.red));
    assert_eq!(line.spans[1].style.fg, Some(p.yellow));
    app.mode = Mode::Filter;
    app.filter = "task".into();
    assert!(overflow_message(&app, 34, &p).is_none());
    assert!(rows_at(&app, 34, 24).pop().unwrap().starts_with("[codex!] /task"));
    for w in 0..10 {
        assert!(line_w(&footer_spans(&app, "warning", p.yellow, w, &p)) <= w);
    }
}

#[test]
fn codex_help_is_opt_in_and_matches_scroll_geometry() {
    let mut app = app_with(many(1));
    app.mode = Mode::Help;
    let plain = rows_at(&app, 80, 60).join("\n");
    assert!(!plain.contains("Codex"));
    app.codex.settings.url = "ws://localhost".into();
    app.help_lines = help_line_count(true);
    let enabled = rows_at(&app, 34, 60).join("\n");
    for (key, action) in CODEX_KEYS {
        assert!(display_width(key) <= 10 && display_width(action) <= 22, "{key}: {action}");
        let line = enabled.lines().find(|line| line.contains(action)).expect(action);
        assert!(!line.contains('…'), "{line}");
    }
    assert!(enabled.contains("not loaded in server"));
    assert!(enabled.contains("creates Claude session"));
    assert!(enabled.contains("records launch target"));
    assert!(enabled.contains("open Codex TUI"));
    assert!(enabled.contains("Codex panes skipped"));
    assert!(enabled.contains("change TUI, not map"));
    assert!(enabled.contains("parked: resume launch"));
    assert!(CODEX_KEYS.contains(&("C-x ×2", "archive ○/◇ thread")));
    assert!(!enabled.contains("pane actions unavailable"));
    assert_eq!(help_line_count(true), help_line_count(false) + CODEX_KEYS.len());
}


#[test]
fn both_providers_cwd_render_without_controls_at_every_size() {
    let cwd = "/safe/a\x1b]0;PWNED\x07b\x1b[2J\r\n\t\x7f\u{9d}PWNED\u{9c}";
    for provider in [Provider::Claude, Provider::Codex] {
        let mut row = codex_row(crate::model::CodexStatus::Idle, Status::Idle, None);
        row.provider = provider;
        if provider == Provider::Claude { row.codex = None; }
        row.cwd = cwd.into();
        let app = app_with(vec![row]);
        for &(w, h) in SIZES {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| draw(f, &app)).unwrap();
            for cell in term.backend().buffer().content() {
                assert!(!cell.symbol().chars().any(char::is_control), "{provider:?} {w}x{h}: {cell:?}");
            }
            let text = rows_at(&app, w, h).join("\n");
            assert!(!text.contains("PWNED"), "{provider:?} {w}x{h}: {text}");
        }
        assert!(rows_at(&app, 34, 24).join("\n").contains("/safe/ab"));
        assert_eq!(app.sessions[0].cwd, cwd, "render must not change the stored path");
        assert!(app.sessions[0].filter_haystack().contains(&cwd.to_lowercase()));
    }
}

#[test]
fn the_codex_group_reclaims_the_marker_columns_at_every_width() {
    let mut row = codex_row(crate::model::CodexStatus::Idle, Status::Idle, None);
    for name in ["abcdefghijklmnopqrstuvwxyz123456789", "> literal name", "汉字名字🙂长名字"] {
        row.name = name.into();
        let mut claude = row.clone();
        claude.provider = Provider::Claude;
        claude.codex = None;
        for dark in [false, true] {
            let mut app = app_with(vec![row.clone()]);
            app.dark = dark;
            let p = Palette::for_app(&app);
            for selected in [false, true] {
                for w in 0..=120 {
                    assert_eq!(session_line(&app, &row, selected, w, &p),
                        session_line(&app, &claude, selected, w, &p),
                        "{w} columns, name {name:?}");
                }
            }
        }
    }
}

#[test]
fn codex_group_counts_only_visible_rows_and_preserves_state_contrast() {
    for dark in [false, true] {
        let mut app = app_with(codex_fleet());
        app.dark = dark;
        let p = Palette::for_app(&app);
        assert_eq!(app.rows[0], Row::Header { group: Group::Codex, count: 8 });
        let drawn = rows_at(&app, 34, 24);
        assert_eq!(drawn[1].trim(), "── Codex                       8");
        for s in &app.sessions {
            let want = match s.state {
                Some(State::Blocked | State::Working) => p.fg,
                Some(State::Unloaded) => p.dim,
                _ => p.gray,
            };
            assert_eq!(name_tier(s, &p), want);
        }
        app.on_key(crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE));
        assert_eq!(app.rows[0], Row::Header { group: Group::Codex, count: 7 });
        let drawn = rows_at(&app, 34, 24);
        assert_eq!(drawn[1].trim(), "── Codex                       7");
        assert!(!drawn.join("\n").contains('◇'));
        assert!(drawn.join("\n").contains('▲'));
    }
}

#[test]
fn every_codex_state_is_visible_when_selected_across_the_render_matrix() {
    let mut sessions = many(7);
    sessions.extend(codex_fleet());
    for dark in [false, true] {
        for &(w, h) in SIZES {
            for idx in 7..sessions.len() {
                let mut app = app_with(sessions.clone());
                app.dark = dark;
                app.codex.settings.url = "ws://localhost".into();
                app.viewport = list_viewport_rows(h);
                app.selected_key = Some(app.sessions[idx].session_id.clone());
                app.reanchor_selection();
                let drawn = rows_at(&app, w, h);
                assert_eq!(app.sessions[idx].group(), Group::Codex);
                if w > 0 && h >= 3 {
                    let (glyph, _) = status_glyph(&app.sessions[idx], &Palette::for_app(&app));
                    assert_eq!(app.rows[app.selected], Row::Session { idx });
                    let y = 1 + app.selected - app.scroll;
                    assert!(y <= app.viewport as usize, "{w}x{h}: selection is outside the viewport");
                    let x = if w < 20 { 0 } else { 2 };
                    assert_eq!(drawn[y].chars().nth(x).unwrap().to_string(), glyph,
                        "{w}x{h}, {:?}: {drawn:?}", app.sessions[idx].state);
                }
                for line in drawn { assert_eq!(display_width(&line), w as usize); }
            }
        }
    }
}

#[test]
fn claude_only_list_matches_the_pre_codex_group_baseline() {
    let mut app = app_with(many(7));
    app.sessions.iter_mut().enumerate().for_each(|(i, s)| s.name = format!("Claude {i}"));
    // Captured before the fifth-group change, including spaces and row order.
    let expected = [
        " ── Blocked                     2 ",
        "▏ ▲ Claude 6                   1h ",
        "  ▲ Claude 4                   1h ",
        "                                  ",
        " ── Working                     2 ",
        "  ◐ Claude 1                   1h ",
        "  ● Claude 0                   1h ",
        "                                  ",
        " ── Idle                        1 ",
        "  ? Claude 3                   1h ",
        "                                  ",
        " ── Completed                   2 ",
        "  ■ Claude 5                   1h ",
        "  ✓ Claude 2                   1h ",
        "                                  ",
        "                                  ",
        "                                  ",
    ];
    for dark in [false, true] {
        app.dark = dark;
        for codex_enabled in [false, true] {
            app.codex.settings.url = if codex_enabled { "ws://localhost".into() } else { String::new() };
            let drawn = rows_at(&app, 34, 24);
            assert_eq!(&drawn[1..18], &expected);
            assert!(!app.rows.iter().any(|r| matches!(r, Row::Header { group: Group::Codex, .. })));
        }
    }
}

#[test]
fn the_footer_says_archive_only_while_a_codex_row_is_selected() {
    let claude = sess(1, Kind::Background, Status::Busy, Some(State::Working));
    let codex = codex_row(crate::model::CodexStatus::Idle, Status::Idle, None);
    let only = app_with(vec![claude.clone()]);
    let on_claude = app_with(vec![claude.clone(), codex.clone()]);
    let mut on_codex = app_with(vec![claude, codex]);
    on_codex.selected = on_codex.rows.iter().position(|row| matches!(row,
        Row::Session { idx } if on_codex.sessions[*idx].provider == Provider::Codex)).unwrap();
    assert_eq!(only.selected_session().unwrap().provider, Provider::Claude);
    assert_eq!(on_claude.selected_session().unwrap().provider, Provider::Claude);
    assert_eq!(on_codex.selected_session().unwrap().provider, Provider::Codex);

    let pairs = "⏎ open  o/s split  x close  t tab  d/u hide  C-x stop  n new  L logs  / filter  R restart";
    assert!(rows_at(&only, 120, 24)[23].starts_with(pairs));
    assert!(rows_at(&on_codex, 120, 24)[23].starts_with(&pairs.replace("C-x stop", "C-x archive")));
    for w in 0..=120u16 {
        let claude_footer = rows_at(&only, w, 24)[23].clone();
        assert_eq!(rows_at(&on_claude, w, 24)[23], claude_footer, "w={w}");
        let codex_footer = rows_at(&on_codex, w, 24)[23].clone();
        assert!(!codex_footer.contains("stop"), "w={w}: {codex_footer:?}");
        // Whole pairs only: the longer label drops out with everything after it.
        if !codex_footer.contains("C-x archive") {
            assert!(!codex_footer.contains("n new"), "w={w}: {codex_footer:?}");
            let before = claude_footer.split("  C-x stop").next().unwrap().trim_end();
            assert!(codex_footer.starts_with(before) || !claude_footer.contains("C-x"), "w={w}");
        }
    }
}
