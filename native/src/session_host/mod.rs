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
mod host_build;
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

/// Set by the daemon on every host it starts, to the daemon's own build. A
/// host of another build exits with [`BUILD_MISMATCH`] before it reads its
/// bootstrap, so a daemon never adopts a host it did not ship with.
pub const EXPECTED_BUILD: &str = "REMUDA_SESSION_HOST_BUILD";
/// Exit status of a host refused by [`EXPECTED_BUILD`].
pub const BUILD_MISMATCH: i32 = 3;

/// The refusal for a host started by a daemon of another build, if any.
#[doc(hidden)]
pub fn other_build_refusal(expected: Option<&std::ffi::OsStr>) -> Option<String> {
    let expected = expected?;
    (expected != crate::dist::BUILD_VERSION).then(|| {
        format!(
            "this host is build {}, but the daemon expects {}",
            crate::dist::BUILD_VERSION,
            expected.to_string_lossy()
        )
    })
}

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
