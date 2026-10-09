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
use zeroize::Zeroizing;

/// A local control operation for the daemon-owned cluster listener.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ListenerOp {
    Status,
    Reload,
}

/// The daemon's current listener state, shared by local callers and the wire protocol.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ListenerStatus {
    Off,
    On {
        addr: std::net::SocketAddr,
        auto: bool,
        #[serde(default)]
        advertise_addr: Option<std::net::SocketAddr>,
        #[serde(default)]
        listen_addrs: Vec<std::net::SocketAddr>,
    },
    WaitingForLan(String),
    Failed(String),
}

/// Maximum secret payload accepted by [`Request::SecretAnswer`].
pub const SECRET_ANSWER_MAX_BYTES: usize = 4 * 1024;

/// Maximum newline-delimited frame carrying a [`Request::SecretAnswer`].
pub const SECRET_ANSWER_MAX_FRAME_BYTES: usize = 8 * 1024;

/// Maximum visible line accepted by a [`Request::LineAnswer`].
pub const LINE_ANSWER_MAX_BYTES: usize = 1024;

/// Maximum newline-delimited frame carrying a [`Request::LineAnswer`].
pub const LINE_ANSWER_MAX_FRAME_BYTES: usize = 8 * 1024;

/// Whether a character is an invisible formatting or separator character that
/// should not be preserved in a prompt label or visible line answer.
pub fn is_secret_prompt_format_or_separator(ch: char) -> bool {
    matches!(
        ch,
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{2029}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

/// Remove terminal controls and invisible formatting/separator characters
/// from prompt text crossing the local daemon protocol.
pub fn sanitize_secret_prompt_text(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() && !is_secret_prompt_format_or_separator(*ch))
        .collect()
}

/// Secret bytes encoded as base64 on the wire and redacted from debug output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretBytes(#[serde(with = "secret_bytes_base64")] Zeroizing<Vec<u8>>);

impl SecretBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Zeroizing<Vec<u8>> {
        self.0
    }
}

impl core::fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("SecretBytes([REDACTED])")
    }
}

/// A bounded reason a client could not provide a secret answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretAnswerRefusal {
    NotATerminal,
    TooLong,
}

impl SecretAnswerRefusal {
    pub fn error_code(self) -> &'static str {
        match self {
            Self::NotATerminal => "not_a_terminal",
            Self::TooLong => "too_long",
        }
    }
}

mod secret_bytes_base64 {
    use super::SECRET_ANSWER_MAX_BYTES;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde::de::Error as _;
    use serde::ser::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};
    use zeroize::Zeroizing;

    const MAX_ENCODED_BYTES: usize = SECRET_ANSWER_MAX_BYTES.div_ceil(3) * 4;

    pub fn serialize<S>(bytes: &Zeroizing<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if bytes.len() > SECRET_ANSWER_MAX_BYTES {
            return Err(S::Error::custom("secret answer exceeds 4096 bytes"));
        }
        let encoded = Zeroizing::new(STANDARD.encode(bytes));
        serializer.serialize_str(encoded.as_str())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Zeroizing<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = Zeroizing::new(String::deserialize(deserializer)?);
        let input = encoded.as_bytes();
        if input.len() % 4 != 0 || input.len() > MAX_ENCODED_BYTES {
            return Err(D::Error::custom("invalid or oversized secret encoding"));
        }
        let padding = if input.ends_with(b"==") {
            2
        } else if input.ends_with(b"=") {
            1
        } else {
            0
        };
        let decoded_len = (input.len() / 4) * 3 - padding;
        if decoded_len > SECRET_ANSWER_MAX_BYTES {
            return Err(D::Error::custom("secret answer exceeds 4096 bytes"));
        }
        STANDARD
            .decode(encoded.as_str())
            .map(Zeroizing::new)
            .map_err(D::Error::custom)
    }
}

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
    /// Start a session for the Lua API and return its registered identity.
    /// Ordinary callers keep using [`Request::New`] and receive only the name.
    NewWithInstance {
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
    /// Write one caller-supplied clear-line key as an indivisible act.
    ClearInput { name: String, key: Vec<u8> },
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
    /// Read or reload the daemon-owned local cluster listener.
    ClusterListener(ListenerOp),
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
    /// Answer an outstanding secret prompt on this pending word's connection.
    SecretAnswer {
        id: u32,
        secret: Option<SecretBytes>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refusal: Option<SecretAnswerRefusal>,
    },
    /// Answer an outstanding visible line prompt on this pending word's connection.
    LineAnswer {
        id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refusal: Option<SecretAnswerRefusal>,
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
    /// The session-level clear-line key was written; cleared text is optional
    /// because terminal screens do not generally identify composer contents.
    ClearInput {
        cleared: Option<String>,
    },
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
    /// A newly spawned session and the identity returned by its registry insertion.
    SessionStarted {
        name: String,
        instance_id: String,
    },
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
    /// Current state of the daemon-owned cluster listener.
    ClusterListenerStatus(ListenerStatus),
    /// Ask the client on this connection to collect a secret from its terminal.
    PromptSecret {
        id: u32,
        label: String,
        /// Remaining lifetime of the pending reply when this prompt was sent.
        #[serde(default)]
        timeout_ms: u64,
    },
    /// Ask the client on this connection to collect a visible line from its terminal.
    PromptLine {
        id: u32,
        label: String,
        /// Lines shown above the prompt, indented and untagged. An older
        /// client ignores them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        preface: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        /// Remaining lifetime of the pending reply when this prompt was sent.
        #[serde(default)]
        timeout_ms: u64,
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
    /// Whether the child currently expects pasted text in terminal mode 2004.
    /// Missing values from older peers mean the mode is disabled.
    #[serde(default)]
    pub bracketed_paste: bool,
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
                // A wide glyph's empty continuation joins its run, so a
                // CJK row stays one run, not two per glyph (#589).
                && (r.wide == cell.wide || (r.wide && cell.text.is_empty()))
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
            2,
            "the continuation merges into the wide cell it belongs to: {runs:?}"
        );

        let back = expand_runs(&runs);
        assert_eq!(
            back,
            vec![wide, cell("!", Color::Default)],
            "the continuation must not reappear: {back:?}"
        );
    }

    /// #589: a Hangul row must not cost two runs per glyph on the wire.
    #[test]
    fn a_hangul_row_collapses_to_one_run() {
        let mut row = Vec::new();
        for _ in 0..60 {
            let mut wide = cell("한", Color::Default);
            wide.wide = true;
            row.push(wide);
            row.push(cell("", Color::Default));
        }
        assert_eq!(collapse_runs(&row).len(), 1);
    }
}
