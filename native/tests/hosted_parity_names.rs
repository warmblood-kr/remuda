//! A session name is refused while in use and free again as soon as its
//! session is closed, whichever backend runs it (#713). Same scenario on
//! both; see `daemon_support/parity.rs`.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};

#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Parity, MODES};

fn close(daemon: &Parity, name: &str) -> Response {
    daemon.request(&Request::Close {
        name: name.into(),
        instance_id: None,
        confirm: None,
    })
}

#[test]
fn a_name_is_taken_while_used_and_free_right_after_close_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        let first = daemon.shell("reused");

        match daemon.request(&Request::New {
            name: Some("reused".into()),
            command: vec!["/bin/sh".into()],
            size: remuda_core::Size::new(80, 24),
            cwd: None,
            env: None,
        }) {
            Response::Error(reason) => assert!(reason.contains("name taken"), "{mode:?}: {reason}"),
            other => panic!("{mode:?}: a taken name was accepted: {other:?}"),
        }

        // Close and reuse at once, several times over.
        let mut previous = first;
        for round in 0..3 {
            assert_eq!(
                close(&daemon, "reused"),
                Response::Ok,
                "{mode:?} round {round}"
            );
            let next = daemon.shell("reused");
            assert_ne!(next, previous, "{mode:?}: a new launch has a new instance");
            daemon
                .eval(&format!(
                    "remuda.send('reused', [[echo ROUND:$(({round}+100))]])"
                ))
                .unwrap();
            daemon.wait_for("reused", &format!("ROUND:{}", round + 100));
            previous = next;
        }
    }
}
