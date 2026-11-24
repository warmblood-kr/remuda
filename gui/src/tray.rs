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
    let runtime = remuda_native::daemon::runtime_dir();
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
