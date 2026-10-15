//! Experimental diagnostic reducer. No launcher, transport, approval or input
//! integration. Inputs must come from the already-owned server, never disk logs.
//! Protocol adapter: installed codex-cli 0.162.1 generated app-server schema.
//! This reducer authenticates no transport or file. Future callers must supply
//! proven launch identity and selected-thread evidence through a trusted path.
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub principal: String,
    pub instance: String,
    pub server_generation: String,
}

/// Trusted in-process evidence from the owning TUI selection contract. A loaded
/// list, cwd, socket path, PID or session name must not be used as this evidence.
/// There is deliberately no external RPC/file deserializer for this type.
pub struct SelectionEvidence {
    pub identity: Identity,
    pub thread: String,
    pub connection_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadState {
    Unknown,
    Idle,
    Active { flags: Vec<String> },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TurnObservation {
    pub id: String,
    pub state: String,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ReviewKey {
    pub server_generation: String,
    pub thread: String,
    pub turn: String,
    pub review: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct ReviewObservation {
    pub key: ReviewKey,
    pub state: String,
    pub target_item: Option<String>,
    pub receive_sequence: u64,
    pub connection_generation: u64,
}
#[derive(Debug, Serialize)]
pub struct Diagnostic {
    pub schema_version: u8,
    pub identity: Identity,
    pub provider_version: String,
    pub connection_generation: u64,
    pub receive_sequence: u64,
    pub connected: bool,
    pub primary_thread: Option<String>,
    pub thread: ThreadState,
    pub turn: Option<TurnObservation>,
    /// Historical receipts, never a claim that a review is currently pending.
    pub review_receipts: Vec<ReviewObservation>,
    pub schema_known_auto_review: bool,
    pub experimental_requested: bool,
    pub observed_methods: Vec<String>,
    pub unsupported_methods: Vec<String>,
    pub transport_age_ms: Option<u64>,
    pub status_age_ms: Option<u64>,
    pub turn_age_ms: Option<u64>,
    pub reason: &'static str,
}

#[derive(Clone)]
enum Request {
    Initialize,
    Loaded,
    Read { thread: String, revision: u64 },
}

pub struct Observer {
    identity: Identity,
    version: String,
    generation: u64,
    sequence: u64,
    revision: u64,
    next_id: u64,
    connected: bool,
    selected: Option<String>,
    loaded: BTreeSet<String>,
    pending: BTreeMap<u64, Request>,
    state: ThreadState,
    turn: Option<TurnObservation>,
    turn_at: Option<u64>,
    reviews: BTreeMap<ReviewKey, ReviewObservation>,
    observed: BTreeSet<String>,
    unsupported: BTreeSet<String>,
    liveness: Option<u64>,
    reconciled: Option<u64>,
    reason: &'static str,
}

impl Observer {
    pub fn new(identity: Identity, version: String) -> Result<Self, &'static str> {
        if [
            &identity.principal,
            &identity.instance,
            &identity.server_generation,
        ]
        .iter()
        .any(|value| value.is_empty())
        {
            return Err("identity_required");
        }
        Ok(Self {
            identity,
            version,
            generation: 0,
            sequence: 0,
            revision: 0,
            next_id: 0,
            connected: false,
            selected: None,
            loaded: BTreeSet::new(),
            pending: BTreeMap::new(),
            state: ThreadState::Unknown,
            turn: None,
            turn_at: None,
            reviews: BTreeMap::new(),
            observed: BTreeSet::new(),
            unsupported: BTreeSet::new(),
            liveness: None,
            reconciled: None,
            reason: "not_connected",
        })
    }
    fn invalidate(&mut self, reason: &'static str) {
        self.state = ThreadState::Unknown;
        self.reconciled = None;
        self.reason = reason;
    }
    fn request(&mut self, request: Request, method: &str, params: Value) -> Value {
        self.next_id += 1;
        self.pending.insert(self.next_id, request);
        json!({"id":self.next_id,"method":method,"params":params})
    }
    /// Starts a new logical connection only. This never opens a socket.
    /// Caller must tag every received frame with this generation.
    pub fn connect(&mut self, now_ms: u64) -> (u64, Value) {
        self.generation += 1;
        self.connected = true;
        self.liveness = Some(now_ms);
        self.pending.clear();
        self.loaded.clear();
        self.turn = None;
        self.turn_at = None;
        self.selected = None;
        self.invalidate("awaiting_reconciliation");
        let request = self.request(
            Request::Initialize,
            "initialize",
            json!({
                "clientInfo":{"name":"remuda-observer-fixture","version":"0.1"},
                "capabilities":{"experimentalApi":false}
            }),
        );
        (self.generation, request)
    }
    pub fn disconnect(&mut self) {
        self.connected = false;
        self.pending.clear();
        self.loaded.clear();
        self.turn = None;
        self.turn_at = None;
        self.selected = None;
        self.invalidate("disconnected");
    }
    /// A ping/connection read is not authoritative thread reconciliation.
    pub fn heartbeat(&mut self, generation: u64, now_ms: u64) {
        if self.connected && generation == self.generation {
            self.liveness = Some(now_ms);
        }
    }
    pub fn select(&mut self, evidence: SelectionEvidence) -> Vec<Value> {
        self.pending
            .retain(|_, req| !matches!(req, Request::Read { .. }));
        self.turn = None;
        self.turn_at = None;
        self.invalidate("selection_unproven");
        self.selected = if self.connected
            && evidence.connection_generation == self.generation
            && evidence.identity == self.identity
            && !evidence.thread.is_empty()
        {
            Some(evidence.thread)
        } else {
            None
        };
        self.reconcile()
    }
    /// Read metadata only. No resume, subscription, approval routing or input.
    pub fn reconcile(&mut self) -> Vec<Value> {
        let Some(thread) = self.selected.clone() else {
            return vec![];
        };
        if !self.connected
            || !self.loaded.contains(&thread)
            || self
                .pending
                .values()
                .any(|r| matches!(r, Request::Read { .. }))
            || self.unsupported.contains("thread/read")
        {
            return vec![];
        }
        vec![self.request(
            Request::Read {
                thread: thread.clone(),
                revision: self.revision,
            },
            "thread/read",
            json!({"threadId":thread,"includeTurns":false}),
        )]
    }
    pub fn receive(&mut self, generation: u64, frame: &Value, now_ms: u64) -> Vec<Value> {
        if !self.connected || generation != self.generation {
            return vec![];
        }
        self.sequence += 1;
        self.liveness = Some(now_ms);
        if let Some(id) = frame.get("id").and_then(Value::as_u64) {
            return self.response(id, frame, now_ms);
        }
        self.notification(frame, now_ms)
    }
    fn response(&mut self, id: u64, frame: &Value, now_ms: u64) -> Vec<Value> {
        let Some(request) = self.pending.remove(&id) else {
            return vec![];
        };
        if frame.get("error").is_some() {
            if frame.pointer("/error/code").and_then(Value::as_i64) == Some(-32601) {
                self.unsupported.insert(
                    match request {
                        Request::Initialize => "initialize",
                        Request::Loaded => "thread/loaded/list",
                        Request::Read { .. } => "thread/read",
                    }
                    .into(),
                );
            }
            self.invalidate("request_failed");
            return vec![];
        }
        let Some(result) = frame.get("result") else {
            self.invalidate("malformed_response");
            return vec![];
        };
        match request {
            Request::Initialize => vec![
                json!({"method":"initialized","params":{}}),
                self.request(Request::Loaded, "thread/loaded/list", json!({})),
            ],
            Request::Loaded => {
                let Some(data) = result.get("data").and_then(Value::as_array) else {
                    self.invalidate("malformed_loaded_list");
                    return vec![];
                };
                if data.iter().any(|v| v.as_str().is_none()) {
                    self.invalidate("malformed_loaded_list");
                    return vec![];
                }
                self.loaded
                    .extend(data.iter().filter_map(Value::as_str).map(str::to_owned));
                if let Some(cursor) = result.get("nextCursor").and_then(Value::as_str) {
                    vec![self.request(
                        Request::Loaded,
                        "thread/loaded/list",
                        json!({"cursor":cursor}),
                    )]
                } else {
                    self.reconcile()
                }
            }
            Request::Read { thread, revision } => {
                if self.selected.as_deref() != Some(&thread) || revision != self.revision {
                    self.invalidate("snapshot_race");
                    return self.reconcile();
                }
                if result.pointer("/thread/id").and_then(Value::as_str) != Some(&thread) {
                    self.invalidate("thread_mismatch");
                    return vec![];
                }
                match result.pointer("/thread/status").and_then(parse_status) {
                    Some(state) => {
                        if state == ThreadState::Idle {
                            self.turn = None;
                            self.turn_at = None;
                        }
                        self.state = state;
                        self.reconciled = Some(now_ms);
                        self.reason = "reconciled";
                    }
                    None => self.invalidate("unknown_thread_status"),
                }
                vec![]
            }
        }
    }
    fn notification(&mut self, frame: &Value, now_ms: u64) -> Vec<Value> {
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            return vec![];
        };
        let recognized = matches!(
            method,
            "thread/status/changed"
                | "turn/started"
                | "turn/completed"
                | "item/autoApprovalReview/started"
                | "item/autoApprovalReview/completed"
                | "autoApprovalReview/strictReviewRequired"
                | "guardianWarning"
        );
        if !recognized {
            return vec![];
        }
        self.observed.insert(method.into());
        let p = &frame["params"];
        if self.selected.as_deref() != p.get("threadId").and_then(Value::as_str)
            || self.selected.is_none()
        {
            return vec![];
        }
        match method {
            "thread/status/changed" => {
                self.revision += 1;
                if self.reconciled.is_some() {
                    match p.get("status").and_then(parse_status) {
                        Some(state) => {
                            if state == ThreadState::Idle {
                                self.turn = None;
                                self.turn_at = None;
                            }
                            self.state = state;
                            self.reconciled = Some(now_ms);
                        }
                        None => self.invalidate("unknown_thread_status"),
                    }
                }
            }
            "turn/started" | "turn/completed" => return self.turn_event(method, p, now_ms),
            "item/autoApprovalReview/started" | "item/autoApprovalReview/completed" => {
                self.review(p);
            }
            _ => {} // strict review/warning receipts only; no text or approval assumptions.
        }
        vec![]
    }
    fn turn_event(&mut self, method: &str, p: &Value, now_ms: u64) -> Vec<Value> {
        let Some(id) = p.pointer("/turn/id").and_then(Value::as_str) else {
            return vec![];
        };
        // An older completed turn cannot invalidate the newer selected turn.
        if method == "turn/completed" && self.turn.as_ref().is_some_and(|t| t.id != id) {
            return vec![];
        }
        self.revision += 1;
        self.invalidate("turn_transition");
        self.turn = Some(TurnObservation {
            id: id.into(),
            state: if method == "turn/started" {
                "active"
            } else {
                "completed"
            }
            .into(),
        });
        self.turn_at = Some(now_ms);
        self.reconcile()
    }
    fn review(&mut self, p: &Value) {
        if self.version != "0.162.1" {
            return;
        }
        let (Some(thread), Some(turn), Some(review), Some(state)) = (
            p["threadId"].as_str(),
            p["turnId"].as_str(),
            p["reviewId"].as_str(),
            p["review"]["status"].as_str(),
        ) else {
            return;
        };
        if !matches!(
            state,
            "inProgress" | "approved" | "denied" | "timedOut" | "aborted"
        ) {
            return;
        }
        let key = ReviewKey {
            server_generation: self.identity.server_generation.clone(),
            thread: thread.into(),
            turn: turn.into(),
            review: review.into(),
        };
        if self
            .reviews
            .get(&key)
            .is_some_and(|r| r.state != "inProgress")
        {
            return;
        }
        self.reviews.insert(
            key.clone(),
            ReviewObservation {
                key,
                state: state.into(),
                target_item: p["targetItemId"].as_str().map(str::to_owned),
                receive_sequence: self.sequence,
                connection_generation: self.generation,
            },
        );
    }
    /// Caller clock is one producer epoch. Readers still need authenticated
    /// receipt expiry; this snapshot supplies no authorization or admission bit.
    pub fn diagnostic(&self, now_ms: u64) -> Diagnostic {
        let transport_age = self.liveness.and_then(|at| now_ms.checked_sub(at));
        let status_age = self.reconciled.and_then(|at| now_ms.checked_sub(at));
        let turn_age = self.turn_at.and_then(|at| now_ms.checked_sub(at));
        let turn_fresh = self.connected
            && transport_age.is_some_and(|age| age <= 30_000)
            && turn_age.is_some_and(|age| age <= 30_000);
        let fresh = self.connected
            && transport_age.is_some_and(|age| age <= 30_000)
            && status_age.is_some_and(|age| age <= 30_000);
        Diagnostic {
            schema_version: 1,
            identity: self.identity.clone(),
            provider_version: self.version.clone(),
            connection_generation: self.generation,
            receive_sequence: self.sequence,
            connected: self.connected,
            primary_thread: self.selected.clone(),
            thread: if fresh {
                self.state.clone()
            } else {
                ThreadState::Unknown
            },
            turn: if turn_fresh { self.turn.clone() } else { None },
            review_receipts: self.reviews.values().cloned().collect(),
            schema_known_auto_review: self.version == "0.162.1",
            experimental_requested: false,
            observed_methods: self.observed.iter().cloned().collect(),
            unsupported_methods: self.unsupported.iter().cloned().collect(),
            transport_age_ms: transport_age,
            status_age_ms: status_age,
            turn_age_ms: turn_age,
            reason: if self.reconciled.is_some() && !fresh {
                "stale_or_clock_changed"
            } else {
                self.reason
            },
        }
    }
}
fn parse_status(status: &Value) -> Option<ThreadState> {
    match status["type"].as_str()? {
        "idle" => Some(ThreadState::Idle),
        "active" => {
            let flags = status["activeFlags"].as_array()?;
            let flags = flags
                .iter()
                .map(|v| match v.as_str() {
                    Some("waitingOnApproval") => Some("waitingOnApproval".into()),
                    Some("waitingOnUserInput") => Some("waitingOnUserInput".into()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            Some(ThreadState::Active { flags })
        }
        _ => None,
    }
}
