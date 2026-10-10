//! One-shot Noise IK frames for cluster requests and responses.

use std::io;
use zeroize::Zeroizing;

const NOISE_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
const MAX_FRAME_SIZE: usize = 65_535;
/// Largest v1 payload after msg2's ephemeral key and authentication tag.
pub const MAX_RESPONSE_PAYLOAD: usize = MAX_FRAME_SIZE - 32 - 16;
const AEAD_TAG_SIZE: usize = 16;
const TIMESTAMP_SIZE: usize = std::mem::size_of::<i64>();
const NOISE_PROLOGUE: &[u8] = b"remuda-cluster-v1";

/// An encrypted request plus the initiator state needed to open its response.
pub struct SealedRequest {
    pub message: Vec<u8>,
    handshake: snow::HandshakeState,
    initiator_private: Zeroizing<Vec<u8>>,
}

/// An authenticated and decrypted request awaiting an encrypted response.
pub struct OpenedRequest {
    pub peer_static: Vec<u8>,
    pub ephemeral: [u8; 32],
    pub timestamp_seconds: i64,
    pub payload: Vec<u8>,
    /// Set by the listener when the peer asked for the chunked (v2) response.
    #[allow(dead_code)] // frame.rs is also compiled into tests that never read it
    pub chunked: bool,
    handshake: snow::HandshakeState,
}

/// Encrypt one timestamped request as Noise IK message 1.
pub fn seal_request(
    initiator_private: &[u8],
    responder_static: &[u8],
    timestamp_seconds: i64,
    payload: &[u8],
) -> io::Result<SealedRequest> {
    let params = parse_pattern()?;
    let mut handshake = snow::Builder::new(params)
        .prologue(NOISE_PROLOGUE)
        .map_err(frame_error)?
        .local_private_key(initiator_private)
        .map_err(frame_error)?
        .remote_public_key(responder_static)
        .map_err(frame_error)?
        .build_initiator()
        .map_err(frame_error)?;
    let mut plaintext = Vec::with_capacity(TIMESTAMP_SIZE + payload.len());
    plaintext.extend_from_slice(&timestamp_seconds.to_be_bytes());
    plaintext.extend_from_slice(payload);
    let mut message = vec![0; MAX_FRAME_SIZE];
    let length = handshake
        .write_message(&plaintext, &mut message)
        .map_err(frame_error)?;
    message.truncate(length);
    Ok(SealedRequest {
        message,
        handshake,
        initiator_private: Zeroizing::new(initiator_private.to_vec()),
    })
}

/// Authenticate and decrypt Noise IK message 1, retaining state for message 2.
pub fn open_request(responder_private: &[u8], message: &[u8]) -> io::Result<OpenedRequest> {
    if message.len() < 32 || message.len() > MAX_FRAME_SIZE {
        return Err(invalid_frame());
    }
    let mut ephemeral = [0; 32];
    ephemeral.copy_from_slice(&message[..32]);
    reject_low_order_dh(responder_private, &ephemeral)?;
    let mut handshake = snow::Builder::new(parse_pattern()?)
        .prologue(NOISE_PROLOGUE)
        .map_err(frame_error)?
        .local_private_key(responder_private)
        .map_err(frame_error)?
        .build_responder()
        .map_err(frame_error)?;
    let mut plaintext = vec![0; MAX_FRAME_SIZE];
    let length = handshake
        .read_message(message, &mut plaintext)
        .map_err(frame_error)?;
    if length < TIMESTAMP_SIZE {
        return Err(invalid_frame());
    }
    let timestamp_seconds = i64::from_be_bytes(
        plaintext[..TIMESTAMP_SIZE]
            .try_into()
            .map_err(|_| invalid_frame())?,
    );
    let peer_static = handshake
        .get_remote_static()
        .ok_or_else(invalid_frame)?
        .to_vec();
    reject_low_order_dh(responder_private, &peer_static)?;
    Ok(OpenedRequest {
        peer_static,
        ephemeral,
        timestamp_seconds,
        payload: plaintext[TIMESTAMP_SIZE..length].to_vec(),
        chunked: false,
        handshake,
    })
}

pub(super) fn reject_low_order_dh(private_key: &[u8], public_key: &[u8]) -> io::Result<()> {
    if private_key.len() != 32 || public_key.len() != 32 {
        return Err(invalid_frame());
    }
    use snow::resolvers::CryptoResolver;
    let params = parse_pattern()?;
    let resolver = snow::resolvers::DefaultResolver;
    let mut dh = resolver.resolve_dh(&params.dh).ok_or_else(invalid_frame)?;
    dh.set(private_key);
    let mut shared = Zeroizing::new([0; 32]);
    dh.dh(public_key, &mut shared[..]).map_err(frame_error)?;
    if shared.iter().all(|byte| *byte == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "low-order Noise DH result",
        ));
    }
    Ok(())
}

/// Refuse a static X25519 public key that produces the all-zero DH result.
pub fn validate_static_public_key(public_key: &[u8]) -> io::Result<()> {
    reject_low_order_dh(&[0x42; 32], public_key)
}

