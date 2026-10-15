//! Durable controller input bridge; no daemon/Registry adoption yet.
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
    /// Includes physical lease contention and transport failure; retain identity.
    Unavailable,
}
pub(crate) struct Owner {
    store: Store,
    fence: Fence,
}
impl Owner {
    /// A process-level owner, shared across sessions; not one new claim per act.
    pub fn claim(store: Store) -> Result<Self, Error> {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(|_| Error::Data(model::Error::Unavailable))?;
        let applied = store.commit(
            store.read()?.cursor(),
            None,
            Mutation::Claim { nonce },
            Duration::from_secs(1),
        )?;
        Ok(Self {
            store,
            fence: applied.fence,
        })
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
        self.commit(copy_target(&target), InputMutation::Adopt(key.clone()))?;
        let Ok(mut peer) = Client::open_recovering(endpoint, target.identity.clone()) else {
            return Ok(Progress::Unavailable);
        };
        // Prior connections and the entire admitted act must drain first. This
        // physical lease is independent of the durable controller writer fence.
        if !matches!(
            peer.request(&Request::Acquire),
            Ok(Response::Granted { .. })
        ) {
            return Ok(Progress::Unavailable);
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
            Some(self.fence),
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
