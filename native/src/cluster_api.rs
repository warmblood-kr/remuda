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
use std::io;
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
        /// Positive peer advertisement of the session-facts sidecar (v1).
        session_facts_v1: bool,
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
                session_facts_v1: advertises_session_facts(&snapshot.registry, &fingerprint),
                fingerprint,
                address: resolved.address,
                pinned_static_key: resolved.pinned_static_key,
            })
        }
    }
}

/// Positive v1 advertisement only; a missing or malformed field is no.
fn advertises_session_facts(registry: &cluster::Registry, fingerprint: &str) -> bool {
    registry
        .authorized_nodes
        .iter()
        .find(|entry| entry.node_fp == fingerprint)
        .and_then(cluster::registry::session_facts_versions)
        .is_some_and(|versions| {
            versions
                .iter()
                .any(|version| version == remuda_core::protocol::CLUSTER_SESSION_FACTS_V1)
        })
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
        ..
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

/// Upper bound for one remote send line. This is the receiver's batch bound,
/// and it is the text length: the receiver appends the one Return, so a sender
/// that adds its own newline would submit twice.
pub const MAX_REMOTE_SEND_BYTES: usize = crate::remote_front::MAX_REMOTE_INPUT_BATCH_BYTES;

/// The result of a remote-aware send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Send {
    /// The target names this node; the caller keeps its existing local path.
    Local,
    /// The peer accepted the line. `duplicate` means this exact client/sequence
    /// was already applied; it is not a new turn or a completion guarantee.
    Remote { duplicate: bool },
}

/// A failed remote send, classified by whether the peer could have typed the
/// line. None is safe to retry automatically: even a refusal is final for this
/// call, and a retry makes a new client id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendFailure {
    /// Refused before framing or by the receiver's own policy; nothing typed.
    Refused(String),
    /// The peer explicitly refused the write before applying it; nothing typed.
    NotWritten(String),
    /// Bytes may have been typed; do not retry with a new client id.
    Uncertain(String),
}

impl SendFailure {
    pub fn message(&self) -> &str {
        match self {
            Self::Refused(message) | Self::NotWritten(message) | Self::Uncertain(message) => {
                message
            }
        }
    }

    /// Every remote send failure is non-retryable in this slice.
    pub fn retryable(&self) -> bool {
        false
    }
}

impl std::fmt::Display for SendFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

/// Send one line to a session on a peer. A refusal is final: this never falls
/// back to the local daemon, never retries with a new client id, and never
/// resolves the node again after a request has started.
pub fn send(target: &str, session: &str, text: &str) -> Result<Send, SendFailure> {
    let refused = SendFailure::Refused;
    let ResolvedCall::Remote {
        fingerprint,
        address,
        pinned_static_key,
        ..
    } = resolve(target).map_err(refused)?
    else {
        return Ok(Send::Local);
    };
    let enabled = cluster::control::remote_send_enabled()
        .map_err(|_| refused("cluster settings are unreadable".to_owned()))?;
    check_remote_send_enabled(enabled)?;
    validate_send_text(text).map_err(refused)?;
    let private_key = cluster::identity::load_static_private_key()
        .map_err(|_| refused("cluster identity is unavailable".to_owned()))?;
    let mut call = |address, pinned: &[u8], request: &Request| {
        ClusterClient::system().request(address, pinned, &private_key, request)
    };
    send_remote(
        &fingerprint,
        address,
        &pinned_static_key,
        session,
        text,
        &mut call,
    )
}

fn check_remote_send_enabled(enabled: bool) -> Result<(), SendFailure> {
    if enabled {
        return Ok(());
    }
    Err(SendFailure::Refused(
        "remote send is disabled on this node; run `remuda cluster remote-send on` after protected panes are verified"
            .into(),
    ))
}

/// One line of text, without the terminator the receiver adds. Control
/// characters are refused rather than stripped, because the line is what the
/// caller asked to type; silently changing it is worse than saying no.
fn validate_send_text(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("send text must not be empty".into());
    }
    if text.len() > MAX_REMOTE_SEND_BYTES {
        return Err(format!("send text exceeds {MAX_REMOTE_SEND_BYTES} bytes"));
    }
    if text.chars().any(is_terminal_control) {
        return Err("send text must be one line without terminal controls".into());
    }
    Ok(())
}

