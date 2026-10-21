//! `remuda reload` (#713), Unix. The running daemon (O) starts a candidate
//! daemon (C) from the target binary, joined by a private socketpair on
//! C's fd 3. C reports its build and what it sees of every session host,
//! without claiming anything. O compares C's view with its own. Then O either
//! aborts (a check) or sends `Take` with its listener and socket lock, and C
//! claims the hosts and serves in O's place.
pub mod fds;
pub mod upgrade;

use crate::session_host::hosted::{Checked, CheckedState, Hosting};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The O-C message version; a candidate of another version is refused.
/// 2 added `Take` and `Serving`.
pub const HANDOFF: u16 = 2;
/// C's end of the socketpair.
pub const CANDIDATE_FD: RawFd = 3;
/// The hidden CLI verb that runs a candidate.
pub const CANDIDATE_VERB: &str = "_reload-candidate";
const FRAME_LIMIT: usize = 64 * 1024;
const HELLO_DEADLINE: Duration = Duration::from_secs(10);
const READY_DEADLINE: Duration = Duration::from_secs(30);
const EXIT_DEADLINE: Duration = Duration::from_secs(2);
/// Test-only: make a candidate fail after `Take` (see `serve_taken`).
const TEST_FAULT: &str = "REMUDA_TEST_RELOAD_FAULT";

/// C to O.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum FromCandidate {
    Hello {
        handoff: u16,
        build: String,
    },
    Ready {
        hosts: Vec<HostView>,
    },
    Failed {
        reason: String,
    },
    /// After `Take`: C claimed the hosts and is about to accept.
    Serving,
}
/// O to C.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum FromOld {
    Abort,
    /// Carries the listener, then the socket lock, as SCM_RIGHTS.
    Take,
}
/// One session host as a daemon saw it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostView {
    pub name: String,
    /// The host's build when it answered Inspect.
    pub build: Option<String>,
    /// Why not, when it did not answer and was not settling.
    pub problem: Option<String>,
}
impl From<Checked> for HostView {
    fn from(checked: Checked) -> Self {
        let (build, problem) = match checked.state {
            CheckedState::Running { build } => (Some(build), None),
            CheckedState::Settling => (None, None),
            CheckedState::Unreachable(reason) => (None, Some(reason)),
        };
        Self {
            name: checked.name,
            build,
            problem,
        }
    }
}

/// What a check found. `blockers` empty means a reload may go ahead.
#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    pub candidate: String,
    pub hosts: Vec<(HostView, HostView)>,
    pub blockers: Vec<String>,
}
impl Report {
    /// Lines for the CLI: verdict first.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![if self.blockers.is_empty() {
            format!(
                "ready: a reload into {} would keep every host",
                self.candidate
            )
        } else {
            format!(
                "blocked: a reload into {} would not be safe",
                self.candidate
            )
        }];
        lines.extend(self.blockers.iter().map(|b| format!("blocker: {b}")));
        for (old, new) in &self.hosts {
            let seen = |view: &HostView| match (&view.build, &view.problem) {
                (Some(build), _) => build.clone(),
                (None, Some(problem)) => format!("unreachable ({problem})"),
                (None, None) => "settling".into(),
            };
            lines.push(format!(
                "host {}: now {}; candidate sees {}",
                old.name,
                seen(old),
                seen(new)
            ));
        }
        lines
    }
}

/// Deadlines for one check; tests shorten them.
#[derive(Clone, Copy, Debug)]
pub struct Deadlines {
    pub hello: Duration,
    pub ready: Duration,
    /// From `Take` to `Serving`.
    pub serving: Duration,
}
impl Default for Deadlines {
    fn default() -> Self {
        let ready = std::env::var("REMUDA_TEST_RELOAD_READY_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
            .map_or(READY_DEADLINE, Duration::from_millis);
        Self {
            hello: HELLO_DEADLINE.min(ready),
            ready,
            serving: ready,
        }
    }
}

static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
struct InFlight;
impl InFlight {
    fn take() -> Result<Self, String> {
        IN_FLIGHT
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| Self)
            .map_err(|_| "another reload is already in progress".into())
    }
}
impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.store(false, Ordering::SeqCst);
    }
}

