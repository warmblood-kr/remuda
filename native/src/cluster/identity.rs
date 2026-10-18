//! Node identity: Snow key generation, secure local persistence, and fingerprints.

use super::encoding;
use super::sealed;
use super::storage;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

const NOISE_PATTERN: &str = "Noise_NN_25519_ChaChaPoly_SHA256";
const IDENTITY_FILE: &str = "identity.key";
const IDENTITY_LEN: usize = 64;
// Authenticated with the sealed file, so it cannot be swapped into another slot.
const IDENTITY_LABEL: &str = "cluster/identity.key";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeIdentity {
    pub node_name: String,
    pub node_fp: String,
    pub static_pubkey: Vec<u8>,
}

pub(super) fn prepare_cluster_dir_at(state_dir: &Path) -> io::Result<std::path::PathBuf> {
    let dir = state_dir.join("cluster");
    match fs::symlink_metadata(&dir) {
        Ok(_) => {
            check_identity_path(&dir)?;
            storage::verify_directory(&dir)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            storage::create_private_directory(&dir)?;
            storage::verify_directory(&dir)?;
        }
        Err(e) => return Err(e),
    }
    Ok(dir)
}

pub(super) fn check_identity_path(dir: &Path) -> io::Result<()> {
    storage::check_directory_type(dir)?;
    let path = dir.join(IDENTITY_FILE);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is a symlink; refusing (remove it to re-initialize)",
                path.display()
            ),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(all(test, unix))]
pub(super) fn init_identity_at(dir: &Path) -> io::Result<(NodeIdentity, bool)> {
    match fs::symlink_metadata(dir) {
        Ok(_) => {
            check_identity_path(dir)?;
            storage::verify_directory(dir)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            storage::create_private_directory(dir)?;
            storage::verify_directory(dir)?;
        }
        Err(e) => return Err(e),
    }
    let _guard = storage::StateLock::acquire(dir)?;
    init_identity_locked(dir)
}

pub(super) fn init_identity_locked(dir: &Path) -> io::Result<(NodeIdentity, bool)> {
    {
        check_identity_path(dir)?;
        storage::verify_directory(dir)?;
        let key_path = dir.join(IDENTITY_FILE);
        let (identity, created) = match fs::symlink_metadata(&key_path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{} is a symlink; refusing (remove it to re-initialize)",
                        key_path.display()
                    ),
                ))
            }
            Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => {
                if let Some(migrated) = migrate_identity_locked(dir)? {
                    eprintln!("remuda: {}", super::describe_migrated_credential(&migrated));
                }
                (load_identity_at(dir)?, false)
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cluster identity path must be a regular file, not a symlink",
                ))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let keypair =
                    snow::Builder::new(NOISE_PATTERN.parse().expect("static Noise pattern"))
                        .generate_keypair()
                        .map_err(io::Error::other)?;
                let private = Zeroizing::new(keypair.private);
                let mut material = Zeroizing::new(Vec::with_capacity(IDENTITY_LEN));
                material.extend_from_slice(&private);
                material.extend_from_slice(&keypair.public);
                let identity = identity_from_parts(&material)?;
                write_identity_locked(dir, &material)?;
                (identity, true)
            }
            Err(e) => return Err(e),
        };
        Ok((identity, created))
    }
}

/// Replace an existing node identity while the caller holds the cluster lock.
pub(super) fn rotate_identity_locked(dir: &Path) -> io::Result<NodeIdentity> {
    check_identity_path(dir)?;
    storage::verify_directory(dir)?;
    let key_path = dir.join(IDENTITY_FILE);
    match fs::symlink_metadata(&key_path) {
        Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cluster identity path must be a regular file, not a symlink",
            ))
        }
        Err(error) => return Err(error),
    }
    let keypair = snow::Builder::new(NOISE_PATTERN.parse().expect("static Noise pattern"))
        .generate_keypair()
        .map_err(io::Error::other)?;
    let private = Zeroizing::new(keypair.private);
    let mut material = Zeroizing::new(Vec::with_capacity(IDENTITY_LEN));
    material.extend_from_slice(&private);
    material.extend_from_slice(&keypair.public);
    let identity = identity_from_parts(&material)?;
    write_identity_locked(dir, &material)?;
    Ok(identity)
}

