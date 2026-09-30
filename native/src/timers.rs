//! Timers owned by the Lua image event loop.
//!
//! The queue stores deadlines only; callbacks stay in the Lua registry and
//! are taken out by the image thread after its current job has returned.

use mlua::{Function, Lua, RegistryKey, UserData, UserDataMethods};
use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub(crate) const MAX_LIVE_TIMERS: usize = 1_024;
pub(crate) const MIN_DELAY: Duration = Duration::from_millis(10);
pub(crate) const MAX_DELAY: Duration = Duration::from_secs(86_400);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Deadline {
    at: Instant,
    id: u64,
}

struct Timer {
    deadline: Instant,
    interval: Option<Duration>,
    callback: RegistryKey,
    owner: Option<String>,
}

pub(crate) struct TimerFire {
    pub(crate) id: u64,
    pub(crate) callback: Function,
    pub(crate) repeating: bool,
}

pub(crate) struct TimerService {
    origin: Instant,
    next_id: u64,
    deadlines: BinaryHeap<Reverse<Deadline>>,
    timers: HashMap<u64, Timer>,
}

impl TimerService {
    pub(crate) fn new() -> Self {
        Self {
            origin: Instant::now(),
            next_id: 1,
            deadlines: BinaryHeap::new(),
            timers: HashMap::new(),
        }
    }

    pub(crate) fn clock_ms(&self) -> u64 {
        self.origin.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }

    pub(crate) fn wait_timeout(&mut self) -> Option<Duration> {
        self.discard_stale_deadlines();
        self.deadlines
            .peek()
            .map(|Reverse(deadline)| deadline.at.saturating_duration_since(Instant::now()))
    }

    pub(crate) fn schedule(
        &mut self,
        lua: &Lua,
        seconds: f64,
        callback: Function,
        owner: Option<String>,
        repeating: bool,
    ) -> mlua::Result<u64> {
        let name = if repeating { "every" } else { "after" };
        if !seconds.is_finite()
            || seconds < MIN_DELAY.as_secs_f64()
            || seconds > MAX_DELAY.as_secs_f64()
        {
            return Err(mlua::Error::runtime(format!(
                "remuda.{name} seconds must be between 0.01 and 86400"
            )));
        }
        if self.timers.len() >= MAX_LIVE_TIMERS {
            return Err(mlua::Error::runtime(format!(
                "remuda.{name} limit reached: at most {MAX_LIVE_TIMERS} live timers"
            )));
        }
        let interval = Duration::from_secs_f64(seconds);
        let deadline = Instant::now() + interval;
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| mlua::Error::runtime("remuda timer handle space exhausted"))?;
        let callback = lua.create_registry_value(callback)?;
        self.timers.insert(
            id,
            Timer {
                deadline,
                interval: repeating.then_some(interval),
                callback,
                owner,
            },
        );
        self.deadlines.push(Reverse(Deadline { at: deadline, id }));
        Ok(id)
    }

    pub(crate) fn cancel(&mut self, lua: &Lua, id: u64) {
        if let Some(timer) = self.timers.remove(&id) {
            let _ = lua.remove_registry_value(timer.callback);
            self.compact_deadlines();
        }
    }

    pub(crate) fn cancel_owner(&mut self, lua: &Lua, owner: &str) {
        let ids: Vec<u64> = self
            .timers
            .iter()
            .filter_map(|(id, timer)| (timer.owner.as_deref() == Some(owner)).then_some(*id))
            .collect();
        for id in ids {
            if let Some(timer) = self.timers.remove(&id) {
                let _ = lua.remove_registry_value(timer.callback);
            }
        }
        self.compact_deadlines();
    }

    pub(crate) fn take_due(&mut self, lua: &Lua, now: Instant) -> mlua::Result<Option<TimerFire>> {
        self.discard_stale_deadlines();
        let Some(Reverse(deadline)) = self.deadlines.peek().copied() else {
            return Ok(None);
        };
        if deadline.at > now {
            return Ok(None);
        }
        self.deadlines.pop();
        let Some(timer) = self.timers.get(&deadline.id) else {
            return Ok(None);
        };
        let callback = lua.registry_value(&timer.callback)?;
        let repeating = timer.interval.is_some();
        if !repeating {
            let timer = self
                .timers
                .remove(&deadline.id)
                .expect("timer just checked");
            lua.remove_registry_value(timer.callback)?;
        }
        Ok(Some(TimerFire {
            id: deadline.id,
            callback,
            repeating,
        }))
    }

    pub(crate) fn finish_fire(&mut self, id: u64, now: Instant) {
        let Some(timer) = self.timers.get_mut(&id) else {
            return;
        };
        let Some(interval) = timer.interval else {
            return;
        };
        // Advance from the prior deadline. If execution fell behind, move to
        // the first future deadline on that same cadence and skip missed ticks.
        let mut next = timer.deadline + interval;
        while next <= now {
            next += interval;
        }
        timer.deadline = next;
        self.deadlines.push(Reverse(Deadline { at: next, id }));
    }

    fn discard_stale_deadlines(&mut self) {
        while let Some(Reverse(deadline)) = self.deadlines.peek() {
            let current = self
                .timers
                .get(&deadline.id)
                .is_some_and(|timer| timer.deadline == deadline.at);
            if current {
                break;
            }
            self.deadlines.pop();
        }
    }

    fn compact_deadlines(&mut self) {
        if self.deadlines.len() > self.timers.len() {
            self.deadlines = self
                .timers
                .iter()
                .map(|(id, timer)| {
                    Reverse(Deadline {
                        at: timer.deadline,
                        id: *id,
                    })
                })
                .collect();
        }
    }
}

pub(crate) type SharedTimerService = Rc<RefCell<TimerService>>;

pub(crate) struct TimerHandle {
    id: u64,
    service: SharedTimerService,
}

impl TimerHandle {
    pub(crate) fn new(id: u64, service: SharedTimerService) -> Self {
        Self { id, service }
    }
}

impl UserData for TimerHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancel", |lua, this, ()| {
            this.service.borrow_mut().cancel(lua, this.id);
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_interval_fire_waits_a_full_interval_before_firing_again() {
        let lua = Lua::new();
        let mut timers = TimerService::new();
        let callback = lua.create_function(|_, ()| Ok(())).unwrap();
        let interval = Duration::from_millis(20);
        let id = timers
            .schedule(&lua, interval.as_secs_f64(), callback, None, true)
            .unwrap();
        let first_deadline = timers.timers.get(&id).unwrap().deadline;
        let late_fire = first_deadline + Duration::from_millis(12);

        let fire = timers.take_due(&lua, late_fire).unwrap().unwrap();
        assert_eq!(fire.id, id);
        timers.finish_fire(id, late_fire);

        assert!(
            timers.timers.get(&id).unwrap().deadline >= late_fire + interval,
            "a late interval fire must not be followed by a burst"
        );
    }
}
