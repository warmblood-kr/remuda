# 044 — retain owner requests across controller process death

## Before

The host survives its launcher, but a replacement controller has no durable
original request or consumer checkpoint. Reconstructing input from UI intent
could duplicate an effect; treating a missing inventory as empty could respawn.

## Desired outcome

Add an inactive private, bounded Native inventory with explicit publication
revision, independent writer fence, immutable session/host identities, original
typed input payloads and retained results. No production routing changes here.

## Contract

One unresolved act per session returns Busy for competing work. Reserve the
original payload/client/sequence/intent before any future host admission. A new
controller may adopt that reservation but adoption does not authorize resending.
Pending, absent, forgotten and conflicting keys stay distinct. Absent is only
an inventory publication fact, never proof of no physical host admission.

Retain completed payloads/results until an exact consumer checkpoint and ACK
commit together. Never evict unresolved or unconsumed work; bounded capacity
rejects new work. Opaque versioned namespace JSON preserves number spelling and
precision, while unsupported required global schema/capabilities fail closed.

Every mutation and decode reserves 16 KiB per pending act and 10 KiB per
unacknowledged result. The budget also normalizes the bounded event/tombstone
tails to their worst-case byte size, counters to twenty decimal digits, and
mutable phases/nonces to maximum serialized width. Already-present bytes spend
their own credit, so progress never requires unrelated free capacity.
Every event/tombstone is checked below its 2,048-byte slot before tail credit
uses checked subtraction; malformed identities cannot hide an oversized tail.
Completion exchanges pending credit for ACK credit (4 KiB reply plus wrapper
fits the 6 KiB difference). Consumer ACK releases 10 KiB for at most 8 KiB raw
checkpoint plus bounded wrapper. The projected budget and every replacement
both obey the absolute 4 MiB limit.
The dedicated schema-1 `consumer` namespace reserves one of sixteen slots;
ordinary metadata cannot fill or overwrite it. Checkpoint CAS still applies.
Multiple consumer subsystems must coordinate a single schema-1 checkpoint
payload using its namespace revision; they may not overwrite each other's
context. Changing that format requires an explicit versioned migration.

A non-evicted lifetime ledger refuses any previously allocated identity tuple,
even after tombstone wrap; its 1,024-entry limit refuses further allocations.
This is a store-incarnation guarantee. A new store is a distinct domain and
must never automatically adopt old session targets. Random host/instance IDs
still require secure allocation in the future launch coordinator.

Transactions hold a fresh OS file lock, check revision/fence, validate a cloned
candidate, then replace a bounded index atomically. No RPC, PTY, spawn or user
callback executes under the lock. Uncertain replacement returns CommitUnknown,
requiring reread/recovery rather than permission to send. Lock acquisition has
a deadline; kernel filesystem I/O does not acquire a false timeout guarantee.

Unix uses owner-only directories/files and held-FD openat/renameat/unlinkat,
file sync and held-directory sync. Root rename/replacement does not redirect an
existing Store's lock or index. Windows uses held-root relative no-reparse
operations, protected current-user ACLs, file sync and native relative rename.
Existing missing/corrupt roots are never reset. Process-crash fixtures do not
prove power-loss durability, network filesystem behavior or atomic OS-effect
plus outcome-ledger commit. Same-UID arbitrary code remains outside the boundary.

Publication is not physical lifecycle authority. Scoped host publication leases,
birth ordering, verified host recovery, controller RPC integration and consumers
remain subsequent work. Hosts must not receive the global controller writer
fence. Exit observations here are caller assertions; no PID liveness inference
or production session deletion is enabled.

## Expected

A killed writer leaves a recoverable original request; kernel locking serializes
same-process and separate-process writers. Uncertain commits grant no admission,
and stale fences/checkpoints, corrupt roots and bounded capacity refuse changes.
Unix filesystem attacks fail closed. Windows tests must execute natively in CI.

## Actual

macOS: 24 inventory tests passed; one internal subprocess helper is intentionally
ignored except when invoked by its parent fixtures. Tests kill only owned private
fixture processes: a fresh process recovers the exact original reservation after
writer death before caller ACK, and a dead lock holder releases the kernel lock.
Other tests cover concurrent same-Store writers, uncertain commit cuts, bounded
retention, stale consumer checkpoints, counter exhaustion, schema compatibility,
event-tail resync, old-exit identity and hostile Unix file aliases/modes. Review
regressions cover root rename/replacement, near-limit metadata fill followed by
300 progress events, rich completion and maximal checkpoint, and identity reuse
after tombstone wrap through lifetime exhaustion. Existing-session completion
and ACK still work at allocation exhaustion. Simultaneous pending entries and
out-of-order consumed-but-retained outcomes are covered; decode refuses a
below-4-MiB index whose projected progress budget exceeds capacity.

Windows source and new tests were cross-typechecked in a narrow source harness;
genuine Windows execution is required in CI before merge. No Windows stable host
or production inventory adoption is claimed. No live session was inspected,
restarted or migrated.

```text
cargo test -p remuda-native session_host::inventory --lib --offline
24 passed; 0 failed; 1 ignored
cargo clippy --workspace --all-targets --offline -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 5.95s
cargo fmt --all -- --check
git diff --check
```

Refs #713; this checkpoint does not complete session-preserving upgrades.
