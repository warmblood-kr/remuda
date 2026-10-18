# 052 — model human input authority in the host coordinator

## Before

The host coordinator knew only a controller fence. Raw human input had no
checked grant, no host-wide outcome sequence and no bounded retention, so an
idle controller and a human typist could not coexist safely, and Stop could
not tell whether a human outcome was still unretired.

## Desired outcome

A pure host-local model under the existing coordinator lock, following the
reviewed human-authority design:

- One grant at a time from a checked CAS against the grant high-water. The
  latest exact request answers evidence (Current/Superseded/Released) and never
  transfers a grant; a changed request with the same ID conflicts; an old
  expectation is Stale forever; exhaustion fails before any change.
- One global raw sequence for the host. Retained exact retries answer before
  the grant check; old or displaced grants never admit New. At most 256
  unretired outcomes; capacity is Busy, spends nothing and evicts nothing.
- One physical barrier: a human raw ticket excludes every controller act and
  vice versa. Takeover lets the admitted ticket drain; completion names the
  exact sequence, never the current holder.
- Retirement is an exact canonical cut from retired-through+1 over one grant's
  completed prefix: the current viewer ACKs its own cut; management discards an
  inactive grant's cut under its fence. FIFO: an abandoned older cut blocks a
  successor's prefix until discarded. One latest retirement receipt replays
  only the identical kind and cut; older answers Forgotten.
- Stop's latch also refuses a live grant or any unretired raw outcome.

## Expected

- Same request from another connection is evidence, never a transfer.
- Busy at capacity does not spend a sequence; retired sequences never become New.
- Wrong hash, wrong viewer, pending entries and mixed grants refuse retirement.
- Controller acts are Busy while a human ticket runs; takeover cannot start a
  second writer.
- Stop refuses until the human grant is revoked and its cut retired.

## Actual

```text
cargo test -p remuda-native --lib -- session_host::coordinator session_host::human
test result: ok. 12 passed; 0 failed
cargo test -p remuda-native --lib session_host
test result: ok. 120 passed; 0 failed; 4 ignored; finished in 66.74s
cargo test -p remuda-native --test session_host_process
test result: ok. 8 passed; 0 failed; finished in 3.90s
```

No wire messages, core attachment CAS, OS writes or role negotiation yet; those
are the next increments. Synthetic model only; no production adoption. Refs #713.
