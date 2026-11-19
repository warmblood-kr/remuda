//! Known, documented differences fail loudly (#713): a hosted input act is
//! capped at 8 KiB so an older build can still read the host inventory.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use remuda_native::script;

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

/// The type_text path reaches the same act cap through Lua rather than
/// `SendLine`; both must refuse by name and leave the session usable.
#[test]
fn hosted_type_text_over_the_act_limit_is_refused_and_small_input_lands() {
    let daemon = Parity::start(Mode::Hosted);
    daemon.shell("sh");
    let refused = script::eval_source(
        &daemon.socket(),
        "hosted-type-text-limit",
        "local ok, err = pcall(remuda.type_text, 'sh', string.rep('x', 9000))\n\
         assert(not ok, 'oversized type_text was not refused')\n\
         assert(tostring(err):find('8 KiB', 1, true), tostring(err))",
    );
    assert!(
        refused.is_ok(),
        "oversized type_text was not refused: {refused:?}"
    );
    assert_eq!(
        daemon.request(&Request::SendLine {
            name: "sh".into(),
            text: "echo SMALL_TT:$((5*5))".into(),
        }),
        Response::Ok
    );
    daemon.wait_for("sh", "SMALL_TT:25");
}
