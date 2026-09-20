//! One seam for "this child must not outlive this daemon": every process the
//! daemon spawns funnels its guarantee through here, even on a path or
//! platform where the guarantee is currently a documented absence rather
//! than a mechanism.
//!
//! `process.rs`'s plain-pipe children route through `harden()` below.
//! `pty.rs`'s children currently survive daemon death by an unrelated kernel
//! accident (the daemon closes the pty master fd on any exit, which SIGHUPs
//! the pty's session-leader child) — not by anything in this module. That
//! path calls `documented_pty_hangup_accident()` at its spawn site so the
//! decision not to add a guard there is visible, not merely absent, and is
//! pinned by a real test: native/tests/pty_survives_daemon_death.rs.

use std::process::Command;

/// What happens to a spawned child if the daemon that spawned it dies.
#[derive(Debug, PartialEq, Eq)]
pub enum Guard {
    // PR_SET_PDEATHSIG(SIGKILL) + its own process group, applied pre-exec.
    // Reaps the DIRECT child on any daemon death via a kernel signal alone.
    // Never reaches a grandchild forked before that signal — see `killpg`.
    LinuxPdeathsig,
    // Not implemented for this platform: the child orphans if this daemon
    // dies unexpectedly. `reason` names the gap, so it's loud, not silent.
    Unimplemented(&'static str),
}

/// Apply the guard to a not-yet-spawned `Command` — the only function in
/// this codebase that may call `pre_exec`/`process_group` for this purpose.
#[cfg(target_os = "linux")]
pub fn harden(command: &mut Command) -> Guard {
    use std::os::unix::process::CommandExt;
    let daemon_pid = std::process::id() as libc::pid_t;

    // Own process group: anything the direct child forks before it dies
    // inherits this pgid (fork always inherits pgid). Nothing kills the
    // group automatically — see process.rs's `killpg` for the one caller
    // that uses this on purpose.
    command.process_group(0);

    // SAFETY: pre_exec runs in the forked child, before exec, with only
    // async-signal-safe calls permitted. prctl(PR_SET_PDEATHSIG), getppid()
    // and raise() are all on that list.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Registration race: our parent may have already died between
            // fork() and this prctl() call, in which case no future signal
            // is coming for an event that already happened. Self-check and
            // self-kill rather than trust a signal that will never fire.
            if libc::getppid() != daemon_pid {
                libc::raise(libc::SIGKILL);
            }
            Ok(())
        });
    }

    Guard::LinuxPdeathsig
}

#[cfg(not(target_os = "linux"))]
pub fn harden(_command: &mut Command) -> Guard {
    static WARNED: std::sync::Once = std::sync::Once::new();
    let reason = "no PDEATHSIG-equivalent wired for this platform yet -- a \
        child spawned here will orphan if this daemon dies unexpectedly \
        (built and measured on Linux only; macOS needs a kqueue \
        EVFILT_PROC supervisor or a pipe-EOF trick, Windows needs a Job \
        Object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE — neither exists \
        here yet)";
    WARNED.call_once(|| eprintln!("remuda: WARNING: {reason}"));
    Guard::Unimplemented(reason)
}

/// Not a guard — a note, called at `pty.rs`'s spawn site, so grep finds an
/// explicit decision there (this module's own doc comment above explains
/// why) instead of silence. Does nothing at runtime.
pub fn documented_pty_hangup_accident() {}
