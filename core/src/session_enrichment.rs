//! Cluster presentation boundary: resolve owner-published facts into a stable
//! forest and format one cluster row.
//!
//! Nothing here is wired to local text or local JSON. Durable member/leader
//! IDs are consumed only inside this module to build an opaque token tree;
//! they never enter [`ResolvedSession`] or any rendered row.
use crate::agent::{ProgramState, ProgramStatus};
use crate::SessionSummary;
use std::collections::{HashMap, HashSet};

/// Field applicability is distinct from a known missing/stale managed value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldState<T> {
    NotApplicable,
    Unavailable,
    Value(T),
}

/// Separate cluster-only sidecar; never add it to local SessionSummary JSON.
/// Request only after positive session_facts=v1 capability advertisement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterSessionFacts {
    pub entries: Vec<ClusterSessionFact>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterSessionFact {
    pub wire_name: String,
    pub instance_id: String,
    pub valid_for_ms: u64,
    pub butler: ButlerFacts,
}

/// Informational allowlist. Sharing durable IDs with List-authorized peers is
/// intentional; only the owner publishes them after exact-instance validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ButlerFacts {
    pub member_id: String,
    pub leader_id: Option<String>,
    pub alias: Option<String>,
    pub kind: FieldState<String>,
    pub model: FieldState<String>,
    pub context_used: FieldState<u64>,
    pub context_window: FieldState<u64>,
    pub context_percent: FieldState<u8>,
}

/// Facts expire against the original request-start deadline, not redraw/Sync.
pub struct ListedTarget<'a> {
    pub token: u32,
    pub summary: &'a SessionSummary,
    /// Positive current owner evidence bound to summary name + instance.
    /// Unknown/no publisher is false; never infer membership from names.
    pub managed: bool,
    pub butler: Option<&'a ButlerFacts>,
    pub facts_remaining_ms: u64,
}

/// Callers partition by stable owning-node identity before resolving.
pub struct NodeRoster<'a> {
    pub stale_or_unreachable: bool,
    pub list_elapsed_ms: u64,
    pub targets: &'a [ListedTarget<'a>],
}

/// Safe facts for cluster-facing text and frontend composition; no durable IDs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentationFacts {
    pub kind: FieldState<String>,
    pub model: FieldState<String>,
    pub context_used: FieldState<u64>,
    pub context_window: FieldState<u64>,
    pub context_percent: FieldState<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataState {
    NotProvided,
    Unavailable,
    Available(PresentationFacts),
}

/// No override of canonical liveness, input policy, selection or routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSession {
    pub token: u32,
    pub parent_token: Option<u32>,
    pub parent_label: Option<String>,
    /// True cluster depth; visual indentation is capped at 20 levels.
    pub depth: usize,
    pub metadata: MetadataState,
    /// Reuse the existing typed OSC report; no second wire activity field.
    pub program_status: Option<ProgramStatus>,
}

/// Cluster/remote picker and List-backed cluster text share this boundary.
/// Existing non-cluster local format/order is outside its scope.
///
/// Return every token once in stable forest preorder. Resolve unique durable
/// IDs within one node; missing, ambiguous, self and cyclic edges become roots.
/// Unknown ownership leaves ordinary compact base rows (NotProvided).
/// Managed missing/stale fields mean unavailable; never infer unmanaged.
/// Fresh typed program status can describe ANY
/// exact live instance. Suppress it on ended/unavailable/stale rows. Never
/// classify from terminal text or use advice as input/close authority.
pub trait SessionListEnrichment {
    fn resolve(&self, roster: &NodeRoster<'_>) -> Result<Vec<ResolvedSession>, EnrichmentError>;
}

/// A target or fact set the resolver refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnrichmentError {
    InvalidTarget,
}

/// Ten seconds is the maximum native publication lifetime; the reader ages it
/// from its own original List request start and never renews it on redraw.
pub const CLUSTER_FACTS_TTL_MS: u64 = 10_000;
/// Upper bound on entries a sidecar response may carry.
pub const MAX_CLUSTER_FACTS_ENTRIES: usize = 4096;
/// Upper bound on one published string, in bytes.
pub const MAX_FACT_STRING_BYTES: usize = 128;
/// Independent response cap: even a full 4096-entry response is shed well
/// before the base List frame limit.
pub const MAX_CLUSTER_FACTS_BYTES: usize = 256 * 1024;
/// Freshness requirement for an authoritative typed program status.
pub const PROGRAM_STATUS_MAX_AGE_MS: u64 = 60_000;
/// Visual indentation is capped at 20 levels (two cells each).
pub const MAX_VISUAL_DEPTH: usize = 20;

