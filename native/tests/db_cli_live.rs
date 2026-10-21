//! Live coverage of `remuda db` (#778): a mod applies its shipped migration
//! files, and the daemon reports, steps back (after a backup) and upgrades.

#![cfg(unix)]

#[path = "daemon_support/hosts.rs"]
mod hosts;
use remuda_core::protocol::Request;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Home {
    root: PathBuf,
    name: String,
    daemon: Option<Child>,
}

impl Home {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("dbl{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        Self {
            root,
            name: format!("db{}", std::process::id()),
            daemon: None,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command
            .args(["-s", &self.name])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", self.root.join("runtime"))
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("REMUDA_NO_UPDATE_CHECK", "1");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).stdin(Stdio::null()).output().unwrap()
    }

    fn socket(&self) -> PathBuf {
        remuda_native::daemon::socket_path_in(&self.root.join("runtime"), &self.name)
    }

    fn start(&mut self) {
        let child = self
            .command(&["daemon"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while remuda_native::client::request(&self.socket(), &Request::List).is_err() {
            assert!(Instant::now() < deadline, "daemon did not start");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn install_mod(&self) {
        let package = self.root.join("data/remuda/mods/sample/packages/sample");
        let migrations = package.join("migrations/notes");
        std::fs::create_dir_all(&migrations).unwrap();
        std::fs::write(
            self.root.join("data/remuda/mods/sample/extension.toml"),
            "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
        )
        .unwrap();
        for (file, sql) in [
            ("0001_create.up.sql", "CREATE TABLE n (x TEXT NOT NULL);"),
            ("0001_create.down.sql", "DROP TABLE n;"),
            (
                "0002_seen.up.sql",
                "ALTER TABLE n ADD COLUMN seen INTEGER NOT NULL DEFAULT 0;",
            ),
            ("0002_seen.down.sql", "ALTER TABLE n DROP COLUMN seen;"),
        ] {
            std::fs::write(migrations.join(file), sql).unwrap();
        }
        std::fs::write(
            package.join("init.lua"),
            r#"return {
              api = "remuda-module-v1", state_version = 1,
              initialize = function() return {} end,
              start = function(state)
                local db = assert(remuda.db.open("notes", { migrations = true }))
                assert(db:exec("INSERT INTO n (x) VALUES ('row')"))
                db:close()
              end,
            }"#,
        )
        .unwrap();
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        self.stop();
        hosts::stop_hosts_under(&self.root);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn ok(output: Output, what: &str) -> String {
    assert!(
        output.status.success(),
        "{what} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn backups(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".bak"))
        .collect()
}

#[test]
fn db_status_downgrade_and_upgrade_run_in_the_daemon() {
    let mut home = Home::new();
    home.install_mod();

    // No daemon: one line, exit 1, nothing created.
    let output = home.run(&["db", "status"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("needs the remuda daemon"), "{text}");

    home.start();
    ok(home.run(&["exec", "sample"]), "exec sample");
    let status = ok(home.run(&["db", "status"]), "db status");
    assert!(
        status.starts_with("sample/notes.db  version 2, can step back to 0; files: up to date"),
        "{status}"
    );

    let plan = ok(
        home.run(&["db", "downgrade", "sample", "notes", "1"]),
        "dry run",
    );
    assert!(plan.contains("undo steps 2 (seen)"), "{plan}");
    let dir = home.root.join("state/remuda/db/sample");
    assert!(backups(&dir).is_empty());

    let done = ok(
        home.run(&["db", "downgrade", "sample", "notes", "1", "--yes"]),
        "downgrade",
    );
    assert!(
        done.contains("sample/notes.db is now at version 1 (was 2)."),
        "{done}"
    );
    let copies = backups(&dir);
    assert_eq!(copies.len(), 1);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&copies[0]).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let status = ok(home.run(&["db", "status", "sample"]), "status after");
    assert!(
        status.contains("version 1, can step back to 0; files: 1 pending"),
        "{status}"
    );

    let up = ok(home.run(&["db", "upgrade", "sample", "notes"]), "upgrade");
    assert_eq!(up, "sample/notes.db is now at version 2 (was 1).\n");
}
