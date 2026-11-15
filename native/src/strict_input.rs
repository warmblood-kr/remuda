//! Instance-bound, sequenced input shared by interactive frontends.
//!
//! Call from a worker: a transport may block. This type does not provide a
//! queue or cancel an in-flight write. Any non-acknowledgement permanently
//! stops this controller because delivery may have happened.
use remuda_core::protocol::{Request, Response};
use std::{io, path::PathBuf, time::Duration};

pub const MAX_BATCH: usize = crate::remote_front::MAX_REMOTE_INPUT_BATCH_BYTES;

/// A transport must make exactly one attempt and must not log input bytes.
pub trait InputTransport {
    fn send(&mut self, request: &Request) -> io::Result<Response>;
}

pub struct LocalInput(pub PathBuf);
impl InputTransport for LocalInput {
    fn send(&mut self, request: &Request) -> io::Result<Response> {
        crate::client::request_with_timeout(&self.0, request, Duration::from_secs(8))
    }
}

/// Resolve the exact admitted fingerprint on every dispatch and use its pinned key.
pub struct PeerInput {
    fingerprint: String,
}
impl PeerInput {
    pub fn new(fingerprint: String) -> io::Result<Self> {
        if fingerprint.is_empty() {
            return Err(io::Error::other("empty peer fingerprint"));
        }
        Ok(Self { fingerprint })
    }
}
impl InputTransport for PeerInput {
    fn send(&mut self, request: &Request) -> io::Result<Response> {
        use crate::cluster_api::{resolve_call, ResolvedCall};
        let ResolvedCall::Remote {
            fingerprint,
            address,
            pinned_static_key,
        } = resolve_call(&self.fingerprint, None)?
        else {
            return Err(io::Error::other("peer unavailable"));
        };
        if fingerprint != self.fingerprint {
            return Err(io::Error::other("peer identity changed"));
        }
        let private = crate::cluster::identity::load_static_private_key()?;
        crate::net::cluster_client::ClusterClient::system()
            .request(address, &pinned_static_key, &private, request)
            .map_err(io::Error::other)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum InputResult {
    Applied,
    InvalidBatch,
    Stopped,
    NotAcknowledged,
}

/// One selected session, one fresh idempotency identity, one ordered sender.
pub struct StrictInput<T> {
    transport: T,
    name: String,
    instance: String,
    client_id: String,
    next: u64,
    stopped: bool,
}
impl<T: InputTransport> StrictInput<T> {
    pub fn new(transport: T, name: String, instance: String) -> io::Result<Self> {
        if name.is_empty() || instance.is_empty() {
            return Err(io::Error::other("missing session identity"));
        }
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).map_err(|e| io::Error::other(e.to_string()))?;
        let client_id = id.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Self {
            transport,
            name,
            instance,
            client_id,
            next: 1,
            stopped: false,
        })
    }
    /// Never retries: NotAcknowledged includes outcomes where delivery is unknown.
    pub fn send(&mut self, bytes: &[u8]) -> InputResult {
        if self.stopped {
            return InputResult::Stopped;
        }
        if bytes.is_empty() || bytes.len() > MAX_BATCH {
            return InputResult::InvalidBatch;
        }
        let Some(next) = self.next.checked_add(1) else {
            self.stopped = true;
            return InputResult::Stopped;
        };
        let request = Request::Input {
            name: self.name.clone(),
            instance_id: self.instance.clone(),
            client_id: self.client_id.clone(),
            seq: self.next,
            bytes: bytes.to_vec(),
        };
        match self.transport.send(&request) {
            Ok(Response::Ack { duplicate: false }) => {
                self.next = next;
                InputResult::Applied
            }
            _ => {
                self.stopped = true;
                InputResult::NotAcknowledged
            }
        }
    }
    pub fn stop(&mut self) {
        self.stopped = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };
    struct Fake {
        requests: Arc<Mutex<Vec<Request>>>,
        replies: VecDeque<io::Result<Response>>,
    }
    impl InputTransport for Fake {
        fn send(&mut self, request: &Request) -> io::Result<Response> {
            self.requests.lock().unwrap().push(request.clone());
            self.replies.pop_front().unwrap()
        }
    }
    #[test]
    fn bounded_batches_sequence_and_uncertain_stop_without_retry() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            requests: requests.clone(),
            replies: [
                Ok(Response::Ack { duplicate: false }),
                Ok(Response::Uncertain),
            ]
            .into(),
        };
        assert!(StrictInput::new(
            Fake {
                requests: requests.clone(),
                replies: VecDeque::new()
            },
            "".into(),
            "id".into()
        )
        .is_err());
        let mut sender = StrictInput::new(fake, "session".into(), "instance".into()).unwrap();
        assert_eq!(sender.send(&[]), InputResult::InvalidBatch);
        assert_eq!(
            sender.send(&vec![1; MAX_BATCH + 1]),
            InputResult::InvalidBatch
        );
        assert_eq!(sender.send(b"a"), InputResult::Applied);
        assert_eq!(sender.send(b"b"), InputResult::NotAcknowledged);
        assert_eq!(sender.send(b"b"), InputResult::Stopped);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let (
            Request::Input {
                name,
                instance_id,
                client_id,
                seq: 1,
                ..
            },
            Request::Input {
                client_id: other,
                seq: 2,
                ..
            },
        ) = (&requests[0], &requests[1])
        else {
            panic!("unexpected requests")
        };
        assert_eq!(
            (name.as_str(), instance_id.as_str()),
            ("session", "instance")
        );
        assert_eq!(client_id, other);
    }
    #[test]
    fn unexpected_duplicate_ack_stops_without_retrying_or_advancing() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            requests: requests.clone(),
            replies: [Ok(Response::Ack { duplicate: true })].into(),
        };
        let mut sender = StrictInput::new(fake, "s".into(), "i".into()).unwrap();
        assert_eq!(sender.send(b"x"), InputResult::NotAcknowledged);
        assert_eq!(sender.send(b"y"), InputResult::Stopped);
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
    #[test]
    fn refusal_and_uncertain_responses_never_allow_another_attempt() {
        for response in [
            Response::WrongInstance,
            Response::Busy,
            Response::RateLimited,
            Response::WriteTimeout,
            Response::Uncertain,
        ] {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let fake = Fake {
                requests: requests.clone(),
                replies: [Ok(response)].into(),
            };
            let mut sender = StrictInput::new(fake, "s".into(), "i".into()).unwrap();
            assert_eq!(sender.send(b"x"), InputResult::NotAcknowledged);
            assert_eq!(sender.send(b"y"), InputResult::Stopped);
            assert_eq!(requests.lock().unwrap().len(), 1);
        }
    }
    #[test]
    fn transport_error_stops_and_explicit_stop_never_sends() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            requests: requests.clone(),
            replies: [Err(io::Error::other("timeout"))].into(),
        };
        let mut sender = StrictInput::new(fake, "s".into(), "i".into()).unwrap();
        assert_eq!(sender.send(b"x"), InputResult::NotAcknowledged);
        assert_eq!(sender.send(b"y"), InputResult::Stopped);
        assert_eq!(requests.lock().unwrap().len(), 1);
        let fake = Fake {
            requests: requests.clone(),
            replies: VecDeque::new(),
        };
        let mut sender = StrictInput::new(fake, "s".into(), "i".into()).unwrap();
        sender.stop();
        assert_eq!(sender.send(b"x"), InputResult::Stopped);
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
}
