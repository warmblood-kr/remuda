# 055 — write human raw input through the physical ticket

## Before

Step054 negotiated the Human role and adopted the core grant CAS, but raw
human bytes had no OS write path; raw outcomes existed only as model fixtures.

## Desired outcome

`HumanInput { grant, sequence, bytes }` (Human role, 1..=8 KiB) reserves one
raw outcome and the host's single physical ticket together under the
coordinator lock, or neither. A worker writes the bytes once through a clone
of the exact current core guard (`write_raw_while_started`), so a takeover or
revoke cannot drop the guard mid-write. Delivery is Written on success,
Rejected only when the backend never reported starting, and Indeterminate
otherwise. The ticket finishes only after the backend writer is idle again.
Worker spawn failure and a missing guard are proved pre-effect and complete
Rejected. Retries of a retained sequence answer evidence and never replay.

## Expected

- One write lands exactly once; a same-bytes retry returns Complete(Written)
  with no second effect; changed bytes conflict; oversize input refuses.
- After takeover the old grant's new sequence is Stale; only the new holder
  writes.

## Actual

```text
cargo test -p remuda-native --lib human_wire   (8 repeated runs)
test result: ok. 5 passed; 0 failed
cargo test -p remuda-native --lib session_host
test result: ok. 125 passed; 0 failed; 4 ignored; finished in 67.05s
cargo test -p remuda-native --test session_host_process
test result: ok. 8 passed; 0 failed
cargo clippy --workspace --all-targets -- -D warnings: Finished
```

Synthetic PTY races (admission vs Stop, takeover mid-write, lost replies,
backpressure) belong to the dedicated tester increment. No production
adoption. Refs #713.
