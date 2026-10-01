//! Windows: `%LOCALAPPDATA%\remuda` is shared by the cluster's private
//! `cluster` dir and by plain data next to it. The cluster leaves the base
//! dir's ACL alone, and refuses, changing nothing, when another account can
//! change the base. Runs on `windows-latest` only.
#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("remuda-acl-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("run")).unwrap();
        let scratch = Self { root };
        fs::create_dir_all(scratch.base()).unwrap();
        scratch
    }

    /// A profile path with a space and non-ASCII letters, as real ones have.
    fn local(&self) -> PathBuf {
        self.root.join("박 정수").join("AppData Local")
    }

    fn base(&self) -> PathBuf {
        self.local().join("remuda")
    }

    fn remuda(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", self.root.join("run"))
            .env("HOME", &self.root)
            .env("LOCALAPPDATA", self.local())
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .output()
            .expect("run remuda")
    }

    /// The daemon boots (its listener reads the cluster config) and runs Lua.
    fn assert_daemon_answers(&self) {
        let answer = self.remuda(&["-e", "return 1 + 1"]);
        assert!(answer.status.success(), "{answer:?}");
        assert_eq!(String::from_utf8_lossy(&answer.stdout).trim(), "2");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.remuda(&["stop", "-f"]);
        let _ = fs::remove_dir_all(&self.root);
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
    let scratch = Scratch::new("refusal");
    let base = scratch.base();
    // Everyone (S-1-1-0) may modify the base: add, delete, write.
    icacls(&base, &["/grant", "*S-1-1-0:(M)"]);
    let acl_before = icacls(&base, &[]);

    scratch.assert_daemon_answers();
    let init = scratch.remuda(&["cluster", "init", "--no-listen"]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(
        !init.status.success(),
        "cluster init was not refused: {said}"
    );
    assert!(
        said.contains("S-1-1-0"),
        "the refusal names the SID: {said}"
    );
    assert!(
        said.contains("Next: "),
        "the refusal says what to do: {said}"
    );

    // The refusal is not a daemon failure.
    scratch.assert_daemon_answers();
    assert!(!base.join("cluster").exists(), "cluster was created");
    assert_eq!(icacls(&base, &[]), acl_before, "the base ACL was changed");
}

// The shape of PR 414's `data_made_before_the_cluster_hardens_stays_readable`:
// plain data is in the base before the daemon starts and the cluster is
// initialized, and the same user must still list, read and write it.
#[test]
fn data_made_before_cluster_init_stays_usable() {
    let scratch = Scratch::new("data-first");
    let base = scratch.base();
    let mods = base.join("mods");
    let hello = mods.join("hello");
    let manifest = hello.join("extension.toml");
    let channel = base.join("channel");
    let cache = base.join("cache");
    let cached = cache.join("update-check.json");
    fs::create_dir_all(&hello).unwrap();
    fs::create_dir_all(&cache).unwrap();
    fs::write(&manifest, "name = \"hello\"\n").unwrap();
    fs::write(&channel, "nightly\n").unwrap();
    fs::write(&cached, "{}").unwrap();
    let acl_before = icacls(&base, &[]);

    scratch.assert_daemon_answers();
    let init = scratch.remuda(&["cluster", "init", "--no-listen"]);

    // Collected, so one CI run reports every path, each with its ACL.
    let mut failures = Vec::new();
    let mut check = |what: &str, path: &Path, result: std::io::Result<()>| {
        if let Err(error) = result {
            let acl = Command::new("icacls").arg(path).output();
            failures.push(format!("{what} {}: {error}\n{acl:?}", path.display()));
        }
    };
    for dir in [&base, &mods, &hello, &cache] {
        check(
            "list",
            dir,
            fs::read_dir(dir).map(|entries| entries.for_each(drop)),
        );
    }
    for file in [&manifest, &channel, &cached] {
        check("read", file, fs::read(file).map(drop));
    }
    check("rewrite", &channel, fs::write(&channel, "stable\n"));
    check("rewrite", &cached, fs::write(&cached, "{\"checked\":1}"));
    for dir in [&base, &mods, &cache] {
        let new_file = dir.join("made-later.txt");
        check("create", &new_file, fs::write(&new_file, "new"));
    }
    let new_dir = mods.join("made-later");
    check("create dir", &new_dir, fs::create_dir(&new_dir));

    assert!(init.status.success(), "cluster init failed: {init:?}");
    assert!(
        base.join("cluster").join("identity.key").is_file(),
        "cluster init wrote no identity.key"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(icacls(&base, &[]), acl_before, "the base ACL was changed");
}
