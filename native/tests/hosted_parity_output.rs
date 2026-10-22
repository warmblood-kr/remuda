//! New output reaches the `session_output` hook, and so wakes `remuda.expect`
//! well before its polling interval, whichever backend runs the session
//! (#713). Same scenario on both; see `daemon_support/parity.rs`.
#![cfg(unix)]

use std::time::{Duration, Instant};

#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Parity, MODES, PATIENCE};

fn lines(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

fn wait_until(mode: parity::Mode, what: &str, within: Duration, done: impl Fn() -> bool) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < deadline, "{mode:?}: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn output_wakes_hooks_and_expect_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        let log = daemon.root().join("events.log");
        let matched = daemon.root().join("matched");
        daemon
            .eval(&format!(
                "remuda.on('session_output', function(name, d) \
                   local f = assert(io.open('{log}', 'a')); f:write('output:', name, ':', d.version, '\\n'); f:close() end, {{ group = 'parity' }}); \
                 remuda.on('session_exited', function(name) \
                   local f = assert(io.open('{log}', 'a')); f:write('exited:', name, '\\n'); f:close() end, {{ group = 'parity' }})",
                log = log.display()
            ))
            .unwrap();
        daemon.shell("sh");
        // A 20 s polling interval: only an output wake can match within a few seconds.
        daemon
            .eval(&format!(
                "remuda.expect('sh', {{ {{ match = 'MATCH:42', action = function() \
                   local f = assert(io.open('{}', 'w')); f:write('yes'); f:close() end }} }}, \
                   {{ timeout = 25, interval = 20 }})",
                matched.display()
            ))
            .unwrap();
        daemon
            .eval("remuda.send('sh', [[echo MATCH:$((6*7))]])")
            .unwrap();
        wait_until(
            mode,
            "expect never woke on output",
            Duration::from_secs(5),
            || matched.exists(),
        );

        let outputs = |log: &[String]| {
            log.iter()
                .filter(|line| line.starts_with("output:sh:"))
                .map(|line| line["output:sh:".len()..].parse::<u64>().unwrap())
                .collect::<Vec<_>>()
        };
        let seen = outputs(&lines(&log));
        assert!(!seen.is_empty(), "{mode:?}: no session_output");
        assert!(
            seen.windows(2).all(|pair| pair[0] < pair[1]),
            "{mode:?}: {seen:?}"
        );

        // The exit is announced after the last output.
        daemon.eval("remuda.send('sh', 'exit')").unwrap();
        wait_until(mode, "no exit event", PATIENCE, || {
            lines(&log).iter().any(|line| line == "exited:sh")
        });
        std::thread::sleep(Duration::from_millis(500));
        let log = lines(&log);
        assert_eq!(
            log.last().map(String::as_str),
            Some("exited:sh"),
            "{mode:?}: {log:?}"
        );
    }
}
