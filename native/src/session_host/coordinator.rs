//! One serialized admission boundary for fencing and retained operation results.
use super::control::{ActId, ConnectionId, Controller, Fence, Refusal};
use super::human::{Cut, GrantError, GrantRequest, GrantState, Human, Raw, Retire};
use super::ledger::{Ledger, Lookup};
use super::result::{Completion, Delivery};
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
    Stopping,
    Retained,
    Grant(GrantError),
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
    stopped: bool,
    human: Human,
    physical: Option<u64>,
}
impl Coordinator {
    pub fn new(incarnation: [u8; 16]) -> Self {
        Self {
            controller: Controller::new(incarnation),
            ledger: Ledger::default(),
            running: None,
            mutation: None,
            stopped: false,
            human: Human::new(incarnation),
            physical: None,
        }
    }
    pub fn acquire(&mut self, connection: ConnectionId) -> Result<Fence, Error> {
        self.admitting()?;
        self.controller.acquire(connection).map_err(Error::Control)
    }
    pub fn release(&mut self, fence: Fence) -> Result<(), Error> {
        self.controller.release(fence).map_err(Error::Control)
    }
    pub fn begin_mutation(&mut self, fence: Fence) -> Result<ActId, Error> {
        self.admitting()?;
        self.physical_idle()?;
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
    fn admitting(&self) -> Result<(), Error> {
        if self.stopped {
            return Err(Error::Stopping);
        }
        Ok(())
    }
    /// Final admission: only the current idle controller may latch; after it,
    /// acquire, mutation and input refuse forever. Outcome reads stay open.
    pub fn latch_stop(&mut self, fence: Fence) -> Result<(), Error> {
        let token = self.begin_mutation(fence)?;
        self.finish_mutation(token)?;
        if !self.human.settled() {
            return Err(Error::Retained);
        }
        self.stopped = true;
        Ok(())
    }
    /// One physical barrier: a human raw ticket excludes every controller act.
    fn physical_idle(&self) -> Result<(), Error> {
        if self.physical.is_some() {
            return Err(Error::Control(Refusal::Busy));
        }
        Ok(())
    }
    /// Checked CAS against the host grant high-water; displaces any holder.
    pub fn grant(
        &mut self,
        connection: ConnectionId,
        request: GrantRequest,
    ) -> Result<(u64, GrantState), Error> {
        self.admitting()?;
        self.human.grant(connection, request).map_err(Error::Grant)
    }
    pub fn disconnect_human(&mut self, connection: ConnectionId) {
        self.human.disconnect(connection);
    }
    /// Management revoke of one exact grant; an in-flight ticket still drains.
    pub fn revoke(&mut self, fence: Fence, grant: u64) -> Result<bool, Error> {
        let token = self.begin_mutation(fence)?;
        let revoked = self.human.revoke(grant);
        self.finish_mutation(token)?;
        Ok(revoked)
    }
    /// Reserve one raw outcome and the physical ticket together, or neither.
    pub fn admit_raw(
        &mut self,
        connection: ConnectionId,
        grant: u64,
        sequence: u64,
        digest: [u8; 32],
    ) -> Result<Raw, Error> {
        let retained = self.human.recover(grant, sequence, digest);
        if retained != Raw::OutOfOrder {
            return Ok(retained);
        }
        self.admitting()?;
        if !self.human.holds(connection, grant) {
            return Ok(Raw::Stale);
        }
        if self.physical.is_some() || self.controller.active() {
            return Ok(Raw::Busy);
        }
        let raw = self.human.reserve(grant, sequence, digest);
        if raw == Raw::New {
            self.physical = Some(sequence);
        }
        Ok(raw)
    }
    /// Finish the exact ticket after all late writes; never by current owner.
    pub fn complete_raw(&mut self, sequence: u64, delivery: Delivery) -> Result<(), Error> {
        if self.physical != Some(sequence) || !self.human.complete(sequence, delivery) {
            return Err(Error::StaleCompletion);
        }
        self.physical = None;
        Ok(())
    }
    pub fn recover_raw(&self, grant: u64, sequence: u64, digest: [u8; 32]) -> Raw {
        self.human.recover(grant, sequence, digest)
    }
    pub fn raw_cut(&self, last: u64) -> Result<Cut, Retire> {
        self.human.cut(last)
    }
    pub fn ack_raw(&mut self, connection: ConnectionId, cut: Cut) -> Retire {
        self.human.ack(connection, cut)
    }
    pub fn discard_raw(&mut self, fence: Fence, cut: Cut) -> Result<Retire, Error> {
        let token = self.begin_mutation(fence)?;
        let retired = self.human.discard(cut);
        self.finish_mutation(token)?;
        Ok(retired)
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
        self.admitting()?;
        self.physical_idle()?;
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
    fn stop_latch_waits_for_active_act_then_refuses_all_new_admission() {
        let mut host = Coordinator::new([6; 16]);
        let owner = host.acquire(ConnectionId(1)).unwrap();
        let act = InputAct::Bytes(vec![1]);
        let token = admitted(host.admit(owner, [1; 16], 1, &act).unwrap());
        assert_eq!(host.latch_stop(owner), Err(Error::Control(Refusal::Busy)));
        host.claim(token).unwrap();
        let result = Completion {
            delivery: Delivery::Written,
            reply: Reply::Unit,
        };
        host.complete(token, result.clone()).unwrap();
        let stale = Fence { epoch: 0, ..owner };
        assert_eq!(host.latch_stop(stale), Err(Error::Control(Refusal::Stale)));
        host.latch_stop(owner).unwrap();
        assert_eq!(host.latch_stop(owner), Err(Error::Stopping));
        assert_eq!(host.admit(owner, [1; 16], 2, &act), Err(Error::Stopping));
        assert_eq!(host.begin_mutation(owner), Err(Error::Stopping));
        host.release(owner).unwrap();
        assert_eq!(host.acquire(ConnectionId(2)), Err(Error::Stopping));
        assert_eq!(host.recover([1; 16], 1, &act).unwrap().1, Some(&result));
    }
    fn grant_request(id: u8, expected: u64) -> GrantRequest {
        GrantRequest {
            request: [id; 16],
            expected,
            recovery: [id; 32],
        }
    }
    #[test]
    fn idle_controller_and_human_share_one_physical_barrier() {
        let mut host = Coordinator::new([3; 16]);
        let owner = host.acquire(ConnectionId(1)).unwrap();
        let human = ConnectionId(9);
        host.grant(human, grant_request(1, 0)).unwrap();
        assert_eq!(host.admit_raw(human, 1, 1, [1; 32]), Ok(Raw::New));
        assert_eq!(
            host.begin_mutation(owner),
            Err(Error::Control(Refusal::Busy))
        );
        let act = InputAct::Bytes(vec![1]);
        assert_eq!(
            host.admit(owner, [1; 16], 1, &act),
            Err(Error::Control(Refusal::Busy))
        );
        // Takeover during the write: old ticket drains, never a second writer.
        host.grant(ConnectionId(10), grant_request(2, 1)).unwrap();
        assert_eq!(
            host.admit_raw(ConnectionId(10), 2, 2, [2; 32]),
            Ok(Raw::Busy)
        );
        assert_eq!(
            host.complete_raw(2, Delivery::Written),
            Err(Error::StaleCompletion)
        );
        host.complete_raw(1, Delivery::Written).unwrap();
        let token = host.begin_mutation(owner).unwrap();
        assert_eq!(
            host.admit_raw(ConnectionId(10), 2, 2, [2; 32]),
            Ok(Raw::Busy)
        );
        host.finish_mutation(token).unwrap();
        assert_eq!(
            host.admit_raw(ConnectionId(10), 2, 2, [2; 32]),
            Ok(Raw::New)
        );
        assert_eq!(
            host.admit_raw(human, 1, 1, [1; 32]),
            Ok(Raw::Complete(Delivery::Written))
        );
    }
    #[test]
    fn stop_latch_refuses_live_grant_and_unretired_raw_until_explicit_retirement() {
        let mut host = Coordinator::new([4; 16]);
        let owner = host.acquire(ConnectionId(1)).unwrap();
        let human = ConnectionId(9);
        host.grant(human, grant_request(1, 0)).unwrap();
        host.admit_raw(human, 1, 1, [1; 32]).unwrap();
        assert_eq!(host.latch_stop(owner), Err(Error::Control(Refusal::Busy)));
        host.complete_raw(1, Delivery::Written).unwrap();
        assert_eq!(host.latch_stop(owner), Err(Error::Retained));
        assert!(host.revoke(owner, 1).unwrap());
        assert!(!host.revoke(owner, 1).unwrap());
        assert_eq!(host.latch_stop(owner), Err(Error::Retained));
        let cut = host.raw_cut(1).unwrap();
        assert_eq!(host.discard_raw(owner, cut), Ok(Retire::Retired));
        host.latch_stop(owner).unwrap();
        assert_eq!(host.grant(human, grant_request(2, 1)), Err(Error::Stopping));
        assert_eq!(host.admit_raw(human, 1, 2, [2; 32]), Err(Error::Stopping));
        assert_eq!(host.recover_raw(1, 1, [1; 32]), Raw::Forgotten);
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
