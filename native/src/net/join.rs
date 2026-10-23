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
    check_join_status(header.lines().next().unwrap_or_default())?;
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

/// Text of the error for a join frame the issuer rejected as outside its
/// replay window. A join frame is fresh, so this means the clocks disagree.
pub const JOIN_CLOCK_SKEW: &str =
    "the inviting machine rejected the request as stale: the two clocks differ by more than 60 seconds";

/// Text of the error for a join the issuer turned away for load or rate limits.
pub const JOIN_RATE_LIMITED: &str = "the inviting machine is busy or rate-limiting join attempts";

/// Map the issuer's HTTP status line. Statuses the listener sends before it
/// looks at the token (409 stale frame, 429 rate limit) leave the token unused.
fn check_join_status(status_line: &str) -> io::Result<()> {
    let code = status_line.split(' ').nth(1).unwrap_or_default();
    match code {
        "200" => Ok(()),
        "409" => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            JOIN_CLOCK_SKEW,
        )),
        "429" => Err(io::Error::new(io::ErrorKind::WouldBlock, JOIN_RATE_LIMITED)),
        _ => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "join was refused",
        )),
    }
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
    fn join_status_names_clock_skew_and_rate_limits() {
        assert!(check_join_status("HTTP/1.1 200 OK").is_ok());
        let stale = check_join_status("HTTP/1.1 409 Conflict").unwrap_err();
        assert_eq!(stale.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(stale.to_string(), JOIN_CLOCK_SKEW);
        let limited = check_join_status("HTTP/1.1 429 Too Many Requests").unwrap_err();
        assert_eq!(limited.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(limited.to_string(), JOIN_RATE_LIMITED);
        for refused in [
            "HTTP/1.1 403 Forbidden",
            "HTTP/1.1 503 Service Unavailable",
            "",
            "garbage",
        ] {
            let error = check_join_status(refused).unwrap_err();
            assert_eq!(error.to_string(), "join was refused", "{refused}");
        }
    }

    #[test]
    fn busy_join_response_is_retryable() {
        let error = check_join_response(br#"{"joined":false,"retry":true}"#).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(error.to_string(), "join issuer is busy; retry");
    }
}
