//! macOS-only acceptance supervisor: every command has its own session.
#[cfg(target_os = "macos")]
pub(crate) mod mac {
    use std::{
        ffi::OsString,
        io,
        os::unix::process::CommandExt,
        path::{Path, PathBuf},
        process::{Child, Command, ExitStatus, Stdio},
        sync::atomic::{AtomicBool, Ordering},
        time::{Duration, Instant},
    };
    static CANCELLED: AtomicBool = AtomicBool::new(false);
    extern "C" fn cancel(_: libc::c_int) {
        CANCELLED.store(true, Ordering::SeqCst);
    }
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    struct Group {
        child: Child,
        reaped: bool,
    }
    impl Group {
        fn spawn(command: &mut Command) -> io::Result<Self> {
            // setsid is async-signal-safe; no other work occurs after fork.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            Ok(Self {
                child: command.spawn()?,
                reaped: false,
            })
        }
        fn exited(&self) -> io::Result<bool> {
            // Keep the leader unreaped, including when it is a zombie. Its PID
            // anchors the session/group identity until every signal is sent.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { info.si_pid() } != 0)
        }
        fn finish(&mut self) -> io::Result<ExitStatus> {
            if !self.reaped {
                let group = -(self.child.id() as libc::pid_t);
                // Only a dedicated session whose leader is still our unreaped
                // child is targeted. Descendants that create a new session
                // are outside this process-group cleanup.
                unsafe {
                    libc::kill(group, libc::SIGTERM);
                }
                std::thread::sleep(Duration::from_millis(200));
                unsafe {
                    libc::kill(group, libc::SIGKILL);
                }
                let result = self.child.wait();
                self.reaped = true;
                return result;
            }
            Err(io::Error::other("child already reaped"))
        }
    }
    impl Drop for Group {
        fn drop(&mut self) {
            if !self.reaped {
                let _ = self.finish();
            }
        }
    }
    fn command(program: &OsString, args: &[&str], root: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .env("REMUDA_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("REMUDA_EXPERIMENTAL_SESSION_HOST", "0");
        cmd
    }
    fn bounded(cmd: &mut Command, timeout: Duration, cancellable: bool) -> io::Result<()> {
        let mut group = Group::spawn(cmd)?;
        let deadline = Instant::now() + timeout;
        while !group.exited()? {
            if (cancellable && CANCELLED.load(Ordering::SeqCst)) || Instant::now() >= deadline {
                return Err(io::Error::other(
                    "acceptance command interrupted or timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if group.finish()?.success() {
            Ok(())
        } else {
            Err(io::Error::other("acceptance command failed"))
        }
    }
    fn required(name: &str) -> io::Result<OsString> {
        let value =
            std::env::var_os(name).ok_or_else(|| io::Error::other(format!("set {name}")))?;
        if !Path::new(&value).is_absolute() {
            return Err(io::Error::other(format!("{name} must be absolute")));
        }
        Ok(value)
    }
    pub fn run() -> io::Result<()> {
        unsafe {
            libc::signal(libc::SIGTERM, cancel as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, cancel as *const () as libc::sighandler_t);
        }
        let remuda = required("REMUDA_GUI_ACCEPTANCE_REMUDA")?;
        let _gui = required("REMUDA_GUI_ACCEPTANCE_WORKER")?;
        let cargo =
            std::env::var_os("REMUDA_GUI_ACCEPTANCE_CARGO").unwrap_or_else(|| "cargo".into());
        let base = std::env::var_os("REMUDA_GUI_ACCEPTANCE_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/private/tmp"));
        let root = base.join(format!("rgtest-{}", std::process::id()));
        std::fs::create_dir(&root)?;
        let scratch = Scratch(root);
        for dir in ["runtime", "config", "data", "cache", "state"] {
            std::fs::create_dir(scratch.0.join(dir))?;
        }
        let mut daemon_cmd = command(&remuda, &["daemon"], &scratch.0);
        daemon_cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let mut daemon = Group::spawn(&mut daemon_cmd)?;
        let result = (|| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !scratch.0.join("runtime/remuda/default.sock").exists() {
                if CANCELLED.load(Ordering::SeqCst)
                    || daemon.exited()?
                    || Instant::now() >= deadline
                {
                    return Err(io::Error::other("daemon unavailable"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            bounded(
                &mut command(
                    &remuda,
                    &[
                        "-e",
                        "remuda.new('local-gui-proof', {'/bin/sh','-c','sleep 60'})",
                    ],
                    &scratch.0,
                ),
                Duration::from_secs(10),
                true,
            )?;
            bounded(
                &mut command(
                    &cargo,
                    &[
                        "test",
                        "--locked",
                        "--manifest-path",
                        "gui/Cargo.toml",
                        "--bin",
                        "remuda-gui",
                        "tests::real_local_daemon_reaches_session_row",
                        "--",
                        "--ignored",
                        "--exact",
                    ],
                    &scratch.0,
                ),
                Duration::from_secs(90),
                true,
            )
        })();
        // Stop gracefully when possible, but never wait indefinitely.
        let _ = bounded(
            &mut command(&remuda, &["stop", "-f"], &scratch.0),
            Duration::from_secs(2),
            false,
        );
        let _ = daemon.finish();
        result
    }
}
#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = mac::run() {
        eprintln!("acceptance failed: {error}");
        std::process::exit(1);
    }
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("The isolated acceptance supervisor currently supports macOS only");
    std::process::exit(1);
}
