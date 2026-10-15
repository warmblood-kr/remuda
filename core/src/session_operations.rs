//! Checked high-level seam. Registry still accepts only Local Session owners.
//! A Remote implementation translates complete operations; it never owns an
//! AgentProcess or a second input/dedup/human clock. No runtime selects it yet.
//!
//! ```compile_fail
//! use remuda_core::{Registry, session_operations::SessionHandle};
//! fn cannot_register_remote(registry: &Registry, handle: SessionHandle) {
//!     registry.register(handle);
//! }
//! ```
//!
//! ```compile_fail
//! use remuda_core::session_operations::{SessionHandle, SessionOperations};
//! use std::sync::Arc;
//! fn no_unverified_production_constructor(backend: Arc<dyn SessionOperations>) {
//!     let _ = SessionHandle::remote(backend);
//! }
//! ```
use crate::agent::{AgentError, ExitInfo, MouseState, OutputWakeup, Size, VersionedSnapshot};
use crate::input::{InputBatch, InputError, InputOutcome};
use crate::protocol::Step;
use crate::session::{Attachment, InputSubmitOutcome, Session};
use std::borrow::Borrow;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub host: Option<[u8; 16]>,
    pub id: String,
    pub name: String,
    pub instance: String,
}
impl Identity {
    fn local(session: &Session) -> Self {
        Self {
            host: None,
            id: session.id().into(),
            name: session.name().into(),
            instance: session.instance_id().into(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || self.name.is_empty()
            || self.instance.is_empty()
            || self.id.len() > 256
            || self.name.len() > 64
            || self.instance.len() > 256
            || [&self.id, &self.name, &self.instance]
                .iter()
                .any(|s| s.chars().any(char::is_control))
        {
            return Err(Error::Protocol("invalid immutable session identity"));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    Recovering,
    Transport,
    Authentication,
    Incompatible,
    Unsupported,
}
#[derive(Debug)]
pub enum Error {
    Unavailable(Unavailable),
    /// A mutating request may have been admitted. Recover its stable request
    /// identity; never allocate a replacement request or replay automatically.
    Indeterminate(Box<RequestIdentity>),
    Forgotten(Box<RequestIdentity>),
    Agent(AgentError),
    Input(InputError),
    Protocol(&'static str),
}
pub type Result<T> = core::result::Result<T, Error>;
impl From<AgentError> for Error {
    fn from(error: AgentError) -> Self {
        Self::Agent(error)
    }
}
impl From<InputError> for Error {
    fn from(error: InputError) -> Self {
        Self::Input(error)
    }
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(f, "session owner unavailable: {reason:?}"),
            Self::Indeterminate(_) => {
                f.write_str("input outcome unknown; recover the original request before retry")
            }
            Self::Forgotten(_) => f.write_str("input outcome was forgotten; replay is refused"),
            Self::Agent(error) => error.fmt(f),
            Self::Input(error) => error.fmt(f),
            Self::Protocol(reason) => write!(f, "session protocol refused: {reason}"),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestIdentity {
    pub session: Identity,
    pub client: [u8; 16],
    pub sequence: u64,
    pub payload: [u8; 32],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Rejected,
    /// OS write/flush, never proof that the child executed the input.
    Written,
    Indeterminate,
}
#[derive(Debug)]
pub enum RecoveredReply {
    Unit,
    Submission(InputSubmitOutcome),
    Cleared(Option<String>),
    Batch(InputOutcome),
    Failed(Error),
}
#[derive(Debug)]
pub enum Recovery {
    Pending,
    Complete {
        delivery: Delivery,
        reply: RecoveredReply,
    },
    Forgotten,
    Conflict,
    OutOfOrder,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    Running,
    Exited(Option<ExitInfo>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservationCut {
    /// Preserves legacy Local reads, which have separate locks/timestamps.
    LocalFields,
    /// One authenticated host Inspect response, not several synthesized reads.
    Host { revision: u64 },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub identity: Identity,
    pub lifecycle: Lifecycle,
    pub cut: ObservationCut,
    pub size: Size,
    pub pid: Option<u32>,
    pub output_version: Option<u64>,
    pub idle: Duration,
    pub output_idle: Duration,
    pub human_idle: Option<Duration>,
    pub mouse: MouseState,
    pub attached: bool,
    pub closing: bool,
}

/// Implementations retain stable client/sequence/payload context across adapter
/// rebuilds and resolve unknown outcomes. These are whole operations, not writes.
pub trait SessionOperations: Send + Sync {
    fn identity(&self) -> Identity;
    fn input_owner(&self) -> Option<[u8; 16]>;
    fn inspect(&self) -> Result<Observation>;
    fn recover(&self, request: &RequestIdentity) -> Result<Recovery>;
    fn send(&self, bytes: &[u8]) -> Result<()>;
    fn send_line(&self, text: &str) -> Result<()>;
    fn input_text(&self, text: &str) -> Result<()>;
    fn type_text(&self, text: &str, settle: Duration) -> Result<InputSubmitOutcome>;
    fn submit(&self, expect: &str) -> Result<InputSubmitOutcome>;
    fn clear_input(&self, key: &[u8]) -> Result<Option<String>>;
    fn feed(&self, steps: &[Step]) -> Result<()>;
    fn apply_input_batch(&self, batch: InputBatch<'_>) -> Result<InputOutcome>;
    fn resize(&self, size: Size) -> Result<()>;
    fn terminate(&self) -> Result<()>;
    fn screen_text(&self) -> Result<String>;
    fn snapshot(&self, scrollback: usize) -> Result<VersionedSnapshot>;
    fn wait_after(&self, version: u64, timeout: Duration) -> Result<VersionedSnapshot>;
    fn wakeup(&self) -> Result<Option<OutputWakeup>>;
    fn attach(&self) -> Result<Box<dyn AttachmentOperations + '_>>;
}
/// The host owns generation, human timers and checked detach on guard drop.
/// Disconnection/drop must never terminate the child or release a successor.
pub trait AttachmentOperations: Send + Sync {
    fn identity(&self) -> Identity;
    fn generation(&self) -> u64;
    fn displaced(&self) -> Result<bool>;
    fn writer_timed_out(&self) -> Result<bool>;
    fn write_raw(
        &self,
        bytes: &[u8],
        cancelled: &dyn Fn() -> bool,
        on_start: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<()>;
    fn screen_bytes(&self) -> Result<Vec<u8>>;
    fn wakeup(&self) -> Result<Option<OutputWakeup>>;
}

enum Backend {
    Local(Arc<Session>),
    #[cfg_attr(not(test), allow(dead_code))]
    Remote(Arc<dyn SessionOperations>),
}
/// A checked handle with immutable identity and no local input safety state.
/// It cannot be registered in the legacy Registry, whose type remains Session.
pub struct SessionHandle {
    identity: Identity,
    input_owner: Option<[u8; 16]>,
    backend: Backend,
}
impl SessionHandle {
    pub fn local(session: Arc<Session>) -> Self {
        Self {
            identity: Identity::local(&session),
            input_owner: None,
            backend: Backend::Local(session),
        }
    }
    // Only mock construction exists until native verified identity, checked
    // inventory and every consumer are implemented. Strings cannot select it.
    #[cfg(test)]
    fn remote(backend: Arc<dyn SessionOperations>) -> Result<Self> {
        let identity = backend.identity();
        identity.validate()?;
        if identity.host.is_none() {
            return Err(Error::Protocol("Remote identity lacks host incarnation"));
        }
        let input_owner = Some(
            backend
                .input_owner()
                .ok_or(Error::Protocol("Remote lacks stable input owner"))?,
        );
        Ok(Self {
            identity,
            input_owner,
            backend: Backend::Remote(backend),
        })
    }
    pub fn identity(&self) -> &Identity {
        &self.identity
    }
    pub fn input_owner(&self) -> Option<[u8; 16]> {
        self.input_owner
    }
    /// Lookup only. The provider retains the original bounded payload/context;
    /// Forgotten/Conflict/unknown never causes an automatic new request.
    pub fn recover(&self, request: &RequestIdentity) -> Result<Recovery> {
        let owner = self
            .input_owner
            .ok_or(Error::Unavailable(Unavailable::Unsupported))?;
        if request.session != self.identity || request.client != owner || request.sequence == 0 {
            return Err(Error::Protocol("recovery identity/owner/sequence mismatch"));
        }
        self.operations().recover(request)
    }
    /// Local-only rate/process/legacy raw-stream methods cannot reach Remote.
    pub fn as_local(&self) -> Option<&Arc<Session>> {
        match &self.backend {
            Backend::Local(session) => Some(session),
            Backend::Remote(_) => None,
        }
    }
    fn operations(&self) -> &dyn SessionOperations {
        match &self.backend {
            Backend::Local(session) => session.as_ref(),
            Backend::Remote(remote) => remote.as_ref(),
        }
    }
    pub fn inspect(&self) -> Result<Observation> {
        let observation = self.operations().inspect()?;
        if observation.identity != self.identity {
            return Err(Error::Protocol("observation instance changed"));
        }
        if matches!(&self.backend, Backend::Remote(_))
            && !matches!(observation.cut, ObservationCut::Host { .. })
        {
            return Err(Error::Protocol("Remote observation lacks a host cut"));
        }
        Ok(observation)
    }
    pub fn send(&self, bytes: &[u8]) -> Result<()> {
        self.operations().send(bytes)
    }
    pub fn send_line(&self, text: &str) -> Result<()> {
        self.operations().send_line(text)
    }
    pub fn input_text(&self, text: &str) -> Result<()> {
        self.operations().input_text(text)
    }
    pub fn type_text(&self, text: &str, settle: Duration) -> Result<InputSubmitOutcome> {
        self.operations().type_text(text, settle)
    }
    pub fn submit(&self, expect: &str) -> Result<InputSubmitOutcome> {
        self.operations().submit(expect)
    }
    pub fn clear_input(&self, key: &[u8]) -> Result<Option<String>> {
        self.operations().clear_input(key)
    }
    pub fn feed(&self, steps: &[Step]) -> Result<()> {
        self.operations().feed(steps)
    }
    pub fn apply_input_batch(&self, batch: InputBatch<'_>) -> Result<InputOutcome> {
        self.operations().apply_input_batch(batch)
    }
    pub fn resize(&self, size: Size) -> Result<()> {
        self.operations().resize(size)
    }
    pub fn terminate(&self) -> Result<()> {
        self.operations().terminate()
    }
    pub fn screen_text(&self) -> Result<String> {
        self.operations().screen_text()
    }
    fn checked_snapshot(&self, snapshot: VersionedSnapshot) -> Result<VersionedSnapshot> {
        if snapshot.instance_id.as_deref() != Some(self.identity.instance.as_str()) {
            return Err(Error::Protocol("snapshot instance changed"));
        }
        Ok(snapshot)
    }
    pub fn snapshot(&self, scrollback: usize) -> Result<VersionedSnapshot> {
        self.checked_snapshot(self.operations().snapshot(scrollback)?)
    }
    pub fn wait_after(&self, version: u64, timeout: Duration) -> Result<VersionedSnapshot> {
        self.checked_snapshot(self.operations().wait_after(version, timeout)?)
    }
    pub fn wakeup(&self) -> Result<Option<OutputWakeup>> {
        self.operations().wakeup()
    }
    pub fn attach(&self) -> Result<SessionAttachment<'_>> {
        let attachment = self.operations().attach()?;
        if attachment.identity() != self.identity || attachment.generation() == 0 {
            return Err(Error::Protocol("attachment instance/generation mismatch"));
        }
        Ok(SessionAttachment {
            identity: self.identity.clone(),
            inner: attachment,
        })
    }
}
pub struct SessionAttachment<'a> {
    identity: Identity,
    inner: Box<dyn AttachmentOperations + 'a>,
}
impl SessionAttachment<'_> {
    pub fn identity(&self) -> &Identity {
        &self.identity
    }
    pub fn generation(&self) -> u64 {
        self.inner.generation()
    }
    pub fn displaced(&self) -> Result<bool> {
        self.inner.displaced()
    }
    pub fn writer_timed_out(&self) -> Result<bool> {
        self.inner.writer_timed_out()
    }
    pub fn write_raw(
        &self,
        bytes: &[u8],
        cancelled: &dyn Fn() -> bool,
        on_start: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<()> {
        self.inner.write_raw(bytes, cancelled, on_start)
    }
    pub fn screen_bytes(&self) -> Result<Vec<u8>> {
        self.inner.screen_bytes()
    }
    pub fn wakeup(&self) -> Result<Option<OutputWakeup>> {
        self.inner.wakeup()
    }
}

impl SessionOperations for Session {
    fn identity(&self) -> Identity {
        Identity::local(self)
    }
    fn input_owner(&self) -> Option<[u8; 16]> {
        None
    }
    fn recover(&self, _: &RequestIdentity) -> Result<Recovery> {
        Err(Error::Unavailable(Unavailable::Unsupported))
    }
    fn inspect(&self) -> Result<Observation> {
        let lifecycle = if self.is_alive() {
            Lifecycle::Running
        } else {
            Lifecycle::Exited(self.exit_info())
        };
        Ok(Observation {
            identity: Identity::local(self),
            lifecycle,
            cut: ObservationCut::LocalFields,
            size: self.size(),
            pid: self.process_id_if_alive(),
            output_version: self.output_version(),
            idle: self.idle_for(),
            output_idle: self.output_idle_for(),
            human_idle: self.human_idle_for(),
            mouse: self.mouse_state(),
            attached: self.is_attached(),
            closing: self.is_closing(),
        })
    }
    fn send(&self, bytes: &[u8]) -> Result<()> {
        Ok(Session::send(self, bytes)?)
    }
    fn send_line(&self, text: &str) -> Result<()> {
        Ok(Session::send_line(self, text)?)
    }
    fn input_text(&self, text: &str) -> Result<()> {
        Ok(Session::input_text(self, text)?)
    }
    fn type_text(&self, text: &str, settle: Duration) -> Result<InputSubmitOutcome> {
        Ok(Session::type_text(self, text, settle)?)
    }
    fn submit(&self, expect: &str) -> Result<InputSubmitOutcome> {
        Ok(Session::submit(self, expect)?)
    }
    fn clear_input(&self, key: &[u8]) -> Result<Option<String>> {
        Ok(Session::clear_input(self, key)?)
    }
    fn feed(&self, steps: &[Step]) -> Result<()> {
        Ok(Session::feed(self, steps)?)
    }
    fn apply_input_batch(&self, batch: InputBatch<'_>) -> Result<InputOutcome> {
        Ok(Session::apply_input_batch(self, batch)?)
    }
    fn resize(&self, size: Size) -> Result<()> {
        Ok(Session::resize(self, size)?)
    }
    fn terminate(&self) -> Result<()> {
        Ok(Session::terminate(self)?)
    }
    fn screen_text(&self) -> Result<String> {
        Ok(Session::screen_text(self)?)
    }
    fn snapshot(&self, scrollback: usize) -> Result<VersionedSnapshot> {
        Ok(self.screen_snapshot_version_at(scrollback)?)
    }
    fn wait_after(&self, version: u64, timeout: Duration) -> Result<VersionedSnapshot> {
        Ok(self.wait_for_output_after(version, timeout)?)
    }
    fn wakeup(&self) -> Result<Option<OutputWakeup>> {
        Ok(self.subscribe_output_wakeup())
    }
    fn attach(&self) -> Result<Box<dyn AttachmentOperations + '_>> {
        Ok(Box::new(Session::attach(self)?))
    }
}
impl<S: Borrow<Session> + Send + Sync> AttachmentOperations for Attachment<S> {
    fn identity(&self) -> Identity {
        Identity::local(self.session())
    }
    fn generation(&self) -> u64 {
        Attachment::generation(self)
    }
    fn displaced(&self) -> Result<bool> {
        Ok(self.is_displaced())
    }
    fn writer_timed_out(&self) -> Result<bool> {
        Ok(self.is_writer_timed_out())
    }
    fn write_raw(
        &self,
        bytes: &[u8],
        cancelled: &dyn Fn() -> bool,
        on_start: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<()> {
        Ok(self.write_raw_while_started(bytes, cancelled, on_start)?)
    }
    fn screen_bytes(&self) -> Result<Vec<u8>> {
        Ok(Attachment::screen_bytes(self)?)
    }
    fn wakeup(&self) -> Result<Option<OutputWakeup>> {
        Ok(self.session().subscribe_output_wakeup())
    }
}

#[cfg(test)]
#[path = "session_operations_tests.rs"]
mod tests;
