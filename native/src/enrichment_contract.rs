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
