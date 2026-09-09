//! Keystrokes and mouse reports, as the bytes a terminal actually sends.
//!
//! 정수님, 2026-09-10: *"pty에 글자 입력, 문자열 입력, 안시코드 입력, 마우스 입력
//! … 여러 기본함수들을 제공할 필요가 있겠습니다."*
//!
//! Those four are not four operations. They are one operation —
//! [`crate::protocol::Request::Send`], a burst of bytes — and four ways of
//! *spelling* the bytes. This module is the spelling; nothing here talks to a
//! session, which is why it lives in the policy layer and needs no pty to test.
//!
//! **The notation is Emacs's `kbd`, deliberately.** `C-c`, `M-x`, `C-M-x`,
//! `<up>`, `RET`, `TAB`, `SPC`, `ESC`, `DEL`, `<f1>`. Inventing a second key
//! notation would mean the person writing a remuda script has to learn one, and
//! the one they already know has been read by more people than any we could
//! design.
//!
//! Read from the manual on this machine rather than from memory
//! (`emacs-30.1/info/elisp.info`, node *Changing Key Bindings*):
//!
//! > Each key stroke is either a single character, or the name of an event,
//! > surrounded by angle brackets. … The only keys that have a special shorthand
//! > syntax are `NUL`, `RET`, `TAB`, `LFD`, `ESC`, `SPC` and `DEL`. … The
//! > modifiers have to be specified in alphabetical order: `A-C-H-M-S-s`.
//!
//! Two deliberate divergences, both toward permissiveness, and both matching
//! what `kbd` itself does — the manual calls it *"very permissive, and will try
//! to return something sensible even if the syntax used isn't completely
//! conforming"*, with `key-valid-p` as the separate strict check:
//!
//! - **Angle brackets are optional.** `up` and `<up>` are the same key here.
//!   remuda has no competing meaning for a bare word, and a script author
//!   should not have to remember which names are shorthands.
//! - **Modifier order is free.** `C-M-x` is the canonical spelling and
//!   `M-C-x` is accepted. Emacs's own *Function Keys* node says the order
//!   "does not matter in arguments to the key-binding lookup and modification
//!   functions", so the two nodes already disagree.
//!
//! The *byte* values are not Emacs's — they are xterm's, which is what a pty on
//! the other end is expecting. One place the two agree, and it is the one most
//! often got wrong: **Backspace sends 127, not 8.** The manual, node *Function
//! Keys*: *"In ASCII, <BS> is really `C-h`. But `backspace` converts into the
//! character code 127 (<DEL>), not into code 8 (<BS>). This is what most users
//! prefer."*

/// The bytes a terminal sends for a key, or `None` if we do not know the name.
///
/// `None` rather than an empty vector on purpose: a caller that treats "unknown
/// key" as "send nothing" produces a script that silently does not press
/// anything, which is the empty-success failure this repo keeps finding. Every
/// surface turns this into a refusal the caller can see.
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

/// Fold Ctrl/Meta into a named key's sequence, xterm-style.
///
/// xterm carries modifiers as a CSI parameter — `ESC [ A` becomes
/// `ESC [ 1 ; 5 A` for Ctrl. The code is `1 + 1·shift + 2·alt + 4·ctrl`.
/// F1–F4 arrive in SS3 form (`ESC O P`) which has nowhere to put a parameter,
/// so a modified one is rewritten into the CSI form the same terminals accept.
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

/// An SGR mouse report for a click at a 1-based screen cell.
///
/// A click is press *and* release, returned as one burst, because that is what
/// a click is — a program that sees only the press waits forever for the
/// button to come up. Wheel events have no release and get one event.
///
/// We encode and send; we do not enable mouse reporting on the program's
/// behalf. Whether it is listening is its own configuration, and turning it on
/// for it would be remuda deciding what the agent's terminal looks like.
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
