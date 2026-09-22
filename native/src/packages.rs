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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Manifest {
    pub name: &'static str,
    pub version: &'static str,
    pub source: &'static str,
    pub status: &'static str,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "butler",
        source: include_str!("../../packages/butler/init.lua"),
        subcommand: Some("butler"),
    },
    Builtin {
        name: "butler/telemetry",
        source: include_str!("../../packages/butler/telemetry.lua"),
        subcommand: None,
    },
    Builtin {
        name: "butler/mail",
        source: include_str!("../../packages/butler/mail.lua"),
        subcommand: None,
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

pub fn manifests() -> impl Iterator<Item = Manifest> {
    BUILTINS.iter().map(|package| Manifest {
        name: package.name,
        version: crate::dist::BUILD_VERSION,
        source: "embedded",
        status: "installed",
    })
}

#[cfg(test)]
mod tests {
    use super::manifests;

    #[test]
    fn every_builtin_has_manifest_metadata() {
        let entries: Vec<_> = manifests().collect();
        assert!(!entries.is_empty());
        assert!(entries.iter().all(|entry| !entry.name.is_empty()
            && !entry.version.is_empty()
            && entry.source == "embedded"
            && entry.status == "installed"));
    }
}
