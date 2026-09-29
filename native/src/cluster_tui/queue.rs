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
    pub name: String,
    pub instance_id: String,
    pub client_id: String,
    pub seq: u64,
    pub bytes: Vec<u8>,
    pub state: QueueState,
    pub status: String,
    retry_at: Instant,
    io_retries: u8,
    rate_retries: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Ack { duplicate: bool },
    Uncertain,
    WrongInstance,
    RateLimited,
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
}

pub const MAX_IO_RETRIES: u8 = 3;

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
    ) {
        self.batches.push_back(PendingBatch {
            name,
            instance_id,
            client_id,
            seq,
            bytes,
            state: QueueState::Waiting,
            status: "waiting to send".into(),
            retry_at: now,
            io_retries: 0,
            rate_retries: 0,
        });
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
        name: &str,
        instance_id: &str,
        client_id: &str,
    ) -> u64 {
        let mut next_seq = 1;
        for batch in &mut self.batches {
            if batch.name == name
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
                    batch.rate_retries = batch.rate_retries.saturating_add(1);
                    schedule_retry(batch, now, "rate limited; retrying")
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
    let retry_count = batch.io_retries as u32 + batch.rate_retries;
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
    use super::{InputQueue, QueueState, SendOutcome};
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
