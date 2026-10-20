//! vt100 takes nonzero sizes. Remuda's sizes are already at least 1, so a zero
//! here is clamped to 1 rather than threaded through every caller.
use std::num::NonZeroU16;

pub fn dim(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

pub fn parser(rows: u16, cols: u16, scrollback: usize) -> vt100::Parser {
    vt100::Parser::new(dim(rows), dim(cols), scrollback)
}

pub fn parser_with_callbacks<CB: vt100::Callbacks>(
    rows: u16,
    cols: u16,
    scrollback: usize,
    callbacks: CB,
) -> vt100::Parser<CB> {
    vt100::Parser::new_with_callbacks(dim(rows), dim(cols), scrollback, callbacks)
}

pub fn set_size(screen: &mut vt100::Screen, rows: u16, cols: u16) {
    screen.set_size(dim(rows), dim(cols));
}

/// `(rows, cols)` as plain numbers.
pub fn size(screen: &vt100::Screen) -> (u16, u16) {
    let (rows, cols) = screen.size();
    (rows.get(), cols.get())
}
