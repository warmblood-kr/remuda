use remuda_native::fs_lock::{self, Outcome};
use std::{
    io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[cfg(target_os = "macos")]
mod macos;
mod picker;
#[cfg(windows)]
mod windows;

pub fn pick_browser() {
    picker::open();
}

pub fn main() -> i32 {
    match run() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("remuda tray: {error}");
            1
        }
    }
}

#[derive(Clone, Copy, Default)]
enum Action {
    #[default]
    Quit,
    Upgrade,
}

fn run() -> io::Result<()> {
    let runtime = lock_directory()?;
    std::fs::create_dir_all(&runtime)?;
    let lock = match fs_lock::acquire(
        &runtime.join("remuda-tray.lock"),
        &fs_lock::info_line("tray"),
    )? {
        Outcome::Acquired(file) => file,
        Outcome::Held(_) => return Ok(()),
    };
    #[cfg(target_os = "macos")]
    let action = macos::run()?;
    #[cfg(windows)]
    let action = windows::run()?;
    if let Action::Upgrade = action {
        let cli = installed_cli();
        let result = cli
            .as_deref()
            .map_err(ToString::to_string)
            .and_then(upgrade);
        // A replacement tray acquires the same lock.
        drop(lock);
        let restart = cli
            .as_deref()
            .map_err(ToString::to_string)
            .and_then(restart_tray);
        if restart.is_err() {
            // The previous GUI slot remains usable after an incomplete install.
            launch(&["--tray"]);
        }
        match (result, restart, cli) {
            (Ok(()), Ok(()), Ok(cli)) => launch_process(cli_command(&cli, &["gui"])),
            (Err(error), Err(restart), _) if error != restart => {
                report_upgrade_error(&format!("{error}\n\n{restart}"));
            }
            (Err(error), _, _) | (_, Err(error), _) => report_upgrade_error(&error),
            _ => {}
        }
    }
    Ok(())
}

fn installed_cli() -> io::Result<PathBuf> {
    std::env::var_os(remuda_native::dist::TRAY_CLI_ENV)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::other("Start the installed tray with remuda tray, then retry."))
}

fn cli_command(cli: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(cli);
    command.args(args).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn upgrade(cli: &Path) -> Result<(), String> {
    let install_dir = cli
        .parent()
        .ok_or("Cannot locate the installation directory.")?;
    let output = cli_command(cli, &["upgrade"])
        .env("REMUDA_INSTALL_DIR", install_dir)
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let error = String::from_utf8_lossy(&output.stderr);
    let summary = error
        .lines()
        .skip(error.lines().count().saturating_sub(8))
        .collect::<Vec<_>>()
        .join("\n");
    if summary.is_empty() {
        Err(format!(
            "The upgrade command exited with {}.",
            output.status
        ))
    } else {
        Err(summary)
    }
}

fn restart_tray(cli: &Path) -> Result<(), String> {
    let status = cli_command(cli, &["tray"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("Cannot restart the tray: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Cannot restart the tray ({status})."))
    }
}

fn about() {
    use remuda_core::protocol::{Request, Response};
    use remuda_native::{client, daemon, dist};

    let cli = installed_cli().ok().and_then(|path| {
        let output = cli_command(&path, &["--version"]).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let line = String::from_utf8(output.stdout).ok()?;
        line.lines()
            .next()?
            .strip_prefix("remuda ")
            .map(str::to_owned)
    });
    let daemon = match client::request_with_timeout(
        &daemon::socket_path("default"),
        &Request::Version,
        std::time::Duration::from_secs(1),
    ) {
        Ok(Response::Value(build)) => build,
        Err(error) if error.kind() == io::ErrorKind::NotFound => "Not running".into(),
        _ => "Unavailable".into(),
    };
    let mut details = format!(
        "Running tray (GUI): {}\nInstalled CLI: {}\nRunning daemon (default): {}\nUpdate channel: {}",
        dist::BUILD_VERSION,
        cli.as_deref().unwrap_or("Unavailable"),
        daemon,
        dist::channel()
    );
    if cli
        .as_deref()
        .is_some_and(|build| build != dist::BUILD_VERSION)
    {
        details.push_str("\n\nThe tray and CLI builds differ.");
    }
    show_dialog("About Remuda", &details, false);
}

fn report_upgrade_error(message: &str) {
    show_dialog("Remuda upgrade needs attention", message, true);
}

fn show_dialog(title: &str, message: &str, warning: bool) {
    #[cfg(target_os = "macos")]
    macos::show_dialog(title, message, warning);
    #[cfg(windows)]
    windows::show_dialog(title, message, warning);
}

#[cfg(target_os = "macos")]
fn lock_directory() -> io::Result<std::path::PathBuf> {
    Ok(remuda_native::daemon::runtime_dir())
}

#[cfg(windows)]
fn lock_directory() -> io::Result<std::path::PathBuf> {
    use std::path::PathBuf;
    // The default daemon runtime is a logical named-pipe namespace on Windows.
    let absolute = |key| {
        std::env::var_os(key)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    absolute("XDG_DATA_HOME")
        .or_else(|| absolute("LOCALAPPDATA"))
        .or_else(|| absolute("USERPROFILE").map(|profile| profile.join("AppData/Local")))
        .map(|base| base.join("remuda/tray"))
        .ok_or_else(|| io::Error::other("cannot find the Windows user data directory"))
}

fn open_remuda() {
    launch(&[]);
}

fn open_browser() {
    launch(&["--browser-picker"]);
}

fn launch(args: &[&str]) {
    match std::env::current_exe() {
        Ok(gui) => {
            let mut command = Command::new(gui);
            command.args(args);
            launch_process(command);
        }
        Err(error) => eprintln!("remuda tray: cannot locate GUI: {error}"),
    }
}

fn launch_process(mut command: Command) {
    match command.spawn() {
        Ok(mut child) => {
            // Reap independently; the open window can outlive the tray.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => eprintln!("remuda tray: cannot open window: {error}"),
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{restart_tray, upgrade};
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn upgrade_targets_the_tray_installation_and_restarts_via_the_cli() {
        let directory = tempfile::tempdir().unwrap();
        let cli = directory.path().join("remuda");
        fs::write(
            &cli,
            r#"#!/bin/sh
set -eu
cd "$(dirname "$0")"
case "$1" in
    upgrade)
        test "$REMUDA_INSTALL_DIR" = "$(pwd)"
        touch upgraded
        ;;
    tray)
        test -f upgraded
        touch restarted
        ;;
    *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        upgrade(&cli).unwrap();
        restart_tray(&cli).unwrap();
        assert!(directory.path().join("restarted").is_file());

        fs::write(
            &cli,
            "#!/bin/sh\nprintf 'installer failed\\n' >&2\nexit 1\n",
        )
        .unwrap();
        assert_eq!(upgrade(&cli).unwrap_err(), "installer failed");
    }
}
