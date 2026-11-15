//! Internal, opt-in supervisor for a verified GUI release artifact.
//! This is not wired into the installer or shipped in a release yet.
//! It cleans its private process group on timeout, parent exit, and completion.
//! Escaped descendants and forced guardian death are outside this guarantee; the marker is a caller assertion.
//! The fixed `--verify`, `--verify-cli`, and `--verify-host` modes query GUI, CLI, and host IDs respectively.

#[cfg(target_os = "macos")]
mod mac {
    use std::{
        io::{self, Read, Write},
        os::{
            fd::AsRawFd,
            unix::process::{CommandExt, ExitStatusExt},
        },
        path::Path,
        process::{Command, ExitCode, Stdio},
        time::{Duration, Instant},
    };

    const DEADLINE: Duration = Duration::from_secs(5);
    const OUT_LIMIT: usize = 256;
    const ERR_LIMIT: usize = 4096;

    #[derive(Clone, Copy)]
    enum Role {
        Gui,
        Cli,
        Host,
    }
    impl Role {
        fn guard_flag(self) -> &'static str {
            match self {
                Self::Gui => "--guard",
                Self::Cli => "--guard-cli",
                Self::Host => "--guard-host",
            }
        }
        fn probe_arg(self) -> &'static str {
            match self {
                Self::Gui => "--internal-build-id",
                Self::Cli => "--version",
                Self::Host => "version",
            }
        }
        fn expected(self, build: &str) -> String {
            match self {
                Self::Cli => format!("remuda {build}\n"),
                _ => format!("{build}\n"),
            }
        }
    }

    pub fn main() -> ExitCode {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.len() != 4 || args[1] != "--verified-archive-v1" {
            eprintln!(
                "installer probe: expected --verify[|-cli|-host] --verified-archive-v1 ABSOLUTE_PATH BUILD"
            );
            return ExitCode::FAILURE;
        }
        let Some(expected) = args[3].to_str() else {
            return ExitCode::FAILURE;
        };
        if expected.is_empty()
            || expected.len() > 200
            || !expected
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
            || !Path::new(&args[2]).is_absolute()
        {
            eprintln!("installer probe: invalid path or build identifier");
            return ExitCode::FAILURE;
        }
        match args[0].to_str() {
            Some("--verify") => parent(&args[2], expected, Role::Gui),
            Some("--verify-cli") => parent(&args[2], expected, Role::Cli),
            Some("--verify-host") => parent(&args[2], expected, Role::Host),
            Some("--guard") => guard(&args[2], expected, Role::Gui),
            Some("--guard-cli") => guard(&args[2], expected, Role::Cli),
            Some("--guard-host") => guard(&args[2], expected, Role::Host),
            _ => ExitCode::FAILURE,
        }
    }

    fn parent(program: &std::ffi::OsStr, expected: &str, role: Role) -> ExitCode {
        let exe = match std::env::current_exe() {
            Ok(path) => path,
            Err(_) => return ExitCode::FAILURE,
        };
        let mut command = Command::new(exe);
        command
            .args([role.guard_flag(), "--verified-archive-v1"])
            .arg(program)
            .arg(expected)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // The guardian remains session leader, anchoring its private group
        // until it exits or kills the group; no unrelated group is signalled.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => return ExitCode::FAILURE,
        };
        // Hold the write end open for the lifetime of the parent. Parent exit
        // closes it, so the guardian can cancel its owned process group.
        let keepalive = child.stdin.take();
        let result = child.wait_with_output().is_ok_and(|output| {
            output.status.signal() == Some(libc::SIGKILL) && output.stdout == [1]
        });
        drop(keepalive);
        if result {
            ExitCode::SUCCESS
        } else {
            eprintln!("installer probe: failed");
            ExitCode::FAILURE
        }
    }

    fn kill_owned_group(group: i32) {
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }

    fn guard(program: &std::ffi::OsStr, expected: &str, role: Role) -> ExitCode {
        let pid = unsafe { libc::getpid() };
        // A directly invoked guard must never signal the caller's group.
        if unsafe { libc::getpgrp() } != pid || unsafe { libc::getsid(0) } != pid {
            return ExitCode::FAILURE;
        }
        std::thread::spawn(move || {
            let mut input = [0];
            let _ = io::stdin().read(&mut input);
            kill_owned_group(pid);
        });
        std::thread::spawn(move || {
            std::thread::sleep(DEADLINE);
            kill_owned_group(pid);
        });
        let result = u8::from(verify(program, expected, role).is_ok());
        let _ = io::stdout().write_all(&[result]);
        let _ = io::stdout().flush();
        // The result is sent first; terminating our anchored private group
        // also cleans descendants after success or early rejection.
        kill_owned_group(pid);
        ExitCode::FAILURE
    }

    fn verify(program: &std::ffi::OsStr, expected: &str, role: Role) -> Result<(), ()> {
        let mut child = Command::new(program)
            .arg(role.probe_arg())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| ())?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let mut reaped = false;
        let result = (|| {
            for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                if flags < 0
                    || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
                {
                    return Err(());
                }
            }
            let until = Instant::now() + DEADLINE;
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let (mut out_done, mut err_done, mut status) = (false, false, None);
            loop {
                read_bounded(&mut stdout, &mut out, OUT_LIMIT, &mut out_done)?;
                read_bounded(&mut stderr, &mut err, ERR_LIMIT, &mut err_done)?;
                if status.is_none() {
                    status = child.try_wait().map_err(|_| ())?;
                    reaped = status.is_some();
                }
                if let Some(status) = status {
                    if out_done && err_done {
                        return if status.success()
                            && err.is_empty()
                            && out == role.expected(expected).as_bytes()
                        {
                            Ok(())
                        } else {
                            Err(())
                        };
                    }
                }
                if Instant::now() >= until {
                    return Err(());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })();
        if result.is_err() && !reaped {
            let _ = child.kill();
        }
        let _ = child.wait();
        result
    }

    fn read_bounded(
        reader: &mut impl Read,
        out: &mut Vec<u8>,
        limit: usize,
        done: &mut bool,
    ) -> Result<(), ()> {
        if *done {
            return Ok(());
        }
        let mut buf = [0; 512];
        for _ in 0..16 {
            match reader.read(&mut buf) {
                Ok(0) => {
                    *done = true;
                    break;
                }
                Ok(n) if n <= limit.saturating_sub(out.len()) => out.extend_from_slice(&buf[..n]),
                Ok(_) => return Err(()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(()),
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    mac::main()
}

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("installer probe: macOS only");
    std::process::ExitCode::FAILURE
}
