use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueState {
    Waiting,
    Sending,
    Sent,
    Uncertain,
    Dropped,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingBatch {
    pub remote_node: Option<String>,
    pub name: String,
    pub instance_id: String,
    pub paste_id: Option<u64>,
    pub client_id: String,
    pub seq: u64,
    pub bytes: Vec<u8>,
    pub state: QueueState,
    pub status: String,
    retry_at: Instant,
    io_retries: u8,
    rate_retries: u8,
}

pub struct InputTarget {
    pub remote_node: Option<String>,
    pub name: String,
    pub instance_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Ack { duplicate: bool },
    Uncertain,
    WrongInstance,
    RateLimited,
    RemoteControlDisabled,
    Error(String),
    IoFailure(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueEvent {
    Sent { seq: u64, duplicate: bool },
    RetryScheduled { seq: u64, retry_at: Instant },
    Uncertain { seq: u64, reason: String },
    Dropped { seq: u64, reason: String },
    Failed { seq: u64, reason: String },
    RemoteControlDisabled { seq: u64, node: String },
    PasteAborted { seq: u64, reason: String },
    RemoteTargetFailed { seq: u64, reason: String },
}

pub const MAX_IO_RETRIES: u8 = 3;
pub const MAX_RATE_LIMIT_RETRIES: u8 = 3;
pub const MAX_QUEUED_BATCHES: usize = 128;

#[derive(Default)]
pub struct InputQueue {
    batches: VecDeque<PendingBatch>,
}

impl InputQueue {
    pub fn enqueue(
        &mut self,
        name: String,
        instance_id: String,
        client_id: String,
        seq: u64,
        bytes: Vec<u8>,
        now: Instant,
    ) -> bool {
        self.enqueue_target(
            InputTarget {
                remote_node: None,
                name,
                instance_id,
            },
            None,
            client_id,
            seq,
            bytes,
            now,
        )
    }

    pub fn enqueue_target(
        &mut self,
        target: InputTarget,
        paste_id: Option<u64>,
        client_id: String,
        seq: u64,
        bytes: Vec<u8>,
        now: Instant,
    ) -> bool {
        if !self.can_enqueue(1) {
            return false;
        }
        self.batches.push_back(PendingBatch {
            remote_node: target.remote_node,
            name: target.name,
            instance_id: target.instance_id,
            paste_id,
            client_id,
            seq,
            bytes,
            state: QueueState::Waiting,
            status: "waiting to send".into(),
            retry_at: now,
            io_retries: 0,
            rate_retries: 0,
        });
        true
    }

    pub fn can_enqueue(&self, count: usize) -> bool {
        self.batches.len().saturating_add(count) <= MAX_QUEUED_BATCHES
    }

    pub fn items(&self) -> impl DoubleEndedIterator<Item = &PendingBatch> {
        self.batches.iter()
    }

    pub fn sending_batch(&self) -> Option<PendingBatch> {
        self.batches
            .iter()
            .find(|batch| batch.state == QueueState::Sending)
            .cloned()
    }

    pub fn restart_waiting_target(
        &mut self,
        remote_node: Option<&str>,
        name: &str,
        instance_id: &str,
        client_id: &str,
    ) -> u64 {
        let mut next_seq = 1;
        for batch in &mut self.batches {
            if batch.name == name
                && batch.remote_node.as_deref() == remote_node
                && batch.instance_id == instance_id
                && batch.state == QueueState::Waiting
            {
                batch.client_id = client_id.into();
                batch.seq = next_seq;
                next_seq += 1;
            }
        }
        next_seq
    }

    pub fn drop_waiting_target(
        &mut self,
        remote_node: Option<&str>,
        name: &str,
        instance_id: &str,
        reason: &str,
    ) {
        for batch in &mut self.batches {
            if batch.name == name
                && batch.remote_node.as_deref() == remote_node
                && batch.instance_id == instance_id
                && batch.state == QueueState::Waiting
            {
                batch.state = QueueState::Dropped;
                batch.status = reason.into();
            }
        }
        self.prune_finished();
    }

    pub fn drop_waiting_paste(
        &mut self,
        remote_node: Option<&str>,
        name: &str,
        instance_id: &str,
        paste_id: u64,
        reason: &str,
    ) {
        for batch in &mut self.batches {
            if batch.name == name
                && batch.remote_node.as_deref() == remote_node
                && batch.instance_id == instance_id
                && batch.paste_id == Some(paste_id)
                && batch.state == QueueState::Waiting
            {
                batch.state = QueueState::Dropped;
                batch.status = reason.into();
            }
        }
        self.prune_finished();
    }

    pub fn drop_waiting_node(&mut self, node: &str, reason: &str) {
        for batch in &mut self.batches {
            if batch.remote_node.as_deref() == Some(node) && batch.state == QueueState::Waiting {
                batch.state = QueueState::Dropped;
                batch.status = reason.into();
            }
        }
        self.prune_finished();
    }

    pub fn begin_due(&mut self, now: Instant) -> Option<PendingBatch> {
        let batch = self
            .batches
            .iter_mut()
            .find(|batch| matches!(batch.state, QueueState::Waiting | QueueState::Sending))?;
        if batch.state != QueueState::Waiting || now < batch.retry_at {
            return None;
        }
        batch.state = QueueState::Sending;
        batch.status = "sending".into();
        Some(batch.clone())
    }

    pub fn finish(&mut self, seq: u64, outcome: SendOutcome, now: Instant) -> Option<QueueEvent> {
        let event = {
            let batch = self
                .batches
                .iter_mut()
                .find(|batch| batch.seq == seq && batch.state == QueueState::Sending)?;
            match outcome {
                SendOutcome::Ack { duplicate } => {
                    batch.state = QueueState::Sent;
                    batch.status = "sent".into();
                    Some(QueueEvent::Sent { seq, duplicate })
                }
                SendOutcome::Uncertain => {
                    mark_uncertain(batch, "delivery uncertain".into());
                    Some(QueueEvent::Uncertain {
                        seq,
                        reason: batch.status.clone(),
                    })
                }
                SendOutcome::WrongInstance => {
                    batch.state = QueueState::Dropped;
                    batch.status = "session restarted; input dropped".into();
                    Some(QueueEvent::Dropped {
                        seq,
                        reason: batch.status.clone(),
                    })
                }
                SendOutcome::RateLimited => {
                    if batch.rate_retries < MAX_RATE_LIMIT_RETRIES {
                        batch.rate_retries += 1;
                        schedule_retry(batch, now, "rate limited; retrying")
                    } else {
                        let reason = format!(
                            "rate limited after {MAX_RATE_LIMIT_RETRIES} retries; input failed"
                        );
                        batch.state = QueueState::Failed;
                        batch.status = reason.clone();
                        Some(QueueEvent::Failed { seq, reason })
                    }
                }
                SendOutcome::RemoteControlDisabled => {
                    batch.state = QueueState::Failed;
                    batch.status = "remote control disabled".into();
                    Some(QueueEvent::RemoteControlDisabled {
                        seq,
                        node: batch.remote_node.clone().unwrap_or_default(),
                    })
                }
                SendOutcome::IoFailure(detail) if batch.io_retries < MAX_IO_RETRIES => {
                    batch.io_retries += 1;
                    schedule_retry(batch, now, &format!("connection lost; retrying: {detail}"))
                }
                SendOutcome::IoFailure(detail) => {
                    mark_uncertain(batch, format!("delivery uncertain after retries: {detail}"));
                    Some(QueueEvent::Uncertain {
                        seq,
                        reason: batch.status.clone(),
                    })
                }
                SendOutcome::Error(reason) => {
                    batch.state = QueueState::Failed;
                    batch.status = reason.clone();
                    Some(QueueEvent::Failed { seq, reason })
                }
            }
        };
        self.prune_finished();
        event
    }

    fn prune_finished(&mut self) {
        let mut batches: Vec<_> = self.batches.drain(..).collect();
        let mut finished = 0;
        let mut retained = VecDeque::with_capacity(batches.len());
        for batch in batches.drain(..).rev() {
            let terminal = matches!(
                batch.state,
                QueueState::Sent | QueueState::Uncertain | QueueState::Dropped | QueueState::Failed
            );
            if terminal {
                finished += 1;
                if finished > 3 {
                    continue;
                }
            }
            retained.push_back(batch);
        }
        retained.make_contiguous().reverse();
        self.batches = retained;
    }
}

fn schedule_retry(batch: &mut PendingBatch, now: Instant, status: &str) -> Option<QueueEvent> {
    let retry_count = batch.io_retries as u32 + u32::from(batch.rate_retries);
    batch.retry_at = now + retry_delay(retry_count);
    batch.state = QueueState::Waiting;
    batch.status = status.into();
    Some(QueueEvent::RetryScheduled {
        seq: batch.seq,
        retry_at: batch.retry_at,
    })
}

fn retry_delay(retry_count: u32) -> Duration {
    let exponent = retry_count.saturating_sub(1).min(5);
    Duration::from_millis(100_u64.saturating_mul(1_u64 << exponent))
}

fn mark_uncertain(batch: &mut PendingBatch, reason: String) {
    batch.state = QueueState::Uncertain;
    batch.status = reason;
}

#[cfg(test)]
mod tests {
    use super::{InputQueue, QueueState, SendOutcome, MAX_QUEUED_BATCHES, MAX_RATE_LIMIT_RETRIES};
    use std::time::{Duration, Instant};

    #[test]
    fn queue_state_transitions_wait_send_ack() {
        let now = Instant::now();
        let mut queue = InputQueue::default();
        queue.enqueue(
            "dev".into(),
            "instance-a".into(),
            "client".into(),
            1,
            b"hi\r".to_vec(),
            now,
        );
        assert_eq!(queue.items().next().unwrap().state, QueueState::Waiting);
        assert!(queue.begin_due(now).is_some());
        assert_eq!(queue.items().next().unwrap().state, QueueState::Sending);
        queue.finish(1, SendOutcome::Ack { duplicate: false }, now);
        assert_eq!(queue.items().next().unwrap().state, QueueState::Sent);
    }

    #[test]
    fn rate_limit_backoff_keeps_batch_waiting_until_due() {
        let now = Instant::now();
        let mut queue = InputQueue::default();
        queue.enqueue(
            "dev".into(),
            "instance-a".into(),
            "client".into(),
            1,
            b"hi\r".to_vec(),
            now,
        );
        queue.begin_due(now).unwrap();
        let retry = queue.finish(1, SendOutcome::RateLimited, now).unwrap();
        assert!(matches!(retry, super::QueueEvent::RetryScheduled { .. }));
        assert!(queue.begin_due(now + Duration::from_millis(1)).is_none());
        assert!(queue.begin_due(now + Duration::from_secs(1)).is_some());
    }

    #[test]
    fn rate_limited_batches_fail_after_the_retry_budget() {
        let mut queue = InputQueue::default();
        let mut now = Instant::now();
        queue.enqueue(
            "dev".into(),
            "instance-a".into(),
            "client".into(),
            1,
            b"hi\r".to_vec(),
            now,
        );

        for retry in 0..=MAX_RATE_LIMIT_RETRIES {
            assert!(queue.begin_due(now).is_some());
            let event = queue.finish(1, SendOutcome::RateLimited, now).unwrap();
            if retry < MAX_RATE_LIMIT_RETRIES {
                let super::QueueEvent::RetryScheduled { retry_at, .. } = event else {
                    panic!("expected retry {retry} to be scheduled, got {event:?}");
                };
                now = retry_at;
            } else {
                assert!(matches!(event, super::QueueEvent::Failed { seq: 1, .. }));
            }
        }
        assert_eq!(queue.items().next().unwrap().state, QueueState::Failed);
    }

    #[test]
    fn queue_rejects_batches_after_the_global_limit() {
        let now = Instant::now();
        let mut queue = InputQueue::default();
        for seq in 1..=MAX_QUEUED_BATCHES + 1 {
            queue.enqueue(
                "dev".into(),
                "instance-a".into(),
                "client".into(),
                seq as u64,
                vec![seq as u8],
                now,
            );
        }
        assert_eq!(queue.items().count(), MAX_QUEUED_BATCHES);
    }

    #[test]
    fn queue_prunes_old_finished_batches_but_keeps_the_last_three() {
        let now = Instant::now();
        let mut queue = InputQueue::default();
        for seq in 1..=5 {
            queue.enqueue(
                "dev".into(),
                "instance-a".into(),
                "client".into(),
                seq,
                vec![seq as u8],
                now,
            );
            queue.begin_due(now).unwrap();
            queue.finish(seq, SendOutcome::Ack { duplicate: false }, now);
        }
        let items: Vec<_> = queue.items().map(|batch| batch.seq).collect();
        assert_eq!(items, [3, 4, 5]);
    }
}
