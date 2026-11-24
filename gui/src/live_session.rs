//! A single attached view's instance-bound input. The view owns focus,
//! currentness and the corresponding strict preview generation.
use crate::{input_helper::InputHelper, Selection};
use gpui::Keystroke;
use remuda_native::strict_input::MAX_BATCH;
use std::{cell::RefCell, io, time::Duration};

pub(crate) use crate::input_helper::{Event, Submit};

pub(crate) struct LiveSession {
    input: InputHelper,
    pending: RefCell<Vec<u8>>,
}

impl LiveSession {
    pub(crate) fn new(selection: &Selection, compose: bool) -> io::Result<Self> {
        if selection.name.is_empty() || selection.instance.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing session identity",
            ));
        }
        let peer = selection.peer.as_deref().unwrap_or_default();
        let kind = match (selection.peer.is_some(), compose) {
            (false, false) => "local",
            (true, false) => "peer",
            (false, true) => "line-local",
            (true, true) => "line-peer",
        };
        let input = InputHelper::start(
            &std::env::current_exe()?,
            &[
                "--internal-input-worker",
                kind,
                peer,
                &selection.name,
                &selection.instance,
            ],
        )?;
        Ok(Self {
            input,
            pending: RefCell::new(Vec::new()),
        })
    }

    pub(crate) fn submit(&self, bytes: &[u8]) -> Submit {
        match self.input.submit(bytes) {
            Submit::Busy => {
                let mut pending = self.pending.borrow_mut();
                if bytes.len() > MAX_BATCH.saturating_sub(pending.len()) {
                    return Submit::Busy;
                }
                pending.extend_from_slice(bytes);
                Submit::Queued
            }
            result => result,
        }
    }

    pub(crate) fn submit_line(&self, bytes: &[u8]) -> Submit {
        self.input.submit(bytes)
    }

    pub(crate) fn paste_image_path(&self, path: &str) -> Submit {
        self.input.paste_image_path(path)
    }

    /// Poll only while the view is still attached and authorized. A successful
    /// receipt releases pending keystrokes. An uncertain one discards them.
    pub(crate) fn poll(&self) -> Option<Event> {
        let event = self.input.recv_event_timeout(Duration::ZERO).ok()?;
        let mut pending = self.pending.borrow_mut();
        if matches!(event, Event::Applied | Event::PasteUnavailable)
            && !pending.is_empty()
            && self.input.submit(&pending) != Submit::Queued
        {
            pending.clear();
            return Some(Event::NotAcknowledged);
        }
        pending.clear();
        Some(event)
    }

    pub(crate) fn stop(&self) {
        self.pending.borrow_mut().clear();
        self.input.stop();
    }
}

pub(crate) fn key_bytes(key: &Keystroke) -> Option<Vec<u8>> {
    let modifiers = key.modifiers;
    if modifiers.platform || modifiers.function {
        return None;
    }
    if modifiers.control && !modifiers.alt {
        let name = key.key.as_bytes();
        return match name {
            [letter] if letter.is_ascii_alphabetic() => Some(vec![letter & 0x1f]),
            b" " | b"space" => Some(vec![0]),
            _ => None,
        };
    }
    let bytes: &[u8] = match key.key.as_str() {
        "enter" => b"\r",
        "escape" => b"\x1b",
        "tab" if modifiers.shift => b"\x1b[Z",
        "tab" => b"\t",
        "backspace" => b"\x7f",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "delete" => b"\x1b[3~",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        _ => key.key_char.as_deref()?.as_bytes(),
    };
    (!bytes.is_empty()).then(|| bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_keys_and_literal_text_preserve_order_and_platform_shortcuts() {
        let mut key = Keystroke {
            key: "enter".into(),
            ..Keystroke::default()
        };
        assert_eq!(key_bytes(&key).as_deref(), Some(&b"\r"[..]));
        key.key = "c".into();
        key.key_char = Some("c".into());
        key.modifiers.control = true;
        assert_eq!(key_bytes(&key).as_deref(), Some(&b"\x03"[..]));
        key.modifiers.platform = true;
        assert_eq!(key_bytes(&key), None);
        key.modifiers = Default::default();
        key.key_char = Some("한".into());
        assert_eq!(key_bytes(&key).as_deref(), Some("한".as_bytes()));
    }

    #[cfg(unix)]
    #[test]
    fn pending_keys_are_coalesced_in_order_after_acknowledgement() {
        let program = std::path::Path::new("/bin/sh");
        let fixture = "printf 'remuda-input-v1\\n'
            dd bs=1 count=5 of=/dev/null 2>/dev/null
            printf '\\001'
            dd bs=1 count=4 of=/dev/null 2>/dev/null
            test \"$(dd bs=1 count=2 2>/dev/null)\" = bc && printf '\\001'";
        let session = LiveSession {
            input: InputHelper::start(program, &["-c", fixture]).unwrap(),
            pending: RefCell::new(Vec::new()),
        };
        for byte in [b"a", b"b", b"c"] {
            assert_eq!(session.submit(byte), Submit::Queued);
        }
        assert_eq!(&*session.pending.borrow(), b"bc");
        for _ in 0..2 {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(event) = session.poll() {
                    assert_eq!(event, Event::Applied);
                    break;
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(session.pending.borrow().is_empty());
    }
}
