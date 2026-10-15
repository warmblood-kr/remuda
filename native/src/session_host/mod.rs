//! Inactive foundations for issue #713's stable session host.
//!
//! These state machines do not launch hosts, authenticate connections, or
//! preserve sessions yet. Runtime integration must retain one host-owned
//! `core::Session`, including complete logical input acts and delayed writes.
#![allow(dead_code)]
mod api;
mod channel;
#[cfg(unix)]
mod child;
mod control;
mod coordinator;
#[cfg(unix)]
mod entry;
mod inventory;
mod ledger;
#[cfg(unix)]
mod owned;
#[cfg(unix)]
mod reaper;
mod result;
#[cfg(unix)]
mod server;
mod strings;
mod subscription;
mod terminal;
#[cfg(unix)]
mod unix;
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