#[cfg(test)]
pub(super) fn request_with_low_order_key(
    responder_static: &[u8],
    low_order_static: bool,
) -> io::Result<Vec<u8>> {
    use snow::resolvers::{CryptoResolver, DefaultResolver};
    use snow::types::{Cipher, Dh, Hash, Random};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct LowOrderResolver {
        dh_instances: AtomicUsize,
        zero_static: bool,
    }

    struct LowOrderDh {
        inner: Box<dyn Dh>,
        zero_public: bool,
    }

    impl Dh for LowOrderDh {
        fn name(&self) -> &'static str {
            self.inner.name()
        }
        fn pub_len(&self) -> usize {
            self.inner.pub_len()
        }
        fn priv_len(&self) -> usize {
            self.inner.priv_len()
        }
        fn set(&mut self, private: &[u8]) {
            self.inner.set(private);
        }
        fn generate(&mut self, rng: &mut dyn Random) -> Result<(), snow::Error> {
            self.inner.generate(rng)
        }
        fn pubkey(&self) -> &[u8] {
            if self.zero_public {
                &[0; 32]
            } else {
                self.inner.pubkey()
            }
        }
        fn privkey(&self) -> &[u8] {
            self.inner.privkey()
        }
        fn dh(&self, public: &[u8], out: &mut [u8]) -> Result<(), snow::Error> {
            if self.zero_public {
                out.fill(0);
                Ok(())
            } else {
                self.inner.dh(public, out)
            }
        }
    }

    impl CryptoResolver for LowOrderResolver {
        fn resolve_rng(&self) -> Option<Box<dyn Random>> {
            DefaultResolver.resolve_rng()
        }
        fn resolve_dh(&self, choice: &snow::params::DHChoice) -> Option<Box<dyn Dh>> {
            let instance = self.dh_instances.fetch_add(1, Ordering::Relaxed);
            let zero_public = if instance == 0 {
                self.zero_static
            } else {
                instance == 1 && !self.zero_static
            };
            Some(Box::new(LowOrderDh {
                inner: DefaultResolver.resolve_dh(choice)?,
                zero_public,
            }))
        }
        fn resolve_hash(&self, choice: &snow::params::HashChoice) -> Option<Box<dyn Hash>> {
            DefaultResolver.resolve_hash(choice)
        }
        fn resolve_cipher(&self, choice: &snow::params::CipherChoice) -> Option<Box<dyn Cipher>> {
            DefaultResolver.resolve_cipher(choice)
        }
    }

    let initiator = snow::Builder::new(parse_pattern()?)
        .generate_keypair()
        .map_err(frame_error)?;
    let resolver = LowOrderResolver {
        dh_instances: AtomicUsize::new(0),
        zero_static: low_order_static,
    };
    let mut handshake = snow::Builder::with_resolver(parse_pattern()?, Box::new(resolver))
        .prologue(NOISE_PROLOGUE)
        .map_err(frame_error)?
        .local_private_key(&initiator.private)
        .map_err(frame_error)?
        .remote_public_key(responder_static)
        .map_err(frame_error)?
        .build_initiator()
        .map_err(frame_error)?;
    let mut plaintext = 1_000_i64.to_be_bytes().to_vec();
    plaintext.extend_from_slice(b"{}");
    let mut message = vec![0; MAX_FRAME_SIZE];
    let length = handshake
        .write_message(&plaintext, &mut message)
        .map_err(frame_error)?;
    message.truncate(length);
    Ok(message)
}

/// Encrypt a response as Noise IK message 2.
pub fn seal_response(mut request: OpenedRequest, payload: &[u8]) -> io::Result<Vec<u8>> {
    let mut message = vec![0; MAX_FRAME_SIZE];
    let length = request
        .handshake
        .write_message(payload, &mut message)
        .map_err(frame_error)?;
    message.truncate(length);
    Ok(message)
}

/// Authenticate and decrypt Noise IK message 2 for the matching request.
pub fn open_response(mut request: SealedRequest, message: &[u8]) -> io::Result<Vec<u8>> {
    if message.len() < 32 || message.len() > MAX_FRAME_SIZE {
        return Err(invalid_frame());
    }
    reject_low_order_dh(&request.initiator_private, &message[..32])?;
    let mut payload = vec![0; MAX_FRAME_SIZE];
    let length = request
        .handshake
        .read_message(message, &mut payload)
        .map_err(frame_error)?;
    payload.truncate(length);
    Ok(payload)
}

// Deviation from the design note: no flags byte and no FINAL flag. The authenticated total length
// (msg2 header) is mandatory and proves completeness, so FINAL is redundant; empty input is zero
// records with total 0.
/// Largest plaintext carried by a single transport record (Noise message cap minus the AEAD tag).
pub const MAX_RECORD_PLAINTEXT: usize = MAX_FRAME_SIZE - AEAD_TAG_SIZE;
/// Hard cap on the total plaintext of a chunked response.
pub const MAX_RESPONSE_TOTAL: usize = 4 * 1024 * 1024;
/// Hard cap on the number of records: ceil(MAX_RESPONSE_TOTAL / MAX_RECORD_PLAINTEXT) = 65.
/// Enough because msg2 carries chunk0 outside these records, so records hold at most the cap.
pub const MAX_RECORDS: usize = MAX_RESPONSE_TOTAL.div_ceil(MAX_RECORD_PLAINTEXT);

