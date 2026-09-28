//! Send the one-shot encrypted Join frame to a pinned issuer.

use crate::cluster::join_line::JoinLine;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::net::TcpStream;
use std::time::Duration;

const MAX_JOIN_RESPONSE_BYTES: usize = 64 * 1024;

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
    if opened != b"{\"joined\":true}" {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "join was refused",
        ));
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
}
