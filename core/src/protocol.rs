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
    /// Start a session. `command` is argv; empty means the user's shell.
    New {
        name: String,
        command: Vec<String>,
        size: Size,
    },
    /// Deliver one instruction as an indivisible act. Refused while a human is
    /// attached — see [`crate::session::Session`], invariant 3.
    SendLine { name: String, text: String },
    /// Read the screen as text, without taking the session over.
    ///
    /// This is how a machine looks: it needs no terminal, no raw mode and no
    /// exclusivity, so it works while a human is attached. Attaching for a
    /// glance would lock the core out of a session it only wanted to read.
    Capture { name: String },
    /// Take the session over for a human at a terminal. On `Ok`, this
    /// connection becomes a byte pipe.
    Attach { name: String },
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Response {
    Sessions(Vec<SessionSummary>),
    Screen(String),
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
