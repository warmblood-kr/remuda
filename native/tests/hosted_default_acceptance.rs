//! Acceptance: an ordinary new session survives a daemon reload.
#![cfg(unix)]
#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Mode, Parity};

#[test]
fn default_session_keeps_process_and_output_across_reload() {
    let daemon = Parity::start(Mode::Default);
    struct StopReloaded<'a>(&'a Parity);
    impl Drop for StopReloaded<'_> {
        fn drop(&mut self) {
            let _ = self.0.cli(&["stop", "-f", "--yes"]);
        }
    }
    let _cleanup = StopReloaded(&daemon);
    daemon.shell("sh");
    daemon
        .eval("remuda.send('sh', [[echo BEFORE_DEFAULT:$((2+3))]])")
        .unwrap();
    daemon.wait_for("sh", "BEFORE_DEFAULT:5");
    let out = daemon.cli(&["reload"]);
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    daemon.wait_for("sh", "BEFORE_DEFAULT:5");
    daemon
        .eval("remuda.send('sh', [[echo AFTER_DEFAULT:$((3+4))]])")
        .unwrap();
    daemon.wait_for("sh", "AFTER_DEFAULT:7");
}
