//! Local, owner-only audit log of what admitted peers asked this node to do.
//!
//! One JSON object per line in `cluster/audit.jsonl`. Only actions that change
//! a session are recorded: input, resize, close and create. Reads (List, Sync,
//! capture) are not. Input contents and lengths are never recorded. The file
//! keeps the most recent [`MAX_ENTRIES`] lines; older lines are trimmed in one
//! atomic rewrite once [`TRIM_SLACK`] extra lines have been appended.
//!
//! Recording is best effort: a write failure is reported once on stderr and
//! never refuses the peer's request.
use remuda_core::protocol::{Request, Response};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, BufRead, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const AUDIT_FILE: &str = "audit.jsonl";
pub const MAX_ENTRIES: usize = 10_000;
pub const TRIM_SLACK: usize = 1_000;

/// What the peer asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Input,
    Resize,
    Close,
    Create,
}

/// What this node did with the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Forwarded to the daemon, which did not answer with an error.
    Done,
    /// Forwarded to the daemon, which answered with an error or timed out.
    Failed,
    /// Refused before dispatch by policy (remote control off, allowlist).
    Refused,
    /// Refused before dispatch by a rate or capacity limit.
    Limited,
    /// The peer was revoked while the request was held.
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub at_unix_ms: u64,
    /// Authenticated peer fingerprint (the Noise static key), never a label.
    pub peer: String,
    pub op: Op,
    /// Session name as the peer sent it; empty for an unnamed create.
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    pub outcome: Outcome,
}

/// The audited part of a request: everything except input bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub op: Op,
    pub session: String,
    pub instance_id: Option<String>,
}

/// Return the audit record for a session-changing request, `None` for reads
/// and for cluster protocol traffic.
pub fn action(request: &Request) -> Option<Action> {
    let (op, session, instance_id) = match request {
        Request::Input {
            name, instance_id, ..
        }
        | Request::InputLine {
            name, instance_id, ..
        } => (Op::Input, name.clone(), Some(instance_id.clone())),
        Request::Send { name, .. }
        | Request::SendLine { name, .. }
        | Request::Feed { name, .. }
        | Request::ClearInput { name, .. } => (Op::Input, name.clone(), None),
        Request::ResizeSession {
            name, instance_id, ..
        } => (Op::Resize, name.clone(), Some(instance_id.clone())),
        Request::Resize { name, .. } => (Op::Resize, name.clone(), None),
        Request::Close {
            name, instance_id, ..
        } => (Op::Close, name.clone(), instance_id.clone()),
        Request::New { name, .. } | Request::NewWithInstance { name, .. } => {
            (Op::Create, name.clone().unwrap_or_default(), None)
        }
        _ => return None,
    };
    Some(Action {
        op,
        session,
        instance_id,
    })
}

/// Classify a daemon reply that reached the listener.
pub fn outcome_of_reply(reply: &[u8]) -> Outcome {
    match serde_json::from_slice::<Response>(reply) {
        Ok(Response::Error(_))
        | Ok(Response::RemoteControlDisabled)
        | Ok(Response::RateLimited)
        | Err(_) => Outcome::Failed,
        Ok(_) => Outcome::Done,
    }
}

/// Where entries go. The listener holds one; tests use [`AuditSink::Off`] or
/// a temporary directory so they never touch the user's state.
pub enum AuditSink {
    Off,
    File(Mutex<AuditFile>),
}

impl AuditSink {
    /// The production log under the cluster state directory, opened lazily.
    pub fn default_state_dir() -> Self {
        match super::storage::cluster_state_dir() {
            Ok(dir) => Self::at(dir.join("cluster")),
            Err(error) => {
                eprintln!("remuda: cluster audit log disabled: {error}");
                Self::Off
            }
        }
    }

    pub fn at(cluster_dir: impl Into<PathBuf>) -> Self {
        Self::File(Mutex::new(AuditFile::new(cluster_dir.into())))
    }

    pub fn record(&self, peer: &str, action: &Action, outcome: Outcome) {
        let Self::File(file) = self else { return };
        let entry = Entry {
            at_unix_ms: now_unix_ms(),
            peer: peer.into(),
            op: action.op,
            session: action.session.clone(),
            instance_id: action.instance_id.clone(),
            outcome,
        };
        let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) = file.append(&entry) {
            if !file.warned {
                file.warned = true;
                eprintln!("remuda: cluster audit log write failed: {error}");
            }
        }
    }
}

pub struct AuditFile {
    dir: PathBuf,
    handle: Option<File>,
    lines: usize,
    warned: bool,
}

impl AuditFile {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            handle: None,
            lines: 0,
            warned: false,
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.join(AUDIT_FILE)
    }

    fn append(&mut self, entry: &Entry) -> io::Result<()> {
        if self.handle.is_none() {
            let mut file = open_private(&self.path())?;
            self.lines = count_lines(&mut file)?;
            self.handle = Some(file);
        }
        let mut line = serde_json::to_vec(entry).map_err(io::Error::other)?;
        line.push(b'\n');
        let file = self.handle.as_mut().expect("opened above");
        file.seek(io::SeekFrom::End(0))?;
        if let Err(error) = file.write_all(&line) {
            self.handle = None;
            return Err(error);
        }
        self.lines += 1;
        if self.lines > MAX_ENTRIES + TRIM_SLACK {
            self.trim()?;
        }
        Ok(())
    }

    fn trim(&mut self) -> io::Result<()> {
        let path = self.path();
        let kept = read_last(&path, MAX_ENTRIES)?;
        let mut bytes = Vec::new();
        for line in &kept {
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
        }
        self.handle = None;
        super::storage::atomic_write(&path, &bytes)?;
        self.lines = kept.len();
        Ok(())
    }
}

