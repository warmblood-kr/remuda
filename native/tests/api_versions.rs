//! Frozen API fixtures remain executable as the live surface grows.

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon, script};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

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

fn eval(path: &std::path::Path, code: &str) -> String {
    match client::request(
        path,
        &Request::Eval {
            code: code.to_string(),
            name: None,
        },
    )
    .expect("eval")
    {
        Response::Value(value) => value,
        other => panic!("unexpected eval response: {other:?}"),
    }
}

fn start_session(path: &std::path::Path, name: &str, command: Vec<String>) {
    let response = client::request(
        path,
        &Request::New {
            name: Some(name.to_string()),
            command,
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("start session");
    assert_eq!(response, Response::Value(name.to_string()));
}

fn wait_for_exit_event(path: &std::path::Path, name: &str) {
    let probe = format!("return tostring(remuda._api_v5_exit_seen({name:?}))");
    let deadline = Instant::now() + Duration::from_secs(10);
    while eval(path, &probe) != "true" {
        assert!(
            Instant::now() < deadline,
            "no session_exited event for {name}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn exercise_session_exit_payload(path: &std::path::Path) {
    let close_name = format!("api-v5-close-{}", std::process::id());
    #[cfg(windows)]
    let long_lived = vec![
        "cmd.exe".into(),
        "/d".into(),
        "/c".into(),
        "ping -n 30 127.0.0.1 >nul".into(),
    ];
    #[cfg(not(windows))]
    let long_lived = vec!["/bin/sleep".into(), "30".into()];
    start_session(path, &close_name, long_lived.clone());
    assert_eq!(
        client::request(
            path,
            &Request::Close {
                name: close_name.clone(),
                instance_id: None,
                confirm: None,
            },
        )
        .expect("close session"),
        Response::Ok
    );
    wait_for_exit_event(path, &close_name);
    eval(
        path,
        &format!("remuda._api_v5_assert_exit({close_name:?}, 'closed')"),
    );

    for iteration in 0..200 {
        let name = format!("api-v5-close-race-{}-{iteration}", std::process::id());
        start_session(path, &name, long_lived.clone());
        assert_eq!(
            client::request(
                path,
                &Request::Close {
                    name: name.clone(),
                    instance_id: None,
                    confirm: None,
                },
            )
            .expect("close race session"),
            Response::Ok
        );
        wait_for_exit_event(path, &name);
        eval(
            path,
            &format!("remuda._api_v5_assert_exit({name:?}, 'closed')"),
        );
    }

    let exit_name = format!("api-v5-exit-{}", std::process::id());
    #[cfg(windows)]
    let exits_with_code = vec!["cmd.exe".into(), "/d".into(), "/c".into(), "exit 23".into()];
    #[cfg(not(windows))]
    let exits_with_code = vec!["/bin/sh".into(), "-c".into(), "exit 23".into()];
    start_session(path, &exit_name, exits_with_code);
    wait_for_exit_event(path, &exit_name);
    eval(
        path,
        &format!("remuda._api_v5_assert_exit({exit_name:?}, 'exited', 23)"),
    );

    #[cfg(unix)]
    {
        let signal_name = format!("api-v5-signal-{}", std::process::id());
        start_session(
            path,
            &signal_name,
            vec!["/bin/sh".into(), "-c".into(), "kill -TERM $$".into()],
        );
        wait_for_exit_event(path, &signal_name);
        eval(
            path,
            &format!("remuda._api_v5_assert_exit({signal_name:?}, 'exited', nil, 15, 'SIGTERM')"),
        );
    }
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
        if version == "v5" {
            exercise_session_exit_payload(&path);
        }
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
