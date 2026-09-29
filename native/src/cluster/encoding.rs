//! Shared Base64 and node fingerprint helpers for cluster identities and the CLI.

use std::io;

pub fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        result.push(TABLE[(a >> 2) as usize] as char);
        result.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        result.push(if chunk.len() > 1 {
            TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            TABLE[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    result
}

pub fn decode_base64(value: &str) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(value.len() * 3 / 4);
    let mut chunk = [0u8; 4];
    let mut count = 0;
    for byte in value.bytes() {
        if byte == b'=' {
            break;
        }
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid registry public key encoding",
                ))
            }
        };
        chunk[count] = digit;
        count += 1;
        if count == 4 {
            bytes.extend_from_slice(&[
                (chunk[0] << 2) | (chunk[1] >> 4),
                (chunk[1] << 4) | (chunk[2] >> 2),
                (chunk[2] << 6) | chunk[3],
            ]);
            count = 0;
        }
    }
    match count {
        0 => {}
        2 => bytes.push((chunk[0] << 2) | (chunk[1] >> 4)),
        3 => bytes.extend_from_slice(&[
            (chunk[0] << 2) | (chunk[1] >> 4),
            (chunk[1] << 4) | (chunk[2] >> 2),
        ]),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid registry public key encoding",
            ))
        }
    }
    if encode_base64(&bytes).trim_end_matches('=') != value.trim_end_matches('=') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "non-canonical registry public key encoding",
        ));
    }
    Ok(bytes)
}

pub fn fingerprint(public_key: &[u8]) -> String {
    use snow::resolvers::CryptoResolver;
    let params: snow::params::NoiseParams = "Noise_NN_25519_ChaChaPoly_SHA256"
        .parse()
        .expect("static Noise pattern");
    let resolver = snow::resolvers::DefaultResolver;
    let mut hash = resolver
        .resolve_hash(&params.hash)
        .expect("Snow SHA-256 provider");
    hash.input(public_key);
    let mut digest = vec![0; hash.hash_len()];
    hash.result(&mut digest);
    format!("SHA256:{}", encode_base64(&digest).trim_end_matches('='))
}