/// The remote half with its transport injected, so the exact probe, list and
/// line requests, and every response class, are exercised without a peer.
fn send_remote(
    fingerprint: &str,
    address: SocketAddr,
    pinned_static_key: &[u8],
    session: &str,
    text: &str,
    call: &mut impl FnMut(SocketAddr, &[u8], &Request) -> Result<Response, ClientError>,
) -> Result<Send, SendFailure> {
    let label = cluster::node_label(fingerprint);
    match call(address, pinned_static_key, &Request::RemoteInputSupport)
        .map_err(|error| preflight_error(fingerprint, error))?
    {
        Response::Ok => {}
        Response::Error(reason) => {
            return Err(SendFailure::Refused(sanitize(format!(
                "send on {label}: the peer does not enforce per-session remote input ({reason})"
            ))))
        }
        _ => {
            return Err(refused(
                &label,
                "the peer did not confirm session remote input",
            ))
        }
    }
    let instance = match call(address, pinned_static_key, &Request::List)
        .map_err(|error| preflight_error(fingerprint, error))?
    {
        Response::Sessions(rows) => {
            pinned_instance(&rows, session).map_err(|error| refused(&label, &error))?
        }
        _ => {
            return Err(refused(
                &label,
                "the peer returned an unexpected session list",
            ))
        }
    };
    let client_id = new_client_id()?;
    let request = Request::RemoteInputLine {
        name: session.to_owned(),
        instance_id: instance,
        client_id,
        seq: 1,
        bytes: text.as_bytes().to_vec(),
    };
    match call(address, pinned_static_key, &request) {
        Err(error) => Err(send_transport_error(fingerprint, error)),
        Ok(Response::Ack { duplicate: false }) => Ok(Send::Remote { duplicate: false }),
        Ok(Response::Ack { duplicate: true }) => Ok(Send::Remote { duplicate: true }),
        Ok(Response::WrongInstance) => Err(refused(
            &label,
            "the target session instance changed; not sent",
        )),
        Ok(Response::RemoteControlDisabled) => Err(refused(
            &label,
            "the target session has remote input disabled",
        )),
        Ok(Response::RateLimited) => Err(known_not_written(
            &label,
            "session input rate limit exceeded",
        )),
        Ok(Response::Busy) => Err(known_not_written(&label, "another write is in flight")),
        Ok(Response::Uncertain) => Err(uncertain(&label, "the peer could not confirm the line")),
        Ok(Response::WriteTimeout) => Err(uncertain(
            &label,
            "the PTY write timed out; delivery may be partial or late",
        )),
        Ok(Response::Error(reason)) if reason == "invalid remote request" => {
            Err(SendFailure::Refused(format!(
                "send on {label}: the peer does not support guarded remote send; nothing was written"
            )))
        }
        Ok(Response::Error(reason)) => Err(uncertain(
            &label,
            &sanitize(format!("the peer reported {reason} after the request")),
        )),
        Ok(_) => Err(uncertain(
            &label,
            "the peer returned an unexpected response",
        )),
    }
}

/// Pick exactly one live, available session row and its exact instance. The
/// local flag is checked here to avoid spending a rate token on a request the
/// receiver would refuse; the receiver's check remains the safety boundary.
fn pinned_instance(rows: &[remuda_core::SessionSummary], session: &str) -> Result<String, String> {
    let mut matching = rows.iter().filter(|row| row.name == session);
    let Some(row) = matching.next() else {
        return Err("no such session on the peer".into());
    };
    if matching.next().is_some() {
        return Err("the peer listed more than one session with that name".into());
    }
    if !row.alive {
        return Err("the target session is not alive".into());
    }
    if row.unavailable {
        return Err("the target session is unavailable on its host".into());
    }
    if row.remote_input_disabled {
        return Err("the target session has remote input disabled".into());
    }
    row.instance_id
        .clone()
        .filter(|instance| !instance.is_empty())
        .ok_or_else(|| "the peer cannot prove the target instance".to_owned())
}

