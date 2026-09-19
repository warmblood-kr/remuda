//! Built-in packages: Lua source embedded at compile time, keyed by name.
//!
//! The single source of truth for package resolution. Both the bin crate's
//! `remuda exec <name>` (over IPC) and the lib crate's `remuda.exec(name)`
//! Lua binding (in-process, script.rs) resolve through this one table —
//! there used to be two copies of this match, one per crate; this replaces
//! both.

/// A built-in package's entry source, embedded at compile time. No install
/// mechanism and no registry yet — a name either matches one of these arms
/// or it doesn't.
pub fn builtin(name: &str) -> Option<&'static str> {
    match name {
        "butler" => Some(include_str!("../../packages/butler/init.lua")),
        _ => None,
    }
}
