//! Idempotent, bounded byte batches for one session.

use crate::agent::AgentError;
use core::fmt;
use core::time::Duration;
use std::collections::VecDeque;

pub const INPUT_RING_CAPACITY: usize = 256;
pub const MAX_INPUT_CLIENTS: usize = 16;
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
pub const INPUT_RATE_BYTES_PER_SECOND: usize = 256 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputError {
    InvalidSequence,
    InvalidLength,
    RateLimited,
    Unavailable,
}

impl fmt::Display for InputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidSequence => "input sequence must start at 1",
            Self::InvalidLength => "input must contain 1 to 65536 bytes",
            Self::RateLimited => "session input rate limit exceeded",
            Self::Unavailable => "session input rate check unavailable",
        };
        formatter.write_str(message)
    }
}

#[derive(Default)]
pub struct InputRateLimiter {
    window_second: Option<u64>,
    bytes_in_window: usize,
}

impl InputRateLimiter {
    pub fn check_rate(&mut self, now: Duration, bytes: usize) -> Result<(), InputError> {
        if bytes == 0 || bytes > MAX_INPUT_BYTES {
            return Err(InputError::InvalidLength);
        }
        let second = now.as_secs();
        if self.window_second != Some(second) {
            self.window_second = Some(second);
            self.bytes_in_window = 0;
        }
        if self.bytes_in_window.saturating_add(bytes) > INPUT_RATE_BYTES_PER_SECOND {
            return Err(InputError::RateLimited);
        }
        self.bytes_in_window += bytes;
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub struct InputBatch<'a> {
    pub instance_id: &'a str,
    pub client_id: [u8; 16],
    pub seq: u64,
    pub bytes: &'a [u8],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputOutcome {
    Ack { duplicate: bool },
    Uncertain,
    WrongInstance,
    Exited,
}

#[derive(Default)]
pub struct InputDeduplicator {
    clients: VecDeque<ClientHistory>,
}

struct ClientHistory {
    id: [u8; 16],
    entries: VecDeque<SequenceResult>,
    evicted_watermark: u64,
}

struct SequenceResult {
    seq: u64,
    applied: bool,
}

impl InputDeduplicator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply<F>(&mut self, client_id: [u8; 16], seq: u64, write: F) -> InputOutcome
    where
        F: FnOnce() -> Result<(), AgentError>,
    {
        if let Some(index) = self
            .clients
            .iter()
            .position(|client| client.id == client_id)
        {
            let mut client = self.clients.remove(index).expect("located client exists");
            let outcome = apply_known(&mut client, seq, write);
            self.clients.push_back(client);
            return outcome;
        }
        if seq != 1 {
            return InputOutcome::Uncertain;
        }
        let mut client = ClientHistory {
            id: client_id,
            entries: VecDeque::new(),
            evicted_watermark: 0,
        };
        let outcome = write_and_remember(&mut client, seq, write);
        if self.clients.len() == MAX_INPUT_CLIENTS {
            self.clients.pop_front();
        }
        self.clients.push_back(client);
        outcome
    }
}

fn apply_known<F>(client: &mut ClientHistory, seq: u64, write: F) -> InputOutcome
where
    F: FnOnce() -> Result<(), AgentError>,
{
    if let Some(entry) = client.entries.iter().find(|entry| entry.seq == seq) {
        return if entry.applied {
            InputOutcome::Ack { duplicate: true }
        } else {
            InputOutcome::Uncertain
        };
    }
    if seq <= client.evicted_watermark {
        return InputOutcome::Uncertain;
    }
    write_and_remember(client, seq, write)
}

fn write_and_remember<F>(client: &mut ClientHistory, seq: u64, write: F) -> InputOutcome
where
    F: FnOnce() -> Result<(), AgentError>,
{
    let applied = write().is_ok();
    if client.entries.len() == INPUT_RING_CAPACITY {
        if let Some(evicted) = client.entries.pop_front() {
            client.evicted_watermark = client.evicted_watermark.max(evicted.seq);
        }
    }
    client.entries.push_back(SequenceResult { seq, applied });
    if applied {
        InputOutcome::Ack { duplicate: false }
    } else {
        InputOutcome::Uncertain
    }
}

pub fn validate_batch(client_id: &str, seq: u64, bytes: &[u8]) -> Result<[u8; 16], String> {
    let client_id = parse_client_id(client_id)?;
    if seq == 0 {
        return Err("input sequence must start at 1".into());
    }
    if bytes.is_empty() || bytes.len() > MAX_INPUT_BYTES {
        return Err(format!("input must contain 1 to {MAX_INPUT_BYTES} bytes"));
    }
    Ok(client_id)
}

pub fn parse_client_id(value: &str) -> Result<[u8; 16], String> {
    if value.len() != 32 {
        return Err("client_id must be 32 hexadecimal characters".into());
    }
    let mut output = [0; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0]).ok_or_else(invalid_client_id)?;
        let low = hex_nibble(pair[1]).ok_or_else(invalid_client_id)?;
        output[index] = high << 4 | low;
    }
    Ok(output)
}

fn invalid_client_id() -> String {
    "client_id must be 32 hexadecimal characters".into()
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}
