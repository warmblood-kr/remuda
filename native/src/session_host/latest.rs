//! Coalesced latest-state watches; never an incremental byte-stream baseline.
use super::api::{OutputEnd, Response, ViewState};
use super::child::View;
use remuda_core::agent::OutputWakeup;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};
const MAX_WAIT: Duration = Duration::from_millis(750);
const REQUEST_GAP: Duration = Duration::from_millis(25);

pub(crate) struct Latest {
    wake: OutputWakeup,
    acknowledged: Option<u64>,
    next_request: Instant,
    #[cfg(test)]
    before_sleep: Option<Box<dyn FnOnce() + Send>>,
}
impl Latest {
    pub fn new(view: &View) -> Result<Self, String> {
        Ok(Self {
            wake: view.subscribe().map_err(|_| "watch_subscription_limit")?,
            acknowledged: None,
            next_request: Instant::now(),
            #[cfg(test)]
            before_sleep: None,
        })
    }
    pub fn acknowledge(&mut self, generation: u64, view: &View) -> Result<Response, String> {
        self.acknowledged = Some(generation);
        self.wake.version_after_wake();
        status(view)?.response(generation)
    }
    pub fn wait(
        &mut self,
        generation: u64,
        timeout_ms: u16,
        view: &View,
    ) -> Result<Response, String> {
        let now = Instant::now();
        let timeout = Duration::from_millis(u64::from(timeout_ms));
        if timeout > MAX_WAIT || self.acknowledged != Some(generation) {
            return Err("watch_requires_matching_snapshot_ack_and_bounded_wait".into());
        }
        if now < self.next_request {
            return Err("watch_request_cooldown".into());
        }
        self.next_request = now + REQUEST_GAP;
        let until = now + timeout;
        loop {
            let current = status(view)?;
            if current.changed(generation) || Instant::now() >= until {
                return current.response(generation);
            }
            let remaining = until.saturating_duration_since(Instant::now());
            #[cfg(test)]
            if let Some(before_sleep) = self.before_sleep.take() {
                before_sleep();
            }
            match self.wake.recv_timeout(remaining) {
                Ok(()) => {
                    self.wake.version_after_wake();
                }
                Err(RecvTimeoutError::Timeout) => return status(view)?.response(generation),
                Err(RecvTimeoutError::Disconnected) => {
                    let final_state = status(view)?;
                    if final_state.changed(generation) {
                        return final_state.response(generation);
                    }
                    return Err("watch_source_disconnected".into());
                }
            }
        }
    }
}
struct Status {
    generation: u64,
    end: OutputEnd,
    unsupported: u64,
    query_overflow: u64,
    fault: bool,
}
impl Status {
    fn changed(&self, after: u64) -> bool {
        self.generation != after
            || !matches!(self.end, OutputEnd::Open)
            || self.fault
            || self.unsupported != 0
            || self.query_overflow != 0
    }
    fn response(self, after: u64) -> Result<Response, String> {
        if after > self.generation {
            return Err("watch_future_generation".into());
        }
        let state = if self.fault || self.unsupported != 0 || self.query_overflow != 0 {
            ViewState::Degraded
        } else if !matches!(self.end, OutputEnd::Open) {
            ViewState::Ended
        } else if after < self.generation {
            ViewState::ResyncRequired
        } else {
            ViewState::Unchanged
        };
        Ok(Response::ViewState {
            generation: self.generation,
            resync_required: self.generation > after,
            state,
            output_end: self.end,
            unsupported_osc: self.unsupported,
            query_overflow: self.query_overflow,
        })
    }
}
fn status(view: &View) -> Result<Status, String> {
    let ((unsupported, query_overflow), eof, fault) =
        view.health().map_err(|_| "view_unavailable")?;
    // Observe EOF before generation: final output was stored before EOF Release.
    let end = if eof {
        view.output_end()
    } else {
        OutputEnd::Open
    };
    let generation = view.status().map_err(|_| "view_unavailable")?.0;
    Ok(Status {
        generation,
        end,
        unsupported,
        query_overflow,
        fault,
    })
}

#[cfg(test)]
#[path = "latest_tests.rs"]
mod tests;
