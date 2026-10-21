//! A human attached to a host-backed session through the daemon (#713).
//! One Human-role connection holds one checked grant. Each write is one raw
//! sequence that the daemon waits to settle and then retires, so the host's
//! outcome window never fills and a later Stop is never held up by it.
//! A host that predates the human counters in Inspect cannot be granted;
//! the daemon's attach then stays read-only.
use super::api::{HumanCounters, Identity as HostIdentity, Request, Response};
use super::entry::Client;
use super::human::Raw;
use super::result::Delivery;
use remuda_core::agent::{AgentError, OutputWakeup};
use remuda_core::session_operations::{AttachmentOperations, Error, Identity, Result, Unavailable};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long one write may take to settle before it is reported uncertain.
const SETTLE_DEADLINE: Duration = Duration::from_secs(30);
const SETTLE_POLL: Duration = Duration::from_millis(1);
/// Grant races (another viewer granted between Inspect and grant) retried.
const GRANT_ATTEMPTS: usize = 3;
/// How long a host may answer Busy for one sequence before the write is refused.
/// Bounded because Busy retries hold the client mutex and the drain gate.
const BUSY_DEADLINE: Duration = Duration::from_secs(5);

pub(crate) struct HostAttachment {
    identity: Identity,
    grant: u64,
    client: Mutex<Client>,
    last_raw: Mutex<u64>,
    displaced: AtomicBool,
    gate: Arc<super::gate::Gate>,
}

impl HostAttachment {
    pub fn open(
        endpoint: &Path,
        host: HostIdentity,
        identity: Identity,
        gate: Arc<super::gate::Gate>,
    ) -> Result<Self> {
        let _pass = gate.enter()?;
        let mut client = Client::open_human(endpoint, host)
            .map_err(|_| Error::Unavailable(Unavailable::Unsupported))?;
        for _ in 0..GRANT_ATTEMPTS {
            let counters = counters(&mut client)?;
            let response = client
                .request(&Request::HumanGrant {
                    request: random()?,
                    expected: counters.high_water,
                    recovery_secret: random()?,
                })
                .map_err(transport)?;
            match response {
                Response::HumanGranted { grant, .. } => {
                    return Ok(Self {
                        identity,
                        grant,
                        client: Mutex::new(client),
                        last_raw: Mutex::new(counters.last_raw),
                        displaced: AtomicBool::new(false),
                        gate: Arc::clone(&gate),
                    });
                }
                Response::Error { code } if code == "human_grant_stale" => {}
                Response::Error { .. } => return Err(Error::Protocol("host refused attach")),
                _ => return Err(Error::Protocol("unexpected host grant reply")),
            }
        }
        Err(Error::Agent(AgentError::Busy))
    }

