//! Built-in packages: Lua source embedded at compile time, keyed by name.
//!
//! The single source of truth for package resolution. Both the bin crate's
//! `remuda exec <name>` (over IPC) and the lib crate's `remuda.exec(name)`
//! Lua binding (in-process, script.rs) resolve through this one table —
//! there used to be two copies of this match, one per crate; this replaces
//! both.

pub struct Builtin {
    pub name: &'static str,
    pub source: &'static str,
    pub subcommand: Option<&'static str>,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "butler",
        source: include_str!("../../packages/butler/init.lua"),
        subcommand: Some("butler"),
    },
    Builtin {
        name: "butler/agents/claudecode",
        source: include_str!("../../packages/butler/agents/claudecode.lua"),
        subcommand: None,
    },
    Builtin {
        name: "butler/agents/codex",
        source: include_str!("../../packages/butler/agents/codex.lua"),
        subcommand: None,
    },
];

pub fn builtin(name: &str) -> Option<&'static str> {
    BUILTINS
        .iter()
        .find(|package| package.name == name)
        .map(|package| package.source)
}

pub fn subcommand(name: &str) -> Option<&'static Builtin> {
    BUILTINS
        .iter()
        .find(|package| package.subcommand == Some(name))
}
