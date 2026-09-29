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

use crate::agent::{Color, Cursor, MouseState, Size, StyledCell};
use crate::registry::SessionSummary;
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Request {
    /// Every session this node holds.
    List,
    /// Start a session, answering [`Response::Value`] with the name it got.
    /// `command` is argv (empty = shell); unset `name` derives and dedupes one.
    /// `cwd`/`env` default to the daemon's own directory/environment.
    New {
        #[serde(default)]
        name: Option<String>,
        command: Vec<String>,
        size: Size,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: Option<std::collections::HashMap<String, String>>,
    },
    /// Deliver one instruction as an indivisible act. An attached terminal is
    /// a viewer, not a delivery lock.
    SendLine { name: String, text: String },
    /// Submit an idempotent input batch. Client IDs are 32 hex digits and
    /// sequence numbers start at one and increase for each batch.
    Input {
        name: String,
        instance_id: String,
        client_id: String,
        seq: u64,
        bytes: Vec<u8>,
    },
    /// Deliver a burst of input bytes as an indivisible act, appending nothing —
    /// the primitive [`Request::SendLine`] is made of. Indivisible is the
    /// load-bearing word; an attached terminal does not block it.
    Send { name: String, bytes: Vec<u8> },
    /// Deliver a sequence of [`Step`]s as one indivisible act — `Send`/
    /// `SendLine` are its one-`Burst` case. An attached terminal does not
    /// block it.
    Feed { name: String, steps: Vec<Step> },
    /// Resize a session's terminal to its viewer panel. Ordinary sizes are
    /// clamped during deserialization; an explicitly pane-sized value can
    /// retain its narrower visible width.
    Resize { name: String, size: Size },
    /// Move a session's retained terminal history; positive means older.
    /// Read the screen as text without taking the session over. Needs no
    /// terminal, raw mode or exclusivity, so it works while a human is attached.
    Capture { name: String },
    /// Like `Capture`, but styled cells instead of plain text — what a
    /// croppable colour pane reads. See [`Response::StyledScreen`].
    CaptureStyled {
        name: String,
        #[serde(default)]
        scrollback: usize,
    },
    /// Wait for a newer styled screen, returning the current frame on timeout.
    Sync {
        name: String,
        #[serde(default)]
        instance_id: Option<String>,
        since: u64,
        timeout_ms: u64,
    },
    /// Read the child's current mouse mode and encoding.
    MouseState { name: String },
    /// Take the session over for a human at a terminal. On `Ok`, this
    /// connection becomes a byte pipe.
    Attach { name: String },
    /// Attach with a generation that can be checked after the raw stream ends.
    AttachTracked { name: String },
    /// Ask whether a tracked attachment was superseded while its stream ended.
    AttachStatus { name: String, generation: u64 },
    /// End a live or self-exited session and stop tracking it; refused while human-attached.
    /// Local callers may omit optional fields for legacy behavior; the remote front requires
    /// explicit confirmation bound to an instance id.
    Close {
        name: String,
        /// Session start the caller intends to close. Required by the remote
        /// front; omitted by legacy local callers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance_id: Option<String>,
        /// Explicit operator confirmation. Required by the remote front;
        /// omitted local calls retain the established behavior.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confirm: Option<bool>,
    },
    /// List a directory's immediate entries by name, sorted. No session
    /// involved — a plain filesystem primitive for managing topic directories.
    ListDir { path: String },
    /// Create a directory and any missing parents; a fresh, existing, or
    /// already-created path all succeed the same way.
    Mkdir { path: String },
    /// Remove a directory and everything in it — named after the `std::fs`
    /// call it makes, so nobody expects `rmdir`'s empty-directory-only rule.
    RemoveDirAll { path: String },
    /// What build the daemon was started from, as [`Response::Value`]. A daemon
    /// outlives the binary that spawned it, so this is a client's only way to
    /// learn it is talking to yesterday's code before a field mismatch does.
    Version,
    /// Read a bounded page of the authenticated peer's registry. `digest`
    /// lets the peer return an unchanged response; entries use canonical JSON
    /// strings so the policy crate does not depend on the host JSON codec.
    ClusterRegistrySync {
        digest: Option<String>,
        offset: usize,
    },
    /// Apply one bounded, canonical registry update from an authenticated peer.
    ClusterRegistryUpdate { update_json: String },
    /// Stop the daemon, so the next command starts a fresh one. The daemon
    /// refuses a request identifying one of its own sessions unless the
    /// caller explicitly overrides the hosted-session guard.
    Shutdown {
        #[serde(default)]
        requester_daemon_id: Option<String>,
        #[serde(default)]
        requester_session_id: Option<String>,
        #[serde(default)]
        requester_session_name: Option<String>,
        #[serde(default)]
        override_hosted: bool,
    },
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

/// One element of a [`Request::Feed`] act: bytes, or a pause before the next
/// `Burst`. Milliseconds, not `Duration` — kept a plain integer so this enum,
/// unlike `Duration`, can still derive `Eq`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Step {
    Burst(Vec<u8>),
    Pause(u64),
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Response {
    Sessions(Vec<SessionSummary>),
    Screen(String),
    /// An input batch was applied, or was already applied.
    Ack {
        duplicate: bool,
    },
    /// The daemon cannot prove whether an unknown or evicted batch was applied.
    Uncertain,
    /// The target name now refers to a different session start.
    WrongInstance,
    /// The per-session input byte budget has been exhausted for this second.
    RateLimited,
    /// A versioned screen, returned by [`Request::Sync`]. A disconnected
    /// client cannot cancel its daemon request, so its Sync slot remains held
    /// until the bounded wait ends.
    Sync {
        instance_id: String,
        output_version: u64,
        snapshot: StyledScreen,
    },
    /// A Sync request was refused because the daemon or remote-front limit is full.
    SyncAtCapacity,
    /// Another write is already in flight; this request was not queued.
    Busy,
    /// The bounded PTY write deadline elapsed; delivery may be partial or late.
    WriteTimeout,
    /// The receiving cluster node has disabled remote Input locally.
    RemoteControlDisabled,
    /// A styled screen, answering [`Request::CaptureStyled`] — as runs, not
    /// cells; see [`StyledRun`]. `cursor` rides the same round trip, so the
    /// pane's caret and its content are always the same frame. See steps/027.
    StyledScreen {
        rows: Vec<Vec<StyledRun>>,
        /// Session identity for this start; absent in responses from old daemons.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance_id: Option<String>,
        /// Output generation for clients that only need to redraw changed frames.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_version: Option<u64>,
        #[serde(default)]
        wrapped: Vec<bool>,
        #[serde(default)]
        scrollback_len: usize,
        #[serde(default)]
        scrollback_total: usize,
        cursor: Cursor,
    },
    /// Current mouse tracking mode and encoding, read from the live parser.
    MouseState(MouseState),
    /// What an [`Request::Eval`] returned, already rendered to text. Kept
    /// distinct from `Screen` so a client can tell "the session printed
    /// nothing" from "the expression returned nothing".
    Value(String),
    /// A command result completed through a bounded pending reply handle.
    CommandResult {
        exit_code: u8,
        stdout_base64: String,
        stderr_base64: String,
    },
    Ok,
    /// A tracked attachment was accepted and its generation is returned.
    AttachStarted {
        generation: u64,
    },
    /// Whether this generation was displaced by a newer attachment.
    AttachStatus {
        taken_over: bool,
    },
    /// The reason, in words meant for a person. A client prints this; it does
    /// not parse it.
    Error(String),
    /// Directory entries, answering [`Request::ListDir`] — names only, no
    /// path prefix, sorted for a stable diff.
    Entries(Vec<String>),
    /// One page of the registry replication snapshot.
    ClusterRegistryPage {
        sender_fp: String,
        digest: String,
        offset: usize,
        entries_json: String,
        next_offset: Option<usize>,
        unchanged: bool,
    },
    /// A registry update was accepted (whether or not it changed local state).
    ClusterRegistryAck {
        digest: String,
        applied: bool,
    },
}

impl Response {
    /// Failure as a value rather than a convention, so a handler cannot report
    /// a problem by returning `Ok` with an empty list.
    pub fn error(reason: impl core::fmt::Display) -> Self {
        Response::Error(reason.to_string())
    }
}

/// One run of adjacent cells sharing an identical style — the wire shape
/// [`Response::StyledScreen`] actually sends. See steps/022, 023.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StyledRun {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    /// True when every character in `text` is a wide (CJK) glyph — see
    /// [`StyledCell::wide`]. Part of the grouping key: a run never mixes
    /// wide and narrow cells, so this one flag applies to the whole run.
    pub wide: bool,
}

