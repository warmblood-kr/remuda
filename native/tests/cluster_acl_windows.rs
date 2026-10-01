//! Windows: a base `remuda` dir that another account can write to makes the
//! cluster refuse. The daemon and everything outside the cluster keep working,
//! and nobody's ACL is changed.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn remuda(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", self.root.join("run"))
            .env("HOME", &self.root)
            .env("LOCALAPPDATA", self.root.join("local"))
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .output()
            .expect("run remuda")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.remuda(&["stop", "-f"]);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn icacls(path: &Path, args: &[&str]) -> String {
    let output = Command::new("icacls")
        .arg(path)
        .args(args)
        .output()
        .expect("run icacls");
    assert!(output.status.success(), "icacls {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_refused_base_dir_stops_only_the_cluster() {
    let scratch = Scratch {
        root: std::env::temp_dir().join(format!("remuda-acl-refusal-{}", std::process::id())),
    };
    let _ = std::fs::remove_dir_all(&scratch.root);
    let base = scratch.root.join("local").join("remuda");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::create_dir_all(scratch.root.join("run")).unwrap();
    // Everyone (S-1-1-0) may modify the base: add, delete, write.
    icacls(&base, &["/grant", "*S-1-1-0:(M)"]);
    let acl_before = icacls(&base, &[]);

    // The daemon boots (its listener reads the cluster config) and still runs
    // Lua: the refusal is not a daemon failure.
    let booted = scratch.remuda(&["-e", "return 1 + 1"]);
    assert!(booted.status.success(), "{booted:?}");
    assert_eq!(String::from_utf8_lossy(&booted.stdout).trim(), "2");

    let status = scratch.remuda(&["cluster", "status"]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        said.contains("S-1-1-0"),
        "the refusal names the SID: {said}"
    );
    assert!(
        said.contains("Next: "),
        "the refusal says what to do: {said}"
    );

    let still_running = scratch.remuda(&["-e", "return 2 + 2"]);
    assert!(still_running.status.success(), "{still_running:?}");
    assert_eq!(String::from_utf8_lossy(&still_running.stdout).trim(), "4");

    assert!(!base.join("cluster").exists(), "cluster was created");
    assert_eq!(icacls(&base, &[]), acl_before, "the base ACL was changed");
}