fn new_client_id() -> Result<String, SendFailure> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| SendFailure::Refused("randomness unavailable".to_owned()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// A failed probe or list cannot have written input, so it is reported as not
/// sent rather than uncertain. Only the line request itself can be uncertain.
fn preflight_error(fingerprint: &str, error: ClientError) -> SendFailure {
    let label = cluster::node_label(fingerprint);
    SendFailure::Refused(format!("send on {label}: not sent: {error}"))
}

fn send_transport_error(fingerprint: &str, error: ClientError) -> SendFailure {
    let label = cluster::node_label(fingerprint);
    let not_written = matches!(
        error,
        ClientError::Unreachable
            | ClientError::PeerNotListening
            | ClientError::OsBlockedConnection
            | ClientError::Refused(_)
            | ClientError::RequestTooLarge
    );
    if not_written {
        return known_not_written(&label, &format!("{error}"));
    }
    uncertain(&label, &format!("{error}"))
}

fn refused(label: &str, reason: &str) -> SendFailure {
    SendFailure::Refused(format!("send on {label}: {reason}; not sent"))
}

fn known_not_written(label: &str, reason: &str) -> SendFailure {
    SendFailure::NotWritten(format!("send on {label}: not written: {reason}"))
}

fn uncertain(label: &str, reason: &str) -> SendFailure {
    SendFailure::Uncertain(format!("send on {label}: uncertain, not retried: {reason}"))
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

/// Resolve a user target with an optional routing override, keeping the IO
/// error kind so the CLI maps typed failures instead of parsing text. The
/// pinned identity always comes from the registry entry, never the override.
pub fn resolve_call(target: &str, addr_override: Option<SocketAddr>) -> io::Result<ResolvedCall> {
    let snapshot = cluster::snapshot()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no cluster is initialized; run `remuda cluster init`",
        )
    })?;
    let entry = match cluster::addressing::resolve_user_target(
        &snapshot.registry.authorized_nodes,
        &snapshot.aliases,
        target,
    ) {
        Ok(entry) => entry,
        // This node's hostname is local only when no admitted node claims it.
        Err(error)
            if snapshot.identity.node_name == target && error.kind() == io::ErrorKind::NotFound =>
        {
            return Ok(ResolvedCall::Local);
        }
        Err(error) => return Err(error),
    };
    if entry.state != NodeState::Admitted {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("node {} is revoked", cluster::node_label(&entry.node_fp)),
        ));
    }
    if snapshot.identity.node_name == target && entry.node_fp != snapshot.identity.node_fp {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "node {target} is ambiguous: it is both this node's hostname and another admitted node; use the fingerprint"
            ),
        ));
    }
    if entry.node_fp == snapshot.identity.node_fp {
        return Ok(ResolvedCall::Local);
    }
    let resolved = cluster::resolve_target(&entry.node_fp, addr_override)?;
    Ok(ResolvedCall::Remote {
        fingerprint: entry.node_fp.clone(),
        address: resolved.address,
        pinned_static_key: resolved.pinned_static_key,
        session_facts_v1: cluster::registry::session_facts_versions(entry).is_some_and(
            |versions| {
                versions
                    .iter()
                    .any(|version| version == remuda_core::protocol::CLUSTER_SESSION_FACTS_V1)
            },
        ),
    })
}

/// The result of a remote-aware list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum List {
    /// The target names this node; the caller keeps its existing local path.
    Local,
    /// Bounded, sanitized peer rows in peer list order.
    Remote(String),
}

/// At most this many session lines before the truncation marker.
const MAX_LIST_LINES: usize = 256;
/// At most this many bytes of list text, marker included.
const MAX_LIST_BYTES: usize = 32 * 1024;
const LIST_TRUNCATION_MARKER: &str = "... [list truncated]";

