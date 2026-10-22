//! Explicit reserved bootstrap; no daemon launch selector or legacy rescue.
use super::api::{Identity, Manifest, PublicationStatus};
use super::entry::Launch;
use super::inventory::disk::Store;
use super::inventory::model::{
    hash, Error as InventoryError, Lease, Publication, Settled, TerminalEnd,
};
use super::owned::{Owned, Staged};
use super::server::Server;
use super::unix::Endpoint;
use portable_pty::CommandBuilder;
use remuda_core::Size;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::time::Duration;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bootstrap {
    pub store: PathBuf,
    pub lease: Lease,
    pub launch: Box<RawValue>,
}
pub(crate) struct Gate {
    status: AtomicU8,
    store: Arc<Store>,
    lease: Lease,
}
impl Gate {
    fn new(store: Arc<Store>, lease: Lease) -> Self {
        Self {
            status: AtomicU8::new(0),
            store,
            lease,
        }
    }
    fn set(&self, status: PublicationStatus) {
        self.status.store(status as u8, Ordering::Release);
    }
    pub fn status(&self) -> PublicationStatus {
        match self.status.load(Ordering::Acquire) {
            0 => PublicationStatus::Starting,
            1 => PublicationStatus::Ready,
            3 => PublicationStatus::Complete,
            _ => PublicationStatus::Unavailable,
        }
    }
    pub fn effects_allowed(&self) -> bool {
        matches!(
            self.status(),
            PublicationStatus::Ready | PublicationStatus::Complete
        )
    }
    /// One store read outside every host lock; the terminal fact is monotonic.
    pub fn settle(&self, identity: &Identity) -> Result<Settled, &'static str> {
        let index = self
            .store
            .read()
            .map_err(|_| "host_settlement_unavailable")?;
        let settled = self.lease.settled(&index).map_err(|error| match error {
            InventoryError::Busy => "host_retention_pending",
            _ => "host_settlement_unavailable",
        })?;
        if !settled.confirms(&self.lease, identity) {
            return Err("wrong_host_or_session_identity");
        }
        Ok(settled)
    }
}
fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
}
pub(crate) fn run(endpoint_path: &Path) -> io::Result<()> {
    let bytes = super::wire::read_bootstrap(&mut io::stdin().lock())?;
    let bootstrap: Bootstrap = serde_json::from_slice(&bytes).map_err(invalid)?;
    let launch: Launch = serde_json::from_str(bootstrap.launch.get()).map_err(invalid)?;
    super::entry::validate(&launch)?;
    let store = Arc::new(
        Store::open(&bootstrap.store)
            .map_err(|error| invalid(format!("inventory unavailable: {error:?}")))?,
    );
    let index = store
        .read()
        .map_err(|error| invalid(format!("inventory unavailable: {error:?}")))?;
    let (name, identity) = bootstrap
        .lease
        .launch_binding(
            &index,
            endpoint_path
                .to_str()
                .ok_or_else(|| invalid("endpoint encoding"))?,
            hash(bootstrap.launch.get().as_bytes()),
        )
        .map_err(|error| invalid(format!("reservation refused: {error:?}")))?;
    if name != launch.name {
        return Err(invalid("launch name binding"));
    }
    let manifest = Manifest {
        schema: 1,
        identity: identity.clone(),
        name: name.clone(),
        agent: launch.agent.clone(),
        cwd: launch.cwd.clone(),
    };
    let manifest = serde_json::to_vec(&manifest).map_err(invalid)?;
    if manifest.len() > 8192 {
        return Err(invalid("manifest budget"));
    }
    if unsafe { libc::getsid(0) } != unsafe { libc::getpid() } && unsafe { libc::setsid() } < 0 {
        return Err(io::Error::last_os_error());
    }
    // Unique root claim precedes every child effect; never remove/rebind a root.
    let endpoint = Endpoint::bind(endpoint_path)?;
    endpoint.nonblocking()?;
    let gate = Arc::new(Gate::new(store.clone(), bootstrap.lease.clone()));
    let (start, receive) = std::sync::mpsc::sync_channel::<Start>(1);
    // Reserve the essential worker before creating the child. Failure is no-spawn.
    let _publisher = std::thread::Builder::new()
        .name("host-publication".into())
        .spawn(move || {
            if let Ok(start) = receive.recv() {
                publish(start);
            }
        })?;
    #[cfg(test)]
    tests::cut("endpoint");
    let mut command = CommandBuilder::new(&launch.argv[0]);
    command.args(&launch.argv[1..]);
    command.cwd(&launch.cwd);
    command.env_clear();
    for (key, value) in launch.env {
        command.env(key, value);
    }
    let staged = Staged::new(
        Owned::spawn_reserved(
            identity,
            name,
            command,
            Size::for_viewport(launch.cols, launch.rows),
        )
        .map_err(invalid)?,
    );
    #[cfg(test)]
    tests::cut("spawned");
    let host = staged.publish();
    let server = Arc::new(Server::with_publication(
        endpoint,
        host.clone(),
        gate.clone(),
    ));
    let publication = Start {
        store,
        lease: bootstrap.lease,
        host,
        root: endpoint_path.into(),
        manifest,
        gate: gate.clone(),
    };
    if start.send(publication).is_err() {
        gate.set(PublicationStatus::Unavailable);
    }
    // Post-spawn publication/ACK failures preserve the host and its PTY drain.
    server.run()
}
struct Start {
    store: Arc<Store>,
    lease: Lease,
    host: Arc<Owned>,
    root: PathBuf,
    manifest: Vec<u8>,
    gate: Arc<Gate>,
}
fn commit(start: &Start, publication: Publication) -> bool {
    let result = (|| {
        let revision = start.store.read()?.cursor();
        start.store.publish(
            revision,
            &start.lease,
            publication,
            Duration::from_millis(250),
        )
    })();
    result.is_ok()
}
fn pause(start: &Start, delay: &mut Duration) {
    start.gate.set(PublicationStatus::Unavailable);
    std::thread::sleep(*delay);
    *delay = (*delay * 2).min(Duration::from_secs(1));
}
fn publish(start: Start) {
    let mut delay = Duration::from_millis(10);
    loop {
        if crate::fs_atomic::write_atomic_lua_private(
            &start.root.join("session.json"),
            &start.manifest,
        )
        .is_ok()
        {
            #[cfg(test)]
            tests::cut("manifest");
            #[cfg(test)]
            tests::ready_fault_once();
            if commit(&start, Publication::Ready) {
                break;
            }
        }
        pause(&start, &mut delay);
    }
    start.gate.set(PublicationStatus::Ready);
    #[cfg(test)]
    tests::cut("ready");
    let bytes = start.manifest.clone();
    let _ = std::thread::Builder::new()
        .name("host-reserved-ack".into())
        .spawn(move || {
            let mut output = io::stdout().lock();
            let _ = super::wire::write_frame(&mut output, &bytes).and_then(|()| output.flush());
        });
    lifecycle(start);
}
#[cfg(test)]
#[path = "birth_tests.rs"]
mod tests;
fn lifecycle(start: Start) {
    let mut exit = None;
    let mut end = None;
    let mut exit_committed = false;
    let mut end_committed = false;
    let mut delay = Duration::from_millis(10);
    loop {
        let Ok(info) = start.host.inspect() else {
            pause(&start, &mut delay);
            continue;
        };
        if exit.is_none() && (info.exit_code.is_some() || info.signal.is_some()) {
            exit = Some(Publication::Exited {
                generation: info.generation,
                code: info.exit_code,
                signal: info.signal,
            });
        }
        if end.is_none() {
            let kind = match info.output_end {
                super::api::OutputEnd::Open => None,
                super::api::OutputEnd::Eof => Some(TerminalEnd::Eof),
                super::api::OutputEnd::Hangup => Some(TerminalEnd::Hangup),
                super::api::OutputEnd::ReadFailure => Some(TerminalEnd::ReadFailure),
            };
            end = kind.map(|kind| Publication::TerminalEnd {
                generation: info.drained_generation.unwrap_or(info.generation),
                kind,
            });
        }
        if let Some(publication) = exit {
            if !exit_committed {
                exit_committed = commit(&start, publication);
            }
        }
        if let Some(publication) = end {
            if !end_committed {
                end_committed = commit(&start, publication);
            }
        }
        if exit_committed && end_committed && info.output_eof {
            start.gate.set(PublicationStatus::Complete);
            break;
        }
        if info.backend_fault || matches!(info.output_end, super::api::OutputEnd::ReadFailure) {
            start.gate.set(PublicationStatus::Unavailable);
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if (exit.is_some() && !exit_committed) || (end.is_some() && !end_committed) {
            pause(&start, &mut delay);
        } else {
            start.gate.set(PublicationStatus::Ready);
            delay = Duration::from_millis(10);
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
