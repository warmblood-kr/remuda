//! One-shot Noise IK frames for cluster requests and responses.

use std::io;
use zeroize::Zeroizing;

const NOISE_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
const MAX_FRAME_SIZE: usize = 65_535;
pub const MAX_RESPONSE_PAYLOAD: usize = MAX_FRAME_SIZE - 16;
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

/// Largest plaintext carried by a single transport record (Noise message cap minus the AEAD tag).
pub const MAX_RECORD_PLAINTEXT: usize = MAX_FRAME_SIZE - AEAD_TAG_SIZE;
/// Hard cap on the total plaintext of a chunked response.
pub const MAX_RESPONSE_TOTAL: usize = 4 * 1024 * 1024;
/// Hard cap on the number of records in one chunked response.
pub const MAX_RECORDS: usize = MAX_RESPONSE_TOTAL.div_ceil(MAX_RECORD_PLAINTEXT);

// ponytail: PR1 is pure functions; wired into client/listener in later PRs.
/// Seal `plaintext` as a sequence of `[u16 BE len][AEAD record]`; the counter nonce orders records.
#[allow(dead_code)]
pub fn seal_records(transport: &mut snow::TransportState, plaintext: &[u8]) -> io::Result<Vec<u8>> {
    if plaintext.len() > MAX_RESPONSE_TOTAL {
        return Err(too_large());
    }
    let mut out = Vec::new();
    let mut buf = vec![0; MAX_FRAME_SIZE];
    for chunk in plaintext.chunks(MAX_RECORD_PLAINTEXT) {
        let n = transport
            .write_message(chunk, &mut buf)
            .map_err(frame_error)?;
        out.extend_from_slice(&(n as u16).to_be_bytes());
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "chunked response exceeds the size cap",
    )
}

/// Incremental reassembler for records produced by `seal_records`.
#[allow(dead_code)]
pub struct RecordOpener {}

#[allow(dead_code)]
impl RecordOpener {
    /// `expected_total` is the authenticated total plaintext length (checked against the cap).
    pub fn new(_transport: snow::TransportState, _expected_total: usize) -> io::Result<Self> {
        unimplemented!()
    }
    pub fn push(&mut self, _bytes: &[u8]) -> io::Result<()> {
        unimplemented!()
    }
    /// Bytes currently buffered for an unfinished record.
    pub fn pending_len(&self) -> usize {
        unimplemented!()
    }
    pub fn finish(self) -> io::Result<Vec<u8>> {
        unimplemented!()
    }
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
            assert!(opener.pending_len() <= 2 + MAX_FRAME_SIZE);
        }
        assert_eq!(opener.pending_len(), 0);
        assert_eq!(opener.finish().unwrap(), data(n));
    }
}
