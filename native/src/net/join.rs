//! Send the one-shot encrypted Join frame to a pinned issuer.

use crate::cluster::join_line::JoinLine;
use serde::Deserialize;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::net::TcpStream;
use std::time::Duration;

const MAX_JOIN_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinResponse {
    joined: bool,
    #[serde(default)]
    retry: bool,
}

/// Complete a Join exchange after the caller has checked the separate pin.
pub fn join(
    invitation: &JoinLine,
    initiator_private: &[u8],
    timestamp_seconds: i64,
    endpoint: Option<SocketAddr>,
) -> io::Result<()> {
    let payload = serde_json::json!({
        "join": {
            "token": invitation.token.as_str(),
            "endpoint": endpoint.map(|address| address.to_string()),
        }
    });
    let payload = serde_json::to_vec(&payload).map_err(io::Error::other)?;
    let sealed = super::frame::seal_request(
        initiator_private,
        &invitation.issuer_static_pubkey,
        timestamp_seconds,
        &payload,
    )?;
    let mut stream = TcpStream::connect_timeout(&invitation.issuer_addr, Duration::from_secs(10))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    write!(
        stream,
        "POST /cluster HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        invitation.issuer_addr,
        sealed.message.len()
    )?;
    stream.write_all(&sealed.message)?;
    let response = read_join_response(&mut stream)?;
    let split = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .ok_or_else(invalid_join_response)?;
    let header = std::str::from_utf8(&response[..split]).map_err(io::Error::other)?;
    if !header
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 200 "))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "join was refused",
        ));
    }
    let opened = super::frame::open_response(sealed, &response[split + 4..])?;
    check_join_response(&opened)?;
    crate::cluster::record_join_success_local(
        &invitation.issuer_static_pubkey,
        invitation.issuer_addr,
        endpoint,
    )?;
    crate::cluster::replication::bootstrap_from_join_issuer(
        invitation.issuer_addr,
        &invitation.issuer_static_pubkey,
        initiator_private,
    )?;
    Ok(())
}

fn check_join_response(opened: &[u8]) -> io::Result<()> {
    let join_response: JoinResponse =
        serde_json::from_slice(opened).map_err(|_| invalid_join_response())?;
    if join_response.retry && !join_response.joined {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "join issuer is busy; retry",
        ));
    }
    if !join_response.joined {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "join was refused",
        ));
    }
    if join_response.retry {
        return Err(invalid_join_response());
    }
    Ok(())
}

fn invalid_join_response() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid join response")
}

fn read_join_response(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut response = Vec::new();
    reader
        .by_ref()
        .take((MAX_JOIN_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut response)?;
    if response.len() > MAX_JOIN_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "join response exceeds its size limit",
        ));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn join_response_read_is_bounded_to_64_kibibytes() {
        let response = vec![b'x'; MAX_JOIN_RESPONSE_BYTES];
        assert_eq!(
            read_join_response(Cursor::new(&response)).unwrap(),
            response
        );
        assert!(read_join_response(Cursor::new(vec![b'x'; MAX_JOIN_RESPONSE_BYTES + 1])).is_err());
    }

    #[test]
    fn busy_join_response_is_retryable() {
        let error = check_join_response(br#"{"joined":false,"retry":true}"#).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(error.to_string(), "join issuer is busy; retry");
    }
}
