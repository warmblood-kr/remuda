//! User-facing node addressing: fingerprint, deterministic node id, or alias.
//!
//! These resolvers are for *incoming user text* only. Identity, routing and
//! authentication stay fingerprint-based: a resolved entry still has to pass
//! [`super::resolve_target`] and its pinned-key check.

use std::collections::BTreeMap;
use std::io;

use super::{node_label, AuthorizedNode, ClusterSnapshot};

/// Resolve user text by exact fingerprint, unique node id, then unique alias.
/// A tier with several matches fails closed; lower tiers are not consulted.
/// Revoked entries resolve here; transport policy still rejects them.
pub fn resolve_user_target<'a>(
    entries: &'a [AuthorizedNode],
    aliases: &BTreeMap<String, String>,
    target: &str,
) -> io::Result<&'a AuthorizedNode> {
    if let Some(entry) = entries.iter().find(|entry| entry.node_fp == target) {
        return Ok(entry);
    }
    if let Some(entry) = unique_by(entries, target, "node label", |entry| {
        node_label(&entry.node_fp) == target
    })? {
        return Ok(entry);
    }
    if let Some(entry) = unique_by(entries, target, "node alias", |entry| {
        aliases
            .get(&entry.node_fp)
            .is_some_and(|alias| alias == target && super::control::valid_alias(alias))
    })? {
        return Ok(entry);
    }
    Err(unknown_target(target))
}

impl ClusterSnapshot {
    /// Resolve a user target against this snapshot's membership and aliases.
    pub fn resolve_user_target(&self, target: &str) -> io::Result<&AuthorizedNode> {
        resolve_user_target(&self.registry.authorized_nodes, &self.aliases, target)
    }
}

fn unique_by<'a>(
    entries: &'a [AuthorizedNode],
    target: &str,
    kind: &str,
    matches: impl Fn(&AuthorizedNode) -> bool,
) -> io::Result<Option<&'a AuthorizedNode>> {
    let mut found = entries.iter().filter(|entry| matches(entry));
    let Some(entry) = found.next() else {
        return Ok(None);
    };
    if found.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} {target} is ambiguous; use the fingerprint"),
        ));
    }
    Ok(Some(entry))
}

fn unknown_target(target: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("no cluster node matches {target}"),
    )
}

/// A canonical remote target: an exact fingerprint plus the untouched session
/// suffix. `peer` is false for this node (its fingerprint is still carried so
/// the UI never has to look at a mutable alias).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTargetAddress {
    pub node_fp: String,
    pub peer: bool,
    pub session: Option<String>,
}

impl RemoteTargetAddress {
    /// `NODE` or `NODE/SESSION` with the node already canonical.
    pub fn text(&self) -> String {
        match &self.session {
            Some(session) => format!("{}/{session}", self.node_fp),
            None => self.node_fp.clone(),
        }
    }
}

/// Canonicalize `NODE[/SESSION]` once, before any UI or peer choice.
/// Known fingerprints may contain `/`; the hostname is local-only after them.
/// ID and alias tiers follow, with session suffix bytes preserved.
pub fn canonicalize_remote_target(
    target: &str,
    snapshot: &ClusterSnapshot,
    local_hostname: &str,
) -> io::Result<RemoteTargetAddress> {
    if let Some((entry, suffix)) =
        longest_fingerprint_prefix(&snapshot.registry.authorized_nodes, target)
    {
        return Ok(RemoteTargetAddress {
            node_fp: entry.node_fp.clone(),
            peer: entry.node_fp != snapshot.identity.node_fp,
            session: session_suffix(suffix),
        });
    }
    if let Some(fingerprint) = unknown_fingerprint_with_session(target) {
        return Err(unknown_target(fingerprint));
    }

    let (node, session) = match target.split_once('/') {
        Some((node, session)) => (node, Some(session.to_owned())),
        None => (target, None),
    };
    if node.is_empty() {
        return Err(unknown_target(node));
    }
    if node == local_hostname {
        return Ok(RemoteTargetAddress {
            node_fp: snapshot.identity.node_fp.clone(),
            peer: false,
            session,
        });
    }
    let entry = resolve_user_target(&snapshot.registry.authorized_nodes, &snapshot.aliases, node)?;
    Ok(RemoteTargetAddress {
        node_fp: entry.node_fp.clone(),
        peer: entry.node_fp != snapshot.identity.node_fp,
        session,
    })
}

