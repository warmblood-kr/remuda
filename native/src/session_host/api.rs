//! Experimental standalone-host RPC v1, separate from daemon/cluster APIs.
use super::human::{Cut, GrantState, Raw, Retire};
use super::result::Completion;
use super::wire::{Hello, InputAct};
use serde::{Deserialize, Serialize};
pub(crate) const PAGE_BYTES: usize = 12 * 1024;
pub(crate) const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
/// Ledger reserves before effects; clients/watermarks never recycle or reset
/// within an immutable host incarnation. Forgotten can never become New.
pub(crate) const RETAINED_LEDGER: &str = "retained-input-ledger-v1";
pub(crate) const TYPED_VIEW: &str = "typed-terminal-view-v1";
pub(crate) const SCOPED_BIRTH: &str = "scoped-host-publisher-v1";
/// Human role is refused unless the client required this capability first.
pub(crate) const HUMAN_INPUT: &str = "human-input-v1";
/// Inspect carries Session activity; absent on older hosts, never read as zero.
pub(crate) const ACTIVITY: &str = "activity-v1";
/// AcquireWriter is understood; the host refuses any writer older than one seen.
pub(crate) const WRITER_FENCE: &str = "writer-fence-v1";
/// The durable inventory writer a daemon presents: store incarnation, epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Writer {
    pub store: [u8; 16],
    pub epoch: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum PublicationStatus {
    Starting = 0,
    Ready = 1,
    Unavailable = 2,
    Complete = 3,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    pub host: [u8; 16],
    pub session: String,
    pub instance: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Role {
    View,
    Control,
    Human,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Open {
    pub protocol: Hello,
    pub role: Role,
    pub expected: Option<Identity>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub schema: u16,
    pub identity: Identity,
    pub name: String,
    pub agent: String,
    pub cwd: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Lifecycle {
    Running,
    Exited,
    Unavailable,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum OutputEnd {
    Open,
    Eof,
    Hangup,
    ReadFailure,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Info {
    #[serde(default)]
    pub publication: Option<PublicationStatus>,
    pub identity: Identity,
    pub name: String,
    pub pid: Option<u32>,
    pub lifecycle: Lifecycle,
    pub exit_code: Option<u32>,
    pub signal: Option<i32>,
    pub signal_name: Option<String>,
    pub generation: u64,
    pub rows: u16,
    pub cols: u16,
    pub output_eof: bool,
    pub output_end: OutputEnd,
    pub drained_generation: Option<u64>,
    pub host_build: String,
    pub unsupported_osc: u64,
    pub query_overflow: u64,
    pub backend_fault: bool,
    pub active_connections: usize,
    /// Optional for older hosts; daemon observations need these Session fields.
    #[serde(default)]
    pub activity: Option<Activity>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Activity {
    pub idle_ms: u64,
    pub output_idle_ms: u64,
    pub human_idle_ms: Option<u64>,
    pub attached: bool,
    pub closing: bool,
    pub mouse: remuda_core::agent::MouseState,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", deny_unknown_fields)]
pub(crate) enum Request {
    Inspect,
    Acquire,
    /// Only sent to a host advertising WRITER_FENCE; older hosts reject it.
    AcquireWriter {
        writer: Writer,
    },
    Release,
    Input {
        client: [u8; 16],
        sequence: u64,
        act: InputAct,
    },
    Recover {
        client: [u8; 16],
        sequence: u64,
        act: InputAct,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
    Close,
    Snapshot,
    Viewport {
        scrollback: u16,
    },
    AcknowledgeViewport {
        snapshot: u64,
        generation: u64,
    },
    WatchAfter {
        generation: u64,
        timeout_ms: u16,
    },
    Page {
        snapshot: u64,
        index: usize,
    },
    Acknowledge {
        snapshot: u64,
        generation: u64,
    },
    Stop,
    /// Checked human grant; only the secret's digest is retained.
    HumanGrant {
        request: [u8; 16],
        expected: u64,
        recovery_secret: [u8; 32],
    },
    RevokeHuman {
        grant: u64,
    },
    HumanInput {
        grant: u64,
        sequence: u64,
        bytes: Vec<u8>,
    },
    RecoverRaw {
        grant: u64,
        sequence: u64,
        bytes: Vec<u8>,
    },
    RawCut {
        last: u64,
    },
    AcknowledgeRaw {
        cut: Cut,
    },
    DiscardRaw {
        cut: Cut,
    },
}
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum OutcomeState {
    New,
    Pending,
    Complete,
    Forgotten,
    Conflict,
    OutOfOrder,
    ClientLimit,
    Busy,
}
impl From<super::ledger::Lookup> for OutcomeState {
    fn from(value: super::ledger::Lookup) -> Self {
        use super::ledger::Lookup;
        match value {
            Lookup::New => Self::New,
            Lookup::Pending => Self::Pending,
            Lookup::Complete(_) => Self::Complete,
            Lookup::Forgotten => Self::Forgotten,
            Lookup::Conflict => Self::Conflict,
            Lookup::OutOfOrder => Self::OutOfOrder,
            Lookup::ClientLimit => Self::ClientLimit,
            Lookup::Busy => Self::Busy,
        }
    }
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ViewState {
    Unchanged,
    ResyncRequired,
    Ended,
    Degraded,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result")]
pub(crate) enum Response {
    Opened {
        identity: Identity,
        minor: u16,
        capabilities: Vec<String>,
        max_payload: usize,
    },
    Info {
        info: Info,
    },
    Granted {
        epoch: u64,
    },
    Admitted,
    Outcome {
        state: OutcomeState,
        completion: Option<Completion>,
    },
    Snapshot {
        snapshot: u64,
        generation: u64,
        bytes: usize,
        pages: usize,
        rows: u16,
        cols: u16,
    },
    Viewport {
        identity: Identity,
        snapshot: u64,
        generation: u64,
        bytes: usize,
        pages: usize,
        rows: u16,
        cols: u16,
    },
    ViewState {
        generation: u64,
        resync_required: bool,
        state: ViewState,
        output_end: OutputEnd,
        unsupported_osc: u64,
        query_overflow: u64,
    },
    Page {
        snapshot: u64,
        index: usize,
        base64: String,
    },
    HumanGranted {
        grant: u64,
        state: GrantState,
    },
    Revoked {
        revoked: bool,
    },
    RawOutcome {
        state: Raw,
    },
    RawCut {
        cut: Cut,
    },
    Retired {
        state: Retire,
    },
    Ok,
    Error {
        code: String,
    },
}
