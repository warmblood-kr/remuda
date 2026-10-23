//! Human input authority: one checked grant, bounded raw outcomes, exact retirement.
//! Pure model under the host coordinator; no OS effect, wire or core guard here.
use super::control::ConnectionId;
use super::result::Delivery;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

pub(crate) const MAX_RAW: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GrantRequest {
    pub request: [u8; 16],
    pub expected: u64,
    pub recovery: [u8; 32],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum GrantState {
    Current,
    Superseded,
    Released,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GrantError {
    Stale,
    Conflict,
    Exhausted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Raw {
    New,
    Pending,
    Complete(Delivery),
    Forgotten,
    Conflict,
    OutOfOrder,
    Busy,
    Stale,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Ack,
    Discard,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cut {
    pub grant: u64,
    pub first: u64,
    pub last: u64,
    pub hash: [u8; 32],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Retire {
    Retired,
    Forgotten,
    Stale,
    Live,
    Pending,
    Conflict,
}
#[derive(Clone, Copy)]
struct Holder {
    grant: u64,
    connection: ConnectionId,
}
#[derive(Clone, Copy)]
struct Receipt {
    request: GrantRequest,
    grant: u64,
    released: bool,
}
struct Entry {
    grant: u64,
    sequence: u64,
    digest: [u8; 32],
    delivery: Option<Delivery>,
}
pub(crate) struct Human {
    incarnation: [u8; 16],
    high_water: u64,
    current: Option<Holder>,
    receipt: Option<Receipt>,
    next_raw: u64,
    retired_through: u64,
    entries: VecDeque<Entry>,
    retirement: Option<(Kind, Cut)>,
}
#[derive(Serialize)]
struct Canonical<'a> {
    version: u8,
    incarnation: [u8; 16],
    grant: u64,
    first: u64,
    last: u64,
    entries: Vec<(u64, [u8; 32], &'a Delivery)>,
}
impl Human {
    pub fn new(incarnation: [u8; 16]) -> Self {
        Self {
            incarnation,
            high_water: 0,
            current: None,
            receipt: None,
            next_raw: 0,
            retired_through: 0,
            entries: VecDeque::new(),
            retirement: None,
        }
    }
    pub fn high_water(&self) -> u64 {
        self.high_water
    }
    /// The last raw sequence reserved by any grant; the next one is this + 1.
    pub fn last_raw(&self) -> u64 {
        self.next_raw
    }
    /// The latest exact request answers evidence only; it never transfers a
    /// grant. Any other request must name the current high-water.
    pub fn grant(
        &mut self,
        connection: ConnectionId,
        request: GrantRequest,
    ) -> Result<(u64, GrantState), GrantError> {
        let high_water = self.high_water;
        self.grant_with(connection, request, |expected| {
            if expected != high_water {
                return Err(GrantError::Stale);
            }
            high_water.checked_add(1).ok_or(GrantError::Exhausted)
        })
    }
    /// `cas` runs the authoritative compare-and-advance (core attach slot)
    /// only after exact-receipt evidence, and returns the new generation.
    pub fn grant_with(
        &mut self,
        connection: ConnectionId,
        request: GrantRequest,
        cas: impl FnOnce(u64) -> Result<u64, GrantError>,
    ) -> Result<(u64, GrantState), GrantError> {
        if let Some(receipt) = self
            .receipt
            .filter(|r| r.request.request == request.request)
        {
            if receipt.request != request {
                return Err(GrantError::Conflict);
            }
            return Ok((receipt.grant, self.state(receipt)));
        }
        let grant = cas(request.expected)?;
        self.high_water = grant;
        self.current = Some(Holder { grant, connection });
        self.receipt = Some(Receipt {
            request,
            grant,
            released: false,
        });
        Ok((grant, GrantState::Current))
    }
    fn state(&self, receipt: Receipt) -> GrantState {
        if self
            .current
            .is_some_and(|holder| holder.grant == receipt.grant)
        {
            GrantState::Current
        } else if receipt.released {
            GrantState::Released
        } else {
            GrantState::Superseded
        }
    }
    pub fn current(&self) -> Option<u64> {
        self.current.map(|holder| holder.grant)
    }
    pub fn holds(&self, connection: ConnectionId, grant: u64) -> bool {
        self.current
            .is_some_and(|holder| holder.grant == grant && holder.connection == connection)
    }
    /// Exact revoke only; a stale revoke never touches a successor grant.
    pub fn revoke(&mut self, grant: u64) -> bool {
        if self.current() != Some(grant) {
            return false;
        }
        self.current = None;
        if let Some(receipt) = self.receipt.as_mut().filter(|r| r.grant == grant) {
            receipt.released = true;
        }
        true
    }
    pub fn disconnect(&mut self, connection: ConnectionId) -> Option<u64> {
        let holder = self.current.filter(|h| h.connection == connection)?;
        self.revoke(holder.grant);
        Some(holder.grant)
    }
    fn find(&self, sequence: u64) -> Option<&Entry> {
        let offset = sequence.checked_sub(self.retired_through + 1)?;
        self.entries.get(usize::try_from(offset).ok()?)
    }
    /// Read-only outcome evidence; never admits or replays.
    pub fn recover(&self, grant: u64, sequence: u64, digest: [u8; 32]) -> Raw {
        if sequence == 0 {
            return Raw::OutOfOrder;
        }
        if sequence <= self.retired_through {
            return Raw::Forgotten;
        }
        match self.find(sequence) {
            Some(entry) if entry.grant != grant || entry.digest != digest => Raw::Conflict,
            Some(entry) => entry.delivery.map_or(Raw::Pending, Raw::Complete),
            None => Raw::OutOfOrder,
        }
    }
    /// Retained retries answer before the grant check. Busy spends nothing.
    pub fn reserve(&mut self, grant: u64, sequence: u64, digest: [u8; 32]) -> Raw {
        match self.recover(grant, sequence, digest) {
            Raw::OutOfOrder => {}
            existing => return existing,
        }
        if self.current() != Some(grant) {
            return Raw::Stale;
        }
        if Some(sequence) != self.next_raw.checked_add(1) {
            return Raw::OutOfOrder;
        }
        if self.entries.len() == MAX_RAW {
            return Raw::Busy;
        }
        self.entries.push_back(Entry {
            grant,
            sequence,
            digest,
            delivery: None,
        });
        self.next_raw = sequence;
        Raw::New
    }
    pub fn complete(&mut self, sequence: u64, delivery: Delivery) -> bool {
        let offset = sequence.checked_sub(self.retired_through + 1);
        let entry = offset
            .and_then(|offset| usize::try_from(offset).ok())
            .and_then(|offset| self.entries.get_mut(offset));
        match entry {
            Some(entry) if entry.delivery.is_none() => {
                entry.delivery = Some(delivery);
                true
            }
            _ => false,
        }
    }
    /// Canonical cut from retired-through+1 over one grant's completed prefix.
    pub fn cut(&self, last: u64) -> Result<Cut, Retire> {
        let first = self.retired_through + 1;
        if last < first {
            return Err(Retire::Forgotten);
        }
        let mut grant = None;
        let mut entries = Vec::new();
        for sequence in first..=last {
            let entry = self.find(sequence).ok_or(Retire::Conflict)?;
            let delivery = entry.delivery.as_ref().ok_or(Retire::Pending)?;
            if *grant.get_or_insert(entry.grant) != entry.grant {
                return Err(Retire::Conflict);
            }
            entries.push((entry.sequence, entry.digest, delivery));
        }
        let grant = grant.ok_or(Retire::Conflict)?;
        let canonical = Canonical {
            version: 1,
            incarnation: self.incarnation,
            grant,
            first,
            last,
            entries,
        };
        let bytes = serde_json::to_vec(&canonical).map_err(|_| Retire::Conflict)?;
        Ok(Cut {
            grant,
            first,
            last,
            hash: Sha256::digest(bytes).into(),
        })
    }
    /// Current viewer ACKs its own exact completed prefix.
    pub fn ack(&mut self, connection: ConnectionId, cut: Cut) -> Retire {
        self.retire(Kind::Ack, cut, |human| human.holds(connection, cut.grant))
    }
    /// Management discards an inactive grant's exact cut; caller holds a fence.
    pub fn discard(&mut self, cut: Cut) -> Retire {
        if self.current() == Some(cut.grant) {
            return Retire::Live;
        }
        self.retire(Kind::Discard, cut, |_| true)
    }
    fn retire(&mut self, kind: Kind, cut: Cut, allowed: impl Fn(&Self) -> bool) -> Retire {
        if self.retirement == Some((kind, cut)) {
            return Retire::Retired;
        }
        if cut.last <= self.retired_through {
            return Retire::Forgotten;
        }
        if !allowed(self) {
            return Retire::Stale;
        }
        match self.cut(cut.last) {
            Ok(actual) if actual == cut => {}
            Ok(_) => return Retire::Conflict,
            Err(error) => return error,
        }
        let count = cut.last - cut.first + 1;
        self.entries.drain(..count as usize);
        self.retired_through = cut.last;
        self.retirement = Some((kind, cut));
        Retire::Retired
    }
    /// Stop requires no live grant and no unretired raw outcome.
    pub fn settled(&self) -> bool {
        self.current.is_none() && self.entries.is_empty()
    }
}

#[cfg(test)]
#[path = "human_tests.rs"]
mod tests;
