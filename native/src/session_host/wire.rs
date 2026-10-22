//! Separately versioned serialized contract; never a Rust trait ABI.
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub(crate) const MAGIC: [u8; 4] = *b"RMHS";
pub(crate) const MAJOR: u16 = 1;
pub(crate) const MAX_FRAME: usize = 64 * 1024;
/// A host's launch arrives as one frame on its stdin, before any RPC. It
/// carries the whole inherited environment and argv, so it gets its own cap,
/// sized to hold what an in-process launch accepts (see `entry::validate`).
pub(crate) const MAX_BOOTSTRAP: usize = 4 * 1024 * 1024;
pub(crate) const MAX_HELLO: usize = 4096;
pub(crate) const MAX_INPUT: usize = 8192;
pub(crate) const MAX_STEPS: usize = 64;
pub(crate) const MAX_PAUSE_MS: u64 = remuda_core::Session::MAX_TOTAL_PAUSE.as_millis() as u64;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hello {
    pub magic: [u8; 4],
    pub major: u16,
    pub minor: u16,
    pub required: Vec<String>,
}

impl Hello {
    pub fn negotiate(&self, supported: &[&str]) -> Result<u16, &'static str> {
        if self.magic != MAGIC || self.major != MAJOR {
            return Err("unsupported host protocol");
        }
        if self.required.len() > 16
            || self
                .required
                .iter()
                .any(|s| s.len() > 64 || !supported.contains(&s.as_str()))
        {
            return Err("unsupported required capability");
        }
        Ok(0)
    }
}

/// An entire logical act, retained through all its pauses and delayed work.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", deny_unknown_fields)]
pub(crate) enum InputAct {
    Bytes(Vec<u8>),
    SendLine(String),
    InputText(String),
    TypeText { text: String, settle_ms: u64 },
    Submit(String),
    Clear(Vec<u8>),
    Feed(Vec<Step>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", deny_unknown_fields)]
pub(crate) enum Step {
    Bytes(Vec<u8>),
    Pause(u64),
}

impl InputAct {
    /// Hash the typed, normalized act, not attacker-selected digest bytes.
    pub fn digest(&self) -> Result<[u8; 32], &'static str> {
        use sha2::{Digest, Sha256};
        self.validate()?;
        let encoded = serde_json::to_vec(self).map_err(|_| "input serialization failed")?;
        Ok(Sha256::digest(encoded).into())
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        let (bytes, pause) = match self {
            Self::Bytes(b) | Self::Clear(b) => (b.len(), 0),
            Self::SendLine(s) | Self::InputText(s) | Self::Submit(s) => (s.len(), 0),
            Self::TypeText { text, settle_ms } => (text.len(), *settle_ms),
            Self::Feed(steps) => {
                if steps.is_empty() || steps.len() > MAX_STEPS {
                    return Err("invalid input step count");
                }
                steps
                    .iter()
                    .try_fold((0_usize, 0_u64), |(bytes, pause), step| {
                        let next = match step {
                            Step::Bytes(b) => (bytes.checked_add(b.len()), Some(pause)),
                            Step::Pause(ms) => (Some(bytes), pause.checked_add(*ms)),
                        };
                        Ok::<_, &'static str>((
                            next.0.ok_or("input length overflow")?,
                            next.1.ok_or("pause overflow")?,
                        ))
                    })?
            }
        };
        if bytes > MAX_INPUT || pause > MAX_PAUSE_MS {
            return Err("input act limit exceeded");
        }
        Ok(())
    }
}

/// Caller selects a smaller handshake bound before authentication. Runtime
/// must additionally enforce connection count and read/write deadlines.
pub(crate) fn read_frame(reader: &mut impl Read, limit: usize) -> io::Result<Vec<u8>> {
    read_frame_within(reader, limit.min(MAX_FRAME))
}

/// The one launch frame on a host's stdin, up to [`MAX_BOOTSTRAP`].
pub(crate) fn read_bootstrap(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    read_frame_within(reader, MAX_BOOTSTRAP)
}

fn read_frame_within(reader: &mut impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 || len > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "host frame length limit",
        ));
    }
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

pub(crate) fn write_frame(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    write_frame_within(writer, bytes, MAX_FRAME)
}

/// Write the launch frame a host reads with [`read_bootstrap`].
pub(crate) fn write_bootstrap(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    write_frame_within(writer, bytes, MAX_BOOTSTRAP)
}

fn write_frame_within(writer: &mut impl Write, bytes: &[u8], limit: usize) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "host frame length limit",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_lengths_fail_before_payload_read() {
        for length in [0, MAX_HELLO + 1, MAX_FRAME + 1, u32::MAX as usize] {
            let error = read_frame(&mut &(length as u32).to_be_bytes()[..], MAX_HELLO).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
        assert_eq!(
            read_frame(&mut &[0, 0, 0, 2, 1][..], MAX_FRAME)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
    #[test]
    fn exact_frame_bound_roundtrips_and_trailing_frames_are_separate() {
        let payload = vec![7; MAX_FRAME];
        let mut wire = Vec::new();
        write_frame(&mut wire, &payload).unwrap();
        write_frame(&mut wire, b"next").unwrap();
        let mut reader = &wire[..];
        assert_eq!(read_frame(&mut reader, MAX_FRAME).unwrap(), payload);
        assert_eq!(read_frame(&mut reader, MAX_FRAME).unwrap(), b"next");
        assert!(write_frame(&mut Vec::new(), &vec![0; MAX_FRAME + 1]).is_err());
    }
    #[test]
    fn negotiation_and_full_act_limits() {
        let mut hello = Hello {
            magic: MAGIC,
            major: MAJOR,
            minor: 20,
            required: vec!["snapshot".into()],
        };
        assert_eq!(hello.negotiate(&["snapshot"]), Ok(0));
        assert!(hello.negotiate(&[]).is_err());
        hello.major += 1;
        assert!(hello.negotiate(&["snapshot"]).is_err());
        assert_ne!(
            InputAct::Bytes(b"x".to_vec()).digest().unwrap(),
            InputAct::Clear(b"x".to_vec()).digest().unwrap()
        );
        assert_eq!(
            InputAct::SendLine("hello".into()).digest().unwrap(),
            InputAct::SendLine("hello".into()).digest().unwrap()
        );
        assert!(InputAct::Feed(vec![
            Step::Bytes(vec![0; MAX_INPUT]),
            Step::Pause(MAX_PAUSE_MS)
        ])
        .validate()
        .is_ok());
        assert!(InputAct::TypeText {
            text: "x".into(),
            settle_ms: MAX_PAUSE_MS + 1
        }
        .validate()
        .is_err());
        assert!(InputAct::Bytes(vec![0; MAX_INPUT + 1]).validate().is_err());
        assert!(
            InputAct::Feed(vec![Step::Pause(MAX_PAUSE_MS), Step::Pause(1)])
                .validate()
                .is_err()
        );
        assert!(InputAct::Feed(vec![Step::Pause(u64::MAX), Step::Pause(1)])
            .validate()
            .is_err());
    }
}
