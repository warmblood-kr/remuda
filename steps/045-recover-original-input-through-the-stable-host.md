# 045 — recover durable original input through the actual stable host

## Before

Private inventory survives controller death, and a standalone host survives its
controller, but no bridge enforces durable reservation before authenticated host
admission. Rebuilding an act from caller intent after a lost ACK can duplicate it.

## Desired outcome

Connect one process-level durable owner to the real Unix host protocol. Retain
the exact client/sequence/payload/intent before contacting the host; recover the
original request after caller death, while keeping the host as the sole physical
effect owner. No daemon/Registry selector or production queue adapter is enabled.

## Contract

An Owner claims a durable writer fence once across sessions. Reserve commits the
original full typed act before returning its receipt. Exact retries while pending
or retained return the original key; changed payload or intent refuses. Unknown
commit never sends. Caller death before receipt can recover the key from the
persisted pending reservation. Stable clients must survive controller replacement;
allocating one per reconnect would exhaust the host's sixteen lifetime clients.

Resume adopts the reservation under the durable fence, opens with stored expected
identity rather than a server echo, and acquires the independent physical lease.
Prior connections and entire admitted acts must drain first. Recover queries the
original key/payload before any possible Input. Pending never resends; complete
is bounded and persisted before returning; forgotten/conflict/out-of-order stay
distinct. Transport/lease failure preserves Unavailable rather than empty/exit.

Only authenticated New from the same immutable host, under an exclusive physical
lease and its monotonic nonrecycled ledger, proves that original request was never
admitted. It can resolve unresolved or transport-uncertain state using the exact
same request. Observed Admitted followed by New is a contradiction and remains
Indeterminate. Host restart/ledger loss must change incarnation; it cannot turn a
prior possible OS effect into safe replay. There is no exactly-once guarantee
across effect and lost host ledger, nor any application-consumption receipt.

One unresolved physical effect is distinct from Butler's bounded durable FIFO
intent queue. The eventual adapter must persist Butler's consumed-result decision
before releasing Native retention; their separate stores are not one transaction.
Scoped host lifecycle publication, birth ordering, adoption recovery barrier,
human attachment/history and actual Windows host remain subsequent work.

## Expected

Kill only private owned controller processes at deterministic reserve/admission
cuts, including during a full Feed after its first effect. Fresh disk owners keep
the same child PID/identity and one original physical act through 100 rebuilds.
Lost reserve/completion ACKs reuse the key; stale writer, changed identity, live
physical lease, uncertain commit and contradictory admission cannot create effects.
Recovery additionally requires the explicit retained-input-ledger-v1 capability;
an authenticated older host lacking it receives no Acquire or Input request.

## Actual

Seven new recovery tests and one authenticated capability test pass. Three kill actual separate controller processes;
the stable real PTY/host server lives in the parent test process. Admission cuts
trigger only after an actual Admitted response, before inventory observation. The
Feed case observes its first effect before killing the controller, then its
second effect completes once. Existing executable-host process fixtures separately
continue to cover standalone host lifetime and terminal-state preservation.

```text
cargo test -p remuda-native session_host::recovery --lib --offline
7 passed; 0 failed; 1 ignored
cargo test -p remuda-native --test session_host_process --offline
7 passed; 0 failed; 0 ignored
cargo clippy --workspace --all-targets --offline -- -D warnings
Finished `dev` profile
```

The ignored helper runs only when its parent invokes it with a private fixture
root. No real Claude/Codex, live pane, production restart or migration was used.
Refs #713; this remains a reviewed integration increment, not full adoption.
