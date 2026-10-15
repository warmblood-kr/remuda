//! Experimental standalone-host RPC v1, separate from daemon/cluster APIs.
use super::result::Completion;
use super::wire::{Hello, InputAct};
use serde::{Deserialize, Serialize};
pub(crate) const PAGE_BYTES: usize = 12 * 1024;
pub(crate) const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
/// Ledger reserves before effects; clients/watermarks never recycle or reset
/// within an immutable host incarnation. Forgotten can never become New.
pub(crate) const RETAINED_LEDGER: &str = "retained-input-ledger-v1";
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
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", deny_unknown_fields)]
pub(crate) enum Request {
    Inspect,
    Acquire,
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
    Page {
        snapshot: u64,
        index: usize,
    },
    Acknowledge {
        snapshot: u64,
        generation: u64,
    },
    Stop,
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
    Page {
        snapshot: u64,
        index: usize,
        base64: String,
    },
    Ok,
    Error {
        code: String,
    },
}
