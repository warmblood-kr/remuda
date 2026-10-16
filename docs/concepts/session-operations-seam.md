# Checked operations seam for #713

core::Session remains the existing Local input/human/reaper owner. The new
session_operations::SessionHandle contains its exact Arc or a checked Remote
operations provider, immutable host/session/name/instance and stable client ID
metadata. It has no AgentProcess, input/human lock, pending tail, deduplicator,
rate budget, reaper, Clock or monitor thread. Local-only APIs are available only
through as_local()->Option<&Arc<Session>>; Remote returns None. No Deref or
conversion into Local exists.

Remote construction is test-only/private in this increment. Registry::register
still accepts Session, so a SessionHandle/Remote cannot enter legacy bool/Option
list/prune paths. Compile-fail doctests enforce both boundaries. There is no
native Remote provider, daemon selection, registry recovery or production
adoption here. Existing Local APIs/wire behavior remain unchanged.

SessionOperations describes full send/send_line/input_text/type_text/submit/clear/
feed/batch acts, resize, explicit terminate, text/versioned snapshots/history and
output wait/wakeup, and checked attachment. Feed pauses and TypeText settle are
passed intact. Replies preserve Late/Unverified, clear text, batch uncertainty
and underlying Agent/Input errors. A raw local stream is not a Remote API;
coalesced wakeups require snapshot/version reads, not byte replay. Future native
RPC remains separately versioned; this Rust trait is not its wire ABI.

Remote inspect returns one host-cut observation including geometry, version,
durations, mouse, PID/lifecycle and attachment facts. Unavailable(reason) is an
error, never Exited/default geometry/stale success. LocalFields explicitly keeps
legacy separately locked/non-atomic observations; it does not pretend they form
a host cut. Changed identity or a Remote observation without a host cut refuses.
Snapshots/wait results must carry the retained instance. Geometry/payload/result
bounds remain the native host/provider's checked transport/terminal responsibility.

Unknown input errors carry boxed RequestIdentity(host/session/instance, stable
client, sequence, payload binding), distinct from pre-admission unavailability.
Recovery is lookup-only: Pending, Complete(delivery + rich reply), Forgotten,
Conflict or OutOfOrder. Written means OS write/flush, never application execution.
No automatic replay, replacement client/sequence or request allocation occurs in
the facade. The native provider must retain original bounded payload/context
durably with owner launch state; this increment supplies no such persistence or
new ledger. A Local owner has no host input recovery capability.

Attachment guards carry immutable identity and positive host-issued generation.
Failed attach changes no local human timers or cached attachment state. The
provider owns displacement/checked detach; dropping an old generation cannot
release its successor. Raw write cancellation/start callback is passed intact.
Drop/disconnect is separate from explicit terminate. Local delegation uses the
same original guard: ordinary orchestrated input remains allowed while attached,
termination refuses attached, and clear respects recent/inflight human input.

Mock tests cover full-act forwarding/rich results, coherent reads, unavailable/
changed identity, failed attach/no human effects, stale detach/raw write, recovery
identity refusal and 100 facade/provider rebuilds using one mock owner/request
without another input or terminate. These are interface tests, not a durable
store, live RPC/host crash, ledger-capacity or actual monitor-cleanup proof.
All legacy core tests still run unchanged. Real native adapter/reload/host-ledger
and resource tests remain required.

Next dependency order: complete host operations/attach/history; checked inventory
and revisioned membership cut with bounded concurrent inspections outside registry
locks; reservation/recovery before config/Butler reconcile; authenticated native
host ownership/principal bridge; companion Butler metadata/mail/Matrix recovery;
mixed-version capability/downgrade handling; reviewed opt-in future-session
adoption. Per-entry unavailable rows retain identity and explicitly stale last
observation; whole Recovering/Unavailable must never become authoritative empty.
No production restart or rescue of existing daemon-owned sessions is implied.
