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

type ScreenKey = (String, String, usize);
type CachedScreen = (ScreenKey, Result<Response, String>);

#[derive(Default)]
struct Cache {
    list: Option<Result<Response, String>>,
    /// The screen the user is looking at: (name, instance, scrollback).
    wanted: Option<ScreenKey>,
    screen: Option<CachedScreen>,
    last_ok: Option<Instant>,
}

struct Shared {
    cache: Mutex<Cache>,
    changed: Condvar,
    stop: AtomicBool,
    view: Mutex<ViewCache>,
}

#[derive(Default)]
struct ViewCache {
    key: Option<ViewKey>,
    generation: u64,
    in_flight: bool,
    started: Option<Instant>,
    answer: Option<(Instant, remuda_core::session_view::SessionView)>,
}
#[derive(Clone, PartialEq, Eq)]
struct ViewKey {
    width: u16,
    selected: Option<(String, String)>,
}
const VIEW_TTL: Duration = Duration::from_secs(2);
const VIEW_RETRY: Duration = Duration::from_millis(250);

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
            view: Mutex::new(ViewCache::default()),
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
            let screen = wanted.map(|(name, instance, scrollback)| {
                let request = Request::CaptureStyled {
                    name: name.clone(),
                    scrollback,
                };
                (
                    (name, instance, scrollback),
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

/// A bounded, ordered stream of input to one exact remote session instance.
pub struct RemoteKeys {
    sender: mpsc::SyncSender<Vec<u8>>,
    failure: Arc<Mutex<Option<String>>>,
    stopped: Arc<AtomicBool>,
}

impl Drop for RemoteKeys {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

impl KeyStream for RemoteKeys {
    fn keys(&self, bytes: &[u8]) -> io::Result<()> {
        self.check()?;
        if bytes.len() > crate::remote_front::MAX_REMOTE_INPUT_BATCH_BYTES {
            return Err(io::Error::other(
                "remote input exceeds the batch limit; nothing was queued",
            ));
        }
        if !bytes.is_empty() {
            self.sender.try_send(bytes.to_vec()).map_err(|error| {
                io::Error::other(format!("remote input could not be queued: {error}"))
            })?;
        }
        Ok(())
    }

    fn check(&self) -> io::Result<()> {
        match self
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(error) => Err(io::Error::other(error.clone())),
            None => Ok(()),
        }
    }
}

fn remote_keys(
    link: Arc<dyn PeerLink>,
    name: String,
    instance_id: String,
) -> io::Result<RemoteKeys> {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).map_err(|error| io::Error::other(error.to_string()))?;
    let client_id: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(128);
    let failure = Arc::new(Mutex::new(None));
    let stopped = Arc::new(AtomicBool::new(false));
    let worker_failure = failure.clone();
    let worker_stopped = stopped.clone();
    std::thread::spawn(move || {
        for (index, bytes) in receiver.into_iter().enumerate() {
            if worker_stopped.load(Ordering::SeqCst) {
                break;
            }
            let request = Request::Input {
                name: name.clone(),
                instance_id: instance_id.clone(),
                client_id: client_id.clone(),
                seq: index as u64 + 1,
                bytes,
            };
            let result = link.send(&request, REMOTE_LINE_TIMEOUT);
            if !matches!(result, Ok(Response::Ack { .. })) {
                let message = format!("remote input stopped ({result:?}); delivery may be uncertain; remaining keys discarded");
                *worker_failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
                break;
            }
        }
    });
    Ok(RemoteKeys {
        sender,
        failure,
        stopped,
    })
}

impl SessionPort for RemotePort {
    type Keys = RemoteKeys;

    fn request(&self, request: &Request) -> io::Result<Response> {
        match request {
            Request::List => self.cached(|cache| cache.list.clone()),
            Request::CaptureStyled { .. } => Err(io::Error::other(
                "remote preview requires an exact instance",
            )),
            // One-shot user actions (close, new, resize) wait for their answer.
            other => self.link.send(other, REMOTE_REQUEST_TIMEOUT),
        }
    }

    fn hold(&self, _name: &str) -> io::Result<RemoteKeys> {
        Err(io::Error::other("remote input requires an exact instance"))
    }

    fn hold_instance(&self, name: &str, instance: Option<&str>) -> io::Result<RemoteKeys> {
        let expected = instance
            .filter(|id| !id.is_empty())
            .ok_or_else(|| io::Error::other("peer did not provide a session instance"))?;
        let Response::Sessions(rows) = self.request(&Request::List)? else {
            return Err(io::Error::other("peer did not return a session list"));
        };
        let current = crate::cluster_api::pinned_instance(&rows, name).map_err(io::Error::other)?;
        if current != expected {
            return Err(io::Error::other("session instance changed; select again"));
        }
        remote_keys(self.link.clone(), name.to_owned(), expected.to_owned())
    }

    fn capture_instance(
        &self,
        name: &str,
        instance: Option<&str>,
        scrollback: usize,
    ) -> io::Result<Response> {
        let expected = instance
            .filter(|id| !id.is_empty())
            .ok_or_else(|| io::Error::other("peer did not provide a session instance"))?;
        let Response::Sessions(rows) = self.request(&Request::List)? else {
            return Err(io::Error::other("peer did not return a session list"));
        };
        let mut matching = rows.iter().filter(|row| row.name == name);
        let Some(current) = matching.next() else {
            return Err(io::Error::other("session is no longer listed"));
        };
        if matching.next().is_some()
            || current.unavailable
            || current.instance_id.as_deref() != Some(expected)
        {
            return Err(io::Error::other(
                "session instance changed or is unavailable; preview unavailable",
            ));
        }
        let key = (name.to_owned(), expected.to_owned(), scrollback);
        {
            let mut cache = self.cache();
            if cache.wanted.as_ref() != Some(&key) {
                cache.wanted = Some(key.clone());
                self.shared.changed.notify_all();
            }
        }
        self.cached(|cache| match &cache.screen {
            Some((read, answer))
                if read.0 == key.0 && read.1 == key.1 && (read.2 == key.2 || answer.is_ok()) =>
            {
                Some(match answer {
                    Ok(Response::StyledScreen {
                        instance_id: Some(id),
                        ..
                    }) if id == expected => answer.clone(),
                    Ok(Response::StyledScreen {
                        instance_id: None, ..
                    }) => Err("peer cannot prove preview instance; upgrade the peer".into()),
                    Ok(Response::StyledScreen { .. }) => {
                        Err("session instance changed; preview unavailable".into())
                    }
                    other => other.clone(),
                })
            }
            _ => None,
        })
    }

    fn session_list_view(
        &self,
        width: u16,
        selected: Option<(&str, &str)>,
    ) -> Option<remuda_core::session_view::SessionView> {
        let key = ViewKey {
            width,
            selected: selected.map(|(name, instance)| (name.into(), instance.into())),
        };
        let mut cache = self
            .shared
            .view
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.key.as_ref() != Some(&key) {
            cache.key = Some(key.clone());
            cache.generation = cache.generation.wrapping_add(1);
            cache.answer = None;
            cache.started = None;
        }
        let now = Instant::now();
        let answer = cache
            .answer
            .as_ref()
            .filter(|(started, _)| now.duration_since(*started) < VIEW_TTL)
            .map(|(_, view)| view.clone());
        if !cache.in_flight
            && cache
                .started
                .is_none_or(|started| now.duration_since(started) >= VIEW_RETRY)
        {
            cache.in_flight = true;
            cache.started = Some(now);
            let generation = cache.generation;
            let shared = self.shared.clone();
            let link = self.link.clone();
            std::thread::spawn(move || {
                let response = link.send(
                    &Request::RenderedSessionList { width: key.width },
                    REMOTE_REQUEST_TIMEOUT,
                );
                let mut cache = shared
                    .view
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cache.in_flight = false;
                if cache.generation == generation {
                    cache.answer = match response {
                        Ok(Response::RenderedSessionList(view)) if now.elapsed() < VIEW_TTL => {
                            Some((now, view))
                        }
                        _ => None,
                    };
                }
            });
        }
        answer
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
                Request::List => Ok(Response::Sessions(vec![remuda_core::SessionSummary {
                    id: "id".into(),
                    name: "build".into(),
                    instance_id: Some("i".into()),
                    output_version: None,
                    alive: true,
                    idle: Duration::ZERO,
                    output_idle: None,
                    size: remuda_core::Size::new(80, 24),
                    attached: false,
                    human_idle: None,
                    mouse_tracking: false,
                    unavailable: false,
                    program_status: None,
                    program_status_age_ms: None,
                    remote_input_disabled: false,
                }])),
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
    fn cached_preview_never_crosses_same_name_instance_changes() {
        struct Peer(AtomicBool);
        impl PeerLink for Peer {
            fn send(&self, request: &Request, _: Duration) -> io::Result<Response> {
                let instance = if self.0.load(Ordering::SeqCst) {
                    "new"
                } else {
                    "old"
                };
                match request {
                    Request::List => Ok(Response::Sessions(vec![remuda_core::SessionSummary {
                        id: instance.into(),
                        name: "proof".into(),
                        instance_id: Some(instance.into()),
                        output_version: None,
                        alive: true,
                        idle: Duration::ZERO,
                        output_idle: None,
                        size: remuda_core::Size::new(80, 24),
                        attached: false,
                        human_idle: None,
                        mouse_tracking: false,
                        unavailable: false,
                        program_status: None,
                        program_status_age_ms: None,
                        remote_input_disabled: false,
                    }])),
                    Request::CaptureStyled { .. } => Ok(Response::StyledScreen {
                        rows: vec![],
                        instance_id: Some(instance.into()),
                        output_version: Some(1),
                        wrapped: vec![],
                        scrollback_len: 0,
                        scrollback_total: 0,
                        cursor: remuda_core::agent::Cursor {
                            row: 0,
                            col: 0,
                            visible: false,
                        },
                    }),
                    _ => Ok(Response::Error("unsupported".into())),
                }
            }
        }
        let peer = Arc::new(Peer(AtomicBool::new(false)));
        let port = RemotePort::with_link("peer", peer.clone());
        assert!(
            matches!(port.capture_instance("proof", Some("old"), 0), Ok(Response::StyledScreen { instance_id: Some(id), .. }) if id == "old")
        );
        peer.0.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match port.capture_instance("proof", Some("new"), 0) {
                Ok(Response::StyledScreen {
                    instance_id: Some(id),
                    ..
                }) => {
                    assert_eq!(id, "new");
                    break;
                }
                Err(_) => {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => panic!("unexpected preview: {other:?}"),
            }
        }
    }

    #[test]
    fn stalled_view_never_blocks_the_base_list_and_old_peer_falls_back() {
        let peer = Arc::new(SlowPeer::default());
        let port = RemotePort::with_link("peer", peer.clone());
        let started = Instant::now();
        assert!(port.session_list_view(40, None).is_none());
        assert!(matches!(
            port.request(&Request::List),
            Ok(Response::Sessions(_))
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        peer.release();
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            port.session_list_view(40, None).is_none(),
            "unsupported view keeps the plain list"
        );
    }

    #[test]
    fn stale_or_wrong_generation_views_are_not_exposed() {
        let peer = Arc::new(SlowPeer::default());
        let port = RemotePort::with_link("peer", peer.clone());
        let old = ViewKey {
            width: 40,
            selected: None,
        };
        {
            let mut cache = port.shared.view.lock().unwrap();
            cache.key = Some(old);
            cache.started = Some(Instant::now());
            cache.answer = Some((
                Instant::now() - VIEW_TTL,
                remuda_core::session_view::SessionView::new(vec![], 1, vec![]),
            ));
        }
        assert!(
            port.session_list_view(40, None).is_none(),
            "expired view is discarded"
        );
        assert!(
            port.session_list_view(20, None).is_none(),
            "resizing drops the old view"
        );
        assert!(
            port.session_list_view(20, Some(("session", "new-instance")))
                .is_none(),
            "selection changes drop old views"
        );
        peer.release();
    }

    #[test]
    fn late_success_for_old_width_is_discarded_before_next_request() {
        struct Peer {
            started: AtomicBool,
            release: Mutex<bool>,
            gate: Condvar,
        }
        impl PeerLink for Peer {
            fn send(&self, request: &Request, _: Duration) -> io::Result<Response> {
                match request {
                    Request::List => Ok(Response::Sessions(vec![])),
                    Request::RenderedSessionList { width: 40 } => {
                        self.started.store(true, Ordering::SeqCst);
                        let release = self.release.lock().unwrap();
                        let _release = self.gate.wait_while(release, |ok| !*ok).unwrap();
                        Ok(Response::RenderedSessionList(
                            remuda_core::session_view::SessionView::new(vec![], 1, vec![]),
                        ))
                    }
                    Request::RenderedSessionList { .. } => Ok(Response::RenderedSessionList(
                        remuda_core::session_view::SessionView::new(vec![], 1, vec![]),
                    )),
                    _ => Ok(Response::Error("unsupported".into())),
                }
            }
        }
        let peer = Arc::new(Peer {
            started: AtomicBool::new(false),
            release: Mutex::new(false),
            gate: Condvar::new(),
        });
        let port = RemotePort::with_link("peer", peer.clone());
        assert!(port.session_list_view(40, None).is_none());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !peer.started.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(port.session_list_view(20, None).is_none());
        *peer.release.lock().unwrap() = true;
        peer.gate.notify_all();
        while port.shared.view.lock().unwrap().in_flight {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            port.shared.view.lock().unwrap().answer.is_none(),
            "old response must not be cached"
        );
        assert!(port.session_list_view(20, None).is_none());
        while port.session_list_view(20, None).is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
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
        let answer = port.capture_instance("build", Some("i"), 0);
        assert!(matches!(answer, Ok(Response::Error(reason)) if reason == "no such session"));
        assert!(peer.sent.lock().unwrap().iter().any(|request| matches!(
            request,
            Request::CaptureStyled { name, scrollback: 0 } if name == "build"
        )));
        peer.release();
    }
}
