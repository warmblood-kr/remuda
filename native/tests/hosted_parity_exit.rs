//! A session's end is announced once and the session leaves the list,
//! whichever backend runs it (#713). Same scenario on both; see
//! `daemon_support/parity.rs`.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use std::time::{Duration, Instant};

#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Parity, MODES, PATIENCE};

fn events(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

fn listed(daemon: &Parity, name: &str) -> bool {
    match daemon.request(&Request::List) {
        Response::Sessions(sessions) => sessions.iter().any(|session| session.name == name),
        other => panic!("{:?}: ls: {other:?}", daemon.mode),
    }
}

#[test]
fn an_exit_and_a_close_are_each_announced_once_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        let log = daemon.root().join("exited.log");
        daemon
            .eval(&format!(
                "remuda.on('session_exited', function(name, d) \
                   local f = assert(io.open('{}', 'a')); \
                   f:write(name, ':', d.reason, ':', tostring(d.exit_code), ':', tostring(d.instance_id), '\\n'); \
                   f:close() end, {{ group = 'parity' }})",
                log.display()
            ))
            .unwrap();

        // The child exits by itself: announced as exited, with its status.
        let instance = daemon.shell("quits");
        daemon.eval("remuda.send('quits', 'exit 7')").unwrap();
        let deadline = Instant::now() + PATIENCE;
        while listed(&daemon, "quits") {
            assert!(
                Instant::now() < deadline,
                "{mode:?}: an exited session stayed listed"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let expected = format!("quits:exited:7:{instance}");
        let deadline = Instant::now() + PATIENCE;
        while !events(&log).contains(&expected) {
            assert!(
                Instant::now() < deadline,
                "{mode:?}: no exit event: {:?}",
                events(&log)
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        // Closed by request: announced as closed.
        daemon.shell("closed");
        assert_eq!(
            daemon.request(&Request::Close {
                name: "closed".into(),
                instance_id: None,
                confirm: None,
            }),
            Response::Ok,
            "{mode:?}"
        );
        let deadline = Instant::now() + PATIENCE;
        while !events(&log)
            .iter()
            .any(|line| line.starts_with("closed:closed:"))
        {
            assert!(
                Instant::now() < deadline,
                "{mode:?}: no close event: {:?}",
                events(&log)
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        // Let a second ticker pass show any duplicate.
        std::thread::sleep(Duration::from_millis(1500));
        let events = events(&log);
        assert_eq!(
            events
                .iter()
                .filter(|line| line.starts_with("quits:"))
                .count(),
            1,
            "{mode:?}: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|line| line.starts_with("closed:"))
                .count(),
            1,
            "{mode:?}: {events:?}"
        );
        assert!(!listed(&daemon, "closed"), "{mode:?}");
    }
}
