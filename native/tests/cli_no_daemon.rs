//! Verbs that must answer without touching any daemon, and a socket path too
//! long to bind that must say so instead of blaming a second daemon. #115.

#![cfg(unix)]

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn remuda(dir: &Path, args: &[&str]) -> Output {
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
    let dir = std::env::temp_dir().join(format!("rnd{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run `args` against a stand-in at the private server's socket that only
/// records connections, hanging up at once so a connecting CLI fails fast.
fn touches_daemon(tag: &str, args: &[&str]) -> (Output, bool) {
    let dir = scratch(tag);
    let socket = remuda_native::daemon::socket_path_in(&dir, "s");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).expect("bind stand-in");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while listener.accept().is_ok() {
            let _ = tx.send(());
        }
    });
    let out = remuda(&dir, args);
    let touched = rx.try_recv().is_ok();
    let _ = std::fs::remove_dir_all(&dir);
    (out, touched)
}

#[test]
fn help_and_version_never_connect_to_a_daemon() {
    // Bare `remuda` here has no tty (output() nulls stdin), so it prints help.
    for args in [
        &["--help"][..],
        &["-h"],
        &["help"],
        &["--version"],
        &["-V"],
        &["version"],
        &[],
    ] {
        let (out, touched) = touches_daemon("help", args);
        assert!(out.status.success(), "{args:?} failed: {out:?}");
        assert!(!touched, "{args:?} connected to the daemon socket");
    }
}

