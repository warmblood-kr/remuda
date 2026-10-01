//! On Windows the data home is `%LOCALAPPDATA%`, shared with the cluster's
//! hardened `remuda` directory. Runs on `windows-latest` only.
#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "daemon_support/spawn.rs"]
mod spawn;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wdh{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_mod(local: &Path, name: &str) -> PathBuf {
    let root = local.join("remuda").join("mods").join(name);
    fs::create_dir_all(root.join("packages").join(name)).unwrap();
    fs::write(
        root.join("extension.toml"),
        format!("name = \"{name}\"\nversion = \"1.0.0\"\nentry = \"packages/{name}/init.lua\"\napi = \"remuda-lua-v1\"\n"),
    )
    .unwrap();
    fs::write(
        root.join("packages").join(name).join("init.lua"),
        "return {}",
    )
    .unwrap();
    root
}

fn remuda(runtime: &Path, local: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s"])
        .args(args)
        .env("REMUDA_RUNTIME_DIR", runtime)
        .env("LOCALAPPDATA", local)
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .env_remove("XDG_DATA_HOME")
        .env_remove("HOME")
        .output()
        .expect("run remuda")
}

fn icacls(path: &Path) -> String {
    match Command::new("icacls").arg(path).output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
        Err(error) => format!("icacls failed to run: {error}"),
    }
}

/// `icacls` of each path that exists, one block per path.
fn snapshot(paths: &[&Path]) -> String {
    paths
        .iter()
        .map(|path| icacls(path))
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_cache(local: &Path) -> PathBuf {
    let cache = local.join("remuda").join("cache");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("update-check.json"), "{}").unwrap();
    cache
}

/// Collects instead of stopping, so one CI run reports everything. A failure
/// carries `icacls` of the path and of its parent.
struct Failures(Vec<String>);

impl Failures {
    fn check<T>(&mut self, what: &str, path: &Path, result: std::io::Result<T>) {
        if let Err(error) = result {
            let parent = path.parent().unwrap_or(path);
            self.0.push(format!(
                "{what} {}: {error}\n{}{}",
                path.display(),
                icacls(path),
                icacls(parent)
            ));
        }
    }
}

/// Create the data (`create_first`) before the cluster hardens `remuda`, or
/// after, then say what is readable and what the DACLs look like. Never
/// changes cluster code: a failure here is a finding, not something to fix.
fn scenario(tag: &str, create_first: bool) {
    let root = scratch(tag);
    let local = root.join("박 정수").join("AppData Local");
    fs::create_dir_all(&local).unwrap();
    let remuda_dir = local.join("remuda");
    let channel = remuda_dir.join("channel");
    let mods = remuda_dir.join("mods");
    let hello = mods.join("hello");
    let cache = remuda_dir.join("cache");
    let paths = [&remuda_dir, &channel, &mods, &hello, &cache];
    let create = || {
        write_mod(&local, "hello");
        fs::write(&channel, "nightly\n").unwrap();
        write_cache(&local);
    };
    if create_first {
        create();
    }
    let before_dacl = icacls(&remuda_dir);
    let before = snapshot(&paths.map(|p| p.as_path()));

    let daemon = spawn::spawn_and_wait(
        {
            let mut cmd = spawn::base_command(&root);
            cmd.env("LOCALAPPDATA", &local).env_remove("XDG_DATA_HOME");
            cmd
        },
        &root,
    );
    let init = remuda(&root, &local, &["cluster", "init", "--no-listen"]);
    let mut failures = Failures(Vec::new());
    if !init.status.success() {
        failures.0.push(format!("cluster init failed: {init:?}"));
    }
    if !create_first {
        create();
    }
    drop(daemon);
    let after = snapshot(&paths.map(|p| p.as_path()));

    let dacl = icacls(&remuda_dir);
    if !local.join("remuda/cluster/identity.key").is_file() {
        failures.0.push("cluster init wrote no identity.key".into());
    }
    if create_first && dacl == before_dacl {
        failures.0.push("the remuda DACL did not change".into());
    }
    if dacl.contains("(OI)") || dacl.matches(":(").count() != 1 {
        failures
            .0
            .push(format!("remuda is not owner-only:\n{dacl}"));
    }
    failures.check("read channel", &channel, fs::read_to_string(&channel));
    for file in [
        hello.join("extension.toml"),
        hello.join("packages/hello/init.lua"),
        cache.join("update-check.json"),
    ] {
        failures.check("read", &file, fs::read(&file));
    }
    failures.check(
        "list cache",
        &cache,
        fs::read_dir(&cache).map(|d| d.count()),
    );
    failures.check("list mods", &mods, fs::read_dir(&mods).map(|d| d.count()));
    let out = remuda(&root, &local, &["mod", "list"]);
    if !lists(&out, "hello") {
        failures
            .0
            .push(format!("mod list does not show hello: {out:?}"));
    }

    let report = format!(
        "{}\n--- icacls before:\n{before}\n--- icacls after:\n{after}",
        failures.0.join("\n")
    );
    let _ = fs::remove_dir_all(&root);
    assert!(failures.0.is_empty(), "{report}");
}

#[test]
fn data_made_before_the_cluster_hardens_stays_readable() {
    scenario("acl-before", true);
}

#[test]
fn data_made_after_the_cluster_hardened_is_readable() {
    scenario("acl-after", false);
}

fn lists(out: &Output, name: &str) -> bool {
    out.status.success() && String::from_utf8_lossy(&out.stdout).contains(name)
}

#[test]
fn the_binary_reads_mods_from_localappdata() {
    let root = scratch("mods");
    let local = root.join("박 정수").join("AppData Local");
    fs::create_dir_all(&local).unwrap();
    write_mod(&local, "hello");
    let out = remuda(&root, &local, &["mod", "list"]);
    assert!(lists(&out, "hello"), "mod not listed: {out:?}");
    let _ = fs::remove_dir_all(&root);
}
