//! Local cluster membership discovery for the MCP `nodes` tool.
//!
//! Reads the local identity and membership registry only. It never opens a
//! socket, starts a daemon, or mutates membership, aliases or endpoints.

use super::control;
use super::identity;
use super::registry;
use super::storage;
use super::{AuthorizedNode, NodeIdentity, NodeState};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

/// At most this many node lines are emitted.
const MAX_LINES: usize = 256;
/// At most this many bytes are emitted, including the truncation marker.
const MAX_BYTES: usize = 32 * 1024;
/// Present only when at least one registry row was omitted.
pub const TRUNCATION_MARKER: &str = "... [nodes truncated]";

/// A typed discovery failure. The text is the MCP tool error, verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodesError {
    NotInitialized,
    IdentityUnreadable,
    RegistryUnreadable,
    RegistryTooLarge,
}

impl NodesError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInitialized => "cluster is not initialized",
            Self::IdentityUnreadable => "cluster identity is unreadable",
            Self::RegistryUnreadable => "cluster registry is unreadable",
            Self::RegistryTooLarge => "cluster registry is too large",
        }
    }
}

impl fmt::Display for NodesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for NodesError {}

/// One sanitized membership row, ready to render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRow {
    pub id: String,
    pub fingerprint: String,
    pub state: String,
    pub alias: String,
    pub is_self: bool,
    pub endpoint_present: bool,
}

/// Load local membership and render the bounded, tab-separated view.
pub fn list() -> Result<String, NodesError> {
    let rows = load_rows()?;
    Ok(render_rows(&rows))
}

fn load_rows() -> Result<Vec<NodeRow>, NodesError> {
    let dir = cluster_dir()?;
    let _guard = storage::StateLock::acquire(&dir).map_err(|_| NodesError::RegistryUnreadable)?;
    let identity = identity::load_identity_at(&dir).map_err(identity_error)?;
    let registry = registry::load_registry_for_discovery_at(&dir).map_err(registry_error)?;
    let aliases =
        control::node_aliases_with(&registry).map_err(|_| NodesError::RegistryUnreadable)?;
    Ok(build_rows(&identity, &registry.authorized_nodes, &aliases))
}

fn cluster_dir() -> Result<PathBuf, NodesError> {
    let dir = storage::cluster_state_dir()
        .map_err(|_| NodesError::NotInitialized)?
        .join("cluster");
    match fs::symlink_metadata(&dir) {
        Ok(_) => storage::verify_directory(&dir).map_err(|_| NodesError::RegistryUnreadable)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(NodesError::NotInitialized)
        }
        Err(_) => return Err(NodesError::RegistryUnreadable),
    }
    Ok(dir)
}

fn identity_error(error: io::Error) -> NodesError {
    if error.kind() == io::ErrorKind::NotFound {
        NodesError::NotInitialized
    } else {
        NodesError::IdentityUnreadable
    }
}

fn registry_error(error: io::Error) -> NodesError {
    if registry::is_registry_too_large(&error) {
        NodesError::RegistryTooLarge
    } else {
        NodesError::RegistryUnreadable
    }
}

fn build_rows(
    identity: &NodeIdentity,
    entries: &[AuthorizedNode],
    aliases: &BTreeMap<String, String>,
) -> Vec<NodeRow> {
    let mut rows: Vec<NodeRow> = entries
        .iter()
        .map(|entry| row_for(entry, &identity.node_fp, aliases))
        .collect();
    rows.sort_by(order);
    rows
}

fn row_for(
    entry: &AuthorizedNode,
    self_fingerprint: &str,
    aliases: &BTreeMap<String, String>,
) -> NodeRow {
    NodeRow {
        id: super::node_label(&entry.node_fp),
        fingerprint: entry.node_fp.clone(),
        state: state_text(entry.state).to_string(),
        alias: alias_for(&entry.node_fp, aliases),
        is_self: entry.node_fp == self_fingerprint,
        endpoint_present: endpoint_present(entry),
    }
}

fn order(left: &NodeRow, right: &NodeRow) -> std::cmp::Ordering {
    left.fingerprint
        .cmp(&right.fingerprint)
        .then_with(|| left.id.cmp(&right.id))
        .then_with(|| left.alias.cmp(&right.alias))
        .then_with(|| left.state.cmp(&right.state))
        .then_with(|| left.endpoint_present.cmp(&right.endpoint_present))
}

fn state_text(state: NodeState) -> &'static str {
    match state {
        NodeState::Admitted => "admitted",
        NodeState::Revoked => "revoked",
    }
}

fn alias_for(fingerprint: &str, aliases: &BTreeMap<String, String>) -> String {
    aliases
        .get(fingerprint)
        .filter(|alias| control::valid_alias(alias))
        .cloned()
        .unwrap_or_default()
}

fn endpoint_present(entry: &AuthorizedNode) -> bool {
    entry
        .endpoint
        .as_deref()
        .is_some_and(|endpoint| !endpoint.is_empty())
}

fn render_row(row: &NodeRow) -> String {
    format!(
        "id={}\tfingerprint={}\tstate={}\talias={}\tself={}\tendpoint={}",
        row.id,
        row.fingerprint,
        row.state,
        row.alias,
        row.is_self,
        if row.endpoint_present {
            "present"
        } else {
            "missing"
        }
    )
}

