# 054 — negotiate human input and adopt the core grant CAS

## Before

The host coordinator modeled human grants and raw outcomes (step052), and core
gained a checked attach-slot grant (step053), but nothing connected them. No
client could open a human connection, and the host's grant counter was separate
from core's attachment high-water.

## Desired outcome

- New capability `human-input-v1` and role `Human`. A Human open is refused
  unless the client listed that capability as required, so a grant never
  reaches a host lacking these semantics. Human, like Control, must name the
  exact expected host identity. View stays terminal-only.
- `HumanGrant` carries a client recovery secret; only its SHA-256 enters the
  grant request. The coordinator answers the latest exact receipt as evidence
  first; otherwise core's `attach_owned_expected` runs under the coordinator
  lock (coordinator, then core slot) and its generation becomes the grant.
  The host keeps the core guard; displaced guards drop after the lock.
- Revoke and discard need the live controller fence; ACK needs the human
  holder; raw recovery and cuts are visible to Control or the current holder.
  Disconnect and revoke release the exact core guard.

## Expected

- Human without the negotiated capability, View and Control cannot grant.
- Grant expectations are checked against core's high-water; a lost-ACK retry
  from another connection is evidence, never a transfer; changed secret conflicts.
- Revoke releases the core attachment; discard refuses a live grant.

## Actual

```text
cargo test -p remuda-native --lib human_wire
test result: ok. 3 passed; 0 failed
cargo test -p remuda-native --lib session_host
test result: ok. 123 passed; 0 failed; 4 ignored; finished in 65.89s
cargo test -p remuda-native --test session_host_process
test result: ok. 8 passed; 0 failed
cargo clippy --workspace --all-targets -- -D warnings: Finished
```

No raw OS writes yet (raw outcomes in tests are synthetic); the physical write
path is the next increment. No production adoption. Refs #713.
