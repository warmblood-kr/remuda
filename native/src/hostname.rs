//! The OS host name, read from the OS itself (no environment, no shell).

use std::io;

/// Accept a host name as the OS reported it, or refuse it: empty, not UTF-8,
/// or holding a control, line-separator or bidi-control character.
/// Everything else passes through unchanged.
pub fn validate(name: &[u8]) -> io::Result<String> {
    let refuse = |why| io::Error::new(io::ErrorKind::InvalidData, why);
    let name = std::str::from_utf8(name).map_err(|_| refuse("the host name is not UTF-8"))?;
    if name.is_empty() {
        return Err(refuse("the host name is empty"));
    }
    // Cc, then U+2028/U+2029 and every Bidi_Control character.
    let unsafe_to_show = |c: char| {
        c.is_control()
            || matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}')
            || matches!(c, '\u{2028}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
    };
    if name.chars().any(unsafe_to_show) {
        return Err(refuse(
            "the host name holds a control, line-separator or bidi-control character",
        ));
    }
    Ok(name.to_owned())
}

/// The host name the OS reports, validated by [`validate`].
#[cfg(unix)]
pub fn hostname() -> io::Result<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the pointer and length describe this writable buffer.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    validate(&buffer[..end])
}

/// The DNS host name the OS reports, validated by [`validate`].
#[cfg(windows)]
pub fn hostname() -> io::Result<String> {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNameDnsHostname, GetComputerNameExW,
    };

    let mut buffer = [0u16; 256];
    let mut length = buffer.len() as u32;
    // SAFETY: the pointer and `length` describe this writable buffer.
    let ok =
        unsafe { GetComputerNameExW(ComputerNameDnsHostname, buffer.as_mut_ptr(), &mut length) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let name = String::from_utf16(&buffer[..length as usize])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the host name is not UTF-16"))?;
    validate(name.as_bytes())
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
        // Not category Cc, but they split or reorder what a terminal shows.
        let marks = [0x061C, 0x200E, 0x200F];
        for unit in (0x2028..=0x202E).chain(0x2066..=0x2069).chain(marks) {
            let name = format!("a{}b", char::from_u32(unit).unwrap());
            assert!(validate(name.as_bytes()).is_err(), "accepted U+{unit:04X}");
        }
        // U+200D is the joiner real text uses; it and the other neighbours pass.
        for neighbour in [
            '\u{061B}', '\u{061D}', '\u{200C}', '\u{200D}', '\u{2010}', '\u{2027}', '\u{202F}',
            '\u{2065}', '\u{206A}',
        ] {
            let name = format!("a{neighbour}b");
            assert!(validate(name.as_bytes()).is_ok(), "refused {neighbour:?}");
        }
    }
}
