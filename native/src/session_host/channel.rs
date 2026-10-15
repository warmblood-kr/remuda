//! Bounded authenticated channel. Platform verifies kernel identity first.
//! Any framing/crypto/I/O failure consumes the connection, never resumes it.
use super::wire::{read_frame, write_frame, MAX_HELLO};
use snow::{Builder, HandshakeState, TransportState};
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};
use zeroize::Zeroize;
const CIPHER_FRAME: usize = 65_535;
const PAYLOAD: usize = CIPHER_FRAME - 16;
const TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) trait LocalIo: Read + Write {
    fn prepare(&self) -> io::Result<()>;
    fn wait(&self, writing: bool, remaining: Duration) -> io::Result<()>;
}
#[cfg(unix)]
impl LocalIo for std::os::unix::net::UnixStream {
    fn prepare(&self) -> io::Result<()> {
        self.set_nonblocking(true)
    }
    fn wait(&self, writing: bool, remaining: Duration) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let until = Instant::now() + remaining;
        loop {
            let remaining = until
                .checked_duration_since(Instant::now())
                .ok_or_else(deadline_error)?;
            let mut descriptor = libc::pollfd {
                fd: self.as_raw_fd(),
                events: if writing { libc::POLLOUT } else { libc::POLLIN },
                revents: 0,
            };
            let millis = remaining
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32;
            let result = unsafe { libc::poll(&mut descriptor, 1, millis) };
            if result == 0 {
                return Err(deadline_error());
            }
            if result > 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}
fn deadline_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "host absolute I/O deadline")
}
struct Deadline<T> {
    inner: T,
    until: Instant,
}
impl<T: LocalIo> Deadline<T> {
    fn remaining(&self) -> io::Result<Duration> {
        self.until
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
            .ok_or_else(deadline_error)
    }
}
impl<T: LocalIo> Read for Deadline<T> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            self.remaining()?;
            match self.inner.read(bytes) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.inner.wait(false, self.remaining()?)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}
