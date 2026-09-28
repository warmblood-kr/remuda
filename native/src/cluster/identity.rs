//! Node identity: Snow key generation, secure local persistence, and fingerprints.

use super::registry::{self, NodeState};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const NOISE_PATTERN: &str = "Noise_NN_25519_ChaChaPoly_SHA256";
const IDENTITY_FILE: &str = "identity.key";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeIdentity {
    pub node_name: String,
    pub node_fp: String,
    pub static_pubkey: Vec<u8>,
}

/// Initialize one local identity and its cluster-of-one registry.
pub fn init() -> io::Result<NodeIdentity> {
    init_at(&cluster_state_dir()?.join("cluster"))
}

/// Display identity and admitted member count, or `None` before initialization.
pub fn status() -> io::Result<Option<(NodeIdentity, usize)>> {
    let dir = cluster_state_dir()?.join("cluster");
    if !dir.join(IDENTITY_FILE).exists() {
        return Ok(None);
    }
    let identity = load_identity(&dir)?;
    let registry = registry::load_registry_at(&dir)?;
    let members = registry
        .authorized_nodes
        .iter()
        .filter(|entry| entry.state == NodeState::Admitted)
        .count();
    Ok(Some((identity, members)))
}

fn init_at(dir: &Path) -> io::Result<NodeIdentity> {
    fs::create_dir_all(dir)?;
    secure_directory(dir)?;
    let lock_guard = IdentityLock::acquire(dir)?;
    let key_path = dir.join(IDENTITY_FILE);
    let identity = if key_path.exists() {
        load_identity(dir)?
    } else {
        let keypair = snow::Builder::new(NOISE_PATTERN.parse().expect("static Noise pattern"))
            .generate_keypair()
            .map_err(io::Error::other)?;
        let mut bytes = keypair.private;
        bytes.extend_from_slice(&keypair.public);
        atomic_write(&key_path, &bytes)?;
        identity_from_parts(&bytes)?
    };
    let mut registry = registry::load_registry_at(dir)?;
    let before = registry.clone();
    registry.ensure_self(&identity);
    if registry != before {
        registry::save_registry_at(dir, &registry)?;
    }
    drop(lock_guard);
    Ok(identity)
}

fn load_identity(dir: &Path) -> io::Result<NodeIdentity> {
    let path = dir.join(IDENTITY_FILE);
    check_private_mode(&path)?;
    let bytes = fs::read(path)?;
    identity_from_parts(&bytes)
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
    let node_fp = fingerprint(&static_pubkey);
    Ok(NodeIdentity {
        node_name: node_name(&node_fp),
        node_fp,
        static_pubkey,
    })
}

pub(crate) fn cluster_state_dir() -> io::Result<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?,
        )
        .join(".local/state"),
    };
    Ok(base.join("remuda"))
}

pub(super) fn secure_directory(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn check_private_mode(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & !0o600 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "cluster identity {} has loose permissions; run chmod 600",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = parent.join(format!(
        ".{name}-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let result = write_then_rename(&mut file, &temp, path, bytes);
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn write_then_rename(file: &mut File, temp: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    Ok(())
}

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

fn fingerprint(public_key: &[u8]) -> String {
    format!(
        "SHA256:{}",
        base64(&sha256(public_key)).trim_end_matches('=')
    )
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        result.push(TABLE[(a >> 2) as usize] as char);
        result.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        result.push(if chunk.len() > 1 {
            TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            TABLE[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    result
}

pub(super) fn public_key_text(bytes: &[u8]) -> String {
    base64(bytes)
}

// Reuse Snow's SHA-256 provider rather than adding another crypto crate.
fn sha256(input: &[u8]) -> Vec<u8> {
    use snow::resolvers::CryptoResolver;
    let params: snow::params::NoiseParams = NOISE_PATTERN.parse().expect("static Noise pattern");
    let resolver = snow::resolvers::DefaultResolver;
    let mut hash = resolver
        .resolve_hash(&params.hash)
        .expect("Snow SHA-256 provider");
    hash.input(input);
    let mut digest = vec![0; hash.hash_len()];
    hash.result(&mut digest);
    digest
}

pub(super) struct IdentityLock {
    lock_file: File,
}

impl IdentityLock {
    pub(super) fn acquire(dir: &Path) -> io::Result<Self> {
        let path = dir.join("identity.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        file.lock()?;
        Ok(Self { lock_file: file })
    }
}

impl Drop for IdentityLock {
    fn drop(&mut self) {
        drop(self.lock_file.unlock());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
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
        let first = init_at(&dir).unwrap();
        let before = fs::read(dir.join(IDENTITY_FILE)).unwrap();
        let second = init_at(&dir).unwrap();
        assert_eq!(first, second);
        assert_eq!(before, fs::read(dir.join(IDENTITY_FILE)).unwrap());
        let members = registry::load_registry_at(&dir).unwrap().authorized_nodes;
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].node_fp, first.node_fp);
        assert_eq!(members[0].state, NodeState::Admitted);
        assert_eq!(members[0].version, 1);
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
    fn refuses_identity_file_with_loose_permissions() {
        let dir = temp_dir().join("cluster");
        let _ = init_at(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.join(IDENTITY_FILE), fs::Permissions::from_mode(0o644))
                .unwrap();
            let error = load_identity(&dir).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(error.to_string().contains("chmod 600"));
        }
    }

    #[test]
    fn fingerprint_uses_sha256_and_openssh_base64_format() {
        assert_eq!(
            base64(&sha256(b"abc")),
            "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="
        );
        assert!(fingerprint(b"public key").starts_with("SHA256:"));
        assert!(!fingerprint(b"public key").ends_with('='));
    }
}
