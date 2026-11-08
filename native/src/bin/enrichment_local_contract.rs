use super::*;

#[test]
fn enrichment_contract_local_cli_text_and_json_are_unchanged() {
    use remuda_core::{ManualClock, Registry, ScriptedAgent, Session};
    use std::sync::Arc;
    let registry = Registry::new();
    registry
        .register(Session::new(
            "local-shell",
            Box::new(ScriptedAgent::new(vec![])),
            Arc::new(ManualClock::new()),
        ))
        .unwrap();
    let row = registry.list().remove(0);
    let state = if row.unavailable {
        "unreachable"
    } else if row.alive {
        "live"
    } else {
        "dead"
    };
    let held = if row.attached { "attached" } else { "" };
    let expected = format!(
        "{:<20} {:>4}x{:<4} {:<5} idle {:<5} {held}",
        row.name,
        row.size.cols(),
        row.size.rows(),
        state,
        format!("{}s", row.idle.as_secs())
    );
    assert_eq!(session_line(&row), expected);
    assert_eq!(
        sessions_json(&[row.clone()]).unwrap(),
        serde_json::to_string_pretty(&vec![row]).unwrap()
    );
}