impl<T: LocalIo> Write for Deadline<T> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            self.remaining()?;
            match self.inner.write(bytes) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.inner.wait(true, self.remaining()?)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.remaining()?;
        self.inner.flush()
    }
}
pub(crate) struct Key(pub [u8; 32]);
impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
pub(crate) struct Channel<T> {
    io: Option<Deadline<T>>,
    noise: Option<TransportState>,
    budget: Duration,
}
fn error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("host channel: {error}"))
}
fn closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "host channel unusable after failure",
    )
}
impl<T: LocalIo> Channel<T> {
    pub fn authenticate(io: T, key: &Key, initiator: bool) -> io::Result<Self> {
        Self::authenticate_until(io, key, initiator, Instant::now() + TIMEOUT)
    }
    pub fn authenticate_until(
        io: T,
        key: &Key,
        initiator: bool,
        until: Instant,
    ) -> io::Result<Self> {
        io.prepare()?;
        let mut io = Deadline { inner: io, until };
        let builder = Builder::new(
            "Noise_NNpsk0_25519_ChaChaPoly_SHA256"
                .parse()
                .map_err(error)?,
        )
        .prologue(b"remuda-local-session-host-v1")
        .map_err(error)?
        .psk(0, &key.0)
        .map_err(error)?;
        let mut handshake = if initiator {
            builder.build_initiator()
        } else {
            builder.build_responder()
        }
        .map_err(error)?;
        if initiator {
            write_handshake(&mut io, &mut handshake)?;
            read_handshake(&mut io, &mut handshake)?;
        } else {
            read_handshake(&mut io, &mut handshake)?;
            write_handshake(&mut io, &mut handshake)?;
        }
        Ok(Self {
            io: Some(io),
            noise: Some(handshake.into_transport_mode().map_err(error)?),
            budget: TIMEOUT,
        })
    }
    pub fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        let result = self.send_inner(bytes);
        if result.is_err() {
            self.io.take();
            self.noise.take();
        }
        result
    }
    fn send_inner(&mut self, bytes: &[u8]) -> io::Result<()> {
        let io = self.io.as_mut().ok_or_else(closed)?;
        io.until = Instant::now() + self.budget;
        if bytes.is_empty() || bytes.len() > PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "host encrypted payload limit",
            ));
        }
        let mut encrypted = vec![0; bytes.len() + 16];
        let count = self
            .noise
            .as_mut()
            .ok_or_else(closed)?
            .write_message(bytes, &mut encrypted)
            .map_err(error)?;
        write_frame(io, &encrypted[..count])?;
        io.flush()
    }
    pub fn receive(&mut self) -> io::Result<Vec<u8>> {
        let result = self.receive_inner();
        if result.is_err() {
            self.io.take();
            self.noise.take();
        }
        result
    }
    fn receive_inner(&mut self) -> io::Result<Vec<u8>> {
        let io = self.io.as_mut().ok_or_else(closed)?;
        io.until = Instant::now() + self.budget;
        let encrypted = read_frame(io, CIPHER_FRAME)?;
        let mut bytes = vec![0; encrypted.len()];
        let count = self
            .noise
            .as_mut()
            .ok_or_else(closed)?
            .read_message(&encrypted, &mut bytes)
            .map_err(error)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "empty encrypted host message",
            ));
        }
        bytes.truncate(count);
        Ok(bytes)
    }
}
fn write_handshake(io: &mut (impl Read + Write), handshake: &mut HandshakeState) -> io::Result<()> {
    let mut bytes = [0; MAX_HELLO];
    let count = handshake.write_message(&[], &mut bytes).map_err(error)?;
    write_frame(io, &bytes[..count])?;
    io.flush()
}
fn read_handshake(io: &mut (impl Read + Write), handshake: &mut HandshakeState) -> io::Result<()> {
    let bytes = read_frame(io, MAX_HELLO)?;
    let mut payload = [0; MAX_HELLO];
    if handshake
        .read_message(&bytes, &mut payload)
        .map_err(error)?
        != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected host handshake payload",
        ));
    }
    Ok(())
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    fn pair() -> (UnixStream, UnixStream) {
        let (first, second) = UnixStream::pair().unwrap();
        for stream in [&first, &second] {
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
        }
        (first, second)
    }
    #[test]
    fn matching_capability_authenticates_both_sides_and_encrypts_frames() {
        let (server, client) = pair();
        let task = std::thread::spawn(move || {
            let mut server = Channel::authenticate(server, &Key([1; 32]), false).unwrap();
            assert_eq!(server.receive().unwrap(), b"inspect");
            server.send(b"same-session").unwrap();
        });
        let mut client = Channel::authenticate(client, &Key([1; 32]), true).unwrap();
        client.send(b"inspect").unwrap();
        assert_eq!(client.receive().unwrap(), b"same-session");
        assert!(client.send(&vec![0; PAYLOAD + 1]).is_err());
        task.join().unwrap();
    }
    #[test]
    fn ciphertext_failure_poisoning_prevents_nonce_or_frame_reuse() {
        let (server, client) = pair();
        let task = std::thread::spawn(move || {
            let mut server = Channel::authenticate(server, &Key([1; 32]), false).unwrap();
            assert!(server.receive().is_err());
            assert_eq!(
                server.receive().unwrap_err().kind(),
                io::ErrorKind::NotConnected
            );
            assert_eq!(
                server.send(b"unsafe reuse").unwrap_err().kind(),
                io::ErrorKind::NotConnected
            );
        });
        let mut client = Channel::authenticate(client, &Key([1; 32]), true).unwrap();
        write_frame(&mut client.io.as_mut().unwrap().inner, &[0; 17]).unwrap();
        task.join().unwrap();
    }
    #[test]
    fn one_byte_drip_cannot_extend_absolute_frame_budget() {
        let (server, client) = pair();
        let task = std::thread::spawn(move || {
            let mut server = Channel::authenticate(server, &Key([1; 32]), false).unwrap();
            let mut raw = server.io.take().unwrap().inner;
            for byte in [0, 0, 0, 32].into_iter().chain([0; 32]) {
                if raw.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        });
        let mut client = Channel::authenticate(client, &Key([1; 32]), true).unwrap();
        client.budget = Duration::from_millis(100);
        let started = Instant::now();
        let error = client.receive().unwrap_err();
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            client.receive().unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        task.join().unwrap();
    }
    #[test]
    fn wrong_capability_never_enters_transport() {
        let (server, client) = pair();
        let task = std::thread::spawn(move || {
            Channel::authenticate(server, &Key([1; 32]), false).is_err()
        });
        assert!(Channel::authenticate(client, &Key([2; 32]), true).is_err());
        assert!(task.join().unwrap());
    }
}
