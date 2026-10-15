use remuda_core::input::{InputBatch, InputOutcome};
use remuda_core::session::StartIdentity;
use remuda_core::session_operations::SessionHandle;
use remuda_core::{ManualClock, ScriptedAgent, Session};
use std::sync::Arc;

#[test]
fn preassigned_identity_survives_observation_snapshot_and_input_instance_check() {
    let identity = StartIdentity::new("reserved-session", "reserved-instance").unwrap();
    let session = Arc::new(Session::with_start_identity(
        "synthetic",
        identity,
        Box::new(ScriptedAgent::new(vec!["screen".into()])),
        Arc::new(ManualClock::new()),
    ));
    let handle = SessionHandle::local(session.clone());
    assert_eq!(session.id(), "reserved-session");
    assert_eq!(session.instance_id(), "reserved-instance");
    assert_eq!(
        handle.inspect().unwrap().identity.instance,
        "reserved-instance"
    );
    assert_eq!(
        handle.snapshot(0).unwrap().instance_id.as_deref(),
        Some("reserved-instance")
    );
    let mut batch = InputBatch {
        instance_id: "generated-or-replacement",
        client_id: [1; 16],
        seq: 1,
        bytes: b"original",
    };
    assert_eq!(
        session.apply_input_batch(batch).unwrap(),
        InputOutcome::WrongInstance
    );
    batch.instance_id = "reserved-instance";
    assert_eq!(
        session.apply_input_batch(batch).unwrap(),
        InputOutcome::Ack { duplicate: false }
    );
    assert_eq!(
        session.apply_input_batch(batch).unwrap(),
        InputOutcome::Ack { duplicate: true }
    );
}
#[test]
fn invalid_preassigned_identity_is_refused_before_an_agent_is_needed() {
    for (id, instance) in [
        ("", "instance"),
        ("id", ""),
        ("id\n", "instance"),
        ("id", "instance\0"),
    ] {
        assert!(StartIdentity::new(id, instance).is_err());
    }
    assert!(StartIdentity::new("id", "x".repeat(257)).is_err());
    assert!(StartIdentity::new("x".repeat(257), "instance").is_err());
    assert!(StartIdentity::new("x".repeat(256), "i".repeat(256)).is_ok());
}
#[test]
fn legacy_constructor_still_allocates_distinct_instances_for_same_caller_session_id() {
    let make = || {
        Session::new_with_id(
            "synthetic",
            "same-id",
            Box::new(ScriptedAgent::new(vec![])),
            Arc::new(ManualClock::new()),
        )
    };
    let first = make();
    let second = make();
    assert_eq!(first.id(), second.id());
    assert_ne!(first.instance_id(), second.instance_id());
}
