//! What one remuda node says to another.
//!
//! This lives in the policy layer on purpose. It names no socket, no address
//! and no transport — those are the host layer's business, and `clippy.toml`
//! denies them here outright. What is defined is only the *shape* of the
//! conversation.
//!
//! 정수님, 2026-09-10, set the direction this anticipates: remuda daemons on
//! each node, a Matrix homeserver on the host, and each node a headless Matrix
//! client for its agents. Keeping the message shape here and the transport
//! below the seam is what makes that a second transport rather than a rewrite —
//! the same move that put the pty behind `AgentProcess`.
//!
//! Requests and responses are exchanged as one JSON object per line. After an
//! accepted [`Request::Attach`] the connection stops being a request channel
//! and becomes a raw byte pipe in both directions, which is why attaching has
//! no response type beyond the acknowledgement.

use crate::agent::Size;
use crate::registry::SessionSummary;
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Request {
    /// Every session this node holds.
    List,
    /// Start a session, answering [`Response::Value`] with the name it got.
    /// `command` is argv; empty means the user's shell. `None` for `name`
    /// derives one from argv[0] and de-duplicates it; a given name is exact.
    New {
        #[serde(default)]
        name: Option<String>,
        command: Vec<String>,
        size: Size,
    },
    /// Deliver one instruction as an indivisible act. Refused while a human is
    /// attached — see [`crate::session::Session`], invariant 3.
    SendLine { name: String, text: String },
    /// Deliver a burst of input bytes as an indivisible act, appending nothing —
    /// the primitive [`Request::SendLine`] is made of. Indivisible is the
    /// load-bearing word; refused while a human is attached, as `SendLine` is.
    Send { name: String, bytes: Vec<u8> },
    /// Read the screen as text without taking the session over. Needs no
    /// terminal, raw mode or exclusivity, so it works while a human is attached.
    Capture { name: String },
    /// Take the session over for a human at a terminal. On `Ok`, this
    /// connection becomes a byte pipe.
    Attach { name: String },
    /// End a session — live or already self-exited — and stop tracking it. Death
    /// alone does not remove: an exited session stays listed until this. Refused
    /// while a human is attached, as `Send`/`SendLine` are.
    Close { name: String },
    /// What build the daemon was started from, as [`Response::Value`]. A daemon
    /// outlives the binary that spawned it, so this is a client's only way to
    /// learn it is talking to yesterday's code before a field mismatch does.
    Version,
    /// Stop the daemon, so the next command starts a fresh one. Every session
    /// and the whole Lua image die with it; naming that loss and getting it
    /// confirmed is the client's job, not this one's.
    Shutdown,
    /// Evaluate Lua in the daemon's long-lived image. Caution: the state this
    /// touches outlives the request — two `Eval`s share globals, and a script,
    /// a `-e` and a REPL line are three doors into one interpreter.
    Eval {
        code: String,
        /// What a traceback should call this chunk — a file path for
        /// `remuda run`, `None` for a `-e` or a REPL line. Carried on the wire
        /// because only the caller knows where the source came from.
        name: Option<String>,
    },
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Response {
    Sessions(Vec<SessionSummary>),
    Screen(String),
    /// What an [`Request::Eval`] returned, already rendered to text. Kept
    /// distinct from `Screen` so a client can tell "the session printed
    /// nothing" from "the expression returned nothing".
    Value(String),
    Ok,
    /// The reason, in words meant for a person. A client prints this; it does
    /// not parse it.
    Error(String),
}

impl Response {
    /// Failure as a value rather than a convention, so a handler cannot report
    /// a problem by returning `Ok` with an empty list.
    pub fn error(reason: impl core::fmt::Display) -> Self {
        Response::Error(reason.to_string())
    }
}

// `Size` clamps to a floor below which real TUIs silently drop keystrokes, and
// a wire format is the obvious way to smuggle a violation past a constructor.
// Serializing is safe as-is; deserializing routes through `Size::new` so a
// peer — or a corrupted line — cannot hand us an 11-column terminal.
#[derive(Serialize, Deserialize)]
struct SizeWire {
    cols: u16,
    rows: u16,
}

impl Serialize for Size {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        SizeWire {
            cols: self.cols(),
            rows: self.rows(),
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire = SizeWire::deserialize(d)?;
        Ok(Size::new(wire.cols, wire.rows))
    }
}
