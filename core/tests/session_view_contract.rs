use remuda_core::session_view::{SessionView, ViewEntry};
use remuda_core::{SessionSummary, Size};
use std::time::Duration;

fn session(name: &str, instance: &str) -> SessionSummary {
    SessionSummary {
        id: format!("id-{name}"),
        name: name.into(),
        instance_id: Some(instance.into()),
        output_version: None,
        alive: true,
        idle: Duration::ZERO,
        output_idle: None,
        size: Size::new(80, 24),
        attached: false,
        human_idle: None,
        mouse_tracking: false,
        unavailable: false,
        program_status: None,
        program_status_age_ms: None,
        remote_input_disabled: false,
    }
}
fn entry(name: &str, instance: &str, line: &str) -> ViewEntry {
    ViewEntry {
        name: name.into(),
        instance_id: instance.into(),
        lines: vec![line.into()],
    }
}
#[test]
fn accepts_complete_reordered_snapshot_and_lua_styles() {
    let view = SessionView::new(
        vec![session("lead", "a"), session("child", "b")],
        1,
        vec![
            entry("child", "b", "  \x1b[1mchild\x1b[0m · codex · 모델"),
            entry("lead", "a", "lead"),
        ],
    );
    let checked = view.validate_for_display(None, 80).unwrap();
    assert_eq!(checked.sessions()[0].name, "child");
    assert_eq!(checked.sessions()[0].instance_id.as_deref(), Some("b"));
}
#[test]
fn rejects_stale_duplicate_unknown_or_partial_rows() {
    let snapshot = vec![session("lead", "a"), session("child", "b")];
    for rows in [
        vec![entry("lead", "wrong", "x"), entry("child", "b", "y")],
        vec![entry("lead", "a", "x"), entry("lead", "a", "y")],
        vec![entry("lead", "a", "x")],
        vec![entry("lead", "a", "x"), entry("other", "b", "y")],
    ] {
        assert!(SessionView::new(snapshot.clone(), 1, rows)
            .validate()
            .is_err());
    }
}
#[test]
fn rejects_terminal_commands_and_invalid_layout() {
    for line in ["\x1b]52;c;bad\x07", "\x1b[2J", "x\rhidden", "\u{009b}2J"] {
        let view = SessionView::new(vec![session("one", "a")], 1, vec![entry("one", "a", line)]);
        assert!(view.validate().is_err(), "{line:?}");
    }
    assert!(SessionView::new(
        vec![session("one", "a")],
        9000,
        vec![entry("one", "a", "x")]
    )
    .validate()
    .is_err());
}
#[test]
fn rejects_unavailable_or_unbound_snapshot_instances() {
    let mut row = session("one", "a");
    row.unavailable = true;
    assert!(SessionView::new(vec![row], 1, vec![entry("one", "a", "x")])
        .validate()
        .is_err());
    let mut row = session("one", "a");
    row.instance_id = None;
    assert!(SessionView::new(vec![row], 1, vec![entry("one", "a", "x")])
        .validate()
        .is_err());
}
#[test]
fn rejects_misleading_names_invisible_text_and_style_leaks() {
    for line in [
        "  not-one ●",
        "\x1b[8mone\x1b[0m ●",
        "\x1b[1mone ●",
        "\x1b[30;40mone\x1b[0m ●",
        "one\u{202e} ●",
    ] {
        let view = SessionView::new(vec![session("one", "a")], 1, vec![entry("one", "a", line)]);
        assert!(view.validate().is_err(), "{line:?}");
    }
}
#[test]
fn selection_requires_exact_instance_and_ignores_remote_highlight() {
    let view = SessionView::new(
        vec![session("one", "a")],
        1,
        vec![entry("one", "a", "\x1b[1;7;36mone\x1b[0m")],
    );
    let neutral = view.validate_for_display(None, 80).unwrap();
    assert!(!neutral.entries()[0].lines[0].contains("7;36"));
    assert!(view.validate_for_display(Some(("one", "old")), 80).is_err());
    let selected = view.validate_for_display(Some(("one", "a")), 80).unwrap();
    assert!(selected.entries()[0].lines[0].contains("\x1b[1;7;36mone\x1b[0m"));
}

#[test]
fn zero_width_name_is_not_an_actionable_identity() {
    let view = SessionView::new(
        vec![session("ab\u{200b}c", "i")],
        1,
        vec![entry("ab\u{200b}c", "i", "ab\u{200b}c ●")],
    );
    assert!(view.validate_for_display(None, 80).is_err());
}

#[test]
fn peer_resets_cannot_cancel_local_selection_on_the_name() {
    for line in ["\x1b[0mone", "o\x1b[0mne", "  \x1b[0mo\x1b[0mne"] {
        let view = SessionView::new(vec![session("one", "a")], 1, vec![entry("one", "a", line)]);
        let checked = view.validate_for_display(Some(("one", "a")), 80).unwrap();
        assert!(
            checked.entries()[0].lines[0].contains("\x1b[1;7;36mone\x1b[0m"),
            "{line:?}: {:?}",
            checked.entries()[0].lines[0]
        );
    }
}

