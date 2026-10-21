//! Staged socket publication end to end (#660 slice 3): the shipped binary,
//! real sockets, short directories so the staged name fits `sun_path`.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;
use spawn::Daemon;

fn short_dir(tag: &str) -> PathBuf {
    // Not temp_dir(): macOS's long $TMPDIR defeats the staged name budget.
    let dir = PathBuf::from("/tmp").join(format!("rs{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn publication(socket: &Path) -> String {
    match client::request(socket, &Request::SocketPublication) {
        Ok(Response::Value(text)) => text,
        other => panic!("socket publication status: {other:?}"),
    }
}

fn shutdown(socket: &Path) {
    let request = Request::Shutdown {
        requester_daemon_id: None,
        requester_session_id: None,
        requester_session_name: None,
        override_hosted: true,
    };
    assert!(matches!(
        client::request(socket, &request),
        Ok(Response::Ok)
    ));
}

#[test]
fn staged_publication_is_the_default_and_shutdown_removes_the_socket() {
    let dir = short_dir("default");
    let socket = daemon::socket_path_in(&dir, "s");
    let mut daemon = Daemon::spawn(&dir);
    assert_eq!(publication(&socket), "staged");
    // No staging directory survives a successful start.
    let leftovers: Vec<_> = std::fs::read_dir(socket.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".r660-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    shutdown(&socket); // hosted-shutdown override path
    assert!(daemon.left_on_its_own());
    assert!(!socket.exists(), "receipted cleanup removes the socket");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kill_switch_keeps_the_legacy_bind_and_says_so() {
    let dir = short_dir("switch");
    let socket = daemon::socket_path_in(&dir, "s");
    let mut command = spawn::base_command(&dir);
    command.env("REMUDA_SOCKET_PUBLICATION", "legacy");
    let mut daemon = spawn::spawn_and_wait(command, &dir);
    let status = publication(&socket);
    assert!(
        status.starts_with("legacy: REMUDA_SOCKET_PUBLICATION=legacy"),
        "{status}"
    );
    shutdown(&socket);
    assert!(daemon.left_on_its_own());
    assert!(!socket.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn long_runtime_dir_falls_back_to_legacy_with_a_reason() {
    let dir = short_dir("long");
    // Public path fits (<= limit) but the longer staged name cannot.
    let pad = "p".repeat(103 - dir.join("remuda").join("s.sock").as_os_str().len() - 2);
    let long = dir.join(pad);
    std::fs::create_dir_all(&long).unwrap();
    let socket = daemon::socket_path_in(&long, "s");
    assert!(socket.as_os_str().len() <= 103, "fixture must fit sun_path");
    let mut daemon = spawn::spawn_and_wait(spawn::base_command(&long), &long);
    let status = publication(&socket);
    assert!(
        status.starts_with("legacy: socket path too long"),
        "{status}"
    );
    shutdown(&socket);
    assert!(daemon.left_on_its_own());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stale_socket_is_replaced_by_receipt_under_staged_publication() {
    let dir = short_dir("stale");
    let socket = daemon::socket_path_in(&dir, "s");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    // std's listener leaves its node behind on drop, like a crashed daemon.
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());
    let mut daemon = Daemon::spawn(&dir);
    assert_eq!(publication(&socket), "staged");
    shutdown(&socket);
    assert!(daemon.left_on_its_own());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_non_socket_at_the_path_is_refused_and_preserved() {
    let dir = short_dir("file");
    let socket = daemon::socket_path_in(&dir, "s");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::write(&socket, b"not a socket").unwrap();
    let status = spawn::base_command(&dir).status().unwrap();
    assert!(!status.success());
    assert_eq!(std::fs::read(&socket).unwrap(), b"not a socket");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_starts_leave_exactly_one_serving_daemon() {
    let dir = short_dir("race");
    let socket = daemon::socket_path_in(&dir, "s");
    let mut first = Daemon::adopt(spawn::base_command(&dir).spawn().unwrap(), &dir);
    let mut second = Daemon::adopt(spawn::base_command(&dir).spawn().unwrap(), &dir);
    let deadline = Instant::now() + Duration::from_secs(10);
    // One exits with the "already listening" refusal; the other serves.
    while first.0.try_wait().unwrap().is_none() && second.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "neither daemon settled");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (survivor, loser) = if first.0.try_wait().unwrap().is_some() {
        (&mut second, &mut first)
    } else {
        (&mut first, &mut second)
    };
    assert!(!loser.0.try_wait().unwrap().unwrap().success());
    spawn::wait_until_served(&mut survivor.0, &socket, Duration::from_secs(10));
    assert_eq!(publication(&socket), "staged");
    shutdown(&socket);
    assert!(survivor.left_on_its_own());
    let _ = std::fs::remove_dir_all(&dir);
}
