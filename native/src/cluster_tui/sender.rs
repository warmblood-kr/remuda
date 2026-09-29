use super::queue::{InputQueue, InputTarget, PendingBatch, QueueEvent, SendOutcome};
use crate::remote_front::MAX_REMOTE_INPUT_BATCH_BYTES;
use remuda_core::input::MAX_INPUT_BYTES;
use remuda_core::protocol::{Request, Response};
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::time::Instant;

pub struct InputSender {
    client_id: [u8; 16],
    next_paste_id: u64,
    next_seq: HashMap<(Option<String>, String, String), u64>,
    target_client_ids: HashMap<(Option<String>, String, String), [u8; 16]>,
}

impl InputSender {
    pub fn with_client_id(client_id: [u8; 16]) -> Self {
        Self {
            client_id,
            next_paste_id: 1,
            next_seq: HashMap::new(),
            target_client_ids: HashMap::new(),
        }
    }

    pub fn random() -> io::Result<Self> {
        let mut client_id = [0; 16];
        getrandom::fill(&mut client_id).map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self::with_client_id(client_id))
    }

    pub fn enqueue(
        &mut self,
        queue: &mut InputQueue,
        name: &str,
        instance_id: &str,
        bytes: Vec<u8>,
        now: Instant,
    ) -> io::Result<u64> {
        self.enqueue_one(
            queue,
            InputTarget {
                remote_node: None,
                name: name.into(),
                instance_id: instance_id.into(),
            },
            None,
            bytes,
            now,
        )
    }

    /// Queue a remote line as ordered batches within the listener's tighter cap.
    pub fn enqueue_remote(
        &mut self,
        queue: &mut InputQueue,
        node: &str,
        name: &str,
        instance_id: &str,
        bytes: Vec<u8>,
        now: Instant,
    ) -> io::Result<Vec<u64>> {
        let max_batch_bytes = MAX_REMOTE_INPUT_BATCH_BYTES;
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("line too long ({} bytes, max 64 KiB)", bytes.len()),
            ));
        }
        if bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input must contain at least one byte",
            ));
        }
        let chunk_count = bytes.len().div_ceil(max_batch_bytes);
        if !queue.can_enqueue(chunk_count) {
            return Err(queue_full_error());
        }
        let paste_id = if chunk_count > 1 {
            let paste_id = self.next_paste_id;
            self.next_paste_id = paste_id
                .checked_add(1)
                .ok_or_else(|| io::Error::other("paste id exhausted"))?;
            Some(paste_id)
        } else {
            None
        };
        let mut seqs = Vec::with_capacity(chunk_count);
        for chunk in bytes.chunks(max_batch_bytes) {
            seqs.push(self.enqueue_one(
                queue,
                InputTarget {
                    remote_node: Some(node.into()),
                    name: name.into(),
                    instance_id: instance_id.into(),
                },
                paste_id,
                chunk.to_vec(),
                now,
            )?);
        }
        Ok(seqs)
    }

    fn enqueue_one(
        &mut self,
        queue: &mut InputQueue,
        target: InputTarget,
        paste_id: Option<u64>,
        bytes: Vec<u8>,
        now: Instant,
    ) -> io::Result<u64> {
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("line too long ({} bytes, max 64 KiB)", bytes.len()),
            ));
        }
        if bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input must contain at least one byte",
            ));
        }
        if !queue.can_enqueue(1) {
            return Err(queue_full_error());
        }
        let target_key = (
            target.remote_node.clone(),
            target.name.clone(),
            target.instance_id.clone(),
        );
        let client_id = *self
            .target_client_ids
            .entry(target_key.clone())
            .or_insert(self.client_id);
        let next_seq = self.next_seq.entry(target_key).or_insert(1);
        let seq = *next_seq;
        *next_seq = seq
            .checked_add(1)
            .ok_or_else(|| io::Error::other("input sequence exhausted"))?;
        if !queue.enqueue_target(
            target,
            paste_id,
            format_client_id(&client_id),
            seq,
            bytes,
            now,
        ) {
            return Err(queue_full_error());
        }
        Ok(seq)
    }

    pub fn send_due(
        &mut self,
        queue: &mut InputQueue,
        path: &Path,
        now: Instant,
    ) -> Option<QueueEvent> {
        self.attempt_due(queue, now, |request| {
            crate::client::request_with_timeout(path, request, INPUT_SEND_TIMEOUT)
        })
    }

    pub fn begin_due(&self, queue: &mut InputQueue, now: Instant) -> bool {
        queue.begin_due(now).is_some()
    }

    pub fn send_started<F>(
        &mut self,
        queue: &mut InputQueue,
        now: Instant,
        mut send: F,
    ) -> Option<QueueEvent>
    where
        F: FnMut(&Request) -> io::Result<Response>,
    {
        let batch = queue.sending_batch()?;
        let request = input_request(&batch);
        let outcome = match send(&request) {
            Ok(Response::Ack { duplicate }) => SendOutcome::Ack { duplicate },
            Ok(Response::Uncertain) => SendOutcome::Uncertain,
            Ok(Response::WrongInstance) => SendOutcome::WrongInstance,
            Ok(Response::RateLimited) => SendOutcome::RateLimited,
            Ok(Response::Error(reason)) if reason == crate::client::EMPTY_REPLY_ERROR => {
                SendOutcome::IoFailure(reason)
            }
            Ok(Response::Error(reason)) => SendOutcome::Error(reason),
            Ok(other) => SendOutcome::Error(format!("unexpected Input response: {other:?}")),
            Err(error) => SendOutcome::IoFailure(error.to_string()),
        };
        self.finish(queue, &batch, outcome, now)
    }

    /// Send a prepared remote batch. Listener errors are ambiguous because they
    /// can race a dispatch, so this path retries the identical request.
    pub fn send_remote_started<F>(
        &mut self,
        queue: &mut InputQueue,
        now: Instant,
        mut send: F,
    ) -> Option<QueueEvent>
    where
        F: FnMut(&str, &Request) -> io::Result<Response>,
    {
        let batch = queue.sending_batch()?;
        let request = input_request(&batch);
        let Some(node) = batch.remote_node.as_deref() else {
            return self.finish(
                queue,
                &batch,
                SendOutcome::Error("remote target is missing".into()),
                now,
            );
        };
        let outcome = match send(node, &request) {
            Ok(Response::Ack { duplicate }) => SendOutcome::Ack { duplicate },
            Ok(Response::Uncertain) => SendOutcome::Uncertain,
            Ok(Response::WrongInstance) => SendOutcome::WrongInstance,
            Ok(Response::RateLimited) => SendOutcome::RateLimited,
            Ok(Response::RemoteControlDisabled) => SendOutcome::RemoteControlDisabled,
            Ok(Response::Error(reason)) if reason == remote_oversize_error() => {
                SendOutcome::IoFailure(format!("bug: {reason}"))
            }
            Ok(Response::Error(reason)) => SendOutcome::IoFailure(reason),
            Ok(other) => {
                SendOutcome::IoFailure(format!("unexpected remote Input response: {other:?}"))
            }
            Err(error) => SendOutcome::IoFailure(error.to_string()),
        };
        self.finish(queue, &batch, outcome, now)
    }

    fn finish(
        &mut self,
        queue: &mut InputQueue,
        batch: &PendingBatch,
        outcome: SendOutcome,
        now: Instant,
    ) -> Option<QueueEvent> {
        let mut event = queue.finish(batch.seq, outcome, now);
        let paste_failed = batch.paste_id.is_some()
            && matches!(
                event,
                Some(QueueEvent::Uncertain { .. } | QueueEvent::Failed { .. })
            );
        if paste_failed {
            let paste_id = batch.paste_id.expect("paste failure has a paste id");
            queue.drop_waiting_paste(
                batch.remote_node.as_deref(),
                &batch.name,
                &batch.instance_id,
                paste_id,
                "paste stopped after a failed chunk",
            );
            let reason = match event.take().expect("terminal paste event exists") {
                QueueEvent::Uncertain { reason, .. } | QueueEvent::Failed { reason, .. } => reason,
                _ => unreachable!("paste failure is uncertain or failed"),
            };
            event = Some(QueueEvent::PasteAborted {
                seq: batch.seq,
                reason,
            });
        }
        if matches!(
            event,
            Some(
                QueueEvent::Uncertain { .. }
                    | QueueEvent::Failed { .. }
                    | QueueEvent::PasteAborted { .. }
            )
        ) {
            self.rotate_target(queue, batch);
        } else if matches!(event, Some(QueueEvent::Dropped { .. })) {
            queue.drop_waiting_target(
                batch.remote_node.as_deref(),
                &batch.name,
                &batch.instance_id,
                "session restarted; input dropped",
            );
        } else if matches!(event, Some(QueueEvent::RemoteControlDisabled { .. })) {
            if let Some(node) = batch.remote_node.as_deref() {
                queue.drop_waiting_node(node, "remote control disabled");
            }
            self.rotate_target(queue, batch);
        }
        event
    }

    fn rotate_target(&mut self, queue: &mut InputQueue, batch: &PendingBatch) {
        let target = (
            batch.remote_node.clone(),
            batch.name.clone(),
            batch.instance_id.clone(),
        );
        let previous = self
            .target_client_ids
            .get(&target)
            .copied()
            .unwrap_or(self.client_id);
        let mut client_id = [0; 16];
        if getrandom::fill(&mut client_id).is_err() || client_id == previous {
            client_id = increment_client_id(previous);
        }
        let client_id_text = format_client_id(&client_id);
        let next_seq = queue.restart_waiting_target(
            batch.remote_node.as_deref(),
            &batch.name,
            &batch.instance_id,
            &client_id_text,
        );
        self.target_client_ids.insert(target.clone(), client_id);
        self.next_seq.insert(target, next_seq);
    }

    pub fn attempt_due<F>(
        &mut self,
        queue: &mut InputQueue,
        now: Instant,
        send: F,
    ) -> Option<QueueEvent>
    where
        F: FnMut(&Request) -> io::Result<Response>,
    {
        queue.begin_due(now)?;
        self.send_started(queue, now, send)
    }
}

