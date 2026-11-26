//! Scoped document-open messages over the cluster's pinned Noise transport.

use super::browser_delivery::Delivery;
pub use super::browser_delivery::{BrowserDocumentOrigin, DocumentRequest, Outcome, DELIVERY_TTL};
use super::cluster_client::{ClientError, ClusterClient};
use crate::cluster::{
    encoding,
    vfs::{VfsErrorKind, VfsStore},
    vfs_sync,
};
use serde::{Deserialize, Serialize};
use std::io::{self, Read as _};
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

#[path = "browser_drive.rs"]
mod drive;
use drive::DriveGrant;
pub use drive::{
    list_local_drive, read_local_drive, DriveChunk, DriveEntry, DriveListing, DriveReadError,
};

const SOURCE_ENVELOPE: &[u8] = b"remuda-browser-source-v1\n";
const ENVELOPE: &[u8] = b"remuda-browser-v1\n";
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct SourceRequest<'a> {
    uri: &'a str,
    offset: u64,
    revision: Option<&'a str>,
}

#[derive(Serialize, Deserialize)]
pub enum Operation {
    Publish {
        request: DocumentRequest,
    },
    /// Only the configured, admitted ingress node may relay an open.
    RelayPublish {
        request: DocumentRequest,
    },
    Poll {
        instance: String,
    },
    Read {
        uri: String,
    },
    /// List the landing directory or a canonical URI within the grant.
    List {
        directory: Option<String>,
    },
    ListMore {
        directory: Option<String>,
        after: String,
    },
    ReadChunk {
        uri: String,
        offset: u64,
        revision: Option<String>,
    },
    ReadSource {
        uri: String,
        offset: u64,
        revision: Option<String>,
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
    Directory(DriveListing),
    Chunk(DriveChunk),
    DocumentChanged,
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
    document: Option<String>,
    directory: Option<String>,
    source_file: Option<drive::SourceFile>,
    /// Exact fingerprint of the node serving the user's active Browser.
    route_to: Option<String>,
    /// Static public key of the node allowed to relay this user's opens.
    relay_publisher: Option<String>,
    session_publish: Option<bool>,
}

pub(crate) struct Service {
    publisher: Vec<u8>,
    browser: Vec<u8>,
    drive: DriveGrant,
    route_to: Option<String>,
    relay_publisher: Option<Vec<u8>>,
    session_publish: bool,
    node: String,
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
    pub(super) fn configured(node: String) -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os("REMUDA_BROWSER_POLICY") else {
            return Ok(None);
        };
        let policy: Policy = serde_json::from_slice(&std::fs::read(path)?)?;
        let publisher = key(&policy.publisher)?;
        let browser = key(&policy.browser)?;
        let relay_publisher = policy.relay_publisher.as_deref().map(key).transpose()?;
        let drive = DriveGrant::with_source(
            policy.document.as_deref(),
            policy.directory.as_deref(),
            policy.source_file,
        )?;
        let valid_route = policy.route_to.as_deref().is_none_or(|fingerprint| {
            fingerprint.strip_prefix("SHA256:").is_some_and(|digest| {
                digest.len() == 43
                    && encoding::decode_base64(digest).is_ok_and(|bytes| bytes.len() == 32)
            })
        });
        if publisher == browser
            || !valid_route
            || relay_publisher
                .as_ref()
                .is_some_and(|relay| *relay == publisher || *relay == browser)
            || (policy.route_to.is_some() && relay_publisher.is_some())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Browser policy",
            ));
        }
        Ok(Some(Self {
            publisher,
            browser,
            drive,
            route_to: policy.route_to,
            relay_publisher,
            session_publish: policy.session_publish.unwrap_or(true),
            node,
            delivery: Mutex::new(Delivery::default()),
        }))
    }

    /// Invoked only by local daemon IPC after resolving and validating a live caller.
    pub(crate) fn publish_from_session(
        &self,
        id: String,
        uri: String,
        session: String,
        instance_id: String,
    ) -> Option<(BrowserDocumentOrigin, Outcome)> {
        if !self.session_publish || !valid_id(&id) || !self.authorizes_session_document(&uri) {
            return None;
        }
        let origin = BrowserDocumentOrigin {
            node: self.node.clone(),
            session,
            instance_id,
        };
        let request = DocumentRequest {
            id,
            uri,
            origin: Some(origin.clone()),
        };
        let outcome = if let Some(target) = &self.route_to {
            match relay(target, request) {
                Reply::Outcome(outcome) => outcome,
                _ => return None,
            }
        } else {
            if self.relay_publisher.is_some() {
                return None;
            }
            self.delivery.lock().ok()?.publish(request, Instant::now())
        };
        Some((origin, outcome))
    }

    fn authorizes_session_document(&self, uri: &str) -> bool {
        self.drive.authorizes_document(uri)
            || matches!(
                crate::terminal_link::Target::parse(uri),
                Some(crate::terminal_link::Target::File(_))
            )
    }

    pub(super) fn authenticates(&self, payload: &[u8], peer: &[u8], admitted: bool) -> bool {
        if payload.starts_with(SOURCE_ENVELOPE) {
            return admitted && self.drive.cluster_source_enabled();
        }
        payload.starts_with(ENVELOPE)
            && if admitted {
                self.relay_publisher.as_deref() == Some(peer)
            } else {
                peer == self.publisher || peer == self.browser
            }
    }

    pub(super) fn dispatch(&self, payload: &[u8], peer: &[u8], admitted: bool) -> Option<Vec<u8>> {
        if !self.authenticates(payload, peer, admitted) {
            return None;
        }
        let reply = if let Some(payload) = payload.strip_prefix(SOURCE_ENVELOPE) {
            match serde_json::from_slice::<SourceRequest<'_>>(payload) {
                Ok(request) => read_reply(
                    self.drive
                        .read_source(request.uri, request.offset, request.revision, true)
                        .map(Reply::Chunk),
                ),
                Err(_) => Reply::Invalid,
            }
        } else {
            let payload = payload.strip_prefix(ENVELOPE)?;
            match serde_json::from_slice(payload) {
                Ok(operation) => self.handle(operation, peer, admitted),
                Err(_) => Reply::Invalid,
            }
        };
        let bytes = serde_json::to_vec(&reply).ok()?;
        if bytes.len() > super::frame::MAX_RESPONSE_PAYLOAD {
            return serde_json::to_vec(&Reply::ReadFailed).ok();
        }
        Some(bytes)
    }

    fn handle(&self, operation: Operation, peer: &[u8], admitted: bool) -> Reply {
        if let Operation::Publish { request } = &operation {
            if let Some(target) = &self.route_to {
                return if peer == self.publisher
                    && request.origin.is_none()
                    && self.drive.authorizes_document(&request.uri)
                    && valid_id(&request.id)
                {
                    relay(target, request.clone())
                } else {
                    Reply::Denied
                };
            }
        }
        // The ingress publishes only. The egress owns the lease and receipt.
        if self.route_to.is_some() {
            return Reply::Denied;
        }
        if let Operation::Read { uri } = &operation {
            return if peer == self.browser
                && self.drive.authorizes_document(uri)
                && !self.drive.is_source(uri)
            {
                read_document(uri).map_or(Reply::ReadFailed, Reply::Document)
            } else {
                Reply::Denied
            };
        }
        match &operation {
            Operation::ReadSource {
                uri,
                offset,
                revision,
            } => {
                return self.browser_operation(peer, || {
                    self.drive
                        .read_source(uri, *offset, revision.as_deref(), false)
                        .map(Reply::Chunk)
                });
            }
            Operation::List { directory } => {
                return self.browser_operation(peer, || {
                    self.drive
                        .list(directory.as_deref(), None)
                        .map(Reply::Directory)
                });
            }
            Operation::ListMore { directory, after } => {
                return self.browser_operation(peer, || {
                    self.drive
                        .list(directory.as_deref(), Some(after))
                        .map(Reply::Directory)
                });
            }
            Operation::ReadChunk {
                uri,
                offset,
                revision,
            } => {
                return self.browser_operation(peer, || {
                    self.drive
                        .read_chunk(uri, *offset, revision.as_deref())
                        .map(Reply::Chunk)
                });
            }
            _ => {}
        }
        self.handle_delivery(operation, peer, admitted)
    }

    fn handle_delivery(&self, operation: Operation, peer: &[u8], admitted: bool) -> Reply {
        let Ok(mut delivery) = self.delivery.lock() else {
            return Reply::Invalid;
        };
        let now = Instant::now();
        match operation {
            Operation::Publish { request }
                if peer == self.publisher
                    && self.relay_publisher.is_none()
                    && request.origin.is_none()
                    && self.drive.authorizes_document(&request.uri)
                    && valid_id(&request.id) =>
            {
                Reply::Outcome(delivery.publish(request, now))
            }
            Operation::RelayPublish { request }
                if admitted
                    && self.relay_publisher.as_deref() == Some(peer)
                    && valid_id(&request.id)
                    && match &request.origin {
                        Some(origin) => {
                            self.session_publish
                                && origin.node == encoding::fingerprint(peer)
                                && self.authorizes_session_document(&request.uri)
                        }
                        None => self.drive.authorizes_document(&request.uri),
                    } =>
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
    fn browser_operation(
        &self,
        peer: &[u8],
        work: impl FnOnce() -> Result<Reply, DriveReadError>,
    ) -> Reply {
        if peer != self.browser {
            return Reply::Denied;
        }
        read_reply(work())
    }
}

fn relay(target: &str, request: DocumentRequest) -> Reply {
    use crate::cluster_api::{resolve_call, ResolvedCall};

    // Re-resolve membership and the pinned endpoint for every publish or
    // result poll. Revocation also prevents receiving a previous receipt.
    let Ok(ResolvedCall::Remote {
        fingerprint,
        address,
        pinned_static_key,
    }) = resolve_call(target, None)
    else {
        return Reply::Denied;
    };
    if fingerprint != target {
        return Reply::Denied;
    }
    let Ok(private) = crate::cluster::identity::load_static_private_key() else {
        return Reply::Denied;
    };
    let client = Client {
        endpoint: address,
        server: pinned_static_key,
        private,
    };
    match client.send_with_timeout(&Operation::RelayPublish { request }, Duration::from_secs(2)) {
        Ok(reply @ (Reply::Outcome(_) | Reply::Denied)) => reply,
        _ => Reply::Outcome(Outcome::Unknown),
    }
}

fn read_reply(result: Result<Reply, DriveReadError>) -> Reply {
    match result {
        Ok(reply) => reply,
        Err(DriveReadError::Denied) => Reply::Denied,
        Err(DriveReadError::Changed) => Reply::DocumentChanged,
        Err(_) => Reply::ReadFailed,
    }
}

/// Read only the exact source file explicitly shared with the admitted cluster.
pub fn read_cluster_source(fingerprint: &str, uri: &str) -> Result<Vec<u8>, DriveReadError> {
    use crate::cluster_api::{resolve_call, ResolvedCall};
    let ResolvedCall::Remote {
        fingerprint: peer,
        address,
        pinned_static_key,
    } = resolve_call(fingerprint, None).map_err(|_| DriveReadError::Denied)?
    else {
        return Err(DriveReadError::Denied);
    };
    if peer != fingerprint {
        return Err(DriveReadError::Denied);
    }
    let private =
        crate::cluster::identity::load_static_private_key().map_err(|_| DriveReadError::Denied)?;
    let client = ClusterClient::system().with_request_timeout(Duration::from_secs(3));
    drive::transfer_cluster_source(|offset, revision| {
        let mut payload = SOURCE_ENVELOPE.to_vec();
        serde_json::to_writer(
            &mut payload,
            &SourceRequest {
                uri,
                offset,
                revision,
            },
        )
        .map_err(|_| ClientError::BadResponse)?;
        let reply =
            client.request_payload(address, &pinned_static_key, &private, &payload, false)?;
        serde_json::from_slice(&reply).map_err(|_| ClientError::BadResponse)
    })
}

/// Source-side authorization is based on the daemon's current session instance.
pub(crate) fn read_session_file_chunk(
    session: &str,
    root: &std::path::Path,
    uri: &str,
    offset: u64,
    revision: Option<&str>,
) -> remuda_core::protocol::Response {
    use remuda_core::protocol::Response;
    let denied = || Response::error("session file access denied");
    if !session_file_enabled(session) {
        return denied();
    }
    let Some(crate::terminal_link::Target::File(url)) = crate::terminal_link::Target::parse(uri)
    else {
        return denied();
    };
    let Ok(path) = url.to_file_path() else {
        return denied();
    };
    let Ok(mut file) = crate::workdir_read::open(root, &path) else {
        return denied();
    };
    match drive::read_regular_chunk(&mut file, offset, revision) {
        Ok(chunk) => match serde_json::to_string(&chunk) {
            Ok(value) => Response::Value(value),
            Err(_) => Response::error("session file response unavailable"),
        },
        Err(DriveReadError::Changed) => Response::error("session file changed"),
        Err(_) => Response::error("session file unavailable"),
    }
}

fn session_file_enabled(session: &str) -> bool {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Policy {
        #[serde(default = "default_enabled")]
        enabled: bool,
        #[serde(default)]
        denied_sessions: Vec<String>,
    }
    fn default_enabled() -> bool {
        true
    }
    let Ok(config) =
        crate::storage::resolve_dir_for("config", cfg!(windows), &|name| std::env::var_os(name))
    else {
        return false;
    };
    let path = config.join("session-file-policy.json");
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) => return error.kind() == io::ErrorKind::NotFound,
    };
    const MAX_POLICY_BYTES: usize = 64 * 1024;
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let mut data = Vec::new();
    file.take((MAX_POLICY_BYTES + 1) as u64)
        .read_to_end(&mut data)
        .is_ok()
        && data.len() <= MAX_POLICY_BYTES
        && serde_json::from_slice::<Policy>(&data).is_ok_and(|policy| {
            policy.enabled && !policy.denied_sessions.iter().any(|name| name == session)
        })
}

