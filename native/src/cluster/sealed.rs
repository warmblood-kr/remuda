//! Credentials sealed at rest under a per-node key (#746).
//!
//! The node key is 32 random bytes in `<state>/remuda/keys/node.key`, an
//! owner-only file in its own owner-only directory, apart from the credential
//! files under `<state>/remuda/cluster/`. A sealed file is XChaCha20-Poly1305
//! with a fresh random 24-byte nonce on every write. The daemon reads both
//! files directly, so no read ever prompts. See docs/credential-encryption.md.

use super::storage;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};
use std::fs;
#[cfg(not(windows))]
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

pub(super) const NODE_KEY_DIR: &str = "keys";
pub(super) const NODE_KEY_FILE: &str = "node.key";

const NODE_KEY_MAGIC: &[u8; 8] = b"RMDNKEY1";
const NODE_KEY_LEN: usize = 32;
const NODE_KEY_FILE_LEN: usize = NODE_KEY_MAGIC.len() + NODE_KEY_LEN;

const SEALED_MAGIC: &[u8; 8] = b"RMDSEAL1";
const KEY_ID_LEN: usize = 8;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = SEALED_MAGIC.len() + KEY_ID_LEN + NONCE_LEN;

/// Whether a write found the node key or had to create it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NodeKeyState {
    Existing,
    Created,
}

/// The per-node key that seals this node's credentials.
pub(super) struct NodeKey {
    key: Zeroizing<[u8; NODE_KEY_LEN]>,
    id: [u8; KEY_ID_LEN],
}

impl NodeKey {
    fn from_bytes(key: Zeroizing<[u8; NODE_KEY_LEN]>) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"remuda node key id v1\0");
        hasher.update(&key[..]);
        let digest = hasher.finalize();
        let mut id = [0; KEY_ID_LEN];
        id.copy_from_slice(&digest[..KEY_ID_LEN]);
        Self { key, id }
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(&self.key[..]))
    }
}

/// The node key file that belongs to the cluster directory `cluster_dir`.
pub(super) fn node_key_path(cluster_dir: &Path) -> PathBuf {
    node_key_dir(cluster_dir).join(NODE_KEY_FILE)
}

fn node_key_dir(cluster_dir: &Path) -> PathBuf {
    cluster_dir
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(NODE_KEY_DIR)
}

/// Load the node key, or `None` when it has never been created.
pub(super) fn load_node_key(cluster_dir: &Path) -> io::Result<Option<NodeKey>> {
    let dir = node_key_dir(cluster_dir);
    match fs::symlink_metadata(&dir) {
        Ok(_) => storage::verify_directory(&dir)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let path = dir.join(NODE_KEY_FILE);
    let bytes = match read_private_file(&path, "node key", NODE_KEY_FILE_LEN) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if bytes.len() != NODE_KEY_FILE_LEN || &bytes[..NODE_KEY_MAGIC.len()] != NODE_KEY_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("node key {} is not a valid remuda node key", path.display()),
        ));
    }
    let mut key = Zeroizing::new([0; NODE_KEY_LEN]);
    key.copy_from_slice(&bytes[NODE_KEY_MAGIC.len()..]);
    Ok(Some(NodeKey::from_bytes(key)))
}

/// Load the node key, creating it first if it does not exist. The caller
/// holds the cluster lock, so two writers never race to create it. Returns
/// the key and whether this call created it.
pub(super) fn load_or_create_node_key(cluster_dir: &Path) -> io::Result<(NodeKey, bool)> {
    if let Some(key) = load_node_key(cluster_dir)? {
        return Ok((key, false));
    }
    let dir = node_key_dir(cluster_dir);
    storage::create_private_directory(&dir)?;
    storage::verify_directory(&dir)?;
    let mut key = Zeroizing::new([0; NODE_KEY_LEN]);
    getrandom::fill(&mut key[..]).map_err(|error| {
        io::Error::other(format!("secure node key randomness unavailable: {error}"))
    })?;
    let mut file = Zeroizing::new(Vec::with_capacity(NODE_KEY_FILE_LEN));
    file.extend_from_slice(NODE_KEY_MAGIC);
    file.extend_from_slice(&key[..]);
    storage::atomic_write(&dir.join(NODE_KEY_FILE), &file)?;
    Ok((NodeKey::from_bytes(key), true))
}

