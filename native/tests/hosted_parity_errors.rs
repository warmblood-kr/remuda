//! Refusals read the same whichever backend runs the session (#713). Same
//! scenario on both; see `daemon_support/parity.rs`.
#![cfg(unix)]

#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Parity, MODES};

#[test]
fn an_untrusted_clear_key_is_refused_in_the_same_words_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        daemon.shell("sh");
        for key in ["\\15", "\\27OM", "x", &"x".repeat(17)] {
            let refused = daemon
                .eval(&format!("return remuda.clear_input('sh', '{key}')"))
                .unwrap_err();
            assert!(refused.contains("clear key"), "{mode:?} {key:?}: {refused}");
        }
        // The allowlisted key is accepted.
        daemon
            .eval("return remuda.clear_input('sh', '\\21')")
            .unwrap();
    }
}
