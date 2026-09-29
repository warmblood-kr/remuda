//! Text that is safe to write directly to a terminal.

use std::borrow::Cow;

/// Remove control characters from untrusted text before terminal output.
/// Covers C0, C1, and DEL (including ESC) while preserving Unicode.
pub fn strip_terminal_controls(text: &str) -> Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .filter(|character| !character.is_control())
            .collect(),
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
}
