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

use crate::agent::{Color, Cursor, Size, StyledCell};
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
    /// Deliver one instruction as an indivisible act. Refused while a human is
    /// attached — see [`crate::session::Session`], invariant 3.
    SendLine { name: String, text: String },
    /// Deliver a burst of input bytes as an indivisible act, appending nothing —
    /// the primitive [`Request::SendLine`] is made of. Indivisible is the
    /// load-bearing word; refused while a human is attached, as `SendLine` is.
    Send { name: String, bytes: Vec<u8> },
    /// Deliver a sequence of [`Step`]s as one indivisible act — `Send`/
    /// `SendLine` are its one-`Burst` case. Refused while a human is
    /// attached, as they are; see [`crate::session::Session`].
    Feed { name: String, steps: Vec<Step> },
    /// Read the screen as text without taking the session over. Needs no
    /// terminal, raw mode or exclusivity, so it works while a human is attached.
    Capture { name: String },
    /// Like `Capture`, but styled cells instead of plain text — what a
    /// croppable colour pane reads. See [`Response::StyledScreen`].
    CaptureStyled { name: String },
    /// Take the session over for a human at a terminal. On `Ok`, this
    /// connection becomes a byte pipe.
    Attach { name: String },
    /// End a session — live or already self-exited — and stop tracking it.
    /// Refused while a human is attached, as `Send`/`SendLine` are. A session
    /// that ended on its own is dropped by `List`; this is for one still alive.
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
    /// A styled screen, answering [`Request::CaptureStyled`] — as runs, not
    /// cells; see [`StyledRun`]. `cursor` rides the same round trip, so the
    /// pane's caret and its content are always the same frame. See steps/027.
    StyledScreen {
        rows: Vec<Vec<StyledRun>>,
        cursor: Cursor,
    },
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