/// Find the longest known fingerprint that is `target` or a `/`-prefix of it.
fn longest_fingerprint_prefix<'a, 't>(
    entries: &'a [AuthorizedNode],
    target: &'t str,
) -> Option<(&'a AuthorizedNode, &'t str)> {
    entries
        .iter()
        .filter_map(|entry| {
            let suffix = target.strip_prefix(entry.node_fp.as_str())?;
            (suffix.is_empty() || suffix.starts_with('/')).then_some((entry, suffix))
        })
        .max_by_key(|(entry, _)| entry.node_fp.len())
}

fn session_suffix(suffix: &str) -> Option<String> {
    suffix.strip_prefix('/').map(str::to_owned)
}

/// A slash-bearing canonical fingerprint, alone or before `/`, is a node miss
/// rather than an alias prefix. A fingerprint-looking alias stays a valid alias.
fn unknown_fingerprint_with_session(target: &str) -> Option<&str> {
    const LEN: usize = 50;
    let bytes = target.as_bytes();
    let candidate = std::str::from_utf8(bytes.get(..LEN)?).ok()?;
    let continues = bytes.len() == LEN || bytes.get(LEN) == Some(&b'/');
    (continues && candidate.contains('/') && is_canonical_fingerprint(candidate))
        .then_some(candidate)
}