impl ButlerFacts {
    /// Validate owner-supplied facts before they enter the registry or wire.
    pub fn validate(&self) -> Result<(), EnrichmentError> {
        if self.member_id.is_empty() || !safe_fact_text(&self.member_id) {
            return Err(EnrichmentError::InvalidTarget);
        }
        for text in [
            self.leader_id.as_deref(),
            self.alias.as_deref(),
            value_text(&self.kind),
            value_text(&self.model),
        ]
        .into_iter()
        .flatten()
        {
            if text.is_empty() || !safe_fact_text(text) {
                return Err(EnrichmentError::InvalidTarget);
            }
        }
        let used = value_number(&self.context_used);
        let window = value_number(&self.context_window);
        if let FieldState::Value(percent) = self.context_percent {
            if percent > 100 {
                return Err(EnrichmentError::InvalidTarget);
            }
        }
        if window == Some(0) {
            return Err(EnrichmentError::InvalidTarget);
        }
        if let (Some(used), Some(window)) = (used, window) {
            if used > window {
                return Err(EnrichmentError::InvalidTarget);
            }
        }
        Ok(())
    }
}

fn value_text(state: &FieldState<String>) -> Option<&str> {
    match state {
        FieldState::Value(text) => Some(text),
        _ => None,
    }
}

fn value_number(state: &FieldState<u64>) -> Option<u64> {
    match state {
        FieldState::Value(number) => Some(*number),
        _ => None,
    }
}

/// Control, bidi and separator characters that must never reach a row or log.
pub fn safe_fact_text(text: &str) -> bool {
    text.len() <= MAX_FACT_STRING_BYTES
        && !text.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{2028}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
}

fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{2028}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
        .collect()
}

/// Callable production seam for executable specifications.
#[doc(hidden)]
pub fn resolve_cluster_roster(
    roster: &NodeRoster<'_>,
) -> Result<Vec<ResolvedSession>, EnrichmentError> {
    let targets = roster.targets;
    let usable = usable_targets(roster);
    let member_index = unique_member_index(targets, &usable);
    let mut parent = parent_edges(targets, &usable, &member_index);
    break_cycles(&mut parent);
    Ok(preorder(&parent)
        .into_iter()
        .map(|(index, depth)| resolved_row(roster, targets, &usable, &parent, (index, depth)))
        .collect())
}

/// A target is managed only with owner facts bound to the exact current
/// name+instance and still inside the original request-start deadline.
fn usable_targets(roster: &NodeRoster<'_>) -> Vec<bool> {
    roster
        .targets
        .iter()
        .map(|target| {
            target.managed
                && target.butler.is_some()
                && !roster.stale_or_unreachable
                && target.facts_remaining_ms > roster.list_elapsed_ms
        })
        .collect()
}

/// Unique member IDs only: a duplicated ID never becomes a parent.
fn unique_member_index<'a>(
    targets: &'a [ListedTarget<'a>],
    usable: &[bool],
) -> HashMap<&'a str, usize> {
    let mut member_index: HashMap<&str, usize> = HashMap::new();
    let mut duplicated: HashSet<&str> = HashSet::new();
    for (index, target) in targets.iter().enumerate() {
        if !usable[index] {
            continue;
        }
        let facts = target.butler.expect("usable facts exist");
        if facts.member_id.is_empty() {
            continue;
        }
        if member_index
            .insert(facts.member_id.as_str(), index)
            .is_some()
        {
            duplicated.insert(facts.member_id.as_str());
        }
    }
    for id in duplicated {
        member_index.remove(id);
    }
    member_index
}

/// One parent edge per node, joined by exact durable member ID only.
fn parent_edges(
    targets: &[ListedTarget<'_>],
    usable: &[bool],
    member_index: &HashMap<&str, usize>,
) -> Vec<Option<usize>> {
    let mut parent: Vec<Option<usize>> = vec![None; targets.len()];
    for (index, target) in targets.iter().enumerate() {
        if !usable[index] {
            continue;
        }
        let facts = target.butler.expect("usable facts exist");
        let Some(leader) = facts.leader_id.as_deref() else {
            continue;
        };
        if leader.is_empty() || leader == facts.member_id {
            continue;
        }
        let Some(&parent_index) = member_index.get(leader) else {
            continue;
        };
        if parent_index != index {
            parent[index] = Some(parent_index);
        }
    }
    parent
}

/// Every internal edge of a cycle is removed; valid children hanging off a
/// cycle member keep their edge. Iterative so a 5000-chain cannot recurse.
fn break_cycles(parent: &mut [Option<usize>]) {
    let mut state = vec![0u8; parent.len()];
    let starts: Vec<usize> = parent
        .iter()
        .enumerate()
        .filter(|(_, parent_of)| parent_of.is_some())
        .map(|(index, _)| index)
        .collect();
    for start in starts {
        if state[start] != 0 {
            continue;
        }
        let mut path = Vec::new();
        let mut cursor = Some(start);
        while let Some(node) = cursor {
            match state[node] {
                0 => {
                    state[node] = 1;
                    path.push(node);
                    cursor = parent[node];
                }
                1 => {
                    if let Some(position) = path.iter().position(|&item| item == node) {
                        for &cycle_node in &path[position..] {
                            parent[cycle_node] = None;
                        }
                    }
                    break;
                }
                _ => break,
            }
        }
        for node in path {
            if state[node] == 1 {
                state[node] = 2;
            }
        }
    }
}

/// Stable roots in original order, then iterative preorder; children keep
/// original order because they were pushed in index order.
fn preorder(parent: &[Option<usize>]) -> Vec<(usize, usize)> {
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); parent.len()];
    let mut roots = Vec::new();
    for (index, parent_of) in parent.iter().enumerate() {
        match parent_of {
            Some(parent_index) => children[*parent_index].push(index),
            None => roots.push(index),
        }
    }
    let mut ordered = Vec::with_capacity(parent.len());
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|&root| (root, 0)).collect();
    while let Some((index, depth)) = stack.pop() {
        ordered.push((index, depth));
        for &child in children[index].iter().rev() {
            stack.push((child, depth + 1));
        }
    }
    ordered
}

