# 053 — checked attach-slot grant against the high-water

## Before

Core attachment grants always displaced the current human holder. A host that
must arbitrate human takeover by compare-and-swap had no way to say "grant only
if nobody was granted since generation N", so a stale or duplicated request
could silently take over a newer holder.

## Desired outcome

`Session::attach_owned_expected(expected)` compares the shared attachment
high-water with `expected` and advances it inside the authoritative slot lock.
A mismatch returns `AttachRefusal::Stale { current }` and displaces nothing;
exhaustion returns `AttachRefusal::Exhausted` before any change. The unchecked
`attach`/`attach_owned` grants advance the same counter, so they stale every
older expectation. `attach_high_water()` reads the counter under the same lock.

## Expected

- A wrong expectation refuses without displacing the current holder.
- An unchecked grant stales an expectation taken before it.
- Exhaustion leaves the holder and counter unchanged.
- Concurrent grants with the same expectation admit exactly one.

## Actual

```text
cargo test -p remuda-core --lib attachment_tests
test result: ok. 11 passed; 0 failed
cargo test -p remuda-core
10 test binaries ok; 0 failed
cargo test -p remuda-native --lib session_host
test result: ok. 111 passed; 0 failed; 4 ignored; finished in 65.27s
cargo clippy --workspace --all-targets -- -D warnings: Finished
```

Core primitive only: the host coordinator's grant model (#747) and wire adopt
it in later increments. No production adoption. Refs #713.
