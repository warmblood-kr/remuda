use remuda_core::{AgentProcess, Size};
use remuda_native::{CommandBuilder, PtyAgent};
use std::time::{Duration, Instant};

#[test]
fn terminal_wrap_metadata_survives_the_pty_readout() {
    let mut command = CommandBuilder::new("printf");
    command.arg("x".repeat(81));
    let mut agent = PtyAgent::spawn(command, Size::new(80, 24)).expect("spawn pty");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !agent.screen_text().expect("read screen").contains('x') {
        assert!(Instant::now() < deadline, "pty never received output");
        std::thread::sleep(Duration::from_millis(10));
    }

    let wrapped = agent.row_wrapped_at(0).expect("read wrap markers");
    assert!(wrapped[0], "the 81st column continues the first row");
    assert!(!wrapped[1], "the final row has no following continuation");
}
