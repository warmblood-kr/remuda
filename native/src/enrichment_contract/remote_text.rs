use super::*;
use remuda_core::agent::{ProgramState, ProgramStatus};
#[test]
fn enrichment_contract_remote_text_keeps_framing_and_adds_typed_activity() {
    let mut row = crate::enrichment_contract::summary("worker", "C", None);
    row.program_status = Some(ProgramStatus {
        state: ProgramState::Working,
        app: None,
        kind: None,
        progress: None,
    });
    row.program_status_age_ms = Some(0);
    let output = remote_sessions_text(&[row.clone()]);
    assert!(output.contains("worker") && output.contains("alive="));
    assert!(output.contains(row.instance_id.as_deref().unwrap()));
    assert!(!output.contains("member_id") && !output.contains("leader_id"));
    assert!(
        output.ends_with("\tactivity=working"),
        "cluster text must retain framing and typed activity: {output}"
    );
}
