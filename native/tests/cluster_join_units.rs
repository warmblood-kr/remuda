#[cfg(not(windows))]
use remuda_core::{ManualWallClock, WallClock};
use remuda_native::cluster::join_line::JoinLine;
#[cfg(not(windows))]
use remuda_native::cluster::join_token::JoinTokenStore;
#[cfg(not(windows))]
use std::fs;
#[cfg(not(windows))]
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(windows))]
use std::sync::Arc;
#[cfg(not(windows))]
use std::sync::Barrier;
#[cfg(not(windows))]
use std::thread;
#[cfg(not(windows))]
use std::time::Duration;
use zeroize::Zeroizing;

#[cfg(not(windows))]
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

#[cfg(not(windows))]
fn private_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "remuda-join-units-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

#[cfg(not(windows))]
fn remove_dir(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

fn fingerprint(public_key: &[u8]) -> String {
    use snow::resolvers::CryptoResolver;
    let params: snow::params::NoiseParams = "Noise_NN_25519_ChaChaPoly_SHA256".parse().unwrap();
    let resolver = snow::resolvers::DefaultResolver;
    let mut hash = resolver.resolve_hash(&params.hash).unwrap();
    hash.input(public_key);
    let mut digest = vec![0; hash.hash_len()];
    hash.result(&mut digest);
    format!(
        "SHA256:{}",
        remuda_native::cluster::encoding::encode_base64(&digest).trim_end_matches('=')
    )
}

#[test]
#[cfg(not(windows))]
fn join_token_mint_stores_only_hash_with_private_owned_state() {
    let dir = private_dir();
    let clock = Arc::new(ManualWallClock::new(1_700_000_000));
    let store = JoinTokenStore::open_at(&dir, clock).unwrap();
    let minted = store.mint().unwrap();
    assert_eq!(minted.expires_at_unix_seconds, 1_700_000_600);
    let state = fs::read_to_string(dir.join("join_tokens.json")).unwrap();
    assert!(!state.contains(minted.token.as_str()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(dir.join("join_tokens.json")).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(dir.join("join_tokens.json")).unwrap().uid(),
            unsafe { libc::geteuid() }
        );
    }
    assert_eq!(minted.token.len(), 44);
    remove_dir(&dir);
}

#[test]
#[cfg(not(windows))]
fn join_token_consumes_once_across_store_reload() {
    let dir = private_dir();
    let clock = Arc::new(ManualWallClock::new(1_700_000_000));
    let minted = JoinTokenStore::open_at(&dir, clock.clone())
        .unwrap()
        .mint()
        .unwrap();
    JoinTokenStore::open_at(&dir, clock.clone())
        .unwrap()
        .verify_and_consume(&minted.token)
        .unwrap();
    assert!(JoinTokenStore::open_at(&dir, clock)
        .unwrap()
        .verify_and_consume(&minted.token)
        .is_err());
    remove_dir(&dir);
}

#[test]
#[cfg(not(windows))]
fn refused_join_admission_does_not_consume_token() {
    let dir = private_dir();
    let clock = Arc::new(ManualWallClock::new(1_700_000_000));
    let store = JoinTokenStore::open_at(&dir, clock).unwrap();
    let minted = store.mint().unwrap();
    let token_state = dir.join("join_tokens.json");
    let before = fs::read(&token_state).unwrap();
    let refused = store.verify_consume_with(&minted.token, || {
        assert_ne!(fs::read(&token_state).unwrap(), before);
        Err::<(), _>(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "registry full",
        ))
    });
    assert!(refused.is_err());
    assert_eq!(fs::read(&token_state).unwrap(), before);
    assert!(store.verify_and_consume(&minted.token).is_ok());
    assert!(store.verify_and_consume(&minted.token).is_err());
    remove_dir(&dir);
}

#[test]
#[cfg(not(windows))]
fn join_token_rejects_expired_and_unknown_values() {
    let dir = private_dir();
    let clock = Arc::new(ManualWallClock::new(1_700_000_000));
    let store = JoinTokenStore::open_at(&dir, clock.clone()).unwrap();
    let minted = store.mint().unwrap();
    assert!(store.verify_and_consume("unknown").is_err());
    let well_formed_but_unknown = remuda_native::cluster::encoding::encode_base64(&[9; 32]);
    assert!(store.verify_and_consume(&well_formed_but_unknown).is_err());
    clock.advance(Duration::from_secs(600));
    assert!(store.verify_and_consume(&minted.token).is_err());
    remove_dir(&dir);
}