/// Read up to `limit` most recent entries; malformed lines are skipped.
pub fn read_at(cluster_dir: &Path, limit: usize) -> io::Result<Vec<Entry>> {
    let path = cluster_dir.join(AUDIT_FILE);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    Ok(read_last(&path, limit)?
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

fn read_last(path: &Path, limit: usize) -> io::Result<Vec<String>> {
    let mut file = open_private(path)?;
    file.seek(io::SeekFrom::Start(0))?;
    let mut lines = std::collections::VecDeque::with_capacity(limit.min(MAX_ENTRIES) + 1);
    for line in io::BufReader::new(file).lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        lines.push_back(line);
        if lines.len() > limit {
            lines.pop_front();
        }
    }
    Ok(lines.into())
}

fn count_lines(file: &mut File) -> io::Result<usize> {
    file.seek(io::SeekFrom::Start(0))?;
    let mut count = 0;
    for line in io::BufReader::new(&*file).split(b'\n') {
        if !line?.is_empty() {
            count += 1;
        }
    }
    Ok(count)
}

fn open_private(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    let file = super::windows_security::create_or_open_lock(path)?;
    #[cfg(not(windows))]
    let file = {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        options.open(path)?
    };
    super::storage::check_private_file(&file, "cluster audit log", path)?;
    Ok(file)
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::Size;

    fn temp_dir() -> PathBuf {
        static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "remuda-audit-test-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        dir
    }

    fn input(bytes: &[u8]) -> Request {
        Request::Input {
            name: "build".into(),
            instance_id: "inst-1".into(),
            client_id: "c".repeat(32),
            seq: 1,
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn session_changing_requests_are_audited_and_reads_are_not() {
        assert_eq!(action(&input(b"x")).unwrap().op, Op::Input);
        assert_eq!(
            action(&Request::ResizeSession {
                name: "build".into(),
                instance_id: "inst-1".into(),
                size: Size::new(80, 24),
            })
            .unwrap()
            .op,
            Op::Resize
        );
        assert_eq!(
            action(&Request::Close {
                name: "build".into(),
                instance_id: Some("inst-1".into()),
                confirm: Some(true),
            })
            .unwrap()
            .op,
            Op::Close
        );
        assert_eq!(
            action(&Request::New {
                name: None,
                command: vec!["sh".into()],
                size: Size::new(80, 24),
                cwd: None,
                env: None,
            })
            .unwrap(),
            Action {
                op: Op::Create,
                session: String::new(),
                instance_id: None
            }
        );
        assert!(action(&Request::List).is_none());
        assert!(action(&Request::CaptureStyled {
            name: "build".into(),
            scrollback: 0
        })
        .is_none());
    }

    #[test]
    fn input_contents_are_never_written() {
        let dir = temp_dir();
        let sink = AuditSink::at(&dir);
        let secret = b"hunter2-password";
        sink.record(
            "SHA256:peer",
            &action(&input(secret)).unwrap(),
            Outcome::Done,
        );
        let text = std::fs::read_to_string(dir.join(AUDIT_FILE)).unwrap();
        assert!(!text.contains("hunter2"));
        assert!(!text.contains("bytes"));
        let entries = read_at(&dir, 10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].peer, "SHA256:peer");
        assert_eq!(entries[0].op, Op::Input);
        assert_eq!(entries[0].session, "build");
        assert_eq!(entries[0].instance_id.as_deref(), Some("inst-1"));
        assert_eq!(entries[0].outcome, Outcome::Done);
        assert!(entries[0].at_unix_ms > 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn log_keeps_only_the_most_recent_entries() {
        let dir = temp_dir();
        let sink = AuditSink::at(&dir);
        let total = MAX_ENTRIES + TRIM_SLACK + 1;
        for index in 0..total {
            let action = Action {
                op: Op::Resize,
                session: format!("s{index}"),
                instance_id: None,
            };
            sink.record("SHA256:peer", &action, Outcome::Done);
        }
        let entries = read_at(&dir, usize::MAX).unwrap();
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries.last().unwrap().session, format!("s{}", total - 1));
        assert_eq!(
            entries.first().unwrap().session,
            format!("s{}", total - MAX_ENTRIES)
        );
        // A fresh sink counts what is already there and keeps appending.
        AuditSink::at(&dir).record(
            "SHA256:peer",
            &Action {
                op: Op::Close,
                session: "after".into(),
                instance_id: None,
            },
            Outcome::Refused,
        );
        let entries = read_at(&dir, 1).unwrap();
        assert_eq!(entries[0].session, "after");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn log_file_is_owner_only_and_symlinks_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let action = action(&input(b"x")).unwrap();
        AuditSink::at(&dir).record("SHA256:peer", &action, Outcome::Done);
        let mode = std::fs::metadata(dir.join(AUDIT_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        let other = temp_dir();
        let outside = other.join("outside");
        std::fs::write(&outside, b"").unwrap();
        std::os::unix::fs::symlink(&outside, other.join(AUDIT_FILE)).unwrap();
        AuditSink::at(&other).record("SHA256:peer", &action, Outcome::Done);
        assert_eq!(std::fs::read(&outside).unwrap(), b"");
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn replies_are_classified() {
        let done = serde_json::to_vec(&Response::Sessions(Vec::new())).unwrap();
        assert_eq!(outcome_of_reply(&done), Outcome::Done);
        let failed = serde_json::to_vec(&Response::error("nope")).unwrap();
        assert_eq!(outcome_of_reply(&failed), Outcome::Failed);
        assert_eq!(outcome_of_reply(b"not json"), Outcome::Failed);
    }

    #[test]
    fn off_sink_writes_nothing() {
        AuditSink::Off.record("SHA256:peer", &action(&input(b"x")).unwrap(), Outcome::Done);
    }
}
