//! Typed relative history viewport; no raw byte replay or incremental baseline.
use remuda_core::agent::{Cursor, MouseState, ScreenSnapshot, StyledCell};
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
