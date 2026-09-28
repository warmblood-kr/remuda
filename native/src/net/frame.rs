//! One-shot Noise IK frames for cluster requests and responses.

use std::io;
use zeroize::Zeroizing;

const NOISE_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
const MAX_FRAME_SIZE: usize = 65_535;
const TIMESTAMP_SIZE: usize = std::mem::size_of::<i64>();

/// An encrypted request plus the initiator state needed to open its response.
pub struct SealedRequest {
    pub message: Vec<u8>,
    handshake: snow::HandshakeState,
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
    Ok(SealedRequest { message, handshake })
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

fn reject_low_order_dh(private_key: &[u8], public_key: &[u8]) -> io::Result<()> {
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
        return Err(invalid_frame());
    }
    Ok(())
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
    if message.len() > MAX_FRAME_SIZE {
        return Err(invalid_frame());
    }
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
}
