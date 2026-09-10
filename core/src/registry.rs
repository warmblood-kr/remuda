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
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One session's state, copied out. Owned data, never a borrow into the
/// registry — the caller may be a viewer on another machine.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub name: String,
    pub alive: bool,
    pub idle: Duration,
    pub size: Size,
    /// Whether a human holds it right now. A fact about the terminal, not about
    /// the session's job — see the scope line in `steps/012`.
    pub attached: bool,
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
    pub fn new() -> Self {
        Self::default()
    }

    /// Take ownership of a session and hand back a shared handle. Caution: a
    /// name already in use is refused, and the session comes back in `Err`
    /// **unregistered** — replacing would strand a live pty.
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
                attached: s.is_attached(),
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

/// Name-addressed sugar over `get` plus a session method — the shape a remote
/// call takes: name in, result out, no handle crossing the boundary.
impl Registry {
    pub fn send_line(&self, name: &str, text: &str) -> Option<Result<()>> {
        self.get(name).map(|s| s.send_line(text))
    }

    pub fn send(&self, name: &str, bytes: &[u8]) -> Option<Result<()>> {
        self.get(name).map(|s| s.send(bytes))
    }

    pub fn screen_text(&self, name: &str) -> Option<Result<String>> {
        self.get(name).map(|s| s.screen_text())
    }

    pub fn cursor(&self, name: &str) -> Option<Result<Cursor>> {
        self.get(name).map(|s| s.cursor())
    }

    /// End a session, live or already dead, and stop tracking it. Caution:
    /// terminate-then-remove in that order — an attached session refuses and
    /// keeps its entry, so a close cannot disconnect a human mid-drive.
    pub fn close(&self, name: &str) -> Option<Result<()>> {
        let session = self.get(name)?;
        Some(session.terminate().inspect(|()| {
            self.remove(name);
        }))
    }
}