#[test]
#[cfg(all(unix, not(windows)))]
fn bad_join_token_does_not_rewrite_token_state() {
    use std::os::unix::fs::MetadataExt;

    let dir = private_dir();
    let clock = Arc::new(ManualWallClock::new(1_700_000_000));
    let store = JoinTokenStore::open_at(&dir, clock).unwrap();
    let _minted = store.mint().unwrap();
    let path = dir.join("join_tokens.json");
    let before = fs::metadata(&path).unwrap();
    let contents = fs::read(&path).unwrap();

    assert!(store.verify_and_consume("unknown").is_err());

    let after = fs::metadata(&path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), contents);
    assert_eq!(after.ino(), before.ino(), "bad token replaced token state");
    remove_dir(&dir);
}

#[test]
#[cfg(not(windows))]
fn join_token_rollback_expires_existing_tokens_but_allows_minting() {
    let dir = private_dir();
    let clock = Arc::new(ManualWallClock::new(1_700_000_000));
    let old = JoinTokenStore::open_at(&dir, clock.clone())
        .unwrap()
        .mint()
        .unwrap();
    clock.set_unix_seconds(1_699_000_000);
    let store = JoinTokenStore::open_at(&dir, clock.clone()).unwrap();
    assert!(store.verify_and_consume(&old.token).is_err());
    let fresh = store.mint().unwrap();
    assert_eq!(fresh.expires_at_unix_seconds, 1_699_000_600);
    assert!(store.verify_and_consume(&fresh.token).is_ok());
    remove_dir(&dir);
}

#[test]
#[cfg(not(windows))]
fn join_token_unobserved_rollback_extends_lifetime_by_rollback_duration() {
    let dir = private_dir();
    let clock = Arc::new(ManualWallClock::new(1_700_000_000));
    let store = JoinTokenStore::open_at(&dir, clock.clone()).unwrap();
    let token = store.mint().unwrap();
    clock.set_unix_seconds(1_699_999_900);
    clock.advance(Duration::from_secs(699));
    assert_eq!(clock.unix_seconds(), 1_700_000_599);
    assert!(store.verify_and_consume(&token.token).is_ok());
    remove_dir(&dir);
}

#[test]
#[cfg(not(windows))]
fn join_token_periodic_observe_expires_tokens_after_clock_rollback() {
    let dir = private_dir();
    let clock = Arc::new(ManualWallClock::new(1_700_000_000));
    let store = JoinTokenStore::open_at(&dir, clock.clone()).unwrap();
    let token = store.mint().unwrap();
    clock.set_unix_seconds(1_699_999_999);
    store.observe().unwrap();
    assert!(store.verify_and_consume(&token.token).is_err());
    remove_dir(&dir);
}

#[test]
#[cfg(not(windows))]
fn join_token_concurrent_consumers_allow_exactly_one_success() {
    let dir = private_dir();
    let clock = Arc::new(ManualWallClock::new(1_700_000_000));
    let store = Arc::new(JoinTokenStore::open_at(&dir, clock).unwrap());
    let minted = store.mint().unwrap();
    let token = Arc::new(Zeroizing::new(minted.token.as_str().to_owned()));
    let barrier = Arc::new(Barrier::new(3));
    let consumers: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            let token = token.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                store.verify_and_consume(token.as_str()).is_ok()
            })
        })
        .collect();
    barrier.wait();
    let successes = consumers
        .into_iter()
        .map(|consumer| consumer.join().unwrap())
        .filter(|succeeded| *succeeded)
        .count();
    assert_eq!(successes, 1);
    remove_dir(&dir);
}

