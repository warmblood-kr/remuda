//! Read-only cluster calls shared by MCP and future CLI entry points.
//!
//! This module deliberately exposes only the calls a local caller may make to a
//! peer: an authenticated List or a styled capture. It never evaluates Lua on a
//! peer, never opens a remote shell, and never treats a peer response as a
//! trusted instruction.

use crate::cluster::{self, NodeState};
use crate::net::cluster_client::{ClientError, ClusterClient};
use crate::text::{is_terminal_control, strip_terminal_controls};
use remuda_core::protocol::{Request, Response, StyledRun};
use std::collections::BTreeMap;
use std::net::SocketAddr;

/// Upper bound for one remote capture returned to an agent. The MCP layer and
/// the terminal UI have their own output bounds; this one keeps a single tool
/// result from carrying a multi-megabyte screen into a model context.
pub const MAX_CAPTURE_BYTES: usize = 256 * 1024;
/// The truncation marker is part of the bound, not an addition to it.
const TRUNCATION_MARKER: &str = "\n... [capture truncated]";

/// A resolved user target. The fingerprint is the authentication identity; the
/// address is a routing hint only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedCall {
    /// The target is this node; use the ordinary local daemon path.
    Local,
    /// An admitted peer with its registry-pinned key.
    Remote {
        fingerprint: String,
        address: SocketAddr,
        pinned_static_key: Vec<u8>,
    },
}

/// The result of a remote-aware capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Capture {
    /// The target names this node; the caller keeps its existing local path.
    Local,
    /// Plain text from the peer, already sanitized and bounded.
    Remote(String),
}