/// A styled frame captured atomically with its output version for Sync.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StyledScreen {
    pub rows: Vec<Vec<StyledRun>>,
    #[serde(default)]
    pub wrapped: Vec<bool>,
    #[serde(default)]
    pub scrollback_len: usize,
    #[serde(default)]
    pub scrollback_total: usize,
    pub cursor: Cursor,
}

/// Collapse adjacent cells sharing one style (wideness included) into runs.
/// A wide cell's now-empty continuation collapses to nothing either way,
/// which is correct: it claims 0 columns. See steps/022, 023.
pub fn collapse_runs(row: &[StyledCell]) -> Vec<StyledRun> {
    let mut runs: Vec<StyledRun> = Vec::new();
    for cell in row {
        let extends_last = runs.last().is_some_and(|r: &StyledRun| {
            r.fg == cell.fg
                && r.bg == cell.bg
                && r.bold == cell.bold
                && r.dim == cell.dim
                && r.italic == cell.italic
                && r.underline == cell.underline
                && r.inverse == cell.inverse
                && r.wide == cell.wide
        });
        if extends_last {
            runs.last_mut().unwrap().text.push_str(&cell.text);
        } else {
            runs.push(StyledRun {
                text: cell.text.clone(),
                fg: cell.fg,
                bg: cell.bg,
                bold: cell.bold,
                dim: cell.dim,
                italic: cell.italic,
                underline: cell.underline,
                inverse: cell.inverse,
                wide: cell.wide,
            });
        }
    }
    runs
}

