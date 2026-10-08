//! Every session on this node, addressable by name.
//!
//! # This is the local half of a cluster directory
//!
//! 정수님, 2026-09-10: a node started as a host prints an address and a join
//! token; other nodes join with it and a circle forms; **any** node — including
//! one that hosts nothing and is a pure remote terminal, possibly a browser —
//! can then attach a session living on any other node, and detach again.
//!
//! That does not change this type. A session's pty exists on exactly one
//! machine, which is a fact rather than something nodes must agree on, so the
//! cluster needs a *directory* ("session X is on node B") and not a consensus
//! protocol: no Raft, no etcd. A stale directory entry costs a failed connect
//! and a re-ask; it can never make two nodes run the same session. The remote
//! directory is this registry with a node address beside each name, so getting
//! this one right is the whole first step.
//!
//! What is deliberately absent: any notion of a node, an address, or a
//! transport. Adding those here would put a socket in the policy layer, which
//! `core/clippy.toml` denies outright.

use crate::agent::{AgentError, Cursor, ExitInfo, Result, ScreenSnapshot, Size, StyledCell};
use crate::input::{InputBatch, InputError, InputOutcome};
use crate::protocol::Step;
use crate::session::Session;
use core::time::Duration;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Immutable process attribution captured from one registered session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionAttribution {
    pub name: String,
    pub instance_id: String,
}

/// One live session process and the identity of the launch that owns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSessionProcess {
    pub attribution: SessionAttribution,
    pub pid: u32,
}

/// One session's state, copied out. Owned data, never a borrow into the
/// registry — the caller may be a viewer on another machine.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Stable identity for this daemon lifetime; unlike `name`, it is never reused.
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// Unique to this particular start, even when a later process reuses its name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Increases for each processed PTY output chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_version: Option<u64>,
    pub alive: bool,
    /// Time since the last accepted input; unchanged by agent output.
    pub idle: Duration,
    /// Time since the agent last produced output. `None` from older daemons.
    #[serde(default)]
    pub output_idle: Option<Duration>,
    pub size: Size,
    /// Whether a human holds it right now. A fact about the terminal, not about
    /// the session's job — see the scope line in `steps/012`.
    pub attached: bool,
    /// Time since an attached human last typed; `None` if none has (#136).
    /// Defaulted so a listing from an older daemon still parses.
    #[serde(default)]
    pub human_idle: Option<Duration>,
    /// Whether the child currently requests mouse input from its terminal.
    /// Defaulted for sessions listed by an older daemon.
    #[serde(default)]
    pub mouse_tracking: bool,
}

/// A session name from a program's argv[0]: basename, lowercased, anything
/// outside `[a-z0-9_-]` folded to `-`. `"/usr/bin/zsh"` becomes `"zsh"`.
pub fn slug(command: &str) -> String {
    let base = command.rsplit(['/', '\\']).next().unwrap_or(command);
    let folded: String = base
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = folded.trim_matches('-');
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed.to_string()
    }
}

#[derive(Default)]
pub struct Registry {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl Registry {
    pub fn apply_input_batch(
        &self,
        name: &str,
        batch: InputBatch<'_>,
    ) -> Option<core::result::Result<InputOutcome, InputError>> {
        self.get(name)
            .map(|session| session.apply_input_batch(batch))
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Take ownership of a session and hand back a shared handle. Caution: a
    /// name already in use is refused, and the session comes back in `Err`
    /// **unregistered** — replacing would strand a live pty.
    #[allow(
        clippy::result_large_err,
        reason = "the refused session is handed back whole"
    )]
    pub fn register(&self, session: Session) -> core::result::Result<Arc<Session>, Session> {
        let mut sessions = self.lock();
        if sessions.contains_key(session.name()) {
            return Err(session);
        }
        let handle = Arc::new(session);
        sessions.insert(handle.name().to_string(), Arc::clone(&handle));
        Ok(handle)
    }

    /// `base`, or `base-2`, `base-3`… when it is taken. Caution: advisory — the
    /// lock is released before you build the session, so `register` is still
    /// the thing that decides, and it still refuses a collision.
    pub fn unique_name(&self, base: &str) -> String {
        let sessions = self.lock();
        let mut candidate = base.to_string();
        let mut n = 1u32;
        while sessions.contains_key(&candidate) {
            n += 1;
            candidate = format!("{base}-{n}");
        }
        candidate
    }

    /// A handle to a live session. Many callers may hold one at once: that is
    /// what lets a viewer attach while the core keeps driving.
    pub fn get(&self, name: &str) -> Option<Arc<Session>> {
        self.lock().get(name).map(Arc::clone)
    }