/// Seal the 64-byte identity under the node key (creating the key on first
/// use) and atomically replace `identity.key`. The caller holds the cluster lock.
fn write_identity_locked(dir: &Path, material: &[u8]) -> io::Result<sealed::NodeKeyState> {
    let (key, created) = sealed::load_or_create_node_key(dir)?;
    let file = sealed::seal(&key, IDENTITY_LABEL, material)?;
    storage::atomic_write(&dir.join(IDENTITY_FILE), &file)?;
    Ok(if created {
        sealed::NodeKeyState::Created
    } else {
        sealed::NodeKeyState::Existing
    })
}

/// One plaintext credential re-encrypted in place by [`migrate_identity_locked`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigratedCredential {
    /// The credential file, now sealed; its plaintext content was replaced.
    pub path: PathBuf,
    /// The node key that seals it.
    pub node_key: PathBuf,
    /// Whether the migration created that node key.
    pub node_key_created: bool,
}

/// Re-encrypt a plaintext `identity.key` in place (same fingerprint); the
/// atomic replace removes the plaintext. `None` when absent or already sealed.
/// The caller holds the cluster lock.
pub(super) fn migrate_identity_locked(dir: &Path) -> io::Result<Option<MigratedCredential>> {
    let (material, format) = match read_identity_material_at(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if format == Format::Sealed {
        return Ok(None);
    }
    let state = write_identity_locked(dir, &material[..])?;
    // Read back through the sealed path before reporting success.
    let (reread, format) = read_identity_material_at(dir)?;
    if format != Format::Sealed || reread[..] != material[..] {
        return Err(io::Error::other(
            "cluster identity re-encryption did not read back identically",
        ));
    }
    Ok(Some(MigratedCredential {
        path: dir.join(IDENTITY_FILE),
        node_key: sealed::node_key_path(dir),
        node_key_created: state == sealed::NodeKeyState::Created,
    }))
}

pub(super) fn load_identity_at(dir: &Path) -> io::Result<NodeIdentity> {
    let (bytes, _) = read_identity_material_at(dir)?;
    identity_from_parts(&bytes[..])
}

/// Load the node's Noise static private key from the private cluster store.
/// The returned key is zeroized when dropped and must never be logged.
pub fn load_static_private_key() -> io::Result<Zeroizing<Vec<u8>>> {
    let dir = storage::cluster_state_dir()?.join("cluster");
    let (bytes, _) = read_identity_material_at(&dir)?;
    Ok(Zeroizing::new(bytes[..32].to_vec()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Sealed,
    /// Written before #746: 64 plaintext bytes. Still read, so a node whose
    /// migration has not run yet keeps working; every write seals.
    LegacyPlaintext,
}

/// Load this node's 64-byte identity (32-byte Noise private key, then the
/// public key) from a cluster directory, decrypting it with the node key.
/// For tools and tests that need the raw key; it must never be logged.
pub fn load_keypair_at(cluster_dir: &Path) -> io::Result<Zeroizing<Vec<u8>>> {
    let (material, _) = read_identity_material_at(cluster_dir)?;
    Ok(Zeroizing::new(material.to_vec()))
}

fn read_identity_material_at(dir: &Path) -> io::Result<(Zeroizing<[u8; IDENTITY_LEN]>, Format)> {
    let path = dir.join(IDENTITY_FILE);
    let bytes =
        sealed::read_private_file(&path, "cluster identity", sealed::sealed_len(IDENTITY_LEN))?;
    let (plain, format) = if sealed::is_sealed(&bytes) {
        let Some(key) = sealed::load_node_key(dir)? else {
            // Never NotFound: callers treat that as "not initialized" and
            // could otherwise mint a new identity over this one.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "cluster identity {} is encrypted, but its node key {} is missing \
                     (a state directory copied without its node key cannot be read). \
                     Restore that node key from this machine, or remove {} and run \
                     `remuda cluster init` to start a new identity",
                    path.display(),
                    sealed::node_key_path(dir).display(),
                    dir.display()
                ),
            ));
        };
        (
            sealed::open(&key, IDENTITY_LABEL, &bytes, &path)?,
            Format::Sealed,
        )
    } else {
        (bytes, Format::LegacyPlaintext)
    };
    if plain.len() != IDENTITY_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cluster identity key must be exactly 64 bytes",
        ));
    }
    let mut material = Zeroizing::new([0u8; IDENTITY_LEN]);
    material.copy_from_slice(&plain);
    identity_from_parts(&material[..])?;
    Ok((material, format))
}

