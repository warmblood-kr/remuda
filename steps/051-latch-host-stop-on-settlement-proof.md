# 051 — latch host Stop on the settlement proof

## Before

The experimental host Stop checked publication Complete, then a boolean
"no unconsumed retention" read, then quiescence through an empty mutation. The
retention read was not bound to the exact immutable lease identity, and nothing
prevented a later acquire, mutation or input admission after Stop said Ok but
before the server loop exited.

## Desired outcome

Stop consumes the step050 `Settled` evidence. One store read, outside every
host lock, constructs the proof for the host's own lease and confirms it
against the running host's actual identity. Busy retention still answers
`host_retention_pending`; any other proof failure, or an exit generation newer
than the live terminal, answers `host_settlement_unavailable`. The live child
must still report Exited with output EOF.

The final transition is one coordinator-locked latch: only the current idle
controller fence can take it, an active act refuses it as Busy, and a human
holder refuses it. After the latch, acquire, mutation and input admission
refuse forever with `host_stopping`; outcome recovery stays readable.

## Expected

- A stale fence or an active act cannot latch Stop.
- After the latch, no new controller, resize/close or input can be admitted.
- Retained outcomes remain recoverable after the latch.
- Existing retention-pending and consumed-then-Stop process paths are unchanged.

## Actual

```text
cargo test -p remuda-native --lib session_host::coordinator
test result: ok. 4 passed; 0 failed
cargo test -p remuda-native --lib session_host
test result: ok. 112 passed; 0 failed; 4 ignored; finished in 66.12s
cargo test -p remuda-native --test session_host_process
test result: ok. 8 passed; 0 failed; finished in 3.83s
```

Human input authority (checked CAS grants, bounded raw outcomes) will admit
through the same coordinator and therefore inherit the latch. No production
adoption, daemon routing, restart or migration. Refs #713.
