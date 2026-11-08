//! Abstract cluster presentation boundary; no behavior implementation.
use crate::{agent::ProgramStatus, SessionSummary};

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

/// Explicit contract-only placeholder, not a feature implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnrichmentError {
    PendingImplementation,
    InvalidTarget,
}

/// Callable production seam for executable specifications. The programmer
/// replaces this unsupported result; no existing renderer is wired to it yet.
#[doc(hidden)]
pub fn resolve_cluster_roster(
    _roster: &NodeRoster<'_>,
) -> Result<Vec<ResolvedSession>, EnrichmentError> {
    Err(EnrichmentError::PendingImplementation)
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
    _advertised_versions: Option<&[String]>,
) -> Result<ClusterFactsRequestPlan, EnrichmentError> {
    Err(EnrichmentError::PendingImplementation)
}

/// Shared cluster-only formatter seam; canonical state has priority over
/// optional facts at narrow widths. Ordinary local text never calls this.
#[doc(hidden)]
pub fn format_cluster_row(
    _row: &ResolvedSession,
    _name: &str,
    _canonical_status: &str,
    _width: usize,
) -> Result<String, EnrichmentError> {
    Err(EnrichmentError::PendingImplementation)
}

#[cfg(test)]
#[path = "session_enrichment_contract.rs"]
mod enrichment_contract;