/// A candidate that has reported. O either aborts it (a check, or a
/// blocked reload) or hands it the endpoint.
pub(crate) struct Candidate {
    stream: UnixStream,
    child: Child,
    report: Report,
    _one: InFlight,
}
impl Candidate {
    pub(crate) fn report(&self) -> &Report {
        &self.report
    }
    /// Tell C to stop and reap it.
    pub(crate) fn abort(mut self) -> Report {
        let _ = write_frame(&mut self.stream, &FromOld::Abort);
        drop(self.stream);
        reap(&mut self.child);
        self.report
    }
    /// Send `Take` with the listener and the socket lock, then wait for
    /// `Serving`. Anything else kills C: O keeps its copies and rolls back.
    pub(crate) fn take(
        mut self,
        listener: RawFd,
        lock: RawFd,
        until: Duration,
    ) -> Result<(), String> {
        let outcome = self.hand_over(listener, lock, Instant::now() + until);
        if outcome.is_err() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // On success C outlives this daemon; nobody reaps it here.
        outcome
    }
    fn hand_over(&mut self, listener: RawFd, lock: RawFd, until: Instant) -> Result<(), String> {
        let bytes = frame(&FromOld::Take).map_err(|e| format!("take: {e}"))?;
        fds::send(&self.stream, &bytes, &[listener, lock])
            .map_err(|e| format!("could not hand over the listener: {e}"))?;
        match read_frame(&mut self.stream, &mut self.child, until, "serving")? {
            FromCandidate::Serving => Ok(()),
            FromCandidate::Failed { reason } => {
                Err(format!("candidate failed to take over: {reason}"))
            }
            other => Err(format!("candidate sent {other:?} instead of serving")),
        }
    }
}

/// O's side of a check: start C and hear its report. `legacy` names this
/// daemon's in-process sessions, which a reload would end.
pub(crate) fn start(
    socket: &Path,
    binary: &Path,
    hosting: &Hosting,
    legacy: &[String],
    deadlines: Deadlines,
) -> Result<Candidate, String> {
    let one = InFlight::take()?;
    if !binary.is_absolute() || !binary.is_file() {
        return Err(format!(
            "reload target {} is not an absolute path to a file",
            binary.display()
        ));
    }
    let ours: Vec<HostView> = hosting
        .preflight()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(HostView::from)
        .collect();
    let (mut stream, mut child) = spawn(socket, binary)?;
    match converse(&mut stream, &mut child, deadlines) {
        Ok((candidate, theirs)) => Ok(Candidate {
            stream,
            child,
            report: compare(candidate, ours, theirs, legacy),
            _one: one,
        }),
        Err(error) => {
            let _ = write_frame(&mut stream, &FromOld::Abort);
            drop(stream);
            reap(&mut child);
            Err(error)
        }
    }
}

fn converse(
    stream: &mut UnixStream,
    child: &mut Child,
    deadlines: Deadlines,
) -> Result<(String, Vec<HostView>), String> {
    let started = Instant::now();
    let build = match read_frame(stream, child, started + deadlines.hello, "hello")? {
        FromCandidate::Hello { handoff, build } if handoff == HANDOFF => build,
        FromCandidate::Hello { handoff, build } => {
            return Err(format!(
                "candidate {build} speaks handoff version {handoff}, this daemon {HANDOFF}"
            ))
        }
        other => return Err(format!("candidate sent {other:?} before its hello")),
    };
    match read_frame(stream, child, started + deadlines.ready, "ready")? {
        FromCandidate::Ready { hosts } => Ok((build, hosts)),
        FromCandidate::Failed { reason } => Err(format!("candidate {build} failed: {reason}")),
        other => Err(format!("candidate {build} sent {other:?} instead of ready")),
    }
}

/// Every host this daemon can drive must be one the candidate can drive.
fn compare(
    candidate: String,
    ours: Vec<HostView>,
    theirs: Vec<HostView>,
    legacy: &[String],
) -> Report {
    let mut blockers = Vec::new();
    if !legacy.is_empty() {
        blockers.push(format!(
            "in-process sessions would end: {} (only host-backed sessions survive a reload)",
            legacy.join(", ")
        ));
    }
    let mut hosts = Vec::new();
    for old in ours {
        let new = theirs
            .iter()
            .find(|view| view.name == old.name)
            .cloned()
            .unwrap_or_else(|| HostView {
                name: old.name.clone(),
                build: None,
                problem: Some("not in the candidate's inventory".into()),
            });
        if old.build.is_some() && new.build.is_none() {
            blockers.push(format!(
                "host {}: the candidate cannot drive it ({})",
                old.name,
                new.problem.as_deref().unwrap_or("settling")
            ));
        }
        hosts.push((old, new));
    }
    Report {
        candidate,
        hosts,
        blockers,
    }
}

