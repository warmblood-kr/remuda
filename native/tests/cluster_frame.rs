#[path = "../src/net/frame.rs"]
mod frame;

fn keypair() -> snow::Keypair {
    snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap()
}

#[test]
fn static_public_key_validation_rejects_a_known_low_order_point() {
    let mut low_order = [0u8; 32];
    low_order[0] = 1;
    assert!(frame::validate_static_public_key(&low_order).is_err());
}

#[test]
fn frame_noise_ik_round_trip_authenticates_and_protects_payloads() {
    let initiator = keypair();
    let responder = keypair();
    let sealed = frame::seal_request(
        &initiator.private,
        &responder.public,
        1_800_000_000,
        b"input bytes",
    )
    .unwrap();
    let opened = frame::open_request(&responder.private, &sealed.message).unwrap();
    assert_eq!(opened.peer_static, initiator.public.as_slice());
    assert_ne!(opened.ephemeral, [0; 32]);
    assert_eq!(opened.timestamp_seconds, 1_800_000_000);
    assert_eq!(opened.payload, b"input bytes");
    let response = frame::seal_response(opened, b"ack").unwrap();
    assert_eq!(frame::open_response(sealed, &response).unwrap(), b"ack");
}

#[test]
fn frame_rejects_tampered_request_and_response() {
    let responder = keypair();
    let initiator = keypair();
    let sealed = frame::seal_request(
        &initiator.private,
        &responder.public,
        1_800_000_000,
        b"secret",
    )
    .unwrap();
    let mut request = sealed.message.clone();
    *request.last_mut().unwrap() ^= 1;
    assert!(frame::open_request(&responder.private, &request).is_err());

    let opened = frame::open_request(&responder.private, &sealed.message).unwrap();
    let mut response = frame::seal_response(opened, b"ack").unwrap();
    *response.last_mut().unwrap() ^= 1;
    assert!(frame::open_response(sealed, &response).is_err());
}

#[test]
fn oversized_response_payload_cannot_fit_a_noise_frame() {
    let responder = keypair();
    let initiator = keypair();
    let sealed =
        frame::seal_request(&initiator.private, &responder.public, 1000, b"request").unwrap();
    let opened = frame::open_request(&responder.private, &sealed.message).unwrap();
    let oversized = vec![0; frame::MAX_RESPONSE_PAYLOAD + 1];
    assert!(frame::seal_response(opened, &oversized).is_err());
}