fn render_rows(rows: &[NodeRow]) -> String {
    let mut out = String::new();
    let mut included = 0usize;
    for row in rows.iter().take(MAX_LINES) {
        let line = render_row(row);
        let separator = usize::from(!out.is_empty());
        let marker = if included + 1 < rows.len() {
            separator + TRUNCATION_MARKER.len()
        } else {
            0
        };
        if out.len() + separator + line.len() + marker > MAX_BYTES {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
        included += 1;
    }
    if included < rows.len() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(TRUNCATION_MARKER);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn entry(
        fingerprint: &str,
        state: NodeState,
        alias: Option<&str>,
        endpoint: Option<&str>,
    ) -> AuthorizedNode {
        let mut optional_fields = BTreeMap::new();
        if let Some(alias) = alias {
            optional_fields.insert(
                "alias".to_string(),
                serde_json::json!({"at": 1, "by": "SHA256:writer", "name": alias}),
            );
        }
        AuthorizedNode {
            node_fp: fingerprint.to_string(),
            static_pubkey: String::new(),
            format_major: 1,
            format_minor: 0,
            endpoint: endpoint.map(str::to_string),
            delivered_by: None,
            state,
            version: 1,
            by: "SHA256:writer".to_string(),
            optional_fields,
        }
    }

    fn identity(fingerprint: &str) -> NodeIdentity {
        NodeIdentity {
            node_name: "self".to_string(),
            node_fp: fingerprint.to_string(),
            static_pubkey: Vec::new(),
        }
    }

    #[test]
    fn rows_are_ordered_by_fingerprint_then_id() {
        let me = identity("SHA256:b");
        let entries = vec![
            entry("SHA256:c", NodeState::Admitted, None, None),
            entry("SHA256:a", NodeState::Admitted, None, None),
            entry("SHA256:b", NodeState::Revoked, None, None),
        ];
        let rows = build_rows(&me, &entries, &BTreeMap::new());
        assert_eq!(
            rows.iter()
                .map(|row| row.fingerprint.as_str())
                .collect::<Vec<_>>(),
            vec!["SHA256:a", "SHA256:b", "SHA256:c"]
        );
        assert!(rows[1].is_self);
        assert!(!rows[0].is_self);
    }

    #[test]
    fn state_and_endpoint_are_rendered_from_presence() {
        let me = identity("SHA256:a");
        let entries = vec![
            entry(
                "SHA256:a",
                NodeState::Admitted,
                None,
                Some("192.0.2.1:7441"),
            ),
            entry("SHA256:b", NodeState::Revoked, None, Some("")),
        ];
        let rows = build_rows(&me, &entries, &BTreeMap::new());
        assert_eq!(rows[0].state, "admitted");
        assert!(rows[0].endpoint_present);
        assert_eq!(rows[1].state, "revoked");
        assert!(!rows[1].endpoint_present);
    }

    #[test]
    fn invalid_aliases_render_empty_but_valid_ones_render() {
        let aliases = BTreeMap::from([
            ("SHA256:a".to_string(), "studio".to_string()),
            ("SHA256:b".to_string(), "bad\talias".to_string()),
            ("SHA256:c".to_string(), "bad/alias".to_string()),
        ]);
        assert_eq!(alias_for("SHA256:a", &aliases), "studio");
        assert_eq!(alias_for("SHA256:b", &aliases), "");
        assert_eq!(alias_for("SHA256:c", &aliases), "");
        assert_eq!(alias_for("SHA256:missing", &aliases), "");
    }

    #[test]
    fn render_row_has_six_tab_separated_fields() {
        let row = NodeRow {
            id: "node-a".to_string(),
            fingerprint: "SHA256:a".to_string(),
            state: "admitted".to_string(),
            alias: String::new(),
            is_self: true,
            endpoint_present: false,
        };
        let line = render_row(&row);
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[0], "id=node-a");
        assert_eq!(fields[4], "self=true");
        assert_eq!(fields[5], "endpoint=missing");
    }

    #[test]
    fn empty_rows_render_empty_text() {
        assert_eq!(render_rows(&[]), "");
    }

    #[test]
    fn line_bound_keeps_exactly_256_rows_and_appends_marker() {
        let rows: Vec<NodeRow> = (0..300)
            .map(|index| NodeRow {
                id: format!("node-{index:03}"),
                fingerprint: format!("SHA256:{index:04}"),
                state: "admitted".to_string(),
                alias: String::new(),
                is_self: false,
                endpoint_present: false,
            })
            .collect();
        let body = render_rows(&rows);
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 257);
        assert_eq!(lines.last().copied(), Some(TRUNCATION_MARKER));
    }

    #[test]
    fn byte_bound_keeps_whole_lines_and_appends_marker() {
        let rows: Vec<NodeRow> = (0..256)
            .map(|index| NodeRow {
                id: format!("node-{index:03}"),
                fingerprint: format!("SHA256:{index:04}"),
                state: "admitted".to_string(),
                alias: "a".repeat(64),
                is_self: false,
                endpoint_present: false,
            })
            .collect();
        let body = render_rows(&rows);
        assert!(body.len() <= MAX_BYTES);
        assert!(body.ends_with(TRUNCATION_MARKER));
        for line in body.lines() {
            if line == TRUNCATION_MARKER {
                continue;
            }
            assert_eq!(line.split('\t').count(), 6);
        }
    }

    #[test]
    fn errors_have_the_contract_text() {
        assert_eq!(
            NodesError::NotInitialized.as_str(),
            "cluster is not initialized"
        );
        assert_eq!(
            NodesError::IdentityUnreadable.as_str(),
            "cluster identity is unreadable"
        );
        assert_eq!(
            NodesError::RegistryUnreadable.as_str(),
            "cluster registry is unreadable"
        );
        assert_eq!(
            NodesError::RegistryTooLarge.as_str(),
            "cluster registry is too large"
        );
    }
}
