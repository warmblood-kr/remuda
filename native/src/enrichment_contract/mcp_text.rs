use super::*;
use remuda_core::agent::{ProgramState, ProgramStatus};
#[test]
fn enrichment_contract_remote_mcp_keeps_shared_cluster_text() {
    let mut row = crate::enrichment_contract::summary("worker", "C", None);
    row.program_status = Some(ProgramStatus {
        state: ProgramState::Working,
        app: None,
        kind: None,
        progress: None,
    });
    row.program_status_age_ms = Some(0);
    // explicit_node_reply's remote ls arm uses this formatter + tool_text;
    // the dispatch itself is source-verified, not a network integration probe.
    let remote = crate::cluster_api::enrichment_contract_remote_text(&[row]);
    let result = tool_text(&remote);
    let output = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(output, remote);
    assert!(
        output.ends_with("\tactivity=working"),
        "remote MCP must retain shared cluster text: {output}"
    );
}
#[test]
fn enrichment_contract_local_mcp_format_stays_unchanged() {
    let row = crate::enrichment_contract::summary("worker", "C", None);
    let expected = format!(
        "worker\t{}x{}\talive={}\tidle={:.0}s\toutput_idle={:.0}s",
        row.size.cols(),
        row.size.rows(),
        row.alive,
        row.idle.as_secs_f64(),
        row.output_idle.unwrap_or(row.idle).as_secs_f64()
    );
    let output = sessions_text(vec![row]);
    assert_eq!(output["content"][0]["text"], expected);
}
