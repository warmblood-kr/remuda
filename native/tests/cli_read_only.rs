use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s"])
        .args(args)
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("HOME", dir)
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run remuda")
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-read-only-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("make private runtime directory");
    dir
}

#[test]
fn read_only_verbs_without_a_daemon_fail_without_starting_one() {
    for (tag, args) in [
        ("ls", &["ls"][..]),
        ("doc", &["doc", "--format", "markdown"]),
    ] {
        let dir = scratch(tag);
        let socket = remuda_native::daemon::socket_path_in(&dir, "s");
        let output = run(&dir, args);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let message = format!(
            "no daemon running for \"s\" (socket {}); start one with remuda new/run or remuda -e ...",
            socket.display()
        );
        let started = stderr.contains("started a daemon");
        let daemon_alive = remuda_native::ipc::connect(&socket).is_ok();

        // Clean any unexpected auto-start before assertions can fail.
        if daemon_alive {
            let _ = run(&dir, &["stop", "-f"]);
        }
        let socket_exists = cfg!(unix) && socket.exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            !output.status.success(),
            "read-only {tag} succeeded: {output:?}"
        );
        assert!(
            stderr.contains(&message),
            "missing diagnostic {message:?}: {stderr:?}"
        );
        assert!(
            !started,
            "read-only {tag} auto-started the daemon: {stderr:?}"
        );
        assert!(
            !daemon_alive,
            "read-only {tag} left a daemon running at {socket:?}"
        );
        assert!(
            !socket_exists,
            "read-only {tag} left a socket at {socket:?}"
        );
    }
}
