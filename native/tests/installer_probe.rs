#![cfg(all(target_os = "macos", feature = "installer-probe-experimental"))]

use std::{
    io::Read,
    os::unix::net::UnixListener,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

struct Scratch(PathBuf);
impl Scratch {
    fn new(label: &str) -> Self {
        let path =
            PathBuf::from("/private/tmp").join(format!("rip-{}-{label}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct OwnedChild(std::process::Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn command(mode: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_remuda-installer-probe"));
    cmd.args([
        "--verify",
        "--verified-archive-v1",
        env!("CARGO_BIN_EXE_installer_probe_fixture"),
        "build.1",
    ])
    .env("REMUDA_PROBE_MODE", mode)
    .stdin(Stdio::null());
    cmd
}

#[test]
fn accepts_exact_identity_and_rejects_invalid_and_oversized_output() {
    let ok = command("good").output().unwrap();
    assert!(ok.status.success());
    assert!(ok.stdout.is_empty());
    for mode in ["extra", "no-newline", "stdout", "stderr", "nonzero"] {
        let out = command(mode).output().unwrap();
        assert!(!out.status.success(), "{mode}");
        assert!(out.stdout.is_empty(), "{mode}");
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("xxxxx"),
            "{mode}"
        );
    }
}

#[test]
fn cli_host_and_gui_use_only_their_fixed_probe_protocols() {
    for (flag, argument) in [
        ("--verify", "--internal-build-id"),
        ("--verify-cli", "--version"),
        ("--verify-host", "version"),
    ] {
        let status = Command::new(env!("CARGO_BIN_EXE_remuda-installer-probe"))
            .args([
                flag,
                "--verified-archive-v1",
                env!("CARGO_BIN_EXE_installer_probe_fixture"),
                "build.1",
            ])
            .env("REMUDA_PROBE_MODE", "role")
            .env("REMUDA_PROBE_EXPECT_ARG", argument)
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "{flag}");
    }
    let status = Command::new(env!("CARGO_BIN_EXE_remuda-installer-probe"))
        .args([
            "--verify-cli",
            "--verified-archive-v1",
            env!("CARGO_BIN_EXE_installer_probe_fixture"),
            "build.1",
        ])
        .env("REMUDA_PROBE_MODE", "good")
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "CLI protocol must reject host-style output"
    );
}

#[test]
#[ignore = "requires GUI, CLI and host binaries built with the same build ID"]
fn verifies_actual_build_triple() {
    let build = std::env::var("REMUDA_PROBE_BUILD").expect("build ID");
    for (flag, var) in [
        ("--verify", "REMUDA_PROBE_GUI"),
        ("--verify-cli", "REMUDA_PROBE_CLI"),
        ("--verify-host", "REMUDA_PROBE_HOST"),
    ] {
        let path = std::env::var_os(var).expect("absolute binary path");
        let status = Command::new(env!("CARGO_BIN_EXE_remuda-installer-probe"))
            .args([flag, "--verified-archive-v1"])
            .arg(path)
            .arg(&build)
            .env_remove("REMUDA_PROBE_MODE")
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "{flag}");
    }
}

#[test]
fn requires_explicit_verified_archive_contract_and_safe_key() {
    let scratch = Scratch::new("opt-in");
    let marker = scratch.0.join("started");
    for args in [
        vec![
            "--verify",
            env!("CARGO_BIN_EXE_installer_probe_fixture"),
            "build.1",
        ],
        vec![
            "--verify",
            "--verified-archive-v1",
            env!("CARGO_BIN_EXE_installer_probe_fixture"),
            "../bad",
        ],
        vec![
            "--guard",
            "--verified-archive-v1",
            env!("CARGO_BIN_EXE_installer_probe_fixture"),
            "build.1",
        ],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_remuda-installer-probe"))
            .args(args)
            .env("REMUDA_PROBE_MODE", "good")
            .env("REMUDA_PROBE_MARKER", &marker)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(!marker.exists(), "fixture was started");
    }
}

#[test]
fn hanging_child_is_bounded() {
    let start = Instant::now();
    let out = command("hang").output().unwrap();
    assert!(!out.status.success());
    assert!(start.elapsed() < Duration::from_secs(12));
}

#[test]
fn success_and_rejection_clean_owned_descendants() {
    for mode in ["good-descendant", "bad-descendant"] {
        let scratch = Scratch::new(mode);
        let socket = scratch.0.join("probe.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut parent = OwnedChild(
            command(mode)
                .env("REMUDA_PROBE_SOCKET", &socket)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(e) => panic!("descendant never connected: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        assert_eq!(
            stream.read(&mut byte).unwrap(),
            0,
            "descendant survived {mode}"
        );
        let status = loop {
            if let Some(status) = parent.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "supervisor stuck");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.success(), mode == "good-descendant");
    }
}

#[test]
fn parent_exit_kills_owned_child_without_signalling_unrelated_process() {
    let scratch = Scratch::new("parent-exit");
    let socket = scratch.0.join("probe.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut unrelated = OwnedChild(Command::new("/bin/sleep").arg("30").spawn().unwrap());
    let mut parent = OwnedChild(
        command("socket-parent")
            .env("REMUDA_PROBE_SOCKET", &socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut streams = Vec::new();
    while streams.len() < 2 {
        match listener.accept() {
            Ok((stream, _)) => streams.push(stream),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => panic!("probe never connected: {e}"),
        }
    }
    let mut byte = [0];
    for stream in &mut streams {
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        stream.read_exact(&mut byte).unwrap();
    }
    parent.0.kill().unwrap();
    for stream in &mut streams {
        assert_eq!(
            stream.read(&mut byte).unwrap(),
            0,
            "owned process retained socket"
        );
    }
    assert!(
        unrelated.0.try_wait().unwrap().is_none(),
        "unrelated process was signalled"
    );
}
