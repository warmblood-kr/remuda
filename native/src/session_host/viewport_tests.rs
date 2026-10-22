use super::*;
use crate::session_host::terminal::Terminal;

fn feed(terminal: &mut Terminal, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let count = terminal.process(bytes).unwrap();
        bytes = &bytes[count..];
    }
}

#[test]
fn typed_viewport_freezes_styles_wide_wrap_cursor_modes_and_geometry() {
    let mut terminal = Terminal::new(3, 8, 10).unwrap();
    feed(
        &mut terminal,
        "\x1b[31;1m界e\u{301}\x1b[0mabcdef\r\nnext\x1b[?2004h\x1b[?1003h\x1b[?1006h\x1b[?1h\x1b="
            .as_bytes(),
    );
    let captured = terminal.viewport(0);
    captured.validate().unwrap();
    let bytes = captured.encode().unwrap();
    let decoded: Viewport = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.cells, captured.cells);
    assert_eq!(decoded.wrapped, captured.wrapped);
    assert_eq!(decoded.cursor, captured.cursor);
    assert_eq!((decoded.rows, decoded.cols), (3, 8));
    assert!(decoded.cells[0][0].wide);
    assert_eq!(decoded.cells[0][1].text, "");
    assert_eq!(decoded.cells[0][0].fg, remuda_core::agent::Color::Idx(1));
    assert!(decoded.cells[0][0].bold);
    assert!(decoded.wrapped[0]);
    assert!(decoded.modes.application_cursor && decoded.modes.application_keypad);
    assert!(decoded.modes.mouse.bracketed_paste);
    assert_eq!(
        decoded.modes.mouse.mode,
        remuda_core::agent::MouseMode::AnyMotion
    );
    assert_eq!(
        decoded.modes.mouse.encoding,
        remuda_core::agent::MouseEncoding::Sgr
    );
    feed(&mut terminal, b"\x1b[?1049halternate\x1b[?25l");
    let alternate = terminal.viewport(0);
    assert!(alternate.modes.alternate_screen);
    assert!(!alternate.cursor.visible);
    assert!(alternate.generation > captured.generation);
    assert!(!captured.modes.alternate_screen);
    assert_eq!(captured.cells, decoded.cells);
}

#[test]
fn history_offsets_are_relative_and_effective_clamp_does_not_change_live_view() {
    let mut terminal = Terminal::new(3, 8, 4).unwrap();
    let empty_history = terminal.viewport(usize::MAX);
    assert_eq!(empty_history.offset, 0);
    assert!(empty_history.cursor.visible);
    feed(
        &mut terminal,
        b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\n",
    );
    let before = terminal.viewport(0);
    let history = terminal.viewport(usize::MAX);
    assert_eq!(history.history_len, 4);
    assert_eq!(history.offset, 4);
    assert!(!history.cursor.visible);
    let after = terminal.viewport(0);
    assert_eq!(before.cells, after.cells);
    assert_eq!(before.generation, after.generation);
    assert_eq!(history.generation, before.generation);
    feed(&mut terminal, b"seven\r\n");
    let moved = terminal.viewport(4);
    assert_eq!(moved.offset, history.offset);
    assert_ne!(moved.cells, history.cells);
    assert!(moved.history_total >= history.history_total);
}

#[test]
fn bounds_refuse_bad_shape_cursor_and_cell_text_before_encoding() {
    let mut terminal = Terminal::new(3, 8, 4).unwrap();
    let mut view = terminal.viewport(0);
    view.cells[0][0].text = "x".repeat(23);
    assert!(view.encode().is_err());
    view.cells[0][0].text = "x".into();
    view.cursor.col = 8;
    assert!(view.validate().is_err());
    view.cursor.col = 0;
    view.cells.pop();
    assert!(view.validate().is_err());
}

#[test]
fn encoder_enforces_escaped_byte_budget_during_writes_without_growing_capacity() {
    let mut writer = Bounded {
        bytes: Vec::with_capacity(16),
        limit: 16,
    };
    writer.write_all(b"123456789012345").unwrap();
    let capacity = writer.bytes.capacity();
    assert!(writer.write_all(b"too long").is_err());
    assert_eq!(writer.bytes, b"123456789012345");
    assert_eq!(writer.bytes.capacity(), capacity);
    assert!(encode(&"\x01".repeat(22), 64).is_err());
    let bytes = encode(&"\x01".repeat(22), 256).unwrap();
    assert!(bytes.len() > 22);
    assert!(bytes.capacity() <= 256);
}

