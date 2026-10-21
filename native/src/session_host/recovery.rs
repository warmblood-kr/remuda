//! Durable controller input bridge and launch/retire writes for one owner.
use super::api::{OutcomeState, Request, Response};
use super::entry::Client;
use super::inventory::disk::{Error, Store};
use super::inventory::model::{
    self, Fence, InputMutation, InputPhase, Key, Lookup, Mutation, Target,
};
use super::result::Completion;
use super::wire::InputAct;
use std::path::Path;
use std::time::Duration;

pub(crate) enum Progress {
    Pending,
    Complete(Completion),
    Forgotten,
    Conflict,
    OutOfOrder,
    /// Persisted admission and host absence disagree; never synthesize a retry.
    Indeterminate,
    /// The host answered but its lease is held, e.g. by the admitted act itself.
    Busy,
    /// Transport failure or a refused connection; retain identity.
    Unavailable,
}
/// Metadata namespace a daemon writes once it closed a session's child.
pub(crate) const CLOSING: &str = "closing";
/// One writer claim: a fresh nonce under the next epoch. A host publishing
/// between our read and commit is retried, not fatal.
fn new_claim(store: &Store) -> Result<Fence, Error> {
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce).map_err(|_| Error::Data(model::Error::Unavailable))?;
    let until = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match store.commit(
            store.read()?.cursor(),
            None,
            Mutation::Claim { nonce },
            Duration::from_secs(1),
        ) {
            Ok(applied) => return Ok(applied.fence),
            Err(Error::Busy | Error::Data(model::Error::StaleRevision))
                if std::time::Instant::now() < until =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}
