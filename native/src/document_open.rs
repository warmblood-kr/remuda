//! Opt-in local ADE document delivery using the existing VFS transport.
use crate::{documents::Browser, fs_lock};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io,
    path::Path,
    sync::{mpsc, Arc},
};

mod mailbox;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Open {
    pub instance: String,
    pub seq: u64,
    pub uri: String,
    pub expires: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Receipt {
    pub instance: String,
    pub seq: u64,
    pub uri: String,
    pub status: String,
    pub identity: Option<(u64, String)>,
}

pub struct Inbox {
    _lock: Arc<File>,
    instance: String,
    last_seq: u64,
    incoming: mpsc::Receiver<Open>,
    outgoing: mpsc::SyncSender<Receipt>,
    active: Option<(Open, u64)>,
}

impl Inbox {
    /// No environment opt-in means no mailbox, worker, or change to normal UI.
    pub fn from_env(socket: &Path) -> io::Result<Option<Self>> {
        let Some(target) = std::env::var_os("REMUDA_DOCUMENT_TARGET") else {
            return Ok(None);
        };
        if !cfg!(unix) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "document delivery is Unix-only",
            ));
        }
        let target = target
            .to_str()
            .ok_or_else(|| io::Error::other("invalid document target"))?;
        Self::start(socket, target).map(Some)
    }

    fn start(socket: &Path, target: &str) -> io::Result<Self> {
        if target.is_empty()
            || target.len() > 64
            || !target
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(io::Error::other(
                "document target must be 1–64 ASCII letters, digits or '-'",
            ));
        }
        let (node, _) = crate::cluster::nodes()?
            .ok_or_else(|| io::Error::other("document delivery needs a local cluster identity"))?;
        let directory = crate::cluster::storage::cluster_state_dir()?.join("cluster");
        let lock_path = directory.join(format!("document-{target}.lock"));
        let fs_lock::Outcome::Acquired(lock) =
            fs_lock::acquire(&lock_path, &fs_lock::info_line(target))?
        else {
            return Err(io::Error::other(
                "document target already belongs to another ADE",
            ));
        };
        let lock = Arc::new(lock);
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|e| io::Error::other(e.to_string()))?;
        let instance: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let (incoming, outgoing) =
            mailbox::start(socket, target, &instance, &node.node_fp, lock.clone())?;
        Ok(Self {
            _lock: lock,
            instance,
            last_seq: 0,
            incoming,
            outgoing,
            active: None,
        })
    }

    /// Exact boot identity and increasing sequence numbers prevent stale replay.
    pub fn take(&mut self) -> Option<Open> {
        let request = self.incoming.try_recv().ok()?;
        if request.instance != self.instance || request.seq == 0 || request.seq <= self.last_seq {
            return None;
        }
        self.last_seq = request.seq;
        let now = mailbox::now();
        if request.expires <= now || request.expires > now.saturating_add(300) {
            self.report(&request, "expired", None);
            return None;
        }
        let Some(path) = request.uri.strip_prefix("remuda:///") else {
            self.report(&request, "invalid_uri", None);
            return None;
        };
        if remuda_core::vfs::validate_path(path).is_err() || path.starts_with("ade-open/") {
            self.report(&request, "invalid_uri", None);
            return None;
        }
        self.report(&request, "received", None);
        if let Some((prior, _)) = self.active.take() {
            self.report(&prior, "superseded", None);
        }
        Some(request)
    }

    pub fn track(&mut self, request: Open, generation: u64) {
        self.active = Some((request, generation));
    }

    /// Called only after the terminal frame was successfully written/flushed.
    /// This proves presentation to the terminal, not human attention.
    pub fn presented(&mut self, browser: Option<&Browser>, visible: bool, has_body: bool) {
        let Some((request, generation)) = self.active.as_ref() else {
            return;
        };
        let status = match browser {
            Some(b) if !visible || b.generation() != *generation => "superseded",
            Some(b) if b.loading => return,
            Some(b) if b.page.error.is_some() => "load_failed",
            Some(b) if b.page.identity.is_some() && has_body => "rendered",
            Some(_) => return,
            None => "superseded",
        };
        let identity = if status == "rendered" {
            browser.and_then(|b| b.page.identity.clone())
        } else {
            None
        };
        let request = request.clone();
        self.report(&request, status, identity);
        self.active = None;
    }

    fn report(&self, request: &Open, status: &str, identity: Option<(u64, String)>) {
        // A full/disconnected result queue means unknown delivery, never success.
        let _ = self.outgoing.try_send(Receipt {
            instance: self.instance.clone(),
            seq: request.seq,
            uri: request.uri.clone(),
            status: status.into(),
            identity,
        });
    }
}