fn identity_from_parts(bytes: &[u8]) -> io::Result<NodeIdentity> {
    if bytes.len() != 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cluster identity key",
        ));
    }
    use snow::resolvers::CryptoResolver;
    let params: snow::params::NoiseParams = NOISE_PATTERN.parse().expect("static Noise pattern");
    let resolver = snow::resolvers::DefaultResolver;
    let mut dh = resolver
        .resolve_dh(&params.dh)
        .expect("Snow Curve25519 provider");
    dh.set(&bytes[..32]);
    if dh.pubkey() != &bytes[32..] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cluster identity private and public keys do not match",
        ));
    }
    let static_pubkey = bytes[32..].to_vec();
    let node_fp = encoding::fingerprint(&static_pubkey);
    Ok(NodeIdentity {
        node_name: node_name(&node_fp),
        node_fp,
        static_pubkey,
    })
}

pub(super) fn node_name(fingerprint: &str) -> String {
    let suffix: String = fingerprint
        .strip_prefix("SHA256:")
        .unwrap_or(fingerprint)
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .take(8)
        .map(|byte| char::from(byte).to_ascii_lowercase())
        .collect();
    format!("node-{suffix}")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn temp_dir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "remuda-cluster-identity-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn init_is_idempotent_and_key_is_private_with_stable_fingerprint() {
        let dir = temp_dir().join("cluster");
        let first = init_identity_at(&dir).unwrap();
        let key = fs::read(dir.join(IDENTITY_FILE)).unwrap();
        let second = init_identity_at(&dir).unwrap();
        assert_eq!(first.0, second.0);
        assert!(first.1 && !second.1);
        assert_eq!(key, fs::read(dir.join(IDENTITY_FILE)).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.join(IDENTITY_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn rejects_dangling_identity_symlink_instead_of_regenerating() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir().join("cluster");
        let _ = init_identity_at(&dir).unwrap();
        fs::remove_file(dir.join(IDENTITY_FILE)).unwrap();
        symlink(dir.join("missing-target"), dir.join(IDENTITY_FILE)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let error = init_identity_at(&dir).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error
            .to_string()
            .contains(&dir.join(IDENTITY_FILE).display().to_string()));
        assert!(error.to_string().contains("is a symlink; refusing"));
        assert!(error.to_string().contains("remove it to re-initialize"));
    }

    #[test]
    fn refuses_identity_file_with_loose_permissions() {
        let dir = temp_dir().join("cluster");
        let _ = init_identity_at(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.join(IDENTITY_FILE), fs::Permissions::from_mode(0o644))
                .unwrap();
            let error = load_identity_at(&dir).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(error
                .to_string()
                .contains(&dir.join(IDENTITY_FILE).display().to_string()));
            assert!(error.to_string().contains("chmod 600"));
        }
    }

    #[test]
    fn refuses_a_sealed_identity_with_an_appended_byte() {
        let dir = temp_dir().join("cluster");
        let _ = init_identity_at(&dir).unwrap();
        let path = dir.join(IDENTITY_FILE);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        let error = load_identity_at(&dir).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains(IDENTITY_FILE), "{error}");
    }

    #[test]
    fn refuses_a_tampered_sealed_identity() {
        let dir = temp_dir().join("cluster");
        let _ = init_identity_at(&dir).unwrap();
        let path = dir.join(IDENTITY_FILE);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x80;
        write_mode_600(&path, &bytes);
        let error = load_identity_at(&dir).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("failed authentication"),
            "{error}"
        );
        assert!(load_keypair_at(&dir).is_err());
    }

    #[test]
    fn refuses_a_plaintext_identity_longer_than_64_bytes() {
        let dir = temp_dir().join("cluster");
        let (_, material) = legacy_identity_at(&dir);
        let mut longer = material.to_vec();
        longer.push(b'x');
        write_mode_600(&dir.join(IDENTITY_FILE), &longer);
        let error = load_identity_at(&dir).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exactly 64 bytes"));
    }

    // A pre-#746 node: a private cluster directory holding 64 plaintext bytes.
    fn legacy_identity_at(dir: &Path) -> (NodeIdentity, Zeroizing<Vec<u8>>) {
        let _ = init_identity_at(dir).unwrap();
        let material = load_keypair_at(dir).unwrap();
        write_mode_600(&dir.join(IDENTITY_FILE), &material);
        fs::remove_dir_all(dir.parent().unwrap().join(sealed::NODE_KEY_DIR)).unwrap();
        (identity_from_parts(&material).unwrap(), material)
    }

    fn write_mode_600(path: &Path, bytes: &[u8]) {
        crate::fs_atomic::write_atomic_exact_mode(path, bytes, 0o600).unwrap();
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn a_new_identity_is_sealed_on_disk() {
        let dir = temp_dir().join("cluster");
        let _ = init_identity_at(&dir).unwrap();
        let material = load_keypair_at(&dir).unwrap();
        let on_disk = fs::read(dir.join(IDENTITY_FILE)).unwrap();
        assert!(sealed::is_sealed(&on_disk));
        assert!(
            !contains(&on_disk, &material[..32]),
            "private key in plaintext"
        );
        assert!(sealed::node_key_path(&dir).is_file());
    }

    #[test]
    fn a_sealed_identity_without_its_node_key_is_not_readable_and_not_not_found() {
        let dir = temp_dir().join("cluster");
        let _ = init_identity_at(&dir).unwrap();
        fs::remove_file(sealed::node_key_path(&dir)).unwrap();
        let error = load_identity_at(&dir).unwrap_err();
        // NotFound would make callers mint a new identity over this one.
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("node key"), "{error}");
        assert!(error.to_string().contains("is missing"), "{error}");
        let error = init_identity_at(&dir).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn migration_seals_a_plaintext_identity_once_and_keeps_its_fingerprint() {
        let dir = temp_dir().join("cluster");
        let (before, material) = legacy_identity_at(&dir);
        // Reads keep working before the migration runs.
        assert_eq!(load_identity_at(&dir).unwrap(), before);

        let migrated = migrate_identity_locked(&dir).unwrap().expect("migrated");
        assert_eq!(migrated.path, dir.join(IDENTITY_FILE));
        assert_eq!(migrated.node_key, sealed::node_key_path(&dir));
        assert!(migrated.node_key_created);

        let on_disk = fs::read(dir.join(IDENTITY_FILE)).unwrap();
        assert!(sealed::is_sealed(&on_disk));
        assert!(!contains(&on_disk, &material[..32]), "plaintext copy kept");
        assert_eq!(load_identity_at(&dir).unwrap(), before);
        assert_eq!(&load_keypair_at(&dir).unwrap()[..], &material[..]);
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(IDENTITY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // No temporary or backup copy of the plaintext is left beside it.
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        for name in &names {
            let bytes = fs::read(dir.join(name)).unwrap_or_default();
            assert!(
                !contains(&bytes, &material[..32]),
                "{name} holds the plaintext"
            );
        }

        assert_eq!(
            migrate_identity_locked(&dir).unwrap(),
            None,
            "second run is a no-op"
        );
    }

    #[test]
    fn init_migrates_a_plaintext_identity() {
        let dir = temp_dir().join("cluster");
        let (before, _) = legacy_identity_at(&dir);
        let (after, created) = init_identity_at(&dir).unwrap();
        assert!(!created);
        assert_eq!(after, before);
        assert!(sealed::is_sealed(
            &fs::read(dir.join(IDENTITY_FILE)).unwrap()
        ));
    }

    #[test]
    fn rotation_seals_the_new_identity_under_the_same_node_key() {
        let dir = temp_dir().join("cluster");
        let (first, _) = init_identity_at(&dir).unwrap();
        let node_key = fs::read(sealed::node_key_path(&dir)).unwrap();
        let second = rotate_identity_locked(&dir).unwrap();
        assert_ne!(first, second);
        assert_eq!(load_identity_at(&dir).unwrap(), second);
        assert_eq!(fs::read(sealed::node_key_path(&dir)).unwrap(), node_key);
    }

    #[test]
    fn directory_permission_error_names_path_and_chmod_fix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir().join("cluster");
        let _ = init_identity_at(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let error = storage::verify_directory(&dir).unwrap_err();
        assert!(error.to_string().contains(&dir.display().to_string()));
        assert!(error.to_string().contains("chmod 700"));
    }

    #[test]
    fn fingerprint_uses_sha256_and_openssh_base64_format() {
        assert_eq!(encoding::encode_base64(b"abc"), "YWJj");
        assert_eq!(
            encoding::fingerprint(b"abc"),
            "SHA256:ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0"
        );
        assert!(!encoding::fingerprint(b"public key").ends_with('='));
    }
}
