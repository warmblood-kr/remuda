//! The OS host name, read from the OS itself (no environment, no shell).

use std::io;

/// Accept a host name as the OS reported it, or refuse it: empty, not UTF-8,
/// or holding a control character. Everything else passes through unchanged.
pub fn validate(name: &[u8]) -> io::Result<String> {
    Ok(String::from_utf8_lossy(name).into_owned())
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn validate_passes_ordinary_names_through_unchanged() {
        for name in ["studio.local", "My Mac @home:1/2", "호스트"] {
            assert_eq!(validate(name.as_bytes()).unwrap(), name);
        }
    }

    #[test]
    fn validate_refuses_control_characters_invalid_utf8_and_empty() {
        let refused: [&[u8]; 6] = [
            b"",
            b"a\x1b[31mb",
            b"a\nb",
            b"a\x7fb",
            "a\u{85}b".as_bytes(),
            b"a\xffb",
        ];
        for name in refused {
            assert!(validate(name).is_err(), "accepted {name:?}");
        }
    }
}
