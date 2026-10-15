//! Bound only OSC accumulation before pinned vte's std OSC Vec.
//! DCS/SOS/PM/APC do not accumulate payload in pinned vte and pass through.
//! Oversized OSC is counted and ignored through BEL/ST/CAN/SUB, including
//! split ST. This explicit unsupported-sequence policy is not a raw-byte replay.
const MAX_STRING: usize = 4096;
#[derive(Clone, Copy)]
enum State {
    Ground,
    Escape,
    Intermediate,
    Osc,
    OscEscape,
    Discard { escape: bool },
}
pub(crate) struct Strings {
    state: State,
    buffer: Vec<u8>,
    pub unsupported: u64,
}
impl Default for Strings {
    fn default() -> Self {
        Self {
            state: State::Ground,
            buffer: Vec::new(),
            unsupported: 0,
        }
    }
}
impl Strings {
    pub fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(&[u8])) {
        let mut offset = 0;
        while offset < bytes.len() {
            if matches!(self.state, State::Ground) {
                let count = bytes[offset..]
                    .iter()
                    .position(|b| *b == 27)
                    .unwrap_or(bytes.len() - offset);
                if count != 0 {
                    emit(&bytes[offset..offset + count]);
                    offset += count;
                }
                if offset == bytes.len() {
                    break;
                }
            }
            let byte = bytes[offset];
            offset += 1;
            self.byte(byte, &mut emit);
        }
    }
    fn byte(&mut self, byte: u8, emit: &mut impl FnMut(&[u8])) {
        match self.state {
            State::Ground => self.state = State::Escape,
            State::Escape => match byte {
                b']' => {
                    self.buffer.extend_from_slice(b"\x1b]");
                    self.state = State::Osc;
                }
                // vte keeps Escape after executing C0 and ignoring DEL/high
                // bytes. Emit controls now, but retain an escape sentinel.
                0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f..=0xff => emit(&[27, byte]),
                27 => emit(&[27]),
                0x20..=0x2f => {
                    emit(&[27, byte]);
                    self.state = State::Intermediate;
                }
                _ => {
                    emit(&[27, byte]);
                    self.state = State::Ground;
                }
            },
            State::Intermediate => match byte {
                27 => self.state = State::Escape,
                0x18 | 0x1a | 0x30..=0x7e => {
                    emit(&[byte]);
                    self.state = State::Ground;
                }
                _ => emit(&[byte]),
            },
            State::Osc => {
                if self.buffer.len() == MAX_STRING {
                    self.buffer.clear();
                    self.unsupported = self.unsupported.saturating_add(1);
                    emit(&[27, 24]);
                    self.state = State::Discard { escape: false };
                    self.byte(byte, emit);
                } else if byte == 27 {
                    self.state = State::OscEscape;
                } else {
                    self.buffer.push(byte);
                    if matches!(byte, 7 | 24 | 26) {
                        self.flush(emit);
                        self.state = State::Ground;
                    }
                }
            }
            State::OscEscape => {
                // vte ends a supported OSC at ESC, then parses the next byte
                // in Escape, including executed C0/ignored DEL/intermediates.
                self.flush(emit);
                self.state = State::Escape;
                self.byte(byte, emit);
            }
            State::Discard { escape } => {
                if matches!(byte, 7 | 24 | 26) || (escape && byte == b'\\') {
                    self.state = State::Ground;
                } else {
                    self.state = State::Discard { escape: byte == 27 };
                }
            }
        }
    }
    fn flush(&mut self, emit: &mut impl FnMut(&[u8])) {
        emit(&self.buffer);
        self.buffer.clear();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn filtered(bytes: &[u8], split: usize) -> (vt100::Parser, Strings) {
        let mut parser = vt100::Parser::new(24, 80, 10);
        let mut filter = Strings::default();
        filter.feed(&bytes[..split], |chunk| parser.process(chunk));
        filter.feed(&bytes[split..], |chunk| parser.process(chunk));
        (parser, filter)
    }
    #[test]
    fn controls_inside_escape_cannot_bypass_osc_payload_cap() {
        #[derive(Default)]
        struct Capture {
            largest: usize,
        }
        impl vte::Perform for Capture {
            fn osc_dispatch(&mut self, params: &[&[u8]], _: bool) {
                self.largest = self
                    .largest
                    .max(params.iter().map(|value| value.len()).sum());
            }
        }
        for control in 0..=255_u8 {
            let mut bytes = vec![27, control, b']', b'0', b';'];
            bytes.extend_from_slice(&vec![b'x'; MAX_STRING * 2]);
            bytes.push(7);
            let mut filter = Strings::default();
            let mut parser = vte::Parser::new();
            let mut capture = Capture::default();
            for part in bytes.chunks(17) {
                filter.feed(part, |part| parser.advance(&mut capture, part));
            }
            assert!(
                capture.largest <= MAX_STRING,
                "control {control}, payload {}",
                capture.largest
            );
            if matches!(control, 0..=23 | 25 | 27..=31 | 127..=255) {
                assert_eq!(filter.unsupported, 1);
            }
        }
    }
    #[test]
    fn supported_strings_match_reference_for_every_split() {
        let bytes = b"abc\x1b]0;title\x07\x1bPignored\x1b\\\x1b]title\x1b[31mred\x1b]cancel\x18end";
        let mut reference = vt100::Parser::new(24, 80, 10);
        reference.process(bytes);
        for split in 0..=bytes.len() {
            let (parser, filter) = filtered(bytes, split);
            assert_eq!(
                parser.screen().state_formatted(),
                reference.screen().state_formatted(),
                "split {split}"
            );
            assert_eq!(filter.unsupported, 0);
        }
    }
    #[test]
    fn oversized_string_ignores_suffix_until_split_st_and_reports_it() {
        for start in *b"]" {
            let mut stream = b"\x1b[31".to_vec(); // preceding partial CSI must be cancelled
            stream.extend_from_slice(&[27, start]);
            stream.extend_from_slice(&vec![b'x'; MAX_STRING + 16]);
            stream.extend_from_slice(b"\x1b[2Jhidden\x1b\\visible");
            for split in [
                0,
                2,
                MAX_STRING,
                stream.len() - 8,
                stream.len() - 7,
                stream.len(),
            ] {
                let (parser, filter) = filtered(&stream, split);
                assert_eq!(
                    parser.screen().contents(),
                    "visible",
                    "start {start} split {split}"
                );
                assert_eq!(filter.unsupported, 1);
                assert!(filter.buffer.len() <= MAX_STRING);
                assert!(filter.buffer.capacity() <= MAX_STRING * 2);
            }
        }
    }
    #[test]
    fn cancelled_oversized_osc_and_raw_dcs_st_resume_visible_output() {
        for cancel in [24, 26] {
            let mut bytes = b"\x1b]0;".to_vec();
            bytes.extend_from_slice(&vec![b'x'; MAX_STRING + 1]);
            bytes.push(cancel);
            bytes.extend_from_slice(b"visible");
            for split in [0, 1, MAX_STRING, bytes.len() - 7, bytes.len()] {
                let (parser, filter) = filtered(&bytes, split);
                assert_eq!(parser.screen().contents(), "visible");
                assert_eq!(filter.unsupported, 1);
            }
        }
        let bytes = b"\x1bPqpayload\x9cvisible";
        let mut reference = vt100::Parser::new(24, 80, 10);
        reference.process(bytes);
        for split in 0..=bytes.len() {
            let (parser, filter) = filtered(bytes, split);
            assert_eq!(
                parser.screen().state_formatted(),
                reference.screen().state_formatted()
            );
            assert_eq!(filter.unsupported, 0);
        }
    }
    #[test]
    fn endless_osc_and_dcs_stay_bounded_across_many_chunks() {
        for start in *b"]P" {
            let mut parser = vt100::Parser::new(24, 80, 10);
            let mut filter = Strings::default();
            filter.feed(&[27, start], |bytes| parser.process(bytes));
            for _ in 0..1024 {
                filter.feed(&[b'x'; 4096], |bytes| parser.process(bytes));
            }
            assert!(filter.buffer.len() <= MAX_STRING);
            assert_eq!(filter.unsupported, if start == b']' { 1 } else { 0 });
            filter.feed(b"\x1b", |bytes| parser.process(bytes));
            filter.feed(b"\\done", |bytes| parser.process(bytes));
            assert_eq!(parser.screen().contents(), "done");
        }
    }
}
