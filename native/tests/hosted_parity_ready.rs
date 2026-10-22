//! A session takes input as soon as `new` returns, whichever backend runs it
//! (#713). A host records itself registered just before it accepts input;
//! `new` waits for both, so the first request is not refused as "still
//! starting". Same scenario on both; see `daemon_support/parity.rs`.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};

#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Parity, MODES};

#[test]
fn input_right_after_new_is_accepted_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        for round in 0..10 {
            let name = format!("s{round}");
            daemon.shell(&name);
            assert_eq!(
                daemon.request(&Request::SendLine {
                    name: name.clone(),
                    text: format!("echo READY:$(({round}+1000))"),
                }),
                Response::Ok,
                "{mode:?}: round {round}"
            );
            daemon.wait_for(&name, &format!("READY:{}", round + 1000));
        }
    }
}
