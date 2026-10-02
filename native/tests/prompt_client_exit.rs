#![cfg(unix)]

use std::fs;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WAIT: Duration = Duration::from_secs(10);

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
  local reply = remuda.pending { timeout = 30 }
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
        let deadline = Instant::now() + Duration::from_secs(10);
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
        let deadline = Instant::now() + Duration::from_secs(10);
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
        assert_eq!(
            unsafe { libc::kill(pid, signal) },
            0,
            "send signal; client output: {}",
            self.captured_output()
        );
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

    fn captured_output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
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

struct ClientResult {
    status: Option<std::process::ExitStatus>,
    output: String,
}

fn waiting_prompt(dir: &Path) -> PromptClient {
    let client = PromptClient::spawn(dir);
    assert!(
        client.wait_for_prompt(),
        "prompt did not appear; client output: {}",
        client.captured_output()
    );
    client
}

fn answer_prompt(dir: &Path) -> ClientResult {
    let mut daemon = start_daemon(dir);
    let mut client = waiting_prompt(dir);
    client.write_terminal(b"answer\r");
    let result = ClientResult {
        status: client.wait(),
        output: client.captured_output(),
    };
    client.cleanup();
    daemon.stop(dir);
    result
}

fn stop_daemon_with_prompt(dir: &Path) -> ClientResult {
    let mut daemon = start_daemon(dir);
    let mut client = waiting_prompt(dir);
    daemon.stop(dir);
    let result = ClientResult {
        status: client.wait(),
        output: client.captured_output(),
    };
    client.cleanup();
    result
}

fn sigterm_with_prompt(dir: &Path) -> ClientResult {
    let mut daemon = start_daemon(dir);
    let mut client = waiting_prompt(dir);
    client.signal(libc::SIGTERM);
    let result = ClientResult {
        status: client.wait(),
        output: client.captured_output(),
    };
    client.cleanup();
    daemon.stop(dir);
    result
}

fn close_terminal_with_daemon_stop(dir: &Path) -> ClientResult {
    let mut daemon = start_daemon(dir);
    let mut client = waiting_prompt(dir);
    client.close_terminal();
    daemon.stop(dir);
    let result = ClientResult {
        status: client.wait(),
        output: client.captured_output(),
    };
    client.cleanup();
    result
}

fn close_terminal_with_sigterm(dir: &Path) -> ClientResult {
    let mut daemon = start_daemon(dir);
    let mut client = waiting_prompt(dir);
    client.close_terminal();
    client.signal(libc::SIGTERM);
    let result = ClientResult {
        status: client.wait(),
        output: client.captured_output(),
    };
    client.cleanup();
    daemon.stop(dir);
    result
}

#[test]
fn prompt_client_exits_when_daemon_or_terminal_disappears_and_on_sigterm() {
    let dir = scratch_dir();
    prepare(&dir);
    let normal_answer = answer_prompt(&dir);
    let daemon_stop = stop_daemon_with_prompt(&dir);
    let live_sigterm = sigterm_with_prompt(&dir);
    let terminal_gone = close_terminal_with_daemon_stop(&dir);
    let terminal_gone_sigterm = close_terminal_with_sigterm(&dir);

    assert!(
        normal_answer.status.is_some_and(|status| status.success()),
        "answering a normal prompt did not succeed: {:?}; client output: {}",
        normal_answer.status,
        normal_answer.output
    );
    assert!(
        normal_answer.output.contains("answer: answer"),
        "normal prompt answer was not returned: {}",
        normal_answer.output
    );
    assert!(
        daemon_stop.status.is_some_and(|status| !status.success()),
        "daemon stop did not end the client with a non-zero status: {:?}; client output: {}",
        daemon_stop.status,
        daemon_stop.output
    );
    assert_eq!(
        daemon_stop
            .output
            .matches("daemon connection closed while waiting for a line prompt")
            .count(),
        1,
        "expected one disconnect line; client output: {}",
        daemon_stop.output
    );
    assert_eq!(
        daemon_stop
            .output
            .matches("Next: restart the daemon")
            .count(),
        1,
        "expected one Next: line; client output: {}",
        daemon_stop.output
    );
    assert!(
        live_sigterm.status.is_some_and(|status| !status.success()),
        "SIGTERM left client alive past {WAIT:?}; terminal stayed open; client output: {}",
        live_sigterm.output
    );
    assert!(
        terminal_gone.status.is_some_and(|status| !status.success()),
        "closed pty master left client alive past {WAIT:?}; client output: {}",
        terminal_gone.output
    );
    assert!(
        terminal_gone_sigterm
            .status
            .is_some_and(|status| !status.success()),
        "closed pty master plus SIGTERM left client alive past {WAIT:?}; client output: {}",
        terminal_gone_sigterm.output
    );
}
