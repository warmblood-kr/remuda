//! Pausing the accept loop for a reload handover (#713). While paused, the
//! loop leaves the listener out of its poll, so clients wait in the backlog
//! for whichever daemon accepts next. A pause returns only once the loop has
//! seen it, so no accept is in progress or still to come. [`Answering`]
//! counts accepted requests, so a daemon handing over waits for their
//! answers before it exits.
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) struct AcceptGate {
    state: Mutex<State>,
    seen: Condvar,
    wake_tx: UnixStream,
    wake_rx: UnixStream,
}
#[derive(Default)]
struct State {
    paused: bool,
    asked: u64,
    seen: u64,
    listener: RawFd,
}

impl AcceptGate {
    pub(crate) fn new() -> io::Result<Self> {
        let (wake_tx, wake_rx) = UnixStream::pair()?;
        wake_rx.set_nonblocking(true)?;
        wake_tx.set_nonblocking(true)?;
        Ok(Self {
            state: Mutex::default(),
            seen: Condvar::new(),
            wake_tx,
            wake_rx,
        })
    }
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// The loop polls this with the listener; readable means look again.
    pub(crate) fn wake_fd(&self) -> RawFd {
        self.wake_rx.as_raw_fd()
    }
    /// The loop calls this at the top of every turn, with the listener it
    /// will poll. False while paused: leave the listener out.
    pub(crate) fn turn(&self, listener: RawFd) -> bool {
        let mut bytes = [0u8; 64];
        while matches!((&self.wake_rx).read(&mut bytes), Ok(n) if n > 0) {}
        let mut state = self.state();
        state.listener = listener;
        state.seen = state.asked;
        self.seen.notify_all();
        !state.paused
    }
    /// Stop accepting. Returns the listener once the loop has seen the
    /// pause, or an error if it did not within `deadline`.
    pub(crate) fn pause(&self, deadline: Duration) -> io::Result<RawFd> {
        let until = Instant::now() + deadline;
        let mut state = self.state();
        state.paused = true;
        state.asked += 1;
        let asked = state.asked;
        self.wake();
        while state.seen < asked {
            let now = Instant::now();
            if now >= until {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the accept loop did not pause",
                ));
            }
            state = self
                .seen
                .wait_timeout(state, until - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Ok(state.listener)
    }
    /// Accept again.
    pub(crate) fn resume(&self) {
        let mut state = self.state();
        state.paused = false;
        state.asked += 1;
        self.wake();
    }
    fn wake(&self) {
        // A full pair already holds a wake; the loop drains it next turn.
        let _ = (&self.wake_tx).write(&[1]);
    }
}

static ANSWERING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Held from accept until a request is answered.
pub(crate) struct Answering(());
impl Answering {
    pub(crate) fn begin() -> Self {
        ANSWERING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(())
    }
    /// Wait up to `deadline` until at most `own` requests (the caller's)
    /// are still being answered; answers how many others were left.
    pub(crate) fn wait_for_others(own: usize, deadline: Duration) -> usize {
        let until = Instant::now() + deadline;
        loop {
            let left = ANSWERING
                .load(std::sync::atomic::Ordering::SeqCst)
                .saturating_sub(own);
            if left == 0 || Instant::now() >= until {
                return left;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Answering {
    fn drop(&mut self) {
        ANSWERING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A stand-in accept loop: counts the turns it would accept on.
    fn run(gate: Arc<AcceptGate>, accepted: Arc<AtomicUsize>, stop: Arc<AtomicUsize>) {
        while stop.load(Ordering::SeqCst) == 0 {
            if gate.turn(7) {
                accepted.fetch_add(1, Ordering::SeqCst);
            }
            let mut poll = [libc::pollfd {
                fd: gate.wake_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            // SAFETY: one live pollfd.
            unsafe { libc::poll(poll.as_mut_ptr(), 1, 5) };
        }
    }

    #[test]
    fn a_pause_returns_once_the_loop_stops_accepting_and_resume_restarts_it() {
        let gate = Arc::new(AcceptGate::new().unwrap());
        let accepted = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicUsize::new(0));
        let looping = {
            let (gate, accepted, stop) = (gate.clone(), accepted.clone(), stop.clone());
            std::thread::spawn(move || run(gate, accepted, stop))
        };
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(gate.pause(Duration::from_secs(5)).unwrap(), 7);
        let paused_at = accepted.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            paused_at,
            "accepted while paused"
        );
        gate.resume();
        std::thread::sleep(Duration::from_millis(50));
        assert!(accepted.load(Ordering::SeqCst) > paused_at, "never resumed");
        stop.store(1, Ordering::SeqCst);
        looping.join().unwrap();
    }

    #[test]
    fn a_pause_with_no_loop_running_times_out() {
        let gate = AcceptGate::new().unwrap();
        let error = gate.pause(Duration::from_millis(20)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
