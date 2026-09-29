//! Parsing for the SGR mouse reports emitted by terminals during direct attach.

use remuda_core::agent::{MouseEncoding, MouseMode, MouseState};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SgrMouse {
    pub button: u16,
    pub x: u16,
    pub y: u16,
    pub release: bool,
}

pub fn wheel_delta(event: SgrMouse) -> Option<isize> {
    if event.button & 0x40 == 0 {
        None
    } else if event.button & 1 == 0 {
        Some(3)
    } else {
        Some(-3)
    }
}

pub fn scroll_offset(current: usize, delta: isize, limit: usize) -> usize {
    if delta >= 0 {
        current.saturating_add(delta as usize).min(limit)
    } else {
        current.saturating_sub(delta.unsigned_abs())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MouseAction {
    Forward(Vec<u8>),
    Scroll(usize),
    Ignore,
}

pub fn route_mouse_event(
    event: SgrMouse,
    state: MouseState,
    mouse_on: bool,
    current_offset: usize,
) -> MouseAction {
    if !mouse_on {
        return MouseAction::Forward(
            format!(
                "\x1b[<{};{};{}{term}",
                event.button,
                event.x,
                event.y,
                term = if event.release { 'm' } else { 'M' }
            )
            .into_bytes(),
        );
    }
    if state.mode == MouseMode::None {
        return match wheel_delta(event) {
            Some(delta) => {
                let next = scroll_offset(current_offset, delta, 10_000);
                if next == current_offset {
                    MouseAction::Ignore
                } else {
                    MouseAction::Scroll(next)
                }
            }
            None => MouseAction::Ignore,
        };
    }
    encode_for_child(event, state).map_or(MouseAction::Ignore, MouseAction::Forward)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputToken {
    Bytes(Vec<u8>),
    Mouse(SgrMouse),
    /// Bytes between bracketed-paste markers, including both markers. They
    /// bypass mouse parsing and attach hotkeys verbatim.
    Paste(Vec<u8>),
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Default)]
pub struct SgrParser {
    pending: Vec<u8>,
    pending_since: Option<std::time::Instant>,
    in_paste: bool,
}

const ESC_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(25);

/// Translate a host SGR event into the live child terminal's selected wire
/// format. Unsupported release or motion reports are omitted.
pub fn encode_for_child(event: SgrMouse, state: MouseState) -> Option<Vec<u8>> {
    if state.mode == MouseMode::None || (event.release && state.mode == MouseMode::Press) {
        return None;
    }
    let motion = event.button & 32 != 0;
    if motion && matches!(state.mode, MouseMode::Press | MouseMode::PressRelease) {
        return None;
    }
    if motion && state.mode == MouseMode::ButtonMotion && event.button & 3 == 3 {
        return None;
    }
    let xterm = if event.release {
        (event.button & !3) | 3
    } else {
        event.button
    };
    let (x, y) = (event.x, event.y);
    Some(match state.encoding {
        MouseEncoding::Sgr => format!(
            "\x1b[<{xterm};{x};{}{term}",
            y,
            term = if event.release { 'm' } else { 'M' }
        )
        .into_bytes(),
        MouseEncoding::Default => {
            let coords = [x.saturating_add(32), y.saturating_add(32)];
            if x > 223 || y > 223 || xterm > 223 {
                return None;
            }
            vec![
                0x1b,
                b'[',
                b'M',
                (xterm + 32) as u8,
                coords[0] as u8,
                coords[1] as u8,
            ]
        }
        MouseEncoding::Utf8 => {
            let mut bytes = b"\x1b[M".to_vec();
            for value in [xterm + 32, x + 32, y + 32] {
                let ch = char::from_u32(value as u32)?;
                let mut encoded = [0; 4];
                bytes.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
            }
            bytes
        }
    })
}

impl SgrParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<InputToken> {
        if self.pending.is_empty() && bytes.contains(&0x1b) {
            self.pending_since = Some(std::time::Instant::now());
        }
        self.pending.extend_from_slice(bytes);
        let tokens = self.parse(false);
        if self.pending.is_empty() {
            self.pending_since = None;
        }
        tokens
    }

    pub fn finish(&mut self) -> Vec<InputToken> {
        let tokens = self.parse(true);
        self.pending_since = None;
        tokens
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn timeout_remaining(&self) -> Option<std::time::Duration> {
        self.pending_since
            .map(|since| ESC_TIMEOUT.saturating_sub(since.elapsed()))
    }

    pub fn flush_expired(&mut self) -> Vec<InputToken> {
        if self
            .pending_since
            .is_some_and(|since| since.elapsed() >= ESC_TIMEOUT)
        {
            self.finish()
        } else {
            Vec::new()
        }
    }

    fn parse(&mut self, finishing: bool) -> Vec<InputToken> {
        let mut out = Vec::new();
        loop {
            if self.in_paste {
                if let Some(end) = self
                    .pending
                    .windows(PASTE_END.len())
                    .position(|window| window == PASTE_END)
                {
                    let len = end + PASTE_END.len();
                    out.push(InputToken::Paste(self.pending.drain(..len).collect()));
                    self.in_paste = false;
                    continue;
                }
                let held = longest_suffix_prefix(&self.pending, PASTE_END);
                let deliver = self.pending.len().saturating_sub(held);
                if deliver > 0 {
                    out.push(InputToken::Paste(self.pending.drain(..deliver).collect()));
                }
                if finishing && !self.pending.is_empty() {
                    out.push(InputToken::Paste(std::mem::take(&mut self.pending)));
                }
                break;
            }
            let Some(at) = self.pending.iter().position(|&b| b == 0x1b) else {
                if !self.pending.is_empty() {
                    out.push(InputToken::Bytes(std::mem::take(&mut self.pending)));
                }
                break;
            };
            if at > 0 {
                out.push(InputToken::Bytes(self.pending.drain(..at).collect()));
            }
            if self.pending.starts_with(PASTE_START) {
                out.push(InputToken::Paste(
                    self.pending.drain(..PASTE_START.len()).collect(),
                ));
                self.in_paste = true;
                continue;
            }
            if PASTE_START.starts_with(&self.pending) {
                if finishing {
                    out.push(InputToken::Bytes(std::mem::take(&mut self.pending)));
                }
                break;
            }
            if self.pending.len() == 1 {
                if finishing {
                    out.push(InputToken::Bytes(std::mem::take(&mut self.pending)));
                }
                break;
            }
            if self.pending[1] != b'[' {
                out.push(InputToken::Bytes(std::mem::take(&mut self.pending)));
                continue;
            }
            if self.pending.len() == 2 {
                if finishing {
                    out.push(InputToken::Bytes(std::mem::take(&mut self.pending)));
                }
                break;
            }
            if self.pending[2] != b'<' {
                if let Some(end) = self.pending[2..]
                    .iter()
                    .position(|b| (0x40..=0x7e).contains(b))
                {
                    let len = end + 3;
                    out.push(InputToken::Bytes(self.pending.drain(..len).collect()));
                    continue;
                }
                // Once the third byte is not `<`, this cannot become an SGR
                // mouse report. Preserve it now; the input loop must not wait.
                out.push(InputToken::Bytes(std::mem::take(&mut self.pending)));
                continue;
            }
            match parse_sgr(&self.pending) {
                Parse::Incomplete if !finishing => break,
                Parse::Incomplete | Parse::Invalid => {
                    out.push(InputToken::Bytes(vec![self.pending.remove(0)]));
                }
                Parse::Mouse(mouse, len) => {
                    self.pending.drain(..len);
                    out.push(InputToken::Mouse(mouse));
                }
            }
        }
        let mut merged = Vec::with_capacity(out.len());
        for token in out {
            match token {
                InputToken::Bytes(bytes)
                    if matches!(merged.last_mut(), Some(InputToken::Bytes(_))) =>
                {
                    if let Some(InputToken::Bytes(previous)) = merged.last_mut() {
                        previous.extend(bytes);
                    }
                }
                other => merged.push(other),
            }
        }
        merged
    }
}

fn longest_suffix_prefix(bytes: &[u8], prefix: &[u8]) -> usize {
    (1..=bytes.len().min(prefix.len()))
        .rev()
        .find(|&len| bytes[bytes.len() - len..] == prefix[..len])
        .unwrap_or(0)
}

enum Parse {
    Incomplete,
    Invalid,
    Mouse(SgrMouse, usize),
}

fn parse_sgr(bytes: &[u8]) -> Parse {
    let mut values = [0u16; 3];
    let mut field = 0;
    let mut digits = 0;
    for (i, &byte) in bytes.iter().enumerate().skip(3) {
        match byte {
            b'0'..=b'9' => {
                digits += 1;
                let Some(value) = values[field]
                    .checked_mul(10)
                    .and_then(|v| v.checked_add((byte - b'0') as u16))
                else {
                    return Parse::Invalid;
                };
                values[field] = value;
            }
            b';' if field < 2 && digits > 0 => {
                field += 1;
                digits = 0;
            }
            b'M' | b'm' if field == 2 && digits > 0 => {
                if values[1] == 0 || values[2] == 0 {
                    return Parse::Invalid;
                }
                return Parse::Mouse(
                    SgrMouse {
                        button: values[0],
                        x: values[1],
                        y: values[2],
                        release: byte == b'm',
                    },
                    i + 1,
                );
            }
            _ => return Parse::Invalid,
        }
    }
    Parse::Incomplete
}

#[cfg(test)]
mod tests {
    use super::{
        encode_for_child, route_mouse_event, scroll_offset, wheel_delta, InputToken, MouseAction,
        SgrMouse, SgrParser,
    };
    use remuda_core::agent::{MouseEncoding, MouseMode, MouseState};

    #[test]
    fn parses_a_fragmented_wheel_report_without_changing_surrounding_keys() {
        let mut parser = SgrParser::default();
        assert_eq!(
            parser.feed(b"j\x1b[<64;"),
            vec![InputToken::Bytes(b"j".to_vec())]
        );
        assert_eq!(
            parser.feed(b"12;9M"),
            vec![InputToken::Mouse(SgrMouse {
                button: 64,
                x: 12,
                y: 9,
                release: false
            })]
        );
        assert_eq!(
            parser.feed(b"\x1b[A"),
            vec![InputToken::Bytes(b"\x1b[A".to_vec())]
        );
    }

    #[test]
    fn malformed_mouse_like_input_is_preserved_byte_for_byte() {
        let mut parser = SgrParser::default();
        let input = b"\x1b[<nopeM";
        assert_eq!(parser.feed(input), vec![InputToken::Bytes(input.to_vec())]);
    }

    #[test]
    fn esc_deadline_and_alt_key_disambiguation() {
        let mut parser = SgrParser::default();
        assert!(parser.feed(b"\x1b").is_empty());
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(
            parser.flush_expired(),
            vec![InputToken::Bytes(b"\x1b".to_vec())]
        );
        assert_eq!(parser.feed(b"x"), vec![InputToken::Bytes(b"x".to_vec())]);
        let mut parser = SgrParser::default();
        assert_eq!(
            parser.feed(b"\x1bx"),
            vec![InputToken::Bytes(b"\x1bx".to_vec())]
        );
    }

    #[test]
    fn split_bracketed_paste_preserves_sgr_and_hotkey_bytes() {
        let mut parser = SgrParser::default();
        assert!(parser.feed(b"\x1b[20").is_empty());
        assert_eq!(
            parser.feed(b"0~\x1b[<64;1;2M\x1d\x1b[20"),
            vec![
                InputToken::Paste(b"\x1b[200~".to_vec()),
                InputToken::Paste(b"\x1b[<64;1;2M\x1d".to_vec())
            ]
        );
        assert_eq!(
            parser.feed(b"1~\x1b[<64;2;3M"),
            vec![
                InputToken::Paste(b"\x1b[201~".to_vec()),
                InputToken::Mouse(SgrMouse {
                    button: 64,
                    x: 2,
                    y: 3,
                    release: false
                })
            ]
        );
    }

    #[test]
    fn encodes_events_in_the_child_encoding_and_omits_unrequested_releases() {
        let event = SgrMouse {
            button: 0,
            x: 12,
            y: 9,
            release: true,
        };
        assert_eq!(
            encode_for_child(
                event,
                MouseState {
                    mode: MouseMode::Press,
                    encoding: MouseEncoding::Sgr,
                    bracketed_paste: false,
                }
            ),
            None
        );
        assert_eq!(
            encode_for_child(
                event,
                MouseState {
                    mode: MouseMode::PressRelease,
                    encoding: MouseEncoding::Sgr,
                    bracketed_paste: false,
                }
            )
            .unwrap(),
            b"\x1b[<3;12;9m"
        );
        assert_eq!(
            encode_for_child(
                SgrMouse {
                    release: false,
                    ..event
                },
                MouseState {
                    mode: MouseMode::Press,
                    encoding: MouseEncoding::Default,
                    bracketed_paste: false,
                }
            )
            .unwrap(),
            b"\x1b[M \x2c\x29"
        );
        assert_eq!(
            encode_for_child(
                SgrMouse {
                    button: 32,
                    release: false,
                    ..event
                },
                MouseState {
                    mode: MouseMode::PressRelease,
                    encoding: MouseEncoding::Sgr,
                    bracketed_paste: false,
                }
            ),
            None
        );
    }

    #[test]
    fn wheel_events_move_through_the_history_in_three_row_steps() {
        let up = SgrMouse {
            button: 64,
            x: 1,
            y: 1,
            release: false,
        };
        let down = SgrMouse { button: 65, ..up };
        assert_eq!(wheel_delta(up), Some(3));
        assert_eq!(wheel_delta(down), Some(-3));
        assert_eq!(wheel_delta(SgrMouse { button: 0, ..up }), None);
        assert_eq!(scroll_offset(0, 3, 10_000), 3);
        assert_eq!(scroll_offset(3, -3, 10_000), 0);
        assert_eq!(scroll_offset(9_999, 3, 10_000), 10_000);
    }

    #[test]
    fn fake_attach_routes_wheel_by_child_mode_and_respects_the_mouse_knob() {
        let wheel = SgrMouse {
            button: 64,
            x: 12,
            y: 9,
            release: false,
        };
        assert_eq!(
            route_mouse_event(wheel, MouseState::default(), true, 0),
            MouseAction::Scroll(3)
        );
        assert_eq!(
            route_mouse_event(
                wheel,
                MouseState {
                    mode: MouseMode::Press,
                    encoding: MouseEncoding::Sgr,
                    bracketed_paste: false,
                },
                true,
                0,
            ),
            MouseAction::Forward(b"\x1b[<64;12;9M".to_vec())
        );
        assert_eq!(
            route_mouse_event(wheel, MouseState::default(), false, 0),
            MouseAction::Forward(b"\x1b[<64;12;9M".to_vec())
        );
    }
}
