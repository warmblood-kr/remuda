//! A cluster peer as a session-screen port: each request goes to the peer's
//! remote front over the pinned Noise channel, whose allowlist decides.
//! Peer I/O runs on background threads so a slow peer never stalls the screen.
use super::port::{KeyStream, SessionPort};
use crate::cluster::ResolvedTarget;
use crate::net::cluster_client::{ClientTimeouts, ClusterClient};
use remuda_core::protocol::{Request, Response};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Bounds one round trip to the peer.
const REMOTE_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// A submitted line may wait for its Return to land, as on the cluster tree.
const REMOTE_LINE_TIMEOUT: Duration = Duration::from_secs(8);
/// Gap between background reads of the list and the shown screen.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How long the screen waits for a first answer before showing "waiting".
const FIRST_ANSWER_WAIT: Duration = Duration::from_millis(300);

/// Sends one request to the peer. A trait so tests need no network.
pub trait PeerLink: Send + Sync + 'static {
    fn send(&self, request: &Request, timeout: Duration) -> io::Result<Response>;
}

struct NoiseLink {
    local_static_private: zeroize::Zeroizing<Vec<u8>>,
    target: ResolvedTarget,
}

impl PeerLink for NoiseLink {
    fn send(&self, request: &Request, timeout: Duration) -> io::Result<Response> {
        ClusterClient::with_timeouts(
            Arc::new(crate::SystemWallClock::new()),
            ClientTimeouts {
                connect: REMOTE_REQUEST_TIMEOUT,
                read: timeout,
                total: timeout,
            },
        )
        .request(
            self.target.address,
            &self.target.pinned_static_key,
            &self.local_static_private,
            request,
        )
        .map_err(io::Error::other)
    }
}

#[derive(Default)]
struct Cache {
    list: Option<Result<Response, String>>,
    /// The screen the user is looking at: (name, scrollback) and its answer.
    wanted: Option<(String, usize)>,
    screen: Option<((String, usize), Result<Response, String>)>,
    last_ok: Option<Instant>,
}

struct Shared {
    cache: Mutex<Cache>,
    changed: Condvar,
    stop: AtomicBool,
}

pub struct RemotePort {
    link: Arc<dyn PeerLink>,
    shared: Arc<Shared>,
    lines: mpsc::Sender<Request>,
    replies: Mutex<mpsc::Receiver<io::Result<Response>>>,
    node: String,
}

impl RemotePort {
    /// Resolve NODE (label or fingerprint) to its admitted pin and address.
    pub fn system(node: &str) -> io::Result<Self> {
        let target = crate::cluster::resolve_target(node, None)?;
        let local_static_private = crate::cluster::identity::load_static_private_key()?;
        Ok(Self::with_link(
            node,
            Arc::new(NoiseLink {
                local_static_private,
                target,
            }),
        ))
    }

    pub fn with_link(node: &str, link: Arc<dyn PeerLink>) -> Self {
        let shared = Arc::new(Shared {
            cache: Mutex::new(Cache::default()),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        spawn_reader(link.clone(), shared.clone());
        let (lines, replies) = spawn_writer(link.clone());
        Self {
            link,
            shared,
            lines,
            replies: Mutex::new(replies),
            node: node.into(),
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.shared
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The cached answer, waiting briefly for a first one.
    fn cached(
        &self,
        pick: impl Fn(&Cache) -> Option<Result<Response, String>>,
    ) -> io::Result<Response> {
        let deadline = Instant::now() + FIRST_ANSWER_WAIT;
        let mut cache = self.cache();
        loop {
            if let Some(answer) = pick(&cache) {
                return answer.map_err(io::Error::other);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::other(format!("waiting for node {}", self.node)));
            }
            cache = self
                .shared
                .changed
                .wait_timeout(cache, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }
}

impl Drop for RemotePort {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
    }
}

fn spawn_reader(link: Arc<dyn PeerLink>, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        while !shared.stop.load(Ordering::SeqCst) {
            let list = link.send(&Request::List, REMOTE_REQUEST_TIMEOUT);
            let wanted = shared
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .wanted
                .clone();
            let screen = wanted.map(|(name, scrollback)| {
                let request = Request::CaptureStyled {
                    name: name.clone(),
                    scrollback,
                };
                (
                    (name, scrollback),
                    link.send(&request, REMOTE_REQUEST_TIMEOUT),
                )
            });
            let mut cache = shared
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if list.is_ok() {
                cache.last_ok = Some(Instant::now());
            }
            cache.list = Some(list.map_err(|error| error.to_string()));
            if let Some((key, answer)) = screen {
                cache.screen = Some((key, answer.map_err(|error| error.to_string())));
            }
            shared.changed.notify_all();
            // Sleep until the interval passes or the shown screen changes.
            let before = cache.wanted.clone();
            let _ = shared
                .changed
                .wait_timeout_while(cache, POLL_INTERVAL, |cache| {
                    cache.wanted == before && !shared.stop.load(Ordering::SeqCst)
                });
        }
    });
}

/// One line at a time, in order; the screen's input queue owns retries.
fn spawn_writer(
    link: Arc<dyn PeerLink>,
) -> (mpsc::Sender<Request>, mpsc::Receiver<io::Result<Response>>) {
    let (line_tx, line_rx) = mpsc::channel::<Request>();
    let (reply_tx, reply_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for request in line_rx {
            if reply_tx
                .send(link.send(&request, REMOTE_LINE_TIMEOUT))
                .is_err()
            {
                return;
            }
        }
    });
    (line_tx, reply_rx)
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
        match request {
            Request::List => self.cached(|cache| cache.list.clone()),
            Request::CaptureStyled { name, scrollback } => {
                let key = (name.clone(), *scrollback);
                {
                    let mut cache = self.cache();
                    if cache.wanted.as_ref() != Some(&key) {
                        cache.wanted = Some(key.clone());
                        self.shared.changed.notify_all();
                    }
                }
                // The exact view if read, else this session's last screen.
                self.cached(|cache| match &cache.screen {
                    Some((read, answer)) if *read == key => Some(answer.clone()),
                    Some((read, Ok(answer))) if read.0 == key.0 => Some(Ok(answer.clone())),
                    _ => None,
                })
            }
            // One-shot user actions (close, new, resize) wait for their answer.
            other => self.link.send(other, REMOTE_REQUEST_TIMEOUT),
        }
    }

