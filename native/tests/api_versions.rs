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
    let dir = std::env::temp_dir().join(format!("r-api-{}-{version}", std::process::id()));
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

fn session_output_version(path: &std::path::Path, name: &str) -> u64 {
    match client::request(path, &Request::List).expect("list sessions") {
        Response::Sessions(sessions) => sessions
            .into_iter()
            .find(|session| session.name == name)
            .and_then(|session| session.output_version)
            .expect("session output version"),
        other => panic!("unexpected list response: {other:?}"),
    }
}

fn wait_for_fixture_output(path: &std::path::Path, version: &str, needles: &[&str]) {
    let prefix = format!("api-{version}-");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let session_name = match client::request(path, &Request::List).expect("list API sessions") {
            Response::Sessions(sessions) => sessions
                .into_iter()
                .find(|session| session.name.starts_with(&prefix))
                .map(|session| session.name),
            other => panic!("list API sessions: {other:?}"),
        };
        if let Some(name) = session_name {
            let screen = match client::request(path, &Request::Capture { name: name.clone() }) {
                Ok(Response::Screen(screen)) => screen,
                other => panic!("capture {name}: {other:?}"),
            };
            if needles.iter().all(|needle| screen.contains(needle)) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "{version} fixture output did not reach {needles:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
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

fn wait_for_output_event(path: &std::path::Path, name: &str) {
    let probe = format!("return tostring(remuda._api_v5_output_seen({name:?}))");
    let deadline = Instant::now() + Duration::from_secs(10);
    while eval(path, &probe) != "true" {
        assert!(
            Instant::now() < deadline,
            "no session_output event for {name}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn exercise_session_output_payload(path: &std::path::Path) {
    let name = format!("api-v5-output-{}", std::process::id());
    #[cfg(windows)]
    let command = vec![
        "cmd.exe".into(),
        "/d".into(),
        "/c".into(),
        "echo session-output-ready & ping -n 3 127.0.0.1 >nul".into(),
    ];
    #[cfg(not(windows))]
    let command = vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf session-output-ready; sleep 2".into(),
    ];
    start_session(path, &name, command);
    wait_for_output_event(path, &name);
    eval(path, &format!("remuda._api_v5_assert_output({name:?})"));
    client::request(
        path,
        &Request::Close {
            name,
            instance_id: None,
            confirm: None,
        },
    )
    .expect("close output fixture session");
}

fn exercise_expect_wakes_on_session_output(path: &std::path::Path) {
    let name = format!("api-v5-expect-output-{}", std::process::id());
    let marker = format!("EXPECTOUTPUTREADY{}", std::process::id());
    eval(
        path,
        &format!(
            "remuda._api_v5_expect_handle = remuda.expect({name:?}, {{{{ match = {marker:?} }}}}, {{timeout=5, interval=1}})"
        ),
    );
    #[cfg(windows)]
    let command = vec![
        "cmd.exe".into(),
        "/d".into(),
        "/c".into(),
        format!("ping -n 3 127.0.0.1 >nul & echo {marker} & ping -n 6 127.0.0.1 >nul"),
    ];
    #[cfg(not(windows))]
    let command = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!("sleep 1.5; printf {marker}; sleep 5"),
    ];
    start_session(path, &name, command);
    let marker_visible =
        format!("return tostring(remuda.capture({name:?}):find({marker:?}, 1, true) ~= nil)");
    let mut startup_wake_checked = false;
    let marker_deadline = Instant::now() + Duration::from_secs(5);
    let marker_seen_at = loop {
        let version = session_output_version(path, &name);
        if version > 0 && !startup_wake_checked {
            startup_wake_checked = true;
            let wake_deadline = Instant::now() + Duration::from_millis(750);
            while eval(
                path,
                "return tostring(remuda._api_v5_expect_handle.state.last_screen ~= nil)",
            ) != "true"
            {
                assert!(
                    Instant::now() < wake_deadline,
                    "session output before the marker did not wake the pre-registered expect"
                );
                thread::sleep(Duration::from_millis(5));
            }
        }
        if eval(path, &marker_visible) == "true" {
            break Instant::now();
        }
        assert!(
            Instant::now() < marker_deadline,
            "session output marker did not become visible"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let wake_deadline = marker_seen_at + Duration::from_millis(750);
    loop {
        if eval(path, "return remuda._api_v5_expect_handle.state.status") == "matched" {
            break;
        }
        if Instant::now() >= wake_deadline {
            panic!(
                "remuda.expect did not wake promptly; state: {}",
                eval(
                    path,
                    "local h=remuda._api_v5_expect_handle; return tostring(h.state.status)..'|'..tostring(h.branches[1].match)..'|'..tostring(h.state.last_screen:find(h.branches[1].match))..'|'..tostring(h.state.last_screen)"
                )
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
    client::request(
        path,
        &Request::Close {
            name,
            instance_id: None,
            confirm: None,
        },
    )
    .expect("close expect fixture session");
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
        if version == "v5" {
            let private_path = dir.join("private-atomic-write");
            std::fs::write(&private_path, b"old contents").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&private_path, std::fs::Permissions::from_mode(0o644))
                    .unwrap();
            }
            eval(
                &path,
                &format!(
                    "remuda._api_v5_private_write_path = {:?}",
                    private_path.to_string_lossy()
                ),
            );
            #[cfg(unix)]
            {
                std::fs::hard_link(&private_path, dir.join("private-atomic-write.old")).unwrap();
            }
        }
        std::fs::write(&fixture, source).unwrap();
        script::run(&path, &fixture).unwrap_or_else(|error| panic!("{version} fixture: {error}"));
        match version {
            "v1" => {
                wait_for_fixture_output(&path, version, &["42-v1", "64-v1"]);
                eval(
                    &path,
                    "remuda._api_v1_assert_output(); return 'v1 output verified'",
                );
            }
            "v2" => wait_for_fixture_output(&path, version, &["9-v2", "one-v2", "two-v2"]),
            _ => {}
        }
        if version == "v5" {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let private_path = dir.join("private-atomic-write");
                let metadata = std::fs::metadata(&private_path).unwrap();
                assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
                assert_eq!(
                    std::fs::read(&private_path).unwrap(),
                    b"private replacement"
                );
                assert_eq!(
                    std::fs::read(dir.join("private-atomic-write.old")).unwrap(),
                    b"old contents",
                    "atomic rename leaves the prior hard link unchanged"
                );
            }
        }
        if version == "v5" {
            exercise_session_exit_payload(&path);
            exercise_session_output_payload(&path);
            exercise_expect_wakes_on_session_output(&path);
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
