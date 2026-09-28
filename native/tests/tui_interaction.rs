use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use remuda_core::{SessionSummary, Size};
use remuda_native::tui::{pane_size, render, Action, Ui};
use std::time::Duration;

fn ui_with_mouse_tracking(mouse_tracking: bool) -> Ui {
    Ui::new(
        vec![SessionSummary {
            id: String::new(),
            name: "agent".into(),
            alive: true,
            idle: Duration::ZERO,
            output_idle: Some(Duration::ZERO),
            size: Size::new(80, 24),
            attached: false,
            human_idle: None,
            mouse_tracking,
        }],
        "sh",
        None,
    )
}

fn ui() -> Ui {
    ui_with_mouse_tracking(false)
}

fn key(ch: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
}

#[test]
fn visual_yank_is_local_but_input_mode_keeps_vim_keys_for_the_child() {
    let mut ui = ui();
    assert_eq!(ui.on_key(key('v')), Action::Nothing);
    for event in [
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 19,
            row: 4,
            modifiers: KeyModifiers::NONE,
        },
        MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 20,
            row: 5,
            modifiers: KeyModifiers::NONE,
        },
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 20,
            row: 5,
            modifiers: KeyModifiers::NONE,
        },
    ] {
        assert_eq!(ui.on_mouse(event, 80, 24), Action::Nothing);
    }
    assert_eq!(ui.on_key(key('y')), Action::CopySelection("agent".into()));
    assert_eq!(ui.on_key(key('p')), Action::Paste);

    assert_eq!(
        ui.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Focus("agent".into())
    );
    for ch in ['v', 'y', 'p'] {
        assert_eq!(ui.on_key(key(ch)), Action::Type(vec![ch as u8]));
    }
}

#[test]
fn l_hides_the_list_and_gives_the_preview_the_full_terminal_width() {
    let mut ui = ui();
    assert_eq!(ui.on_key(key('l')), Action::Nothing);
    assert_eq!(pane_size(&ui, 120, 30), Size::new(120, 29));
    let hidden = render(&ui, "child screen", "default", 120, 30);
    assert!(hidden.contains("remuda · default 🏇"));
    assert!(!hidden.contains('│'));

    assert_eq!(ui.on_key(key('l')), Action::Nothing);
    let shown = render(&ui, "child screen", "default", 120, 30);
    assert!(shown.contains("remuda · default 🏇"));
    assert!(!shown.contains("remuda · default│"));
}

#[test]
fn the_server_brand_and_horse_live_in_the_footer_not_the_pane_header() {
    let ui = ui();
    let frame = render(&ui, "child screen", "default", 80, 5);
    assert!(!frame.contains("remuda · default│"));
    let first_line = frame
        .split("\x1b[1;1H")
        .nth(1)
        .and_then(|row| row.split("\x1b[2;1H").next())
        .expect("first body row");
    assert!(
        first_line.contains("agent"),
        "first row has the session: {first_line:?}"
    );
    assert!(
        first_line.contains("child screen"),
        "preview content also starts on the first row: {first_line:?}"
    );
    let footer = frame.split("\x1b[5;1H").nth(1).expect("footer row");
    assert!(footer.ends_with("remuda · default 🏇"));
}

#[test]
fn wheel_reaches_a_focused_agent_but_scrolls_remuda_history_in_browse_mode() {
    let wheel = MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 19,
        row: 4,
        modifiers: KeyModifiers::NONE,
    };
    let mut ui = ui_with_mouse_tracking(false);
    assert_eq!(ui.on_mouse(wheel, 80, 24), Action::Scroll(-3));

    assert_eq!(
        ui.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Focus("agent".into())
    );
    assert_eq!(ui.on_mouse(wheel, 80, 24), Action::Nothing);

    let mut tracking = ui_with_mouse_tracking(true);
    assert_eq!(
        tracking.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Focus("agent".into())
    );
    assert_eq!(
        tracking.on_mouse(wheel, 80, 24),
        Action::Type(remuda_core::keys::mouse("wheel-down", 3, 5).unwrap())
    );
}
