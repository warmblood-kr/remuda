use std::{
    path::PathBuf,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn probe(extra: bool, root: &std::path::Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_remuda-gui"));
    command.arg("--internal-build-id");
    if extra {
        command.arg("unexpected");
    }
    command
        .env("REMUDA_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("REMUDA_BUILD", "runtime-must-not-override")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("build ID probe did not terminate");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn reports_embedded_build_without_starting_gui_or_touching_state() {
    let root = std::env::temp_dir().join(format!("remuda-gui-build-id-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let scratch = Scratch(root);
    let out = probe(false, &scratch.0);
    assert!(out.status.success());
    assert_eq!(
        out.stdout,
        format!("{}\n", remuda_native::dist::BUILD_VERSION).as_bytes()
    );
    assert!(out.stderr.is_empty());
    let invalid = probe(true, &scratch.0);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    for dir in ["runtime", "config", "data", "state", "cache"] {
        assert!(
            !scratch.0.join(dir).exists(),
            "unexpected {dir} side effect"
        );
    }
}

#[test]
#[ignore = "requires CLI and session host built with the same version/build environment"]
fn gui_cli_and_host_identify_the_same_build() {
    let root = std::env::temp_dir().join(format!("remuda-gui-build-triple-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let scratch = Scratch(root);
    let run = |var: &str, arg: &str| {
        let path = std::env::var_os(var).expect("binary path");
        let out = Command::new(path)
            .arg(arg)
            .env("REMUDA_RUNTIME_DIR", scratch.0.join("runtime"))
            .env("XDG_CONFIG_HOME", scratch.0.join("config"))
            .env("XDG_DATA_HOME", scratch.0.join("data"))
            .env("XDG_STATE_HOME", scratch.0.join("state"))
            .env("XDG_CACHE_HOME", scratch.0.join("cache"))
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    let gui = probe(false, &scratch.0);
    assert!(gui.status.success());
    let host = run("REMUDA_GUI_BUILD_HOST", "version");
    let cli = run("REMUDA_GUI_BUILD_CLI", "--version");
    assert_eq!(gui.stdout, host.as_bytes());
    assert_eq!(cli, format!("remuda {host}"));
}
