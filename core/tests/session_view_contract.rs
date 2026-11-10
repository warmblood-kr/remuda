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
