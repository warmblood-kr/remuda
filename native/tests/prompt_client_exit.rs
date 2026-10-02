#![cfg(unix)]

use std::fs;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WAIT: Duration = Duration::from_secs(2);

struct ScratchDir(PathBuf);

impl std::ops::Deref for ScratchDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scratch_dir() -> ScratchDir {
    let root = if cfg!(target_os = "macos") {
        PathBuf::from("/private/tmp")
    } else {
        std::env::temp_dir()
    };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    ScratchDir(root.join(format!("remuda-prompt-exit-{}-{stamp}", std::process::id())))
}

fn prepare(dir: &Path) {
    let package = dir.join("data/remuda/mods/prompt_exit");
    fs::create_dir_all(package.join("packages/prompt_exit")).unwrap();
    fs::write(
        package.join("extension.toml"),
        "name = \"prompt_exit\"\nentry = \"packages/prompt_exit/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"prompt_exit\"\n",
    )
    .unwrap();
    fs::write(
        package.join("packages/prompt_exit/init.lua"),
        r#"remuda.extension_command("prompt_exit", function(args)
  assert(args[1] == "wait")
  local reply = remuda.pending { timeout = 300 }
  reply:prompt_line { label = "wizard prompt", callback = function(value, err)
    if err then reply:reject(tostring(err) .. "\nNext: rerun remuda prompt_exit")
    else reply:resolve(0, "answer: " .. (value or ""), "") end
  end }
  return reply
end)"#,
    )
    .unwrap();
    fs::create_dir_all(dir.join("runtime")).unwrap();
}

