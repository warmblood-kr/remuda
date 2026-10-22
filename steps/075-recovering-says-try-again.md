# 075 — A refusal during a reload says to try again

## Before

While a reload drains the old daemon, input to a hosted session is refused
with "agent io error: session owner unavailable: Recovering". It is safe to
retry, but the text does not say what is happening or what to do.

## Desired outcome

The refusal names the cause and the next step: "session owner unavailable:
Recovering (a reload or restart is moving this session; try again)".
The text up to "Recovering" stays the same, so callers that match it keep
working. Other `Unavailable` reasons read as before.

## Expected

`core/src/session_operations_tests.rs`:
`a_recovering_refusal_says_to_try_again` checks the prefix, the "try
again" hint, and that `Transport` is unchanged.

## Actual

```
$ cargo test -p remuda-core --lib a_recovering_refusal
test result: ok. 1 passed; 0 failed

# red: before the change
assertion failed: text.contains("try again")
session owner unavailable: Recovering
```