/// Resolve a user-supplied node target. Aliases are display metadata; the
/// returned fingerprint is what authenticates the peer.
pub fn resolve(target: &str) -> Result<ResolvedCall, String> {
    let snapshot = cluster::snapshot()
        .map_err(|_| "cluster state is unreadable".to_owned())?
        .ok_or_else(|| "no cluster is initialized; run `remuda cluster init`".to_owned())?;
    match resolve_name(
        &snapshot.registry.authorized_nodes,
        &snapshot.aliases,
        &snapshot.identity.node_fp,
        Some(&snapshot.identity.node_name),
        target,
    )? {
        NameResolution::Local => Ok(ResolvedCall::Local),
        NameResolution::Remote(fingerprint) => {
            let resolved = cluster::resolve_target(&fingerprint, None).map_err(|error| {
                sanitize(format!(
                    "cluster node {}: {error}",
                    cluster::node_label(&fingerprint)
                ))
            })?;
            Ok(ResolvedCall::Remote {
                fingerprint,
                address: resolved.address,
                pinned_static_key: resolved.pinned_static_key,
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NameResolution {
    Local,
    Remote(String),
}

/// Capture one session on a peer. A local target is reported as
/// [`Capture::Local`] so the existing local `Capture` request stays byte-for-byte
/// unchanged; this function never reads local PTY state itself.
pub fn capture(target: &str, session: &str) -> Result<Capture, String> {
    let ResolvedCall::Remote {
        fingerprint,
        address,
        pinned_static_key,
    } = resolve(target)?
    else {
        return Ok(Capture::Local);
    };
    capture_remote(
        &fingerprint,
        address,
        &pinned_static_key,
        session,
        |address, pinned, request| {
            let private_key =
                cluster::identity::load_static_private_key().map_err(|_| ClientError::Crypto)?;
            ClusterClient::system().request(address, pinned, &private_key, request)
        },
    )
}

/// The remote half with its transport injected, so the exact request and every
/// error path can be exercised without a live peer or a local daemon.
fn capture_remote(
    fingerprint: &str,
    address: SocketAddr,
    pinned_static_key: &[u8],
    session: &str,
    send: impl FnOnce(SocketAddr, &[u8], &Request) -> Result<Response, ClientError>,
) -> Result<Capture, String> {
    let response = send(
        address,
        pinned_static_key,
        &Request::CaptureStyled {
            name: session.to_owned(),
            scrollback: 0,
        },
    )
    .map_err(|error| remote_error(fingerprint, error))?;
    match response {
        Response::StyledScreen { rows, .. } => Ok(Capture::Remote(render_rows(&rows))),
        Response::Error(message) => Err(sanitize(format!(
            "capture on {}: {message}",
            cluster::node_label(fingerprint)
        ))),
        Response::RemoteControlDisabled => Err(format!(
            "capture on {}: remote control is disabled there",
            cluster::node_label(fingerprint)
        )),
        _ => Err(format!(
            "capture on {}: peer returned an unexpected response",
            cluster::node_label(fingerprint)
        )),
    }
}

fn remote_error(fingerprint: &str, error: ClientError) -> String {
    let code = match error {
        ClientError::Unreachable
        | ClientError::PeerNotListening
        | ClientError::PeerClosedConnection
        | ClientError::OsBlockedConnection
        | ClientError::Timeout => "unreachable",
        ClientError::Refused(_) => "refused",
        ClientError::Crypto
        | ClientError::BadResponse
        | ClientError::PeerTooNew
        | ClientError::RequestTooLarge => "protocol",
    };
    format!(
        "capture on {}: {code}: {error}",
        cluster::node_label(fingerprint)
    )
}

/// Flatten styled rows to the same plain-text shape a local capture returns.
/// Row separators are LF; every cell passes the single terminal-control
/// sanitizer. The result is bounded and marks its own truncation.
fn render_rows(rows: &[Vec<StyledRun>]) -> String {
    // Buffer at most the public bound, then replace its tail with the marker
    // only when more content exists. This keeps the contract that a screen at
    // or under 256 KiB is returned untouched while never expanding the frame.
    let limit = MAX_CAPTURE_BYTES;
    let mut text = String::with_capacity(limit);
    let mut truncated = false;
    'rows: for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            if text.len() == limit {
                truncated = true;
                break;
            }
            text.push('\n');
        }
        for run in row {
            for character in run.text.chars() {
                if is_terminal_control(character) {
                    continue;
                }
                if text.len() + character.len_utf8() > limit {
                    truncated = true;
                    break 'rows;
                }
                text.push(character);
            }
        }
    }
    if truncated {
        let keep = MAX_CAPTURE_BYTES.saturating_sub(TRUNCATION_MARKER.len());
        let mut cut = keep;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str(TRUNCATION_MARKER);
    }
    text
}

fn sanitize(message: String) -> String {
    strip_terminal_controls(&message).into_owned()
}

/// Resolve against an in-memory registry. Kept separate from the storage read
/// so alias and revocation rules can be tested without a real cluster.
fn resolve_name(
    entries: &[cluster::AuthorizedNode],
    aliases: &BTreeMap<String, String>,
    self_fingerprint: &str,
    self_name: Option<&str>,
    target: &str,
) -> Result<NameResolution, String> {
    let entry = match cluster::addressing::resolve_user_target(entries, aliases, target) {
        Ok(entry) => entry,
        // A hostname that is not an admitted node's fingerprint/label/alias is
        // still this node's local name.
        Err(error) if self_name == Some(target) && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(NameResolution::Local);
        }
        Err(error) => return Err(sanitize(format!("cluster: {error}"))),
    };
    if entry.state != NodeState::Admitted {
        return Err(format!(
            "cluster node {} is revoked",
            cluster::node_label(&entry.node_fp)
        ));
    }
    // The registry wins. A peer alias or label equal to this node's hostname
    // must not silently route the screen to the wrong node.
    if self_name == Some(target) && entry.node_fp != self_fingerprint {
        return Err(format!(
            "cluster: node {target} is ambiguous: it is both this node's hostname and another admitted node; use the fingerprint"
        ));
    }
    if entry.node_fp == self_fingerprint {
        return Ok(NameResolution::Local);
    }
    Ok(NameResolution::Remote(entry.node_fp.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::registry::AuthorizedNode;

    fn entry(fingerprint: &str, state: NodeState) -> AuthorizedNode {
        AuthorizedNode {
            node_fp: fingerprint.to_owned(),
            static_pubkey: String::new(),
            format_major: 1,
            format_minor: 0,
            endpoint: None,
            delivered_by: None,
            state,
            version: 1,
            by: String::new(),
            optional_fields: BTreeMap::new(),
        }
    }

    #[test]
    fn exact_local_fingerprint_is_local_and_never_remote() {
        let entries = [entry("SHA256:self", NodeState::Admitted)];
        assert_eq!(
            resolve_name(
                &entries,
                &BTreeMap::new(),
                "SHA256:self",
                None,
                "SHA256:self"
            ),
            Ok(NameResolution::Local)
        );
    }

    #[test]
    fn this_nodes_hostname_is_local_only_when_no_admitted_node_uses_it() {
        let entries = [entry("SHA256:self", NodeState::Admitted)];
        assert_eq!(
            resolve_name(
                &entries,
                &BTreeMap::new(),
                "SHA256:self",
                Some("studio-host"),
                "studio-host",
            ),
            Ok(NameResolution::Local)
        );

        let peer = [entry("SHA256:peer", NodeState::Admitted)];
        let aliases = BTreeMap::from([("SHA256:peer".to_owned(), "studio-host".to_owned())]);
        let error = resolve_name(
            &peer,
            &aliases,
            "SHA256:self",
            Some("studio-host"),
            "studio-host",
        )
        .unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
    }

    #[test]
    fn a_hostname_collision_does_not_hide_a_revoked_peer() {
        let entries = [entry("SHA256:old", NodeState::Revoked)];
        let aliases = BTreeMap::from([("SHA256:old".to_owned(), "studio-host".to_owned())]);
        let error = resolve_name(
            &entries,
            &aliases,
            "SHA256:self",
            Some("studio-host"),
            "studio-host",
        )
        .unwrap_err();
        assert!(error.contains("revoked"), "{error}");
    }

    #[test]
    fn unique_alias_resolves_to_its_fingerprint() {
        let entries = [entry("SHA256:peer", NodeState::Admitted)];
        let aliases = BTreeMap::from([("SHA256:peer".to_owned(), "studio".to_owned())]);
        let resolved = resolve_name(&entries, &aliases, "SHA256:self", None, "studio").unwrap();
        assert_eq!(resolved, NameResolution::Remote("SHA256:peer".to_owned()));
    }

    #[test]
    fn duplicate_alias_fails_closed() {
        let entries = [
            entry("SHA256:a", NodeState::Admitted),
            entry("SHA256:b", NodeState::Admitted),
        ];
        let aliases = BTreeMap::from([
            ("SHA256:a".to_owned(), "studio".to_owned()),
            ("SHA256:b".to_owned(), "studio".to_owned()),
        ]);
        let error = resolve_name(&entries, &aliases, "SHA256:self", None, "studio").unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
    }

    #[test]
    fn revoked_node_is_never_resolved() {
        let entries = [entry("SHA256:old", NodeState::Revoked)];
        let error = resolve_name(
            &entries,
            &BTreeMap::new(),
            "SHA256:self",
            None,
            "SHA256:old",
        )
        .unwrap_err();
        assert!(error.contains("revoked"), "{error}");
    }

    #[test]
    fn unknown_node_is_an_error_not_a_local_fallback() {
        let entries = [entry("SHA256:self", NodeState::Admitted)];
        let error =
            resolve_name(&entries, &BTreeMap::new(), "SHA256:self", None, "missing").unwrap_err();
        assert!(error.contains("no cluster node matches"), "{error}");
    }

    #[test]
    fn control_characters_in_a_user_target_are_stripped_from_the_error() {
        let entries = [entry("SHA256:self", NodeState::Admitted)];
        let error = resolve_name(
            &entries,
            &BTreeMap::new(),
            "SHA256:self",
            None,
            "bad\u{1b}]0;pwn\u{7}",
        )
        .unwrap_err();
        assert!(
            !error.contains('\u{1b}') && !error.contains('\u{7}'),
            "{error}"
        );
    }

    fn plain_run(text: &str) -> StyledRun {
        use remuda_core::agent::Color;
        StyledRun {
            text: text.to_owned(),
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
            wide: false,
        }
    }

    #[test]
    fn remote_capture_sends_the_exact_request_and_pinned_key() {
        let address: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let result = capture_remote(
            "SHA256:peer",
            address,
            b"pinned-key",
            "agent/slash",
            |seen_address, seen_key, request| {
                assert_eq!(seen_address, address);
                assert_eq!(seen_key, b"pinned-key");
                match request {
                    Request::CaptureStyled { name, scrollback } => {
                        assert_eq!(name, "agent/slash");
                        assert_eq!(*scrollback, 0);
                    }
                    other => panic!("unexpected request: {other:?}"),
                }
                Ok(Response::StyledScreen {
                    rows: vec![vec![plain_run("screen")]],
                    instance_id: Some("instance".into()),
                    output_version: Some(7),
                    wrapped: Vec::new(),
                    scrollback_len: 0,
                    scrollback_total: 0,
                    cursor: remuda_core::agent::Cursor {
                        row: 0,
                        col: 0,
                        visible: true,
                    },
                })
            },
        );
        assert_eq!(result, Ok(Capture::Remote("screen".into())));
    }

    #[test]
    fn remote_capture_errors_are_reported_without_a_local_fallback() {
        let address: SocketAddr = "127.0.0.1:9".parse().unwrap();
        for (error, expected) in [
            (ClientError::Timeout, "unreachable"),
            (ClientError::Refused(403), "refused"),
            (ClientError::Crypto, "protocol"),
            (ClientError::BadResponse, "protocol"),
        ] {
            let result = capture_remote("SHA256:peer", address, b"key", "agent", move |_, _, _| {
                Err(error)
            });
            assert!(result.unwrap_err().contains(expected), "{expected}");
        }
        let unexpected = capture_remote("SHA256:peer", address, b"key", "agent", |_, _, _| {
            Ok(Response::Busy)
        });
        assert!(unexpected.unwrap_err().contains("unexpected response"));
    }

    #[test]
    fn rendered_rows_keep_lf_and_drop_control_characters() {
        let rows = vec![
            vec![plain_run("a\u{1b}]0;pwn\u{7}b")],
            vec![plain_run("c\u{009b}31md")],
        ];
        assert_eq!(render_rows(&rows), "a]0;pwnb\nc31md");
    }

    #[test]
    fn streaming_truncation_keeps_a_utf8_boundary_and_counts_the_marker() {
        let rows = vec![vec![plain_run(&"é".repeat(MAX_CAPTURE_BYTES))]];
        let bounded = render_rows(&rows);
        assert!(bounded.len() <= MAX_CAPTURE_BYTES);
        assert!(bounded.ends_with(TRUNCATION_MARKER));
        assert!(bounded.is_char_boundary(bounded.len() - TRUNCATION_MARKER.len()));
        assert!(bounded[..bounded.len() - TRUNCATION_MARKER.len()]
            .chars()
            .all(|character| character == 'é'));
    }

    #[test]
    fn text_at_the_bound_is_untouched_and_empty_rows_survive() {
        let exact = vec![vec![plain_run(&"x".repeat(MAX_CAPTURE_BYTES))]];
        assert_eq!(render_rows(&exact).len(), MAX_CAPTURE_BYTES);
        let over = vec![vec![plain_run(&"x".repeat(MAX_CAPTURE_BYTES + 1))]];
        assert!(render_rows(&over).ends_with(TRUNCATION_MARKER));
        let rows = vec![vec![plain_run("a")], Vec::new(), vec![plain_run("b")]];
        assert_eq!(render_rows(&rows), "a\n\nb");
    }
}
