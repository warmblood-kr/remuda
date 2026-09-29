//! Hash-only, single-use join tokens with persistent expiry.

use super::encoding;
use super::storage;
use remuda_core::WallClock;
use serde::{Deserialize, Serialize};
use std::io;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use zeroize::Zeroizing;

const TOKEN_FILE: &str = "join_tokens.json";
const TOKEN_LIFETIME_SECONDS: u64 = 10 * 60;
const MAX_OUTSTANDING_TOKENS: usize = 16;
const MAX_TOKEN_FILE_SIZE: usize = 8 * 1024;
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
        let _guard = storage::StateLock::acquire(&self.directory)?;
        let now = self.clock.unix_seconds();
        let mut state = load_state(&self.directory, now)?;
        state.observe(now);
        let position = state.tokens.iter().position(|record| {
            record.expires_at_unix_seconds > now && hashes_equal(&record.hash, &hash)
        });
        let Some(position) = position else {
            // Do not let invalid-token traffic rewrite the persistent file.
            return Err(refused_token());
        };
        let consumed = state.tokens.remove(position);
        save_state(&self.directory, &state)?;
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
        save_state(&self.directory, &state)
    }
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