pub fn read_cluster_session_file(
    fingerprint: &str,
    session: &str,
    instance: &str,
    uri: &str,
) -> Result<Vec<u8>, DriveReadError> {
    use crate::cluster_api::{resolve_call, ResolvedCall};
    let ResolvedCall::Remote {
        fingerprint: peer,
        address,
        pinned_static_key,
    } = resolve_call(fingerprint, None).map_err(|_| DriveReadError::Denied)?
    else {
        return Err(DriveReadError::Denied);
    };
    if peer != fingerprint {
        return Err(DriveReadError::Denied);
    }
    let private =
        crate::cluster::identity::load_static_private_key().map_err(|_| DriveReadError::Denied)?;
    let client = ClusterClient::system().with_request_timeout(Duration::from_secs(3));
    read_session_file_using(session, instance, uri, |request| {
        client.request(address, &pinned_static_key, &private, &request)
    })
}

/// Read a bounded, consistent source file over an already authenticated connection.
pub fn read_session_file_using<E>(
    session: &str,
    instance: &str,
    uri: &str,
    mut request_response: impl FnMut(
        remuda_core::protocol::Request,
    ) -> Result<remuda_core::protocol::Response, E>,
) -> Result<Vec<u8>, DriveReadError> {
    use remuda_core::protocol::{Request, Response};
    drive::transfer_cluster_source(|offset, revision| {
        let request = Request::ReadSessionFile {
            name: session.to_owned(),
            instance_id: instance.to_owned(),
            uri: uri.to_owned(),
            offset,
            revision: revision.map(str::to_owned),
        };
        let response = request_response(request).map_err(|_| ClientError::BadResponse)?;
        match response {
            Response::Value(data) => serde_json::from_str(&data)
                .map(Reply::Chunk)
                .map_err(|_| ClientError::BadResponse),
            Response::Error(reason) if reason == "session file changed" => {
                Ok(Reply::DocumentChanged)
            }
            Response::Error(reason) if reason == "session file access denied" => Ok(Reply::Denied),
            _ => Ok(Reply::ReadFailed),
        }
    })
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
        Self::from_file(std::path::Path::new(&path)).map(Some)
    }

    /// Open one pinned Browser credential file with the normal private-file check.
    pub fn from_file(path: &std::path::Path) -> io::Result<Self> {
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
        Ok(Self {
            endpoint: config.endpoint,
            server: key(&config.server)?,
            private: Zeroizing::new(key(&private)?),
        })
    }

    /// Read the configured file through the designated Browser credential.
    pub fn read_source(&self, source: &str, uri: &str) -> Result<Vec<u8>, DriveReadError> {
        if encoding::fingerprint(&self.server) != source {
            return Err(DriveReadError::Denied);
        }
        drive::transfer_cluster_source(|offset, revision| {
            self.send(&Operation::ReadSource {
                uri: uri.to_owned(),
                offset,
                revision: revision.map(str::to_owned),
            })
        })
    }

    pub fn send(&self, operation: &Operation) -> Result<Reply, ClientError> {
        self.send_with_timeout(operation, Duration::from_secs(3))
    }

    fn send_with_timeout(
        &self,
        operation: &Operation,
        timeout: Duration,
    ) -> Result<Reply, ClientError> {
        let mut payload = ENVELOPE.to_vec();
        serde_json::to_writer(&mut payload, operation).map_err(|_| ClientError::BadResponse)?;
        let response = ClusterClient::system()
            .with_request_timeout(timeout)
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

#[cfg(test)]
mod drive_role_tests {
    use super::*;

    #[test]
    fn drive_operations_do_not_grant_access_to_the_publisher_or_the_ingress() {
        let publisher = vec![1; 32];
        let browser = vec![2; 32];
        let relay_publisher = vec![3; 32];
        let mut service = Service {
            publisher: publisher.clone(),
            browser: browser.clone(),
            drive: DriveGrant::new(None, Some("remuda:///fixture")).unwrap(),
            route_to: None,
            relay_publisher: Some(relay_publisher.clone()),
            session_publish: true,
            node: encoding::fingerprint(&[4; 32]),
            delivery: Mutex::new(Delivery::default()),
        };
        for (peer, admitted) in [(&publisher, false), (&relay_publisher, true)] {
            for operation in [
                Operation::List { directory: None },
                Operation::ReadChunk {
                    uri: "remuda:///fixture/note.md".into(),
                    offset: 0,
                    revision: None,
                },
            ] {
                let mut payload = ENVELOPE.to_vec();
                serde_json::to_writer(&mut payload, &operation).unwrap();
                let reply = service.dispatch(&payload, peer, admitted).unwrap();
                assert!(matches!(serde_json::from_slice(&reply), Ok(Reply::Denied)));
            }
        }
        for directory in ["remuda:///fixture-sibling", "remuda://other/fixture"] {
            assert!(matches!(
                service.handle(
                    Operation::List {
                        directory: Some(directory.into())
                    },
                    &browser,
                    false,
                ),
                Reply::Denied
            ));
        }
        service.route_to = Some("fixture-ingress".into());
        assert!(matches!(
            service.handle(Operation::List { directory: None }, &browser, false),
            Reply::Denied
        ));
    }
}