/// The exact inverse of [`collapse_runs`]: one `StyledCell` per character,
/// carrying the run's style and wideness. An empty-`text` run (only
/// continuations) expands back to nothing, same as it started.
pub fn expand_runs(runs: &[StyledRun]) -> Vec<StyledCell> {
    runs.iter()
        .flat_map(|run| {
            run.text.chars().map(move |c| StyledCell {
                text: c.to_string(),
                fg: run.fg,
                bg: run.bg,
                bold: run.bold,
                dim: run.dim,
                italic: run.italic,
                underline: run.underline,
                inverse: run.inverse,
                wide: run.wide,
            })
        })
        .collect()
}

// `Size::new` clamps to a floor below which real TUIs silently drop
// keystrokes. Ordinary wire sizes take that same path. Pane sizes carry an
// explicit opt-in because their child must lay out at the width actually
// visible beside the list.
#[derive(Serialize, Deserialize)]
struct SizeWire {
    cols: u16,
    rows: u16,
    #[serde(default, skip_serializing_if = "is_false")]
    allow_narrow: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Serialize for Size {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        SizeWire {
            cols: self.cols(),
            rows: self.rows(),
            allow_narrow: self.cols() < Size::MIN_COLS,
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire = SizeWire::deserialize(d)?;
        Ok(if wire.allow_narrow {
            Size::for_pane(wire.cols, wire.rows)
        } else {
            Size::new(wire.cols, wire.rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(text: &str, fg: Color) -> StyledCell {
        StyledCell {
            text: text.to_string(),
            fg,
            ..Default::default()
        }
    }

    /// [MEASURED] `expand_runs` is the exact inverse of `collapse_runs` — a
    /// styled row survives the round trip byte for byte. See steps/022.
    #[test]
    fn collapsing_into_runs_and_expanding_back_changes_nothing_visible() {
        let row: Vec<StyledCell> = vec![
            cell("u", Color::Idx(2)),
            cell("s", Color::Idx(2)),
            cell("r", Color::Idx(2)),
            cell(":", Color::Default),
            cell("~", Color::Idx(4)),
            cell("$", Color::Idx(4)),
            cell(" ", Color::Default),
        ];
        let runs = collapse_runs(&row);
        assert_eq!(runs.len(), 4, "four style groups, not seven runs: {runs:?}");
        assert_eq!(expand_runs(&runs), row);
    }

    /// [MEASURED] A wide cell's flag survives the round trip; its
    /// now-empty continuation never merges into it and never reappears.
    /// See steps/023.
    #[test]
    fn a_wide_cells_flag_survives_the_round_trip_and_its_continuation_vanishes() {
        let mut wide = cell("안", Color::Idx(2));
        wide.wide = true;
        let continuation = cell("", Color::Idx(2)); // wide: false, by construction
        let row = vec![wide.clone(), continuation, cell("!", Color::Default)];

        let runs = collapse_runs(&row);
        assert_eq!(
            runs.len(),
            3,
            "the continuation's differing `wide` must start its own run, \
             not merge into the wide cell despite sharing a colour: {runs:?}"
        );

        let back = expand_runs(&runs);
        assert_eq!(
            back,
            vec![wide, cell("!", Color::Default)],
            "the continuation must not reappear: {back:?}"
        );
    }
}
