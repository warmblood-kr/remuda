//! Key and mouse spellings, checked against the bytes a terminal really sends.
//!
//! These are pure functions, so this suite needs no pty and no process — which
//! is the point of putting the encoders in the policy layer. Whether the bytes
//! then *arrive* is `native/tests/live_sessions.rs`; whether they are the right
//! bytes is here.

use remuda_core::keys::{key, mouse};

fn bytes(spec: &str) -> Vec<u8> {
    key(spec).unwrap_or_else(|| panic!("{spec:?} should be a key we know"))
}

#[test]
fn a_plain_character_is_itself() {
    assert_eq!(bytes("a"), b"a");
    assert_eq!(bytes("Z"), b"Z");
    assert_eq!(bytes("7"), b"7");
    // Not ASCII-only: a session is a UTF-8 terminal.
    assert_eq!(bytes("가"), "가".as_bytes());
}

#[test]
fn control_is_the_low_five_bits() {
    assert_eq!(bytes("C-c"), [0x03], "the interrupt every operator knows");
    assert_eq!(bytes("C-d"), [0x04]);
    assert_eq!(bytes("C-a"), [0x01]);
    // Case is not a third modifier: C-A and C-a are the same wire byte, which
    // is what a terminal does.
    assert_eq!(bytes("C-A"), bytes("C-a"));
    assert_eq!(bytes("C-SPC"), [0x00], "NUL, the classic set-mark");
    assert_eq!(
        bytes("C-["),
        [0x1b],
        "which is why Escape and C-[ are one key"
    );
}

#[test]
fn meta_is_an_escape_prefix() {
    assert_eq!(bytes("M-x"), [0x1b, b'x']);
    assert_eq!(bytes("C-M-x"), [0x1b, 0x18]);
    // Emacs's canonical order is alphabetical (`A-C-H-M-S-s`, node *Changing
    // Key Bindings*), so `C-M-x` is the spelling to write. We accept the other
    // order, as `kbd` does and as Emacs's own *Function Keys* node permits.
    assert_eq!(bytes("M-C-x"), bytes("C-M-x"));

    // The case an earlier draft got wrong: skipping the ESC prefix for anything
    // already beginning with 0x1b is correct for a CSI sequence and wrong for
    // the Escape key itself, which would have collapsed M-ESC into ESC.
    assert_eq!(bytes("M-ESC"), [0x1b, 0x1b]);
}

#[test]
fn all_seven_emacs_shorthands_are_spellable() {
    // Not just the four an author reaches for first. Read from the manual, node
    // *Changing Key Bindings*: NUL RET TAB LFD ESC SPC DEL.
    assert_eq!(bytes("NUL"), [0x00]);
    assert_eq!(bytes("RET"), b"\r", "the same CR send_line appends");
    assert_eq!(bytes("TAB"), b"\t");
    assert_eq!(bytes("LFD"), b"\n", "and LFD is not RET");
    assert_eq!(bytes("ESC"), [0x1b]);
    assert_eq!(bytes("SPC"), b" ");
    assert_eq!(bytes("DEL"), b"\x7f", "what Backspace actually sends");
}

#[test]
fn named_keys_carry_their_escape_sequences() {
    assert_eq!(bytes("up"), b"\x1b[A");
    assert_eq!(bytes("<up>"), b"\x1b[A", "angle brackets are optional");
    assert_eq!(bytes("down"), b"\x1b[B");
    assert_eq!(
        bytes("backspace"),
        bytes("DEL"),
        "elisp.info, Function Keys: `backspace` converts into 127, not 8"
    );
    assert_eq!(bytes("delete"), b"\x1b[3~", "and Delete is not Backspace");
    assert_eq!(bytes("f1"), b"\x1bOP", "F1..F4 arrive in SS3 form");
    assert_eq!(bytes("f5"), b"\x1b[15~", "F5 up is CSI, and 16 is skipped");
}

#[test]
fn a_modified_named_key_becomes_a_csi_parameter() {
    // xterm's encoding: 1 + 1·shift + 2·alt + 4·ctrl.
    assert_eq!(bytes("C-up"), b"\x1b[1;5A");
    assert_eq!(bytes("M-up"), b"\x1b[1;3A");
    assert_eq!(bytes("C-M-up"), b"\x1b[1;7A");
    assert_eq!(bytes("C-delete"), b"\x1b[3;5~", "the parameter is kept");
    assert_eq!(bytes("C-f1"), b"\x1b[1;5P", "SS3 is rewritten as CSI");

    // A single control character has no CSI form, so its modifier is the
    // control-byte arithmetic instead of a parameter.
    assert_eq!(bytes("M-RET"), [0x1b, b'\r']);
}

#[test]
fn an_unknown_key_is_refused_rather_than_encoded_as_nothing() {
    // The point of `Option` here. A caller that read "unknown" as "send an
    // empty burst" would produce a script that silently presses nothing —
    // the empty-success failure this repo keeps finding, one layer lower.
    assert_eq!(key("nosuchkey"), None);
    assert_eq!(key(""), None);
    assert_eq!(key("C-"), None, "a modifier with nothing under it");
    assert_eq!(key("C-1"), None, "Ctrl on a digit has no agreed byte");
    assert_eq!(key("C-ESC"), None, "and none on Escape either");
    assert_eq!(key("hello"), None, "not the first letter of a typo");
}

#[test]
fn a_click_is_a_press_and_its_release() {
    // A program that sees only the press waits forever for the button to come
    // up, so both halves go in one indivisible burst.
    assert_eq!(
        mouse("left", 40, 12).unwrap(),
        b"\x1b[<0;40;12M\x1b[<0;40;12m"
    );
    assert_eq!(mouse("right", 1, 1).unwrap(), b"\x1b[<2;1;1M\x1b[<2;1;1m");
}

#[test]
fn a_wheel_event_has_no_release() {
    assert_eq!(mouse("wheel-up", 5, 5).unwrap(), b"\x1b[<64;5;5M");
    assert_eq!(mouse("wheel-down", 5, 5).unwrap(), b"\x1b[<65;5;5M");
}

#[test]
fn an_impossible_click_is_refused() {
    assert_eq!(mouse("elbow", 1, 1), None, "not a button");
    // 1-based protocol: rounding 0 up to cell 1 would click somewhere the
    // caller did not ask for, which is worse than refusing.
    assert_eq!(mouse("left", 0, 5), None);
    assert_eq!(mouse("left", 5, 0), None);
}