fn remote_oversize_error() -> String {
    format!("remote Input batch exceeds {MAX_REMOTE_INPUT_BATCH_BYTES} bytes")
}

fn queue_full_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "input queue is full (max {} batches)",
            super::queue::MAX_QUEUED_BATCHES
        ),
    )
}

/// Uncertain is bounded by 8s: four 1s attempts plus retry and UI-loop overhead.
pub const INPUT_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

pub fn input_request(batch: &PendingBatch) -> Request {
    Request::Input {
        name: batch.name.clone(),
        instance_id: batch.instance_id.clone(),
        client_id: batch.client_id.clone(),
        seq: batch.seq,
        bytes: batch.bytes.clone(),
    }
}

pub fn format_client_id(client_id: &[u8; 16]) -> String {
    client_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

fn increment_client_id(mut client_id: [u8; 16]) -> [u8; 16] {
    for byte in client_id.iter_mut().rev() {
        let (next, carry) = byte.overflowing_add(1);
        *byte = next;
        if !carry {
            break;
        }
    }
    client_id
}

#[cfg(test)]
mod tests {
    use super::InputSender;
    use crate::cluster_tui::queue::{InputQueue, QueueEvent, QueueState, MAX_QUEUED_BATCHES};
    use remuda_core::input::MAX_INPUT_BYTES;
    use remuda_core::protocol::{Request, Response};
    use std::collections::HashSet;
    use std::io;
    use std::time::{Duration, Instant};

    fn remote_queued(bytes: Vec<u8>) -> (InputSender, InputQueue, Instant) {
        let now = Instant::now();
        let mut sender = InputSender::with_client_id([7; 16]);
        let mut queue = InputQueue::default();
        sender
            .enqueue_remote(&mut queue, "laptop", "build", "remote-instance", bytes, now)
            .unwrap();
        (sender, queue, now)
    }

    fn queued() -> (InputSender, InputQueue, Instant) {
        let now = Instant::now();
        let mut sender = InputSender::with_client_id([7; 16]);
        let mut queue = InputQueue::default();
        sender
            .enqueue(&mut queue, "dev", "instance-a", b"line\r".to_vec(), now)
            .unwrap();
        (sender, queue, now)
    }

    #[test]
    fn rate_limited_batch_retries_after_backoff() {
        let (mut sender, mut queue, now) = queued();
        let first_request = std::cell::RefCell::new(None);
        assert!(matches!(
            sender.attempt_due(&mut queue, now, |request| {
                first_request.replace(Some(request.clone()));
                Ok(Response::RateLimited)
            }),
            Some(QueueEvent::RetryScheduled { seq: 1, .. })
        ));
        assert!(sender
            .attempt_due(&mut queue, now + Duration::from_millis(1), |_| {
                Ok(Response::Ack { duplicate: false })
            })
            .is_none());
        let mut request_seen = None;
        assert!(matches!(
            sender.attempt_due(&mut queue, now + Duration::from_secs(1), |request| {
                request_seen = Some(request.clone());
                Ok(Response::Ack { duplicate: false })
            }),
            Some(QueueEvent::Sent { seq: 1, .. })
        ));
        assert_eq!(
            first_request.into_inner().unwrap(),
            request_seen.clone().unwrap()
        );
        assert!(
            matches!(request_seen, Some(Request::Input { seq: 1, bytes, .. }) if bytes == b"line\r")
        );
        assert_eq!(queue.items().next().unwrap().state, QueueState::Sent);
    }

    #[test]
    fn uncertain_batch_is_never_retried() {
        let (mut sender, mut queue, now) = queued();
        let calls = std::cell::Cell::new(0);
        assert!(matches!(
            sender.attempt_due(&mut queue, now, |_| {
                calls.set(calls.get() + 1);
                Ok(Response::Uncertain)
            }),
            Some(QueueEvent::Uncertain { seq: 1, .. })
        ));
        assert!(sender
            .attempt_due(&mut queue, now + Duration::from_secs(60), |_| {
                calls.set(calls.get() + 1);
                Ok(Response::Ack { duplicate: false })
            })
            .is_none());
        assert_eq!(calls.get(), 1);
        assert_eq!(queue.items().next().unwrap().state, QueueState::Uncertain);
    }

    #[test]
    fn wrong_instance_drops_batch_with_notice() {
        let (mut sender, mut queue, now) = queued();
        assert!(matches!(
            sender.attempt_due(&mut queue, now, |_| Ok(Response::WrongInstance)),
            Some(QueueEvent::Dropped { seq: 1, .. })
        ));
        let item = queue.items().next().unwrap();
        assert_eq!(item.state, QueueState::Dropped);
        assert!(item.status.contains("session restarted"));
    }

    #[test]
    fn input_error_is_shown_without_automatic_retry() {
        let (mut sender, mut queue, now) = queued();
        let first_client = queue.items().next().unwrap().client_id.clone();
        sender
            .enqueue(&mut queue, "dev", "instance-a", b"next\r".to_vec(), now)
            .unwrap();
        assert!(matches!(
            sender.attempt_due(&mut queue, now, |_| {
                Ok(Response::Error("no such session: dev".into()))
            }),
            Some(QueueEvent::Failed { seq: 1, .. })
        ));
        assert_eq!(queue.items().next().unwrap().state, QueueState::Failed);
        let next = queue.items().next_back().unwrap();
        assert_eq!(next.seq, 1);
        assert_ne!(next.client_id, first_client);
        let rotated_client = next.client_id.clone();
        assert_eq!(
            sender
                .enqueue(&mut queue, "dev", "instance-a", b"third\r".to_vec(), now)
                .unwrap(),
            2
        );
        let third = queue.items().next_back().unwrap();
        assert_eq!(third.client_id, rotated_client);
    }

    #[test]
    fn io_failure_retries_identical_batch_and_applies_once() {
        let (mut sender, mut queue, now) = queued();
        let first_request = std::cell::RefCell::new(None);
        let writes = std::cell::Cell::new(0);
        let mut seen = HashSet::new();
        assert!(matches!(
            sender.attempt_due(&mut queue, now, |request| {
                first_request.replace(Some(request.clone()));
                if let Request::Input { client_id, seq, .. } = request {
                    if seen.insert((client_id.clone(), *seq)) {
                        writes.set(writes.get() + 1);
                    }
                }
                Err(io::Error::new(io::ErrorKind::ConnectionReset, "reply lost"))
            }),
            Some(QueueEvent::RetryScheduled { seq: 1, .. })
        ));
        let first = first_request.into_inner().unwrap();
        let second_request = std::cell::RefCell::new(None);
        assert!(matches!(
            sender.attempt_due(&mut queue, now + Duration::from_secs(1), |request| {
                second_request.replace(Some(request.clone()));
                if let Request::Input { client_id, seq, .. } = request {
                    if !seen.insert((client_id.clone(), *seq)) {
                        return Ok(Response::Ack { duplicate: true });
                    }
                    writes.set(writes.get() + 1);
                }
                Ok(Response::Ack { duplicate: false })
            }),
            Some(QueueEvent::Sent {
                seq: 1,
                duplicate: true
            })
        ));
        assert_eq!(first, second_request.into_inner().unwrap());
        assert_eq!(writes.get(), 1);
    }

    #[test]
    fn uncertain_batch_rotates_client_id_and_restarts_sequence_for_next_line() {
        let (mut sender, mut queue, now) = queued();
        let first_client = queue.items().next().unwrap().client_id.clone();
        sender
            .enqueue(&mut queue, "dev", "instance-a", b"next\r".to_vec(), now)
            .unwrap();
        assert!(matches!(
            sender.attempt_due(&mut queue, now, |_| Ok(Response::Uncertain)),
            Some(QueueEvent::Uncertain { seq: 1, .. })
        ));
        let next = queue.items().next_back().unwrap();
        assert_eq!(next.seq, 1);
        assert_ne!(next.client_id, first_client);
        let rotated_client = next.client_id.clone();
        assert_eq!(
            sender
                .enqueue(&mut queue, "dev", "instance-a", b"third\r".to_vec(), now)
                .unwrap(),
            2
        );
        assert_eq!(queue.items().next_back().unwrap().client_id, rotated_client);
    }

    #[test]
    fn empty_daemon_reply_uses_identical_retry_path() {
        let (mut sender, mut queue, now) = queued();
        let first = std::cell::RefCell::new(None);
        assert!(matches!(
            sender.attempt_due(&mut queue, now, |request| {
                first.replace(Some(request.clone()));
                Ok(Response::Error(
                    "the daemon hung up without answering".into(),
                ))
            }),
            Some(QueueEvent::RetryScheduled { seq: 1, .. })
        ));
        assert!(matches!(
            sender.attempt_due(&mut queue, now + Duration::from_secs(1), |request| {
                assert_eq!(Some(request.clone()), first.borrow().clone());
                Ok(Response::Ack { duplicate: true })
            }),
            Some(QueueEvent::Sent {
                seq: 1,
                duplicate: true
            })
        ));
    }

    #[test]
    fn oversized_line_reports_its_byte_count_and_limit() {
        let mut sender = InputSender::with_client_id([7; 16]);
        let mut queue = InputQueue::default();
        let error = sender
            .enqueue(
                &mut queue,
                "dev",
                "instance-a",
                vec![b'x'; MAX_INPUT_BYTES + 1],
                Instant::now(),
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("line too long ({} bytes, max 64 KiB)", MAX_INPUT_BYTES + 1)
        );
    }

    #[test]
    fn seq_starts_at_one_for_each_session_instance() {
        let mut sender = InputSender::with_client_id([3; 16]);
        let mut queue = InputQueue::default();
        let now = Instant::now();
        assert_eq!(
            sender
                .enqueue(&mut queue, "dev", "instance-a", b"a\r".to_vec(), now)
                .unwrap(),
            1
        );
        assert_eq!(
            sender
                .enqueue(&mut queue, "shell", "instance-b", b"b\r".to_vec(), now)
                .unwrap(),
            1
        );
        assert_eq!(
            sender
                .enqueue(&mut queue, "dev", "instance-a", b"c\r".to_vec(), now)
                .unwrap(),
            2
        );
    }

    #[test]
    fn io_retries_are_bounded_then_mark_delivery_uncertain() {
        let (mut sender, mut queue, now) = queued();
        let calls = std::cell::Cell::new(0);
        for attempt in 0..=super::super::queue::MAX_IO_RETRIES {
            let result = sender.attempt_due(
                &mut queue,
                now + Duration::from_secs(10 * u64::from(attempt)),
                |_| {
                    calls.set(calls.get() + 1);
                    Err(io::Error::new(io::ErrorKind::ConnectionReset, "reply lost"))
                },
            );
            if attempt < super::super::queue::MAX_IO_RETRIES {
                assert!(matches!(result, Some(QueueEvent::RetryScheduled { .. })));
            } else {
                assert!(matches!(result, Some(QueueEvent::Uncertain { .. })));
            }
        }
        assert_eq!(calls.get(), 4);
        assert!(sender
            .attempt_due(&mut queue, now + Duration::from_secs(100), |_| {
                calls.set(calls.get() + 1);
                Ok(Response::Ack { duplicate: true })
            })
            .is_none());
        assert_eq!(calls.get(), 4);
        assert_eq!(queue.items().next().unwrap().state, QueueState::Uncertain);
    }

    #[test]
    fn remote_line_is_chunked_to_the_listener_limit() {
        let now = Instant::now();
        let mut sender = InputSender::with_client_id([7; 16]);
        let mut queue = InputQueue::default();
        let limit = crate::remote_front::MAX_REMOTE_INPUT_BATCH_BYTES;
        let seqs = sender
            .enqueue_remote(
                &mut queue,
                "laptop",
                "build",
                "remote-instance",
                vec![b'x'; limit * 2 + 7],
                now,
            )
            .unwrap();
        assert_eq!(seqs, [1, 2, 3]);
        let batches = queue.items().collect::<Vec<_>>();
        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.bytes.len())
                .collect::<Vec<_>>(),
            [limit, limit, 7]
        );
        assert!(batches
            .iter()
            .all(|batch| batch.remote_node.as_deref() == Some("laptop")));
    }

    #[test]
    fn remote_line_is_rejected_atomically_when_its_chunks_exceed_queue_capacity() {
        let now = Instant::now();
        let mut sender = InputSender::with_client_id([7; 16]);
        let mut queue = InputQueue::default();
        for seq in 1..MAX_QUEUED_BATCHES {
            assert!(queue.enqueue(
                format!("session-{seq}"),
                "instance".into(),
                "client".into(),
                seq as u64,
                vec![b'x'],
                now,
            ));
        }

        let result = sender.enqueue_remote(
            &mut queue,
            "fp-laptop",
            "build",
            "remote-instance",
            vec![b'x'; MAX_INPUT_BYTES],
            now,
        );

        assert!(result.is_err());
        assert_eq!(queue.items().count(), MAX_QUEUED_BATCHES - 1);
    }

    #[test]
    fn remote_listener_errors_retry_identically_then_uncertain_rotates_target() {
        let (mut sender, mut queue, now) = remote_queued(b"first\r".to_vec());
        let first_client = queue.items().next().unwrap().client_id.clone();
        sender
            .enqueue_remote(
                &mut queue,
                "laptop",
                "build",
                "remote-instance",
                b"next\r".to_vec(),
                now,
            )
            .unwrap();
        let mut first_request = None;
        for attempt in 0..=super::super::queue::MAX_IO_RETRIES {
            let at = now + Duration::from_secs(10 * u64::from(attempt));
            assert!(queue.begin_due(at).is_some());
            let result = sender.send_remote_started(&mut queue, at, |node, request| {
                assert_eq!(node, "laptop");
                if let Some(first) = &first_request {
                    assert_eq!(first, request);
                } else {
                    first_request = Some(request.clone());
                }
                if attempt == 0 {
                    Ok(Response::Error("listener dispatch error".into()))
                } else {
                    Err(io::Error::new(io::ErrorKind::TimedOut, "listener timeout"))
                }
            });
            if attempt < super::super::queue::MAX_IO_RETRIES {
                assert!(matches!(
                    result,
                    Some(QueueEvent::RetryScheduled { seq: 1, .. })
                ));
            } else {
                assert!(matches!(result, Some(QueueEvent::Uncertain { seq: 1, .. })));
            }
        }
        let waiting = queue.items().next_back().unwrap();
        assert_eq!(waiting.state, QueueState::Waiting);
        assert_eq!(waiting.seq, 1);
        assert_ne!(waiting.client_id, first_client);
    }

    #[test]
    fn remote_control_disabled_marks_batch_and_stops_waiting_target() {
        let (mut sender, mut queue, now) = remote_queued(b"first\r".to_vec());
        let client_id = queue.items().next().unwrap().client_id.clone();
        sender
            .enqueue_remote(
                &mut queue,
                "laptop",
                "build",
                "remote-instance",
                b"next\r".to_vec(),
                now,
            )
            .unwrap();
        assert!(queue.begin_due(now).is_some());
        assert!(matches!(
            sender.send_remote_started(&mut queue, now, |_, _| {
                Ok(Response::RemoteControlDisabled)
            }),
            Some(QueueEvent::RemoteControlDisabled { node, .. }) if node == "laptop"
        ));
        assert!(queue
            .items()
            .all(|batch| batch.state != QueueState::Waiting));
        assert_eq!(queue.items().next().unwrap().client_id, client_id);
    }

    #[test]
    fn remote_control_disabled_rotates_client_and_restarts_sequence() {
        let (mut sender, mut queue, now) = remote_queued(b"first\r".to_vec());
        let first_client_id = queue.items().next().unwrap().client_id.clone();
        assert!(queue.begin_due(now).is_some());
        assert!(matches!(
            sender.send_remote_started(&mut queue, now, |_, _| {
                Ok(Response::RemoteControlDisabled)
            }),
            Some(QueueEvent::RemoteControlDisabled { .. })
        ));

        sender
            .enqueue_remote(
                &mut queue,
                "laptop",
                "build",
                "remote-instance",
                b"next\r".to_vec(),
                now,
            )
            .unwrap();
        let next = queue
            .items()
            .find(|batch| batch.state == QueueState::Waiting)
            .unwrap();
        assert_eq!(next.seq, 1);
        assert_ne!(next.client_id, first_client_id);
    }

    #[test]
    fn remote_control_disabled_drops_every_queued_session_on_that_node() {
        let now = Instant::now();
        let mut sender = InputSender::with_client_id([7; 16]);
        let mut queue = InputQueue::default();
        for (node, name) in [
            ("laptop", "build"),
            ("laptop", "logs"),
            ("desktop", "build"),
        ] {
            sender
                .enqueue_remote(
                    &mut queue,
                    node,
                    name,
                    "remote-instance",
                    b"line\r".to_vec(),
                    now,
                )
                .unwrap();
        }
        assert!(queue.begin_due(now).is_some());
        assert!(matches!(
            sender.send_remote_started(&mut queue, now, |_, _| {
                Ok(Response::RemoteControlDisabled)
            }),
            Some(QueueEvent::RemoteControlDisabled { node, .. }) if node == "laptop"
        ));
        let waiting: Vec<_> = queue
            .items()
            .filter(|batch| batch.state == QueueState::Waiting)
            .map(|batch| batch.remote_node.as_deref())
            .collect();
        assert_eq!(waiting, [Some("desktop")]);
    }

    #[test]
    fn remote_rate_limit_retries_the_same_batch() {
        let (mut sender, mut queue, now) = remote_queued(b"line\r".to_vec());
        let mut first = None;
        assert!(queue.begin_due(now).is_some());
        assert!(matches!(
            sender.send_remote_started(&mut queue, now, |_, request| {
                first = Some(request.clone());
                Ok(Response::RateLimited)
            }),
            Some(QueueEvent::RetryScheduled { seq: 1, .. })
        ));
        let retry_at = now + Duration::from_secs(1);
        assert!(queue.begin_due(retry_at).is_some());
        assert!(matches!(
            sender.send_remote_started(&mut queue, retry_at, |_, request| {
                assert_eq!(Some(request.clone()), first);
                Ok(Response::Ack { duplicate: false })
            }),
            Some(QueueEvent::Sent { seq: 1, .. })
        ));
    }

    #[test]
    fn remote_wrong_instance_drops_all_queued_chunks() {
        let (mut sender, mut queue, now) = remote_queued(b"first\r".to_vec());
        sender
            .enqueue_remote(
                &mut queue,
                "laptop",
                "build",
                "remote-instance",
                b"next\r".to_vec(),
                now,
            )
            .unwrap();
        assert!(queue.begin_due(now).is_some());
        assert!(matches!(
            sender.send_remote_started(&mut queue, now, |_, _| Ok(Response::WrongInstance)),
            Some(QueueEvent::Dropped { seq: 1, .. })
        ));
        assert!(queue
            .items()
            .all(|batch| batch.state == QueueState::Dropped));
    }

    #[test]
    fn remote_oversize_error_retries_identically_then_marks_uncertain_with_bug_notice() {
        let (mut sender, mut queue, now) = remote_queued(b"line\r".to_vec());
        sender
            .enqueue_remote(
                &mut queue,
                "laptop",
                "build",
                "remote-instance",
                b"next\r".to_vec(),
                now,
            )
            .unwrap();
        let first_client_id = queue.items().next().unwrap().client_id.clone();
        let mut first_request = None;
        let oversize = Response::Error(format!(
            "remote Input batch exceeds {} bytes",
            crate::remote_front::MAX_REMOTE_INPUT_BATCH_BYTES
        ));
        for attempt in 0..=super::super::queue::MAX_IO_RETRIES {
            let attempt_at = now + Duration::from_secs(u64::from(attempt));
            assert!(queue.begin_due(attempt_at).is_some());
            let result = sender.send_remote_started(&mut queue, attempt_at, |_, request| {
                if let Some(first) = &first_request {
                    assert_eq!(request, first);
                } else {
                    first_request = Some(request.clone());
                }
                Ok(oversize.clone())
            });
            if attempt < super::super::queue::MAX_IO_RETRIES {
                assert!(matches!(
                    result,
                    Some(QueueEvent::RetryScheduled { seq: 1, .. })
                ));
            } else {
                assert!(matches!(
                    result,
                    Some(QueueEvent::Uncertain { seq: 1, reason })
                        if reason.contains("bug:") && reason.contains("exceeds")
                ));
            }
        }
        let next = queue.items().next().unwrap();
        assert_eq!(next.state, QueueState::Uncertain);
        assert!(next.status.contains("bug:"));
        let waiting = queue
            .items()
            .find(|batch| batch.state == QueueState::Waiting)
            .unwrap();
        assert_eq!(waiting.seq, 1);
        assert_ne!(waiting.client_id, first_client_id);
    }
}
