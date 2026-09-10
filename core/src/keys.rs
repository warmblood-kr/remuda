//! Keystrokes and mouse reports, as the bytes a terminal actually sends.
//!
//! Character, string, ANSI and mouse input are not four operations. They are
//! one — [`crate::protocol::Request::Send`], a burst of bytes — and four ways of
//! *spelling* the bytes. This module is the spelling; nothing here talks to a
//! session, which is why it lives in the policy layer and needs no pty to test.
//!
//! **The notation is Emacs's `kbd`, deliberately**: `C-c`, `M-x`, `C-M-x`,
//! `<up>`, `RET`, `TAB`, `SPC`, `ESC`, `DEL`, `<f1>`. Two deliberate
//! divergences, both toward permissiveness and both matching what `kbd` does:
//!
//! - **Angle brackets are optional.** `up` and `<up>` are the same key here.
//! - **Modifier order is free.** `C-M-x` is canonical, `M-C-x` is accepted.
//!
//! The *byte* values are not Emacs's — they are xterm's, which is what a pty on
//! the other end expects. ⚠ The one most often got wrong, where the two agree
//! anyway: **Backspace sends 127 (DEL), not 8 (BS).**

/// The bytes a terminal sends for a key, or `None` if we do not know the name.
/// `None` rather than an empty vector on purpose — every surface must turn an
/// unknown key into a visible refusal, never into a silent no-op.
pub fn key(spec: &str) -> Option<Vec<u8>> {
    let (ctrl, alt, base) = modifiers(spec);
    if base.is_empty() {
        return None;
    }

    if let Some(named) = named(base) {
        return modified(named, ctrl, alt);
    }

    let mut chars = base.chars();
    let one = chars.next()?;
    if chars.next().is_some() {
        // More than one character and not a name we know. Better to refuse than
        // to send the first letter of a typo.
        return None;
    }

    let mut buf = [0u8; 4];
    let mut bytes = if ctrl {
        vec![control_byte(one)?]
    } else {
        one.encode_utf8(&mut buf).as_bytes().to_vec()
    };
    // Meta is an ESC prefix. This is the only place it applies to a plain
    // character; a named key's Meta is folded into its CSI parameter instead,
    // inside `modified`. Deciding it in one place per branch rather than once
    // at the end is what keeps `M-ESC` from collapsing into `ESC` — the earlier
    // draft skipped the prefix for anything already starting with 0x1b, which
    // is right for a CSI sequence and wrong for the Escape key itself.
    if alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// Strip leading `C-` / `M-` prefixes, in any order and any number.
fn modifiers(spec: &str) -> (bool, bool, &str) {
    let (mut ctrl, mut alt) = (false, false);
    let mut rest = spec;
    loop {
        match rest.split_at_checked(2) {
            Some(("C-", tail)) => {
                ctrl = true;
                rest = tail;
            }
            Some(("M-", tail)) => {
                alt = true;
                rest = tail;
            }
            _ => return (ctrl, alt, rest),
        }
    }
}

/// The escape sequence for a named key, unmodified. Angle brackets optional.
fn named(spec: &str) -> Option<&'static [u8]> {
    let name = spec.trim_start_matches('<').trim_end_matches('>');
    Some(match name {
        // The seven Emacs spells as bare shorthands, plus the lower-case event
        // names it uses for the same keys in symbol form.
        "NUL" => b"\0",
        "RET" | "return" | "enter" => b"\r",
        "LFD" | "linefeed" => b"\n",
        "TAB" | "tab" => b"\t",
        "SPC" | "space" => b" ",
        "ESC" | "escape" => b"\x1b",
        // What the Backspace key actually sends on a modern terminal. Emacs
        // spells this DEL for the same historical reason.
        "DEL" | "backspace" => b"\x7f",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "insert" => b"\x1b[2~",
        "delete" => b"\x1b[3~",
        "prior" | "pageup" => b"\x1b[5~",
        "next" | "pagedown" => b"\x1b[6~",
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        "f5" => b"\x1b[15~",
        "f6" => b"\x1b[17~",
        "f7" => b"\x1b[18~",
        "f8" => b"\x1b[19~",
        "f9" => b"\x1b[20~",
        "f10" => b"\x1b[21~",
        "f11" => b"\x1b[23~",
        "f12" => b"\x1b[24~",
        _ => return None,
    })
}

/// Fold Ctrl/Meta into a named key's sequence, xterm-style: modifiers ride as a
/// CSI parameter, code `1 + 1·shift + 2·alt + 4·ctrl`. F1–F4 arrive in SS3 form
/// (`ESC O P`), which has no parameter slot, so a modified one is rewritten CSI.
fn modified(base: &'static [u8], ctrl: bool, alt: bool) -> Option<Vec<u8>> {
    if !ctrl && !alt {
        return Some(base.to_vec());
    }
    let code = 1 + u8::from(alt) * 2 + u8::from(ctrl) * 4;

    match base {
        // A single control character (RET, TAB, ESC, DEL, SPC) has no CSI form.
        // Ctrl on those is the control-byte arithmetic, not a parameter.
        [byte] => {
            let one = char::from(*byte);
            let mut out = vec![if ctrl { control_byte(one)? } else { *byte }];
            if alt {
                out.insert(0, 0x1b);
            }
            Some(out)
        }
        // ESC O P … → ESC [ 1 ; code P
        [0x1b, b'O', final_byte] => {
            Some(format!("\x1b[1;{code}{}", char::from(*final_byte)).into())
        }
        // ESC [ 2 ~ → ESC [ 2 ; code ~   |   ESC [ A → ESC [ 1 ; code A
        [0x1b, b'[', rest @ ..] => {
            let (final_byte, params) = rest.split_last()?;
            let params = if params.is_empty() { b"1" } else { params };
            let params = core::str::from_utf8(params).ok()?;
            Some(format!("\x1b[{params};{code}{}", char::from(*final_byte)).into())
        }
        _ => None,
    }
}

/// The byte Ctrl produces for a character, or `None` where Ctrl means nothing.
fn control_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 & 0x1f),
        'A'..='Z' => Some(c.to_ascii_lowercase() as u8 & 0x1f),
        '@' | ' ' => Some(0),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// An SGR mouse report for a click at a 1-based screen cell. A click is press
/// *and* release in one burst; a wheel event has no release. Caution: we only
/// encode — enabling mouse reporting stays the program's own configuration.
pub fn mouse(button: &str, col: u16, row: u16) -> Option<Vec<u8>> {
    // Zero would be off the screen in a 1-based protocol; treating it as cell 1
    // would silently click somewhere the caller did not ask for.
    if col == 0 || row == 0 {
        return None;
    }
    let (code, wheel) = match button {
        "left" => (0, false),
        "middle" => (1, false),
        "right" => (2, false),
        "wheel-up" => (64, true),
        "wheel-down" => (65, true),
        _ => return None,
    };
    let mut report = format!("\x1b[<{code};{col};{row}M").into_bytes();
    if !wheel {
        report.extend_from_slice(format!("\x1b[<{code};{col};{row}m").as_bytes());
    }
    Some(report)
}
