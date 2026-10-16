//! One serialized admission boundary for fencing and retained operation results.
use super::control::{ActId, ConnectionId, Controller, Fence, Refusal};
use super::ledger::{Ledger, Lookup};
use super::result::Completion;
use super::wire::InputAct;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    Started(ActId),
    Existing(Lookup),
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Control(Refusal),
    Invalid(&'static str),
    StaleCompletion,
}
struct Running {
    token: ActId,
    client: [u8; 16],
    sequence: u64,
    act: InputAct,
    started: bool,
}
pub(crate) struct Coordinator {
    controller: Controller,
    ledger: Ledger,
    running: Option<Running>,
    mutation: Option<ActId>,
}
impl Coordinator {
    pub fn new(incarnation: [u8; 16]) -> Self {
        Self {
            controller: Controller::new(incarnation),
            ledger: Ledger::default(),
            running: None,
            mutation: None,
        }
    }
    pub fn acquire(&mut self, connection: ConnectionId) -> Result<Fence, Error> {
        self.controller.acquire(connection).map_err(Error::Control)
    }
    pub fn release(&mut self, fence: Fence) -> Result<(), Error> {
        self.controller.release(fence).map_err(Error::Control)
    }
    pub fn begin_mutation(&mut self, fence: Fence) -> Result<ActId, Error> {
        let token = self.controller.begin_act(fence).map_err(Error::Control)?;
        self.mutation = Some(token);
        Ok(token)
    }
    pub fn finish_mutation(&mut self, token: ActId) -> Result<(), Error> {
        if self.mutation != Some(token) {
            return Err(Error::StaleCompletion);
        }
        self.controller.finish_act(token).map_err(Error::Control)?;
        self.mutation = None;
        Ok(())
    }
    /// Caller holds the coordinator lock only for admission, never across an
    /// OS write, pause or snapshot. Execution retains token until fully settled.
    pub fn admit(
        &mut self,
        fence: Fence,
        client: [u8; 16],
        sequence: u64,
        act: &InputAct,
    ) -> Result<Admission, Error> {
        let digest = act.digest().map_err(Error::Invalid)?;
        let token = self.controller.begin_act(fence).map_err(Error::Control)?;
        let state = self.ledger.reserve(client, sequence, digest);
        if state != Lookup::New {
            self.controller.finish_act(token).map_err(Error::Control)?;
            return Ok(Admission::Existing(state));
        }
        self.running = Some(Running {
            token,
            client,
            sequence,
            act: act.clone(),
            started: false,
        });
        Ok(Admission::Started(token))
    }
    pub fn reject_unstarted(&mut self, token: ActId) -> Result<(), Error> {
        let running = self
            .running
            .as_ref()
            .filter(|running| running.token == token && !running.started)
            .ok_or(Error::StaleCompletion)?;
        let completion = super::result::Completion {
            delivery: super::result::Delivery::Rejected,
            reply: super::result::Reply::Error(super::result::ErrorCode::Unavailable),
        };
        if !self
            .ledger
            .complete_result(running.client, running.sequence, completion)
        {
            return Err(Error::StaleCompletion);
        }
        self.controller.finish_act(token).map_err(Error::Control)?;
        self.running = None;
        Ok(())
    }
    /// Claim the exact admitted payload once before any effect. A delayed or
    /// duplicate worker cannot replay an old act or substitute another payload.
    pub fn claim(&mut self, token: ActId) -> Result<InputAct, Error> {
        let running = self
            .running
            .as_mut()
            .filter(|running| running.token == token && !running.started)
            .ok_or(Error::StaleCompletion)?;
        running.started = true;
        Ok(running.act.clone())
    }
    /// Finish only after late workers/follow-ups settle, even if an RPC has
    /// already returned a timeout or Late. Retain the rich result before grant.
    pub fn complete(&mut self, token: ActId, result: Completion) -> Result<(), Error> {
        let running = self
            .running
            .as_ref()
            .filter(|running| running.token == token && running.started)
            .ok_or(Error::StaleCompletion)?;
        if !self
            .ledger
            .complete_result(running.client, running.sequence, result)
        {
            return Err(Error::StaleCompletion);
        }
        self.controller.finish_act(token).map_err(Error::Control)?;
        self.running = None;
        Ok(())
    }
    /// Outcome recovery is an authenticated read, independent of controller
    /// epoch. It returns full retained reply and never initiates a replay.
    pub fn recover(
        &self,
        client: [u8; 16],
        sequence: u64,
        act: &InputAct,
    ) -> Result<(Lookup, Option<&Completion>), Error> {
        let state = self
            .ledger
            .lookup(client, sequence, act.digest().map_err(Error::Invalid)?);
        let result = if matches!(state, Lookup::Complete(_)) {
            self.ledger.result(client, sequence)
        } else {
            None
        };
        Ok((state, result))
    }
}
#[cfg(test)]
mod tests {
    use super::super::result::{Delivery, Reply, Submission};
    use super::*;
    fn admitted(value: Admission) -> ActId {
        match value {
            Admission::Started(token) => token,
            _ => panic!("expected admitted act"),
        }
    }
    #[test]
    fn full_act_result_survives_disconnect_and_lost_ack_without_replay() {
        let mut host = Coordinator::new([8; 16]);
        let first = host.acquire(ConnectionId(1)).unwrap();
        let act = InputAct::TypeText {
            text: "hello".into(),
            settle_ms: 100,
        };
        let token = admitted(host.admit(first, [1; 16], 1, &act).unwrap());
        host.release(first).unwrap();
        assert_eq!(
            host.acquire(ConnectionId(2)),
            Err(Error::Control(Refusal::Busy))
        );
        assert_eq!(host.recover([1; 16], 1, &act).unwrap().0, Lookup::Pending);
        let result = Completion {
            delivery: Delivery::Written,
            reply: Reply::Submission(Submission::Unverified),
        };
        host.claim(token).unwrap();
        assert!(host.claim(token).is_err());
        host.complete(token, result.clone()).unwrap();
        let replacement = host.acquire(ConnectionId(2)).unwrap();
        assert_eq!(host.recover([1; 16], 1, &act).unwrap().1, Some(&result));
        assert!(matches!(
            host.admit(replacement, [1; 16], 1, &act).unwrap(),
            Admission::Existing(Lookup::Complete(_))
        ));
        assert!(host.running.is_none());
        let next = admitted(host.admit(replacement, [1; 16], 2, &act).unwrap());
        assert_eq!(
            host.complete(token, result.clone()),
            Err(Error::StaleCompletion)
        );
        host.release(replacement).unwrap();
        assert_eq!(
            host.acquire(ConnectionId(3)),
            Err(Error::Control(Refusal::Busy))
        );
        host.claim(next).unwrap();
        host.complete(next, result).unwrap();
        assert!(host.acquire(ConnectionId(3)).is_ok());
    }
    #[test]
    fn failed_worker_rejects_only_unstarted_payload_and_mutation_tokens_cannot_cross() {
        let mut host = Coordinator::new([5; 16]);
        let owner = host.acquire(ConnectionId(1)).unwrap();
        let mutation = host.begin_mutation(owner).unwrap();
        let act = InputAct::Bytes(vec![1]);
        assert_eq!(
            host.admit(owner, [1; 16], 1, &act),
            Err(Error::Control(Refusal::Busy))
        );
        host.finish_mutation(mutation).unwrap();
        let token = admitted(host.admit(owner, [1; 16], 1, &act).unwrap());
        assert_eq!(host.finish_mutation(mutation), Err(Error::StaleCompletion));
        host.release(owner).unwrap();
        host.reject_unstarted(token).unwrap();
        assert_eq!(
            host.recover([1; 16], 1, &act).unwrap().1.unwrap().delivery,
            Delivery::Rejected
        );
        let replacement = host.acquire(ConnectionId(2)).unwrap();
        let token = admitted(host.admit(replacement, [1; 16], 2, &act).unwrap());
        host.claim(token).unwrap();
        assert_eq!(host.reject_unstarted(token), Err(Error::StaleCompletion));
        host.release(replacement).unwrap();
        assert_eq!(
            host.acquire(ConnectionId(3)),
            Err(Error::Control(Refusal::Busy))
        );
        host.complete(
            token,
            Completion {
                delivery: Delivery::Indeterminate,
                reply: Reply::Unit,
            },
        )
        .unwrap();
        assert!(host.acquire(ConnectionId(3)).is_ok());
    }
    #[test]
    fn rich_clear_result_and_payload_binding_survive_replacement() {
        let mut host = Coordinator::new([9; 16]);
        let owner = host.acquire(ConnectionId(1)).unwrap();
        let act = InputAct::Clear(vec![21]);
        let token = admitted(host.admit(owner, [4; 16], 1, &act).unwrap());
        let result = Completion {
            delivery: Delivery::Written,
            reply: Reply::Cleared(Some("draft".into())),
        };
        host.claim(token).unwrap();
        assert!(host.claim(token).is_err());
        host.complete(token, result.clone()).unwrap();
        assert_eq!(host.recover([4; 16], 1, &act).unwrap().1, Some(&result));
        let altered = InputAct::Bytes(vec![21]);
        assert_eq!(
            host.recover([4; 16], 1, &altered).unwrap(),
            (Lookup::Conflict, None)
        );
    }
}