fn is_canonical_fingerprint(value: &str) -> bool {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const LAST: &[u8] = b"AEIMQUYcgkosw048";
    let Some(encoded) = value.strip_prefix("SHA256:") else {
        return false;
    };
    encoded.len() == 43
        && encoded.bytes().all(|byte| TABLE.contains(&byte))
        && encoded
            .as_bytes()
            .last()
            .is_some_and(|byte| LAST.contains(byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{NodeIdentity, NodeState, Registry};
    use std::collections::BTreeMap;

    fn entry(fingerprint: &str, state: NodeState) -> AuthorizedNode {
        AuthorizedNode {
            node_fp: fingerprint.into(),
            static_pubkey: String::new(),
            delivered_by: None,
            format_major: 1,
            format_minor: 0,
            optional_fields: BTreeMap::new(),
            endpoint: None,
            state,
            version: 1,
            by: String::new(),
        }
    }

    fn snapshot(fingerprints: &[&str], aliases: &[(&str, &str)], local: &str) -> ClusterSnapshot {
        ClusterSnapshot {
            identity: NodeIdentity {
                node_name: node_label(local),
                node_fp: local.into(),
                static_pubkey: Vec::new(),
            },
            registry: Registry {
                authorized_nodes: fingerprints
                    .iter()
                    .map(|fingerprint| entry(fingerprint, NodeState::Admitted))
                    .collect(),
            },
            aliases: aliases
                .iter()
                .map(|(fingerprint, alias)| ((*fingerprint).into(), (*alias).into()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn fingerprint_wins_over_an_identical_alias() {
        let snapshot = snapshot(
            &["SHA256:aaaa", "SHA256:bbbb"],
            &[("SHA256:bbbb", "SHA256:aaaa")],
            "SHA256:aaaa",
        );
        let entry = snapshot.resolve_user_target("SHA256:aaaa").unwrap();
        assert_eq!(entry.node_fp, "SHA256:aaaa");
    }

    #[test]
    fn node_id_wins_over_an_identical_alias() {
        let alias = node_label("SHA256:bbbb");
        let snapshot = snapshot(
            &["SHA256:aaaa", "SHA256:bbbb"],
            &[("SHA256:aaaa", &alias)],
            "SHA256:aaaa",
        );
        let entry = snapshot.resolve_user_target(&alias).unwrap();
        assert_eq!(entry.node_fp, "SHA256:bbbb");
    }

    #[test]
    fn alias_resolves_when_no_higher_tier_matches() {
        let snapshot = snapshot(
            &["SHA256:aaaa"],
            &[("SHA256:aaaa", "studio")],
            "SHA256:aaaa",
        );
        let entry = snapshot.resolve_user_target("studio").unwrap();
        assert_eq!(entry.node_fp, "SHA256:aaaa");
    }

    #[test]
    fn duplicate_ids_fail_even_when_an_alias_could_choose_one() {
        let id = node_label("SHA256:abcdefgh1");
        assert_eq!(id, node_label("SHA256:abcdefgh2"));
        let snapshot = snapshot(
            &["SHA256:abcdefgh1", "SHA256:abcdefgh2"],
            &[("SHA256:aaaa", "studio")],
            "SHA256:aaaa",
        );
        let error = snapshot.resolve_user_target(&id).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("use the fingerprint"));
    }

    #[test]
    fn duplicate_aliases_are_ambiguous() {
        let snapshot = snapshot(
            &["SHA256:aaaa", "SHA256:bbbb"],
            &[("SHA256:aaaa", "studio"), ("SHA256:bbbb", "studio")],
            "SHA256:aaaa",
        );
        let error = snapshot.resolve_user_target("studio").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn admitted_and_revoked_aliases_stay_ambiguous() {
        let snapshot = ClusterSnapshot {
            identity: NodeIdentity {
                node_name: node_label("SHA256:aaaa"),
                node_fp: "SHA256:aaaa".into(),
                static_pubkey: Vec::new(),
            },
            registry: Registry {
                authorized_nodes: vec![
                    entry("SHA256:aaaa", NodeState::Admitted),
                    entry("SHA256:bbbb", NodeState::Revoked),
                ],
            },
            aliases: BTreeMap::from([
                ("SHA256:aaaa".into(), "studio".into()),
                ("SHA256:bbbb".into(), "studio".into()),
            ]),
        };
        let error = snapshot.resolve_user_target("studio").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn revoked_entries_still_resolve() {
        let mut snapshot = snapshot(
            &["SHA256:aaaa"],
            &[("SHA256:aaaa", "studio")],
            "SHA256:bbbb",
        );
        snapshot.registry.authorized_nodes[0].state = NodeState::Revoked;
        let entry = snapshot.resolve_user_target("studio").unwrap();
        assert_eq!(entry.state, NodeState::Revoked);
    }

    #[test]
    fn unknown_target_is_not_found() {
        let snapshot = snapshot(&["SHA256:aaaa"], &[], "SHA256:aaaa");
        let error = snapshot.resolve_user_target("studio").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn remote_fingerprint_with_slash_keeps_the_session_suffix() {
        let snapshot = snapshot(&["SHA256:ab/cd", "SHA256:other"], &[], "SHA256:other");
        let target =
            canonicalize_remote_target("SHA256:ab/cd/build/task", &snapshot, "studio").unwrap();
        assert_eq!(
            target,
            RemoteTargetAddress {
                node_fp: "SHA256:ab/cd".into(),
                peer: true,
                session: Some("build/task".into()),
            }
        );
    }

    #[test]
    fn remote_alias_and_local_alias_canonicalize() {
        let snapshot = snapshot(
            &["SHA256:aaaa", "SHA256:bbbb"],
            &[("SHA256:aaaa", "mine"), ("SHA256:bbbb", "laptop")],
            "SHA256:aaaa",
        );
        assert_eq!(
            canonicalize_remote_target("laptop/build", &snapshot, "studio").unwrap(),
            RemoteTargetAddress {
                node_fp: "SHA256:bbbb".into(),
                peer: true,
                session: Some("build".into()),
            }
        );
        assert_eq!(
            canonicalize_remote_target("mine/build", &snapshot, "studio").unwrap(),
            RemoteTargetAddress {
                node_fp: "SHA256:aaaa".into(),
                peer: false,
                session: Some("build".into()),
            }
        );
    }

    #[test]
    fn local_hostname_is_accepted_before_ids_and_aliases() {
        let snapshot = snapshot(
            &["SHA256:aaaa", "SHA256:bbbb"],
            &[("SHA256:bbbb", "studio")],
            "SHA256:aaaa",
        );
        assert_eq!(
            canonicalize_remote_target("studio/dev", &snapshot, "studio").unwrap(),
            RemoteTargetAddress {
                node_fp: "SHA256:aaaa".into(),
                peer: false,
                session: Some("dev".into()),
            }
        );
    }

    #[test]
    fn whole_node_alias_and_trailing_session_are_preserved() {
        let snapshot = snapshot(
            &["SHA256:aaaa"],
            &[("SHA256:aaaa", "studio")],
            "SHA256:bbbb",
        );
        assert_eq!(
            canonicalize_remote_target("studio", &snapshot, "host").unwrap(),
            RemoteTargetAddress {
                node_fp: "SHA256:aaaa".into(),
                peer: true,
                session: None,
            }
        );
        assert_eq!(
            canonicalize_remote_target("studio/", &snapshot, "host").unwrap(),
            RemoteTargetAddress {
                node_fp: "SHA256:aaaa".into(),
                peer: true,
                session: Some(String::new()),
            }
        );
    }

    #[test]
    fn unknown_fingerprint_prefix_is_not_a_match() {
        let snapshot = snapshot(&["SHA256:ab/cd"], &[], "SHA256:other");
        let error =
            canonicalize_remote_target("SHA256:ab/other/dev", &snapshot, "host").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    fn full_fingerprint(with_slash: bool) -> String {
        let mut value = String::from("SHA256:");
        if with_slash {
            value.push('/');
        }
        value.push_str(&"A".repeat(if with_slash { 41 } else { 42 }));
        value.push('E');
        value
    }

    #[test]
    fn unknown_fingerprint_with_slash_does_not_fall_back_to_an_alias() {
        let fingerprint = full_fingerprint(true);
        let snapshot = snapshot(
            &["SHA256:other"],
            &[("SHA256:other", "SHA256:")],
            "SHA256:local",
        );
        let target = format!("{fingerprint}/session");
        let error = canonicalize_remote_target(&target, &snapshot, "host").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains(&fingerprint));
    }

    #[test]
    fn unknown_slash_fingerprint_alone_is_not_an_alias() {
        let fingerprint = full_fingerprint(true);
        let snapshot = snapshot(
            &["SHA256:other"],
            &[("SHA256:other", "SHA256:")],
            "SHA256:local",
        );
        let error = canonicalize_remote_target(&fingerprint, &snapshot, "host").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains(&fingerprint));
    }

    #[test]
    fn fingerprint_looking_alias_stays_valid_alone_and_with_session() {
        let fingerprint = full_fingerprint(false);
        let snapshot = snapshot(
            &["SHA256:other"],
            &[("SHA256:other", &fingerprint)],
            "SHA256:local",
        );
        let whole = canonicalize_remote_target(&fingerprint, &snapshot, "host").unwrap();
        assert_eq!(
            whole,
            RemoteTargetAddress {
                node_fp: "SHA256:other".into(),
                peer: true,
                session: None,
            }
        );
        let with_session =
            canonicalize_remote_target(&format!("{fingerprint}/build"), &snapshot, "host").unwrap();
        assert_eq!(
            with_session,
            RemoteTargetAddress {
                node_fp: "SHA256:other".into(),
                peer: true,
                session: Some("build".into()),
            }
        );
    }

    #[test]
    fn decorated_display_text_works_only_as_a_literal_alias() {
        let snapshot = snapshot(
            &["SHA256:other"],
            &[("SHA256:other", "macbook (node-abcd1234)")],
            "SHA256:local",
        );
        assert_eq!(
            canonicalize_remote_target("macbook (node-abcd1234)/build", &snapshot, "host").unwrap(),
            RemoteTargetAddress {
                node_fp: "SHA256:other".into(),
                peer: true,
                session: Some("build".into()),
            }
        );
    }

    #[test]
    fn remote_unknown_target_is_a_helpful_error() {
        let snapshot = snapshot(&["SHA256:aaaa"], &[], "SHA256:aaaa");
        let error = canonicalize_remote_target("missing/dev", &snapshot, "studio").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("missing"));
    }
}
