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
fn codex_states_and_marker_preserve_existing_row_geometry() {
    use crate::model::CodexStatus;
    let cases = [
        (CodexStatus::NotLoaded, Status::Idle, Some(State::Unloaded), "◇", "unloaded", Group::Completed),
        (CodexStatus::Idle, Status::Idle, None, "○", "idle", Group::Idle),
        (CodexStatus::SystemError, Status::Unknown("systemError".into()), None, "?", "systemError", Group::Idle),
        (CodexStatus::Active { flags: vec![] }, Status::Busy, Some(State::Working), "●", "working", Group::Working),
        (CodexStatus::Active { flags: vec!["newFlag".into()] }, Status::Unknown("newFlag".into()), Some(State::Working), "?", "unknown", Group::Working),
        (CodexStatus::Active { flags: vec!["newFlag".into(), "waitingOnApproval".into()] }, Status::Waiting, Some(State::Blocked), "▲", "blocked", Group::Blocked),
    ];
    for (runtime, status, state, glyph, detail, group) in cases {
        let row = codex_row(runtime, status, state);
        let mut app = app_with(vec![row.clone()]);
        app.codex.settings.url = "ws://localhost".into();
        let p = Palette::for_app(&app);
        assert_eq!(status_glyph(&row, &p).0, glyph);
        assert_eq!(row.group(), group);
        for &(w, _) in SIZES {
            let line = session_line(&app, &row, true, w as usize, &p);
            assert_eq!(line_w(&line), w as usize);
            if w >= 6 {
                assert!(line.spans.iter().any(|s| s.content == "> " && s.style.fg == Some(p.gray)));
            }
        }
        let rendered = rows_at(&app, 60, 24).join("\n");
        assert!(rendered.contains(&format!("00000011 codex {detail}")), "{rendered}");
        assert!(!row.name.starts_with("> "));
        assert!(row.filter_haystack().contains(&row.session_id));
        all_modes(&mut app);
    }
}

#[test]
fn mixed_provider_render_matrix_covers_standing_errors_flashes_and_drift() {
    let mut sessions = many(20);
    sessions.push(codex_row(crate::model::CodexStatus::NotLoaded, Status::Idle, Some(State::Unloaded)));
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
    let enabled = rows_at(&app, 80, 60).join("\n");
    assert!(enabled.contains("not loaded in this server"));
    assert!(enabled.contains("creates a Claude session"));
    assert!(enabled.contains("records pane launch target"));
    assert_eq!(help_line_count(true), help_line_count(false) + CODEX_KEYS.len());
}
