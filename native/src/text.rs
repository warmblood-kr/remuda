//! Text that is safe to write directly to a terminal.

use std::borrow::Cow;

/// Remove control characters and bidi formatting controls from untrusted text
/// before terminal output. Covers C0, C1, DEL (including ESC), and the Unicode
/// bidi control characters while preserving other Unicode.
pub fn strip_terminal_controls(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_terminal_control) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .filter(|character| !is_terminal_control(*character))
            .collect(),
    )
}

fn is_terminal_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{206f}'
        )
}

#[cfg(test)]
mod tests {
    use super::strip_terminal_controls;

    #[test]
    fn removes_c0_c1_del_and_osc_payload_controls() {
        let name = "x\u{1b}]0;pwn\u{7}y";
        let screen = "a\u{1b}]52;c;aGk=\u{7}b\u{009b}31mred\u{009c}";
        assert_eq!(strip_terminal_controls(name), "x]0;pwny");
        assert_eq!(strip_terminal_controls(screen), "a]52;c;aGk=b31mred");
    }

    #[test]
    fn removes_bidi_formatting_controls() {
        assert_eq!(
            strip_terminal_controls("safe\u{202e}txt\u{2066}אבג\u{2069}"),
            "safetxtאבג"
        );
    }
}