// ponytail: PR1 is pure functions; wired into client/listener in later PRs.
/// Seal `plaintext` as a sequence of `[u16 BE len][AEAD record]`; the counter nonce orders records.
#[allow(dead_code)]
pub fn seal_records(transport: &mut snow::TransportState, plaintext: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    seal_records_into(transport, plaintext, &mut out)?;
    Ok(out)
}

fn seal_records_into(
    transport: &mut snow::TransportState,
    plaintext: &[u8],
    out: &mut Vec<u8>,
) -> io::Result<()> {
    if plaintext.len() > MAX_RESPONSE_TOTAL {
        return Err(too_large());
    }
    let mut buf = vec![0; MAX_FRAME_SIZE];
    for chunk in plaintext.chunks(MAX_RECORD_PLAINTEXT) {
        let n = transport
            .write_message(chunk, &mut buf)
            .map_err(frame_error)?;
        out.extend_from_slice(&(n as u16).to_be_bytes());
        out.extend_from_slice(&buf[..n]);
    }
    Ok(())
}

fn response_wire_size(payload_len: usize) -> usize {
    let chunk0 = payload_len.min(MAX_CHUNK0);
    let remaining = payload_len - chunk0;
    let record_count = remaining.div_ceil(MAX_RECORD_PLAINTEXT);
    2 + 32 + 16 + MSG2_HEADER + payload_len + record_count * (2 + AEAD_TAG_SIZE)
}

/// Marker for PR3: the peer sent a response version we do not understand (`ErrorKind::Unsupported`).
pub const NEWER_FORMAT_MSG: &str = "peer uses newer cluster response format";

fn newer_format() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, NEWER_FORMAT_MSG)
}

fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "chunked response exceeds the size cap",
    )
}

/// Incremental reassembler for records produced by `seal_records`.
#[allow(dead_code)]
pub struct RecordOpener {
    transport: snow::TransportState,
    expected: usize,
    pending: Vec<u8>,
    plaintext: Zeroizing<Vec<u8>>,
    records: usize,
    failed: bool,
}

#[allow(dead_code)]
impl RecordOpener {
    /// `expected_total` is the authenticated total plaintext length (checked against the cap).
    pub fn new(transport: snow::TransportState, expected_total: usize) -> io::Result<Self> {
        Self::with_prefix(transport, expected_total, &[])
    }

    fn with_prefix(
        transport: snow::TransportState,
        expected_total: usize,
        prefix: &[u8],
    ) -> io::Result<Self> {
        if expected_total > MAX_RESPONSE_TOTAL {
            return Err(too_large());
        }
        if prefix.len() > expected_total {
            return Err(invalid_frame());
        }
        let mut plaintext = Zeroizing::new(Vec::with_capacity(expected_total));
        plaintext.extend_from_slice(prefix);
        Ok(Self {
            transport,
            expected: expected_total,
            pending: Vec::new(),
            plaintext,
            records: 0,
            failed: false,
        })
    }

    /// Feed bytes as they arrive. Memory only grows with bytes actually received.
    /// Any error is terminal: later `push`/`finish` calls fail without touching state.
    pub fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.failed {
            return Err(invalid_frame());
        }
        let result = self.push_inner(bytes);
        self.failed = result.is_err();
        result
    }

    fn push_inner(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            if self.plaintext.len() == self.expected {
                return Err(invalid_frame());
            }
            let need = match self.record_len()? {
                None => 2 - self.pending.len(),
                Some(len) => 2 + len - self.pending.len(),
            };
            let take = need.min(bytes.len());
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self
                .record_len()?
                .is_some_and(|len| self.pending.len() == 2 + len)
            {
                self.open_record()?;
            }
        }
        Ok(())
    }

    /// Length prefix of the record being read, once its two bytes are in and valid.
    fn record_len(&self) -> io::Result<Option<usize>> {
        if self.pending.len() < 2 {
            return Ok(None);
        }
        let len = u16::from_be_bytes([self.pending[0], self.pending[1]]) as usize;
        if len <= AEAD_TAG_SIZE {
            return Err(invalid_frame());
        }
        Ok(Some(len))
    }

    fn open_record(&mut self) -> io::Result<()> {
        self.records += 1;
        if self.records > MAX_RECORDS {
            return Err(too_large());
        }
        let mut out = vec![0; self.pending.len() - 2];
        let n = self
            .transport
            .read_message(&self.pending[2..], &mut out)
            .map_err(frame_error)?;
        if self.plaintext.len() + n > self.expected {
            return Err(invalid_frame());
        }
        self.plaintext.extend_from_slice(&out[..n]);
        self.pending.clear();
        Ok(())
    }

    /// Bytes currently buffered for an unfinished record.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Succeeds only when exactly `expected_total` plaintext bytes arrived in whole records.
    pub fn finish(mut self) -> io::Result<Vec<u8>> {
        if self.failed || !self.pending.is_empty() || self.plaintext.len() != self.expected {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated chunked response",
            ));
        }
        Ok(std::mem::take(&mut *self.plaintext))
    }
}

const RESPONSE_V2: u8 = 0x01;
const MSG2_HEADER: usize = 1 + 4;
/// Largest chunk0 that fits msg2: frame cap - tag - ephemeral key - header.
pub const MAX_CHUNK0: usize = MAX_FRAME_SIZE - AEAD_TAG_SIZE - 32 - MSG2_HEADER;