#[test]
fn largest_valid_grid_with_maximally_escaped_cell_text_fits_the_total_cap() {
    let mut terminal = Terminal::new(256, 256, 1000).unwrap();
    let mut view = terminal.viewport(0);
    for row in &mut view.cells {
        for cell in row {
            cell.text = "\x01".repeat(22);
            cell.fg = remuda_core::agent::Color::Rgb(255, 255, 255);
            cell.bg = cell.fg;
        }
    }
    let encoded = view.encode().unwrap();
    assert!(encoded.len() <= crate::session_host::api::MAX_SNAPSHOT_BYTES);
    assert!(encoded.capacity() <= crate::session_host::api::MAX_SNAPSHOT_BYTES);
    assert!(encoded.len() > 16 * 1024 * 1024);
}

#[test]
fn fixed_combining_text_retention_is_an_explicit_rendering_profile() {
    let mut terminal = Terminal::new(24, 80, 1000).unwrap();
    feed(
        &mut terminal,
        format!("a{}", "\u{301}".repeat(1024)).as_bytes(),
    );
    let view = terminal.viewport(0);
    assert_eq!(view.rendering_profile, RenderingProfile::Vt100Fixed22);
    assert!(view.cells[0][0].text.len() <= 22);
    assert!(view.cells[0][0].text.chars().count() < 1025);
    assert_eq!(view.cells[0][1].text, " ");
    view.validate().unwrap();
}

#[test]
fn a_plain_viewport_stays_within_the_history_older_daemons_accept() {
    let mut terminal = Terminal::new(3, 8, crate::session_host::terminal::HISTORY_ROWS).unwrap();
    let lines: String = (0..2500).map(|n| format!("{n}\r\n")).collect();
    feed(&mut terminal, lines.as_bytes());
    let deep = terminal.viewport(2000);
    assert!(deep.history_len >= 2400, "{}", deep.history_len);
    assert_eq!(deep.offset, 2000);
    deep.validate().unwrap();
    let legacy = terminal.viewport(2000).within_legacy_history();
    assert_eq!(legacy.history_len, 1000);
    assert!(legacy.offset <= 1000);
    legacy.validate().unwrap();
}

#[test]
fn the_compact_form_round_trips_every_cell_and_is_a_fraction_of_the_size() {
    let mut terminal = Terminal::new(24, 80, 10).unwrap();
    feed(
        &mut terminal,
        "\x1b[31;1m界e\u{301}\x1b[0mabcdef\x1b[44m blue \x1b[0m\r\n\x1b[7minv\x1b[0m 한글 text\x1b[?2004h"
            .as_bytes(),
    );
    let lines: String = (0..30)
        .map(|n| format!("line {n} {}\r\n", "x".repeat(60)))
        .collect();
    feed(&mut terminal, lines.as_bytes());
    let view = terminal.viewport(5);
    let full = view.encode().unwrap();
    let compact = view.encode_compact().unwrap();
    assert!(
        compact.len() * 10 < full.len(),
        "{} vs {}",
        compact.len(),
        full.len()
    );
    let back = Viewport::decode(&compact).unwrap();
    back.validate().unwrap();
    assert_eq!(back.cells, view.cells);
    assert_eq!(back.wrapped, view.wrapped);
    assert_eq!(back.cursor, view.cursor);
    assert_eq!(back.modes, view.modes);
    assert_eq!(
        (
            back.generation,
            back.offset,
            back.history_len,
            back.history_total
        ),
        (
            view.generation,
            view.offset,
            view.history_len,
            view.history_total
        )
    );
    // The full form still decodes, for hosts without compact-view-v1.
    assert_eq!(Viewport::decode(&full).unwrap().cells, view.cells);
}

#[test]
fn a_compact_row_longer_than_the_screen_is_refused_before_expanding() {
    let view = terminal_view(Terminal::new(3, 8, 10).unwrap());
    let mut value: serde_json::Value =
        serde_json::from_slice(&view.encode_compact().unwrap()).unwrap();
    value["runs"][0] = serde_json::json!([{ "c": vec!["x"; 9] }]);
    assert!(Viewport::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    value["runs"][0] = serde_json::json!([{ "c": vec!["x"; 8], "blink": true }]);
    assert!(Viewport::decode(&serde_json::to_vec(&value).unwrap()).is_err());
}

fn terminal_view(mut terminal: Terminal) -> Viewport {
    terminal.viewport(0)
}
