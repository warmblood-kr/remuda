//! Scoped host publications survive controller epoch replacement; no effects.
use super::*;
pub(super) const CAP: &str = crate::session_host::api::SCOPED_BIRTH;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Lease {
    store: [u8; 16],
    name: String,
    identity: Identity,
    launch: [u8; 16],
    nonce: [u8; 16],
}
impl Lease {
    pub fn new(store: [u8; 16], target: Target, launch: [u8; 16], nonce: [u8; 16]) -> Self {
        Self {
            store,
            name: target.name,
            identity: target.identity,
            launch,
            nonce,
        }
    }
    pub fn launch_binding(
        &self,
        index: &Index,
        endpoint: &str,
        payload: [u8; 32],
    ) -> Result<(String, Identity)> {
        if self.store != index.incarnation {
            return Err(Error::StaleStore);
        }
        let entry = index.entries.get(&self.name).ok_or(Error::Unavailable)?;
        if entry.identity != self.identity
            || entry.launch != self.launch
            || entry.launch_hash != payload
        {
            return Err(Error::Conflict);
        }
        let birth = entry.birth.as_ref().ok_or(Error::Unavailable)?;
        if birth.nonce != self.nonce || birth.endpoint != endpoint {
            return Err(Error::StaleFence);
        }
        if entry.phase != Phase::Reserved {
            return Err(Error::Conflict);
        }
        Ok((self.name.clone(), self.identity.clone()))
    }
    pub fn has_unconsumed(&self, index: &Index) -> Result<bool> {
        if self.store != index.incarnation {
            return Err(Error::StaleStore);
        }
        let entry = index.entries.get(&self.name).ok_or(Error::Unavailable)?;
        if entry.identity != self.identity || entry.launch != self.launch {
            return Err(Error::Conflict);
        }
        if entry
            .birth
            .as_ref()
            .is_none_or(|birth| birth.nonce != self.nonce)
        {
            return Err(Error::StaleFence);
        }
        Ok(entry.pending.is_some()
            || entry
                .streams
                .iter()
                .any(|stream| !stream.results.is_empty()))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Birth {
    schema: u16,
    endpoint: String,
    nonce: [u8; 16],
    progress: Progress,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    exit: Option<DirectExit>,
    end: Option<End>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectExit {
    generation: u64,
    code: Option<u32>,
    signal: Option<i32>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct End {
    generation: u64,
    kind: TerminalEnd,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum TerminalEnd {
    Eof,
    Hangup,
    ReadFailure,
}
#[derive(Clone, Copy)]
pub(crate) enum Publication {
    Ready,
    Exited {
        generation: u64,
        code: Option<u32>,
        signal: Option<i32>,
    },
    TerminalEnd {
        generation: u64,
        kind: TerminalEnd,
    },
}
impl Birth {
    pub fn new(endpoint: String, nonce: [u8; 16]) -> Result<Self> {
        let birth = Self {
            schema: 1,
            endpoint,
            nonce,
            progress: Progress::default(),
        };
        birth.validate(Phase::Reserved)?;
        Ok(birth)
    }
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub(super) fn validate(&self, phase: Phase) -> Result<()> {
        if self.schema != 1
            || self.endpoint.is_empty()
            || self.endpoint.len() > 1024
            || self.endpoint.chars().any(char::is_control)
            || self.nonce == [0; 16]
        {
            return Err(Error::Invalid("host birth binding"));
        }
        if phase == Phase::Reserved && (self.progress.exit.is_some() || self.progress.end.is_some())
        {
            return Err(Error::Corrupt);
        }
        match (phase, self.progress.exit) {
            (Phase::Exited { generation }, Some(exit)) if generation == exit.generation => {
                exit.validate()?
            }
            (Phase::Exited { .. }, _) | (_, Some(_)) => return Err(Error::Corrupt),
            _ => {}
        }
        self.reserved_bytes()?;
        Ok(())
    }
    pub(super) fn reserved_bytes(&self) -> Result<usize> {
        let actual = serde_json::to_vec(&self.progress)
            .map_err(|_| Error::Corrupt)?
            .len();
        512usize.checked_sub(actual).ok_or(Error::Corrupt)
    }
    pub(super) fn retirable(&self) -> bool {
        self.progress.exit.is_some()
            && self
                .progress
                .end
                .is_some_and(|end| end.kind != TerminalEnd::ReadFailure)
    }
}
impl DirectExit {
    fn validate(self) -> Result<()> {
        if self.code.is_some() == self.signal.is_some()
            || self
                .signal
                .is_some_and(|signal| signal <= 0 || signal > 255)
        {
            return Err(Error::Invalid("confirmed direct-child exit required"));
        }
        Ok(())
    }
}
impl Index {
    /// Separate authority from apply's global writer fence; returns no Fence.
    pub fn publish(
        &self,
        expected: Revision,
        lease: &Lease,
        publication: Publication,
    ) -> Result<Self> {
        if expected.store != self.incarnation || lease.store != self.incarnation {
            return Err(Error::StaleStore);
        }
        if expected.number != self.revision {
            return Err(Error::StaleRevision);
        }
        let mut next = self.clone();
        let entry = next.entry(&lease.name, &lease.identity)?;
        if entry.launch != lease.launch {
            return Err(Error::Conflict);
        }
        let birth = entry.birth.as_mut().ok_or(Error::Unavailable)?;
        if birth.nonce != lease.nonce {
            return Err(Error::StaleFence);
        }
        let changed = match publication {
            Publication::Ready => match entry.phase {
                Phase::Reserved => {
                    entry.phase = Phase::Registered;
                    true
                }
                Phase::Registered => false,
                Phase::Exited { .. } => return Err(Error::Conflict),
            },
            Publication::Exited {
                generation,
                code,
                signal,
            } => {
                if entry.phase == Phase::Reserved {
                    return Err(Error::Unavailable);
                }
                let exit = DirectExit {
                    generation,
                    code,
                    signal,
                };
                exit.validate()?;
                let changed = set_once(&mut birth.progress.exit, exit)?;
                entry.phase = Phase::Exited { generation };
                changed
            }
            Publication::TerminalEnd { generation, kind } => {
                if entry.phase == Phase::Reserved {
                    return Err(Error::Unavailable);
                }
                set_once(&mut birth.progress.end, End { generation, kind })?
            }
        };
        if !changed {
            next.validate()?;
            return Ok(next);
        }
        next.revision = next.revision.checked_add(1).ok_or(Error::Exhausted)?;
        next.events.push_back(Event {
            revision: next.revision,
            name: Some(lease.name.clone()),
            identity: Some(lease.identity.clone()),
            kind: EventKind::Host,
        });
        while next.events.len() > MAX_EVENTS {
            next.events.pop_front();
        }
        next.validate()?;
        Ok(next)
    }
}
fn set_once<T: Copy + PartialEq>(slot: &mut Option<T>, value: T) -> Result<bool> {
    match slot {
        Some(old) if *old != value => Err(Error::Conflict),
        Some(_) => Ok(false),
        None => {
            *slot = Some(value);
            Ok(true)
        }
    }
}
#[cfg(test)]
#[path = "publication_tests.rs"]
mod tests;
