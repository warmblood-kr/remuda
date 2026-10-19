# 057 — a host-backed session operations adapter

## Before

The stable host (`remuda-session-host`) owned a PTY, parser, input ledger and
controller lease behind its private protocol, and the core
`SessionOperations` seam (step043) defined whole-session operations. But
only a test mock implemented that seam remotely. Nothing on the daemon side
could drive a host as a session, so a daemon could not yet be replaced
underneath one.

## Desired outcome

`session_host::remote::HostSession` implements `SessionOperations` over one
host. It takes the expected identity and endpoint only from the private
inventory, and connects only through `entry::Client`. That path checks
modes, peer UID, the PSK handshake and the exact identity.

- Input acts (`send`, `send_line`, `input_text`, `type_text`, `submit`,
  `clear_input`, `feed`) are reserved durably under the owner fence before any
  effect. They are driven through the host ledger by their original key, and
  a consumer receipt retires the result after the caller gets it.
- An unresolved act refuses new input with `Indeterminate(request)` until
  `recover` resolves that exact key. A retired key answers `Forgotten`. Input
  is never replayed.
- After the reservation, any store or host failure is `Indeterminate`. Only
  lock or revision contention is retried in place. A host error that may have
  written (delivery not `Rejected`) stays recoverable evidence, not a clean
  refusal.
- Retention is bounded: once 16 results are unreceipted, the oldest ones are
  retired before a new reservation. They then answer `Forgotten`.
- A published exit refuses new input with `Exited` before any reservation.
  Stopping or unpublished hosts are `Unavailable`, never an exited session.
- `resume` skips its Adopt commit only while this owner's exact fence (store
  incarnation, epoch, nonce) is current.
- `inspect` returns one host Inspect cut. Session activity (idle times,
  attached, closing, mouse) comes from a new additive `activity` field,
  negotiated as `activity-v1`.
- `snapshot`, `screen_text` and `wait_after` use acknowledged typed viewports
  and the bounded watch. `resize` and `terminate` take the controller lease.

Out of scope for now: `attach` and `apply_input_batch` return
`Unavailable(Unsupported)`, and `wakeup` returns `None`. `Registry` and
daemon routing do not select the adapter (increment D). Windows (#214) and
host replace/rollback stay out.

## Expected

- Whole acts, reads and resize go through one separate host process; a
  consumed key recovers as `Forgotten` with no second effect.
- An old daemon is SIGKILLed mid-act (`feed` first, a 1.5 s pause, second).
  Afterwards the new owner sees the same child pid running, gets
  `Indeterminate` for new input, and recovers the original act as
  `Written`. Each line lands exactly once.
- A newer owner claim fences the old adapter before any effect.
- An old daemon is SIGKILLed after its durable reservation but before any
  RPC. The new owner recovers that exact act, which lands once.
- A newer claim also fences `resume` of an act the old owner already
  reserved. Only the current owner then resolves it, once.
- 20 possibly-written failures all stay `Indeterminate`, with sequences 1 to
  20. Exactly 20 - 16 are retired as `Forgotten`.
- An exited session refuses new input; acts that raced the exit resolve by
  key.

## Actual

```text
cargo test -p remuda-native --lib session_host::remote::tests   (10 repeated runs)
test result: ok. 8 passed; 0 failed; 1 ignored; finished in 8.97-9.09s
cargo test -p remuda-native --lib session_host
test result: ok. 145 passed; 0 failed; 5 ignored; finished in 65.95s
cargo test -p remuda-native --test session_host_process
test result: ok. 8 passed; 0 failed
cargo test -p remuda-core session_operations
test result: ok. 9 passed; 0 failed
cargo clippy --workspace --all-targets -- -D warnings: Finished
```

The host in these tests is a separate process running the same
`run-reserved` code path as `remuda-session-host`. The old daemon is a
separate process killed with SIGKILL. No live daemon, installed binary or
real agent is involved. Refs #713.
