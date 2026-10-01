//! The binary reads mods from `%LOCALAPPDATA%\remuda`, and channel, mods and
//! cache stay readable once the cluster has made `remuda\cluster` private.
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

/// Data made under `%LOCALAPPDATA%\remuda` must stay readable once the
/// cluster has created its private `remuda\cluster` there.
#[test]
fn data_stays_readable_after_the_cluster_starts() {
    let root = scratch("acl");
    let local = root.join("박 정수").join("AppData Local");
    let mod_root = write_mod(&local, "hello");
    let cache = local.join("remuda").join("cache");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("update-check.json"), "{}").unwrap();
    let channel = local.join("remuda").join("channel");
    fs::write(&channel, "nightly\n").unwrap();

    let daemon = spawn::spawn_and_wait(
        {
            let mut cmd = spawn::base_command(&root);
            cmd.env("LOCALAPPDATA", &local).env_remove("XDG_DATA_HOME");
            cmd
        },
        &root,
    );
    let init = remuda(&root, &local, &["cluster", "init", "--no-listen"]);
    assert!(init.status.success(), "cluster init failed: {init:?}");
    drop(daemon);
    assert!(local.join("remuda/cluster/identity.key").is_file());

    assert_eq!(fs::read_to_string(&channel).unwrap(), "nightly\n");
    assert!(fs::read(mod_root.join("extension.toml")).is_ok());
    assert!(fs::read(mod_root.join("packages/hello/init.lua")).is_ok());
    assert!(fs::read(cache.join("update-check.json")).is_ok());
    let out = remuda(&root, &local, &["mod", "list"]);
    assert!(lists(&out, "hello"), "mod no longer resolves: {out:?}");
    let _ = fs::remove_dir_all(&root);
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