/// List a peer's sessions. One authenticated `List`; a local target is reported
/// as [`List::Local`] so the local MCP path stays byte-identical. Never
/// evaluates, syncs, captures, writes, or reads settings.
pub fn list(target: &str) -> Result<List, String> {
    let ResolvedCall::Remote {
        fingerprint,
        address,
        pinned_static_key,
        session_facts_v1,
    } = resolve_call(target, None).map_err(|error| sanitize(format!("cluster: {error}")))?
    else {
        return Ok(List::Local);
    };
    let private_key = cluster::identity::load_static_private_key()
        .map_err(|_| "cluster identity is unavailable".to_owned())?;
    let client = ClusterClient::system();
    let started = std::time::Instant::now();
    let response = client
        .request(address, &pinned_static_key, &private_key, &Request::List)
        .map_err(|error| list_remote_error(&fingerprint, error))?;
    let rows = match response {
        Response::Sessions(rows) => rows,
        Response::Error(message) => {
            return Err(sanitize(format!(
                "list on {}: {message}",
                cluster::node_label(&fingerprint)
            )))
        }
        Response::RemoteControlDisabled => {
            return Err(format!(
                "list on {}: remote control is disabled there",
                cluster::node_label(&fingerprint)
            ))
        }
        _ => {
            return Err(format!(
                "list on {}: peer returned an unexpected response",
                cluster::node_label(&fingerprint)
            ))
        }
    };
    // Positive advertisement only. Any sidecar failure leaves the base rows
    // intact; the elapsed List request still ages typed status.
    let facts = if session_facts_v1 {
        match client.request(
            address,
            &pinned_static_key,
            &private_key,
            &Request::ClusterSessionFacts,
        ) {
            Ok(Response::ClusterSessionFacts(entries)) => entries,
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    Ok(List::Remote(remote_sessions_text_with_facts(
        &rows, &facts, elapsed_ms,
    )))
}

fn list_remote_error(fingerprint: &str, error: ClientError) -> String {
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
        "list on {}: {code}: {error}",
        cluster::node_label(fingerprint)
    )
}

/// Every peer row: one line each, in the peer's order, bounded by whole lines.
#[cfg(test)]
#[path = "enrichment_contract/remote_text.rs"]
mod enrichment_contract;

#[cfg(test)]
pub(crate) fn enrichment_contract_remote_text(sessions: &[remuda_core::SessionSummary]) -> String {
    remote_sessions_text(sessions)
}

#[cfg(test)]
fn remote_sessions_text(sessions: &[remuda_core::SessionSummary]) -> String {
    remote_sessions_text_with_facts(sessions, &[], 0)
}

fn remote_sessions_text_with_facts(
    sessions: &[remuda_core::SessionSummary],
    facts: &[remuda_core::protocol::ClusterSessionFactWire],
    elapsed_ms: u64,
) -> String {
    let mut text = String::new();
    let mut truncated = false;
    for (index, session) in sessions.iter().enumerate() {
        let fact = session
            .instance_id
            .as_deref()
            .and_then(|instance| {
                facts
                    .iter()
                    .find(|fact| fact.wire_name == session.name && fact.instance_id == instance)
            })
            .filter(|fact| fact.valid_for_ms > elapsed_ms);
        let line = remote_list_line(session, fact, elapsed_ms);
        let separator = usize::from(!text.is_empty());
        let more_after = index + 1 < sessions.len();
        // Reserve room for the marker whenever more content exists, so the
        // bound always includes it and the marker is never a partial line.
        let reserve = if more_after {
            separator + LIST_TRUNCATION_MARKER.len()
        } else {
            0
        };
        if text.len() + separator + line.len() + reserve > MAX_LIST_BYTES {
            truncated = true;
            break;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&line);
        if index + 1 == MAX_LIST_LINES && more_after {
            truncated = true;
            break;
        }
    }
    if truncated {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(LIST_TRUNCATION_MARKER);
    }
    text
}

fn remote_list_line(
    session: &remuda_core::SessionSummary,
    fact: Option<&remuda_core::protocol::ClusterSessionFactWire>,
    elapsed_ms: u64,
) -> String {
    let mut line = format!(
        "{}\t{}x{}\talive={}\tidle={:.0}s\toutput_idle={:.0}s\t{}",
        list_name(&session.name),
        session.size.cols(),
        session.size.rows(),
        session.alive,
        session.idle.as_secs_f64(),
        session.output_idle.unwrap_or(session.idle).as_secs_f64(),
        list_instance(&session.instance_id),
    );
    if session.unavailable {
        line.push_str("\tunavailable=true");
    }
    if session.remote_input_disabled {
        line.push_str("\tremote_input_disabled=true");
    }
    if let Some(activity) = activity_field(session, elapsed_ms) {
        line.push_str("\tactivity=");
        line.push_str(activity);
    }
    if let Some(fact) = fact {
        append_fact_fields(&mut line, fact);
    }
    line
}

/// A fresh typed OSC status describes any exact live instance; it is never
/// guessed from terminal text and is suppressed on ended/unavailable/stale rows.
fn activity_field(session: &remuda_core::SessionSummary, elapsed_ms: u64) -> Option<&'static str> {
    if !session.alive || session.unavailable {
        return None;
    }
    if session.instance_id.as_deref().is_none_or(str::is_empty) {
        return None;
    }
    let status = session.program_status.as_ref()?;
    if status.state == remuda_core::agent::ProgramState::Unknown {
        return None;
    }
    let age = session.program_status_age_ms?.saturating_add(elapsed_ms);
    if age >= remuda_core::session_enrichment::program_status_max_age_ms(status.state) {
        return None;
    }
    Some(status.state.as_str())
}

