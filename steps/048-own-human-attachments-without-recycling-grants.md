# 048 — own human attachments without recycling grants

## Before

Attached borrows Session, so a stable host cannot retain its existing guard in a
connection without a self reference or extra owner. Generation fetch_add wraps;
the next-generation-minus-one observer cannot represent a final u64::MAX grant.

## Desired outcome

Add an Arc-owned guard retaining the exact Session. Borrowed and owned guards
must share the authoritative slot, human clocks/inflight count, raw write and
checked drop implementation. Refuse generation exhaustion before displacing the
holder or changing attachment state. Keep successful legacy behavior unchanged.

## Expected

Use one generic Attachment with borrowed/Arc aliases and private fields; no
unsafe lifetime extension, second Session, slot or input state. Track the last
allocated generation, initialized zero. Under the existing slot mutex, check
increment before changes; grant the final nonzero generation once. Observer
comparisons use the actual high-water mark. Session::attach returns Result;
the existing daemon caller returns its error before acknowledgement, subscription
or raw stream setup. SessionOperations propagates errors for both guard forms.

Prove owned lifetime and exact identity, borrowed/owned takeover and stale drop,
concurrent ordinary/final grants, failed-grant preservation of slot/flags/timers/
inflight, and already admitted owned writes finishing once after takeover. Run
legacy core and private synthetic daemon/PTY attachment regressions.

## Actual

Seven new core tests pass. The full core suite passes (47 unit tests and 90
integration tests, plus two compile-fail doctests) on main ae9ce951. Existing
private daemon takeover and same-read input/detach tests pass, as do three
synthetic PTY repaint/mouse-mode/detach tests. No real agent/live pane or running
production daemon was read, written, restarted or migrated.

```text
test session::attachment_tests::concurrent_exhaustion_grants_the_final_generation_to_only_one_holder ... ok
test session::attachment_tests::owned_inflight_write_finishes_once_and_stale_drop_preserves_borrowed_successor ... ok
test result: ok. 47 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.53s
test a_new_attach_takes_over_and_old_raw_clients_get_a_plain_notice ... ok
test attach_keys_before_detach_in_one_read_reach_the_child ... ok
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 6 filtered out; finished in 0.64s
cargo clippy --workspace --all-targets --offline -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.12s
```

This prepares ownership only. It adds no host attachment RPC, lost-grant-ACK
recovery, raw-input request ledger, terminal stream/history, Registry adoption or
rollout. A takeover retains the documented possibility that one already admitted
buffer finishes; it is never automatically replayed. Actual Windows CI remains
required for this head. Refs #713.