    /// Look up a tracked session identity without reaping exited sessions.
    pub fn name_for_id(&self, id: &str) -> Option<String> {
        self.lock()
            .values()
            .find(|session| session.id() == id)
            .map(|session| session.name().to_string())
    }

    /// Process IDs for the live session children. Exited sessions remain
    /// listed, but cannot identify an active caller.
    pub fn live_process_ids(&self) -> Vec<u32> {
        self.live_processes()
            .into_iter()
            .map(|(_, pid)| pid)
            .collect()
    }

    /// Names paired with the process IDs used to identify session ancestry.
    pub fn live_processes(&self) -> Vec<(String, u32)> {
        self.lock()
            .values()
            .filter_map(|session| {
                session
                    .process_id_if_alive()
                    .map(|pid| (session.name().to_string(), pid))
            })
            .collect()
    }

    /// Live process candidates with their launch identity captured together
    /// under the registry lock. Carry this value through ancestry resolution
    /// instead of looking the session name up again afterwards.
    pub fn live_processes_with_identity(&self) -> Vec<LiveSessionProcess> {
        self.lock()
            .values()
            .filter_map(|session| {
                session.process_id_if_alive().map(|pid| LiveSessionProcess {
                    attribution: SessionAttribution {
                        name: session.name().to_string(),
                        instance_id: session.instance_id().to_string(),
                    },
                    pid,
                })
            })
            .collect()
    }

    /// The session whose backend holds `pid` as its own. Every listed session
    /// is asked, also one whose child has exited: its descendants may live on.
    pub fn session_owning(
        &self,
        pid: u32,
        process_handle: Option<usize>,
    ) -> std::io::Result<Option<String>> {
        let mut unknown = false;
        for session in self.lock().values() {
            match session.owns_process(pid, process_handle) {
                Ok(true) => return Ok(Some(session.name().to_string())),
                Ok(false) => {}
                Err(_) => unknown = true,
            }
        }
        if unknown {
            Err(std::io::Error::other(
                "session process membership unavailable",
            ))
        } else {
            Ok(None)
        }
    }

    /// The immutable launch identity whose backend owns `pid`, resolved while
    /// the registry entry is locked. This includes dead owners because Windows
    /// job membership can outlive the session's primary process.
    pub fn session_attribution_owning(
        &self,
        pid: u32,
        process_handle: Option<usize>,
    ) -> std::io::Result<Option<SessionAttribution>> {
        let mut unknown = false;
        for session in self.lock().values() {
            match session.owns_process(pid, process_handle) {
                Ok(true) => {
                    return Ok(Some(SessionAttribution {
                        name: session.name().to_string(),
                        instance_id: session.instance_id().to_string(),
                    }));
                }
                Ok(false) => {}
                Err(_) => unknown = true,
            }
        }
        if unknown {
            Err(std::io::Error::other(
                "session process membership unavailable",
            ))
        } else {
            Ok(None)
        }
    }

