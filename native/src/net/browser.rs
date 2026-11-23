//! Scoped document-open messages over the cluster's pinned Noise transport.

use super::browser_delivery::Delivery;
pub use super::browser_delivery::{DocumentRequest, Outcome};
use super::cluster_client::{ClientError, ClusterClient};
use crate::cluster::{
    encoding,
    vfs::{VfsErrorKind, VfsStore},
    vfs_sync,
};
use remuda_core::vfs::VfsUri;
use serde::{Deserialize, Serialize};
use std::io;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Instant;
use zeroize::Zeroizing;

const ENVELOPE: &[u8] = b"remuda-browser-v1\n";
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
pub enum Operation {
    Publish {
        request: DocumentRequest,
    },
    Poll {
        instance: String,
    },
    Read {
        uri: String,
    },
    Finish {
        instance: String,
        request: DocumentRequest,
        outcome: Outcome,
    },
}

#[derive(Serialize, Deserialize)]
pub enum Reply {
    Outcome(Outcome),
    Event(Option<DocumentRequest>),
    Document(String),
    Accepted,
    Denied,
    Invalid,
    ReadFailed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    publisher: String,
    browser: String,
    document: String,
}

pub(super) struct Service {
    publisher: Vec<u8>,
    browser: Vec<u8>,
    document: String,
    delivery: Mutex<Delivery>,
}

fn key(value: &str) -> io::Result<Vec<u8>> {
    encoding::decode_base64(value)
        .ok()
        .filter(|bytes| bytes.len() == 32)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid Browser key"))
}

fn valid_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

impl Service {
    pub(super) fn configured() -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os("REMUDA_BROWSER_POLICY") else {
            return Ok(None);
        };
        let policy: Policy = serde_json::from_slice(&std::fs::read(path)?)?;
        let publisher = key(&policy.publisher)?;
        let browser = key(&policy.browser)?;
        let valid_uri = VfsUri::parse(&policy.document)
            .is_ok_and(|uri| uri.cluster.is_none() && !uri.path.is_empty());
        if publisher == browser || !valid_uri {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Browser policy",
            ));
        }
        Ok(Some(Self {
            publisher,
            browser,
            document: policy.document,
            delivery: Mutex::new(Delivery::default()),
        }))
    }

    pub(super) fn authenticates(&self, payload: &[u8], peer: &[u8]) -> bool {
        payload.starts_with(ENVELOPE) && (peer == self.publisher || peer == self.browser)
    }

    pub(super) fn dispatch(&self, payload: &[u8], peer: &[u8]) -> Option<Vec<u8>> {
        let payload = payload.strip_prefix(ENVELOPE)?;
        let reply = match serde_json::from_slice(payload) {
            Ok(operation) => self.handle(operation, peer),
            Err(_) => Reply::Invalid,
        };
        serde_json::to_vec(&reply).ok()
    }

    fn handle(&self, operation: Operation, peer: &[u8]) -> Reply {
        if let Operation::Read { uri } = &operation {
            return if peer == self.browser && *uri == self.document {
                read_document(uri).map_or(Reply::ReadFailed, Reply::Document)
            } else {
                Reply::Denied
            };
        }
        let Ok(mut delivery) = self.delivery.lock() else {
            return Reply::Invalid;
        };
        let now = Instant::now();
        match operation {
            Operation::Publish { request }
                if peer == self.publisher
                    && request.uri == self.document
                    && valid_id(&request.id) =>
            {
                Reply::Outcome(delivery.publish(request, now))
            }
            Operation::Poll { instance } if peer == self.browser && valid_id(&instance) => delivery
                .poll(&instance, now)
                .map_or_else(Reply::Outcome, Reply::Event),
            Operation::Finish {
                instance,
                request,
                outcome,
            } if peer == self.browser => {
                if delivery.finish(&instance, &request, outcome, now) {
                    Reply::Accepted
                } else {
                    Reply::Denied
                }
            }
            _ => Reply::Denied,
        }
    }
}

fn read_document(uri: &str) -> Option<String> {
    let store = VfsStore::open_default().ok()?;
    let uri = store.parse_uri(uri).ok()?;
    let index = store.load_index().ok()?;
    let entry = index.live(&uri.path)?;
    if entry.size > MAX_DOCUMENT_BYTES {
        return None;
    }
    let bytes = match store.read_blob(&entry.sha256) {
        Ok(bytes) => bytes,
        Err(error) if matches!(error.kind, VfsErrorKind::NotLocal | VfsErrorKind::Corrupt) => {
            vfs_sync::fetch_blob(&store, entry).ok()?
        }
        Err(_) => return None,
    };
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// A Browser or publisher key is independent of the node registry.
pub struct Client {
    endpoint: SocketAddr,
    server: Vec<u8>,
    private: Zeroizing<Vec<u8>>,
}

impl Client {
    pub fn from_env() -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os("REMUDA_BROWSER_CLIENT") else {
            return Ok(None);
        };
        let path = std::path::Path::new(&path);
        let file = std::fs::File::open(path)?;
        crate::cluster::storage::check_private_file(&file, "Browser credential", path)?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Config {
            endpoint: SocketAddr,
            server: String,
            private: String,
        }
        let config: Config = serde_json::from_reader(file)?;
        let private = Zeroizing::new(config.private);
        Ok(Some(Self {
            endpoint: config.endpoint,
            server: key(&config.server)?,
            private: Zeroizing::new(key(&private)?),
        }))
    }

    pub fn send(&self, operation: &Operation) -> Result<Reply, ClientError> {
        let mut payload = ENVELOPE.to_vec();
        serde_json::to_writer(&mut payload, operation).map_err(|_| ClientError::BadResponse)?;
        let response = ClusterClient::system()
            .with_request_timeout(std::time::Duration::from_secs(3))
            .request_payload(self.endpoint, &self.server, &self.private, &payload, false)?;
        serde_json::from_slice(&response).map_err(|_| ClientError::BadResponse)
    }
}

/// Arguments shared by Browser's native and terminal SSH clients.
pub fn ssh_arguments(destination: &str) -> [&str; 8] {
    [
        "-a",
        "-x",
        "-o",
        "ClearAllForwardings=yes",
        "-t",
        "--",
        destination,
        "remuda",
    ]
}

pub fn valid_ssh_destination(destination: &str) -> bool {
    fn name(value: &str) -> bool {
        value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    }
    let host = match destination.split_once('@') {
        Some((user, host)) if name(user) => host,
        Some(_) => return false,
        None => destination,
    };
    name(host) || host.parse::<std::net::Ipv6Addr>().is_ok()
}
