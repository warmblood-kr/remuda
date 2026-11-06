//! `remuda attach` to a host-backed session (#713).
//!
//! A host keeps no raw output stream, only typed screens, so the viewer is
//! painted from snapshots: the first frame in full, then only rows that
//! changed, each frame wrapped in synchronized output. Keys go to the host
//! through a human grant (one per attach; a newer attach takes over). A host
//! too old to grant gets a read-only view, and the first key shows a notice.
use crate::ipc::{self, Stream, TryClone};
use remuda_core::agent::{
    Cursor, MouseEncoding, MouseMode, ScreenSnapshot, StyledCell, TerminalModes,
};
use remuda_core::protocol::Response;
use remuda_core::session_operations::{
    Error, Lifecycle, SessionAttachment, SessionHandle, Unavailable,
};
use std::fmt::Write as _;
use std::io::{BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long one wait for new output lasts before liveness is checked again.
const WATCH: Duration = Duration::from_millis(250);
/// At most this many frames a second, however fast the child prints.
const MIN_FRAME_GAP: Duration = Duration::from_millis(33);

/// What a viewer leaves behind: set by the key side, read by the paint side.
#[derive(Default)]
struct Viewer {
    gone: AtomicBool,
    typed: AtomicBool,
    input_failed: AtomicBool,
    stop: AtomicBool,
}

/// Serve one viewer until it detaches, is taken over, the session ends, or
/// a write fails. A tracked attach is acknowledged with its grant; a
/// read-only one with generation 0, which nothing can take over.
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
    let attachment = match handle.attach() {
        Ok(attachment) => Some(attachment),
        Err(Error::Unavailable(Unavailable::Unsupported)) => None,
        Err(error) => return super::reply(&stream, &Response::error(error.to_string())),
    };
    let acknowledgement = if tracked {
        Response::AttachStarted {
            generation: attachment.as_ref().map_or(0, SessionAttachment::generation),
        }
    } else {
        Response::Ok
    };
    // Best effort, as in-process: a viewer that stopped reading may still
    // have sent keys, and those must reach the child before its EOF.
    let _ = super::reply(&stream, &acknowledgement);
    let mut out = stream.try_clone()?;
    let viewer = Viewer::default();
    std::thread::scope(|scope| {
        let keys = scope.spawn(|| pump_keys(reader, attachment.as_ref(), &viewer));
        if paint(&mut out, handle, attachment.as_ref(), first, &viewer).is_err() {
            // The output side failed, which can just mean the viewer closed
            // its read half after typing. Let the key pump drain to EOF unless
            // there is nowhere safe left to deliver its keys.
            while !keys.is_finished() && !must_stop(handle, attachment.as_ref(), &viewer) {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if viewer.input_failed.load(Ordering::SeqCst) {
            let _ = out
                .write_all(super::ATTACH_INPUT_FAILURE_NOTICE.as_bytes())
                .and_then(|()| out.flush());
        }
        ipc::stop_reader(&stream, &viewer.stop, || keys.is_finished());
        let _ = keys.join();
    });
    Ok(())
}

/// Input has nowhere safe to go: the input failed, a newer attach took
/// over, or the session stopped running.
fn must_stop(
    handle: &SessionHandle,
    attachment: Option<&SessionAttachment<'_>>,
    viewer: &Viewer,
) -> bool {
    viewer.input_failed.load(Ordering::SeqCst)
        || attachment.is_some_and(|attachment| attachment.displaced().unwrap_or(false))
        || !handle
            .inspect()
            .is_ok_and(|observation| observation.lifecycle == Lifecycle::Running)
}

/// The output side: repaint on every new generation until the viewer leaves,
/// is taken over, or the session stops running.
fn paint(
    out: &mut Stream,
    handle: &SessionHandle,
    attachment: Option<&SessionAttachment<'_>>,
    first: remuda_core::agent::VersionedSnapshot,
    viewer: &Viewer,
) -> std::io::Result<()> {
    out.write_all(&modes(None, first.modes))?;
    out.write_all(&frame(None, &first.snapshot))?;
    out.flush()?;
    let mut shown = first;
    let mut noticed = false;
    while !viewer.gone.load(Ordering::SeqCst) {
        if attachment.is_none() && !noticed && viewer.typed.load(Ordering::SeqCst) {
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
            out.write_all(&modes(shown.modes, next.modes))?;
            out.write_all(&frame(Some(&shown.snapshot), &next.snapshot))?;
            out.flush()?;
            shown = next;
            std::thread::sleep(MIN_FRAME_GAP.saturating_sub(started.elapsed()));
            continue;
        }
        if attachment.is_some_and(|attachment| attachment.displaced().unwrap_or(false)) {
            // Same plain text as an in-process takeover, before the EOF.
            out.write_all(b"\r\n[remuda] attached elsewhere, detached\r\n")?;
            out.flush()?;
            break;
        }
        let running = handle
            .inspect()
            .is_ok_and(|observation| observation.lifecycle == Lifecycle::Running);
        if !running || viewer.input_failed.load(Ordering::SeqCst) {
            break;
        }
    }
    Ok(())
}

/// The input side: each read goes to the host as one write, in order. EOF
/// means the viewer left. Without a grant, keys only raise the notice.
fn pump_keys(
    mut reader: BufReader<Stream>,
    attachment: Option<&SessionAttachment<'_>>,
    viewer: &Viewer,
) {
    let mut buf = [0u8; 4096];
    let started: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
    let cancelled = || viewer.stop.load(Ordering::SeqCst);
    while !viewer.stop.load(Ordering::SeqCst) {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let Some(attachment) = attachment else {
            viewer.typed.store(true, Ordering::SeqCst);
            continue;
        };
        if attachment
            .write_raw(&buf[..n], &cancelled, Arc::clone(&started))
            .is_err()
        {
            // A takeover is reported by the paint side; anything else here.
            if !attachment.displaced().unwrap_or(false) {
                viewer.input_failed.store(true, Ordering::SeqCst);
            }
            break;
        }
    }
    viewer.gone.store(true, Ordering::SeqCst);
}

/// What the viewer sees when it types into a read-only view.
pub(super) const READ_ONLY_NOTICE: &str =
    "[remuda] read-only view: this session's host is too old to accept input (detach with your usual key)";

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

/// Mode sequences moving a viewer's terminal from `before` (none: defaults)
/// to `after`, so keys, pastes and mouse reports reach the child as it asked.
/// Nothing when the host does not report modes.
pub(super) fn modes(before: Option<TerminalModes>, after: Option<TerminalModes>) -> Vec<u8> {
    let Some(after) = after else {
        return Vec::new();
    };
    // Like the in-process paint (vt100 `state_formatted`), a fresh terminal is
    // taken to be in default modes, so only modes the child changed are sent.
    // Stating "mouse off" would also undo the attach client's own wheel mode.
    let before = Some(before.unwrap_or_default());
    let mut out = String::new();
    let changed =
        |pick: fn(&TerminalModes) -> bool| before.is_none_or(|b| pick(&b) != pick(&after));
    if before.is_none_or(|b| b.mouse.mode != after.mouse.mode) {
        out.push_str("\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l");
        out.push_str(match after.mouse.mode {
            MouseMode::None => "",
            MouseMode::Press => "\x1b[?9h",
            MouseMode::PressRelease => "\x1b[?1000h",
            MouseMode::ButtonMotion => "\x1b[?1002h",
            MouseMode::AnyMotion => "\x1b[?1003h",
        });
    }
    if before.is_none_or(|b| b.mouse.encoding != after.mouse.encoding) {
        out.push_str("\x1b[?1005l\x1b[?1006l");
        out.push_str(match after.mouse.encoding {
            MouseEncoding::Default => "",
            MouseEncoding::Utf8 => "\x1b[?1005h",
            MouseEncoding::Sgr => "\x1b[?1006h",
        });
    }
    if changed(|m| m.mouse.bracketed_paste) {
        out.push_str(if after.mouse.bracketed_paste {
            "\x1b[?2004h"
        } else {
            "\x1b[?2004l"
        });
    }
    if changed(|m| m.application_cursor) {
        out.push_str(if after.application_cursor {
            "\x1b[?1h"
        } else {
            "\x1b[?1l"
        });
    }
    if changed(|m| m.application_keypad) {
        out.push_str(if after.application_keypad {
            "\x1b="
        } else {
            "\x1b>"
        });
    }
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
    fn modes_differing_from_default_are_stated_first_then_only_when_they_change() {
        let vim = TerminalModes {
            application_cursor: true,
            mouse: remuda_core::agent::MouseState {
                bracketed_paste: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let first = text(modes(None, Some(vim)));
        assert_eq!(first, "\x1b[?2004h\x1b[?1h");
        assert!(modes(None, Some(TerminalModes::default())).is_empty());
        assert!(modes(Some(vim), Some(vim)).is_empty());
        let mut mouse = vim;
        mouse.mouse.mode = MouseMode::PressRelease;
        mouse.mouse.encoding = MouseEncoding::Sgr;
        let changed = text(modes(Some(vim), Some(mouse)));
        assert!(
            changed.ends_with("\x1b[?1000h\x1b[?1005l\x1b[?1006l\x1b[?1006h"),
            "{changed:?}"
        );
        assert!(modes(None, None).is_empty());
    }

    #[test]
    fn the_notice_fits_the_bottom_row() {
        let painted = text(notice(&screen(&["abcd", "efgh"], (0, 0))));
        assert!(painted.contains("\x1b[2;1H\x1b[0m\x1b[7m[rem\x1b[0m\x1b[K"));
    }
}
