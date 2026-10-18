//! Connection-bound epochs; a started act includes pauses and late writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnectionId(pub u128);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fence {
    pub incarnation: [u8; 16],
    pub connection: ConnectionId,
    pub epoch: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ActId {
    fence: Fence,
    sequence: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    Busy,
    Stale,
    Exhausted,
}
#[derive(Default)]
pub(crate) struct Controller {
    incarnation: [u8; 16],
    epoch: u64,
    owner: Option<Fence>,
    active_act: Option<ActId>,
    next_act: u64,
    draining: bool,
}
impl Controller {
    pub fn new(incarnation: [u8; 16]) -> Self {
        Self {
            incarnation,
            ..Self::default()
        }
    }
    /// Connection IDs come exclusively from the future authenticated runtime.
    pub fn acquire(&mut self, connection: ConnectionId) -> Result<Fence, Refusal> {
        if self.owner.is_some() || self.active_act.is_some() {
            return Err(Refusal::Busy);
        }
        self.epoch = self.epoch.checked_add(1).ok_or(Refusal::Exhausted)?;
        let fence = Fence {
            incarnation: self.incarnation,
            connection,
            epoch: self.epoch,
        };
        self.owner = Some(fence);
        Ok(fence)
    }
    fn authorize(&self, fence: Fence) -> Result<(), Refusal> {
        if self.owner != Some(fence) || self.draining {
            return Err(Refusal::Stale);
        }
        Ok(())
    }
    /// Reserve every effect (input, resize, close) atomically; authorization
    /// alone is private so callers cannot split a check from effect admission.
    pub fn begin_act(&mut self, fence: Fence) -> Result<ActId, Refusal> {
        self.authorize(fence)?;
        if self.active_act.is_some() {
            return Err(Refusal::Busy);
        }
        self.next_act = self.next_act.checked_add(1).ok_or(Refusal::Exhausted)?;
        let act = ActId {
            fence,
            sequence: self.next_act,
        };
        self.active_act = Some(act);
        Ok(act)
    }
    pub fn active(&self) -> bool {
        self.active_act.is_some()
    }
    pub fn release(&mut self, fence: Fence) -> Result<(), Refusal> {
        self.authorize(fence)?;
        self.draining = self.active_act.is_some();
        if self.active_act.is_none() {
            self.owner = None;
        }
        Ok(())
    }
    /// Runtime calls only after the entire act, including timeout-running
    /// worker/follow-up, has resolved. A request timeout is NOT completion.
    pub fn finish_act(&mut self, act: ActId) -> Result<(), Refusal> {
        if self.owner != Some(act.fence) || self.active_act != Some(act) {
            return Err(Refusal::Stale);
        }
        self.active_act = None;
        if self.draining {
            self.owner = None;
            self.draining = false;
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disconnect_drains_whole_act_before_new_writer() {
        let mut control = Controller::new([7; 16]);
        let old = control.acquire(ConnectionId(1)).unwrap();
        assert_eq!(
            control.authorize(Fence {
                incarnation: [8; 16],
                ..old
            }),
            Err(Refusal::Stale)
        );
        let old_act = control.begin_act(old).unwrap();
        control.release(old).unwrap();
        assert_eq!(control.authorize(old), Err(Refusal::Stale));
        assert_eq!(control.acquire(ConnectionId(2)), Err(Refusal::Busy));
        control.finish_act(old_act).unwrap();
        let new = control.acquire(ConnectionId(2)).unwrap();
        assert!(new.epoch > old.epoch);
        assert_eq!(control.begin_act(old), Err(Refusal::Stale));
        assert_eq!(
            control.authorize(Fence {
                connection: ConnectionId(1),
                ..new
            }),
            Err(Refusal::Stale)
        );
        control.begin_act(new).unwrap();
    }
    #[test]
    fn delayed_completion_cannot_finish_a_later_act() {
        let mut control = Controller::default();
        let owner = control.acquire(ConnectionId(1)).unwrap();
        let first = control.begin_act(owner).unwrap();
        control.finish_act(first).unwrap();
        let second = control.begin_act(owner).unwrap();
        assert_eq!(control.finish_act(first), Err(Refusal::Stale));
        control.release(owner).unwrap();
        assert_eq!(control.acquire(ConnectionId(2)), Err(Refusal::Busy));
        control.finish_act(second).unwrap();
        assert!(control.acquire(ConnectionId(2)).is_ok());
    }
    #[test]
    fn epoch_never_wraps_or_allows_two_acts() {
        let mut control = Controller::default();
        let owner = control.acquire(ConnectionId(1)).unwrap();
        let act = control.begin_act(owner).unwrap();
        assert_eq!(control.begin_act(owner), Err(Refusal::Busy));
        control.finish_act(act).unwrap();
        control.release(owner).unwrap();
        control.epoch = u64::MAX;
        assert_eq!(control.acquire(ConnectionId(2)), Err(Refusal::Exhausted));
    }
}
