//! Known, documented differences fail loudly (#713): a hosted input act is
//! capped at 8 KiB so an older build can still read the host inventory.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};

#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Mode, Parity};

#[test]
fn hosted_input_over_the_act_limit_is_refused_by_name_and_smaller_input_lands() {
    let daemon = Parity::start(Mode::Hosted);
    daemon.shell("sh");
    let refused = daemon.request(&Request::SendLine {
        name: "sh".into(),
        text: format!("echo {}", "x".repeat(9000)),
    });
    match refused {
        Response::Error(message) => assert!(
            message.contains("hosted input acts are limited to 8 KiB; split the message"),
            "{message}"
        ),
        other => panic!("oversized input was not refused: {other:?}"),
    }
    // Nothing was reserved: the next input goes through.
    assert_eq!(
        daemon.request(&Request::SendLine {
            name: "sh".into(),
            text: "echo SMALL:$((4*4))".into(),
        }),
        Response::Ok
    );
    daemon.wait_for("sh", "SMALL:16");
}
