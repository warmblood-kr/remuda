//! Hash-only, single-use join tokens with persistent expiry.

use super::encoding;
use super::storage;
use remuda_core::WallClock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use zeroize::Zeroizing;

const TOKEN_FILE: &str = "join_tokens.json";
const TOKEN_LIFETIME_SECONDS: u64 = 10 * 60;
const MAX_OUTSTANDING_TOKENS: usize = 16;
const MAX_TOKEN_FILE_SIZE: usize = 8 * 1024;
const JOIN_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(1);
const NOISE_PATTERN: &str = "Noise_NN_25519_ChaChaPoly_SHA256";

/// A newly minted bearer token. Keep `token` secret; the state file stores only its hash.
pub struct MintedJoinToken {
    pub token: Zeroizing<String>,
    pub expires_at_unix_seconds: u64,
}

/// Persistent token state rooted in the local cluster directory.
pub struct JoinTokenStore {
    directory: PathBuf,
    clock: Arc<dyn WallClock>,
    cache: Mutex<TokenCache>,
}

#[derive(Default)]
struct TokenCache {
    expires_at: HashMap<String, u64>,
    last_observed_unix_seconds: u64,
    file_stamp: Option<TokenFileStamp>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TokenFileStamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TokenState {
    version: u8,
    last_observed_unix_seconds: u64,
    tokens: Vec<TokenRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TokenRecord {
    hash: String,
    expires_at_unix_seconds: u64,
}

impl JoinTokenStore {
    /// Open token state in the local initialized cluster directory.
    pub fn open(clock: Arc<dyn WallClock>) -> io::Result<Self> {
        Self::open_at(&storage::cluster_state_dir()?.join("cluster"), clock)
    }

    /// Open token state in an existing private cluster directory.
    pub fn open_at(directory: &Path, clock: Arc<dyn WallClock>) -> io::Result<Self> {
        {
            storage::verify_directory(directory)?;
            Ok(Self {
                directory: directory.to_path_buf(),
                clock,
                cache: Mutex::new(TokenCache::default()),
            })
        }
    }

    /// Mint a 256-bit token, storing only its SHA-256 digest and 10-minute expiry.
    pub fn mint(&self) -> io::Result<MintedJoinToken> {
        {
            let _guard = storage::StateLock::acquire(&self.directory)?;
            let now = self.clock.unix_seconds();
            let mut state = load_state(&self.directory, now)?;
            state.observe(now);
            save_state(&self.directory, &state)?;
            self.refresh_cache(&state);
            if state.tokens.len() >= MAX_OUTSTANDING_TOKENS {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "cannot mint join token: 16 outstanding tokens is the limit",
                ));
            }
            let expires_at_unix_seconds = now
                .checked_add(TOKEN_LIFETIME_SECONDS)
                .ok_or_else(|| io::Error::other("join token expiry is out of range"))?;
            let token = random_token()?;
            let hash = token_hash(&token)?;
            state.tokens.push(TokenRecord {
                hash,
                expires_at_unix_seconds,
            });
            save_state(&self.directory, &state)?;
            self.refresh_cache(&state);
            Ok(MintedJoinToken {
                token,
                expires_at_unix_seconds,
            })
        }
    }

    /// Verify and durably consume a token; all later uses are refused.
    pub fn verify_and_consume(&self, token: &str) -> io::Result<()> {
        self.verify_consume_with(token, || Ok(()))
    }

    /// Persist token consumption before admission while holding the state lock.
    /// If admission refuses, restore the token before releasing the lock.
    pub fn verify_consume_with<T>(
        &self,
        token: &str,
        admit: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        let hash = token_hash(token)?;
        if !self.may_hold(&hash)? {
            return Err(refused_token());
        }
        let _guard = storage::lock_with_deadline(&self.directory, JOIN_LOCK_WAIT)?;
        let now = self.clock.unix_seconds();
        let mut state = load_state(&self.directory, now)?;
        state.observe(now);
        self.refresh_cache(&state);
        let position = state.tokens.iter().position(|record| {
            record.expires_at_unix_seconds > now && hashes_equal(&record.hash, &hash)
        });
        let Some(position) = position else {
            // Do not let invalid-token traffic rewrite the persistent file.
            return Err(refused_token());
        };
        let consumed = state.tokens.remove(position);
        save_state(&self.directory, &state)?;
        self.refresh_cache(&state);
        match admit() {
            Ok(result) => Ok(result),
            Err(admission_error) => {
                state.tokens.push(consumed);
                if let Err(restore_error) = save_state(&self.directory, &state) {
                    return Err(io::Error::other(format!(
                        "join admission failed ({}); token restoration failed ({restore_error})",
                        admission_error
                    )));
                }
                self.refresh_cache(&state);
                Err(admission_error)
            }
        }
    }