    /// A snapshot of every session, sorted by name so callers can diff two
    /// listings without sorting first.
    pub fn list(&self) -> Vec<SessionSummary> {
        let mut out: Vec<_> = self
            .lock()
            .values()
            .map(|s| SessionSummary {
                id: s.id().to_string(),
                name: s.name().to_string(),
                instance_id: Some(s.instance_id().to_string()),
                output_version: s.output_version(),
                alive: s.is_alive(),
                idle: s.idle_for(),
                output_idle: Some(s.output_idle_for()),
                size: s.size(),
                attached: s.is_attached(),
                human_idle: s.human_idle_for(),
                mouse_tracking: s.mouse_tracking(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Stop tracking a session. The process is not killed — any handle already
    /// taken keeps working, which is what makes this safe to call on a viewer's
    /// behalf.
    pub fn remove(&self, name: &str) -> Option<Arc<Session>> {
        self.lock().remove(name)
    }

    /// Drop every session whose process has exited, returning their names.
    pub fn reap(&self) -> Vec<String> {
        self.reap_with_exit_info()
            .into_iter()
            .map(|(name, _, _, _, _)| name)
            .collect()
    }

    /// Drop exited sessions and retain any status their backend observed.
    pub fn reap_with_exit_info(
        &self,
    ) -> Vec<(String, String, String, &'static str, Option<ExitInfo>)> {
        let mut sessions = self.lock();
        let dead: Vec<_> = sessions
            .iter()
            .filter(|(_, s)| !s.is_alive())
            .map(|(name, session)| {
                (
                    name.clone(),
                    session.id().to_string(),
                    session.instance_id().to_string(),
                    if session.is_closing() {
                        "closed"
                    } else {
                        "exited"
                    },
                    Arc::clone(session),
                )
            })
            .collect();
        for (name, _, _, _, _) in &dead {
            sessions.remove(name);
        }
        drop(sessions);
        dead.into_iter()
            .map(|(name, id, instance_id, reason, session)| {
                if let Err(error) = session.terminate_for_reap() {
                    eprintln!("remuda core: failed to terminate reaped session {name}: {error}");
                }
                (name, id, instance_id, reason, session.exit_info())
            })
            .collect()
    }

    /// A panic under this lock cannot leave the map half-updated — an entry is
    /// either inserted or it is not — so the contents stay meaningful and
    /// recovering beats poisoning every later call.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Name-addressed sugar over `get` plus a session method — the shape a remote
/// call takes: name in, result out, no handle crossing the boundary.
impl Registry {
    pub fn send_line(&self, name: &str, text: &str) -> Option<Result<()>> {
        self.get(name).map(|s| s.send_line(text))
    }

    pub fn send(&self, name: &str, bytes: &[u8]) -> Option<Result<()>> {
        self.get(name).map(|s| s.send(bytes))
    }

    pub fn feed(&self, name: &str, steps: &[Step]) -> Option<Result<()>> {
        self.get(name).map(|s| s.feed(steps))
    }

    pub fn resize(&self, name: &str, size: Size) -> Option<Result<()>> {
        self.get(name).map(|s| s.resize(size))
    }

    pub fn screen_text(&self, name: &str) -> Option<Result<String>> {
        self.get(name).map(|s| s.screen_text())
    }

    pub fn screen_cells(&self, name: &str) -> Option<Result<Vec<Vec<StyledCell>>>> {
        self.get(name).map(|s| s.screen_cells())
    }

    pub fn screen_cells_at(
        &self,
        name: &str,
        scrollback: usize,
    ) -> Option<Result<Vec<Vec<StyledCell>>>> {
        self.get(name).map(|s| s.screen_cells_at(scrollback))
    }

    pub fn scrollback_len(&self, name: &str) -> Option<usize> {
        self.get(name).map(|s| s.scrollback_len())
    }

    pub fn scrollback_total(&self, name: &str) -> Option<usize> {
        self.get(name).map(|s| s.scrollback_total())
    }

    pub fn row_wrapped_at(&self, name: &str, scrollback: usize) -> Option<Result<Vec<bool>>> {
        self.get(name).map(|s| s.row_wrapped_at(scrollback))
    }

    pub fn screen_snapshot_at(
        &self,
        name: &str,
        scrollback: usize,
    ) -> Option<Result<ScreenSnapshot>> {
        self.get(name).map(|s| s.screen_snapshot_at(scrollback))
    }

    /// A screen and its output generation read within one session-lock interval.
    pub fn screen_snapshot_version_at(
        &self,
        name: &str,
        scrollback: usize,
    ) -> Option<Result<crate::agent::VersionedSnapshot>> {
        self.get(name)
            .map(|session| session.screen_snapshot_version_at(scrollback))
    }

    pub fn cursor(&self, name: &str) -> Option<Result<Cursor>> {
        self.get(name).map(|s| s.cursor())
    }

    /// End and stop tracking a session; an attached one refuses and stays.
    /// `Ok(false)`: a concurrent `reap` removed it first and owns the notice.
    pub fn close(&self, name: &str) -> Option<Result<bool>> {
        let session = {
            let sessions = self.lock();
            let session = Arc::clone(sessions.get(name)?);
            session.mark_closing();
            session
        };
        Some(match session.terminate_for_close() {
            Ok(()) => {
                let removed = self.remove(name).is_some();
                session.wake_sync_waiters();
                Ok(removed)
            }
            Err(error) => {
                session.clear_closing();
                Err(error)
            }
        })
    }

    /// Close only the session start named by `instance_id`. The registry lock
    /// covers identity validation, termination, and removal so a same-name
    /// replacement can never be closed by a stale request.
    pub fn close_instance(&self, name: &str, instance_id: &str) -> Option<Result<bool>> {
        let mut sessions = self.lock();
        let session = sessions.get(name)?.clone();
        if session.instance_id() != instance_id {
            return Some(Err(AgentError::Io(
                "session restarted; close was refused".into(),
            )));
        }
        session.mark_closing();
        if let Err(error) = session.terminate() {
            session.clear_closing();
            return Some(Err(error));
        }
        Some(Ok(sessions.remove(name).is_some()))
    }
}
