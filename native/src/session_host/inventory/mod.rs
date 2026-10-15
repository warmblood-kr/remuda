//! Inactive bounded inventory/request transaction domain. No daemon adoption.
//! Persisted publications are not an atomic physical-process snapshot. Unknown
//! stores/hosts and unresolved acts never become an authoritative empty fleet.
#[cfg(any(unix, windows))]
mod directory;
#[cfg(any(unix, windows))]
pub(super) mod disk;
#[cfg(all(test, any(unix, windows)))]
mod disk_tests;
pub(super) mod model;
#[cfg(test)]
mod model_tests;
