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

use crate::agent::{Cursor, Result, Size};
use crate::session::Session;
use core::time::Duration;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One session's state, copied out.
///
/// Owned data, never a borrow into the registry — a caller may be a viewer on
/// another machine, and this is what would go on the wire.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionSummary {
    pub name: String,
    pub alive: bool,
    pub idle: Duration,
    pub size: Size,
}

#[derive(Default)]
pub struct Registry {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take ownership of a session and hand back a shared handle.
    ///
    /// A name already in use is refused and the session is returned in `Err`
    /// **unregistered**. Replacing silently would drop the last handle to a
    /// live pty — a running agent with nobody able to reach it — and the
    /// caller would see success.
    pub fn register(&self, session: Session) -> core::result::Result<Arc<Session>, Session> {
        let mut sessions = self.lock();
        if sessions.contains_key(session.name()) {
            return Err(session);
        }
        let handle = Arc::new(session);
        sessions.insert(handle.name().to_string(), Arc::clone(&handle));
        Ok(handle)
    }

    /// A handle to a live session. Many callers may hold one at once: that is
    /// what lets a viewer attach while the core keeps driving.
    pub fn get(&self, name: &str) -> Option<Arc<Session>> {
        self.lock().get(name).map(Arc::clone)
    }

    /// A snapshot of every session, sorted by name so callers can diff two
    /// listings without sorting first.
    pub fn list(&self) -> Vec<SessionSummary> {
        let mut out: Vec<_> = self
            .lock()
            .values()
            .map(|s| SessionSummary {
                name: s.name().to_string(),
                alive: s.is_alive(),
                idle: s.idle_for(),
                size: s.size(),
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
        let mut sessions = self.lock();
        let dead: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| !s.is_alive())
            .map(|(n, _)| n.clone())
            .collect();
        for name in &dead {
            sessions.remove(name);
        }
        dead
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

/// Deliver one instruction to a named session.
///
/// Sugar over `get` + `send_line`, and the shape a remote call would take:
/// name in, result out, no handle crossing the boundary.
impl Registry {
    pub fn send_line(&self, name: &str, text: &str) -> Option<Result<()>> {
        self.get(name).map(|s| s.send_line(text))
    }

    pub fn screen_text(&self, name: &str) -> Option<Result<String>> {
        self.get(name).map(|s| s.screen_text())
    }

    pub fn cursor(&self, name: &str) -> Option<Result<Cursor>> {
        self.get(name).map(|s| s.cursor())
    }
}
