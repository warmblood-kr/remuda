//! A session client keeps using the runtime directory chosen by its daemon.

#[cfg(unix)]
mod unix {
    use remuda_core::protocol::{Request, Response};
    use remuda_core::Size;
    use remuda_native::{client, daemon, ipc};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let base = if Path::new("/private/tmp").is_dir() {
                PathBuf::from("/private/tmp")
            } else {
                std::env::temp_dir()
                    .canonicalize()
                    .expect("resolve real temporary directory")
            };
            let root = base.join(format!("r453-{}", std::process::id()));
            let mut builder = std::fs::DirBuilder::new();
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
                .create(&root)
                .expect("create private scratch directory");
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    fn create_private_dir(path: &Path) {
        let mut builder = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(path).expect("create private test directory");
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Daemon(Child);

    impl Daemon {
        fn start(runtime: &Path, home: &Path, server: &str) -> Self {
            let child = Command::new(env!("CARGO_BIN_EXE_remuda"))
                .args(["-s", server, "daemon"])
                .env("HOME", home)
                .env("XDG_RUNTIME_DIR", runtime)
                .env_remove("REMUDA_RUNTIME_DIR")
                .env_remove("XDG_CONFIG_HOME")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start isolated daemon");
            let socket = daemon::socket_path_in(runtime, server);
            let mut daemon = Self(child);
            let deadline = Instant::now() + Duration::from_secs(10);
            while ipc::connect(&socket).is_err() {
                if let Ok(Some(status)) = daemon.0.try_wait() {
                    panic!("isolated daemon exited before binding {socket:?}: {status}");
                }
                assert!(
                    Instant::now() < deadline,
                    "isolated daemon never bound {socket:?}"
                );
                thread::sleep(Duration::from_millis(10));
            }
            daemon
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn daemon_process_count(server: &str) -> usize {
        let output = Command::new("ps")
            .args(["-axo", "command="])
            .output()
            .expect("list processes started by this test");
        assert!(output.status.success(), "ps failed: {output:?}");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.contains(&format!("-s {server} daemon")))
            .count()
    }

    fn wait_for_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(Instant::now() < deadline, "session command did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn session_client_uses_daemon_runtime_when_xdg_is_unset() {
        let scratch = Scratch::new();
        let runtime = scratch.path().join("a");
        let home = scratch.path().join("h");
        create_private_dir(&runtime);
        create_private_dir(&home);
        let runtime = runtime
            .canonicalize()
            .expect("resolve XDG runtime directory");
        let home = home.canonicalize().expect("resolve scratch home");
        let server = format!("s{}", std::process::id());
        let daemon = Daemon::start(&runtime, &home, &server);
        let socket = daemon::socket_path_in(&runtime, &server);
        let list_output = scratch.path().join("list.out");
        let status_output = scratch.path().join("status.out");
        let exec_output = scratch.path().join("exec.out");
        let exec_status = scratch.path().join("exec-status.out");
        let mut env = HashMap::new();
        env.insert("HOME".into(), home.display().to_string());
        env.insert("TEST_HOME".into(), home.display().to_string());
        env.insert("TEST_XDG_RUNTIME_DIR".into(), runtime.display().to_string());
        env.insert("REMUDA_BIN".into(), env!("CARGO_BIN_EXE_remuda").into());
        env.insert("SERVER".into(), server.clone());
        env.insert("LIST_OUTPUT".into(), list_output.display().to_string());
        env.insert("STATUS_OUTPUT".into(), status_output.display().to_string());
        env.insert("EXEC_OUTPUT".into(), exec_output.display().to_string());
        env.insert("EXEC_STATUS".into(), exec_status.display().to_string());
        let script = r#"
test "$HOME" = "$TEST_HOME" || exit 90
test "$XDG_RUNTIME_DIR" = "$TEST_XDG_RUNTIME_DIR" || exit 91
unset XDG_RUNTIME_DIR
"$REMUDA_BIN" -s "$SERVER" ls >"$LIST_OUTPUT" 2>&1
printf '%s\n' "$?" >"$STATUS_OUTPUT"
"$REMUDA_BIN" -s "$SERVER" exec "missing-$SERVER" >"$EXEC_OUTPUT" 2>&1
printf '%s\n' "$?" >"$EXEC_STATUS"
"#;
        assert_eq!(
            client::request(
                &socket,
                &Request::New {
                    name: Some("session-client".into()),
                    command: vec!["sh".into(), "-c".into(), script.into()],
                    size: Size::new(80, 24),
                    cwd: None,
                    env: Some(env),
                },
            )
            .expect("launch client session"),
            Response::Value("session-client".into())
        );

        wait_for_file(&status_output);
        wait_for_file(&exec_status);
        let status = std::fs::read_to_string(&status_output).expect("read client status");
        let output = std::fs::read_to_string(&list_output).expect("read client output");
        let exec_status = std::fs::read_to_string(&exec_status).expect("read exec status");
        let exec_output = std::fs::read_to_string(&exec_output).expect("read exec output");
        assert_eq!(
            daemon_process_count(&server),
            1,
            "a second daemon was started"
        );
        assert_eq!(
            status.trim(),
            "0",
            "remuda ls failed inside the session: {output}"
        );
        assert!(
            output.contains("session-client"),
            "client reached the wrong daemon: {output}"
        );
        assert_ne!(
            exec_status.trim(),
            "0",
            "exec of a missing package unexpectedly succeeded"
        );
        assert!(
            exec_output.contains(&format!("no such package: missing-{server}")),
            "daemon-starting command did not reach the original daemon: {exec_output}"
        );
        drop(daemon);
    }
}