/// v2 response body: `[u16 len][msg2]` then records. msg2 plaintext is `[ver][u32 BE total][chunk0]`.
#[allow(dead_code)]
pub fn seal_response_chunked(mut request: OpenedRequest, payload: &[u8]) -> io::Result<Vec<u8>> {
    if payload.len() > MAX_RESPONSE_TOTAL {
        return Err(too_large());
    }
    let (chunk0, rest) = payload.split_at(payload.len().min(MAX_CHUNK0));
    let mut out = Vec::with_capacity(response_wire_size(payload.len()));
    {
        let mut plain = Vec::with_capacity(MSG2_HEADER + chunk0.len());
        plain.push(RESPONSE_V2);
        plain.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        plain.extend_from_slice(chunk0);
        let mut msg = vec![0; MAX_FRAME_SIZE];
        let n = request
            .handshake
            .write_message(&plain, &mut msg)
            .map_err(frame_error)?;
        out.extend_from_slice(&(n as u16).to_be_bytes());
        out.extend_from_slice(&msg[..n]);
    }
    let mut transport = request
        .handshake
        .into_transport_mode()
        .map_err(frame_error)?;
    seal_records_into(&mut transport, rest, &mut out)?;
    Ok(out)
}

/// Open a v2 body from a slice.
#[allow(dead_code)]
pub fn open_response_chunked(request: SealedRequest, body: &[u8]) -> io::Result<Vec<u8>> {
    open_response_chunked_from(request, &mut &*body)
}

/// Open a v2 body from a reader; nothing past msg2 is read until its total_len passes the cap.
#[allow(dead_code)]
pub fn open_response_chunked_from(
    mut request: SealedRequest,
    reader: &mut impl io::Read,
) -> io::Result<Vec<u8>> {
    let mut len = [0; 2];
    reader.read_exact(&mut len)?;
    let mut msg = vec![0; u16::from_be_bytes(len) as usize];
    reader.read_exact(&mut msg)?;
    let plain = decrypt_msg2(&mut request, &msg)?;
    match plain.first() {
        Some(&RESPONSE_V2) => {}
        Some(_) => return Err(newer_format()),
        None => return Err(invalid_frame()),
    }
    let header: [u8; 4] = plain
        .get(1..MSG2_HEADER)
        .and_then(|h| h.try_into().ok())
        .ok_or_else(invalid_frame)?;
    let total = u32::from_be_bytes(header) as usize;
    if total > MAX_RESPONSE_TOTAL {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response total_len exceeds the size cap",
        ));
    }
    let chunk0 = &plain[MSG2_HEADER..];
    if chunk0.len() > total {
        return Err(invalid_frame());
    }
    let transport = request
        .handshake
        .into_transport_mode()
        .map_err(frame_error)?;
    let mut opener = RecordOpener::with_prefix(transport, total, chunk0)?;
    // Bound the read; the opener rejects any byte after the final record anyway.
    let mut limited = io::Read::take(
        &mut *reader,
        (MAX_RECORDS * (2 + MAX_FRAME_SIZE) + 1) as u64,
    );
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = io::Read::read(&mut limited, &mut buf)?;
        if n == 0 {
            break;
        }
        opener.push(&buf[..n])?;
    }
    opener.finish()
}

/// Open either format: a bare msg2 is v1 (returned untouched), a length-prefixed msg2 is v2.
#[allow(dead_code)]
pub fn open_response_any(mut request: SealedRequest, body: &[u8]) -> io::Result<Vec<u8>> {
    // A failed read_message restores the handshake state, so trying v1 first is safe.
    if let Ok(payload) = decrypt_msg2(&mut request, body) {
        // JSON never starts with the v2 version byte; a prefix-stripped v2 msg2 would land here.
        if payload.first() == Some(&RESPONSE_V2) {
            return Err(invalid_frame());
        }
        return Ok(payload);
    }
    open_response_chunked(request, body)
}

fn decrypt_msg2(request: &mut SealedRequest, message: &[u8]) -> io::Result<Vec<u8>> {
    if message.len() < 32 || message.len() > MAX_FRAME_SIZE {
        return Err(invalid_frame());
    }
    reject_low_order_dh(&request.initiator_private, &message[..32])?;
    let mut payload = vec![0; MAX_FRAME_SIZE];
    let n = request
        .handshake
        .read_message(message, &mut payload)
        .map_err(frame_error)?;
    payload.truncate(n);
    Ok(payload)
}

fn parse_pattern() -> io::Result<snow::params::NoiseParams> {
    NOISE_PATTERN.parse().map_err(frame_error)
}

