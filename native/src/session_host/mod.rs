//! Experimental standalone session hosts for issue #713; opt-in daemon use.
//! A host owns one complete Session, PTY/parser and admitted logical acts.
//! The default spawn path, existing sessions and production rollout are separate.
#![allow(dead_code)]
mod api;
#[cfg(unix)]
mod birth;
mod channel;
#[cfg(unix)]
mod child;
mod control;
mod coordinator;
#[cfg(unix)]
mod entry;
#[cfg(unix)]
mod gate;
#[cfg(unix)]
pub(crate) mod hosted;
mod human;
mod inventory;
#[cfg(unix)]
mod latest;
mod ledger;
#[cfg(unix)]
mod owned;
#[cfg(unix)]
mod reaper;
#[cfg(unix)]
mod recovery;
#[cfg(unix)]
mod remote;
mod result;
#[cfg(unix)]
mod server;
mod strings;
mod subscription;
mod terminal;
#[cfg(unix)]
mod unix;
mod view_budget;
mod view_frames;
mod viewport;
#[cfg(windows)]
#[doc(hidden)]
pub mod windows_process;
mod wire;

/// Experimental standalone entry; never used by daemon launch/adoption.
#[doc(hidden)]
pub fn executable(operation: &str, path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if operation == "run" {
            entry::run(path)
        } else if operation == "run-reserved" {
            birth::run(path)
        } else {
            entry::command(path, operation)
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (operation, path);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "experimental standalone host backend is not implemented for this platform",
        ))
    }
}
