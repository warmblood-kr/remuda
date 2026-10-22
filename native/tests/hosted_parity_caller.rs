//! A client running inside a session is attributed to it, whichever backend
//! runs the session (#713). Same scenario on both; see `daemon_support/parity.rs`.
#![cfg(unix)]

use std::time::{Duration, Instant};

#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Parity, MODES, PATIENCE};

#[test]
fn a_client_inside_a_session_is_attributed_to_it_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        let result = daemon.root().join("caller.txt");
        let lua = format!(
            "remuda.extension_command('probe', function(_, caller) \
               local live = pcall(remuda._caller_live, caller.session, caller.instance_id); \
               local f = assert(io.open('{}', 'w')); \
               f:write(tostring(caller.session), ':', caller.kind, ':', tostring(caller.instance_id), ':', tostring(live)); \
               f:close() end); \
             return remuda._dispatch_extension_command('probe')",
            result.display()
        );
        let remuda = env!("CARGO_BIN_EXE_remuda");
        let instance = daemon.start_command(
            "probe",
            vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("\"{remuda}\" -s s -e \"{lua}\"; sleep 30"),
            ],
        );

        let deadline = Instant::now() + PATIENCE;
        let seen = loop {
            if let Ok(contents) = std::fs::read_to_string(&result) {
                if !contents.is_empty() {
                    break contents;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{mode:?}: the session's client never reported its caller:\n{}",
                daemon.screen("probe")
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(seen, format!("probe:session:{instance}:true"), "{mode:?}");

        // The test process itself is outside every session.
        assert_eq!(
            daemon.eval("return remuda.caller().kind"),
            Ok("outside".into()),
            "{mode:?}"
        );
    }
}