fn append_fact_fields(line: &mut String, fact: &remuda_core::protocol::ClusterSessionFactWire) {
    if let Some(kind) = &fact.kind {
        line.push_str("\tkind=");
        line.push_str(&fact_value(kind));
    } else {
        line.push_str("\tkind=unavailable");
    }
    if let Some(model) = &fact.model {
        line.push_str("\tmodel=");
        line.push_str(&fact_value(model));
    } else {
        line.push_str("\tmodel=unavailable");
    }
    match (fact.context_used, fact.context_window) {
        (Some(used), Some(window)) => {
            line.push_str(&format!("\tctx={used}/{window}"));
            if let Some(percent) = fact.context_percent {
                line.push_str(&format!("\tctx_percent={percent}"));
            }
        }
        _ => line.push_str("\tctx=unavailable"),
    }
}

/// One TAB/key=value field value: framing characters and controls removed.
fn fact_value(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\t' | '\n' | '\r' => ' ',
            other => other,
        })
        .collect::<String>()
        .chars()
        .filter(|character| !is_terminal_control(*character))
        .collect()
}

/// Tab/newline framing becomes one space; other terminal controls are removed.
fn list_name(name: &str) -> String {
    let framed: String = name
        .chars()
        .map(|character| match character {
            '\t' | '\n' | '\r' | '\u{2028}' | '\u{2029}' => ' ',
            other => other,
        })
        .collect();
    strip_terminal_controls(&framed).into_owned()
}

