#![cfg(unix)]
use std::{
    io::Read,
    os::unix::net::UnixListener,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn stalled_worker_exits_after_its_parent() {
    let root = std::env::temp_dir().join(format!("rgw-{}", std::process::id()));
    std::fs::create_dir_all(root.join("remuda")).unwrap();
    let listener = UnixListener::bind(root.join("remuda/default.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut parent = Command::new("/bin/sh")
        .args([
            "-c",
            "\"$1\" --internal-list-worker >/dev/null 2>&1 &",
            "sh",
            env!("CARGO_BIN_EXE_remuda-gui"),
        ])
        .env("REMUDA_RUNTIME_DIR", &root)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    assert!(parent.wait().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut socket = loop {
        if let Ok((socket, _)) = listener.accept() {
            break socket;
        }
        assert!(Instant::now() < deadline, "helper did not connect");
        std::thread::sleep(Duration::from_millis(10));
    };
    socket.set_nonblocking(true).unwrap();
    let mut buf = [0; 1024];
    loop {
        match socket.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => (),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => panic!("worker retained daemon connection: {error}"),
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