    fn client(&self) -> std::sync::MutexGuard<'_, Client> {
        self.client.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write one chunk as the next raw sequence, wait for it to settle, then
    /// retire it.
    fn write_chunk(
        &self,
        client: &mut Client,
        bytes: &[u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        let mut last_raw = self.last_raw.lock().unwrap_or_else(|e| e.into_inner());
        let mut sequence = *last_raw + 1;
        let mut resynced = false;
        let budget = BusyBudget::new(Instant::now());
        let state = loop {
            let state = raw(client.request(&Request::HumanInput {
                grant: self.grant,
                sequence,
                bytes: bytes.to_vec(),
            }))?;
            match state {
                // Another grant's write moved the sequence; it is ours to follow once.
                Raw::OutOfOrder if !resynced => {
                    resynced = true;
                    sequence = counters(client)?.last_raw + 1;
                }
                Raw::Busy => budget.retry(Instant::now(), cancelled())?,
                state => break state,
            }
        };
        let delivery = match state {
            Raw::New | Raw::Pending => self.settle(client, sequence, bytes)?,
            Raw::Complete(delivery) => delivery,
            Raw::Stale => {
                self.displaced.store(true, Ordering::SeqCst);
                return Err(Error::Agent(AgentError::Attached));
            }
            Raw::Forgotten | Raw::Conflict | Raw::OutOfOrder | Raw::Busy => {
                return Err(Error::Protocol("host refused the input sequence"));
            }
        };
        *last_raw = sequence;
        retire(client, sequence);
        match delivery {
            Delivery::Written => Ok(()),
            Delivery::Rejected => Err(Error::Agent(AgentError::Io(
                "host rejected the input before writing it".into(),
            ))),
            Delivery::Indeterminate => Err(Error::Agent(AgentError::Io(
                "input may have been written partially".into(),
            ))),
        }
    }

    fn settle(&self, client: &mut Client, sequence: u64, bytes: &[u8]) -> Result<Delivery> {
        let until = Instant::now() + SETTLE_DEADLINE;
        loop {
            match raw(client.request(&Request::RecoverRaw {
                grant: self.grant,
                sequence,
                bytes: bytes.to_vec(),
            }))? {
                Raw::Complete(delivery) => return Ok(delivery),
                Raw::Pending if Instant::now() < until => std::thread::sleep(SETTLE_POLL),
                Raw::Pending => {
                    return Err(Error::Agent(AgentError::Io(
                        "input did not settle in time".into(),
                    )))
                }
                _ => return Err(Error::Protocol("host lost the input outcome")),
            }
        }
    }
}

impl AttachmentOperations for HostAttachment {
    fn identity(&self) -> Identity {
        self.identity.clone()
    }
    fn generation(&self) -> u64 {
        self.grant
    }
    /// A newer grant took over, seen by a refused write or by Inspect.
    fn displaced(&self) -> Result<bool> {
        if self.displaced.load(Ordering::SeqCst) {
            return Ok(true);
        }
        let taken = counters(&mut self.client())?.high_water > self.grant;
        if taken {
            self.displaced.store(true, Ordering::SeqCst);
        }
        Ok(taken)
    }
    fn writer_timed_out(&self) -> Result<bool> {
        Ok(false)
    }
    fn write_raw(
        &self,
        bytes: &[u8],
        cancelled: &dyn Fn() -> bool,
        on_start: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<()> {
        let _pass = self.gate.enter()?;
        let mut client = self.client();
        for chunk in bytes.chunks(super::wire::MAX_INPUT) {
            if cancelled() {
                break;
            }
            on_start();
            self.write_chunk(&mut client, chunk, cancelled)?;
        }
        Ok(())
    }
    /// The daemon paints host attaches from typed viewports instead.
    fn screen_bytes(&self) -> Result<Vec<u8>> {
        Err(Error::Unavailable(Unavailable::Unsupported))
    }
    fn wakeup(&self) -> Result<Option<OutputWakeup>> {
        Ok(None)
    }
}

/// Best effort: a cut that cannot retire now is retired by a later one.
fn retire(client: &mut Client, last: u64) {
    if let Ok(Response::RawCut { cut }) = client.request(&Request::RawCut { last }) {
        let _ = client.request(&Request::AcknowledgeRaw { cut });
    }
}

fn counters(client: &mut Client) -> Result<HumanCounters> {
    match client.request(&Request::Inspect).map_err(transport)? {
        Response::Info { info } => info
            .human
            .ok_or(Error::Unavailable(Unavailable::Unsupported)),
        Response::Error { .. } => Err(Error::Unavailable(Unavailable::Recovering)),
        _ => Err(Error::Protocol("unexpected host inspect reply")),
    }
}

fn raw(response: std::io::Result<Response>) -> Result<Raw> {
    match response.map_err(transport)? {
        Response::RawOutcome { state } => Ok(state),
        Response::Error { code } if code == "host_stopping" => {
            Err(Error::Unavailable(Unavailable::Recovering))
        }
        Response::Error { .. } => Err(Error::Protocol("host refused the input")),
        _ => Err(Error::Protocol("unexpected host input reply")),
    }
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|_| Error::Unavailable(Unavailable::Transport))?;
    Ok(bytes)
}

fn transport(_: std::io::Error) -> Error {
    Error::Unavailable(Unavailable::Transport)
}

/// One sequence's Busy retry budget. Waiting past `until` would hold the
/// client mutex and the drain gate forever, so the write is refused instead.
struct BusyBudget {
    until: Instant,
}

impl BusyBudget {
    fn new(now: Instant) -> Self {
        Self {
            until: now + BUSY_DEADLINE,
        }
    }

    /// One more short wait, or the refusal that ends the retry loop.
    fn retry(&self, now: Instant, cancelled: bool) -> Result<()> {
        if cancelled {
            return Err(Error::Agent(AgentError::Attached));
        }
        if now >= self.until {
            return Err(Error::Agent(AgentError::Busy));
        }
        std::thread::sleep(SETTLE_POLL);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{BusyBudget, BUSY_DEADLINE};
    use remuda_core::agent::AgentError;
    use remuda_core::session_operations::Error;
    use std::time::{Duration, Instant};

    #[test]
    fn a_busy_retry_is_bounded_and_stops_when_cancelled() {
        let now = Instant::now();
        let budget = BusyBudget {
            until: now + Duration::from_secs(1),
        };
        assert!(budget.retry(now, false).is_ok(), "a fresh budget waits");
        assert!(
            matches!(
                budget.retry(now + Duration::from_secs(2), false),
                Err(Error::Agent(AgentError::Busy))
            ),
            "a host still busy after the deadline is refused"
        );
        assert!(
            matches!(
                budget.retry(now, true),
                Err(Error::Agent(AgentError::Attached))
            ),
            "cancellation ends the wait immediately"
        );
        assert!(
            BusyBudget::new(now).until <= now + BUSY_DEADLINE,
            "the default budget is bounded"
        );
    }
}
