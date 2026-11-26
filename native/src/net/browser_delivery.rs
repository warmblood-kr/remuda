//! Live delivery state for one user's active Browser.

pub use remuda_core::protocol::{BrowserDocumentOrigin, BrowserDocumentOutcome as Outcome};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const SINK_TTL: Duration = Duration::from_secs(5);
pub const DELIVERY_TTL: Duration = Duration::from_secs(60);
const MAX_RESULTS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct DocumentRequest {
    pub id: String,
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<BrowserDocumentOrigin>,
}

struct Sink {
    instance: String,
    last_poll: Instant,
}

struct Pending {
    request: DocumentRequest,
    deadline: Instant,
    dispatched: bool,
}

#[derive(Default)]
pub(crate) struct Delivery {
    sink: Option<Sink>,
    pending: Option<Pending>,
    results: VecDeque<(DocumentRequest, Outcome)>,
}

impl Delivery {
    pub(crate) fn publish(&mut self, request: DocumentRequest, now: Instant) -> Outcome {
        self.expire(now);
        if let Some((prior, outcome)) = self
            .results
            .iter()
            .find(|(prior, _)| prior.id == request.id)
        {
            return if *prior == request {
                *outcome
            } else {
                Outcome::ConflictingRequest
            };
        }
        if let Some(pending) = &self.pending {
            return if pending.request == request {
                Outcome::Pending
            } else if pending.request.id == request.id {
                Outcome::ConflictingRequest
            } else {
                Outcome::Busy
            };
        }
        if self.sink.is_none() {
            return Outcome::NoActiveBrowser;
        }
        self.pending = Some(Pending {
            request,
            deadline: now + DELIVERY_TTL,
            dispatched: false,
        });
        Outcome::Pending
    }

    pub(crate) fn poll(
        &mut self,
        instance: &str,
        now: Instant,
    ) -> Result<Option<DocumentRequest>, Outcome> {
        self.expire(now);
        if let Some(sink) = &self.sink {
            if sink.instance != instance {
                return Err(Outcome::Busy);
            }
        }
        self.sink = Some(Sink {
            instance: instance.to_owned(),
            last_poll: now,
        });
        Ok(self.pending.as_mut().map(|pending| {
            pending.dispatched = true;
            pending.request.clone()
        }))
    }

    pub(crate) fn finish(
        &mut self,
        instance: &str,
        request: &DocumentRequest,
        outcome: Outcome,
        now: Instant,
    ) -> bool {
        self.expire(now);
        if !matches!(outcome, Outcome::Displayed | Outcome::ReadFailed)
            || !self
                .sink
                .as_ref()
                .is_some_and(|sink| sink.instance == instance)
            || !self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.dispatched && pending.request == *request)
        {
            return false;
        }
        self.complete(outcome);
        true
    }

    fn expire(&mut self, now: Instant) {
        if self
            .sink
            .as_ref()
            .is_some_and(|sink| now.duration_since(sink.last_poll) >= SINK_TTL)
        {
            self.sink = None;
        }
        if let Some(pending) = &self.pending {
            if self.sink.is_none() || now >= pending.deadline {
                let outcome = if pending.dispatched {
                    Outcome::Unknown
                } else {
                    Outcome::NoActiveBrowser
                };
                self.complete(outcome);
            }
        }
    }

    fn complete(&mut self, outcome: Outcome) {
        if let Some(pending) = self.pending.take() {
            if self.results.len() == MAX_RESULTS {
                self.results.pop_front();
            }
            self.results.push_back((pending.request, outcome));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> DocumentRequest {
        DocumentRequest {
            id: "first".into(),
            uri: "remuda:///pilot/reading.md".into(),
            origin: None,
        }
    }

    #[test]
    fn result_requires_dispatch_and_the_active_browser() {
        let now = Instant::now();
        let request = request();
        let mut delivery = Delivery::default();
        assert_eq!(
            delivery.publish(request.clone(), now),
            Outcome::NoActiveBrowser
        );
        assert_eq!(delivery.poll("browser", now), Ok(None));
        assert_eq!(delivery.publish(request.clone(), now), Outcome::Pending);
        assert!(!delivery.finish("browser", &request, Outcome::Displayed, now));
        assert_eq!(delivery.poll("second", now), Err(Outcome::Busy));
        assert_eq!(delivery.poll("browser", now), Ok(Some(request.clone())));
        assert!(!delivery.finish("second", &request, Outcome::Displayed, now));
        assert!(delivery.finish("browser", &request, Outcome::Displayed, now));
        assert_eq!(delivery.publish(request.clone(), now), Outcome::Displayed);
        assert_eq!(delivery.poll("browser", now), Ok(None));
        let other = DocumentRequest {
            uri: "remuda:///other.md".into(),
            ..request
        };
        assert_eq!(delivery.publish(other, now), Outcome::ConflictingRequest);
    }

    #[test]
    fn expired_sink_after_dispatch_leaves_an_unknown_result() {
        let now = Instant::now();
        let request = request();
        let mut delivery = Delivery::default();
        assert_eq!(delivery.poll("browser", now), Ok(None));
        assert_eq!(delivery.publish(request.clone(), now), Outcome::Pending);
        assert_eq!(delivery.poll("browser", now), Ok(Some(request.clone())));
        let expired = now + SINK_TTL;
        assert_eq!(delivery.publish(request.clone(), expired), Outcome::Unknown);
        assert_eq!(delivery.poll("second", expired), Ok(None));
        assert!(!delivery.finish("browser", &request, Outcome::Displayed, expired));
    }
}