/// Whether `bytes` is in the sealed format, rather than legacy plaintext.
pub(super) fn is_sealed(bytes: &[u8]) -> bool {
    bytes.starts_with(SEALED_MAGIC)
}

/// The size of a sealed file holding `plaintext_len` bytes.
pub(super) const fn sealed_len(plaintext_len: usize) -> usize {
    HEADER_LEN + plaintext_len + TAG_LEN
}

fn associated_data(header: &[u8], label: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(header.len() + label.len());
    aad.extend_from_slice(header);
    aad.extend_from_slice(label.as_bytes());
    aad
}

/// Encrypt `plaintext` for the credential slot `label` with a fresh nonce.
/// The label is authenticated, so a sealed file cannot be moved to another slot.
pub(super) fn seal(key: &NodeKey, label: &str, plaintext: &[u8]) -> io::Result<Vec<u8>> {
    let mut nonce = [0; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|error| {
        io::Error::other(format!("secure nonce randomness unavailable: {error}"))
    })?;
    let mut sealed = Vec::with_capacity(sealed_len(plaintext.len()));
    sealed.extend_from_slice(SEALED_MAGIC);
    sealed.extend_from_slice(&key.id);
    sealed.extend_from_slice(&nonce);
    let aad = associated_data(&sealed, label);
    let ciphertext = key
        .cipher()
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| io::Error::other("credential encryption failed"))?;
    sealed.extend_from_slice(&ciphertext);
    Ok(sealed)
}

/// Decrypt and authenticate a sealed credential. Never returns unverified bytes.
pub(super) fn open(
    key: &NodeKey,
    label: &str,
    sealed: &[u8],
    path: &Path,
) -> io::Result<Zeroizing<Vec<u8>>> {
    if !is_sealed(sealed) || sealed.len() < HEADER_LEN + TAG_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a sealed remuda credential", path.display()),
        ));
    }
    let (header, ciphertext) = sealed.split_at(HEADER_LEN);
    let key_id = &header[SEALED_MAGIC.len()..SEALED_MAGIC.len() + KEY_ID_LEN];
    if key_id != key.id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} was encrypted with a different node key than this node's; it cannot be read here",
                path.display()
            ),
        ));
    }
    let nonce = &header[SEALED_MAGIC.len() + KEY_ID_LEN..];
    let aad = associated_data(header, label);
    key.cipher()
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} failed authentication: it was modified or corrupted",
                    path.display()
                ),
            )
        })
}

