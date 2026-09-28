//! Node identity: Snow key generation, secure local persistence, and fingerprints.

#[cfg(not(windows))]
use super::encoding;
#[cfg(not(windows))]
use super::storage;
#[cfg(not(windows))]
use std::fs::{self, OpenOptions};
use std::io;
#[cfg(not(windows))]
use std::io::Read;
#[cfg(not(windows))]
use std::path::Path;
#[cfg(not(windows))]
use zeroize::Zeroizing;

#[cfg(not(windows))]
const NOISE_PATTERN: &str = "Noise_NN_25519_ChaChaPoly_SHA256";
#[cfg(not(windows))]
const IDENTITY_FILE: &str = "identity.key";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeIdentity {
    pub node_name: String,
    pub node_fp: String,
    pub static_pubkey: Vec<u8>,
}

pub(super) fn prepare_cluster_dir() -> io::Result<std::path::PathBuf> {
    #[cfg(windows)]
    return Err(windows_storage_error());
    #[cfg(not(windows))]
    {
        let dir = storage::cluster_state_dir()?.join("cluster");
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
}

#[cfg(not(windows))]
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

#[cfg(windows)]
pub(super) fn windows_storage_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "cluster identity storage is not yet hardened on Windows; see warmblood-kr/remuda#214",
    )
}

#[cfg(all(test, unix))]
pub(super) fn init_identity_at(dir: &Path) -> io::Result<(NodeIdentity, bool)> {
    #[cfg(windows)]
    return Err(windows_storage_error());
    #[cfg(not(windows))]
    {
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
}

#[cfg(not(windows))]
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
                let mut material = Zeroizing::new(Vec::with_capacity(64));
                material.extend_from_slice(&private);
                material.extend_from_slice(&keypair.public);
                storage::atomic_write(&key_path, &material)?;
                (identity_from_parts(&material)?, true)
            }
            Err(e) => return Err(e),
        };
        Ok((identity, created))
    }
}

#[cfg(not(windows))]
pub(super) fn load_identity_at(dir: &Path) -> io::Result<NodeIdentity> {
    let path = dir.join(IDENTITY_FILE);
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&path).map_err(|error| {
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
        error
    })?;
    storage::check_private_file(&file, "cluster identity", &path)?;
    let mut bytes = Zeroizing::new([0u8; 64]);
    file.read_exact(&mut bytes[..])?;
    let mut trailing = [0u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cluster identity key must be exactly 64 bytes",
        ));
    }
    identity_from_parts(&bytes[..])
}

#[cfg(not(windows))]
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

#[cfg(not(windows))]
fn node_name(fingerprint: &str) -> String {
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
