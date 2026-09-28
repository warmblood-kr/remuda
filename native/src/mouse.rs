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
}

#[derive(Default)]
pub struct SgrParser {
    pending: Vec<u8>,
}

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
        self.pending.extend_from_slice(bytes);
        self.parse(false)
    }

    pub fn finish(&mut self) -> Vec<InputToken> {
        self.parse(true)
    }

    fn parse(&mut self, finishing: bool) -> Vec<InputToken> {
        let mut out = Vec::new();
        loop {
            let Some(at) = self.pending.iter().position(|&b| b == 0x1b) else {
                if !self.pending.is_empty() {
                    out.push(InputToken::Bytes(std::mem::take(&mut self.pending)));
                }
                break;
            };
            if at > 0 {
                out.push(InputToken::Bytes(self.pending.drain(..at).collect()));
            }
            if self.pending.len() < 3 {
                if finishing {
                    out.push(InputToken::Bytes(std::mem::take(&mut self.pending)));
                }
                break;
            }
            if self.pending[..3] != *b"\x1b[<" {
                if self.pending[1] == b'[' {
                    if let Some(end) = self.pending[2..]
                        .iter()
                        .position(|b| (0x40..=0x7e).contains(b))
                    {
                        let len = end + 3;
                        out.push(InputToken::Bytes(self.pending.drain(..len).collect()));
                        continue;
                    }
                    if !finishing {
                        break;
                    }
                }
                out.push(InputToken::Bytes(vec![self.pending.remove(0)]));
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
                    encoding: MouseEncoding::Sgr
                }
            ),
            None
        );
        assert_eq!(
            encode_for_child(
                event,
                MouseState {
                    mode: MouseMode::PressRelease,
                    encoding: MouseEncoding::Sgr
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
                    encoding: MouseEncoding::Default
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
                    encoding: MouseEncoding::Sgr
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