/// Read an owner-only regular file of at most `max_len` bytes, refusing
/// symlinks and loose permissions like every other cluster state file.
pub(super) fn read_private_file(
    path: &Path,
    description: &str,
    max_len: usize,
) -> io::Result<Zeroizing<Vec<u8>>> {
    let not_found = |error: io::Error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                error.kind(),
                format!("{description} {} not found", path.display()),
            )
        } else {
            error
        }
    };
    #[cfg(windows)]
    let file = {
        let file = super::windows_security::open_for_read(path).map_err(not_found)?;
        storage::check_private_file(&file, description, path)?;
        file
    };
    #[cfg(not(windows))]
    let file = {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path).map_err(|error| {
            #[cfg(unix)]
            if error.raw_os_error() == Some(libc::ELOOP) {
                return io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{} is a symlink; refusing (remove it to re-initialize)",
                        path.display()
                    ),
                );
            }
            not_found(error)
        })?;
        storage::check_private_file(&file, description, path)?;
        file
    };
    let mut bytes = Zeroizing::new(Vec::with_capacity(max_len));
    file.take(max_len as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{description} {} is larger than {max_len} bytes",
                path.display()
            ),
        ));
    }
    Ok(bytes)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn cluster_dir() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "remuda-sealed-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root.join("cluster")
    }

    #[test]
    fn round_trip_returns_the_plaintext_and_hides_it_on_disk() {
        let dir = cluster_dir();
        let (key, created) = load_or_create_node_key(&dir).unwrap();
        assert!(created);
        let secret = b"a very secret 64-byte identity, padded out to sixty-four bytes!!";
        let sealed = seal(&key, "cluster/identity.key", secret).unwrap();
        assert_eq!(sealed.len(), sealed_len(secret.len()));
        assert!(is_sealed(&sealed));
        assert!(!sealed
            .windows(16)
            .any(|window| secret.windows(16).any(|s| s == window)));
        let opened = open(&key, "cluster/identity.key", &sealed, Path::new("x")).unwrap();
        assert_eq!(&opened[..], &secret[..]);
    }

    #[test]
    fn every_seal_uses_a_fresh_nonce() {
        let dir = cluster_dir();
        let (key, _) = load_or_create_node_key(&dir).unwrap();
        let first = seal(&key, "slot", b"same").unwrap();
        let second = seal(&key, "slot", b"same").unwrap();
        assert_ne!(first[..HEADER_LEN], second[..HEADER_LEN]);
        assert_ne!(first, second);
    }

    #[test]
    fn any_flipped_byte_is_detected() {
        let dir = cluster_dir();
        let (key, _) = load_or_create_node_key(&dir).unwrap();
        let sealed = seal(&key, "slot", b"secret material").unwrap();
        // Key-id bytes report a different key; every other byte fails authentication.
        for index in SEALED_MAGIC.len()..sealed.len() {
            let mut tampered = sealed.clone();
            tampered[index] ^= 0x01;
            let error = open(&key, "slot", &tampered, Path::new("x")).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "byte {index}");
        }
        let error = open(&key, "slot", &sealed[..sealed.len() - 1], Path::new("x")).unwrap_err();
        assert!(
            error.to_string().contains("failed authentication"),
            "{error}"
        );
    }

    #[test]
    fn a_sealed_file_cannot_move_to_another_slot() {
        let dir = cluster_dir();
        let (key, _) = load_or_create_node_key(&dir).unwrap();
        let sealed = seal(&key, "slot-a", b"secret").unwrap();
        let error = open(&key, "slot-b", &sealed, Path::new("x")).unwrap_err();
        assert!(
            error.to_string().contains("failed authentication"),
            "{error}"
        );
    }

    #[test]
    fn another_nodes_key_cannot_open_it() {
        let (ours, _) = load_or_create_node_key(&cluster_dir()).unwrap();
        let (theirs, _) = load_or_create_node_key(&cluster_dir()).unwrap();
        let sealed = seal(&ours, "slot", b"secret").unwrap();
        let error = open(&theirs, "slot", &sealed, Path::new("x")).unwrap_err();
        assert!(error.to_string().contains("different node key"), "{error}");
    }

    #[test]
    fn node_key_is_owner_only_stable_and_apart_from_the_cluster_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = cluster_dir();
        let (first, created) = load_or_create_node_key(&dir).unwrap();
        let (second, created_again) = load_or_create_node_key(&dir).unwrap();
        assert!(created && !created_again);
        assert_eq!(first.id, second.id);
        let path = node_key_path(&dir);
        assert!(!path.starts_with(&dir));
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(fs::read(&path).unwrap().len(), NODE_KEY_FILE_LEN);
    }

    #[test]
    fn a_loose_node_key_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = cluster_dir();
        load_or_create_node_key(&dir).unwrap();
        fs::set_permissions(node_key_path(&dir), fs::Permissions::from_mode(0o644)).unwrap();
        let error = load_node_key(&dir).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("chmod 600"), "{error}");
    }
}
