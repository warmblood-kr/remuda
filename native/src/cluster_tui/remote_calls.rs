//! Remote calls the cluster UI makes: Input, InputLine, ResizeSession and
//! Close. Each opens a connection to a peer, which can take up to the client
//! timeout, so the UI loop hands them to a [`RemotePort`] and applies the
//! replies later instead of waiting.

use super::sender::RemoteStarted;
use crate::cluster_remote::RemoteInputTransport;
use remuda_core::protocol::{Request, Response};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

/// What a remote call was for, so its reply can be applied.
#[derive(Clone, Debug)]
pub enum RemoteCall {
    Input(Box<RemoteStarted>),
    Close(super::PendingClose),
    Resize(super::ResizeAttempt),
}

pub struct RemoteReply {
    pub call: RemoteCall,
    pub request: Request,
    pub result: io::Result<Response>,
}

pub trait RemotePort {
    /// Start one call to `node`. Its reply comes back from `take_replies`.
    fn submit(&mut self, node: &str, call: RemoteCall, request: Request);

    /// Replies that have arrived since the last call, oldest first.
    fn take_replies(&mut self) -> Vec<RemoteReply>;
}

fn call(
    transport: &dyn RemoteInputTransport,
    node: &str,
    call: &RemoteCall,
    request: &Request,
) -> io::Result<Response> {
    match call {
        RemoteCall::Close(_) => transport.send_close(node, request),
        RemoteCall::Input(_) | RemoteCall::Resize(_) => transport.send_input(node, request),
    }
}

/// Runs each call before `submit` returns. Unit tests use it to step the UI
/// without threads.
pub struct InlinePort<'a> {
    transport: &'a dyn RemoteInputTransport,
    replies: Vec<RemoteReply>,
}

impl<'a> InlinePort<'a> {
    pub fn new(transport: &'a dyn RemoteInputTransport) -> Self {
        Self {
            transport,
            replies: Vec::new(),
        }
    }
}

impl RemotePort for InlinePort<'_> {
    fn submit(&mut self, node: &str, remote_call: RemoteCall, request: Request) {
        let result = call(self.transport, node, &remote_call, &request);
        self.replies.push(RemoteReply {
            call: remote_call,
            request,
            result,
        });
    }

    fn take_replies(&mut self) -> Vec<RemoteReply> {
        std::mem::take(&mut self.replies)
    }
}

type Job = (RemoteCall, Request);

/// Runs calls on one worker thread per node, in the order they were
/// submitted, so a stalled peer delays only its own calls. Workers end when
/// the dispatcher is dropped and their current call returns.
pub struct RemoteDispatcher {
    transport: Arc<dyn RemoteInputTransport>,
    workers: HashMap<String, mpsc::Sender<Job>>,
    replies_tx: mpsc::Sender<RemoteReply>,
    replies_rx: mpsc::Receiver<RemoteReply>,
    finished: Arc<AtomicU64>,
}

impl RemoteDispatcher {
    pub fn new(transport: Arc<dyn RemoteInputTransport>) -> Self {
        let (replies_tx, replies_rx) = mpsc::channel();
        Self {
            transport,
            workers: HashMap::new(),
            replies_tx,
            replies_rx,
            finished: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Moves after each reply is queued, so the UI loop can wait on it.
    pub fn finished(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.finished)
    }

    fn worker(&mut self, node: &str) -> io::Result<mpsc::Sender<Job>> {
        if let Some(worker) = self.workers.get(node) {
            return Ok(worker.clone());
        }
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let transport = Arc::clone(&self.transport);
        let replies = self.replies_tx.clone();
        let finished = Arc::clone(&self.finished);
        let worker_node = node.to_owned();
        std::thread::Builder::new()
            .name("cluster-tui-remote-call".into())
            .spawn(move || {
                for (remote_call, request) in jobs_rx {
                    let result = call(transport.as_ref(), &worker_node, &remote_call, &request);
                    let reply = RemoteReply {
                        call: remote_call,
                        request,
                        result,
                    };
                    if replies.send(reply).is_err() {
                        return;
                    }
                    finished.fetch_add(1, Ordering::SeqCst);
                }
            })?;
        self.workers.insert(node.to_owned(), jobs_tx.clone());
        Ok(jobs_tx)
    }

    fn fail(&self, remote_call: RemoteCall, request: Request, error: io::Error) {
        let _ = self.replies_tx.send(RemoteReply {
            call: remote_call,
            request,
            result: Err(error),
        });
        self.finished.fetch_add(1, Ordering::SeqCst);
    }
}

impl RemotePort for RemoteDispatcher {
    fn submit(&mut self, node: &str, remote_call: RemoteCall, request: Request) {
        let worker = match self.worker(node) {
            Ok(worker) => worker,
            Err(error) => return self.fail(remote_call, request, error),
        };
        if let Err(mpsc::SendError((remote_call, request))) = worker.send((remote_call, request)) {
            self.workers.remove(node);
            self.fail(
                remote_call,
                request,
                io::Error::other("remote call worker stopped"),
            );
        }
    }

