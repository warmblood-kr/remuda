//! Authenticated incompatible-peer tests; private synthetic endpoints only.
use super::*;
use std::path::PathBuf;
struct Cleanup(PathBuf);
#[test]
fn typed_view_refuses_missing_capability_before_requesting_a_snapshot() {
    let path = PathBuf::from("/tmp").join(format!("rmv-cap-{}", std::process::id()));
    let cleanup = Cleanup(path.clone());
    let endpoint = Endpoint::bind(&path).unwrap();
    let identity = super::super::api::Identity {
        host: [8; 16],
        session: "synthetic".into(),
        instance: "instance".into(),
    };
    let expected = identity.clone();
    let peer = std::thread::spawn(move || {
        let mut channel = endpoint.accept().unwrap();
        let open: Open = serde_json::from_slice(&channel.receive().unwrap()).unwrap();
        assert!(open
            .protocol
            .required
            .iter()
            .any(|value| value == super::super::api::TYPED_VIEW));
        channel
            .send(
                &serde_json::to_vec(&Response::Opened {
                    identity,
                    minor: 0,
                    capabilities: vec!["inspect".into()],
                    max_payload: 65_519,
                })
                .unwrap(),
            )
            .unwrap();
        assert!(
            channel.receive().is_err(),
            "client requested a typed frame from an incompatible peer"
        );
    });
    assert!(Client::open_viewport(&path, expected).is_err());
    peer.join().unwrap();
    drop(cleanup);
}
#[test]
fn recovery_refuses_missing_monotonic_ledger_capability_before_acquisition() {
    let path = std::env::temp_dir().join(format!("rm-host-recovery-cap-{}", std::process::id()));
    let cleanup = Cleanup(path.clone());
    let endpoint = Endpoint::bind(&path).unwrap();
    let identity = super::super::api::Identity {
        host: [8; 16],
        session: "synthetic".into(),
        instance: "instance".into(),
    };
    let expected = identity.clone();
    let peer = std::thread::spawn(move || {
        let mut channel = endpoint.accept().unwrap();
        let open: Open = serde_json::from_slice(&channel.receive().unwrap()).unwrap();
        assert!(open
            .protocol
            .required
            .iter()
            .any(|value| value == super::super::api::RETAINED_LEDGER));
        channel
            .send(
                &serde_json::to_vec(&Response::Opened {
                    identity,
                    minor: 0,
                    capabilities: vec![
                        "inspect".into(),
                        "controller-epoch-v1".into(),
                        "input-act-v1".into(),
                    ],
                    max_payload: 65_519,
                })
                .unwrap(),
            )
            .unwrap();
        assert!(
            channel.receive().is_err(),
            "recovery client mutated a host without ledger proof"
        );
    });
    assert!(Client::open_recovering(&path, expected).is_err());
    peer.join().unwrap();
    drop(cleanup);
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn missing_required_capability_refuses_before_any_controller_mutation() {
    let required = ["inspect", "controller-epoch-v1", "input-act-v1"];
    for omitted in required {
        let path =
            std::env::temp_dir().join(format!("rm-host-caps-{}-{omitted}", std::process::id()));
        let cleanup = Cleanup(path.clone());
        let endpoint = Endpoint::bind(&path).unwrap();
        let manifest = Manifest {
            schema: 1,
            identity: super::super::api::Identity {
                host: [9; 16],
                session: "synthetic-session".into(),
                instance: "synthetic-instance".into(),
            },
            name: "fixture".into(),
            agent: "fixture".into(),
            cwd: path.to_string_lossy().into(),
        };
        crate::fs_atomic::write_atomic_lua_private(
            &path.join("session.json"),
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let peer = std::thread::spawn(move || {
            let mut channel = endpoint.accept().unwrap();
            let open: Open = serde_json::from_slice(&channel.receive().unwrap()).unwrap();
            assert_eq!(open.role, Role::Control);
            assert_eq!(open.expected, Some(manifest.identity.clone()));
            assert!(open.protocol.required.iter().any(|value| value == omitted));
            channel
                .send(
                    &serde_json::to_vec(&Response::Opened {
                        identity: manifest.identity,
                        minor: 0,
                        capabilities: required
                            .into_iter()
                            .filter(|value| *value != omitted)
                            .map(str::to_owned)
                            .collect(),
                        max_payload: 65_519,
                    })
                    .unwrap(),
                )
                .unwrap();
            assert!(
                channel.receive().is_err(),
                "client sent a request after peer omitted {omitted}"
            );
        });
        // close ordinarily emits Acquire followed by Close; incompatible
        // peers must see neither, including on an authenticated connection.
        assert!(command(&path, "close").is_err());
        peer.join().unwrap();
        drop(cleanup);
    }
}
