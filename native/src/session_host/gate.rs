//! Admission for a daemon's host-backed work (#713, design in #758). Every
//! act, control call and host start holds a [`Pass`]. A drain closes the gate,
//! so new work is refused as `Recovering`, then waits for passes still out.
//! Only a reload that rolls back opens it again.
//! The next daemon resolves any act left pending by its key, so a drain that
//! times out loses nothing; it only makes the handover slower.
use remuda_core::session_operations::{Error, Unavailable};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct Gate {
    state: Mutex<State>,
    idle: Condvar,
}
#[derive(Default)]
struct State {
    draining: bool,
    active: usize,
}

/// Held for one whole piece of host-backed work.
pub(crate) struct Pass(Arc<Gate>);

/// What a drain left behind.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Drained {
    /// Passes still out at the deadline.
    pub left: usize,
}

impl Gate {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// A pass, or `Recovering` once a drain has begun.
    pub fn enter(self: &Arc<Self>) -> Result<Pass, Error> {
        let mut state = self.state();
        if state.draining {
            return Err(Error::Unavailable(Unavailable::Recovering));
        }
        state.active += 1;
        Ok(Pass(Arc::clone(self)))
    }
    /// Close the gate, then wait up to `deadline` for passes out.
    pub fn drain(&self, deadline: Duration) -> Drained {
        let until = Instant::now() + deadline;
        let mut state = self.state();
        state.draining = true;
        while state.active > 0 {
            let now = Instant::now();
            if now >= until {
                break;
            }
            state = self
                .idle
                .wait_timeout(state, until - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Drained { left: state.active }
    }
    /// Admit work again: a reload that rolled back keeps this daemon.
    pub fn reopen(&self) {
        self.state().draining = false;
    }
}

impl Drop for Pass {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.active -= 1;
        if state.active == 0 {
            self.0.idle.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drain_waits_for_passes_out_and_refuses_new_ones() {
        let gate = Arc::new(Gate::default());
        let pass = gate.enter().unwrap();
        let drainer = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || gate.drain(Duration::from_secs(5)))
        };
        // The drain has begun once a new pass is refused.
        while gate.enter().is_ok() {
            std::thread::yield_now();
        }
        assert!(matches!(
            gate.enter(),
            Err(Error::Unavailable(Unavailable::Recovering))
        ));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!drainer.is_finished(), "the drain returned with a pass out");
        drop(pass);
        assert_eq!(drainer.join().unwrap(), Drained { left: 0 });
    }

    #[test]
    fn a_drain_gives_up_at_its_deadline_and_reports_what_is_left() {
        let gate = Arc::new(Gate::default());
        let _stuck = gate.enter().unwrap();
        let started = Instant::now();
        assert_eq!(gate.drain(Duration::from_millis(100)), Drained { left: 1 });
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_reopened_gate_admits_again() {
        let gate = Arc::new(Gate::default());
        assert_eq!(gate.drain(Duration::from_millis(1)).left, 0);
        assert!(gate.enter().is_err());
        gate.reopen();
        drop(gate.enter().unwrap());
    }

    #[test]
    fn an_idle_gate_drains_at_once() {
        let gate = Arc::new(Gate::default());
        drop(gate.enter().unwrap());
        let started = Instant::now();
        assert_eq!(gate.drain(Duration::from_secs(5)), Drained { left: 0 });
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
