//! Fixtures for tests against actual main behavior, not a resolver mock.
use remuda_core::{ManualClock, Registry, ScriptedAgent, Session, SessionSummary};
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) fn wire(name: &str, member: &str, parent: Option<&str>) -> Value {
    let registry = Registry::new();
    registry
        .register(Session::new(
            name,
            Box::new(ScriptedAgent::new(vec![])),
            Arc::new(ManualClock::new()),
        ))
        .unwrap();
    let mut wire = serde_json::to_value(registry.list().remove(0)).unwrap();
    let mut display = json!({"version":1,"provider":"butler","member_id":member,
        "kind":"codex","model":"configured-model","context_used":42000,
        "context_window":100000,"context_percent":42,"valid_for_ms":10000});
    if let Some(parent) = parent {
        display["leader_id"] = json!(parent);
    }
    wire["display"] = display;
    wire
}

pub(crate) fn summary(name: &str, member: &str, parent: Option<&str>) -> SessionSummary {
    serde_json::from_value(wire(name, member, parent)).unwrap()
}

#[test]
fn enrichment_contract_local_summary_json_stays_unchanged() {
    let mut wire = wire("worker", "C", Some("L"));
    wire["future_node_hint"] = json!("bounded extension");
    let mut expected = wire.clone();
    expected.as_object_mut().unwrap().remove("display");
    expected.as_object_mut().unwrap().remove("future_node_hint");
    let row: SessionSummary = serde_json::from_value(wire.clone()).unwrap();
    let output = serde_json::to_value(row).unwrap();
    assert_eq!(output["name"], wire["name"]);
    assert_eq!(output["instance_id"], wire["instance_id"]);
    assert_eq!(output["alive"], wire["alive"]);
    assert_eq!(
        output, expected,
        "local JSON must not acquire cluster-only join metadata"
    );
}

#[test]
fn enrichment_contract_existing_typed_program_status_survives() {
    let mut wire = wire("ordinary-shell", "S", None);
    wire.as_object_mut().unwrap().remove("display");
    wire["program_status"] = json!({"state":"blocked","kind":"permission"});
    wire["program_status_age_ms"] = json!(1000);
    let row: SessionSummary = serde_json::from_value(wire.clone()).unwrap();
    let output = serde_json::to_value(row).unwrap();
    assert_eq!(output["program_status"], wire["program_status"]);
    assert_eq!(
        output["program_status_age_ms"],
        wire["program_status_age_ms"]
    );
    assert_eq!(output["instance_id"], wire["instance_id"]);
}

/// Non-tautological local-JSON control: real owner facts are published, and
/// the ordinary SessionSummary serialization still has no join or display key.
#[test]
fn published_cluster_facts_never_enter_local_session_json() {
    use remuda_core::session_enrichment::{ButlerFacts, FieldState};
    let registry = std::sync::Arc::new(Registry::with_cluster_facts_clock(std::sync::Arc::new(
        ManualClock::new(),
    )));
    let session = registry
        .register(Session::new(
            "worker",
            Box::new(ScriptedAgent::new(vec![])),
            Arc::new(ManualClock::new()),
        ))
        .unwrap();
    let instance = session.instance_id().to_string();
    let generation = registry.begin_cluster_facts_generation();
    registry
        .set_cluster_facts_for(
            generation,
            "worker",
            &instance,
            Some(ButlerFacts {
                member_id: "private-MID".into(),
                leader_id: Some("private-LEAD".into()),
                alias: Some("worker".into()),
                kind: FieldState::Value("codex".into()),
                model: FieldState::Unavailable,
                context_used: FieldState::Value(42),
                context_window: FieldState::Value(100),
                context_percent: FieldState::Value(42),
            }),
        )
        .unwrap();
    let local = serde_json::to_value(registry.list().remove(0)).unwrap();
    assert!(
        local.get("display").is_none()
            && local.get("member_id").is_none()
            && local.get("leader_id").is_none()
            && local.get("facts").is_none(),
        "local JSON acquired cluster-only facts: {local}"
    );
    let sidecar = registry.cluster_facts_now();
    assert_eq!(sidecar.len(), 1);
    assert_eq!(sidecar[0].3.member_id, "private-MID");
}

/// The sidecar encoding is explicit: absent optional fields are omitted, so an
/// older peer never sees a field it cannot parse, and no join ID leaks into
/// the base List row.
#[test]
fn session_facts_wire_encoding_is_additive_and_omits_absent_fields() {
    use remuda_core::protocol::{ClusterSessionFactWire, Request, Response};
    let request = serde_json::to_value(Request::ClusterSessionFacts).unwrap();
    assert_eq!(request, json!("ClusterSessionFacts"));
    let response = Response::ClusterSessionFacts(vec![ClusterSessionFactWire {
        wire_name: "worker".into(),
        instance_id: "instance-1".into(),
        valid_for_ms: 9_000,
        member_id: "private-MID".into(),
        leader_id: Some("private-LEAD".into()),
        alias: None,
        kind: Some("codex".into()),
        model: None,
        context_used: Some(42_000),
        context_window: Some(100_000),
        context_percent: Some(42),
    }]);
    let value = serde_json::to_value(&response).unwrap();
    let entry = &value["ClusterSessionFacts"][0];
    assert_eq!(entry["wire_name"], "worker");
    assert_eq!(entry["instance_id"], "instance-1");
    assert_eq!(entry["member_id"], "private-MID");
    assert_eq!(entry["context_percent"], 42);
    assert!(entry.get("alias").is_none());
    assert!(entry.get("model").is_none());
    // Old readers only need the enum tag; an unknown variant is never sent
    // without a positive advertisement, which is a transport policy test.
    let old: serde_json::Value = serde_json::from_str(r#""Sessions""#).unwrap();
    assert_eq!(old, json!("Sessions"));
}
