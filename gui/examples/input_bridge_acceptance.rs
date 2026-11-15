//! Isolated acceptance driver for the production parent supervisor and worker.
#[allow(dead_code)]
#[path = "../src/input_helper.rs"]
mod input_helper;
use input_helper::{Event, InputHelper, Submit};
use std::{path::PathBuf, time::Duration};

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    assert_eq!(args.len(), 3);
    let gui = PathBuf::from(std::env::var_os("REMUDA_GUI_ACCEPTANCE_BIN").unwrap());
    let [kind, peer, instance] = &args[..] else {
        unreachable!()
    };
    let helper = InputHelper::start(
        &gui,
        &["--internal-input-worker", kind, peer, "proof", instance],
    )
    .unwrap();
    for suffix in ["one", "two"] {
        let bytes = format!("bridge-{kind}-{suffix}\r");
        assert_eq!(helper.submit(bytes.as_bytes()), Submit::Queued);
        assert_eq!(
            helper.recv_event_timeout(Duration::from_secs(10)).unwrap(),
            Event::Applied
        );
    }
    helper.stop();
    let stale = InputHelper::start(
        &gui,
        &[
            "--internal-input-worker",
            kind,
            peer,
            "proof",
            "wrong-instance",
        ],
    )
    .unwrap();
    assert_eq!(stale.submit(b"bridge-wrong\r"), Submit::Queued);
    assert_eq!(
        stale.recv_event_timeout(Duration::from_secs(10)).unwrap(),
        Event::NotAcknowledged
    );
    assert_eq!(stale.submit(b"bridge-retry\r"), Submit::Stopped);
}
