//! Strict one-line encoding and pin verification for cluster invitations.

use super::encoding;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use zeroize::Zeroizing;

const JOIN_LINE_VERSION: &str = "remuda-join-v1";
const MAX_JOIN_LINE_SIZE: usize = 256;

/// An invitation naming the issuer endpoint, pinned Noise key, and bearer token.
#[derive(PartialEq, Eq)]
pub struct JoinLine {
    pub issuer_addr: SocketAddr,
    pub issuer_fingerprint: String,
    pub issuer_static_pubkey: [u8; 32],
    pub token: Zeroizing<String>,
}

impl fmt::Debug for JoinLine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinLine")
            .field("issuer_addr", &self.issuer_addr)
            .field("issuer_static_pubkey", &self.issuer_static_pubkey)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl JoinLine {
    /// Encode the invitation as one canonical, whitespace-delimited line.
    pub fn encode(&self) -> io::Result<String> {
        validate_endpoint(self.issuer_addr)?;
        validate_token(&self.token)?;
        validate_static_key(&self.issuer_static_pubkey)?;
        Ok(format!(
            "{JOIN_LINE_VERSION} {} {} {} {}",
            self.issuer_addr,
            self.issuer_fingerprint,
            encoding::encode_base64(&self.issuer_static_pubkey),
            self.token.as_str()
        ))
    }

    /// Parse an invitation with exactly five canonical fields on one line.
    pub fn decode(line: &str) -> io::Result<Self> {
        if line.len() > MAX_JOIN_LINE_SIZE || line.contains(['\r', '\n', '\t']) {
            return Err(invalid_join_line());
        }
        let fields: Vec<&str> = line.split(' ').collect();
        if fields.len() != 5 || fields.iter().any(|field| field.is_empty()) {
            return Err(invalid_join_line());
        }
        if fields[0] != JOIN_LINE_VERSION {
            return Err(invalid_join_line());
        }
        let issuer_addr: SocketAddr = fields[1].parse().map_err(|_| invalid_join_line())?;
        if issuer_addr.to_string() != fields[1] {
            return Err(invalid_join_line());
        }
        validate_endpoint(issuer_addr)?;
        let public_key = decode_canonical_key(fields[3], "issuer public key")?;
        let issuer_static_pubkey = public_key.try_into().map_err(|_| invalid_join_line())?;
        validate_static_key(&issuer_static_pubkey)?;
        if encoding::fingerprint(&issuer_static_pubkey) != fields[2] {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "issuer key does not match pinned fingerprint",
            ));
        }
        validate_token(fields[4])?;
        Ok(Self {
            issuer_addr,
            issuer_fingerprint: fields[2].to_owned(),
            issuer_static_pubkey,
            token: Zeroizing::new(fields[4].to_owned()),
        })
    }

    /// Verify the shown fingerprint before any handshake is attempted.
    pub fn verify_pin(&self, shown_fingerprint: &str) -> io::Result<()> {
        if self.issuer_fingerprint != shown_fingerprint
            || encoding::fingerprint(&self.issuer_static_pubkey) != shown_fingerprint
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "issuer static key does not match the shown fingerprint",
            ));
        }
        Ok(())
    }
}

fn validate_token(value: &str) -> io::Result<()> {
    let decoded =
        Zeroizing::new(encoding::decode_base64(value).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid join token encoding")
        })?);
    if decoded.len() != 32 || encoding::encode_base64(&decoded) != value {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "join token must be 32 canonical base64 bytes",
        ));
    }
    Ok(())
}

fn validate_static_key(public_key: &[u8; 32]) -> io::Result<()> {
    if public_key.iter().all(|byte| *byte == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "issuer static public key cannot be all zero",
        ));
    }
    crate::net::frame::validate_static_public_key(public_key)
}

fn decode_canonical_key(value: &str, description: &str) -> io::Result<Vec<u8>> {
    let decoded = encoding::decode_base64(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid {description} base64 encoding"),
        )
    })?;
    if decoded.len() != 32 || encoding::encode_base64(&decoded) != value {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} must be 32 canonical base64 bytes"),
        ));
    }
    Ok(decoded)
}

pub fn validate_endpoint(address: SocketAddr) -> io::Result<()> {
    let ip = address.ip();
    let broadcast = ip == std::net::IpAddr::V4(std::net::Ipv4Addr::BROADCAST);
    let scoped = matches!(address, SocketAddr::V6(value) if value.scope_id() != 0);
    if address.port() == 0 || ip.is_unspecified() || ip.is_multicast() || broadcast || scoped {
        return Err(invalid_join_line());
    }
    Ok(())
}

fn invalid_join_line() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid cluster join line")
}
