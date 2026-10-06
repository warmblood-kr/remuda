//! `remuda.process.exec`: asynchronous bounded process output on the image loop.

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;

static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(1);

struct Node {
    _daemon: spawn::Daemon,
    scratch: PathBuf,
}

impl Node {
    fn start() -> Self {
        let root = if cfg!(unix) {
            PathBuf::from("/tmp")
                .canonicalize()
                .expect("canonical /tmp")
        } else {
            std::env::temp_dir()
        };
        let scratch = root.join(format!(
            "remuda-exec-{}-{}",
            std::process::id(),
            NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&scratch).expect("create scratch");
        let command = spawn::base_command(&scratch);
        let daemon = spawn::spawn_and_wait(command, &scratch);
        Self {
            _daemon: daemon,
            scratch,
        }
    }

    fn eval(&self, code: &str) -> String {
        let socket = daemon::socket_path_in(&self.scratch, "s");
        match client::request(
            &socket,
            &Request::Eval {
                code: code.into(),
                name: None,
            },
        )
        .expect("eval request")
        {
            Response::Value(value) => value,
            other => panic!("eval failed: {other:?}"),
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

fn lua_string(value: &str) -> String {
    format!("{value:?}")
}

fn helper_argv() -> String {
    format!(
        "{{ {}, '--exact', 'process_exec_child', '--nocapture' }}",
        lua_string(
            &std::env::current_exe()
                .expect("test binary")
                .to_string_lossy()
        )
    )
}

#[test]
fn process_exec_child() {
    match std::env::var("REMUDA_EXEC_CHILD_MODE").as_deref() {
        Ok("slow") => {
            println!("slow-stdout");
            eprintln!("slow-stderr");
            std::thread::sleep(Duration::from_millis(400));
        }
        Ok("stdin") => {
            use std::io::{BufRead, Read, Write};
            let mut stdin = std::io::stdin().lock();
            let mut line = String::new();
            if std::env::var("REMUDA_EXEC_HOLD").is_ok() {
                stdin.read_line(&mut line).expect("read first line");
                println!("ready");
                std::io::stdout().flush().expect("flush ready");
                stdin
                    .read_to_string(&mut line)
                    .expect("read remaining stdin");
            } else {
                stdin.read_to_string(&mut line).expect("read stdin");
            }
            print!("{line}");
            std::io::stdout().flush().expect("flush stdout");
        }
        Ok("tree") => {
            let marker = std::env::var_os("REMUDA_EXEC_TREE_MARKER").expect("marker path");
            let mut child =
                std::process::Command::new(std::env::current_exe().expect("test binary"))
                    .args(["--exact", "process_exec_descendant_child", "--nocapture"])
                    .env("REMUDA_EXEC_DESCENDANT_MARKER", marker)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("spawn descendant");
            std::thread::sleep(Duration::from_secs(10));
            let _ = child.wait();
        }
        _ => {}
    }
}

#[test]
fn process_exec_descendant_child() {
    if let Some(marker) = std::env::var_os("REMUDA_EXEC_DESCENDANT_MARKER") {
        let marker = PathBuf::from(marker);
        std::fs::write(marker.with_extension("ready"), "ready").expect("write ready marker");
        std::thread::sleep(Duration::from_millis(2_000));
        std::fs::write(marker, "survived").expect("write survival marker");
    }
}

#[test]
fn process_exec_delivers_while_image_serves_timer() {
    let node = Node::start();
    let started = Instant::now();
    let argv = helper_argv();
    assert_eq!(
        node.eval(&format!(
            "remuda.process.exec({{ argv = {argv}, env = {{ REMUDA_EXEC_CHILD_MODE = 'slow' }} }}, function(r) remuda._exec_result = r end); remuda.after(0.05, function() remuda._exec_timer = true end); return 'started'"
        )),
        "started"
    );
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "exec blocked the image"
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if node.eval("return tostring(remuda._exec_timer)") == "true" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timer did not run during child execution"
        );
    }
    assert_eq!(node.eval("return tostring(remuda._exec_result)"), "nil");
    let deadline = Instant::now() + Duration::from_secs(2);
    while node.eval("return tostring(remuda._exec_result)") == "nil" {
        assert!(Instant::now() < deadline, "exec callback did not arrive");
    }
    assert_eq!(node.eval("return tostring(remuda._exec_result.code)"), "0");
    assert_eq!(node.eval("return remuda._exec_result.stdout"), "slow-stdout\n");
    assert_eq!(node.eval("return remuda._exec_result.stderr"), "slow-stderr\n");
    assert_eq!(node.eval("return tostring(remuda._exec_result.timed_out)"), "false");
}

#[test]
fn process_exec_refuses_bad_spec_synchronously() {
    let node = Node::start();
    let error = node.eval("local ok, err = pcall(function() remuda.process.exec({ argv = {} }, function() end) end); return ok and 'no error' or tostring(err)");
    assert!(
        error.contains("non-empty `argv`"),
        "unexpected error: {error}"
    );
}

#[test]
fn process_exec_timeout_kills_child_group() {
    let node = Node::start();
    let marker = node.scratch.join("survivor");
    let argv = helper_argv();
    assert_eq!(
        node.eval(&format!(
            "remuda.process.exec({{ argv = {argv}, timeout = 1.0, env = {{ REMUDA_EXEC_CHILD_MODE = 'tree', REMUDA_EXEC_TREE_MARKER = {} }} }}, function(r) remuda._exec_timeout = tostring(r.timed_out) end); return 'started'",
            lua_string(&marker.to_string_lossy())
        )),
        "started"
    );
    let ready = marker.with_extension("ready");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "descendant did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    while node.eval("return tostring(remuda._exec_timeout)") == "nil" {
        assert!(Instant::now() < deadline, "timeout callback did not arrive");
    }
    assert_eq!(node.eval("return remuda._exec_timeout"), "true");
    std::thread::sleep(Duration::from_millis(1_200));
    assert!(
        !marker.exists(),
        "descendant survived the timed-out process group"
    );
}

#[test]
fn process_exec_stdin_hold_until_lines() {
    let node = Node::start();
    let argv = helper_argv();
    assert_eq!(
        node.eval(&format!(
            "remuda.process.exec({{ argv = {argv}, stdin = 'request\\nresponse', stdin_hold_until_lines = 1, timeout = 2, env = {{ REMUDA_EXEC_CHILD_MODE = 'stdin', REMUDA_EXEC_HOLD = '1' }} }}, function(r) remuda._exec_stdin = r.stdout end); return 'started'"
        )),
        "started"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while node.eval("return tostring(remuda._exec_stdin)") == "nil" {
        assert!(Instant::now() < deadline, "stdin callback did not arrive");
    }
    assert!(
        node.eval("return remuda._exec_stdin")
            .contains("ready\nrequest\nresponse"),
        "stdin was not kept open until the child emitted a line"
    );
}
