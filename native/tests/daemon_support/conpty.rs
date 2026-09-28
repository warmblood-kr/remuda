//! Small helpers for driving programs hosted by Windows ConPTY.

use std::io::Write;

/// Answer cursor-position reports emitted by ConPTY clients during startup.
/// `pending` retains a partial escape sequence across output reads.
pub fn answer_conpty_cursor_queries(writer: &mut impl Write, pending: &mut Vec<u8>, bytes: &[u8]) {
    const QUERY: &[u8] = b"\x1b[6n";
    pending.extend_from_slice(bytes);
    while let Some(offset) = pending
        .windows(QUERY.len())
        .position(|window| window == QUERY)
    {
        if writer.write_all(b"\x1b[1;1R").is_err() || writer.flush().is_err() {
            return;
        }
        pending.drain(..offset + QUERY.len());
    }
    if pending.len() >= QUERY.len() {
        pending.drain(..pending.len() - (QUERY.len() - 1));
    }
}