/// Start C from `binary` with its end of a socketpair on fd 3, this
/// daemon's environment and working directory, and its stderr.
fn spawn(socket: &Path, binary: &Path) -> Result<(UnixStream, Child), String> {
    use std::os::unix::process::CommandExt;
    let (ours, theirs) = UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
    let fd = theirs.as_raw_fd();
    let mut command = Command::new(binary);
    command
        .arg(CANDIDATE_VERB)
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    // SAFETY: dup2 and fcntl are async-signal-safe; nothing else runs here.
    unsafe {
        command.pre_exec(move || {
            if fd == CANDIDATE_FD {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(fd, CANDIDATE_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|e| format!("start candidate {}: {e}", binary.display()))?;
    drop(theirs);
    Ok((ours, child))
}

fn reap(child: &mut Child) {
    let until = Instant::now() + EXIT_DEADLINE;
    while Instant::now() < until {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// C's side: say hello, check every host read-only and report. Claims,
/// binds and writes nothing unless O then sends `Take`.
pub fn candidate(socket: &Path) -> Result<(), String> {
    // SAFETY: O placed our end of the socketpair on this fd before exec.
    let mut stream = unsafe { UnixStream::from_raw_fd(CANDIDATE_FD) };
    // Sessions this daemon starts must not inherit it.
    set_cloexec(CANDIDATE_FD).map_err(|e| format!("handoff fd: {e}"))?;
    write_frame(
        &mut stream,
        &FromCandidate::Hello {
            handoff: HANDOFF,
            build: crate::dist::BUILD_VERSION.into(),
        },
    )
    .map_err(|e| format!("hello: {e}"))?;
    let report = match Hosting::for_socket(socket).preflight() {
        Ok(checked) => FromCandidate::Ready {
            hosts: checked.into_iter().map(HostView::from).collect(),
        },
        Err(error) => FromCandidate::Failed {
            reason: error.to_string(),
        },
    };
    write_frame(&mut stream, &report).map_err(|e| format!("report: {e}"))?;
    // Abort, or O gone: either way this candidate is done.
    match read_word(&stream) {
        Ok((FromOld::Take, fds)) => take(socket, stream, fds),
        Ok((FromOld::Abort, _)) | Err(_) => Ok(()),
    }
}

/// Become the daemon: adopt O's listener and lock, claim the hosts, serve.
/// Returns only if that fails before serving, after telling O why.
fn take(socket: &Path, mut stream: UnixStream, fds: Vec<OwnedFd>) -> Result<(), String> {
    // O started this process and is alive until we serve.
    // SAFETY: getppid has no preconditions.
    let _ = crate::daemon::RELOADED_FROM.set(unsafe { libc::getppid() } as u32);
    let error = match serve_taken(socket, &stream, fds) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    let reason = error.clone();
    let _ = write_frame(&mut stream, &FromCandidate::Failed { reason });
    Err(error)
}

fn serve_taken(socket: &Path, stream: &UnixStream, fds: Vec<OwnedFd>) -> Result<(), String> {
    let [listener, lock] = <[OwnedFd; 2]>::try_from(fds)
        .map_err(|fds| format!("take carried {} fds, not 2", fds.len()))?;
    let mut reply = stream.try_clone().map_err(|e| format!("handoff fd: {e}"))?;
    // Outlive O: its session and process group are not this daemon's.
    // SAFETY: setsid has no memory effects; failure only means we lead one.
    unsafe { libc::setsid() };
    let fault = std::env::var(TEST_FAULT).ok();
    let serving = Box::new(move || {
        match fault.as_deref() {
            // Tests: fail after claiming the hosts, before serving.
            Some("exit-before-serving") => std::process::exit(1),
            Some("hang-before-serving") => loop {
                std::thread::sleep(Duration::from_secs(60));
            },
            _ => {}
        }
        let _ = write_frame(&mut reply, &FromCandidate::Serving);
    });
    let handed = crate::daemon::Handed {
        listener,
        lock,
        serving,
    };
    crate::daemon::serve_handed(socket, handed).map_err(|error| error.to_string())
}

/// O's next word and any fds sent with it.
fn read_word(stream: &UnixStream) -> std::io::Result<(FromOld, Vec<OwnedFd>)> {
    stream.set_read_timeout(None)?;
    let mut length = [0; 4];
    let mut fds = fds::recv(stream, &mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > FRAME_LIMIT {
        return Err(std::io::Error::other("handoff frame too large"));
    }
    let mut bytes = vec![0; length];
    fds.extend(fds::recv(stream, &mut bytes)?);
    let word = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    Ok((word, fds))
}

fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl on an fd this process owns.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn frame(message: &impl Serialize) -> std::io::Result<Vec<u8>> {
    let body = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    if body.len() > FRAME_LIMIT {
        return Err(std::io::Error::other("handoff frame too large"));
    }
    let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

fn write_frame(stream: &mut UnixStream, message: &impl Serialize) -> std::io::Result<()> {
    stream.write_all(&frame(message)?)?;
    stream.flush()
}

fn read_raw(stream: &mut UnixStream, until: Option<Instant>) -> std::io::Result<Vec<u8>> {
    let timeout = until.map(|until| {
        until
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1))
    });
    stream.set_read_timeout(timeout)?;
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > FRAME_LIMIT {
        return Err(std::io::Error::other("handoff frame too large"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// The next message from C, or why there is none: C exited, the deadline
/// passed, or it sent something else.
fn read_frame(
    stream: &mut UnixStream,
    child: &mut Child,
    until: Instant,
    awaited: &str,
) -> Result<FromCandidate, String> {
    match read_raw(stream, Some(until)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("candidate sent an unreadable {awaited}: {e}")),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Err(format!("candidate sent no {awaited} in time"))
        }
        Err(_) => {
            reap(child);
            let status = child
                .try_wait()
                .ok()
                .flatten()
                .map_or_else(|| "unknown status".into(), |status| status.to_string());
            Err(format!("candidate exited before its {awaited} ({status})"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(name: &str, build: Option<&str>, problem: Option<&str>) -> HostView {
        HostView {
            name: name.into(),
            build: build.map(str::to_owned),
            problem: problem.map(str::to_owned),
        }
    }

    #[test]
    fn a_host_we_drive_that_the_candidate_cannot_blocks_the_reload() {
        let report = compare(
            "2+b".into(),
            vec![
                view("a", Some("1+a"), None),
                view("b", Some("1+a"), None),
                view("dead", None, Some("refused")),
            ],
            vec![
                view("a", Some("1+a"), None),
                view("b", None, Some("incompatible")),
                view("dead", None, Some("refused")),
            ],
            &[],
        );
        assert_eq!(
            report.blockers,
            ["host b: the candidate cannot drive it (incompatible)"]
        );
        assert_eq!(
            report.lines()[0],
            "blocked: a reload into 2+b would not be safe"
        );
    }

    #[test]
    fn in_process_sessions_block_and_a_clean_fleet_is_ready() {
        let report = compare(
            "2+b".into(),
            vec![view("a", Some("1+a"), None)],
            vec![view("a", Some("1+a"), None)],
            &["shell".into()],
        );
        assert!(report.blockers[0].contains("in-process sessions would end: shell"));
        let report = compare(
            "2+b".into(),
            vec![view("a", Some("1+a"), None)],
            vec![],
            &[],
        );
        assert!(report.blockers[0].contains("not in the candidate's inventory"));
        let report = compare(
            "2+b".into(),
            vec![view("a", Some("1+a"), None)],
            vec![view("a", Some("1+a"), None)],
            &[],
        );
        assert!(report.blockers.is_empty());
        assert_eq!(
            report.lines(),
            [
                "ready: a reload into 2+b would keep every host",
                "host a: now 1+a; candidate sees 1+a"
            ]
        );
    }

    #[test]
    fn only_one_reload_runs_at_a_time() {
        let first = InFlight::take().unwrap();
        assert!(InFlight::take().is_err());
        drop(first);
        drop(InFlight::take().unwrap());
    }
}
