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
use crate::input::{ExpectedInstanceInputError, InputBatch, InputError, InputOutcome};
use crate::protocol::Step;
use crate::session::{InputSubmitOutcome, Session};
use crate::session_operations::{Lifecycle, SessionHandle};
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

/// Why a captured caller identity no longer names a live registry entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveInstanceError {
    NotRegistered,
    InstanceChanged,
    NotLive,
}

impl LiveInstanceError {
    pub fn message(self) -> &'static str {
        match self {
            Self::NotRegistered => "session is no longer registered",
            Self::InstanceChanged => "session instance changed",
            Self::NotLive => "session is no longer live",
        }
    }
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
    /// Host-backed sessions (#713). One name space with `sessions`; lock
    /// order is always `sessions`, then `hosted`.
    hosted: Mutex<HashMap<String, Arc<SessionHandle>>>,
    /// Host-backed launches (by instance id) a close has started ending, so a
    /// reaper that sees the child die first still reports "closed".
    closing_hosted: Mutex<std::collections::HashSet<String>>,
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

    pub fn apply_input_line_batch(
        &self,
        name: &str,
        batch: InputBatch<'_>,
    ) -> Option<core::result::Result<InputOutcome, InputError>> {
        self.get(name)
            .map(|session| session.apply_input_line_batch(batch))
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
        if sessions.contains_key(session.name()) || self.hosted_lock().contains_key(session.name())
        {
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
        let hosted = self.hosted_lock();
        let mut candidate = base.to_string();
        let mut n = 1u32;
        while sessions.contains_key(&candidate) || hosted.contains_key(&candidate) {
            n += 1;
            candidate = format!("{base}-{n}");
        }
        candidate
    }

    /// Track a checked host-backed handle under its own name. A name already
    /// used by either map is refused and the handle comes back unregistered.
    #[allow(
        clippy::result_large_err,
        reason = "the refused handle is handed back whole"
    )]
    pub fn register_hosted(
        &self,
        handle: SessionHandle,
    ) -> core::result::Result<Arc<SessionHandle>, SessionHandle> {
        let sessions = self.lock();
        let mut hosted = self.hosted_lock();
        let name = handle.identity().name.clone();
        if sessions.contains_key(&name) || hosted.contains_key(&name) {
            return Err(handle);
        }
        let handle = Arc::new(handle);
        hosted.insert(name, Arc::clone(&handle));
        Ok(handle)
    }

    /// A host-backed session by name. Legacy `get` never returns one.
    pub fn hosted(&self, name: &str) -> Option<Arc<SessionHandle>> {
        self.hosted_lock().get(name).map(Arc::clone)
    }

    /// Stop tracking a host-backed session; its host is not touched.
    pub fn remove_hosted(&self, name: &str) -> Option<Arc<SessionHandle>> {
        self.hosted_lock().remove(name)
    }

    /// A handle to a live session. Many callers may hold one at once: that is
    /// what lets a viewer attach while the core keeps driving.
    pub fn get(&self, name: &str) -> Option<Arc<Session>> {
        self.lock().get(name).map(Arc::clone)
    }

    /// Validate one caller launch atomically with removal and replacement.
    pub fn validate_live_instance(
        &self,
        name: &str,
        instance_id: &str,
    ) -> std::result::Result<(), LiveInstanceError> {
        let sessions = self.lock();
        let session = sessions.get(name).ok_or(LiveInstanceError::NotRegistered)?;
        if session.instance_id() != instance_id {
            return Err(LiveInstanceError::InstanceChanged);
        }
        if !session.is_alive() {
            return Err(LiveInstanceError::NotLive);
        }
        Ok(())
    }

    /// Look up a tracked session identity, legacy or host-backed, without
    /// reaping exited sessions. A host-backed id survives a daemon restart.
    pub fn name_for_id(&self, id: &str) -> Option<String> {
        let legacy = self
            .lock()
            .values()
            .find(|session| session.id() == id)
            .map(|session| session.name().to_string());
        legacy.or_else(|| {
            self.hosted_lock()
                .values()
                .find(|handle| handle.identity().id == id)
                .map(|handle| handle.identity().name.clone())
        })
    }

    /// Each host-backed session's host build, by name; `None` if unreachable.
    pub fn hosted_builds(&self) -> Vec<crate::protocol::HostBuild> {
        let mut builds: Vec<_> = self
            .hosted_handles()
            .iter()
            .map(|handle| crate::protocol::HostBuild {
                name: handle.identity().name.clone(),
                build: handle.inspect().ok().and_then(|seen| seen.host_build),
            })
            .collect();
        builds.sort_by(|a, b| a.name.cmp(&b.name));
        builds
    }

    /// Every host-backed handle, copied out so callers can inspect each host
    /// without holding the registry lock.
    pub fn hosted_handles(&self) -> Vec<Arc<SessionHandle>> {
        self.hosted_lock().values().map(Arc::clone).collect()
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
        let hosted: Vec<_> = self.hosted_lock().values().map(Arc::clone).collect();
        out.extend(hosted.iter().map(|handle| hosted_summary(handle)));
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

    /// Drop host-backed sessions (#713) whose child has exited, with what the
    /// host saw. Each host is asked outside the lock; an entry is removed
    /// only if it is still the handle that was asked, so a concurrent close
    /// or replacement keeps its own notice. An unreachable host is kept.
    pub fn reap_hosted_with_exit_info(
        &self,
    ) -> Vec<(String, String, String, &'static str, Option<ExitInfo>)> {
        let exited: Vec<_> = self
            .hosted_handles()
            .into_iter()
            .filter_map(|handle| {
                let observation = handle.inspect().ok()?;
                match observation.lifecycle {
                    Lifecycle::Exited(info) => Some((handle, observation.closing, info)),
                    Lifecycle::Running => None,
                }
            })
            .collect();
        let mut hosted = self.hosted_lock();
        exited
            .into_iter()
            .filter_map(|(handle, closing, info)| {
                let identity = handle.identity();
                let current = hosted
                    .get(&identity.name)
                    .is_some_and(|entry| Arc::ptr_eq(entry, &handle));
                if !current {
                    return None;
                }
                hosted.remove(&identity.name);
                let closed = self.closing_hosted_lock().remove(&identity.instance);
                Some((
                    identity.name.clone(),
                    identity.id.clone(),
                    identity.instance.clone(),
                    if closing || closed {
                        "closed"
                    } else {
                        "exited"
                    },
                    info,
                ))
            })
            .collect()
    }

    fn closing_hosted_lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashSet<String>> {
        self.closing_hosted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// A panic under this lock cannot leave the map half-updated — an entry is
    /// either inserted or it is not — so the contents stay meaningful and
    /// recovering beats poisoning every later call.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn hosted_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<SessionHandle>>> {
        self.hosted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run `local` on a legacy session, else `hosted` on a host-backed one.
    fn route<T>(
        &self,
        name: &str,
        local: impl FnOnce(&Session) -> Result<T>,
        hosted: impl FnOnce(&SessionHandle) -> crate::session_operations::Result<T>,
    ) -> Option<Result<T>> {
        if let Some(session) = self.get(name) {
            return Some(local(&session));
        }
        let handle = self.hosted(name)?;
        Some(hosted(&handle).map_err(crate::session_operations::Error::into_agent))
    }
}

/// The pre-write checks a fenced input makes on a host-backed session: the
/// handle is bound to one launch, so its identity is the instance to match.
fn hosted_instance_ready(
    handle: &SessionHandle,
    expected_instance: &str,
) -> core::result::Result<(), ExpectedInstanceInputError> {
    if handle.identity().instance != expected_instance {
        return Err(ExpectedInstanceInputError::WrongInstance);
    }
    match handle.inspect() {
        Ok(observation) if observation.lifecycle == Lifecycle::Running => Ok(()),
        _ => Err(ExpectedInstanceInputError::NotStarted),
    }
}

/// One host Inspect cut. An unreachable host stays listed as not exited:
/// the inventory says it is registered; its operations answer Unavailable.
fn hosted_summary(handle: &SessionHandle) -> SessionSummary {
    let identity = handle.identity();
    let mut summary = SessionSummary {
        id: identity.id.clone(),
        name: identity.name.clone(),
        instance_id: Some(identity.instance.clone()),
        output_version: None,
        alive: true,
        idle: Duration::ZERO,
        output_idle: None,
        size: Size::new(0, 0),
        attached: false,
        human_idle: None,
        mouse_tracking: false,
    };
    if let Ok(observation) = handle.inspect() {
        summary.output_version = observation.output_version;
        summary.alive = observation.lifecycle == Lifecycle::Running;
        summary.idle = observation.idle;
        summary.output_idle = Some(observation.output_idle);
        summary.size = observation.size;
        summary.attached = observation.attached;
        summary.human_idle = observation.human_idle;
        summary.mouse_tracking = observation.mouse.mode != crate::agent::MouseMode::default();
    }
    summary
}

/// Name-addressed sugar over `get` plus a session method — the shape a remote
/// call takes: name in, result out, no handle crossing the boundary.
impl Registry {
    pub fn send_line(&self, name: &str, text: &str) -> Option<Result<()>> {
        self.route(name, |s| s.send_line(text), |h| h.send_line(text))
    }

    pub fn send(&self, name: &str, bytes: &[u8]) -> Option<Result<()>> {
        self.route(name, |s| s.send(bytes), |h| h.send(bytes))
    }

    pub fn clear_input(&self, name: &str, key: &[u8]) -> Option<Result<Option<String>>> {
        self.route(
            name,
            |s| s.clear_input(key),
            // Checked here too, so a host-backed session refuses in the same words.
            |h| {
                crate::session::check_clear_key(key)?;
                h.clear_input(key)
            },
        )
    }

    pub fn feed(&self, name: &str, steps: &[Step]) -> Option<Result<()>> {
        self.route(name, |s| s.feed(steps), |h| h.feed(steps))
    }

    /// Deliver text as one burst (bracketed paste when the child enabled it).
    pub fn input_text(&self, name: &str, text: &str) -> Option<Result<()>> {
        self.route(name, |s| s.input_text(text), |h| h.input_text(text))
    }

    /// Submit visible composer text that ends with `expect`.
    pub fn submit(&self, name: &str, expect: &str) -> Option<Result<InputSubmitOutcome>> {
        self.route(name, |s| s.submit(expect), |h| h.submit(expect))
    }

    /// Type text, wait `settle`, then submit it, as one act.
    pub fn type_text(
        &self,
        name: &str,
        text: &str,
        settle: Duration,
    ) -> Option<Result<InputSubmitOutcome>> {
        self.route(
            name,
            |s| s.type_text(text, settle),
            |h| h.type_text(text, settle),
        )
    }

    /// `type_text` pinned to one launch; `None` when no session has `name`.
    pub fn type_text_if_instance(
        &self,
        name: &str,
        expected_instance: &str,
        text: &str,
        settle: Duration,
    ) -> Option<core::result::Result<InputSubmitOutcome, ExpectedInstanceInputError>> {
        if let Some(session) = self.get(name) {
            return Some(session.type_text_if_instance(expected_instance, text, settle));
        }
        let handle = self.hosted(name)?;
        Some(
            hosted_instance_ready(&handle, expected_instance).and_then(|()| {
                handle
                    .type_text(text, settle)
                    .map_err(|_| ExpectedInstanceInputError::OutcomeUncertain)
            }),
        )
    }

    /// Exactly these key bytes, pinned to one launch.
    pub fn send_key_if_instance(
        &self,
        name: &str,
        expected_instance: &str,
        bytes: &[u8],
    ) -> Option<core::result::Result<(), ExpectedInstanceInputError>> {
        if let Some(session) = self.get(name) {
            return Some(session.send_key_if_instance(expected_instance, bytes));
        }
        let handle = self.hosted(name)?;
        Some(
            hosted_instance_ready(&handle, expected_instance).and_then(|()| {
                handle
                    .send(bytes)
                    .map_err(|_| ExpectedInstanceInputError::OutcomeUncertain)
            }),
        )
    }

    pub fn resize_instance(
        &self,
        name: &str,
        instance_id: &str,
        size: Size,
    ) -> Option<Result<bool>> {
        self.route(
            name,
            |session| session.resize_instance(instance_id, size),
            |handle| {
                if handle.identity().instance != instance_id {
                    return Ok(false);
                }
                handle.resize(size).map(|()| true)
            },
        )
    }

    pub fn resize(&self, name: &str, size: Size) -> Option<Result<()>> {
        self.route(name, |s| s.resize(size), |h| h.resize(size))
    }

    pub fn screen_text(&self, name: &str) -> Option<Result<String>> {
        self.route(name, |s| s.screen_text(), |h| h.screen_text())
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
        self.route(
            name,
            |session| session.screen_snapshot_version_at(scrollback),
            |handle| handle.snapshot(scrollback),
        )
    }

    pub fn cursor(&self, name: &str) -> Option<Result<Cursor>> {
        self.get(name).map(|s| s.cursor())
    }

    /// End and stop tracking a session; an attached one refuses and stays.
    /// `Ok(false)`: a concurrent `reap` removed it first and owns the notice.
    pub fn close(&self, name: &str) -> Option<Result<bool>> {
        let session = {
            let sessions = self.lock();
            let Some(session) = sessions.get(name).map(Arc::clone) else {
                drop(sessions);
                return self.close_hosted(name, None);
            };
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
        let Some(session) = sessions.get(name).cloned() else {
            drop(sessions);
            return self.close_hosted(name, Some(instance_id));
        };
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

    /// End a host-backed child, then stop tracking that exact handle. Host
    /// teardown (Stop, inventory retirement) belongs to the native owner.
    fn close_hosted(&self, name: &str, instance_id: Option<&str>) -> Option<Result<bool>> {
        let handle = self.hosted(name)?;
        if instance_id.is_some_and(|id| handle.identity().instance != id) {
            return Some(Err(AgentError::Io(
                "session restarted; close was refused".into(),
            )));
        }
        self.closing_hosted_lock()
            .insert(handle.identity().instance.clone());
        if let Err(error) = handle.terminate() {
            self.closing_hosted_lock()
                .remove(&handle.identity().instance);
            return Some(Err(error.into_agent()));
        }
        let mut hosted = self.hosted_lock();
        let removed = hosted
            .get(name)
            .is_some_and(|current| Arc::ptr_eq(current, &handle));
        if removed {
            hosted.remove(name);
        }
        // Lock order: `hosted`, then `closing_hosted`, as in the reaper.
        self.closing_hosted_lock()
            .remove(&handle.identity().instance);
        Some(Ok(removed))
    }
}

#[cfg(test)]
mod attribution_tests {
    use super::*;
    use crate::agent::{AgentProcess, Cursor, ScriptedAgent, Size};
    use crate::clock::ManualClock;
    use crate::session::Session;
    use std::sync::Arc;

    struct OwnershipAgent {
        owned: std::io::Result<bool>,
        alive: bool,
        pid: u32,
    }

    impl AgentProcess for OwnershipAgent {
        fn write(&mut self, _: &[u8]) -> crate::agent::Result<()> {
            Ok(())
        }
        fn screen_text(&mut self) -> crate::agent::Result<String> {
            Ok(String::new())
        }
        fn cursor(&mut self) -> crate::agent::Result<Cursor> {
            Ok(Cursor {
                row: 0,
                col: 0,
                visible: true,
            })
        }
        fn is_alive(&mut self) -> bool {
            self.alive
        }
        fn terminate(&mut self) -> crate::agent::Result<()> {
            self.alive = false;
            Ok(())
        }
        fn size(&self) -> Size {
            Size::default()
        }
        fn process_id(&self) -> Option<u32> {
            self.alive.then_some(self.pid)
        }
        fn owns_process(&self, _: u32, _: Option<usize>) -> std::io::Result<bool> {
            match &self.owned {
                Ok(value) => Ok(*value),
                Err(error) => Err(std::io::Error::new(error.kind(), error.to_string())),
            }
        }
    }

    fn registry_with(name: &str, agent: impl AgentProcess + 'static) -> (Registry, Arc<Session>) {
        let registry = Registry::new();
        let session = Session::new_with_id(
            name,
            format!("id-{name}"),
            Box::new(agent),
            Arc::new(ManualClock::new()),
        );
        let handle = registry.register(session).expect("register session");
        (registry, handle)
    }

    #[test]
    fn session_attribution_owning_returns_the_live_matched_launch() {
        let (registry, session) = registry_with(
            "work",
            OwnershipAgent {
                owned: Ok(true),
                alive: true,
                pid: 30,
            },
        );
        assert_eq!(
            registry.session_attribution_owning(40, Some(7)).unwrap(),
            Some(SessionAttribution {
                name: "work".into(),
                instance_id: session.instance_id().into(),
            })
        );
    }

    #[test]
    fn session_attribution_owning_retains_dead_owner_provenance() {
        let (registry, session) = registry_with(
            "work",
            OwnershipAgent {
                owned: Ok(true),
                alive: false,
                pid: 30,
            },
        );
        assert_eq!(
            registry.session_attribution_owning(40, Some(7)).unwrap(),
            Some(SessionAttribution {
                name: "work".into(),
                instance_id: session.instance_id().into(),
            })
        );
    }

    #[test]
    fn session_attribution_owning_propagates_query_errors() {
        let (registry, _) = registry_with(
            "work",
            OwnershipAgent {
                owned: Err(std::io::Error::other("ownership unavailable")),
                alive: true,
                pid: 30,
            },
        );
        assert!(registry.session_attribution_owning(40, Some(7)).is_err());
    }

    #[test]
    fn session_attribution_owning_returns_none_when_no_session_matches() {
        let (registry, _) = registry_with(
            "work",
            OwnershipAgent {
                owned: Ok(false),
                alive: true,
                pid: 30,
            },
        );
        assert_eq!(
            registry.session_attribution_owning(40, Some(7)).unwrap(),
            None
        );
    }

    #[test]
    fn session_attribution_owning_uses_the_matched_sessions_instance() {
        let registry = Registry::new();
        let first = Session::new_with_id(
            "first",
            "id-first",
            Box::new(OwnershipAgent {
                owned: Ok(false),
                alive: true,
                pid: 30,
            }),
            Arc::new(ManualClock::new()),
        );
        let second = Session::new_with_id(
            "second",
            "id-second",
            Box::new(ScriptedAgent::new(vec![]).owning(40)),
            Arc::new(ManualClock::new()),
        );
        registry.register(first).unwrap();
        let second = registry.register(second).unwrap();
        assert_eq!(
            registry
                .session_attribution_owning(40, None)
                .unwrap()
                .unwrap()
                .instance_id,
            second.instance_id()
        );
    }
}