fn resolved_row(
    roster: &NodeRoster<'_>,
    targets: &[ListedTarget<'_>],
    usable: &[bool],
    parent: &[Option<usize>],
    (index, depth): (usize, usize),
) -> ResolvedSession {
    let target = &targets[index];
    let metadata = if usable[index] {
        MetadataState::Available(presentation_facts(
            target.butler.expect("usable facts exist"),
        ))
    } else if target.managed {
        MetadataState::Unavailable
    } else {
        MetadataState::NotProvided
    };
    ResolvedSession {
        token: target.token,
        parent_token: parent[index].map(|parent_index| targets[parent_index].token),
        parent_label: parent[index].map(|parent_index| targets[parent_index].summary.name.clone()),
        depth,
        metadata,
        program_status: fresh_program_status(roster, target.summary),
    }
}

fn presentation_facts(facts: &ButlerFacts) -> PresentationFacts {
    PresentationFacts {
        kind: facts.kind.clone(),
        model: facts.model.clone(),
        context_used: facts.context_used.clone(),
        context_window: facts.context_window.clone(),
        context_percent: facts.context_percent.clone(),
    }
}

fn fresh_program_status(
    roster: &NodeRoster<'_>,
    summary: &SessionSummary,
) -> Option<ProgramStatus> {
    if roster.stale_or_unreachable
        || !summary.alive
        || summary.unavailable
        || summary.instance_id.as_deref().is_none_or(str::is_empty)
    {
        return None;
    }
    let age = summary.program_status_age_ms?;
    if age >= PROGRAM_STATUS_MAX_AGE_MS {
        return None;
    }
    let status = summary.program_status.as_ref()?;
    if status.state == ProgramState::Unknown {
        return None;
    }
    Some(status.clone())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClusterFactsRequestPlan {
    BaseListOnly,
    FetchFactsV1,
}

/// Never send a new request to an old peer: require positively advertised
/// AuthorizedNode optional_fields.session_facts containing v1 first.
#[doc(hidden)]
pub fn plan_cluster_facts_request(
    advertised_versions: Option<&[String]>,
) -> Result<ClusterFactsRequestPlan, EnrichmentError> {
    Ok(match advertised_versions {
        Some(versions) if versions.iter().any(|version| version == "v1") => {
            ClusterFactsRequestPlan::FetchFactsV1
        }
        _ => ClusterFactsRequestPlan::BaseListOnly,
    })
}

/// Shared cluster-only formatter seam; canonical state has priority over
/// optional facts at narrow widths. Ordinary local text never calls this.
#[doc(hidden)]
pub fn format_cluster_row(
    row: &ResolvedSession,
    name: &str,
    canonical_status: &str,
    width: usize,
) -> Result<String, EnrichmentError> {
    let (label, status) = format_cluster_row_parts(row, name, canonical_status, width)?;
    if label.is_empty() {
        Ok(status)
    } else if status.is_empty() {
        Ok(label)
    } else {
        Ok(format!("{label} · {status}"))
    }
}

/// Split form for the picker, which keeps its own status column. The label
/// never contains the canonical status; the caller renders it separately.
#[doc(hidden)]
pub fn format_cluster_row_parts(
    row: &ResolvedSession,
    name: &str,
    canonical_status: &str,
    width: usize,
) -> Result<(String, String), EnrichmentError> {
    let name = sanitize(name);
    let status = sanitize(canonical_status);
    let indent = " ".repeat(row.depth.min(MAX_VISUAL_DEPTH) * 2);
    let facts = facts_text(&row.metadata);
    let parent = row
        .parent_label
        .as_deref()
        .map(|label| format!("← {}", sanitize(label)))
        .filter(|label| !label.is_empty());

    let join = |indent: &str, name: &str, facts: &[String], parent: Option<&String>| {
        let mut text = format!("{indent}{name}");
        for part in facts {
            text.push_str(" · ");
            text.push_str(part);
        }
        if let Some(parent) = parent {
            text.push_str(" · ");
            text.push_str(parent);
        }
        text
    };

    let full = join(&indent, &name, &facts, parent.as_ref());
    if fits(&full, &status, width) {
        return Ok((full, status));
    }
    // Narrow widths keep the name and canonical state first: every secondary
    // part (facts, parent label, indent) disappears together.
    let compact = name;
    Ok((compact, status))
}

fn fits(label: &str, status: &str, width: usize) -> bool {
    let label_width = label.chars().count();
    let status_width = if status.is_empty() {
        0
    } else {
        3 + status.chars().count()
    };
    label_width + status_width <= width
}

fn facts_text(metadata: &MetadataState) -> Vec<String> {
    let MetadataState::Available(facts) = metadata else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    if let Some(text) = field_cue("kind", &facts.kind) {
        parts.push(text);
    }
    if let Some(text) = field_cue("model", &facts.model) {
        parts.push(text);
    }
    let context = match (&facts.context_used, &facts.context_window) {
        (FieldState::Value(used), FieldState::Value(window)) => Some(format!(
            "{}/{}",
            compact_number(*used),
            compact_number(*window)
        )),
        (FieldState::Unavailable, _) | (_, FieldState::Unavailable) => {
            Some("context unavailable".to_owned())
        }
        _ => None,
    };
    let context_known = context.is_some();
    if let Some(context) = context {
        parts.push(context);
    }
    match &facts.context_percent {
        FieldState::Value(percent) => parts.push(format!("{percent}% ctx")),
        // A missing percent beside a known context total stays quiet; the
        // known used/window already carries the useful field.
        FieldState::Unavailable if !context_known => parts.push("context unavailable".to_owned()),
        _ => {}
    }
    parts
}

fn field_cue(label: &str, state: &FieldState<String>) -> Option<String> {
    match state {
        FieldState::NotApplicable => None,
        FieldState::Unavailable => Some(format!("{label} unavailable")),
        FieldState::Value(value) => Some(sanitize(value)),
    }
}

fn compact_number(value: u64) -> String {
    if value.is_multiple_of(1000) {
        format!("{}K", value / 1000)
    } else {
        format!("{:.1}K", value as f64 / 1000.0)
    }
}

#[cfg(test)]
#[path = "session_enrichment_contract.rs"]
mod enrichment_contract;

#[cfg(test)]
mod status_age_tests {
    use super::*;
    use crate::agent::{ProgramBlockKind, ProgramState};
    use crate::{ManualClock, Registry, ScriptedAgent, Session};
    use std::sync::Arc;

    fn base(name: &str) -> SessionSummary {
        let registry = Registry::new();
        registry
            .register(Session::new(
                name,
                Box::new(ScriptedAgent::new(vec![])),
                Arc::new(ManualClock::new()),
            ))
            .unwrap();
        registry.list().remove(0)
    }

    fn resolve_status(summary: &SessionSummary, stale: bool) -> Option<ProgramStatus> {
        let targets = [ListedTarget {
            token: 1,
            summary,
            managed: false,
            butler: None,
            facts_remaining_ms: 0,
        }];
        resolve_cluster_roster(&NodeRoster {
            stale_or_unreachable: stale,
            list_elapsed_ms: 0,
            targets: &targets,
        })
        .unwrap()
        .remove(0)
        .program_status
    }

    #[test]
    fn typed_status_is_fresh_strictly_under_sixty_seconds() {
        let mut summary = base("shell");
        summary.program_status = Some(ProgramStatus {
            state: ProgramState::Working,
            app: None,
            kind: Some(ProgramBlockKind::Permission),
            progress: None,
        });
        summary.program_status_age_ms = Some(PROGRAM_STATUS_MAX_AGE_MS - 1);
        assert!(resolve_status(&summary, false).is_some());
        summary.program_status_age_ms = Some(PROGRAM_STATUS_MAX_AGE_MS);
        assert!(resolve_status(&summary, false).is_none());
    }

    #[test]
    fn a_stale_or_unreachable_roster_suppresses_typed_status() {
        let mut summary = base("shell");
        summary.program_status = Some(ProgramStatus {
            state: ProgramState::Idle,
            app: None,
            kind: None,
            progress: None,
        });
        summary.program_status_age_ms = Some(0);
        assert!(resolve_status(&summary, false).is_some());
        assert!(resolve_status(&summary, true).is_none());
    }
}