/// An id is emitted byte-exact only when it is nonempty and control-free; a
/// partial or rewritten id could pin the wrong instance, so it is refused
/// wholesale instead.
fn list_instance(instance: &Option<String>) -> String {
    match instance {
        Some(value) if !value.is_empty() && !value.chars().any(is_terminal_control) => {
            format!("instance_id={value}")
        }
        _ => "instance_id=\tunpinnable=true".to_string(),
    }
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
    use std::time::Duration;

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
    fn line_separators_in_names_become_spaces() {
        assert_eq!(list_name("a\tb\nc\rd\u{2028}e\u{2029}f"), "a b c d e f");
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

    fn summary(name: &str, instance: Option<&str>) -> remuda_core::SessionSummary {
        remuda_core::SessionSummary {
            id: format!("id-{name}"),
            name: name.to_owned(),
            instance_id: instance.map(str::to_owned),
            output_version: Some(1),
            alive: true,
            idle: Duration::ZERO,
            output_idle: Some(Duration::ZERO),
            size: remuda_core::Size::new(80, 24),
            attached: false,
            human_idle: None,
            mouse_tracking: false,
            unavailable: false,
            program_status: None,
            program_status_age_ms: None,
            remote_input_disabled: false,
        }
    }

    fn list(rows: Vec<remuda_core::SessionSummary>) -> Response {
        Response::Sessions(rows)
    }

    #[test]
    fn remote_send_is_off_until_the_local_rollout_gate_is_open() {
        let error = check_remote_send_enabled(false).unwrap_err();
        assert!(error.message().contains("remote-send on"), "{error}");
        assert!(check_remote_send_enabled(true).is_ok());
    }

    #[test]
    fn remote_send_probes_lists_and_sends_one_exact_line() {
        let address: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let mut seen = Vec::new();
        let result = send_remote(
            "SHA256:peer",
            address,
            b"pinned-key",
            "agent/slash",
            "hello",
            &mut |seen_address, seen_key, request| {
                assert_eq!(seen_address, address);
                assert_eq!(seen_key, b"pinned-key");
                seen.push(request.clone());
                match request {
                    Request::RemoteInputSupport => Ok(Response::Ok),
                    Request::List => Ok(list(vec![
                        summary("other", Some("x")),
                        summary("agent/slash", Some("instance")),
                    ])),
                    Request::RemoteInputLine {
                        name,
                        instance_id,
                        client_id,
                        seq,
                        bytes,
                    } => {
                        assert_eq!(name, "agent/slash");
                        assert_eq!(instance_id, "instance");
                        assert_eq!(*seq, 1);
                        assert_eq!(client_id.len(), 32);
                        assert!(client_id.bytes().all(|byte| byte.is_ascii_hexdigit()));
                        assert_eq!(bytes, b"hello", "the sender adds no Return");
                        Ok(Response::Ack { duplicate: false })
                    }
                    other => panic!("unexpected request: {other:?}"),
                }
            },
        );
        assert_eq!(result, Ok(Send::Remote { duplicate: false }));
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn remote_send_refuses_an_unproven_peer_before_listing() {
        let mut calls = 0;
        let result = send_remote(
            "SHA256:old",
            "127.0.0.1:9".parse().unwrap(),
            b"key",
            "agent",
            "x",
            &mut |_, _, _| {
                calls += 1;
                Ok(Response::Error("invalid remote request".into()))
            },
        );
        let error = result.expect_err("old peer refused");
        assert!(
            error.message().contains("does not enforce per-session"),
            "{error:?}"
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn remote_send_refuses_rows_it_cannot_pin_and_never_writes() {
        let cases = [
            (Vec::new(), "no such session"),
            (vec![summary("agent", None)], "cannot prove"),
            (
                {
                    let mut row = summary("agent", Some("instance"));
                    row.alive = false;
                    vec![row]
                },
                "not alive",
            ),
            (
                {
                    let mut row = summary("agent", Some("instance"));
                    row.unavailable = true;
                    vec![row]
                },
                "unavailable",
            ),
            (
                {
                    let mut row = summary("agent", Some("instance"));
                    row.remote_input_disabled = true;
                    vec![row]
                },
                "disabled",
            ),
            (
                vec![summary("agent", Some("one")), summary("agent", Some("two"))],
                "more than one",
            ),
        ];
        for (rows, needle) in cases {
            let mut line_calls = 0;
            let result = send_remote(
                "SHA256:peer",
                "127.0.0.1:9".parse().unwrap(),
                b"key",
                "agent",
                "x",
                &mut |_, _, request| match request {
                    Request::RemoteInputSupport => Ok(Response::Ok),
                    Request::List => Ok(list(rows.clone())),
                    _ => {
                        line_calls += 1;
                        Ok(Response::Ack { duplicate: false })
                    }
                },
            );
            let error = result.expect_err("must refuse");
            assert!(error.message().contains(needle), "{needle}: {error:?}");
            assert_eq!(line_calls, 0, "no line request for {needle}");
        }
    }

    #[test]
    fn remote_send_classifies_every_answer() {
        let address: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let answer = |response: Response| {
            send_remote(
                "SHA256:peer",
                address,
                b"key",
                "agent",
                "x",
                &mut |_, _, request| match request {
                    Request::RemoteInputSupport => Ok(Response::Ok),
                    Request::List => Ok(list(vec![summary("agent", Some("instance"))])),
                    _ => Ok(response.clone()),
                },
            )
        };
        assert_eq!(
            answer(Response::Ack { duplicate: false }),
            Ok(Send::Remote { duplicate: false })
        );
        assert_eq!(
            answer(Response::Ack { duplicate: true }),
            Ok(Send::Remote { duplicate: true })
        );
        for (response, needle) in [
            (Response::WrongInstance, "instance changed"),
            (Response::RemoteControlDisabled, "remote input disabled"),
            (Response::RateLimited, "rate limit"),
            (Response::Busy, "in flight"),
            (Response::Uncertain, "uncertain"),
            (Response::WriteTimeout, "uncertain"),
            (Response::Error("no such session".into()), "uncertain"),
            (Response::Ok, "unexpected"),
        ] {
            let error = answer(response).unwrap_err();
            assert!(
                error.message().contains(needle),
                "{error} does not contain {needle}"
            );
        }
        let rate = answer(Response::RateLimited).unwrap_err();
        assert!(rate.message().contains("not written"));
        assert!(!rate.retryable());
        let timeout = answer(Response::WriteTimeout).unwrap_err();
        assert!(timeout.message().contains("not retried"));
        assert!(matches!(timeout, SendFailure::Uncertain(_)));
    }

    #[test]
    fn a_guarded_line_that_an_old_receiver_cannot_parse_writes_nothing() {
        let result = send_remote(
            "SHA256:peer",
            "127.0.0.1:9".parse().unwrap(),
            b"key",
            "agent",
            "x",
            &mut |_, _, request| match request {
                Request::RemoteInputSupport => Ok(Response::Ok),
                Request::List => Ok(list(vec![summary("agent", Some("instance"))])),
                Request::RemoteInputLine { .. } => {
                    Ok(Response::Error("invalid remote request".into()))
                }
                other => panic!("unexpected request: {other:?}"),
            },
        );
        let error = result.expect_err("old receiver");
        assert!(error.message().contains("guarded remote send"));
        assert!(matches!(error, SendFailure::Refused(_)));
    }

    #[test]
    fn remote_send_transport_errors_are_never_a_local_retry() {
        let address: SocketAddr = "127.0.0.1:9".parse().unwrap();
        for error in [
            ClientError::Unreachable,
            ClientError::PeerNotListening,
            ClientError::Timeout,
            ClientError::PeerClosedConnection,
            ClientError::Crypto,
            ClientError::BadResponse,
        ] {
            let needle = "not sent";
            let result = send_remote(
                "SHA256:peer",
                address,
                b"key",
                "agent",
                "x",
                &mut |_, _, request| match request {
                    Request::RemoteInputSupport => Err(error),
                    _ => panic!("must not continue after a failed probe"),
                },
            );
            let error = result.expect_err("must refuse");
            assert!(error.message().contains(needle), "{needle}: {error:?}");
        }
        // A line failure after a good probe is uncertain, never a second request.
        for (error, needle) in [
            (ClientError::Timeout, "uncertain"),
            (ClientError::PeerClosedConnection, "uncertain"),
            (ClientError::Crypto, "uncertain"),
            (ClientError::PeerTooNew, "uncertain"),
        ] {
            let mut line_calls = 0;
            let result = send_remote(
                "SHA256:peer",
                address,
                b"key",
                "agent",
                "x",
                &mut |_, _, request| match request {
                    Request::RemoteInputSupport => Ok(Response::Ok),
                    Request::List => Ok(list(vec![summary("agent", Some("instance"))])),
                    _ => {
                        line_calls += 1;
                        Err(error)
                    }
                },
            );
            let error = result.expect_err("must refuse");
            assert!(error.message().contains(needle), "{needle}: {error:?}");
            assert_eq!(line_calls, 1);
        }
    }

    #[test]
    fn remote_send_text_is_one_bounded_line() {
        assert!(validate_send_text("").is_err());
        assert!(validate_send_text("a\nb").is_err());
        assert!(validate_send_text("a\rb").is_err());
        assert!(validate_send_text("a\u{1b}b").is_err());
        assert!(validate_send_text("a\u{9b}b").is_err());
        assert!(validate_send_text(&"x".repeat(MAX_REMOTE_SEND_BYTES)).is_ok());
        assert!(validate_send_text(&"x".repeat(MAX_REMOTE_SEND_BYTES + 1)).is_err());
        assert!(validate_send_text("한 line").is_ok());
    }

    #[test]
    fn remote_text_adds_bounded_managed_fields_but_never_join_ids() {
        use remuda_core::agent::{ProgramState, ProgramStatus};
        use remuda_core::protocol::ClusterSessionFactWire;
        let mut row = crate::enrichment_contract::summary("worker", "MID", Some("LEAD"));
        row.program_status = Some(ProgramStatus {
            state: ProgramState::Working,
            app: None,
            kind: None,
            progress: None,
        });
        row.program_status_age_ms = Some(0);
        let fact = ClusterSessionFactWire {
            wire_name: "worker".into(),
            instance_id: row.instance_id.clone().unwrap(),
            valid_for_ms: 10_000,
            member_id: "MID".into(),
            leader_id: Some("LEAD".into()),
            alias: Some("worker".into()),
            kind: Some("codex".into()),
            model: None,
            context_used: Some(42_000),
            context_window: Some(100_000),
            context_percent: Some(42),
        };
        let text = remote_sessions_text_with_facts(&[row.clone()], std::slice::from_ref(&fact), 0);
        assert!(text.ends_with(
            "\tactivity=working\tkind=codex\tmodel=unavailable\tctx=42000/100000\tctx_percent=42"
        ), "{text}");
        assert!(!text.contains("MID") && !text.contains("LEAD"));
        assert!(!text.contains("member_id") && !text.contains("leader_id"));

        // Expired facts never reach the row, while the base framing stays.
        let expired = ClusterSessionFactWire {
            valid_for_ms: 0,
            ..fact
        };
        let text = remote_sessions_text_with_facts(&[row], &[expired], 0);
        assert!(text.contains("worker") && text.contains("alive=true"));
        assert!(!text.contains("kind=codex") && !text.contains("MID"));
    }
}
