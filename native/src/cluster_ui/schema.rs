//! Atomic validation of a Lua composition. Nothing is patched or partially
//! accepted: a missing, duplicate or unknown target rejects the whole
//! document so a stale row can never route an action.

use super::geometry::{Axis, Layout};
use super::{
    resolve_geometry, Color, ComposeSource, ComposedGroup, ComposedSession, Composition, GroupKind,
    Model, Row, Span, MAX_OUTPUT_BYTES, MAX_ROWS, MAX_SPANS_PER_ROW, MAX_SPAN_BYTES,
    MAX_TOTAL_SPANS,
};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RawResult {
    pub value: RawDoc,
    /// Serialized size of the Lua document before conversion.
    pub encoded_len: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RawDoc {
    pub version: u32,
    pub title: Vec<RawRow>,
    pub groups: Vec<RawGroup>,
    pub empty: Vec<RawRow>,
    pub layout: RawLayout,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RawGroup {
    pub id: u32,
    pub rows: Vec<RawRow>,
    pub sessions: Vec<RawSession>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RawSession {
    pub id: u32,
    pub rows: Vec<RawRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RawRow {
    pub spans: Vec<RawSpan>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RawSpan {
    pub text: String,
    /// `Some("status")` asks Rust to substitute the canonical status text of
    /// the enclosing target. No other field is accepted.
    pub field: Option<String>,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RawLayout {
    pub axis: Axis,
    pub list_cells: u16,
    pub divider: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposeError {
    pub message: String,
}

impl ComposeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ComposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub fn validate(model: &Model, raw: RawResult) -> Result<Composition, ComposeError> {
    if raw.encoded_len > MAX_OUTPUT_BYTES {
        return Err(ComposeError::new("composition exceeds the output cap"));
    }
    if raw.value.version != 1 || model.version != 1 {
        return Err(ComposeError::new("unsupported composition version"));
    }
    let total_spans: usize = count_spans(&raw.value);
    if total_spans > MAX_TOTAL_SPANS {
        return Err(ComposeError::new("composition exceeds the span cap"));
    }
    let total_rows = count_rows(&raw.value);
    if total_rows > MAX_ROWS {
        return Err(ComposeError::new("composition exceeds the row cap"));
    }

    let title = validate_decorative(&raw.value.title)?;
    let empty = validate_decorative(&raw.value.empty)?;
    let layout = resolve_geometry(
        &model.viewport,
        &Layout {
            axis: raw.value.layout.axis,
            list_cells: raw.value.layout.list_cells,
            divider: raw.value.layout.divider.clone(),
        },
    )
    .map_err(|error| ComposeError::new(error.to_string()))?;

    let groups = validate_groups(model, raw.value.groups)?;
    if groups.len() != model.groups.len() {
        return Err(ComposeError::new("missing group target"));
    }

    Ok(Composition {
        title,
        groups,
        empty,
        layout,
        diagnostic: None,
        source: ComposeSource::Default,
    })
}

fn validate_groups(
    model: &Model,
    raw_groups: Vec<RawGroup>,
) -> Result<Vec<ComposedGroup>, ComposeError> {
    let gutter = model.viewport.gutter_cells > 0;
    let known_groups: HashMap<u32, &super::Group> =
        model.groups.iter().map(|group| (group.id, group)).collect();
    let known_status: HashMap<u32, &str> = model
        .statuses
        .iter()
        .map(|(id, text)| (*id, text.as_str()))
        .collect();
    let mut seen_groups = HashSet::new();
    let mut seen_sessions = HashSet::new();
    let mut groups = Vec::new();
    for raw_group in raw_groups {
        if !seen_groups.insert(raw_group.id) {
            return Err(ComposeError::new("duplicate group target"));
        }
        let Some(group) = known_groups.get(&raw_group.id).copied() else {
            return Err(ComposeError::new("unknown group target"));
        };
        let navigable = group.kind == GroupKind::Node;
        let group_status = known_status
            .get(&group.id)
            .map(|text| (*text).to_string())
            .or_else(|| model.status_for(group.id));
        if navigable && group_status.is_none() {
            return Err(ComposeError::new("node target has no status fact"));
        }
        let rows = validate_target_rows(
            raw_group.rows,
            group.id,
            navigable,
            group.selected,
            group_status.as_deref(),
            navigable && gutter,
        )?;
        let known_sessions: HashMap<u32, &super::SessionEntry> = group
            .sessions
            .iter()
            .map(|session| (session.id, session))
            .collect();
        let mut sessions = Vec::new();
        for raw_session in raw_group.sessions {
            if !seen_sessions.insert(raw_session.id) {
                return Err(ComposeError::new("duplicate session target"));
            }
            let Some(session) = known_sessions.get(&raw_session.id).copied() else {
                return Err(ComposeError::new("session target outside its group"));
            };
            let status = known_status
                .get(&raw_session.id)
                .map(|text| (*text).to_string())
                .or_else(|| model.status_for(raw_session.id))
                .ok_or_else(|| ComposeError::new("session target has no status fact"))?;
            let rows = validate_target_rows(
                raw_session.rows,
                raw_session.id,
                true,
                session.selected,
                Some(status.as_str()),
                gutter,
            )?;
            sessions.push(ComposedSession {
                id: raw_session.id,
                rows,
            });
        }
        if sessions.len() != group.sessions.len() {
            return Err(ComposeError::new("missing session target"));
        }
        groups.push(ComposedGroup {
            id: raw_group.id,
            kind: group.kind,
            rows,
            sessions,
        });
    }
    Ok(groups)
}

fn validate_decorative(rows: &[RawRow]) -> Result<Vec<Row>, ComposeError> {
    rows.iter()
        .map(|row| {
            let (spans, status, status_index) = validate_spans(row, None)?;
            if status > 0 || status_index.is_some() {
                return Err(ComposeError::new(
                    "decorative rows cannot carry a status span",
                ));
            }
            Ok(Row {
                spans,
                target: None,
                selected: false,
                gutter: false,
                status: None,
            })
        })
        .collect()
}

fn validate_target_rows(
    rows: Vec<RawRow>,
    id: u32,
    navigable: bool,
    selected: bool,
    status: Option<&str>,
    gutter: bool,
) -> Result<Vec<Row>, ComposeError> {
    if rows.is_empty() {
        if !navigable {
            return Ok(Vec::new());
        }
        return Err(ComposeError::new("target has no row"));
    }
    let mut out = Vec::new();
    let mut status_spans = 0usize;
    for (index, row) in rows.into_iter().enumerate() {
        let (spans, count, status_index) = validate_spans(&row, status)?;
        status_spans += count;
        out.push(Row {
            spans,
            target: (navigable && index == 0).then_some(id),
            selected: selected && navigable && index == 0,
            gutter: gutter && (navigable || status.is_some()),
            status: status_index,
        });
    }
    if navigable && status_spans != 1 {
        return Err(ComposeError::new(
            "each target block needs exactly one status span",
        ));
    }
    if !navigable && status_spans > 0 {
        return Err(ComposeError::new(
            "a non-navigable block cannot carry a status span",
        ));
    }
    Ok(out)
}

fn validate_spans(
    row: &RawRow,
    status: Option<&str>,
) -> Result<(Vec<Span>, usize, Option<usize>), ComposeError> {
    if row.spans.is_empty() || row.spans.len() > MAX_SPANS_PER_ROW {
        return Err(ComposeError::new("row span count is out of bounds"));
    }
    let mut spans = Vec::with_capacity(row.spans.len());
    let mut status_spans = 0usize;
    let mut status_index = None;
    for raw in &row.spans {
        let text = match raw.field.as_deref() {
            Some("status") => {
                status_spans += 1;
                status_index = Some(spans.len());
                if !raw.text.is_empty() {
                    return Err(ComposeError::new(
                        "a status span must not carry its own text",
                    ));
                }
                status
                    .ok_or_else(|| ComposeError::new("status span outside a target"))?
                    .to_string()
            }
            Some(_) => return Err(ComposeError::new("unknown span field")),
            None => {
                if raw.text.len() > MAX_SPAN_BYTES {
                    return Err(ComposeError::new("span text is too long"));
                }
                raw.text.clone()
            }
        };
        if text.len() > MAX_SPAN_BYTES && raw.field.is_some() {
            return Err(ComposeError::new("status text is too long"));
        }
        let text = crate::text::strip_terminal_controls(&text).into_owned();
        spans.push(Span {
            text,
            fg: raw.fg,
            bg: raw.bg,
            bold: raw.bold,
            dim: raw.dim,
            italic: raw.italic,
            underline: raw.underline,
            reverse: raw.reverse,
        });
    }
    Ok((spans, status_spans, status_index))
}

fn count_rows(doc: &RawDoc) -> usize {
    doc.title.len()
        + doc.empty.len()
        + doc
            .groups
            .iter()
            .map(|group| {
                group.rows.len()
                    + group
                        .sessions
                        .iter()
                        .map(|session| session.rows.len())
                        .sum::<usize>()
            })
            .sum::<usize>()
}

fn count_spans(doc: &RawDoc) -> usize {
    let rows = doc
        .title
        .iter()
        .chain(doc.empty.iter())
        .chain(doc.groups.iter().flat_map(|group| group.rows.iter()))
        .chain(
            doc.groups
                .iter()
                .flat_map(|group| group.sessions.iter())
                .flat_map(|session| session.rows.iter()),
        );
    rows.map(|row| row.spans.len()).sum()
}