#[test]
#[cfg(not(windows))]
fn join_token_state_refuses_loose_file_permissions() {
    let dir = private_dir();
    let clock = Arc::new(ManualWallClock::new(1_700_000_000));
    let token = JoinTokenStore::open_at(&dir, clock.clone())
        .unwrap()
        .mint()
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            dir.join("join_tokens.json"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert_eq!(
            JoinTokenStore::open_at(&dir, clock)
                .unwrap()
                .verify_and_consume(&token.token)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
    remove_dir(&dir);
}

#[test]
#[cfg(not(windows))]
fn join_token_store_caps_outstanding_tokens_at_sixteen() {
    let dir = private_dir();
    let clock = Arc::new(ManualWallClock::new(1_700_000_000));
    let store = JoinTokenStore::open_at(&dir, clock).unwrap();
    for _ in 0..16 {
        store.mint().unwrap();
    }
    let error = store.mint().err().unwrap();
    assert!(error.to_string().contains("16 outstanding"));
    remove_dir(&dir);
}

#[test]
#[cfg(not(windows))]
fn join_token_purges_expired_entries_before_applying_the_limit() {
    let dir = private_dir();
    let clock = Arc::new(ManualWallClock::new(1_700_000_000));
    let store = JoinTokenStore::open_at(&dir, clock.clone()).unwrap();
    for _ in 0..16 {
        store.mint().unwrap();
    }
    clock.advance(Duration::from_secs(600));
    for _ in 0..16 {
        store.mint().unwrap();
    }
    remove_dir(&dir);
}

#[test]
fn join_line_round_trips_ipv4_and_ipv6() {
    let line = JoinLine {
        issuer_addr: "[fd00::1]:443".parse().unwrap(),
        issuer_fingerprint: fingerprint(&[7; 32]),
        issuer_static_pubkey: [7; 32],
        token: Zeroizing::new(remuda_native::cluster::encoding::encode_base64(&[9; 32])),
    };
    assert_eq!(JoinLine::decode(&line.encode().unwrap()).unwrap(), line);
    assert!(!format!("{line:?}").contains(line.token.as_str()));
    let ipv4 = JoinLine {
        issuer_addr: "10.0.0.1:443".parse().unwrap(),
        issuer_fingerprint: fingerprint(&[7; 32]),
        issuer_static_pubkey: [7; 32],
        token: Zeroizing::new(remuda_native::cluster::encoding::encode_base64(&[9; 32])),
    };
    assert_eq!(JoinLine::decode(&ipv4.encode().unwrap()).unwrap(), ipv4);
}

#[test]
fn join_line_rejects_bad_fields_and_noncanonical_keys() {
    let valid = format!(
        "remuda-join-v1 10.0.0.1:443 {} {} {}",
        fingerprint(&[7; 32]),
        remuda_native::cluster::encoding::encode_base64(&[7; 32]),
        remuda_native::cluster::encoding::encode_base64(&[9; 32])
    );
    for invalid in [
        "remuda-join-v1 10.0.0.1:443",
        "remuda-join-v1 10.0.0.1:443 key token extra",
        "remuda-join-v1  10.0.0.1:443 key token",
        "remuda-join-v1 10.0.0.1:443 key token\n",
        "remuda-join-v2 10.0.0.1:443 key token",
        &"x".repeat(257),
    ] {
        assert!(JoinLine::decode(invalid).is_err(), "accepted {invalid:?}");
    }
    let noncanonical = valid.replace("= ", " ");
    assert!(JoinLine::decode(&noncanonical).is_err());
}

#[test]
fn join_line_rejects_noncanonical_or_nonunicast_endpoints() {
    let key = remuda_native::cluster::encoding::encode_base64(&[7; 32]);
    let token = remuda_native::cluster::encoding::encode_base64(&[9; 32]);
    for address in [
        "10.0.0.1:0443",
        "224.0.0.1:443",
        "255.255.255.255:443",
        "[ff02::1]:443",
        "[fe80::1%1]:443",
    ] {
        let line = format!(
            "remuda-join-v1 {address} {} {key} {token}",
            fingerprint(&[7; 32])
        );
        assert!(JoinLine::decode(&line).is_err(), "accepted {address}");
    }
}

#[test]
fn join_line_rejects_all_zero_static_key() {
    let invalid = format!(
        "remuda-join-v1 10.0.0.1:443 {} {} {}",
        fingerprint(&[0; 32]),
        remuda_native::cluster::encoding::encode_base64(&[0; 32]),
        remuda_native::cluster::encoding::encode_base64(&[9; 32])
    );
    assert!(JoinLine::decode(&invalid).is_err());
}

#[test]
fn join_line_rejects_known_low_order_static_key() {
    let mut low_order = [0u8; 32];
    low_order[0] = 1;
    let invalid = format!(
        "remuda-join-v1 10.0.0.1:443 {} {} {}",
        fingerprint(&low_order),
        remuda_native::cluster::encoding::encode_base64(&low_order),
        remuda_native::cluster::encoding::encode_base64(&[9; 32])
    );
    assert!(JoinLine::decode(&invalid).is_err());
}

#[test]
fn join_line_pin_check_matches_and_rejects_mismatch() {
    let keypair = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let expected_fp = fingerprint(&keypair.public);
    let line = JoinLine {
        issuer_addr: "10.0.0.1:443".parse().unwrap(),
        issuer_fingerprint: expected_fp.clone(),
        issuer_static_pubkey: keypair.public.as_slice().try_into().unwrap(),
        token: Zeroizing::new(remuda_native::cluster::encoding::encode_base64(&[9; 32])),
    };
    assert!(line.verify_pin(&expected_fp).is_ok());
    let error = line.verify_pin("SHA256:wrong").unwrap_err();
    assert!(error.to_string().contains("expected SHA256:wrong"));
    assert!(error
        .to_string()
        .contains(&format!("received {expected_fp}")));
}

#[test]
fn join_line_rejects_fingerprint_that_does_not_match_key() {
    let line = format!(
        "remuda-join-v1 10.0.0.1:443 {} {} {}",
        fingerprint(&[8; 32]),
        remuda_native::cluster::encoding::encode_base64(&[7; 32]),
        remuda_native::cluster::encoding::encode_base64(&[9; 32])
    );
    let error = JoinLine::decode(&line).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("does not match"));
}
