//! Shared size bounds for synchronous and deferred daemon replies.

/// Maximum logical reply content, and maximum combined deferred stdout/stderr.
pub const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;

/// Maximum serialized response frame, including room for JSON escaping and
/// base64 encoding of a deferred reply at the logical content limit.
pub const fn max_reply_wire_bytes() -> usize {
    MAX_REPLY_BYTES * 6 + 256
}
