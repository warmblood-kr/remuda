//! Typed relative history viewport; no raw byte replay or incremental baseline.
use remuda_core::agent::{Color, Cursor, MouseState, ScreenSnapshot, StyledCell};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};

/// Pinned parser subset: fixed cell text can truncate excess combining marks.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RenderingProfile {
    Vt100Fixed22,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Modes {
    pub mouse: MouseState,
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub alternate_screen: bool,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Viewport {
    pub schema: u16,
    pub rendering_profile: RenderingProfile,
    pub generation: u64,
    pub rows: u16,
    pub cols: u16,
    pub offset: u16,
    pub history_len: u16,
    pub history_total: u64,
    pub cells: Vec<Vec<StyledCell>>,
    pub wrapped: Vec<bool>,
    pub cursor: Cursor,
    pub modes: Modes,
}
impl Viewport {
    pub fn validate(&self) -> io::Result<()> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "typed viewport bounds");
        super::terminal::Terminal::validate_geometry(
            self.rows,
            self.cols,
            super::terminal::HISTORY_ROWS,
        )
        .map_err(|_| invalid())?;
        if self.schema != 1
            || self.offset > self.history_len
            || usize::from(self.history_len) > super::terminal::HISTORY_ROWS
            || self.cells.len() != usize::from(self.rows)
            || self.wrapped.len() != usize::from(self.rows)
            || self.cursor.row >= self.rows
            || self.cursor.col >= self.cols
        {
            return Err(invalid());
        }
        for row in &self.cells {
            // Pinned vt100 Cell has fixed 22-byte text storage. This cap states
            // retention, not complete combining-mark or emulator fidelity.
            if row.len() != usize::from(self.cols) || row.iter().any(|cell| cell.text.len() > 22) {
                return Err(invalid());
            }
        }
        Ok(())
    }
    /// What a plain `Viewport` answers: an older daemon refuses a viewport
    /// claiming more history than [`super::terminal::LEGACY_HISTORY_ROWS`].
    pub fn within_legacy_history(mut self) -> Self {
        let legacy = super::terminal::LEGACY_HISTORY_ROWS;
        self.history_len = self.history_len.min(legacy);
        self.offset = self.offset.min(self.history_len);
        self
    }
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        encode(self, super::api::MAX_SNAPSHOT_BYTES)
    }
    /// The `compact-view-v1` form: each row as runs of neighbouring cells
    /// that share a style, default fields left out. Exact cells come back
    /// from [`Viewport::decode`].
    pub fn encode_compact(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let runs = self
            .cells
            .iter()
            .map(|row| {
                let mut runs: Vec<Run> = Vec::new();
                for cell in row {
                    let style = Style::of(cell);
                    match runs.last_mut() {
                        Some(run) if run.style == style => run.c.push(cell.text.clone()),
                        _ => runs.push(Run {
                            c: vec![cell.text.clone()],
                            style,
                        }),
                    }
                }
                runs
            })
            .collect();
        encode(
            &CompactViewport {
                schema: COMPACT_SCHEMA,
                rendering_profile: RenderingProfile::Vt100Fixed22,
                generation: self.generation,
                rows: self.rows,
                cols: self.cols,
                offset: self.offset,
                history_len: self.history_len,
                history_total: self.history_total,
                runs,
                wrapped: self.wrapped.clone(),
                cursor: self.cursor,
                modes: self.modes.clone(),
            },
            super::api::MAX_SNAPSHOT_BYTES,
        )
    }
    /// Either wire form; the caller still validates the result.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        #[derive(Deserialize)]
        struct Schema {
            schema: u16,
        }
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "typed viewport encoding");
        let schema: Schema = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        if schema.schema != COMPACT_SCHEMA {
            return serde_json::from_slice(bytes).map_err(|_| invalid());
        }
        let compact: CompactViewport = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        let cols = usize::from(compact.cols);
        let mut cells = Vec::with_capacity(compact.runs.len());
        for runs in compact.runs {
            let mut row = Vec::with_capacity(cols);
            for run in runs {
                // Bounded before expanding: a row never holds more than `cols`.
                if run.c.is_empty() || run.c.len() > cols - row.len().min(cols) {
                    return Err(invalid());
                }
                row.extend(run.c.into_iter().map(|text| run.style.cell(text)));
            }
            cells.push(row);
        }
        Ok(Self {
            schema: 1,
            rendering_profile: compact.rendering_profile,
            generation: compact.generation,
            rows: compact.rows,
            cols: compact.cols,
            offset: compact.offset,
            history_len: compact.history_len,
            history_total: compact.history_total,
            cells,
            wrapped: compact.wrapped,
            cursor: compact.cursor,
            modes: compact.modes,
        })
    }
    pub fn into_screen(self) -> ScreenSnapshot {
        ScreenSnapshot {
            cells: self.cells,
            wrapped: self.wrapped,
            cursor: self.cursor,
            scrollback_len: usize::from(self.history_len),
            scrollback_total: usize::try_from(self.history_total).unwrap_or(usize::MAX),
        }
    }
}
const COMPACT_SCHEMA: u16 = 2;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactViewport {
    schema: u16,
    rendering_profile: RenderingProfile,
    generation: u64,
    rows: u16,
    cols: u16,
    offset: u16,
    history_len: u16,
    history_total: u64,
    runs: Vec<Vec<Run>>,
    wrapped: Vec<bool>,
    cursor: Cursor,
    modes: Modes,
}
/// Neighbouring cells with one style: their texts, in order.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    c: Vec<String>,
    #[serde(flatten)]
    style: Style,
}
#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
struct Style {
    #[serde(default, skip_serializing_if = "is_default")]
    fg: Color,
    #[serde(default, skip_serializing_if = "is_default")]
    bg: Color,
    #[serde(default, skip_serializing_if = "is_default")]
    bold: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    dim: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    italic: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    underline: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    inverse: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    wide: bool,
}
impl Style {
    fn of(cell: &StyledCell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            bold: cell.bold,
            dim: cell.dim,
            italic: cell.italic,
            underline: cell.underline,
            inverse: cell.inverse,
            wide: cell.wide,
        }
    }
    fn cell(&self, text: String) -> StyledCell {
        StyledCell {
            text,
            fg: self.fg,
            bg: self.bg,
            bold: self.bold,
            dim: self.dim,
            italic: self.italic,
            underline: self.underline,
            inverse: self.inverse,
            wide: self.wide,
        }
    }
}
fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}
fn encode(value: &impl Serialize, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(limit).map_err(io::Error::other)?;
    let mut writer = Bounded { bytes, limit };
    serde_json::to_writer(&mut writer, value).map_err(io::Error::other)?;
    Ok(writer.bytes)
}
struct Bounded {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("typed snapshot encoding budget"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "viewport_tests.rs"]
mod tests;
