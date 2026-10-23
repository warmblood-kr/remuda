//! `remuda attach` to a host-backed session (#713), read-only for now.
//!
//! A host keeps no raw output stream, only typed screens, so the viewer is
//! painted from snapshots: the first frame in full, then only rows that
//! changed, each frame wrapped in synchronized output. Keys are not
//! forwarded yet; the first key shows a one-line notice instead.
use crate::ipc::{self, Stream, TryClone};
use remuda_core::agent::{Cursor, ScreenSnapshot, StyledCell};
use remuda_core::protocol::Response;
use remuda_core::session_operations::{Lifecycle, SessionHandle};
use std::fmt::Write as _;
use std::io::{BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long one wait for new output lasts before liveness is checked again.
const WATCH: Duration = Duration::from_millis(250);
/// At most this many frames a second, however fast the child prints.
const MIN_FRAME_GAP: Duration = Duration::from_millis(33);

/// Serve one viewer until it detaches, the session ends, or a write fails.
/// A tracked attach is acknowledged with generation 0: a read-only view
/// takes nothing over, so it is never displaced.
pub(super) fn serve(
    stream: Stream,
    reader: BufReader<Stream>,
    handle: &SessionHandle,
    tracked: bool,
) -> std::io::Result<()> {
    let first = match handle.snapshot(0) {
        Ok(first) => first,
        Err(error) => return super::reply(&stream, &Response::error(error.to_string())),
    };
    let acknowledgement = if tracked {
        Response::AttachStarted { generation: 0 }
    } else {
        Response::Ok
    };
    super::reply(&stream, &acknowledgement)?;
    let mut out = stream.try_clone()?;
    let gone = AtomicBool::new(false);
    let typed = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let keys = scope.spawn(|| discard_keys(reader, &gone, &typed, &stop));
        let _ = paint(&mut out, handle, first, &gone, &typed);
        ipc::stop_reader(&stream, &stop, || keys.is_finished());
        let _ = keys.join();
    });
    Ok(())
}

/// The output side: repaint on every new generation until the viewer leaves
/// or the session stops running.
fn paint(
    out: &mut Stream,
    handle: &SessionHandle,
    first: remuda_core::agent::VersionedSnapshot,
    gone: &AtomicBool,
    typed: &AtomicBool,
) -> std::io::Result<()> {
    out.write_all(&frame(None, &first.snapshot))?;
    out.flush()?;
    let mut shown = first;
    let mut noticed = false;
    while !gone.load(Ordering::SeqCst) {
        if !noticed && typed.load(Ordering::SeqCst) {
            noticed = true;
            out.write_all(&notice(&shown.snapshot))?;
            out.flush()?;
            // Forget the bottom row so the next frame paints over the notice.
            if let Some(row) = shown.snapshot.cells.last_mut() {
                row.clear();
            }
        }
        let since = shown.output_version.unwrap_or(0);
        let started = Instant::now();
        let next = handle
            .wait_after(since, WATCH)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        if next.output_version.unwrap_or(0) > since {
            out.write_all(&frame(Some(&shown.snapshot), &next.snapshot))?;
            out.flush()?;
            shown = next;
            std::thread::sleep(MIN_FRAME_GAP.saturating_sub(started.elapsed()));
            continue;
        }
        let running = handle
            .inspect()
            .is_ok_and(|observation| observation.lifecycle == Lifecycle::Running);
        if !running {
            break;
        }
    }
    Ok(())
}

/// The input side: nothing is forwarded yet. EOF means the viewer left.
fn discard_keys(
    mut reader: BufReader<Stream>,
    gone: &AtomicBool,
    typed: &AtomicBool,
    stop: &AtomicBool,
) {
    let mut buf = [0u8; 1024];
    while !stop.load(Ordering::SeqCst) {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => typed.store(true, Ordering::SeqCst),
        }
    }
    gone.store(true, Ordering::SeqCst);
}

/// What the viewer sees when it types into a read-only view.
pub(super) const READ_ONLY_NOTICE: &str =
    "[remuda] read-only view: input to host-backed sessions is not supported yet (detach with your usual key)";

