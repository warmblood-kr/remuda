use remuda_native::fs_lock::{self, Outcome};
use std::{io, process::Command};

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

fn run() -> io::Result<()> {
    let runtime = lock_directory()?;
    std::fs::create_dir_all(&runtime)?;
    let _lock = match fs_lock::acquire(
        &runtime.join("remuda-tray.lock"),
        &fs_lock::info_line("tray"),
    )? {
        Outcome::Acquired(file) => file,
        Outcome::Held(_) => return Ok(()),
    };
    #[cfg(target_os = "macos")]
    return macos::run();
    #[cfg(windows)]
    return windows::run();
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
    let result = std::env::current_exe().and_then(|gui| Command::new(gui).args(args).spawn());
    match result {
        Ok(mut child) => {
            // Reap independently; the open window can outlive the tray.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => eprintln!("remuda tray: cannot open window: {error}"),
    }
}
