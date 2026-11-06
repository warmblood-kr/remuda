//! Read-only cluster discovery for Lua.
//!
//! Only this node's membership snapshot is read. Nothing here contacts a peer,
//! exposes an endpoint or key, or returns a remote control response.

use crate::cluster::{self, ClusterSnapshot, NodeState};
use crate::text::strip_terminal_controls;
use mlua::{Lua, Table, Value};

/// One admitted node, as much as a script may see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRow {
    /// Deterministic user-facing node id, e.g. `node-abcd1234`.
    pub id: String,
    /// Exact authentication identity, for disambiguation only.
    pub fingerprint: String,
    /// Optional human alias. Display metadata, never an authentication fact.
    pub alias: Option<String>,
    /// Always `admitted`; revoked entries are not offered as targets.
    pub state: &'static str,
    /// Whether this row is the node running the script.
    pub self_node: bool,
}

/// Build rows from one snapshot. Aliases are local display metadata; a
/// duplicated alias is retained for both nodes so the caller can see why a
/// target is ambiguous, but neither row is preferred.
pub fn rows(snapshot: &ClusterSnapshot) -> Vec<NodeRow> {
    snapshot
        .entries()
        .iter()
        .filter(|entry| entry.state == NodeState::Admitted)
        .map(|entry| {
            let alias = snapshot
                .aliases
                .get(&entry.node_fp)
                .map(|alias| strip_terminal_controls(alias).into_owned())
                .filter(|alias| !alias.is_empty());
            NodeRow {
                id: cluster::node_label(&entry.node_fp),
                fingerprint: entry.node_fp.clone(),
                alias,
                state: "admitted",
                self_node: entry.node_fp == snapshot.identity.node_fp,
            }
        })
        .collect()
}

/// `remuda.cluster` namespace.
pub fn bindings(lua: &Lua) -> mlua::Result<Table> {
    let cluster_table = lua.create_table()?;
    cluster_table.set(
        "nodes",
        lua.create_function(|lua, ()| -> mlua::Result<(Value, Option<String>)> {
            let snapshot = match cluster::snapshot() {
                Ok(Some(snapshot)) => snapshot,
                Ok(None) => return Ok((Value::Nil, Some("no_cluster".to_owned()))),
                Err(_) => return Ok((Value::Nil, Some("cluster_state_unreadable".to_owned()))),
            };
            let list = lua.create_table()?;
            for row in rows(&snapshot) {
                let table = lua.create_table()?;
                table.set("id", row.id)?;
                table.set("fingerprint", row.fingerprint)?;
                if let Some(alias) = row.alias {
                    table.set("alias", alias)?;
                }
                table.set("state", row.state)?;
                table.set("self", row.self_node)?;
                list.set(list.len()? + 1, table)?;
            }
            Ok((Value::Table(list), None))
        })?,
    )?;
    Ok(cluster_table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::registry::{AuthorizedNode, Registry};
    use crate::cluster::NodeIdentity;
    use std::collections::BTreeMap;

    fn entry(fingerprint: &str, state: NodeState) -> AuthorizedNode {
        AuthorizedNode {
            node_fp: fingerprint.to_owned(),
            static_pubkey: String::new(),
            format_major: 1,
            format_minor: 0,
            endpoint: Some("192.0.2.1:7441".to_owned()),
            delivered_by: None,
            state,
            version: 1,
            by: String::new(),
            optional_fields: BTreeMap::new(),
        }
    }

    fn snapshot(entries: Vec<AuthorizedNode>, aliases: &[(&str, &str)]) -> ClusterSnapshot {
        ClusterSnapshot {
            identity: NodeIdentity {
                node_name: "self".to_owned(),
                node_fp: "SHA256:self".to_owned(),
                static_pubkey: vec![0; 32],
            },
            registry: Registry {
                authorized_nodes: entries,
            },
            aliases: aliases
                .iter()
                .map(|(fingerprint, alias)| ((*fingerprint).to_owned(), (*alias).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn admitted_self_and_peer_rows_carry_no_endpoint_or_key() {
        let snapshot = snapshot(
            vec![
                entry("SHA256:self", NodeState::Admitted),
                entry("SHA256:peer", NodeState::Admitted),
            ],
            &[("SHA256:peer", "studio")],
        );
        let rows = rows(&snapshot);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, cluster::node_label("SHA256:self"));
        assert!(rows[0].self_node);
        assert_eq!(rows[0].alias, None);
        assert_eq!(rows[1].alias.as_deref(), Some("studio"));
        assert_eq!(rows[1].state, "admitted");
        assert!(!rows[1].self_node);
    }

    #[test]
    fn revoked_entries_are_not_offered() {
        let snapshot = snapshot(
            vec![
                entry("SHA256:self", NodeState::Admitted),
                entry("SHA256:gone", NodeState::Revoked),
            ],
            &[],
        );
        let rows = rows(&snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].fingerprint, "SHA256:self");
    }

    #[test]
    fn duplicate_aliases_both_appear_so_the_caller_can_see_the_ambiguity() {
        let snapshot = snapshot(
            vec![
                entry("SHA256:a", NodeState::Admitted),
                entry("SHA256:b", NodeState::Admitted),
            ],
            &[("SHA256:a", "studio"), ("SHA256:b", "studio")],
        );
        let rows = rows(&snapshot);
        assert_eq!(
            rows.iter()
                .filter(|row| row.alias.as_deref() == Some("studio"))
                .count(),
            2
        );
    }

    #[test]
    fn control_characters_in_an_alias_never_reach_lua() {
        let snapshot = snapshot(
            vec![entry("SHA256:peer", NodeState::Admitted)],
            &[("SHA256:peer", "st\u{1b}]0;pwn\u{7}udio")],
        );
        let rows = rows(&snapshot);
        assert_eq!(rows[0].alias.as_deref(), Some("st]0;pwnudio"));
    }
}