/// The terminal bytes that turn `before` (none: a blank, unknown terminal)
/// into `after`.
pub(super) fn frame(before: Option<&ScreenSnapshot>, after: &ScreenSnapshot) -> Vec<u8> {
    let mut out = String::from("\x1b[?2026h");
    if before.is_none() {
        out.push_str("\x1b[0m\x1b[H\x1b[2J");
    }
    for (index, row) in after.cells.iter().enumerate() {
        let same = before
            .and_then(|before| before.cells.get(index))
            .is_some_and(|old| old == row);
        if !same {
            paint_row(&mut out, index, row);
        }
    }
    place_cursor(&mut out, after.cursor);
    out.push_str("\x1b[?2026l");
    out.into_bytes()
}

/// The notice on the bottom row, inverse video. The next frame repaints
/// that row, so pass the frame to [`frame`] as changed by clearing `before`.
pub(super) fn notice(screen: &ScreenSnapshot) -> Vec<u8> {
    let row = screen.cells.len().max(1);
    let width = screen.cells.first().map_or(80, Vec::len);
    let text: String = READ_ONLY_NOTICE.chars().take(width).collect();
    let mut out = format!("\x1b[?2026h\x1b[{row};1H\x1b[0m\x1b[7m{text}\x1b[0m\x1b[K");
    place_cursor(&mut out, screen.cursor);
    out.push_str("\x1b[?2026l");
    out.into_bytes()
}

fn paint_row(out: &mut String, index: usize, row: &[StyledCell]) {
    let _ = write!(out, "\x1b[{};1H\x1b[0m", index + 1);
    out.push_str(&crate::tui::render_styled_row(row));
    out.push_str("\x1b[0m\x1b[K");
}

fn place_cursor(out: &mut String, cursor: Cursor) {
    let _ = write!(out, "\x1b[{};{}H", cursor.row + 1, cursor.col + 1);
    out.push_str(if cursor.visible {
        "\x1b[?25h"
    } else {
        "\x1b[?25l"
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::agent::Color;

    fn cell(text: &str) -> StyledCell {
        StyledCell {
            text: text.into(),
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
            wide: false,
        }
    }

    fn screen(rows: &[&str], cursor: (u16, u16)) -> ScreenSnapshot {
        ScreenSnapshot {
            cells: rows
                .iter()
                .map(|row| row.chars().map(|c| cell(&c.to_string())).collect())
                .collect(),
            wrapped: vec![false; rows.len()],
            cursor: Cursor {
                row: cursor.0,
                col: cursor.1,
                visible: true,
            },
            scrollback_len: 0,
            scrollback_total: 0,
        }
    }

    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn the_first_frame_clears_and_paints_every_row() {
        let painted = text(frame(None, &screen(&["ab", "cd"], (1, 1))));
        assert!(painted.starts_with("\x1b[?2026h\x1b[0m\x1b[H\x1b[2J"));
        assert!(painted.contains("\x1b[1;1H\x1b[0mab\x1b[0m\x1b[K"));
        assert!(painted.contains("\x1b[2;1H\x1b[0mcd\x1b[0m\x1b[K"));
        assert!(painted.ends_with("\x1b[2;2H\x1b[?25h\x1b[?2026l"));
    }

    #[test]
    fn a_later_frame_paints_only_changed_rows() {
        let before = screen(&["ab", "cd"], (0, 0));
        let after = screen(&["ab", "xy"], (1, 2));
        let painted = text(frame(Some(&before), &after));
        assert!(!painted.contains("\x1b[2J"));
        assert!(!painted.contains("\x1b[1;1H"));
        assert!(painted.contains("\x1b[2;1H\x1b[0mxy"));
        assert!(painted.contains("\x1b[2;3H"));
    }

    #[test]
    fn a_hidden_cursor_stays_hidden() {
        let mut after = screen(&["ab"], (0, 0));
        after.cursor.visible = false;
        assert!(text(frame(None, &after)).contains("\x1b[?25l"));
    }

    #[test]
    fn the_notice_fits_the_bottom_row() {
        let painted = text(notice(&screen(&["abcd", "efgh"], (0, 0))));
        assert!(painted.contains("\x1b[2;1H\x1b[0m\x1b[7m[rem\x1b[0m\x1b[K"));
    }
}
