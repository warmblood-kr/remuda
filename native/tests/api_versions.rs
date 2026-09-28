//! Frozen API fixtures remain executable as the live surface grows.

use remuda_native::{daemon, script};
use std::path::PathBuf;

#[path = "daemon_support/spawn.rs"]
mod spawn;

fn scratch(version: &str) -> PathBuf {
    let root = if cfg!(target_os = "macos") {
        PathBuf::from("/private/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = root.join(format!("r-api-{}-{version}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn frozen_api_fixtures_v1_through_v4_and_new_v5_surface_run() {
    for (version, source) in [
        ("v1", include_str!("api/v1.lua")),
        ("v2", include_str!("api/v2.lua")),
        ("v3", include_str!("api/v3.lua")),
        ("v4", include_str!("api/v4.lua")),
        ("v5", include_str!("api/v5.lua")),
    ] {
        let dir = scratch(version);
        let path = daemon::socket_path_in(&dir, "s");
        let mut private_daemon = spawn::Daemon::spawn(&dir);
        let fixture = dir.join(format!("{version}.lua"));
        std::fs::write(&fixture, source).unwrap();
        script::run(&path, &fixture).unwrap_or_else(|error| panic!("{version} fixture: {error}"));
        assert_eq!(
            remuda_native::client::request(
                &path,
                &remuda_core::protocol::Request::Shutdown {
                    requester_daemon_id: None,
                    requester_session_id: None,
                    requester_session_name: None,
                    override_hosted: true,
                },
            )
            .unwrap(),
            remuda_core::protocol::Response::Ok
        );
        assert!(
            private_daemon.left_on_its_own(),
            "private fixture daemon stopped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