#[test]
fn accepts_the_status_cell_before_the_name_and_selects_the_name_only() {
    let view = SessionView::new(
        vec![session("busy", "a"), session("resting", "b")],
        1,
        vec![
            entry("busy", "a", "  \x1b[1m🏇 \x1b[0mbusy"),
            entry("resting", "b", "  \x1b[32m○\x1b[0m  resting"),
        ],
    );
    let checked = view.validate_for_display(Some(("busy", "a")), 80).unwrap();
    assert!(
        checked.entries()[0].lines[0].contains("\x1b[1;7;36mbusy\x1b[0m"),
        "{:?}",
        checked.entries()[0].lines[0]
    );
    assert!(
        !checked.entries()[0].lines[0].contains("1;7;36m🏇"),
        "the selection must not cover the status cell: {:?}",
        checked.entries()[0].lines[0]
    );
    assert!(checked.entries()[1].lines[0].contains("○"));
}

#[test]
fn fit_keeps_the_status_cell_and_cuts_the_name() {
    use remuda_core::session_view::{fit_session_row, fitted_row_shows_name};
    let row = "\x1b[32m●\x1b[0m  \x1b[1ma-long-session-name\x1b[0m";
    let fitted = fit_session_row(row, 12);
    assert!(fitted.starts_with("\x1b[32m●\x1b[0m  "), "{fitted:?}");
    assert!(fitted.contains('→'), "{fitted:?}");
    assert_eq!(remuda_core::session_view::fit(&fitted, 12), fitted);
    assert!(!fitted_row_shows_name(&fitted, "a-long-session-name"));
    let whole = fit_session_row(row, 22);
    assert!(fitted_row_shows_name(&whole, "a-long-session-name"));
}

#[test]
fn legacy_peer_rows_stay_accepted_while_the_cluster_upgrades() {
    let view = SessionView::new(
        vec![session("one", "a"), session("two", "b")],
        1,
        vec![
            entry("one", "a", "\x1b[1mone\x1b[0m \x1b[32m●\x1b[0m"),
            entry("two", "b", "\x1b[1mtwo\x1b[0m \x1b[32m●\x1b[0m 🏇"),
        ],
    );
    let checked = view.validate_for_display(Some(("one", "a")), 80).unwrap();
    assert!(checked.entries()[0].lines[0].contains("\x1b[1;7;36mone\x1b[0m"));
    assert!(checked.entries()[1].lines[0].contains("🏇"));
}

#[test]
fn nested_colored_status_rows_keep_their_cell_when_selected_or_cut() {
    use remuda_core::session_view::{fit_session_row, fitted_row_shows_name};
    // The exact shapes the renderer now emits at depth: colored status cell,
    // separator, styled name. A selection must cover the name and nothing else.
    let suite = [
        ("  \x1b[32m○\x1b[0m  \x1b[1midle-child\x1b[0m", "idle-child"),
        (
            "    \x1b[32m●\x1b[0m  \x1b[1mlive-grandchild\x1b[0m",
            "live-grandchild",
        ),
    ];
    for (line, name) in suite {
        let view = SessionView::new(vec![session(name, "i")], 1, vec![entry(name, "i", line)]);
        let checked = view
            .validate_for_display(Some((name, "i")), 40)
            .expect("nested colored row with a selection");
        let painted = &checked.entries()[0].lines[0];
        assert!(
            painted.contains(&format!("\x1b[1;7;36m{name}\x1b[0m")),
            "{painted:?}"
        );
        assert!(
            painted.contains("\x1b[32m○\x1b[0m") || painted.contains("\x1b[32m●\x1b[0m"),
            "selection must not repaint the status cell: {painted:?}"
        );
        // Truncation: the cell survives, the long name is cut and refused as an
        // action target until the pane is wide enough.
        let narrow = fit_session_row(line, 12);
        assert!(narrow.contains('○') || narrow.contains('●'), "{narrow:?}");
        assert_eq!(
            remuda_core::session_view::fit(&narrow, 12),
            narrow,
            "{narrow:?}"
        );
        assert!(!fitted_row_shows_name(&narrow, name), "{narrow:?}");
        let wide = fit_session_row(line, line.len() as u16 + 30);
        assert!(fitted_row_shows_name(&wide, name), "{wide:?}");
    }
    assert!(SessionView::new(
        vec![session("한글", "b")],
        1,
        vec![entry("한글", "b", "  \x1b[32m○\x1b[0m  \x1b[1m한글\x1b[0m")],
    )
    .validate_for_display(Some(("한글", "b")), 8)
    .is_err());
}
