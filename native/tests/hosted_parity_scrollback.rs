//! A session keeps the same scrollback whichever backend runs it (#713).
//! Same scenario on both; see `daemon_support/parity.rs`.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};

#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Parity, MODES};

#[test]
fn scrollback_reaches_thousands_of_rows_back_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        daemon.shell("sh");
        daemon
            .eval("remuda.send('sh', [[i=0; while [ $i -lt 3000 ]; do echo L$i; i=$((i+1)); done; echo DO''NE]])")
            .unwrap();
        daemon.wait_for("sh", "DONE");
        let Response::StyledScreen {
            rows,
            scrollback_len,
            ..
        } = daemon.request(&Request::CaptureStyled {
            name: "sh".into(),
            scrollback: 2500,
        })
        else {
            panic!("{mode:?}: styled capture refused");
        };
        assert!(scrollback_len >= 2900, "{mode:?}: only {scrollback_len} rows kept");
        let earliest = rows
            .iter()
            .filter_map(|runs| {
                let text: String = runs.iter().map(|run| run.text.as_str()).collect();
                text.trim().strip_prefix('L')?.parse::<u32>().ok()
            })
            .min()
            .unwrap_or_else(|| panic!("{mode:?}: no numbered rows"));
        assert!(earliest < 1000, "{mode:?}: reached only L{earliest}");
    }
}
