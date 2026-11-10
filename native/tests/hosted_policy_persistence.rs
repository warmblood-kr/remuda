//! Acceptance: hosted remote-input preferences belong to the exact instance
//! and survive a daemon crash before remote input can ever be enabled.
#![cfg(unix)]
use remuda_core::{
    protocol::{Request, Response},
    Size,
};
#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Mode, Parity};

fn start(daemon: &Parity, name: &str, disabled: bool) -> String {
    match daemon.request(&Request::NewWithInstance {
        name: Some(name.into()),
        command: vec!["/bin/sh".into()],
        size: Size::new(80, 24),
        cwd: Some("/".into()),
        env: None,
        remote_input_disabled: disabled,
    }) {
        Response::SessionStarted { instance_id, .. } => instance_id,
        other => panic!("unexpected launch: {other:?}"),
    }
}
fn disabled(daemon: &Parity, name: &str, instance: &str) -> bool {
    match daemon.request(&Request::List) {
        Response::Sessions(rows) => {
            let row = rows
                .iter()
                .find(|row| row.name == name)
                .expect("session listed");
            assert_eq!(row.instance_id.as_deref(), Some(instance));
            row.remote_input_disabled
        }
        other => panic!("unexpected list: {other:?}"),
    }
}
fn set(daemon: &Parity, name: &str, instance: &str, opted_out: bool) -> Response {
    daemon.request(&Request::SetRemoteInputOptOut {
        name: name.into(),
        instance_id: instance.into(),
        opted_out,
    })
}
#[test]
fn launch_policy_and_later_changes_survive_daemon_crash() {
    let mut daemon = Parity::start(Mode::Hosted);
    let guarded = start(&daemon, "guarded", true);
    let open = start(&daemon, "open", false);
    assert!(disabled(&daemon, "guarded", &guarded));
    assert!(!disabled(&daemon, "open", &open));
    daemon.kill_daemon();
    daemon.restart();
    assert!(disabled(&daemon, "guarded", &guarded));
    assert!(!disabled(&daemon, "open", &open));
    assert_eq!(set(&daemon, "guarded", &guarded, false), Response::Ok);
    assert_eq!(set(&daemon, "open", &open, true), Response::Ok);
    daemon.kill_daemon();
    daemon.restart();
    assert!(!disabled(&daemon, "guarded", &guarded));
    assert!(disabled(&daemon, "open", &open));
}

#[test]
fn wrong_instance_cannot_change_policy_and_name_reuse_starts_fresh() {
    let daemon = Parity::start(Mode::Hosted);
    let old = start(&daemon, "guarded", true);
    assert_ne!(
        set(&daemon, "guarded", "wrong-instance", false),
        Response::Ok
    );
    assert!(disabled(&daemon, "guarded", &old));
    assert_eq!(
        daemon.request(&Request::Close {
            name: "guarded".into(),
            instance_id: None,
            confirm: None
        }),
        Response::Ok
    );
    let new = start(&daemon, "guarded", false);
    assert_ne!(old, new);
    assert_ne!(set(&daemon, "guarded", &old, true), Response::Ok);
    assert!(!disabled(&daemon, "guarded", &new));
}
