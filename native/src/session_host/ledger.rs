//! Stable client histories survive controller epochs, not host crashes.
use super::result::{Completion, Delivery, Reply};
use std::collections::VecDeque;

const MAX_CLIENTS: usize = 16;
const RESULTS_PER_CLIENT: usize = 256;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Rejected,
    Written,
    Indeterminate,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lookup {
    New,
    Pending,
    Busy,
    Complete(Outcome),
    Forgotten,
    Conflict,
    OutOfOrder,
    ClientLimit,
}
struct Entry {
    sequence: u64,
    digest: [u8; 32],
    outcome: Option<Completion>,
}
struct Client {
    id: [u8; 16],
    highest: u64,
    entries: VecDeque<Entry>,
}
#[derive(Default)]
pub(crate) struct Ledger {
    clients: Vec<Client>,
}
impl Ledger {
    /// Authenticated runtime supplies the SHA-256 of canonical, validated
    /// InputAct bytes, including operation kind. No epoch is part of identity.
    pub fn lookup(&self, id: [u8; 16], sequence: u64, digest: [u8; 32]) -> Lookup {
        if sequence == 0 {
            return Lookup::OutOfOrder;
        }
        let Some(client) = self.clients.iter().find(|client| client.id == id) else {
            return if self.clients.len() == MAX_CLIENTS {
                Lookup::ClientLimit
            } else if sequence == 1 {
                Lookup::New
            } else {
                Lookup::OutOfOrder
            };
        };
        if let Some(entry) = client
            .entries
            .iter()
            .find(|entry| entry.sequence == sequence)
        {
            return if entry.digest != digest {
                Lookup::Conflict
            } else {
                entry.outcome.as_ref().map_or(Lookup::Pending, |result| {
                    Lookup::Complete(match result.delivery {
                        Delivery::Rejected => Outcome::Rejected,
                        Delivery::Written => Outcome::Written,
                        Delivery::Indeterminate => Outcome::Indeterminate,
                    })
                })
            };
        }
        if sequence <= client.highest {
            Lookup::Forgotten
        } else if client.highest.checked_add(1) == Some(sequence) {
            Lookup::New
        } else {
            Lookup::OutOfOrder
        }
    }
    pub fn reserve(&mut self, id: [u8; 16], sequence: u64, digest: [u8; 32]) -> Lookup {
        let state = self.lookup(id, sequence, digest);
        if state != Lookup::New {
            return state;
        }
        if !self.clients.iter().any(|client| client.id == id) {
            self.clients.push(Client {
                id,
                highest: 0,
                entries: VecDeque::new(),
            });
        }
        let client = self
            .clients
            .iter_mut()
            .find(|client| client.id == id)
            .unwrap();
        // Pending work is never evicted. Coordinator admits at most one act.
        if client.entries.iter().any(|entry| entry.outcome.is_none()) {
            return Lookup::Busy;
        }
        if client.entries.len() == RESULTS_PER_CLIENT {
            client.entries.pop_front();
        }
        client.highest = sequence;
        client.entries.push_back(Entry {
            sequence,
            digest,
            outcome: None,
        });
        Lookup::New
    }
    pub fn complete(&mut self, id: [u8; 16], sequence: u64, outcome: Outcome) -> bool {
        self.complete_result(
            id,
            sequence,
            Completion {
                delivery: match outcome {
                    Outcome::Rejected => Delivery::Rejected,
                    Outcome::Written => Delivery::Written,
                    Outcome::Indeterminate => Delivery::Indeterminate,
                },
                reply: Reply::Unit,
            },
        )
    }
    pub fn result(&self, id: [u8; 16], sequence: u64) -> Option<&Completion> {
        self.clients
            .iter()
            .find(|client| client.id == id)?
            .entries
            .iter()
            .find(|entry| entry.sequence == sequence)?
            .outcome
            .as_ref()
    }
    pub fn complete_result(&mut self, id: [u8; 16], sequence: u64, outcome: Completion) -> bool {
        let entry = self
            .clients
            .iter_mut()
            .find(|client| client.id == id)
            .and_then(|client| {
                client
                    .entries
                    .iter_mut()
                    .find(|entry| entry.sequence == sequence)
            });
        let Some(entry) = entry else {
            return false;
        };
        if entry.outcome.is_some() {
            return false;
        }
        entry.outcome = Some(outcome.bounded());
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lost_ack_is_recovered_without_epoch_and_changed_payload_conflicts() {
        let mut ledger = Ledger::default();
        assert_eq!(ledger.reserve([1; 16], 1, [8; 32]), Lookup::New);
        assert_eq!(ledger.reserve([1; 16], 1, [8; 32]), Lookup::Pending);
        assert_eq!(ledger.lookup([1; 16], 1, [9; 32]), Lookup::Conflict);
        assert!(ledger.complete([1; 16], 1, Outcome::Written));
        assert_eq!(
            ledger.reserve([1; 16], 1, [8; 32]),
            Lookup::Complete(Outcome::Written)
        );
        assert!(!ledger.complete([1; 16], 1, Outcome::Rejected));
    }
    #[test]
    fn eviction_refuses_replay_and_clients_are_never_forgotten() {
        let mut ledger = Ledger::default();
        for seq in 1..=RESULTS_PER_CLIENT as u64 + 1 {
            assert_eq!(ledger.reserve([1; 16], seq, [0; 32]), Lookup::New);
            assert!(ledger.complete([1; 16], seq, Outcome::Indeterminate));
        }
        assert_eq!(ledger.lookup([1; 16], 1, [0; 32]), Lookup::Forgotten);
        for id in 2..=MAX_CLIENTS as u8 {
            assert_eq!(ledger.reserve([id; 16], 1, [0; 32]), Lookup::New);
            ledger.complete([id; 16], 1, Outcome::Rejected);
        }
        assert_eq!(ledger.reserve([99; 16], 1, [0; 32]), Lookup::ClientLimit);
        assert_eq!(ledger.lookup([1; 16], 1, [0; 32]), Lookup::Forgotten);
        assert_eq!(ledger.clients[0].entries.len(), RESULTS_PER_CLIENT);
    }
    #[test]
    fn sequences_cannot_skip_zero_wrap_or_evict_pending() {
        let mut ledger = Ledger::default();
        assert_eq!(ledger.reserve([0; 16], 0, [0; 32]), Lookup::OutOfOrder);
        assert_eq!(ledger.reserve([0; 16], 2, [0; 32]), Lookup::OutOfOrder);
        ledger.reserve([0; 16], 1, [0; 32]);
        assert_eq!(ledger.reserve([0; 16], 2, [0; 32]), Lookup::Busy);
        assert_eq!(ledger.clients[0].highest, 1);
        ledger.clients[0].highest = u64::MAX;
        assert_eq!(ledger.lookup([0; 16], 2, [0; 32]), Lookup::Forgotten);
    }
}