#[test]
fn unknown_cluster_verbs_suggest_the_cluster_path() {
    let dir = scratch("unknown-cluster-verb");
    let out = remuda(&dir, &["nodes"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr.trim(),
        "remuda: unknown command 'nodes'. Did you mean 'remuda cluster nodes'?"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn nearby_top_level_typos_get_did_you_mean() {
    let dir = scratch("unknown-nearby-verb");
    let out = remuda(&dir, &["atach"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr.trim(),
        "remuda: unknown command 'atach'. Did you mean 'remuda attach'?"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unrelated_unknown_verb_gets_concise_help() {
    let dir = scratch("unknown-unrelated-verb");
    let out = remuda(&dir, &["zzzz"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr.trim(),
        "remuda: unknown command 'zzzz'. Run 'remuda help' for commands."
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn help_flags_write_usage_to_stdout_and_mod_commands_to_stderr() {
    let dir = scratch("help-output");
    let mod_dir = dir.join("data/remuda/mods/probe");
    std::fs::create_dir_all(&mod_dir).unwrap();
    std::fs::write(
        mod_dir.join("extension.toml"),
        "name = \"probe\"\nentry = \"packages/probe/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"probe\"\n",
    )
    .unwrap();
    for args in [&["help"][..], &["--help"], &["-h"]] {
        let out = remuda(&dir, args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("terminal orchestration"));
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains("Installed mod commands:\n  remuda probe"),
            "{args:?}: {out:?}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A bad channel fails before any download, after the handshake would have run.
#[test]
fn upgrade_never_connects_to_a_daemon() {
    let (out, touched) = touches_daemon("upgrade", &["upgrade", "--channel", "bogus"]);
    assert!(
        !out.status.success(),
        "a bogus channel was accepted: {out:?}"
    );
    assert!(!touched, "upgrade connected to the daemon socket");
}

#[test]
fn a_socket_path_over_sun_path_names_the_length_not_a_second_daemon() {
    let dir = scratch("long").join("x".repeat(120));
    std::fs::create_dir_all(&dir).unwrap();

    let out = remuda(&dir, &["ls"]);
    let said = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());

    assert!(
        !out.status.success(),
        "ls with an unbindable path succeeded"
    );
    assert!(
        !said.contains("second daemon"),
        "blamed a second daemon: {said}"
    );
    assert!(said.contains("REMUDA_RUNTIME_DIR"), "no cure named: {said}");
}

/// `mod install --reload` reaches the daemon through its own connect path; a
/// local repo stands in for GitHub via git's `insteadOf`.
#[test]
fn mod_reload_over_sun_path_names_the_length_not_a_daemon() {
    let root = scratch("reload");
    let repo = root.join("src/owner/sample.git");
    std::fs::create_dir_all(repo.join("packages/sample")).unwrap();
    std::fs::write(
        repo.join("extension.toml"),
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("packages/sample/init.lua"),
        "return { api = \"remuda-module-v1\", state_version = 1 }",
    )
    .unwrap();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .current_dir(&repo)
            .args(args)
            .status()
            .unwrap();
        assert!(ok.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "sample",
    ]);

    let long = root.join("x".repeat(120));
    std::fs::create_dir_all(&long).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s", "mod", "install", "owner/sample", "--reload"])
        .env("REMUDA_RUNTIME_DIR", &long)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("HOME", &root)
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .env("GIT_CONFIG_COUNT", "1")
        .env(
            "GIT_CONFIG_KEY_0",
            format!("url.file://{}/.insteadOf", root.join("src").display()),
        )
        .env("GIT_CONFIG_VALUE_0", "https://github.com/")
        .output()
        .expect("run remuda");
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        said.contains("installed mod sample"),
        "install itself failed: {said}"
    );
    assert!(
        !said.contains("cannot connect to remuda daemon"),
        "blamed a daemon: {said}"
    );
    assert!(said.contains("REMUDA_RUNTIME_DIR"), "no cure named: {said}");
}

/// #134: a mod directory without a readable manifest (half-swapped during an
/// update) is named, not answered with the generic usage text.
#[test]
fn a_half_installed_mod_is_named_instead_of_generic_usage() {
    for (tag, manifest) in [
        ("nomanifest", None),
        ("badmanifest", Some("this is not toml [")),
    ] {
        let dir = scratch(tag);
        let mod_dir = dir.join("data/remuda/mods/butler");
        std::fs::create_dir_all(mod_dir.join("packages/butler")).unwrap();
        if let Some(text) = manifest {
            std::fs::write(mod_dir.join("extension.toml"), text).unwrap();
        }
        let out = remuda(&dir, &["butler", "--headless"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{tag}: {stderr}");
        assert!(stderr.contains("mod 'butler' at"), "{tag}: {stderr}");
        assert!(
            stderr.contains(&mod_dir.display().to_string()),
            "{tag}: {stderr}"
        );
        assert!(
            stderr.contains("partially installed or mid-update"),
            "{tag}: {stderr}"
        );
        assert!(
            !stderr.contains("terminal orchestration"),
            "{tag}: generic usage: {stderr}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A word that names no mod directory at all gets the concise diagnostic.
    let dir = scratch("unknown-word");
    let out = remuda(&dir, &["nosuchverb"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "remuda: unknown command 'nosuchverb'. Run 'remuda help' for commands."
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// PR #146 / butler-qa half134 rows 5-6: one half-installed mod must not take
/// down its siblings. The scan skips it with a warning naming it, a healthy
/// mod's command still dispatches, and the bad mod's own command still gets
/// the named error.
#[test]
fn a_half_installed_sibling_does_not_hide_healthy_mods() {
    let dir = scratch("sibling");
    let mods = dir.join("data/remuda/mods");
    std::fs::create_dir_all(mods.join("butler/packages/butler")).unwrap(); // no manifest
    std::fs::create_dir_all(mods.join("probe/packages/probe")).unwrap();
    std::fs::write(
        mods.join("probe/extension.toml"),
        "name = \"probe\"\nentry = \"packages/probe/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"probe\"\n",
    )
    .unwrap();
    std::fs::write(
        mods.join("probe/packages/probe/init.lua"),
        "remuda.extension_command('probe', function(args) return 'pong ' .. (args[1] or '') end)",
    )
    .unwrap();

    let help = remuda(&dir, &["help"]);
    let help_err = String::from_utf8_lossy(&help.stderr);
    assert!(
        help_err.contains("probe"),
        "help lists the healthy mod: {help_err}"
    );
    assert!(
        help_err.contains(&mods.join("butler").display().to_string()),
        "and names the bad dir: {help_err}"
    );

    let launch = remuda(&dir, &["probe", "--headless"]);
    let ping = remuda(&dir, &["probe", "ping"]);
    let bad = remuda(&dir, &["butler"]);
    remuda(&dir, &["stop", "-f"]);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        launch.status.success(),
        "{}",
        String::from_utf8_lossy(&launch.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&ping.stdout).trim(),
        "pong ping",
        "{}",
        String::from_utf8_lossy(&ping.stderr)
    );
    let bad_err = String::from_utf8_lossy(&bad.stderr);
    assert!(
        !bad.status.success() && bad_err.contains("mod 'butler' at"),
        "{bad_err}"
    );
}

/// #150: a half-installed dependency manifest names its owner, dependency,
/// and path instead of returning only the raw parse failure.
#[test]
fn a_corrupt_required_manifest_names_owner_dependency_and_path() {
    let dir = scratch("bad-requirement");
    let mods = dir.join("data/remuda/mods");
    let owner = mods.join("A");
    let dependency = mods.join("B");
    std::fs::create_dir_all(owner.join("packages/A")).unwrap();
    std::fs::write(
        owner.join("extension.toml"),
        "name = \"A\"\nversion = \"1.0.0\"\nentry = \"packages/A/init.lua\"\napi = \"remuda-lua-v1\"\nrequires = { B = \">=1.0\" }\n",
    )
    .unwrap();
    std::fs::write(
        owner.join("packages/A/init.lua"),
        "return { api = \"remuda-lua-v1\" }",
    )
    .unwrap();
    std::fs::create_dir_all(dependency.join("packages/B")).unwrap();
    let dependency_manifest = dependency.join("extension.toml");
    std::fs::write(&dependency_manifest, "version = \"1.0.0\"\n").unwrap();

    let out = remuda(&dir, &["exec", "A"]);
    let error = String::from_utf8_lossy(&out.stderr);
    let _ = remuda(&dir, &["stop", "-f"]);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !out.status.success(),
        "executed despite a corrupt dependency manifest"
    );
    assert!(error.contains("A"), "owner is not named: {error}");
    assert!(error.contains("B"), "dependency is not named: {error}");
    assert!(
        error.contains(&dependency_manifest.display().to_string()),
        "manifest path is not named: {error}"
    );
}
