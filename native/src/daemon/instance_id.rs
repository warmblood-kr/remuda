//! A daemon-instance id that outlives one daemon process (reload step 2).
//!
//! Kept beside the socket as `<socket>.daemon-id` and read under the socket
//! lock, so every daemon that serves this socket bakes the same
//! `REMUDA_DAEMON_ID` into its sessions. Host-backed sessions survive their
//! daemon; the shutdown guard must still recognize them after a restart.

use std::io::{self, Read as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

const HEX_LEN: usize = 32;

/// `<socket>.daemon-id`, next to the `<socket>.hosts` inventory.
pub(super) fn path_for(socket: &Path) -> PathBuf {
    let mut name = socket.file_name().unwrap_or_default().to_os_string();
    name.push(".daemon-id");
    socket.with_file_name(name)
}

/// The id stored for `socket`. A missing, malformed or symlinked file is
/// replaced by a fresh random id written owner-only.
pub(super) fn load_or_create(socket: &Path) -> io::Result<String> {
    let path = path_for(socket);
    if let Some(id) = read_valid(&path)? {
        return Ok(id);
    }
    let mut bytes = [0u8; HEX_LEN / 2];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(format!("entropy: {error}")))?;
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    crate::fs_atomic::write_atomic_exact_mode(&path, id.as_bytes(), 0o600)?;
    Ok(id)
}

fn read_valid(path: &Path) -> io::Result<Option<String>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path);
    let file = match file {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut text = String::new();
    file.take(HEX_LEN as u64 + 2).read_to_string(&mut text).ok();
    let id = text.trim_end();
    let valid = id.len() == HEX_LEN && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    Ok(valid.then(|| id.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{load_or_create, path_for};
    use std::os::unix::fs::PermissionsExt as _;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("remuda-daemon-id-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_same_socket_keeps_its_id_and_another_socket_gets_its_own() {
        let dir = scratch("stable");
        let socket = dir.join("s.sock");
        let first = load_or_create(&socket).unwrap();
        assert_eq!(first.len(), 32);
        assert_eq!(
            load_or_create(&socket).unwrap(),
            first,
            "a second daemon reads the same id"
        );
        assert_ne!(load_or_create(&dir.join("t.sock")).unwrap(), first);
        let mode = std::fs::metadata(path_for(&socket))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_or_symlinked_id_file_is_replaced_not_trusted() {
        let dir = scratch("malformed");
        let socket = dir.join("s.sock");
        std::fs::write(path_for(&socket), "12345").unwrap();
        let replaced = load_or_create(&socket).unwrap();
        assert_ne!(replaced, "12345");
        assert_eq!(load_or_create(&socket).unwrap(), replaced);

        let target = dir.join("elsewhere");
        std::fs::write(&target, "0123456789abcdef0123456789abcdef").unwrap();
        std::fs::remove_file(path_for(&socket)).unwrap();
        std::os::unix::fs::symlink(&target, path_for(&socket)).unwrap();
        let fresh = load_or_create(&socket).unwrap();
        assert_ne!(
            fresh, "0123456789abcdef0123456789abcdef",
            "a symlink is not followed"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "0123456789abcdef0123456789abcdef",
            "the symlink target is not written through"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
