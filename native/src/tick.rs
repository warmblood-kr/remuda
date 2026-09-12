//! The periodic clock a registered Lua schedule fires on.
//!
//! 「N초마다 깨워준다」는 remuda가 알아도 되지만 「그때 컴팩션을 돌려라」는
//! remuda가 알면 안 된다 (see
//! `steps/031-native-keeps-the-clock-lua-keeps-the-calendar.md`). This type
//! knows only its own period and the current time; every registrant's own
//! interval and every registrant's own callback live in Lua (`tools.lua`'s
//! `remuda.schedule`/`remuda._run_due_schedules`), not here.
//!
//! A registered schedule is in-memory only and does not survive a daemon
//! restart — the same ceiling `steps/014-a-tool-registry.md:292-295` names
//! for the tool registry this extends; not solved here, on purpose.

use remuda_core::Clock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type JobReceiver = Receiver<Result<String, String>>;

/// The skip counters, shared between the `Ticker` that writes them and the
/// Lua binding that reads them — constructed before either exists, since the
/// image's bindings are built before `daemon::serve` has a `Ticker` to ask.
#[derive(Default)]
pub struct SkipCounters {
    consecutive: AtomicU64,
    total: AtomicU64,
}

impl SkipCounters {
    pub fn consecutive(&self) -> u64 {
        self.consecutive.load(Ordering::SeqCst)
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::SeqCst)
    }

    fn record_skip(&self) {
        self.total.fetch_add(1, Ordering::SeqCst);
        self.consecutive.fetch_add(1, Ordering::SeqCst);
    }

    fn record_reset(&self) {
        self.consecutive.store(0, Ordering::SeqCst);
    }
}

/// Fires `submit` once per `period`, never more than one call in flight —
/// a hung callback must skip a tick, not queue a second job behind the one
/// still running in the image's single unbounded, strictly FIFO queue.
pub struct Ticker {
    submit: Box<dyn Fn() -> Result<JobReceiver, String> + Send + Sync>,
    clock: Arc<dyn Clock>,
    period: Duration,
    in_flight: Mutex<Option<JobReceiver>>,
    counters: Arc<SkipCounters>,
}

impl Ticker {
    pub fn new(
        submit: impl Fn() -> Result<JobReceiver, String> + Send + Sync + 'static,
        clock: Arc<dyn Clock>,
        period: Duration,
        counters: Arc<SkipCounters>,
    ) -> Self {
        Self {
            submit: Box::new(submit),
            clock,
            period,
            in_flight: Mutex::new(None),
            counters,
        }
    }

    /// Run forever: sleep one period, then `step`. The production loop —
    /// `daemon.rs` spawns a thread that just calls this in a `loop`.
    pub fn run_forever(&self) -> ! {
        loop {
            self.clock.sleep(self.period);
            self.step();
        }
    }

    /// One period's worth of work, with no sleep — the unit a test drives
    /// directly, any number of times, without waiting on real time.
    pub fn step(&self) {
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
        let ready = match in_flight.as_ref() {
            None => true,
            Some(rx) => match rx.try_recv() {
                Ok(_answer) => true,
                Err(TryRecvError::Empty) => false,
                // The image is gone, or the job's sender was dropped without
                // answering — either way nothing is still running, so the
                // next period is free to submit again rather than skip
                // forever on a call that will never resolve.
                Err(TryRecvError::Disconnected) => true,
            },
        };

        if ready {
            self.counters.record_reset();
            *in_flight = (self.submit)().ok();
        } else {
            // Escalation hook-point: one skip is normal (a callback ran a
            // little long). A long CONSECUTIVE run means the image is wedged,
            // not slow — that is the distinction worth surfacing once
            // something acts on it. What threshold and what action are
            // undecided; this counter is where that decision attaches later.
            self.counters.record_skip();
        }
    }

    pub fn consecutive_skips(&self) -> u64 {
        self.counters.consecutive()
    }

    pub fn total_skips(&self) -> u64 {
        self.counters.total()
    }
}

#[cfg(test)]
mod tests {
    use super::Ticker;

    #[test]
    fn a_still_running_callback_is_skipped_not_queued_behind() {
        let clock = std::sync::Arc::new(remuda_core::ManualClock::new());
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Result<String, String>>();
        // First call hands back `reply_rx` (never answered during this test);
        // any further call would prove the ticker piled a second job on top of
        // an unfinished one, which is exactly the failure this type exists to
        // prevent.
        let reply_rx = std::sync::Mutex::new(Some(reply_rx));
        let ticker = Ticker::new(
            move || {
                reply_rx.lock().unwrap().take().ok_or_else(|| {
                    "ticker submitted twice while one was still in flight".to_string()
                })
            },
            clock,
            std::time::Duration::from_millis(1),
            std::sync::Arc::new(super::SkipCounters::default()),
        );
        ticker.step(); // submits the one job
        ticker.step(); // still not answered — must skip, not resubmit
        ticker.step();
        assert_eq!(ticker.consecutive_skips(), 2);
        assert_eq!(ticker.total_skips(), 2);

        reply_tx.send(Ok("done".into())).unwrap();
        ticker.step(); // sees the answer, resets, submits again
        assert_eq!(ticker.consecutive_skips(), 0);
    }
}
