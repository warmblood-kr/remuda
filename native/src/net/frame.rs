//! One-shot Noise IK frames for cluster requests and responses.

use std::io;
use zeroize::Zeroizing;

const NOISE_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
const MAX_FRAME_SIZE: usize = 65_535;
pub const MAX_RESPONSE_PAYLOAD: usize = MAX_FRAME_SIZE - 16;
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

        let error = open_response(sealed, &message).err().unwrap();
        assert_eq!(error.to_string(), "low-order Noise DH result");
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
}