    /// Persist a periodic wall-clock observation so serving closes token expiry
    /// state promptly after a system clock rollback.
    pub fn observe(&self) -> io::Result<()> {
        let _guard = storage::StateLock::acquire(&self.directory)?;
        let now = self.clock.unix_seconds();
        let mut state = load_state(&self.directory, now)?;
        state.observe(now);
        save_state(&self.directory, &state)?;
        self.refresh_cache(&state);
        Ok(())
    }

    /// Check the in-memory token copy before waiting for durable state access.
    /// A cache miss refreshes from the atomic token file without taking the lock.
    pub fn may_hold(&self, hash: &str) -> io::Result<bool> {
        let now = self.clock.unix_seconds();
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| io::Error::other("join token cache is unavailable"))?;
        observe_cache(&mut cache, now);
        if cache.expires_at.contains_key(hash) {
            return Ok(true);
        }

        for _ in 0..3 {
            let before = token_file_stamp(&self.directory)?;
            if cache.file_stamp == before {
                return Ok(false);
            }
            let mut state = load_state(&self.directory, now)?;
            state.observe(now);
            let after = token_file_stamp(&self.directory)?;
            if before != after {
                continue;
            }
            cache.replace(&state, after);
            return Ok(cache.expires_at.contains_key(hash));
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "join token state changed while refreshing",
        ))
    }

    fn refresh_cache(&self, state: &TokenState) {
        let stamp = token_file_stamp(&self.directory).ok().flatten();
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.replace(state, stamp);
    }
}

impl TokenCache {
    fn replace(&mut self, state: &TokenState, stamp: Option<TokenFileStamp>) {
        self.expires_at = state
            .tokens
            .iter()
            .map(|record| (record.hash.clone(), record.expires_at_unix_seconds))
            .collect();
        self.last_observed_unix_seconds = state.last_observed_unix_seconds;
        self.file_stamp = stamp;
    }
}

fn observe_cache(cache: &mut TokenCache, now: u64) {
    if now < cache.last_observed_unix_seconds {
        cache.expires_at.clear();
    } else {
        cache.expires_at.retain(|_, expires_at| *expires_at > now);
    }
    cache.last_observed_unix_seconds = now;
}

impl TokenState {
    fn new(now: u64) -> Self {
        Self {
            version: 1,
            last_observed_unix_seconds: now,
            tokens: Vec::new(),
        }
    }

    fn observe(&mut self, now: u64) {
        if now < self.last_observed_unix_seconds {
            self.tokens.clear();
        } else {
            self.tokens
                .retain(|record| record.expires_at_unix_seconds > now);
        }
        self.last_observed_unix_seconds = now;
    }
}

fn load_state(directory: &Path, now: u64) -> io::Result<TokenState> {
    use std::fs;
    let path = directory.join(TOKEN_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is a symlink; refusing", path.display()),
            ))
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "join token state must be a regular file",
            ))
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(TokenState::new(now)),
        Err(error) => return Err(error),
    }
    #[cfg(not(windows))]
    let mut file = {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        options.open(&path)?
    };
    #[cfg(windows)]
    let mut file = super::windows_security::open_for_read(&path)?;
    storage::check_private_file(&file, "join token state", &path)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_TOKEN_FILE_SIZE + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_TOKEN_FILE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "join token state exceeds its size limit",
        ));
    }
    let state: TokenState = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    validate_state(&state)?;
    Ok(state)
}

fn token_file_stamp(directory: &Path) -> io::Result<Option<TokenFileStamp>> {
    let path = directory.join(TOKEN_FILE);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    let (device, inode) = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    Ok(Some(TokenFileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        device,
        #[cfg(unix)]
        inode,
    }))
}

