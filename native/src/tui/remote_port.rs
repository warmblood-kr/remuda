//! A cluster peer as a session-screen port: each request goes to the peer's
//! remote front over the pinned Noise channel, whose allowlist decides.
use super::port::{KeyStream, SessionPort};
use crate::cluster::ResolvedTarget;
use crate::net::cluster_client::{ClientTimeouts, ClusterClient};
use remuda_core::protocol::{Request, Response};
use std::io;
use std::sync::Arc;
use std::time::Duration;

/// Bounds one round trip, since the screen still waits for each refresh.
const REMOTE_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

pub struct RemotePort {
    client: ClusterClient,
    local_static_private: zeroize::Zeroizing<Vec<u8>>,
    target: ResolvedTarget,
    node: String,
}

impl RemotePort {
    /// Resolve NODE (label or fingerprint) to its admitted pin and address.
    pub fn system(node: &str) -> io::Result<Self> {
        let target = crate::cluster::resolve_target(node, None)?;
        let local_static_private = crate::cluster::identity::load_static_private_key()?;
        let client = ClusterClient::with_timeouts(
            Arc::new(crate::SystemWallClock::new()),
            ClientTimeouts {
                connect: REMOTE_REQUEST_TIMEOUT,
                read: REMOTE_REQUEST_TIMEOUT,
                total: REMOTE_REQUEST_TIMEOUT,
            },
        );
        Ok(Self {
            client,
            local_static_private,
            target,
            node: node.into(),
        })
    }
}

/// Live keys to a peer are not wired yet; taking them is refused.
pub struct NoRemoteKeys;

impl KeyStream for NoRemoteKeys {
    fn keys(&self, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::other(
            "live keys to a remote session are not available yet",
        ))
    }
}

impl SessionPort for RemotePort {
    type Keys = NoRemoteKeys;

    fn request(&self, request: &Request) -> io::Result<Response> {
        self.client
            .request(
                self.target.address,
                &self.target.pinned_static_key,
                &self.local_static_private,
                request,
            )
            .map_err(io::Error::other)
    }

    fn hold(&self, _name: &str) -> io::Result<NoRemoteKeys> {
        Err(io::Error::other(
            "live keys to a remote session are not available yet",
        ))
    }

    fn probe(&self) -> io::Result<()> {
        self.request(&Request::List).map(|_| ())
    }

    fn probe_verdict(&self, _error: &io::Error) -> (bool, bool) {
        // An unreachable peer may come back; never declare it gone at once.
        (false, false)
    }

    fn label(&self) -> String {
        format!("node {}", self.node)
    }

    fn has_lua(&self) -> bool {
        false
    }

    fn restartable(&self) -> bool {
        false
    }
}
