use super::queue::{InputQueue, PendingBatch, QueueEvent, SendOutcome};
use remuda_core::input::MAX_INPUT_BYTES;
use remuda_core::protocol::{Request, Response};
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::time::Instant;

pub struct InputSender {
    client_id: [u8; 16],
    next_seq: HashMap<(String, String), u64>,
    target_client_ids: HashMap<(String, String), [u8; 16]>,
}

impl InputSender {
    pub fn with_client_id(client_id: [u8; 16]) -> Self {
        Self {
            client_id,
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
        let target = (name.to_owned(), instance_id.to_owned());
        let client_id = *self
            .target_client_ids
            .entry(target.clone())
            .or_insert(self.client_id);
        let next_seq = self.next_seq.entry(target).or_insert(1);
        let seq = *next_seq;
        *next_seq = seq
            .checked_add(1)
            .ok_or_else(|| io::Error::other("input sequence exhausted"))?;
        queue.enqueue(
            name.into(),
            instance_id.into(),
            format_client_id(&client_id),
            seq,
            bytes,
            now,
        );
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
        let event = queue.finish(batch.seq, outcome, now);
        if matches!(
            event,
            Some(QueueEvent::Uncertain { .. } | QueueEvent::Failed { .. })
        ) {
            self.rotate_target(queue, &batch.name, &batch.instance_id);
        }
        event
    }

    fn rotate_target(&mut self, queue: &mut InputQueue, name: &str, instance_id: &str) {
        let target = (name.to_owned(), instance_id.to_owned());
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
        let next_seq = queue.restart_waiting_target(name, instance_id, &client_id_text);
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

pub const INPUT_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    use crate::cluster_tui::queue::{InputQueue, QueueEvent, QueueState};
    use remuda_core::input::MAX_INPUT_BYTES;
    use remuda_core::protocol::{Request, Response};
    use std::collections::HashSet;
    use std::io;
    use std::time::{Duration, Instant};

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
}
