# 056 — prove human raw races on a synthetic PTY

## Before

Step055 wrote human raw input through the physical ticket, but its tests
exercised one admission at a time on a PTY that always drained. Nothing
raced admission against Stop's latch, held a write physically in flight while
a takeover, disconnect or controller act arrived, or drove the 256 raw-outcome
cap and connection churn through the real wire.

## Desired outcome

Tests only, no production behavior change (one `#[cfg(test)]` module line in
`owned.rs`). `race_tests.rs` adds two layers:

- Coordinator races: contenders start together on a `Barrier`; a rotating
  skew forces each interleaving order; 500 rounds per test.
- Wire races: a `/bin/sh` child switches to raw mode and reads nothing until a
  GATE file exists, so a full 8 KiB human or controller write stays blocked on
  the PTY (`writer.is_busy()`). Effects are counted exactly from an EFFECTS
  file; a final sentinel write proves no replay, since the PTY is FIFO.

## Expected

- Stop Ok leaves no admitted act, grant or raw ticket; a lost Stop reply then
  reconnect and retry answers the same refusal or `host_stopping`, never an effect.
- A takeover mid-write is granted, but the new holder gets Busy until the old
  ticket drains; the old ticket completes exactly once; resize/close/input are
  `controller_busy`, and a controller act makes human input Busy.
- A disconnect with a late writer keeps the outcome recoverable; retries return
  evidence, never a replay.
- At 256 outcomes Busy spends no sequence and evicts nothing; an exact one-entry
  ACK frees exactly one slot.
- 300 reconnect/grant rounds leave one connection and no core guard.

## Actual

```text
cargo test -p remuda-native --lib race_tests   (20 repeated runs, 20 passed)
takeover race: handed off 270..296, busy 204..230 of 500
stop race: latched 167..173, refused 327..333 of 500
capacity race: admitted first 174..186, after ack 314..326 of 500
wire stop race: stopped 5..6, granted 3..4 of 9
PTY write blocked at sequence 1   (macOS; Linux fills until one blocks)
test result: ok. 11 passed; 0 failed; finished in 1.66s
cargo test -p remuda-native --lib session_host
test result: ok. 137 passed; 0 failed; 4 ignored; finished in 66.34s
cargo test -p remuda-native --test session_host_process
test result: ok. 8 passed; 0 failed; finished in 3.87s
cargo clippy --workspace --all-targets -- -D warnings: Finished
```

No bug found. No production adoption, Windows, replace/rollback or daemon
routing. Refs #713.
