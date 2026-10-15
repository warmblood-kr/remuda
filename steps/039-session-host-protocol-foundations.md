# 039 — bounded foundations before moving session ownership

## Before

The daemon owns PTY/parser/child/job and core Session input safety. Replacing it
cannot preserve those resources. Issue #713 records a reviewed ownership split,
but no separate host protocol exists. Existing raw subscriptions and default
parser OSC storage are not bounded by retained scrollback. This increment does
not change those runtime paths or imply they are safe for adoption.

## Desired outcome

An explicitly inactive internal milestone provides bounded framing, typed whole
input acts, connection/incarnation-bound controller epochs, stable input outcome
records and subscriber snapshot/resync sequencing. Review these before a usable
isolated host and high-level Session backend seam. No daemon restart, production
session migration or upgrade-preservation claim is authorized by this increment.

## Expected

Oversized frames reject before payload read/allocation. Controller takeover stays
Busy through an entire admitted act; a delayed duplicate completion cannot release
a newer reservation. Client identities never disappear to permit old input replay.
Evicted outcomes refuse replay. Subscriber overflow while awaiting snapshot ACK
invalidates the snapshot, queued deltas and stale ACK. Limits are deterministic
and tested without real agents, sockets or PTYs. Host auth/parser/runtime bounds
and full operation result retention remain required before integration.

## Actual

Review found that finishing by controller fence alone released a later act. A
synthetic negative control reproduced that defect before opaque ActId was added:

```text
test session_host::control::tests::delayed_completion_cannot_finish_a_later_act ... FAILED
assertion `left == right` failed
  left: Ok(())
 right: Err(Stale)
```

After the fix, targeted pure tests pass:

```text
cargo test --offline -p remuda-native --lib session_host
running 11 tests
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 1103 filtered out; finished in 0.01s
cargo clippy --offline --workspace --all-targets -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.51s
```

No host was launched and no live daemon was contacted. These results prove pure
sequencing/limit behavior only. The complete owner task remains open in #713.
