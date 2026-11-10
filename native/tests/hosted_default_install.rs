//! A locally mocked release exercises the real installer and installed pair.
#![cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "daemon_support/hosts.rs"]
mod hosts;
use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon};
use sha2::{Digest, Sha256};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    root: PathBuf,
    child: Option<Child>,
}
impl Fixture {
    fn new() -> Self {
        let base = if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        };
        let root = PathBuf::from(base).join(format!("rhi-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        Self { root, child: None }
    }
    fn command(&self) -> Command {
        let mut c = Command::new(self.root.join("bin/remuda"));
        c.args(["-s", "s"])
            .env("REMUDA_RUNTIME_DIR", &self.root)
            .env("HOME", &self.root)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env_remove("REMUDA_EXPERIMENTAL_SESSION_HOST");
        c
    }
    fn cli(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
    fn socket(&self) -> PathBuf {
        daemon::socket_path_in(&self.root, "s")
    }
    fn request(&self, req: &Request) -> Response {
        client::request(&self.socket(), req).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.root.join("bin/remuda").exists() {
            let _ = self.cli(&["stop", "-f", "--yes"]);
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        hosts::stop_hosts_under(&self.root);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn write_exec(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
fn fake_release(f: &Fixture) {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        _ => return,
    };
    let version = "0.1.0-nightly.20261111000000.abcdef0";
    let asset = format!("remuda-{version}-{target}.tar.gz");
    let files = f.root.join("files");
    let fake = f.root.join("fake");
    std::fs::create_dir(&files).unwrap();
    std::fs::create_dir(&fake).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_remuda"), files.join("remuda")).unwrap();
    std::fs::copy(
        env!("CARGO_BIN_EXE_remuda-session-host"),
        files.join("remuda-session-host"),
    )
    .unwrap();
    assert!(Command::new("tar")
        .args(["-czf"])
        .arg(f.root.join(&asset))
        .args(["-C"])
        .arg(&files)
        .args(["remuda", "remuda-session-host"])
        .status()
        .unwrap()
        .success());
    let digest = Sha256::digest(std::fs::read(f.root.join(&asset)).unwrap());
    std::fs::write(f.root.join("SHA256SUMS"), format!("{digest:x}  {asset}\n")).unwrap();
    std::fs::write(
        f.root.join("latest.json"),
        format!("{{\"nightly\":\"{version}\"}}\n"),
    )
    .unwrap();
    write_exec(&fake.join("curl"), "#!/bin/sh\ncase \"$2\" in */latest.json) cat \"$FAKE_RELEASE/latest.json\";; */SHA256SUMS) cat \"$FAKE_RELEASE/SHA256SUMS\";; *.tar.gz) cat \"$FAKE_RELEASE/${2##*/}\";; *) exit 1;; esac\n");
    let path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", fake.display());
    let out = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/install.sh"))
        .env("HOME", &f.root)
        .env("XDG_DATA_HOME", f.root.join("data"))
        .env("REMUDA_INSTALL_DIR", f.root.join("bin"))
        .env("REMUDA_CHANNEL", "nightly")
        .env("FAKE_RELEASE", &f.root)
        .env("PATH", path)
        .env_remove("REMUDA_INSTALL_BUTLER")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "installer failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(f.root.join("bin/remuda-session-host").is_file());
}
fn screen(f: &Fixture, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Response::Screen(text) = f.request(&Request::Capture { name: "sh".into() }) {
            if text.contains(needle) {
                return text;
            }
        }
        assert!(Instant::now() < deadline, "missing {needle}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Uses the installed old binary pair as immutable input; all writes stay under Fixture.root.
#[test]
#[ignore = "requires REMUDA_COMPAT_PREVIOUS with an earlier CLI and session-host pair"]
fn isolated_installer_upgrade_keeps_old_host_instance_pid_and_input() {
    let previous = PathBuf::from(
        std::env::var_os("REMUDA_COMPAT_PREVIOUS").expect("previous binary directory"),
    );
    let mut f = Fixture::new();
    std::fs::create_dir(f.root.join("bin")).unwrap();
    for name in ["remuda", "remuda-session-host"] {
        std::fs::copy(previous.join(name), f.root.join("bin").join(name)).unwrap();
    }
    let old_version = f.cli(&["--version"]);
    assert!(old_version.status.success());
    let old_host_hash =
        Sha256::digest(std::fs::read(f.root.join("bin/remuda-session-host")).unwrap());
    f.child = Some(
        f.command()
            .env("REMUDA_EXPERIMENTAL_SESSION_HOST", "1")
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while remuda_native::ipc::connect(&f.socket()).is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let Response::SessionStarted { instance_id, .. } = f.request(&Request::NewWithInstance {
        name: Some("sh".into()),
        command: vec!["/bin/sh".into()],
        size: Size::new(80, 24),
        cwd: None,
        env: None,
        remote_input_disabled: false,
    }) else {
        panic!("old host session did not launch");
    };
    assert_eq!(
        f.request(&Request::SendLine {
            name: "sh".into(),
            text: "printf 'PID:%s\\n' \"$$\"".into()
        }),
        Response::Ok
    );
    let original = screen(&f, "PID:");
    let pid: u32 = original
        .lines()
        .find_map(|line| line.strip_prefix("PID:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    fake_release(&f);
    let new_host_hash =
        Sha256::digest(std::fs::read(f.root.join("bin/remuda-session-host")).unwrap());
    assert_ne!(
        old_host_hash, new_host_hash,
        "installer must replace host binary too"
    );
    let new_version = f.cli(&["--version"]);
    assert!(new_version.status.success());
    assert_ne!(
        old_version.stdout, new_version.stdout,
        "installer must replace CLI"
    );
    let reload = f.cli(&["reload"]);
    assert!(
        reload.status.success(),
        "reload: {}{}",
        String::from_utf8_lossy(&reload.stdout),
        String::from_utf8_lossy(&reload.stderr)
    );
    let Response::Sessions(rows) = f.request(&Request::List) else {
        panic!("list");
    };
    assert!(rows
        .iter()
        .any(|row| row.name == "sh" && row.instance_id.as_deref() == Some(&instance_id)));
    assert_eq!(
        f.request(&Request::SendLine {
            name: "sh".into(),
            text: "printf 'AFTER_PID:%s\\n' \"$$\"".into()
        }),
        Response::Ok
    );
    let after = screen(&f, "AFTER_PID:");
    let after_pid: u32 = after
        .lines()
        .find_map(|line| line.strip_prefix("AFTER_PID:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(pid, after_pid, "same process after upgrade");
}
