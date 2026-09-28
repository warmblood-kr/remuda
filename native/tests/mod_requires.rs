//! `requires` between mods: install and activation refuse what is missing or
//! too old, `exec` activates hosts before guests, `mod remove` refuses while a
//! dependent is installed. DESIGN §4 "Dependencies, load order, removal".

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rmq{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Record every mod `start` (a mod sees `remuda` read-only, so it emits).
    fn record_starts(&self) {
        let out = self.remuda(&["-e", "remuda._order = ''; remuda.on('started', function(n) remuda._order = remuda._order .. n .. ';' end)"]);
        assert!(out.status.success(), "{}", stderr(&out));
    }

    /// Write an installed lifecycle mod whose `start` emits its name.
    fn installed(&self, name: &str, version: &str, requires: &str) {
        write_mod(
            &self.0.join("data/remuda/mods").join(name),
            name,
            version,
            requires,
        );
    }

    fn remuda(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &self.0)
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("HOME", &self.0)
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .env("GIT_CONFIG_COUNT", "1")
            .env(
                "GIT_CONFIG_KEY_0",
                format!("url.file://{}/.insteadOf", self.0.join("src").display()),
            )
            .env("GIT_CONFIG_VALUE_0", "https://github.com/")
            .output()
            .expect("run remuda")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.remuda(&["stop", "-f"]);
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_mod(root: &Path, name: &str, version: &str, requires: &str) {
    fs::create_dir_all(root.join("packages").join(name)).unwrap();
    fs::write(
        root.join("extension.toml"),
        format!(
            "name = \"{name}\"\nversion = \"{version}\"\nentry = \"packages/{name}/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n{requires}\n"
        ),
    )
    .unwrap();
    fs::write(
        root.join("packages").join(name).join("init.lua"),
        format!(
            "return {{ api = \"remuda-module-v1\", state_version = 1,\n  initialize = function() return {{}} end,\n  start = function() remuda.emit(\"started\", \"{name}\") end }}"
        ),
    )
    .unwrap();
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

#[test]
fn remove_refuses_while_a_dependent_is_installed() {
    let home = Home::new("remove");
    home.installed("host", "1.0.0", "");
    home.installed("guest", "1.0.0", r#"requires = { host = "*" }"#);

    let refused = home.remuda(&["mod", "remove", "host"]);
    assert!(!refused.status.success(), "removed a required mod");
    assert!(
        stderr(&refused).contains("guest"),
        "dependents not named: {}",
        stderr(&refused)
    );
    assert!(
        home.0.join("data/remuda/mods/host").is_dir(),
        "host was removed anyway"
    );

    assert!(home.remuda(&["mod", "remove", "guest"]).status.success());
    assert!(home.remuda(&["mod", "remove", "host"]).status.success());
}

#[test]
fn install_refuses_a_missing_or_too_old_requirement() {
    let home = Home::new("install");
    let repo = home.0.join("src/owner/guest.git");
    write_mod(&repo, "guest", "1.0.0", r#"requires = { host = ">=2" }"#);
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .current_dir(&repo)
            .args(args)
            .status()
            .unwrap()
            .success());
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
        "guest",
    ]);

    let missing = home.remuda(&["mod", "install", "owner/guest"]);
    assert!(
        !missing.status.success(),
        "installed without its requirement"
    );
    assert!(
        stderr(&missing).contains("host") && stderr(&missing).contains("not installed"),
        "{}",
        stderr(&missing)
    );

    home.installed("host", "1.5.0", "");
    let old = home.remuda(&["mod", "install", "owner/guest"]);
    assert!(
        !old.status.success(),
        "installed against a too-old requirement"
    );
    assert!(
        stderr(&old).contains("1.5.0") && stderr(&old).contains(">=2"),
        "{}",
        stderr(&old)
    );
    assert!(
        !home.0.join("data/remuda/mods/guest").exists(),
        "a refused install left files"
    );

    home.installed("host", "2.0.0", "");
    let ok = home.remuda(&["mod", "install", "owner/guest"]);
    assert!(ok.status.success(), "{}", stderr(&ok));
}

#[test]
fn exec_activates_requirements_first_and_only_once() {
    let home = Home::new("exec");
    home.installed("base", "1.0.0", "");
    home.installed("host", "1.0.0", r#"requires = { base = "*" }"#);
    home.installed(
        "guest",
        "1.0.0",
        r#"requires = { host = ">=1", base = "*" }"#,
    );

    home.record_starts();
    assert!(home.remuda(&["exec", "guest"]).status.success());
    assert!(home.remuda(&["exec", "guest"]).status.success());
    let order = home.remuda(&["-e", "return remuda._order"]);
    assert_eq!(
        String::from_utf8_lossy(&order.stdout).trim(),
        "base;host;guest;"
    );
}

#[test]
fn exec_refuses_a_missing_too_old_or_cyclic_requirement() {
    let home = Home::new("refuse");
    home.record_starts();
    home.installed("lonely", "1.0.0", r#"requires = { absent = "*" }"#);
    let missing = home.remuda(&["exec", "lonely"]);
    assert!(!missing.status.success());
    assert!(
        stderr(&missing).contains("absent") && stderr(&missing).contains("not installed"),
        "{}",
        stderr(&missing)
    );

    home.installed("old", "0.9.0", "");
    home.installed("picky", "1.0.0", r#"requires = { old = ">=1" }"#);
    let old = home.remuda(&["exec", "picky"]);
    assert!(!old.status.success());
    assert!(
        stderr(&old).contains("0.9.0") && stderr(&old).contains(">=1"),
        "{}",
        stderr(&old)
    );

    home.installed("ping", "1.0.0", r#"requires = { pong = "*" }"#);
    home.installed("pong", "1.0.0", r#"requires = { ping = "*" }"#);
    let cycle = home.remuda(&["exec", "ping"]);
    assert!(!cycle.status.success());
    assert!(
        stderr(&cycle).contains("cycle") && stderr(&cycle).contains("ping"),
        "{}",
        stderr(&cycle)
    );
    let order = home.remuda(&["-e", "return remuda._order"]);
    assert_eq!(
        String::from_utf8_lossy(&order.stdout).trim(),
        "",
        "nothing activated"
    );
}
