# 043 — introduce a checked whole-session operations seam

## Before

The stable host prototype owns a complete Session, but a controller adapter
wrapped in another Session would create a second input/human/dedup owner. Legacy
Session and Registry bool/Option APIs cannot express an unavailable remote owner.

## Desired outcome

Add an inactive checked high-level handle that reuses the exact Local Arc owner
or a Remote operations provider, with full acts, typed unavailable/unknown input,
retained identity/recovery and host attachment generations. Keep the legacy
Registry Local-only by type and prohibit production Remote construction until
verified identity, checked inventory and all consumers are implemented.

## Expected

Forward Feed including pauses and TypeText including settle once, retain rich
results, refuse unavailable/mismatched reads and failed attachments without human
effects, preserve successor attachment after stale drop, and reuse one mock
owner/request over100 adapter rebuilds without replay/terminate. Local uses the
same Arc and human gate. Compile-fail examples prove no legacy registration or
production Remote construction. Existing core tests and workspace lint pass.

## Actual

The first9 new seam tests and all existing core tests pass; the legacy owner
implementation and Registry are unchanged. The mock backend has no Session or
AgentProcess. Its recovery test supplies a stable shared context; it does not
establish durable native persistence or real host-ledger behavior.

```text
cargo test -p remuda-core --all-targets
test result: ok. 37 passed; 0 failed
test result: ok. 14 passed; 0 failed
test result: ok. 36 passed; 0 failed
test result: ok. 10 passed; 0 failed
test result: ok. 5 passed; 0 failed
test result: ok. 8 passed; 0 failed
test result: ok. 1 passed; 0 failed
test result: ok. 15 passed; 0 failed

cargo clippy --workspace --all-targets -- -D warnings
Finished `dev` profile

cargo test -p remuda-core --doc
test result: ok. 2 passed; 0 failed
```

The total is126 core tests, including9 new seam tests, plus both boundary
compile-fail doctests. No native Remote adapter, listing migration or production
selector is implemented by this checkpoint.