fn save_state(directory: &Path, state: &TokenState) -> io::Result<()> {
    let bytes = serde_json::to_vec(state).map_err(io::Error::other)?;
    storage::atomic_write(&directory.join(TOKEN_FILE), &bytes)
}

fn validate_state(state: &TokenState) -> io::Result<()> {
    if state.version != 1 || state.tokens.len() > MAX_OUTSTANDING_TOKENS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid join token state version or token count",
        ));
    }
    for record in &state.tokens {
        if decode_hash(&record.hash).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid hash in join token state",
            ));
        }
    }
    Ok(())
}

fn random_token() -> io::Result<Zeroizing<String>> {
    let mut bytes = Zeroizing::new([0; 32]);
    getrandom::fill(&mut *bytes).map_err(|error| {
        io::Error::other(format!("secure token randomness unavailable: {error}"))
    })?;
    Ok(Zeroizing::new(encoding::encode_base64(&bytes[..])))
}

fn token_hash(token: &str) -> io::Result<String> {
    let bytes = Zeroizing::new(encoding::decode_base64(token).map_err(|_| refused_token())?);
    if bytes.len() != 32 || encoding::encode_base64(&bytes) != token {
        return Err(refused_token());
    }
    hash_bytes(&bytes)
}

fn hash_bytes(bytes: &[u8]) -> io::Result<String> {
    use snow::resolvers::CryptoResolver;
    let params: snow::params::NoiseParams = NOISE_PATTERN
        .parse()
        .map_err(|error: snow::Error| io::Error::other(error))?;
    let resolver = snow::resolvers::DefaultResolver;
    let mut hash = resolver
        .resolve_hash(&params.hash)
        .ok_or_else(|| io::Error::other("Snow SHA-256 provider is unavailable"))?;
    hash.input(bytes);
    let mut digest = vec![0; hash.hash_len()];
    hash.result(&mut digest);
    Ok(encoding::encode_base64(&digest))
}

fn decode_hash(value: &str) -> io::Result<Vec<u8>> {
    let decoded = encoding::decode_base64(value)?;
    if decoded.len() != 32 || encoding::encode_base64(&decoded) != value {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid join token hash",
        ));
    }
    Ok(decoded)
}

fn hashes_equal(first: &str, second: &str) -> bool {
    match (decode_hash(first), decode_hash(second)) {
        (Ok(first), Ok(second)) if first.len() == second.len() => {
            first
                .iter()
                .zip(second)
                .fold(0u8, |difference, (left, right)| difference | (left ^ right))
                == 0
        }
        _ => false,
    }
}

fn refused_token() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "join token is expired, unknown, or already used",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::ManualWallClock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "remuda-join-token-test-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            storage::create_private_directory(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cache_stays_consistent_after_mint_consume_and_expiry() {
        let directory = TestDirectory::new();
        let clock = Arc::new(ManualWallClock::new(1_700_000_000));
        let store = JoinTokenStore::open_at(&directory.0, clock.clone()).unwrap();

        let restored = store.mint().unwrap();
        let restored_hash = token_hash(&restored.token).unwrap();
        assert!(store.may_hold(&restored_hash).unwrap());
        assert!(store
            .verify_consume_with(&restored.token, || {
                Err::<(), _>(io::Error::other("test refusal"))
            })
            .is_err());
        assert!(store.may_hold(&restored_hash).unwrap());
        store.verify_and_consume(&restored.token).unwrap();
        assert!(!store.may_hold(&restored_hash).unwrap());

        let expiring = store.mint().unwrap();
        let expiring_hash = token_hash(&expiring.token).unwrap();
        assert!(store.may_hold(&expiring_hash).unwrap());
        clock.advance(std::time::Duration::from_secs(TOKEN_LIFETIME_SECONDS + 1));
        assert!(!store.may_hold(&expiring_hash).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn lock_with_deadline_returns_busy_instead_of_waiting_forever() {
        let directory = TestDirectory::new();
        let _holder = storage::StateLock::acquire(&directory.0).unwrap();
        let started = std::time::Instant::now();
        let result = storage::lock_with_deadline(&directory.0, JOIN_LOCK_WAIT);
        let error = match result {
            Ok(_) => panic!("lock unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(started.elapsed() >= JOIN_LOCK_WAIT);
        assert!(started.elapsed() < std::time::Duration::from_millis(1500));
    }
}
