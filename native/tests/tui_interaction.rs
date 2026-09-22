use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use remuda_core::{SessionSummary, Size};
use remuda_native::tui::{pane_size, render, Action, Ui};
use std::time::Duration;

fn ui() -> Ui {
    Ui::new(
        vec![SessionSummary {
            name: "agent".into(),
            alive: true,
            idle: Duration::ZERO,
            size: Size::new(80, 24),
            attached: false,
        }],
        "sh",
        None,
    )
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
    assert!(!hidden.contains("remuda · default"));
    assert!(!hidden.contains('│'));

    assert_eq!(ui.on_key(key('l')), Action::Nothing);
    let shown = render(&ui, "child screen", "default", 120, 30);
    assert!(shown.contains("remuda · default"));
}

#[test]
fn wheel_reaches_a_focused_agent_but_scrolls_remuda_history_in_browse_mode() {
    let wheel = MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 19,
        row: 4,
        modifiers: KeyModifiers::NONE,
    };
    let mut ui = ui();
    assert_eq!(ui.on_mouse(wheel, 80, 24), Action::Scroll(-3));

    assert_eq!(
        ui.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Focus("agent".into())
    );
    assert_eq!(
        ui.on_mouse(wheel, 80, 24),
        Action::Type(remuda_core::keys::mouse("wheel-down", 3, 5).unwrap())
    );
}
