use super::*;
use crate::enrichment_contract::summary;
use remuda_core::agent::{ProgramState, ProgramStatus};

#[test]
fn enrichment_contract_existing_typed_status_is_visible_without_butler_membership() {
    let mut row = summary("ordinary-shell", "S", None);
    row.program_status = Some(ProgramStatus {
        state: ProgramState::Working,
        app: None,
        kind: None,
        progress: None,
    });
    row.program_status_age_ms = Some(0);
    let ui = ClusterUi::new("owner", vec![row], Duration::ZERO);
    let rows = ui.local_tree_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "ordinary-shell");
    let canonical = session_cell(true, false, 0);
    let output = ui.tree_lines(240, Duration::ZERO).join("\n");
    assert!(output.contains(&canonical));
    assert!(
        output.contains("working"),
        "fresh authoritative non-Butler status absent: {output}"
    );
}
#[test]
fn enrichment_contract_unknown_remote_keeps_ordinary_base_row_without_noise() {
    let mut ui = ClusterUi::new("local", vec![], Duration::ZERO);
    ui.remote_snapshot.nodes.push(RemoteNodeSnapshot {
        registry_key: "node-B".into(),
        name: "peer".into(),
        state: RemoteState::Reachable,
        last_sync_age: Some(Duration::ZERO),
        last_error: None,
        sessions: vec![RemoteSessionSnapshot {
            name: "remote-worker".into(),
            wire_name: "wire-worker".into(),
            instance_id: "instance-B".into(),
            alive: true,
            output_version: None,
            bracketed_paste: false,
            screen: None,
            last_error: None,
        }],
    });
    ui.remote_expanded.insert("node-B".into());
    let rows = ui.remote_tree_rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].label, "remote-worker");
    assert_eq!(rows[1].status, session_cell(true, false, 0));
    let output = ui.tree_lines(240, Duration::ZERO).join("\n");
    assert!(
        !output.contains("unmanaged") && !output.contains("unavailable") && !output.contains("0%")
    );
    assert_eq!(
        ui.remote_snapshot.nodes[0].sessions[0].wire_name,
        "wire-worker"
    );
}
#[test]
fn enrichment_contract_base_tree_statuses_are_unchanged() {
    let mut alive = summary("live-shell", "S", None);
    let mut ended = summary("ended-shell", "E", None);
    alive.alive = true;
    ended.alive = false;
    let ui = ClusterUi::new("owner", vec![alive, ended], Duration::ZERO);
    let rows = ui.local_tree_rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].status, session_cell(true, false, 0));
    assert_eq!(rows[1].status, session_cell(false, false, 0));
}
