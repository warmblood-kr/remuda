//! Expire snapshot storage independently of a client blocked in socket I/O.
use super::view_budget::Lease;
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;
pub(crate) struct Frames(Mutex<Vec<Weak<Frame>>>);
pub(crate) struct Frame {
    until: Instant,
    data: Mutex<Option<(Vec<u8>, Lease)>>,
}
impl Frames {
    pub fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }
    pub fn track(&self, bytes: Vec<u8>, lease: Lease, until: Instant) -> Option<Arc<Frame>> {
        let mut frames = self.0.lock().unwrap_or_else(|error| error.into_inner());
        frames.retain(|frame| frame.strong_count() != 0);
        if frames.len() >= 16 {
            drop(bytes);
            drop(lease);
            return None;
        }
        let frame = Arc::new(Frame {
            until,
            data: Mutex::new(Some((bytes, lease))),
        });
        frames.push(Arc::downgrade(&frame));
        Some(frame)
    }
    pub fn expire(&self, now: Instant) {
        let mut frames = self.0.lock().unwrap_or_else(|error| error.into_inner());
        frames.retain(|frame| {
            let Some(frame) = frame.upgrade() else {
                return false;
            };
            if now >= frame.until {
                frame.clear();
                false
            } else {
                true
            }
        });
    }
}
impl Frame {
    pub fn clear(&self) {
        // Tuple drop releases bytes before the lease allows another allocation.
        self.data
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
    pub fn page(&self, index: usize, now: Instant) -> Option<String> {
        use base64::Engine;
        if now >= self.until {
            self.clear();
            return None;
        }
        let data = self.data.lock().unwrap_or_else(|error| error.into_inner());
        let (bytes, _) = data.as_ref()?;
        let start = index
            .checked_mul(super::api::PAGE_BYTES)
            .filter(|start| *start < bytes.len())?;
        let end = bytes.len().min(start + super::api::PAGE_BYTES);
        Some(base64::engine::general_purpose::STANDARD.encode(&bytes[start..end]))
    }
    pub fn available(&self, now: Instant) -> bool {
        now < self.until
            && self
                .data
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_host::{control::ConnectionId, view_budget::Budget};
    use std::time::Duration;
    #[test]
    fn maintenance_releases_memory_and_permit_while_connection_retains_frame() {
        let frames = Frames::new();
        let budget = Budget::new();
        let now = Instant::now();
        let lease = budget.acquire(ConnectionId(1), now).unwrap();
        let until = now + Duration::from_secs(2);
        let frame = frames.track(vec![1; 1024], lease, until).unwrap();
        assert!(frame.page(0, now).is_some());
        assert!(budget.acquire(ConnectionId(2), now).is_none());
        frames.expire(until);
        assert!(!frame.available(until));
        assert!(frame.page(0, until).is_none());
        assert!(frame.data.lock().unwrap().is_none());
        assert!(budget
            .acquire(ConnectionId(2), until + Duration::from_millis(51))
            .is_some());
    }
    #[test]
    fn repeated_page_reads_never_extend_expiry_or_change_frozen_bytes() {
        let frames = Frames::new();
        let budget = Budget::new();
        let now = Instant::now();
        let lease = budget.acquire(ConnectionId(1), now).unwrap();
        let until = now + Duration::from_secs(2);
        let frame = frames.track(vec![1; 1024], lease, until).unwrap();
        let first = frame.page(0, now).unwrap();
        assert_eq!(
            frame.page(0, until - Duration::from_millis(1)).unwrap(),
            first
        );
        assert!(frame.page(1, now).is_none());
        assert!(frame.page(usize::MAX, now).is_none());
        assert!(frame.page(0, until).is_none());
        assert!(frame.data.lock().unwrap().is_none());
        frames.expire(until);
        assert!(frames.0.lock().unwrap().is_empty());
    }
}