fn frame_error(error: snow::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn invalid_frame() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid Noise IK frame")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_response_payload_limit_includes_the_msg2_ephemeral_key() {
        let initiator = snow::Builder::new(NOISE_PATTERN.parse().unwrap())
            .generate_keypair()
            .unwrap();
        let responder = snow::Builder::new(NOISE_PATTERN.parse().unwrap())
            .generate_keypair()
            .unwrap();
        for (size, accepted) in [(65_486, true), (65_487, true), (65_488, false)] {
            let sealed =
                seal_request(&initiator.private, &responder.public, 1000, b"request").unwrap();
            let opened = open_request(&responder.private, &sealed.message).unwrap();
            assert_eq!(
                seal_response(opened, &vec![0; size]).is_ok(),
                accepted,
                "payload size {size}"
            );
        }
    }

    #[test]
    fn rejects_low_order_dh_result() {
        let private = [7; 32];
        assert!(reject_low_order_dh(&private, &[0; 32]).is_err());
        let peer = snow::Builder::new(NOISE_PATTERN.parse().unwrap())
            .generate_keypair()
            .unwrap();
        assert!(reject_low_order_dh(&private, &peer.public).is_ok());
    }

    #[test]
    fn open_response_rejects_low_order_responder_ephemeral() {
        let initiator = snow::Builder::new(NOISE_PATTERN.parse().unwrap())
            .generate_keypair()
            .unwrap();
        let responder = snow::Builder::new(NOISE_PATTERN.parse().unwrap())
            .generate_keypair()
            .unwrap();
        let sealed = seal_request(&initiator.private, &responder.public, 1000, b"request").unwrap();
        let mut message = vec![0; 32];
        message.extend_from_slice(&[0; 64]);
        let error = open_response(sealed, &message).unwrap_err();
        assert_eq!(error.to_string(), "low-order Noise DH result");
    }

    #[test]
    fn open_request_identifies_low_order_ephemeral_and_static_keys() {
        let responder = snow::Builder::new(NOISE_PATTERN.parse().unwrap())
            .generate_keypair()
            .unwrap();
        for low_order_static in [false, true] {
            let message = request_with_low_order_key(&responder.public, low_order_static).unwrap();
            assert!(message.len() > 32);
            let error = open_request(&responder.private, &message).err().unwrap();
            assert_eq!(error.to_string(), "low-order Noise DH result");
        }
    }

    #[test]
    fn request_prologue_is_part_of_the_noise_handshake() {
        let initiator = snow::Builder::new(NOISE_PATTERN.parse().unwrap())
            .generate_keypair()
            .unwrap();
        let responder = snow::Builder::new(NOISE_PATTERN.parse().unwrap())
            .generate_keypair()
            .unwrap();
        let sealed = seal_request(&initiator.private, &responder.public, 1000, b"request").unwrap();
        assert!(open_request(&responder.private, &sealed.message).is_ok());

        let mut legacy = snow::Builder::new(NOISE_PATTERN.parse().unwrap())
            .local_private_key(&initiator.private)
            .unwrap()
            .remote_public_key(&responder.public)
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut plaintext = [0; 64];
        plaintext[..8].copy_from_slice(&1000i64.to_be_bytes());
        plaintext[8..15].copy_from_slice(b"request");
        let mut message = [0; 128];
        let len = legacy
            .write_message(&plaintext[..15], &mut message)
            .unwrap();
        assert!(open_request(&responder.private, &message[..len]).is_err());
    }

    fn keypair() -> snow::Keypair {
        snow::Builder::new(NOISE_PATTERN.parse().unwrap())
            .generate_keypair()
            .unwrap()
    }

    /// (responder/sender, initiator/receiver) transport states of one handshake.
    fn transport_pair() -> (snow::TransportState, snow::TransportState) {
        let (i, r) = (keypair(), keypair());
        let sealed = seal_request(&i.private, &r.public, 1000, b"req").unwrap();
        let mut opened = open_request(&r.private, &sealed.message).unwrap();
        let (mut msg, mut out) = (vec![0; MAX_FRAME_SIZE], vec![0; MAX_FRAME_SIZE]);
        let n = opened.handshake.write_message(b"", &mut msg).unwrap();
        let mut init = sealed.handshake;
        init.read_message(&msg[..n], &mut out).unwrap();
        (
            opened.handshake.into_transport_mode().unwrap(),
            init.into_transport_mode().unwrap(),
        )
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn open_all(rx: snow::TransportState, total: usize, wire: &[u8]) -> io::Result<Vec<u8>> {
        let mut opener = RecordOpener::new(rx, total)?;
        opener.push(wire)?;
        opener.finish()
    }

    fn roundtrip(n: usize) {
        let (tx, rx) = transport_pair();
        let (mut tx, plain) = (tx, data(n));
        let wire = seal_records(&mut tx, &plain).unwrap();
        assert_eq!(open_all(rx, n, &wire).unwrap(), plain);
    }

    #[test]
    fn roundtrips_1_mib() {
        roundtrip(1 << 20);
    }

    #[test]
    fn roundtrips_exactly_the_cap_and_record_edges() {
        for n in [
            0,
            1,
            MAX_RECORD_PLAINTEXT,
            MAX_RECORD_PLAINTEXT + 1,
            MAX_RESPONSE_TOTAL,
        ] {
            roundtrip(n);
        }
        assert_eq!(MAX_RECORDS, 65);
    }

    #[test]
    fn maximum_response_wire_buffer_reserves_only_its_wire_size() {
        let initiator = keypair();
        let responder = keypair();
        let sealed = seal_request(&initiator.private, &responder.public, 1000, b"req").unwrap();
        let opened = open_request(&responder.private, &sealed.message).unwrap();
        let wire = seal_response_chunked(opened, &data(MAX_RESPONSE_TOTAL)).unwrap();
        assert_eq!(wire.capacity(), wire.len());
    }

    #[test]
    fn cap_plus_one_is_rejected_at_seal_and_open() {
        let (mut tx, rx) = transport_pair();
        assert!(seal_records(&mut tx, &data(MAX_RESPONSE_TOTAL + 1)).is_err());
        assert!(RecordOpener::new(rx, MAX_RESPONSE_TOTAL + 1).is_err());
    }

    #[test]
    fn truncated_stream_never_completes() {
        let n = 3 * MAX_RECORD_PLAINTEXT + 10;
        let (mut tx, _) = transport_pair();
        let wire = seal_records(&mut tx, &data(n)).unwrap();
        let mut cuts = vec![0, 1, 2, 3, wire.len() - 1];
        let mut at = 0;
        while at < wire.len() {
            let len = u16::from_be_bytes([wire[at], wire[at + 1]]) as usize;
            at += 2 + len;
            cuts.extend([at - len, at - 1, at, at + 1]);
        }
        for cut in cuts.into_iter().filter(|c| *c < wire.len()) {
            let (mut tx, rx) = transport_pair();
            let wire = seal_records(&mut tx, &data(n)).unwrap();
            let mut opener = RecordOpener::new(rx, n).unwrap();
            assert!(opener.push(&wire[..cut]).is_ok(), "cut {cut}");
            assert!(opener.finish().is_err(), "cut {cut} completed");
        }
    }

    fn two_records() -> (snow::TransportState, Vec<u8>, Vec<Vec<u8>>) {
        let (mut tx, rx) = transport_pair();
        let wire = seal_records(&mut tx, &data(2 * MAX_RECORD_PLAINTEXT)).unwrap();
        let split = 2 + u16::from_be_bytes([wire[0], wire[1]]) as usize;
        (
            rx,
            Vec::new(),
            vec![wire[..split].to_vec(), wire[split..].to_vec()],
        )
    }

    #[test]
    fn duplicate_record_is_rejected() {
        let (rx, _, r) = two_records();
        let wire = [r[0].clone(), r[0].clone()].concat();
        assert!(open_all(rx, 2 * MAX_RECORD_PLAINTEXT, &wire).is_err());
    }

    #[test]
    fn reordered_records_are_rejected() {
        let (rx, _, r) = two_records();
        let wire = [r[1].clone(), r[0].clone()].concat();
        assert!(open_all(rx, 2 * MAX_RECORD_PLAINTEXT, &wire).is_err());
    }

    #[test]
    fn record_spliced_from_another_session_is_rejected() {
        let (rx, _, r) = two_records();
        let (mut other_tx, _) = transport_pair();
        let other = seal_records(&mut other_tx, &data(2 * MAX_RECORD_PLAINTEXT)).unwrap();
        let split = 2 + u16::from_be_bytes([other[0], other[1]]) as usize;
        let wire = [r[0].clone(), other[split..].to_vec()].concat();
        assert!(open_all(rx, 2 * MAX_RECORD_PLAINTEXT, &wire).is_err());
    }

    #[test]
    fn trailing_garbage_is_rejected() {
        let (mut tx, rx) = transport_pair();
        let mut wire = seal_records(&mut tx, &data(100)).unwrap();
        wire.push(0);
        assert!(open_all(rx, 100, &wire).is_err());
    }

    #[test]
    fn bad_length_prefixes_are_rejected() {
        for prefix in [0u16, 1, 16, u16::MAX] {
            let (_, rx) = transport_pair();
            let mut opener = RecordOpener::new(rx, 100).unwrap();
            let mut wire = prefix.to_be_bytes().to_vec();
            wire.extend(data(70_000));
            assert!(opener.push(&wire).is_err(), "prefix {prefix}");
        }
    }

    #[test]
    fn one_byte_at_a_time_keeps_buffering_bounded() {
        let n = 2 * MAX_RECORD_PLAINTEXT + 5;
        let (mut tx, rx) = transport_pair();
        let wire = seal_records(&mut tx, &data(n)).unwrap();
        let mut opener = RecordOpener::new(rx, n).unwrap();
        for byte in &wire {
            opener.push(std::slice::from_ref(byte)).unwrap();
            assert!(opener.pending_len() <= 2 + MAX_RECORD_PLAINTEXT + AEAD_TAG_SIZE);
        }
        assert_eq!(opener.pending_len(), 0);
        assert_eq!(opener.finish().unwrap(), data(n));
    }

    #[test]
    fn empty_input_is_zero_records_and_zero_total() {
        let (mut tx, rx) = transport_pair();
        assert!(seal_records(&mut tx, b"").unwrap().is_empty());
        assert!(open_all(rx, 0, &[]).unwrap().is_empty());
        let (_, rx) = transport_pair();
        assert!(open_all(rx, 1, &[]).is_err());
    }

    #[test]
    fn opener_stays_failed_after_a_corrupt_record() {
        let (rx, _, r) = two_records();
        let mut bad = r[0].clone();
        bad[10] ^= 1;
        let mut opener = RecordOpener::new(rx, 2 * MAX_RECORD_PLAINTEXT).unwrap();
        assert!(opener.push(&bad).is_err());
        assert!(opener.push(&r[1]).is_err());
        assert!(opener.push(&[]).is_err());
        assert!(opener.finish().is_err());
    }

    #[test]
    fn more_than_max_records_is_refused_by_the_count_cap() {
        let (mut tx, rx) = transport_pair();
        let mut opener = RecordOpener::new(rx, MAX_RESPONSE_TOTAL).unwrap();
        let mut buf = vec![0; 64];
        for i in 0..=MAX_RECORDS {
            let n = tx.write_message(&[1], &mut buf).unwrap();
            let mut rec = (n as u16).to_be_bytes().to_vec();
            rec.extend_from_slice(&buf[..n]);
            let result = opener.push(&rec);
            if i < MAX_RECORDS {
                result.unwrap();
            } else {
                assert_eq!(result.unwrap_err().to_string(), too_large().to_string());
            }
        }
    }

    #[test]
    fn plaintext_beyond_expected_total_is_refused_after_authentication() {
        let (mut tx, rx) = transport_pair();
        let wire = seal_records(&mut tx, &data(20)).unwrap();
        let mut opener = RecordOpener::new(rx, 10).unwrap();
        let error = opener.push(&wire).unwrap_err();
        assert_eq!(error.to_string(), invalid_frame().to_string());
    }

    #[test]
    fn max_prefix_with_full_body_fails_authentication() {
        let (_, rx) = transport_pair();
        let mut opener = RecordOpener::new(rx, 100).unwrap();
        let mut wire = u16::MAX.to_be_bytes().to_vec();
        wire.extend(data(MAX_FRAME_SIZE));
        let error = opener.push(&wire).unwrap_err();
        assert_ne!(error.to_string(), invalid_frame().to_string());
        assert_ne!(error.to_string(), too_large().to_string());
    }

    #[test]
    fn claimed_length_without_data_buffers_only_what_arrived() {
        let (_, rx) = transport_pair();
        let mut opener = RecordOpener::new(rx, 100).unwrap();
        let mut wire = u16::MAX.to_be_bytes().to_vec();
        wire.extend(data(1000));
        opener.push(&wire).unwrap();
        assert_eq!(opener.pending_len(), 1002);
        assert!(opener.finish().is_err());
    }

    // ---- v2 wrappers ----
    fn pair() -> (OpenedRequest, SealedRequest) {
        let (i, r) = (keypair(), keypair());
        let sealed = seal_request(&i.private, &r.public, 1000, b"req").unwrap();
        (open_request(&r.private, &sealed.message).unwrap(), sealed)
    }

    /// Hand-built v2 body: arbitrary header fields, chunk0 and records payload.
    fn forge(mut o: OpenedRequest, ver: u8, total: u32, chunk0: &[u8], rest: &[u8]) -> Vec<u8> {
        let mut plain = vec![ver];
        plain.extend_from_slice(&total.to_be_bytes());
        plain.extend_from_slice(chunk0);
        let mut msg = vec![0; MAX_FRAME_SIZE];
        let n = o.handshake.write_message(&plain, &mut msg).unwrap();
        let mut out = (n as u16).to_be_bytes().to_vec();
        out.extend_from_slice(&msg[..n]);
        let mut t = o.handshake.into_transport_mode().unwrap();
        out.extend_from_slice(&seal_records(&mut t, rest).unwrap());
        out
    }

    fn v2_roundtrip(n: usize) {
        let (o, s) = pair();
        let plain = data(n);
        let body = seal_response_chunked(o, &plain).unwrap();
        assert_eq!(open_response_chunked(s, &body).unwrap(), plain);
    }

    #[test]
    fn v2_roundtrips_edges() {
        for n in [
            0,
            1,
            100,
            MAX_CHUNK0 - 1,
            MAX_CHUNK0,
            MAX_CHUNK0 + 1,
            1 << 20,
            MAX_RESPONSE_TOTAL,
        ] {
            v2_roundtrip(n);
        }
    }

    #[test]
    fn v2_small_response_is_msg2_only() {
        let (o, _s) = pair();
        let body = seal_response_chunked(o, &data(10)).unwrap();
        let msg_len = u16::from_be_bytes([body[0], body[1]]) as usize;
        assert_eq!(body.len(), 2 + msg_len);
    }

    #[test]
    fn v2_cap_plus_one_rejected_at_seal_and_open() {
        let (o, _s) = pair();
        assert!(seal_response_chunked(o, &data(MAX_RESPONSE_TOTAL + 1)).is_err());
        let (o, s) = pair();
        let body = forge(o, 1, MAX_RESPONSE_TOTAL as u32 + 1, b"x", b"");
        let err = open_response_chunked(s, &body).unwrap_err();
        assert_eq!(err.to_string(), "response total_len exceeds the size cap");
    }

    #[test]
    fn v2_over_cap_total_is_rejected_before_reading_records() {
        struct Tripwire<'a>(&'a [u8]);
        impl io::Read for Tripwire<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                assert!(!self.0.is_empty(), "read past msg2");
                let n = self.0.len().min(buf.len());
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let (o, s) = pair();
        let body = forge(o, 1, u32::MAX, b"x", b"");
        let msg_end = 2 + u16::from_be_bytes([body[0], body[1]]) as usize;
        let err = open_response_chunked_from(s, &mut Tripwire(&body[..msg_end])).unwrap_err();
        assert_eq!(err.to_string(), "response total_len exceeds the size cap");
    }

    #[test]
    fn v2_unknown_version_has_exact_message() {
        let (o, s) = pair();
        let body = forge(o, 2, 1, b"x", b"");
        let err = open_response_chunked(s, &body).unwrap_err();
        assert_eq!(err.to_string(), "peer uses newer cluster response format");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn v2_lying_totals_are_rejected() {
        for total in [5, 200] {
            let (o, s) = pair();
            let body = forge(o, 1, total, &data(10), &data(MAX_RECORD_PLAINTEXT));
            assert!(open_response_chunked(s, &body).is_err(), "total {total}");
        }
        let (o, s) = pair();
        let body = forge(o, 1, 5, &data(10), b"");
        assert!(open_response_chunked(s, &body).is_err());
    }

    #[test]
    fn v2_chunk0_at_max_fills_msg2_exactly_and_one_more_spills() {
        let (o, _s) = pair();
        let body = seal_response_chunked(o, &data(MAX_CHUNK0)).unwrap();
        assert_eq!(
            u16::from_be_bytes([body[0], body[1]]) as usize,
            MAX_FRAME_SIZE
        );
        assert_eq!(body.len(), 2 + MAX_FRAME_SIZE);
        let (o, _s) = pair();
        let body = seal_response_chunked(o, &data(MAX_CHUNK0 + 1)).unwrap();
        assert_eq!(
            u16::from_be_bytes([body[0], body[1]]) as usize,
            MAX_FRAME_SIZE
        );
        assert_eq!(body.len(), 2 + MAX_FRAME_SIZE + 2 + 1 + AEAD_TAG_SIZE);
    }

    #[test]
    fn v2_lies_high_with_too_few_records_is_rejected() {
        let (o, s) = pair();
        let body = forge(o, 1, 10 + 100, &data(10), &data(50));
        assert!(open_response_chunked(s, &body).is_err());
    }

    #[test]
    fn v2_short_msg2_plaintext_is_invalid_not_newer() {
        for len in [0usize, 1, 3, 4] {
            let (mut o, s) = pair();
            let mut plain = vec![RESPONSE_V2; len];
            plain.truncate(len);
            let mut msg = vec![0; MAX_FRAME_SIZE];
            let n = o.handshake.write_message(&plain, &mut msg).unwrap();
            let mut body = (n as u16).to_be_bytes().to_vec();
            body.extend_from_slice(&msg[..n]);
            let err = open_response_chunked(s, &body).unwrap_err();
            assert_ne!(err.kind(), io::ErrorKind::Unsupported, "len {len}");
        }
    }

    #[test]
    fn open_response_any_rejects_stripped_prefix_and_corruption() {
        let (o, s) = pair();
        let body = seal_response_chunked(o, b"hi").unwrap();
        assert!(open_response_any(s, &body[2..]).is_err());
        let (o, s) = pair();
        let mut body = seal_response_chunked(o, b"hi").unwrap();
        body[10] ^= 1;
        let err = open_response_any(s, &body).unwrap_err();
        assert_ne!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn v2_truncation_trailing_dup_swap_splice_are_rejected() {
        let plain = data(3 * MAX_RECORD_PLAINTEXT);
        let (o, s) = pair();
        let body = seal_response_chunked(o, &plain).unwrap();
        let rec = 2 + MAX_RECORD_PLAINTEXT + AEAD_TAG_SIZE;
        let msg_end = 2 + u16::from_be_bytes([body[0], body[1]]) as usize;
        assert!(open_response_chunked(s, &body[..msg_end + rec]).is_err()); // record boundary
        let (o, s) = pair();
        let b = seal_response_chunked(o, &plain).unwrap();
        assert!(open_response_chunked(s, &b[..b.len() - 7]).is_err()); // mid-record
        let (o, s) = pair();
        let mut long = seal_response_chunked(o, &plain).unwrap();
        long.push(0);
        assert!(open_response_chunked(s, &long).is_err());
        for order in [[0, 0, 2], [1, 0, 2]] {
            let (o, s) = pair();
            let b = seal_response_chunked(o, &plain).unwrap();
            let mut tampered = b[..msg_end].to_vec();
            for i in order {
                tampered.extend_from_slice(recs_of(&b, msg_end, rec)[i]);
            }
            assert!(open_response_chunked(s, &tampered).is_err());
        }
        // record from another exchange
        let (o2, _s2) = pair();
        let other = seal_response_chunked(o2, &plain).unwrap();
        let other_end = 2 + u16::from_be_bytes([other[0], other[1]]) as usize;
        let (o, s) = pair();
        let mut b = seal_response_chunked(o, &plain).unwrap();
        b.truncate(msg_end);
        b.extend_from_slice(&other[other_end..]);
        assert!(open_response_chunked(s, &b).is_err());
    }

    fn recs_of(b: &[u8], msg_end: usize, rec: usize) -> Vec<&[u8]> {
        b[msg_end..].chunks(rec).collect()
    }

    #[test]
    fn open_response_any_handles_v1_and_v2() {
        let (o, s) = pair();
        let v1 = seal_response(o, b"{\"ok\":true}").unwrap();
        assert_eq!(open_response_any(s, &v1).unwrap(), b"{\"ok\":true}");
        let (o, s) = pair();
        let plain = data(200_000);
        let v2 = seal_response_chunked(o, &plain).unwrap();
        assert_eq!(open_response_any(s, &v2).unwrap(), plain);
        let (o, s) = pair();
        let v2 = seal_response_chunked(o, b"").unwrap();
        assert!(open_response_any(s, &v2).unwrap().is_empty());
    }
}
