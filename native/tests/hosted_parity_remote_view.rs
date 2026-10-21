//! What a cluster remote view uses, Sync for frames and sequenced Input for
//! keys, behaves the same whichever backend runs the session (#713). Same
//! scenario on both; see `daemon_support/parity.rs`.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};

#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Parity, MODES};

const CLIENT: &str = "00112233445566778899aabbccddeeff";

fn sync(daemon: &Parity, instance: Option<&str>, since: u64, timeout_ms: u64) -> Response {
    daemon.request(&Request::Sync {
        name: "sh".into(),
        instance_id: instance.map(String::from),
        since,
        timeout_ms,
    })
}

fn frame_text(response: &Response) -> (u64, String) {
    match response {
        Response::Sync {
            output_version,
            snapshot,
            ..
        } => (
            *output_version,
            snapshot
                .rows
                .iter()
                .map(|runs| runs.iter().map(|run| run.text.as_str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        other => panic!("not a frame: {other:?}"),
    }
}

fn input(daemon: &Parity, instance: &str, seq: u64, bytes: &[u8], line: bool) -> Response {
    let (name, instance_id, client_id, bytes) =
        ("sh".into(), instance.into(), CLIENT.into(), bytes.to_vec());
    daemon.request(&if line {
        Request::InputLine {
            name,
            instance_id,
            client_id,
            seq,
            bytes,
        }
    } else {
        Request::Input {
            name,
            instance_id,
            client_id,
            seq,
            bytes,
        }
    })
}

#[test]
fn sync_follows_output_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        let instance = daemon.shell("sh");
        daemon.wait_for("sh", "$");
        let (first, _) = frame_text(&sync(&daemon, Some(&instance), 0, 2000));
        assert!(first > 0, "{mode:?}");

        // Nothing new: the timeout returns the current frame unchanged.
        let (same, _) = frame_text(&sync(&daemon, Some(&instance), first, 200));
        assert_eq!(same, first, "{mode:?}");

        daemon
            .eval("remuda.send('sh', [[echo SYNC:$((3*3))]])")
            .unwrap();
        let deadline = std::time::Instant::now() + parity::PATIENCE;
        let mut since = first;
        loop {
            let (version, text) = frame_text(&sync(&daemon, Some(&instance), since, 2000));
            if text.contains("SYNC:9") {
                assert!(version > first, "{mode:?}");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "{mode:?}: {text}");
            since = version;
        }

        assert_eq!(
            sync(&daemon, Some("not-this-launch"), 0, 0),
            Response::WrongInstance,
            "{mode:?}"
        );
    }
}

#[test]
fn sequenced_input_is_delivered_once_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        let instance = daemon.shell("sh");
        daemon.wait_for("sh", "$");

        let keys = b"echo IN:$((1+1))\r";
        assert_eq!(
            input(&daemon, &instance, 1, keys, false),
            Response::Ack { duplicate: false },
            "{mode:?}"
        );
        daemon.wait_for("sh", "IN:2");
        // A retry of the same batch is remembered, not replayed.
        assert_eq!(
            input(&daemon, &instance, 1, keys, false),
            Response::Ack { duplicate: true },
            "{mode:?}"
        );
        assert_eq!(
            input(&daemon, &instance, 2, b"echo LINE:$((2+2))", true),
            Response::Ack { duplicate: false },
            "{mode:?}"
        );
        daemon.wait_for("sh", "LINE:4");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let screen = daemon.screen("sh");
        assert_eq!(screen.matches("IN:2").count(), 1, "{mode:?}: {screen}");

        assert_eq!(
            input(&daemon, "not-this-launch", 3, b"x", false),
            Response::WrongInstance,
            "{mode:?}"
        );
        assert_eq!(
            input(&daemon, &instance, 3, b"exit\r", false),
            Response::Ack { duplicate: false },
            "{mode:?}"
        );
        let deadline = std::time::Instant::now() + parity::PATIENCE;
        loop {
            match input(&daemon, &instance, 4, b"x", false) {
                Response::Error(error)
                    if error.contains("session exited") || error.contains("no such session") =>
                {
                    break
                }
                other => {
                    assert!(std::time::Instant::now() < deadline, "{mode:?}: {other:?}");
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
    }
}