    fn hold(&self, _name: &str) -> io::Result<NoRemoteKeys> {
        Err(io::Error::other(
            "live keys to a remote session are not available yet; press m to send a line",
        ))
    }

    fn probe(&self) -> io::Result<()> {
        let recent = self
            .cache()
            .last_ok
            .is_some_and(|at| at.elapsed() < Duration::from_secs(10));
        if recent {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "node {} is not answering",
                self.node
            )))
        }
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

    fn remote_node(&self) -> Option<String> {
        Some(self.node.clone())
    }

    fn staleness(&self) -> Option<Duration> {
        Some(
            self.cache()
                .last_ok
                .map_or(Duration::MAX, |at| at.elapsed()),
        )
    }

    fn submit(&self, request: Request) -> io::Result<()> {
        self.lines
            .send(request)
            .map_err(|_| io::Error::other("remote line sender stopped"))
    }

    fn take_replies(&self) -> Vec<io::Result<Response>> {
        self.replies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .try_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer that answers List at once and stalls every write until released.
    #[derive(Default)]
    struct SlowPeer {
        release: Mutex<bool>,
        released: Condvar,
        sent: Mutex<Vec<Request>>,
    }

    impl PeerLink for SlowPeer {
        fn send(&self, request: &Request, _timeout: Duration) -> io::Result<Response> {
            self.sent.lock().unwrap().push(request.clone());
            match request {
                Request::List => Ok(Response::Sessions(Vec::new())),
                Request::CaptureStyled { .. } => Ok(Response::Error("no such session".into())),
                _ => {
                    let release = self.release.lock().unwrap();
                    let _released = self.released.wait_while(release, |go| !*go).unwrap();
                    Ok(Response::Ack { duplicate: false })
                }
            }
        }
    }

    impl SlowPeer {
        fn release(&self) {
            *self.release.lock().unwrap() = true;
            self.released.notify_all();
        }
    }

    #[test]
    fn a_peer_takes_composed_lines_through_its_writer() {
        let peer = Arc::new(SlowPeer::default());
        let port = RemotePort::with_link("peer", peer.clone());
        assert!(port.accepts_lines());
        assert_eq!(port.remote_node().as_deref(), Some("peer"));
        peer.release();
    }

    #[test]
    fn reads_come_from_the_background_cache() {
        let peer = Arc::new(SlowPeer::default());
        let port = RemotePort::with_link("peer", peer.clone());
        assert!(matches!(
            port.request(&Request::List),
            Ok(Response::Sessions(_))
        ));
        assert!(port.probe().is_ok());
        peer.release();
    }

    #[test]
    fn a_stalled_line_never_blocks_the_screen() {
        let peer = Arc::new(SlowPeer::default());
        let port = RemotePort::with_link("peer", peer.clone());
        let line = Request::InputLine {
            name: "build".into(),
            instance_id: "i".into(),
            client_id: "0".repeat(32),
            seq: 1,
            bytes: b"make\r".to_vec(),
        };
        let started = Instant::now();
        port.submit(line).unwrap();
        assert!(port.take_replies().is_empty());
        assert!(matches!(
            port.request(&Request::List),
            Ok(Response::Sessions(_))
        ));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "screen kept going"
        );
        peer.release();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut replies = Vec::new();
        while replies.is_empty() && Instant::now() < deadline {
            replies = port.take_replies();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            replies[..],
            [Ok(Response::Ack { duplicate: false })]
        ));
    }

    #[test]
    fn the_shown_screen_is_read_for_the_session_asked_for() {
        let peer = Arc::new(SlowPeer::default());
        let port = RemotePort::with_link("peer", peer.clone());
        let answer = port.request(&Request::CaptureStyled {
            name: "build".into(),
            scrollback: 0,
        });
        assert!(matches!(answer, Ok(Response::Error(reason)) if reason == "no such session"));
        assert!(peer.sent.lock().unwrap().iter().any(|request| matches!(
            request,
            Request::CaptureStyled { name, scrollback: 0 } if name == "build"
        )));
        peer.release();
    }
}