pub(crate) struct Owner {
    store: Store,
    /// Replaced only by [`Owner::reclaim`].
    fence: std::sync::Mutex<Fence>,
}
impl Owner {
    /// A process-level owner, shared across sessions; not one new claim per act.
    pub fn claim(store: Store) -> Result<Self, Error> {
        let fence = new_claim(&store)?;
        Ok(Self {
            store,
            fence: std::sync::Mutex::new(fence),
        })
    }
    /// Claim again with a newer epoch, fencing whoever claimed since; for a
    /// daemon that takes its sessions back after a failed handover (#713).
    /// Work under the old fence then fails with `StaleFence`, never twice.
    pub fn reclaim(&self) -> Result<(), Error> {
        let fence = new_claim(&self.store)?;
        *self.fence.lock().unwrap_or_else(|e| e.into_inner()) = fence;
        Ok(())
    }
    fn fence(&self) -> Fence {
        *self.fence.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Durable publication precedes any host connection, effect or caller receipt.
    pub fn reserve(
        &self,
        target: Target,
        intent: [u8; 16],
        client: [u8; 16],
        sequence: u64,
        act: InputAct,
    ) -> Result<Key, Error> {
        let key = Key {
            client,
            sequence,
            payload: act
                .digest()
                .map_err(|reason| Error::Data(model::Error::Invalid(reason)))?,
        };
        let index = self.store.read()?;
        let existing = match index.lookup(&target, &key)? {
            Lookup::Pending(pending) => Some(pending),
            Lookup::Complete(result) => Some(&result.reservation),
            Lookup::Absent => None,
            _ => return Err(Error::Data(model::Error::Conflict)),
        };
        if let Some(existing) = existing {
            return if existing.intent == intent {
                Ok(key)
            } else {
                Err(Error::Data(model::Error::Conflict))
            };
        }
        self.commit(
            target,
            InputMutation::Reserve {
                intent,
                key: key.clone(),
                act,
            },
        )?;
        #[cfg(test)]
        tests::cut(1);
        Ok(key)
    }
    /// Caller may recover a key from disk without having received reserve's ACK.
    /// All RPCs occur outside the file lock; only the stored original act is sent.
    pub fn resume(&self, target: Target, key: Key, endpoint: &Path) -> Result<Progress, Error> {
        let index = self.store.read()?;
        let pending = match index.lookup(&target, &key)? {
            Lookup::Complete(result) => return Ok(Progress::Complete(result.completion.clone())),
            Lookup::Forgotten => return Ok(Progress::Forgotten),
            Lookup::Conflict => return Ok(Progress::Conflict),
            Lookup::OutOfOrder => return Ok(Progress::OutOfOrder),
            Lookup::Absent => return Ok(Progress::Indeterminate),
            Lookup::Pending(pending) => pending.clone(),
        };
        // Skip the disk commit only while this exact fence (store incarnation,
        // epoch, nonce) is current and already wrote the act; otherwise Adopt
        // runs and a newer claim fails with StaleFence before any RPC.
        let fence = self.fence();
        if pending.writer_epoch != fence.epoch() || index.fence() != fence {
            self.commit(copy_target(&target), InputMutation::Adopt(key.clone()))?;
        }
        let Ok(mut peer) = Client::open_recovering(endpoint, target.identity.clone()) else {
            return Ok(Progress::Unavailable);
        };
        // Prior connections and the entire admitted act must drain first. This
        // physical lease is independent of the durable controller writer fence.
        match peer.acquire(self.writer()) {
            Ok(Response::Granted { .. }) => {}
            // A live host still running an admitted act (or another holder).
            Ok(Response::Error { code }) if code == "controller_busy" => return Ok(Progress::Busy),
            // A newer daemon reached this host first; this owner may not write.
            Ok(Response::Error { code }) if code == "superseded_writer" => {
                return Err(Error::Data(model::Error::StaleFence))
            }
            _ => return Ok(Progress::Unavailable),
        }
        let request = Request::Recover {
            client: key.client,
            sequence: key.sequence,
            act: pending.act.clone(),
        };
        let response = match peer.request(&request) {
            Ok(response) => response,
            Err(_) => return Ok(Progress::Unavailable),
        };
        self.reconcile(target, key, pending, peer, response)
    }
    pub fn index(&self) -> Result<model::Index, Error> {
        self.store.read()
    }
    /// What a fencing host is shown; it refuses every older writer after this.
    pub fn writer(&self) -> super::api::Writer {
        self.fence().writer()
    }
    /// Whether this owner's exact fence is still the store's current writer.
    pub fn current(&self) -> Result<bool, Error> {
        Ok(self.store.read()?.fence() == self.fence())
    }
    /// Durable launch reservation under this owner's fence, before any spawn.
    pub fn reserve_launch(&self, name: String, entry: model::Entry) -> Result<(), Error> {
        self.store.commit(
            self.store.read()?.cursor(),
            Some(self.fence()),
            Mutation::ReserveLaunch { name, entry },
            Duration::from_secs(1),
        )?;
        Ok(())
    }
    /// Durable close intent: discovery tears such an entry down instead of
    /// registering it again. Idempotent; contention is retried briefly.
    pub fn mark_closing(&self, target: Target) -> Result<(), Error> {
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let index = self.store.read()?;
            if index
                .entries
                .get(&target.name)
                .is_some_and(|entry| entry.metadata.contains_key(CLOSING))
            {
                return Ok(());
            }
            let update = model::MetadataUpdate {
                namespace: CLOSING.into(),
                expected: None,
                value: model::Namespace::new(1, "true").map_err(Error::Data)?,
            };
            match self.store.commit(
                index.cursor(),
                Some(self.fence()),
                Mutation::Metadata {
                    target: copy_target(&target),
                    update,
                },
                Duration::from_secs(1),
            ) {
                Ok(_) => return Ok(()),
                Err(Error::Busy | Error::Data(model::Error::StaleRevision))
                    if std::time::Instant::now() < until =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }
    /// Remove an exited, settled entry; the host must already be stopped.
    pub fn retire(&self, target: Target) -> Result<(), Error> {
        self.store.commit(
            self.store.read()?.cursor(),
            Some(self.fence()),
            Mutation::Exit {
                target,
                operation: model::ExitMutation::Retire,
            },
            Duration::from_secs(1),
        )?;
        Ok(())
    }
    /// Publish that the caller received this exact result; then it may be GC'd.
    pub fn consume(
        &self,
        target: Target,
        key: Key,
        intent: [u8; 16],
        completion: &Completion,
    ) -> Result<(), Error> {
        let index = self.store.read()?;
        let expected = index
            .entries
            .get(&target.name)
            .and_then(|entry| entry.metadata.get("consumer"))
            .map(|namespace| namespace.revision);
        let encoded = serde_json::to_vec(completion)
            .map_err(|_| Error::Data(model::Error::Invalid("completion encoding")))?;
        let receipt = serde_json::json!({"sequence": key.sequence}).to_string();
        let checkpoint = model::ConsumerCheckpoint {
            intent,
            outcome: model::hash(&encoded),
            metadata: model::MetadataUpdate {
                namespace: "consumer".into(),
                expected,
                value: model::Namespace::new(1, &receipt).map_err(Error::Data)?,
            },
        };
        self.commit(target, InputMutation::Consume { key, checkpoint })
    }
    fn reconcile(
        &self,
        target: Target,
        key: Key,
        pending: model::Pending,
        mut peer: Client,
        response: Response,
    ) -> Result<Progress, Error> {
        match response {
            Response::Outcome {
                state: OutcomeState::Complete,
                completion: Some(completion),
            } => {
                let completion = completion.bounded();
                self.commit(
                    target,
                    InputMutation::Complete {
                        key,
                        completion: completion.clone(),
                    },
                )?;
                Ok(Progress::Complete(completion))
            }
            Response::Outcome {
                state: OutcomeState::New,
                completion: None,
            } if pending.phase != InputPhase::Admitted => {
                // The authenticated same-incarnation host ledger says never
                // admitted, while this connection holds its exclusive lease.
                // Never allocate a new client/sequence or rebuild the payload.
                let response = peer.request(&Request::Input {
                    client: key.client,
                    sequence: key.sequence,
                    act: pending.act,
                });
                #[cfg(test)]
                if matches!(response, Ok(Response::Admitted)) {
                    tests::cut(2);
                    tests::cut(3);
                }
                let phase = if matches!(response, Ok(Response::Admitted)) {
                    InputPhase::Admitted
                } else {
                    InputPhase::Indeterminate
                };
                self.commit(target, InputMutation::Observe { key, phase })?;
                Ok(Progress::Pending)
            }
            Response::Outcome {
                state: OutcomeState::Pending,
                completion: None,
            } => Ok(Progress::Pending),
            Response::Outcome {
                state: OutcomeState::Forgotten,
                completion: None,
            } => Ok(Progress::Forgotten),
            Response::Outcome {
                state: OutcomeState::Conflict,
                completion: None,
            } => Ok(Progress::Conflict),
            Response::Outcome {
                state: OutcomeState::OutOfOrder,
                completion: None,
            } => Ok(Progress::OutOfOrder),
            _ => Ok(Progress::Indeterminate),
        }
    }
    fn commit(&self, target: Target, operation: InputMutation) -> Result<(), Error> {
        self.store.commit(
            self.store.read()?.cursor(),
            Some(self.fence()),
            Mutation::Input { target, operation },
            Duration::from_secs(1),
        )?;
        Ok(())
    }
}
fn copy_target(target: &Target) -> Target {
    Target {
        name: target.name.clone(),
        identity: target.identity.clone(),
    }
}
#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
