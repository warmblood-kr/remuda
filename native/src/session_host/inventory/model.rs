use super::super::api::Identity;
use super::super::result::Completion;
use super::super::wire::InputAct;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, VecDeque};
#[path = "publication.rs"]
mod publication;
pub(crate) use publication::TerminalEnd;
pub(crate) use publication::{Birth, Lease, Publication, Settled};

pub(crate) const MAX_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_ENTRIES: usize = 128;
pub(crate) const MAX_METADATA: usize = 8192;
pub(crate) const MAX_NAMESPACES: usize = 16;
pub(crate) const MAX_CLIENTS: usize = 16;
pub(crate) const MAX_RESULTS: usize = 32;
const MAX_EVENTS: usize = 256;
const MAX_TOMBSTONES: usize = 128;
const MAX_ALLOCATIONS: usize = 1024;
const EVENT_BYTES: usize = 2048;
const PENDING_BYTES: usize = 16 * 1024;
const ACK_BYTES: usize = 10 * 1024;
const REQUIRED: [&str; 4] = [
    "owner-fence-v1",
    "typed-input-json-sha256-v1",
    "raw-launch-json-sha256-v1",
    "nonrecycled-identities-v1",
];
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Revision {
    pub store: [u8; 16],
    pub number: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Fence {
    store: [u8; 16],
    epoch: u64,
    nonce: [u8; 16],
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Key {
    pub client: [u8; 16],
    pub sequence: u64,
    pub payload: [u8; 32],
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Namespace {
    pub revision: u64,
    pub schema: u16,
    pub payload: Box<RawValue>,
}
impl Namespace {
    pub fn new(schema: u16, payload: &str) -> Result<Self> {
        if payload.len() > MAX_METADATA {
            return Err(Error::Capacity);
        }
        let payload =
            RawValue::from_string(payload.into()).map_err(|_| Error::Invalid("metadata JSON"))?;
        Ok(Self {
            revision: 1,
            schema,
            payload,
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Phase {
    Reserved,
    Registered,
    Exited { generation: u64 },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum InputPhase {
    Unresolved,
    Admitted,
    Indeterminate,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pending {
    pub intent: [u8; 16],
    pub key: Key,
    pub schema: u16,
    pub store: [u8; 16],
    pub writer_epoch: u64,
    pub phase: InputPhase,
    pub act: InputAct,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Retained {
    pub reservation: Pending,
    pub completion: Completion,
    pub consumer_ack: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Stream {
    pub client: [u8; 16],
    pub next: Option<u64>,
    pub forgotten_through: u64,
    pub results: VecDeque<Retained>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub identity: Identity,
    pub launch: [u8; 16],
    pub launch_payload: Box<RawValue>,
    pub launch_hash: [u8; 32],
    pub phase: Phase,
    pub metadata: BTreeMap<String, Namespace>,
    pub streams: Vec<Stream>,
    pub pending: Option<Pending>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub birth: Option<Birth>,
}
impl Entry {
    pub fn reserved(identity: Identity, launch: [u8; 16], payload: &str) -> Result<Self> {
        if payload.len() > 64 * 1024 {
            return Err(Error::Capacity);
        }
        let payload =
            RawValue::from_string(payload.into()).map_err(|_| Error::Invalid("launch JSON"))?;
        Ok(Self {
            identity,
            launch,
            launch_hash: hash(payload.get().as_bytes()),
            launch_payload: payload,
            phase: Phase::Reserved,
            metadata: BTreeMap::new(),
            streams: Vec::new(),
            pending: None,
            birth: None,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tombstone {
    pub name: String,
    pub identity: Identity,
    pub generation: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub revision: u64,
    pub name: Option<String>,
    pub identity: Option<Identity>,
    pub kind: EventKind,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum EventKind {
    Writer,
    Launch,
    Register,
    Metadata,
    Input,
    Outcome,
    Consumer,
    Exit,
    Host,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Index {
    schema: u16,
    required: Vec<String>,
    pub(super) incarnation: [u8; 16],
    pub(super) revision: u64,
    epoch: u64,
    nonce: [u8; 16],
    allocations: Vec<Identity>,
    pub entries: BTreeMap<String, Entry>,
    pub tombstones: VecDeque<Tombstone>,
    pub events: VecDeque<Event>,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Invalid(&'static str),
    Incompatible,
    Corrupt,
    Capacity,
    Busy,
    Conflict,
    StaleStore,
    StaleRevision,
    StaleFence,
    Exhausted,
    Unavailable,
}
pub(crate) type Result<T> = std::result::Result<T, Error>;
pub(crate) struct Target {
    pub name: String,
    pub identity: Identity,
}
pub(crate) struct MetadataUpdate {
    pub namespace: String,
    pub expected: Option<u64>,
    pub value: Namespace,
}
pub(crate) struct ConsumerCheckpoint {
    pub intent: [u8; 16],
    pub outcome: [u8; 32],
    pub metadata: MetadataUpdate,
}
pub(crate) enum Mutation {
    Claim {
        nonce: [u8; 16],
    },
    ReserveLaunch {
        name: String,
        entry: Entry,
    },
    Register(Target),
    Metadata {
        target: Target,
        update: MetadataUpdate,
    },
    Input {
        target: Target,
        operation: InputMutation,
    },
    Exit {
        target: Target,
        operation: ExitMutation,
    },
}
pub(crate) enum InputMutation {
    Reserve {
        intent: [u8; 16],
        key: Key,
        act: InputAct,
    },
    Adopt(Key),
    Observe {
        key: Key,
        phase: InputPhase,
    },
    Complete {
        key: Key,
        completion: Completion,
    },
    Consume {
        key: Key,
        checkpoint: ConsumerCheckpoint,
    },
}
pub(crate) enum ExitMutation {
    Observe { generation: u64 },
    Retire,
}
pub(crate) struct Applied {
    pub next: Index,
    pub fence: Fence,
}
#[derive(Debug)]
pub(crate) enum Lookup<'a> {
    Pending(&'a Pending),
    Complete(&'a Retained),
    Forgotten,
    Conflict,
    Absent,
    OutOfOrder,
}
#[derive(Debug)]
pub(crate) enum Tail<'a> {
    Events(Vec<&'a Event>),
    SnapshotRequired,
}
impl Index {
    pub fn new(incarnation: [u8; 16]) -> Self {
        Self {
            schema: 1,
            required: REQUIRED.iter().map(|s| (*s).into()).collect(),
            incarnation,
            revision: 0,
            epoch: 0,
            nonce: [0; 16],
            allocations: Vec::new(),
            entries: BTreeMap::new(),
            tombstones: VecDeque::new(),
            events: VecDeque::new(),
        }
    }
    pub fn cursor(&self) -> Revision {
        Revision {
            store: self.incarnation,
            number: self.revision,
        }
    }
    /// Absent is only absence from this publication cut, never host admission proof.
    pub fn lookup(&self, target: &Target, key: &Key) -> Result<Lookup<'_>> {
        let entry = self.entries.get(&target.name).ok_or(Error::Unavailable)?;
        if entry.identity != target.identity {
            return Err(Error::Conflict);
        }
        if key.sequence == 0 {
            return Ok(Lookup::OutOfOrder);
        }
        if let Some(pending) = &entry.pending {
            if pending.key.client == key.client && pending.key.sequence == key.sequence {
                return Ok(if pending.key.payload == key.payload {
                    Lookup::Pending(pending)
                } else {
                    Lookup::Conflict
                });
            }
        }
        let Some(stream) = entry.streams.iter().find(|s| s.client == key.client) else {
            return Ok(if key.sequence == 1 {
                Lookup::Absent
            } else {
                Lookup::OutOfOrder
            });
        };
        if key.sequence <= stream.forgotten_through {
            return Ok(Lookup::Forgotten);
        }
        if let Some(result) = stream
            .results
            .iter()
            .find(|r| r.reservation.key.sequence == key.sequence)
        {
            return Ok(if result.reservation.key.payload == key.payload {
                Lookup::Complete(result)
            } else {
                Lookup::Conflict
            });
        }
        Ok(if stream.next == Some(key.sequence) {
            Lookup::Absent
        } else {
            Lookup::OutOfOrder
        })
    }
    pub fn tail(&self, cursor: Revision) -> Result<Tail<'_>> {
        if cursor.store != self.incarnation {
            return Err(Error::StaleStore);
        }
        if cursor.number > self.revision {
            return Err(Error::StaleRevision);
        }
        if self
            .events
            .front()
            .is_some_and(|event| cursor.number < event.revision.saturating_sub(1))
        {
            return Ok(Tail::SnapshotRequired);
        }
        Ok(Tail::Events(
            self.events
                .iter()
                .filter(|e| e.revision > cursor.number)
                .collect(),
        ))
    }
    fn fence(&self) -> Fence {
        Fence {
            store: self.incarnation,
            epoch: self.epoch,
            nonce: self.nonce,
        }
    }
    pub fn apply(
        &self,
        expected: Revision,
        fence: Option<Fence>,
        mutation: Mutation,
    ) -> Result<Applied> {
        if expected.store != self.incarnation {
            return Err(Error::StaleStore);
        }
        if expected.number != self.revision {
            return Err(Error::StaleRevision);
        }
        if !matches!(mutation, Mutation::Claim { .. })
            && (self.epoch == 0 || fence != Some(self.fence()))
        {
            return Err(Error::StaleFence);
        }
        let mut next = self.clone();
        let identity = match &mutation {
            Mutation::Claim { .. } => None,
            Mutation::ReserveLaunch { entry, .. } => Some(entry.identity.clone()),
            Mutation::Register(target)
            | Mutation::Metadata { target, .. }
            | Mutation::Input { target, .. }
            | Mutation::Exit { target, .. } => Some(target.identity.clone()),
        };
        next.revision = next.revision.checked_add(1).ok_or(Error::Exhausted)?;
        let (name, kind) = next.mutate(mutation)?;
        next.events.push_back(Event {
            revision: next.revision,
            name,
            identity,
            kind,
        });
        while next.events.len() > MAX_EVENTS {
            next.events.pop_front();
        }
        next.validate()?;
        // Count encoding here too, so a failed aggregate budget leaves self unchanged.
        let _ = next.encode()?;
        Ok(Applied {
            fence: next.fence(),
            next,
        })
    }
    fn reserved_bytes(&self) -> Result<usize> {
        // Replacing already-present event/tombstone bytes spends their own
        // credit. Decimal counters and phases are normalized to maximum width.
        let events = serde_json::to_vec(&self.events)
            .map_err(|_| Error::Corrupt)?
            .len();
        let tombstones = serde_json::to_vec(&self.tombstones)
            .map_err(|_| Error::Corrupt)?
            .len();
        let mut bytes = (MAX_EVENTS * EVENT_BYTES + 2)
            .checked_sub(events)
            .ok_or(Error::Corrupt)?
            + (MAX_TOMBSTONES * EVENT_BYTES + 2)
                .checked_sub(tombstones)
                .ok_or(Error::Corrupt)?
            + digits(self.revision)
            + digits(self.epoch);
        bytes += 65
            - serde_json::to_vec(&self.nonce)
                .map_err(|_| Error::Corrupt)?
                .len();
        for entry in self.entries.values() {
            if let Some(birth) = &entry.birth {
                bytes += birth.reserved_bytes()?;
            }
            bytes += 64
                - serde_json::to_vec(&entry.phase)
                    .map_err(|_| Error::Corrupt)?
                    .len();
            for metadata in entry.metadata.values() {
                bytes += digits(metadata.revision);
            }
            if let Some(pending) = &entry.pending {
                bytes += PENDING_BYTES;
                bytes += digits(pending.writer_epoch) + 32
                    - serde_json::to_vec(&pending.phase)
                        .map_err(|_| Error::Corrupt)?
                        .len();
            }
            for stream in &entry.streams {
                bytes += digits(stream.forgotten_through) + stream.next.map_or(0, digits);
                for result in &stream.results {
                    if !result.consumer_ack {
                        bytes += ACK_BYTES;
                    }
                    bytes += digits(result.reservation.writer_epoch) + 32
                        - serde_json::to_vec(&result.reservation.phase)
                            .map_err(|_| Error::Corrupt)?
                            .len();
                }
            }
        }
        Ok(bytes)
    }
    fn entry(&mut self, name: &str, identity: &Identity) -> Result<&mut Entry> {
        let entry = self.entries.get_mut(name).ok_or(Error::Unavailable)?;
        if entry.identity != *identity {
            return Err(Error::Conflict);
        }
        Ok(entry)
    }
    fn mutate(&mut self, mutation: Mutation) -> Result<(Option<String>, EventKind)> {
        match mutation {
            Mutation::Claim { nonce } => {
                self.epoch = self.epoch.checked_add(1).ok_or(Error::Exhausted)?;
                self.nonce = nonce;
                Ok((None, EventKind::Writer))
            }
            Mutation::ReserveLaunch { name, entry } => {
                if self.entries.len() == MAX_ENTRIES {
                    return Err(Error::Capacity);
                }
                if self.entries.contains_key(&name) || entry.phase != Phase::Reserved {
                    return Err(Error::Conflict);
                }
                if self.allocations.contains(&entry.identity) {
                    return Err(Error::Conflict);
                }
                if self.allocations.len() == MAX_ALLOCATIONS {
                    return Err(Error::Exhausted);
                }
                self.allocations.push(entry.identity.clone());
                if entry.birth.is_some() && !self.required.iter().any(|cap| cap == publication::CAP)
                {
                    self.required.push(publication::CAP.into());
                }
                self.entries.insert(name.clone(), entry);
                Ok((Some(name), EventKind::Launch))
            }
            Mutation::Register(target) => {
                let entry = self.entry(&target.name, &target.identity)?;
                if entry.birth.is_some() {
                    return Err(Error::Invalid("host publisher lease required"));
                }
                if matches!(entry.phase, Phase::Exited { .. }) {
                    return Err(Error::Conflict);
                }
                entry.phase = Phase::Registered;
                Ok((Some(target.name), EventKind::Register))
            }
            Mutation::Metadata { target, update } => {
                if update.namespace == "consumer" {
                    return Err(Error::Invalid("reserved consumer namespace"));
                }
                let entry = self.entry(&target.name, &target.identity)?;
                if !entry.metadata.contains_key(&update.namespace)
                    && entry
                        .metadata
                        .keys()
                        .filter(|key| key.as_str() != "consumer")
                        .count()
                        >= MAX_NAMESPACES - 1
                {
                    return Err(Error::Capacity);
                }
                metadata(entry, update)?;
                Ok((Some(target.name), EventKind::Metadata))
            }
            Mutation::Input { target, operation } => self.input(target, operation),
            Mutation::Exit { target, operation } => self.exit(target, operation),
        }
    }
    fn input(
        &mut self,
        target: Target,
        operation: InputMutation,
    ) -> Result<(Option<String>, EventKind)> {
        let store = self.incarnation;
        let epoch = self.epoch;
        let entry = self.entry(&target.name, &target.identity)?;
        let kind = match operation {
            InputMutation::Reserve { intent, key, act } => {
                reserve(
                    entry,
                    Pending {
                        intent,
                        key,
                        act,
                        schema: 1,
                        store,
                        writer_epoch: epoch,
                        phase: InputPhase::Unresolved,
                    },
                )?;
                EventKind::Input
            }
            InputMutation::Adopt(key) => {
                pending(entry, &key)?.writer_epoch = epoch;
                EventKind::Input
            }
            InputMutation::Observe { key, phase } => {
                if phase == InputPhase::Unresolved {
                    return Err(Error::Invalid("unresolved is not never-admitted proof"));
                }
                let pending = pending(entry, &key)?;
                if pending.writer_epoch != epoch {
                    return Err(Error::StaleFence);
                }
                pending.phase = phase;
                EventKind::Input
            }
            InputMutation::Complete { key, completion } => {
                if pending(entry, &key)?.writer_epoch != epoch {
                    return Err(Error::StaleFence);
                }
                complete(entry, &key, completion)?;
                EventKind::Outcome
            }
            InputMutation::Consume { key, checkpoint } => {
                consume(entry, &key, checkpoint)?;
                EventKind::Consumer
            }
        };
        Ok((Some(target.name), kind))
    }
    fn exit(
        &mut self,
        target: Target,
        operation: ExitMutation,
    ) -> Result<(Option<String>, EventKind)> {
        let entry = self.entry(&target.name, &target.identity)?;
        match operation {
            ExitMutation::Observe { generation } => {
                if entry.birth.is_some() {
                    return Err(Error::Invalid("host publisher lease required"));
                }
                match entry.phase {
                    Phase::Exited { generation: old } if old != generation => {
                        return Err(Error::Conflict)
                    }
                    Phase::Reserved => return Err(Error::Unavailable),
                    _ => {}
                }
                entry.phase = Phase::Exited { generation };
            }
            ExitMutation::Retire => {
                if entry.birth.as_ref().is_some_and(|birth| !birth.retirable()) {
                    return Err(Error::Unavailable);
                }
                let Phase::Exited { generation } = entry.phase else {
                    return Err(Error::Unavailable);
                };
                if entry.pending.is_some() || entry.streams.iter().any(|s| !s.results.is_empty()) {
                    return Err(Error::Busy);
                }
                self.entries.remove(&target.name);
                self.tombstones.push_back(Tombstone {
                    name: target.name.clone(),
                    identity: target.identity,
                    generation,
                });
                while self.tombstones.len() > MAX_TOMBSTONES {
                    self.tombstones.pop_front();
                }
            }
        }
        Ok((Some(target.name), EventKind::Exit))
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut writer = Bounded(Vec::new());
        serde_json::to_writer(&mut writer, self).map_err(|_| Error::Capacity)?;
        Ok(writer.0)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::Capacity);
        }
        #[derive(Deserialize)]
        struct Header {
            schema: u16,
            required: Vec<String>,
        }
        let header: Header = serde_json::from_slice(bytes).map_err(|_| Error::Corrupt)?;
        if header.schema != 1
            || header
                .required
                .iter()
                .any(|cap| !REQUIRED.contains(&cap.as_str()) && cap != publication::CAP)
        {
            return Err(Error::Incompatible);
        }
        let index: Self = serde_json::from_slice(bytes).map_err(|_| Error::Incompatible)?;
        index.validate()?;
        Ok(index)
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != 1
            || self.required.len() > 16
            || REQUIRED
                .iter()
                .any(|s| !self.required.iter().any(|v| v == s))
        {
            return Err(Error::Incompatible);
        }
        if self.entries.len() > MAX_ENTRIES
            || self.events.len() > MAX_EVENTS
            || self.tombstones.len() > MAX_TOMBSTONES
            || self.allocations.len() > MAX_ALLOCATIONS
        {
            return Err(Error::Capacity);
        }
        if self.epoch == 0 && (self.revision != 0 || !self.entries.is_empty()) {
            return Err(Error::Corrupt);
        }
        let mut allocated = std::collections::BTreeSet::new();
        for identity in &self.allocations {
            if !valid_identity(identity)
                || !allocated.insert((
                    identity.host,
                    identity.session.clone(),
                    identity.instance.clone(),
                ))
            {
                return Err(Error::Corrupt);
            }
        }
        let mut identities = std::collections::BTreeSet::new();
        for (name, entry) in &self.entries {
            validate_entry(name, entry, self.incarnation, self.epoch)?;
            if let Some(birth) = &entry.birth {
                if !self.required.iter().any(|cap| cap == publication::CAP) {
                    return Err(Error::Incompatible);
                }
                birth.validate(entry.phase)?;
            }
            if !self.allocations.contains(&entry.identity) {
                return Err(Error::Corrupt);
            }
            if !identities.insert((
                entry.identity.host,
                entry.identity.session.clone(),
                entry.identity.instance.clone(),
            )) {
                return Err(Error::Conflict);
            }
        }
        let first = self
            .revision
            .checked_sub(self.events.len() as u64)
            .ok_or(Error::Corrupt)?;
        for (offset, event) in self.events.iter().enumerate() {
            if event.revision != first + offset as u64 + 1
                || event.name.as_ref().is_some_and(|name| !valid_name(name))
                || event
                    .identity
                    .as_ref()
                    .is_some_and(|identity| !valid_identity(identity))
                || serde_json::to_vec(event).map_err(|_| Error::Corrupt)?.len() >= EVENT_BYTES
            {
                return Err(Error::Corrupt);
            }
        }
        for tombstone in &self.tombstones {
            if !valid_name(&tombstone.name) || !self.allocations.contains(&tombstone.identity) {
                return Err(Error::Corrupt);
            }
            if serde_json::to_vec(tombstone)
                .map_err(|_| Error::Corrupt)?
                .len()
                >= EVENT_BYTES
            {
                return Err(Error::Corrupt);
            }
        }
        if self.encode()?.len().saturating_add(self.reserved_bytes()?) > MAX_BYTES {
            return Err(Error::Capacity);
        }
        Ok(())
    }
}
fn digits(value: u64) -> usize {
    20 - value.to_string().len()
}
fn pending<'a>(entry: &'a mut Entry, key: &Key) -> Result<&'a mut Pending> {
    entry
        .pending
        .as_mut()
        .filter(|p| p.key == *key)
        .ok_or(Error::Conflict)
}
fn reserve(entry: &mut Entry, reservation: Pending) -> Result<()> {
    if entry.phase != Phase::Registered {
        return Err(Error::Unavailable);
    }
    if entry.pending.is_some() {
        return Err(Error::Busy);
    }
    if reservation.key.sequence == 0
        || reservation.act.digest().map_err(Error::Invalid)? != reservation.key.payload
    {
        return Err(Error::Conflict);
    }
    let index = if let Some(index) = entry
        .streams
        .iter()
        .position(|s| s.client == reservation.key.client)
    {
        index
    } else {
        if entry.streams.len() == MAX_CLIENTS {
            return Err(Error::Capacity);
        }
        entry.streams.push(Stream {
            client: reservation.key.client,
            next: Some(1),
            forgotten_through: 0,
            results: VecDeque::new(),
        });
        entry.streams.len() - 1
    };
    let stream = &entry.streams[index];
    if stream.results.len() == MAX_RESULTS {
        return Err(Error::Capacity);
    }
    if stream.next.is_none() {
        return Err(Error::Exhausted);
    }
    if stream.next != Some(reservation.key.sequence) {
        return Err(Error::Conflict);
    }
    entry.pending = Some(reservation);
    Ok(())
}
fn complete(entry: &mut Entry, key: &Key, completion: Completion) -> Result<()> {
    let reservation = pending(entry, key)?.clone();
    let stream = entry
        .streams
        .iter_mut()
        .find(|s| s.client == key.client)
        .ok_or(Error::Corrupt)?;
    if stream.results.len() == MAX_RESULTS {
        return Err(Error::Capacity);
    }
    stream.results.push_back(Retained {
        reservation,
        completion: completion.bounded(),
        consumer_ack: false,
    });
    stream.next = key.sequence.checked_add(1);
    entry.pending = None;
    Ok(())
}
fn metadata(entry: &mut Entry, update: MetadataUpdate) -> Result<()> {
    let current = entry.metadata.get(&update.namespace);
    if current.map(|v| v.revision) != update.expected {
        return Err(Error::Conflict);
    }
    if current.is_some_and(|v| v.schema != update.value.schema) {
        return Err(Error::Incompatible);
    }
    let mut value = update.value;
    value.revision = update
        .expected
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(Error::Exhausted)?;
    entry.metadata.insert(update.namespace, value);
    Ok(())
}
fn consume(entry: &mut Entry, key: &Key, checkpoint: ConsumerCheckpoint) -> Result<()> {
    if checkpoint.metadata.namespace != "consumer" || checkpoint.metadata.value.schema != 1 {
        return Err(Error::Invalid("consumer checkpoint namespace/schema"));
    }
    let result = entry
        .streams
        .iter()
        .find(|s| s.client == key.client)
        .and_then(|s| s.results.iter().find(|r| r.reservation.key == *key))
        .ok_or(Error::Conflict)?;
    let encoded = serde_json::to_vec(&result.completion).map_err(|_| Error::Corrupt)?;
    if result.reservation.intent != checkpoint.intent || hash(&encoded) != checkpoint.outcome {
        return Err(Error::Conflict);
    }
    // The consumer checkpoint and result GC publish in the same transaction.
    metadata(entry, checkpoint.metadata)?;
    let stream = entry
        .streams
        .iter_mut()
        .find(|s| s.client == key.client)
        .ok_or(Error::Conflict)?;
    stream
        .results
        .iter_mut()
        .find(|r| r.reservation.key == *key)
        .ok_or(Error::Conflict)?
        .consumer_ack = true;
    while stream.results.front().is_some_and(|r| r.consumer_ack) {
        stream.forgotten_through = stream.results.pop_front().unwrap().reservation.key.sequence;
    }
    Ok(())
}
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.chars().any(char::is_control)
}
fn valid_identity(identity: &Identity) -> bool {
    [&identity.session, &identity.instance]
        .iter()
        .all(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
}
fn validate_pending(pending: &Pending, store: [u8; 16], epoch: u64) -> Result<()> {
    if pending.store != store
        || pending.schema != 1
        || pending.writer_epoch == 0
        || pending.writer_epoch > epoch
        || pending.key.sequence == 0
        || pending.act.digest().map_err(Error::Invalid)? != pending.key.payload
    {
        return Err(Error::Corrupt);
    }
    Ok(())
}
fn validate_entry(name: &str, entry: &Entry, store: [u8; 16], epoch: u64) -> Result<()> {
    if !valid_name(name) || !valid_identity(&entry.identity) {
        return Err(Error::Invalid("inventory identity"));
    }
    if entry.launch_payload.get().len() > 64 * 1024
        || hash(entry.launch_payload.get().as_bytes()) != entry.launch_hash
    {
        return Err(Error::Conflict);
    }
    if entry.metadata.len() > MAX_NAMESPACES || entry.streams.len() > MAX_CLIENTS {
        return Err(Error::Capacity);
    }
    if entry
        .metadata
        .keys()
        .filter(|key| key.as_str() != "consumer")
        .count()
        >= MAX_NAMESPACES
        || entry
            .metadata
            .get("consumer")
            .is_some_and(|value| value.schema != 1)
    {
        return Err(Error::Capacity);
    }
    for (key, value) in &entry.metadata {
        if !valid_name(key)
            || value.revision == 0
            || value.schema == 0
            || value.payload.get().len() > MAX_METADATA
        {
            return Err(Error::Invalid("metadata budget/schema"));
        }
    }
    if entry.phase == Phase::Reserved && (!entry.streams.is_empty() || entry.pending.is_some()) {
        return Err(Error::Corrupt);
    }
    if let Some(pending) = &entry.pending {
        validate_pending(pending, store, epoch)?;
        if !entry.streams.iter().any(|stream| {
            stream.client == pending.key.client && stream.next == Some(pending.key.sequence)
        }) {
            return Err(Error::Corrupt);
        }
    }
    let mut clients = std::collections::BTreeSet::new();
    for stream in &entry.streams {
        if stream.results.len() > MAX_RESULTS {
            return Err(Error::Capacity);
        }
        if !clients.insert(stream.client) || stream.next == Some(0) {
            return Err(Error::Corrupt);
        }
        let mut expected = stream.forgotten_through.checked_add(1);
        for retained in &stream.results {
            validate_pending(&retained.reservation, store, epoch)?;
            let key = &retained.reservation.key;
            if key.client != stream.client
                || Some(key.sequence) != expected
                || !retained.completion.fits()
            {
                return Err(Error::Corrupt);
            }
            expected = key.sequence.checked_add(1);
        }
        if expected != stream.next || stream.results.front().is_some_and(|r| r.consumer_ack) {
            return Err(Error::Corrupt);
        }
    }
    Ok(())
}
pub(crate) fn hash(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}
struct Bounded(Vec<u8>);
impl std::io::Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("inventory byte budget"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