    fn take_replies(&mut self) -> Vec<RemoteReply> {
        self.replies_rx.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, Mutex};
    use std::time::{Duration, Instant};

    /// Blocks every call to `stalled` until released; other nodes answer at once.
    #[derive(Default)]
    struct Gate {
        released: Mutex<bool>,
        changed: Condvar,
    }

    impl RemoteInputTransport for Gate {
        fn send_input(&self, node: &str, _request: &Request) -> io::Result<Response> {
            if node == "stalled" {
                let released = self.released.lock().unwrap();
                let _released = self
                    .changed
                    .wait_while(released, |released| !*released)
                    .unwrap();
            }
            Ok(Response::Ok)
        }
    }

    fn resize_call(node: &str) -> (RemoteCall, Request) {
        let size = remuda_core::Size::new(80, 24);
        (
            RemoteCall::Resize(super::super::ResizeAttempt {
                node: Some(node.into()),
                name: "build".into(),
                instance: "i".into(),
                size,
            }),
            Request::ResizeSession {
                name: "build".into(),
                instance_id: "i".into(),
                size,
            },
        )
    }

    fn wait_for_reply(dispatcher: &mut RemoteDispatcher) -> Vec<RemoteReply> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let replies = dispatcher.take_replies();
            if !replies.is_empty() || Instant::now() > deadline {
                return replies;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_stalled_peer_blocks_neither_submit_nor_other_nodes() {
        let gate = Arc::new(Gate::default());
        let mut dispatcher = RemoteDispatcher::new(gate.clone());
        let finished = dispatcher.finished();

        let started = Instant::now();
        let (call, request) = resize_call("stalled");
        dispatcher.submit("stalled", call, request);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(dispatcher.take_replies().is_empty());

        let (call, request) = resize_call("healthy");
        dispatcher.submit("healthy", call, request);
        let replies = wait_for_reply(&mut dispatcher);
        assert_eq!(replies.len(), 1);
        assert!(matches!(
            &replies[0].call,
            RemoteCall::Resize(attempt) if attempt.node.as_deref() == Some("healthy")
        ));
        assert_eq!(finished.load(Ordering::SeqCst), 1);

        *gate.released.lock().unwrap() = true;
        gate.changed.notify_all();
        let replies = wait_for_reply(&mut dispatcher);
        assert_eq!(replies.len(), 1);
        assert!(matches!(replies[0].result, Ok(Response::Ok)));
        assert_eq!(finished.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn calls_to_one_node_reply_in_submit_order() {
        struct Echo;
        impl RemoteInputTransport for Echo {
            fn send_input(&self, _node: &str, request: &Request) -> io::Result<Response> {
                if let Request::ResizeSession { size, .. } = request {
                    std::thread::sleep(Duration::from_millis(u64::from(30 - size.cols() / 10)));
                }
                Ok(Response::Ok)
            }
        }
        let mut dispatcher = RemoteDispatcher::new(Arc::new(Echo));
        for cols in [100, 200, 290] {
            let size = remuda_core::Size::new(cols, 24);
            let (call, _) = resize_call("peer");
            dispatcher.submit(
                "peer",
                call,
                Request::ResizeSession {
                    name: "build".into(),
                    instance_id: "i".into(),
                    size,
                },
            );
        }
        let mut order = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while order.len() < 3 && Instant::now() < deadline {
            for reply in dispatcher.take_replies() {
                if let Request::ResizeSession { size, .. } = reply.request {
                    order.push(size.cols());
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(order, [100, 200, 290]);
    }
}
