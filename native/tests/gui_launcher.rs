//! The GUI launcher must not contact a daemon or depend on PATH.
use std::{path::PathBuf, process::Command};

struct Scratch(PathBuf);
impl Scratch {
    fn new(label: &str) -> Self {
        let base = if cfg!(target_os = "macos") {
            PathBuf::from("/private/tmp")
        } else {
            std::env::temp_dir()
        };
        let path = base.join(format!("remuda-gui-launch-{}-{label}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn command(&self) -> Command {
        let mut command = Command::new(
            self.0
                .join(format!("remuda{}", std::env::consts::EXE_SUFFIX)),
        );
        command
            .env("REMUDA_RUNTIME_DIR", self.0.join("runtime"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_CACHE_HOME", self.0.join("cache"));
        command
    }
    fn install_cli(&self) {
        std::fs::copy(
            env!("CARGO_BIN_EXE_remuda"),
            self.0
                .join(format!("remuda{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();
    }
    fn assert_untouched(&self) {
        for name in ["runtime", "config", "data", "state", "cache"] {
            assert!(!self.0.join(name).exists(), "unexpected {name} side effect");
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::os::unix::{fs::PermissionsExt, process::ExitStatusExt};
    fn sibling(scratch: &Scratch, contents: &str) {
        let path = scratch.0.join("remuda-gui");
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[test]
    fn missing_help_invalid_args_and_named_server_never_start_a_daemon() {
        let scratch = Scratch::new("preflight");
        scratch.install_cli();
        let help = scratch.command().args(["gui", "--help"]).output().unwrap();
        assert!(
            help.status.success(),
            "{}",
            String::from_utf8_lossy(&help.stderr)
        );
        assert!(String::from_utf8_lossy(&help.stdout).contains("gui"));
        let missing = scratch.command().arg("gui").output().unwrap();
        assert!(!missing.status.success());
        assert!(String::from_utf8_lossy(&missing.stderr).contains("not installed beside"));
        let unrelated = scratch.0.join("unrelated-bin");
        std::fs::create_dir(&unrelated).unwrap();
        std::fs::write(
            unrelated.join("remuda-gui"),
            "#!/bin/sh\n: > \"$REMUDA_GUI_TEST_MARKER\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            unrelated.join("remuda-gui"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let marker = scratch.0.join("path-launched");
        let missing = scratch
            .command()
            .arg("gui")
            .env("PATH", unrelated)
            .env("REMUDA_GUI_TEST_MARKER", &marker)
            .output()
            .unwrap();
        assert!(!missing.status.success());
        assert!(!marker.exists(), "launcher searched PATH");
        sibling(&scratch, "#!/bin/sh\nexit 37\n");
        for args in [
            &["gui", "unexpected"][..],
            &["-s", "other", "gui"][..],
            &["--stdin", "gui"][..],
        ] {
            let output = scratch.command().args(args).output().unwrap();
            assert!(!output.status.success());
            assert_ne!(output.status.code(), Some(37));
        }
        scratch.assert_untouched();
    }
    #[test]
    fn sibling_replaces_cli_and_receives_environment_and_exit_status() {
        let scratch = Scratch::new("exec");
        scratch.install_cli();
        sibling(
            &scratch,
            "#!/bin/sh\nprintf '%s\\n%s\\n' \"$$\" \"$REMUDA_RUNTIME_DIR\"\nexit 37\n",
        );
        let child = scratch
            .command()
            .arg("gui")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(37));
        let stdout = String::from_utf8(output.stdout).unwrap();
        let mut lines = stdout.lines();
        assert_eq!(lines.next().unwrap(), pid.to_string());
        assert_eq!(
            lines.next().unwrap(),
            scratch.0.join("runtime").to_str().unwrap()
        );
        let explicit = scratch
            .command()
            .args(["-s", "default", "gui"])
            .output()
            .unwrap();
        assert_eq!(explicit.status.code(), Some(37));
        scratch.assert_untouched();
    }
    #[test]
    fn sibling_signal_status_is_preserved() {
        let scratch = Scratch::new("signal");
        scratch.install_cli();
        sibling(&scratch, "#!/bin/sh\nkill -TERM $$\n");
        let output = scratch.command().arg("gui").output().unwrap();
        assert_eq!(output.status.signal(), Some(libc::SIGTERM));
        scratch.assert_untouched();
    }
    #[test]
    fn installed_gui_mod_retains_command_and_named_server() {
        let scratch = Scratch::new("mod-owner");
        scratch.install_cli();
        sibling(&scratch, "#!/bin/sh\nexit 37\n");
        let package = scratch.0.join("data/remuda/mods/other");
        std::fs::create_dir_all(package.join("packages/other")).unwrap();
        std::fs::write(package.join("extension.toml"), "name = \"other\"\nentry = \"packages/other/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"gui\"\n").unwrap();
        std::fs::write(package.join("packages/other/init.lua"), "return {}\n").unwrap();
        for args in [
            &["-s", "other", "gui", "--headless"][..],
            &["--stdin", "gui", "ping"][..],
        ] {
            let output = scratch
                .command()
                .args(args)
                .env("REMUDA_NO_AUTOSTART", "1")
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert_ne!(output.status.code(), Some(37));
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("no daemon running"),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(!scratch.0.join("runtime").exists());
    }
    #[test]
    fn installed_gui_mod_executes_its_handler_on_named_daemon() {
        struct Daemon(std::process::Child);
        impl Drop for Daemon {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let scratch = Scratch::new("real-mod");
        scratch.install_cli();
        sibling(&scratch, "#!/bin/sh\nexit 37\n");
        let package = scratch.0.join("data/remuda/mods/other");
        std::fs::create_dir_all(package.join("packages/other")).unwrap();
        std::fs::create_dir_all(scratch.0.join("runtime")).unwrap();
        // An unrelated partial install must not hide a known valid owner.
        std::fs::create_dir_all(scratch.0.join("data/remuda/mods/broken")).unwrap();
        std::fs::write(package.join("extension.toml"), "name = \"other\"\nentry = \"packages/other/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"gui\"\n").unwrap();
        std::fs::write(package.join("packages/other/init.lua"), "remuda.extension_command('gui', function(args) return 'pong ' .. (args[1] or '') end)\n").unwrap();
        let mut daemon = Daemon(
            scratch
                .command()
                .args(["-s", "other", "daemon"])
                .env("REMUDA_EXPERIMENTAL_SESSION_HOST", "0")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let socket = remuda_native::daemon::socket_path_in(&scratch.0.join("runtime"), "other");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !socket.exists() {
            assert!(daemon.0.try_wait().unwrap().is_none(), "daemon exited");
            assert!(std::time::Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let output = scratch
            .command()
            .args(["-s", "other", "gui", "ping"])
            .env("REMUDA_NO_AUTOSTART", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "pong ping");
    }
    #[test]
    fn half_installed_gui_and_uncertain_mods_do_not_launch_gui() {
        for name in ["gui", "unknown"] {
            let scratch = Scratch::new(name);
            scratch.install_cli();
            sibling(&scratch, "#!/bin/sh\nexit 37\n");
            std::fs::create_dir_all(scratch.0.join("data/remuda/mods").join(name)).unwrap();
            let output = scratch.command().arg("gui").output().unwrap();
            assert!(!output.status.success());
            assert_ne!(output.status.code(), Some(37));
            let stderr = String::from_utf8_lossy(&output.stderr);
            if name == "gui" {
                assert!(stderr.contains("mod 'gui' at"), "{stderr}");
            } else {
                assert!(
                    stderr.contains("cannot determine whether a mod owns gui"),
                    "{stderr}"
                );
            }
            assert!(!scratch.0.join("runtime").exists());
        }
    }
    #[test]
    fn uninspectable_mod_directory_does_not_launch_gui() {
        let scratch = Scratch::new("uninspectable");
        scratch.install_cli();
        sibling(&scratch, "#!/bin/sh\nexit 37\n");
        std::fs::write(scratch.0.join("data"), "not a directory").unwrap();
        let output = scratch.command().arg("gui").output().unwrap();
        assert!(!output.status.success());
        assert_ne!(output.status.code(), Some(37));
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("cannot determine whether a mod owns gui"));
    }
    #[test]
    fn dangling_mod_directory_does_not_launch_gui() {
        let scratch = Scratch::new("dangling");
        scratch.install_cli();
        sibling(&scratch, "#!/bin/sh\nexit 37\n");
        let parent = scratch.0.join("data/remuda");
        std::fs::create_dir_all(&parent).unwrap();
        std::os::unix::fs::symlink(parent.join("missing"), parent.join("mods")).unwrap();
        let output = scratch.command().arg("gui").output().unwrap();
        assert!(!output.status.success());
        assert_ne!(output.status.code(), Some(37));
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("cannot determine whether a mod owns gui"));
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn gui_reports_unsupported_without_starting_a_daemon() {
    let scratch = Scratch::new("unsupported");
    scratch.install_cli();
    let output = scratch.command().arg("gui").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("macOS only"));
    scratch.assert_untouched();
}