fn env_command(args: &[&str], dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
    command
        .args(["-s", "s"])
        .args(args)
        .env("REMUDA_RUNTIME_DIR", dir.join("runtime"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("HOME", dir.join("home"))
        .env("REMUDA_NO_UPDATE_CHECK", "1");
    command
}

struct PrivateDaemon(std::process::Child);

impl PrivateDaemon {
    fn stop(&mut self, dir: &Path) {
        stop_daemon(dir);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.0.try_wait().expect("poll private daemon").is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("private daemon did not stop");
    }
}

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn start_daemon(dir: &Path) -> PrivateDaemon {
    let mut command = env_command(&["daemon"], dir);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().expect("spawn private daemon");
    let socket = remuda_native::daemon::socket_path_in(&dir.join("runtime"), "s");
    let deadline = Instant::now() + Duration::from_secs(10);
    while remuda_native::ipc::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "daemon never bound {socket:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    PrivateDaemon(child)
}

fn stop_daemon(dir: &Path) {
    let output = env_command(&["stop", "-f"], dir)
        .output()
        .expect("stop private daemon");
    assert!(output.status.success(), "stop daemon: {output:?}");
}

struct PromptClient {
    child: std::process::Child,
    master: Arc<Mutex<Option<fs::File>>>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl PromptClient {
    fn spawn(dir: &Path) -> Self {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0,
            "open pty pair"
        );
        let master_file = unsafe { fs::File::from_raw_fd(master_fd) };
        let slave_file = unsafe { fs::File::from_raw_fd(slave_fd) };
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command
            .args(["-s", "s", "prompt_exit", "wait"])
            .env("REMUDA_RUNTIME_DIR", dir.join("runtime"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("HOME", dir.join("home"))
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .stdin(Stdio::from(
                slave_file.try_clone().expect("clone pty slave"),
            ))
            .stdout(Stdio::from(
                slave_file.try_clone().expect("clone pty slave"),
            ))
            .stderr(Stdio::from(slave_file));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn prompt client");
        let output = Arc::new(Mutex::new(Vec::new()));
        let master = Arc::new(Mutex::new(Some(master_file)));
        let reader_master = Arc::clone(&master);
        let reader_output = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut buffer = [0; 1024];
            loop {
                let mut guard = reader_master.lock().unwrap();
                let Some(reader) = guard.as_mut() else {
                    break;
                };
                let mut poll_fd = libc::pollfd {
                    fd: reader.as_raw_fd(),
                    events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut poll_fd, 1, 100) };
                if ready <= 0 {
                    continue;
                }
                let count = match reader.read(&mut buffer) {
                    Ok(count) => count,
                    Err(_) => break,
                };
                if count == 0 {
                    break;
                }
                reader_output
                    .lock()
                    .unwrap()
                    .extend_from_slice(&buffer[..count]);
            }
        });
        Self {
            child,
            master,
            output,
        }
    }

    fn wait_for_prompt(&self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self
                .output
                .lock()
                .unwrap()
                .windows(b"wizard prompt".len())
                .any(|window| window == b"wizard prompt")
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn signal(&self, signal: libc::c_int) {
        let pid = self.child.id() as libc::pid_t;
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0, "send signal");
    }

    fn wait(&mut self) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("poll client") {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    fn close_terminal(&mut self) {
        self.master.lock().unwrap().take();
    }

    fn write_terminal(&self, bytes: &[u8]) {
        self.master
            .lock()
            .unwrap()
            .as_mut()
            .expect("terminal still open")
            .write_all(bytes)
            .expect("write pty input");
    }

    fn cleanup(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Drop for PromptClient {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[test]
fn prompt_client_exits_when_daemon_or_terminal_disappears_and_on_sigterm() {
    let dir = scratch_dir();
    prepare(&dir);

    let mut daemon = start_daemon(&dir);
    let mut normal_answer = PromptClient::spawn(&dir);
    assert!(normal_answer.wait_for_prompt(), "prompt did not appear");
    normal_answer.write_terminal(b"answer\r");
    let normal_answer_exited = normal_answer.wait();
    let normal_answer_output = normal_answer.output.lock().unwrap().clone();
    normal_answer.cleanup();
    daemon.stop(&dir);

    let mut daemon = start_daemon(&dir);
    let mut live_daemon_stop = PromptClient::spawn(&dir);
    assert!(
        live_daemon_stop.wait_for_prompt(),
        "prompt did not appear from {}: {}",
        env!("CARGO_BIN_EXE_remuda"),
        String::from_utf8_lossy(&live_daemon_stop.output.lock().unwrap()),
    );
    daemon.stop(&dir);
    let daemon_stop_exited = live_daemon_stop.wait();
    live_daemon_stop.cleanup();

    let mut daemon = start_daemon(&dir);
    let mut live_sigterm = PromptClient::spawn(&dir);
    assert!(live_sigterm.wait_for_prompt(), "prompt did not appear");
    live_sigterm.signal(libc::SIGTERM);
    let live_sigterm_exited = live_sigterm.wait();
    live_sigterm.cleanup();
    daemon.stop(&dir);

    let mut daemon = start_daemon(&dir);
    let mut gone_terminal = PromptClient::spawn(&dir);
    assert!(gone_terminal.wait_for_prompt(), "prompt did not appear");
    gone_terminal.close_terminal();
    daemon.stop(&dir);
    let terminal_gone_exited = gone_terminal.wait();
    gone_terminal.cleanup();

    let mut daemon = start_daemon(&dir);
    let mut gone_terminal_sigterm = PromptClient::spawn(&dir);
    assert!(
        gone_terminal_sigterm.wait_for_prompt(),
        "prompt did not appear"
    );
    gone_terminal_sigterm.close_terminal();
    gone_terminal_sigterm.signal(libc::SIGTERM);
    let terminal_gone_sigterm_exited = gone_terminal_sigterm.wait();
    gone_terminal_sigterm.cleanup();
    daemon.stop(&dir);

    eprintln!(
        "prompt exit observations: normal answer={normal_answer_exited:?}, daemon stop={daemon_stop_exited:?}, live SIGTERM={live_sigterm_exited:?}, closed terminal + daemon stop={terminal_gone_exited:?}, closed terminal + SIGTERM={terminal_gone_sigterm_exited:?}"
    );
    assert!(
        normal_answer_exited.is_some_and(|status| status.success()),
        "answering a normal prompt did not succeed: {normal_answer_exited:?}"
    );
    assert!(
        String::from_utf8_lossy(&normal_answer_output).contains("answer: answer"),
        "normal prompt answer was not returned: {}",
        String::from_utf8_lossy(&normal_answer_output)
    );
    assert!(
        daemon_stop_exited.is_some_and(|status| !status.success()),
        "daemon stop did not end the client with a non-zero status: {daemon_stop_exited:?}"
    );
    let daemon_stop_output_bytes = live_daemon_stop.output.lock().unwrap().clone();
    let daemon_stop_output = String::from_utf8_lossy(&daemon_stop_output_bytes);
    assert_eq!(
        daemon_stop_output
            .matches("daemon connection closed while waiting for a line prompt")
            .count(),
        1,
        "expected one disconnect line, got {daemon_stop_output:?}"
    );
    assert_eq!(
        daemon_stop_output
            .matches("Next: restart the daemon")
            .count(),
        1,
        "expected one Next: line, got {daemon_stop_output:?}"
    );
    assert!(
        live_sigterm_exited.is_some_and(|status| !status.success()),
        "SIGTERM left client alive past {WAIT:?}; terminal stayed open"
    );
    assert!(
        terminal_gone_exited.is_some_and(|status| !status.success()),
        "closed pty master left client alive past {WAIT:?}"
    );
    assert!(
        terminal_gone_sigterm_exited.is_some_and(|status| !status.success()),
        "closed pty master plus SIGTERM left client alive past {WAIT:?}"
    );
}
