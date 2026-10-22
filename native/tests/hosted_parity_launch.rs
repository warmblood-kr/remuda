//! A session starts with the daemon's environment and the size it asked for,
//! whichever backend runs it (#713). Same scenario on both; see
//! `daemon_support/parity.rs`.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;

#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Parity, MODES};

#[test]
fn a_session_inherits_the_daemon_environment_in_both_backends() {
    // A real environment: one large value and more variables than the
    // first host launch allowed.
    let mut env = vec![
        ("PARITY_PLAIN".to_owned(), "hello".to_owned()),
        ("PARITY_BIG".to_owned(), "x".repeat(6000)),
    ];
    env.extend((0..100).map(|i| (format!("PARITY_N_{i}"), i.to_string())));
    for mode in MODES {
        let daemon = Parity::start_with_env(mode, &env);
        daemon.shell("sh");
        daemon
            .eval(
                "remuda.send('sh', [[echo \"ENV:$PARITY_PLAIN:${#PARITY_BIG}:$PARITY_N_99:${REMUDA_EXPERIMENTAL_SESSION_HOST:-unset}:\"]])",
            )
            .unwrap();
        daemon.wait_for("sh", "ENV:hello:6000:99:unset:");
    }
}

fn stty_size(daemon: &Parity, name: &str, marker: &str) -> String {
    daemon
        .eval(&format!(
            "remuda.send('{name}', [[echo R$((6*7)){marker}:$(stty size | tr ' ' x):]])"
        ))
        .unwrap();
    // `R42` appears only in the output, never in the echoed command.
    let prefix = format!("R42{marker}:");
    let screen = daemon.wait_for(name, &prefix);
    let line = screen
        .lines()
        .rev()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("{:?}: no {marker} line:\n{screen}", daemon.mode));
    line.trim_start_matches(&prefix)
        .trim_end()
        .trim_end_matches(':')
        .to_owned()
}

#[test]
fn an_explicitly_small_size_is_honoured_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        // A viewport size opts out of the 80x24 floor, at launch and resize.
        let started = daemon.request(&Request::New {
            name: Some("small".into()),
            command: vec!["/bin/sh".into()],
            size: Size::for_viewport(40, 10),
            cwd: Some("/".into()),
            env: None,
        });
        assert_eq!(started, Response::Value("small".into()), "{mode:?}");
        assert_eq!(stty_size(&daemon, "small", "AT"), "10x40", "{mode:?}");

        let resized = daemon.request(&Request::Resize {
            name: "small".into(),
            size: Size::for_viewport(30, 12),
        });
        assert_eq!(resized, Response::Ok, "{mode:?}");
        assert_eq!(stty_size(&daemon, "small", "THEN"), "12x30", "{mode:?}");

        // Without the opt-in, the floor applies in both.
        let floored = daemon.request(&Request::New {
            name: Some("floored".into()),
            command: vec!["/bin/sh".into()],
            size: Size::new(40, 10),
            cwd: Some("/".into()),
            env: None,
        });
        assert_eq!(floored, Response::Value("floored".into()), "{mode:?}");
        assert_eq!(stty_size(&daemon, "floored", "FLOOR"), "24x80", "{mode:?}");
    }
}

#[test]
fn a_hosted_launch_over_a_limit_is_refused_naming_the_limit() {
    let daemon = Parity::start(parity::Mode::Hosted);
    let refused = daemon.request(&Request::New {
        name: Some("n".repeat(65)),
        command: vec!["/bin/sh".into()],
        size: Size::new(80, 24),
        cwd: Some("/".into()),
        env: None,
    });
    match refused {
        Response::Error(message) => assert!(
            message.contains("host-backed launch refused: the session name is over 64 bytes"),
            "{message}"
        ),
        other => panic!("a 65-byte name was not refused: {other:?}"),
    }
}
