# 084 — Bound document transport and recover retries

## Before

The document worker calls request_with_timeout, whose synchronous connection
precedes its response timer. A blocked connect retains Browser.flight and
prevents retry, although the UI and generation fence continue working.

## Desired outcome

On Unix, document connect, write and read share one three-second monotonic
deadline. A finished error releases single-flight capacity for retry. No
detached connector/request workers accumulate. Existing session/input callers
retain their behavior. Windows document browsing explicitly returns Unsupported
before connection; Windows support is a separate follow-up, without a claimed
three-second guarantee for its existing request_with_timeout paths.

## Expected

Use an opt-in client API, nonblocking Unix connect with a finite wait and
nonblocking writes/reads checked against the original deadline. Check the peer
after connection readiness because Linux AF_UNIX queue saturation can return
EAGAIN without creating a connection. Retain response wire limits and parsing.
The document Browser keeps one worker and one latest pending navigation; the
new client API creates no threads. Expiry closes its socket, not daemon work.
This is an I/O deadline subject to OS scheduling, not a real-time CPU bound on
arbitrary serialization or VFS decoding.

## Actual

The opt-in request_with_deadline API is used only by documents. Existing
request_with_timeout and general ipc::connect are unchanged. Blocked connector
seams, exhausted/partially spent budgets, a silent reader, real listener queue
saturation and capacity restoration are covered. Browser retries coalesce
behind a retained flight and proceed after it settles; stale replies stay
fenced. The PTY path now waits for an actual deadline error, resumes the private
daemon, retries and reads again while retaining paste/session isolation.

```text
env TMPDIR=/private/tmp cargo test --offline -p remuda-native --lib client::deadline:: -- --test-threads=1
test result: ok. 6 passed; 0 failed

cargo test --offline -p remuda-native --lib documents:: -- --test-threads=1
test result: ok. 5 passed; 0 failed

env TMPDIR=/private/tmp cargo test --offline -p remuda-native --test cluster_tui -- --test-threads=1
test result: ok. 8 passed; 0 failed; finished in 34.14s

env TMPDIR=/private/tmp cargo test --offline -p remuda-native --test cluster_tui documents_browse_plain_text_and_return_without_sending_browser_input -- --test-threads=1
test result: ok. 1 passed; 0 failed; finished in 12.44s (final connection check)

cargo check --offline -p remuda-native --lib --target x86_64-pc-windows-gnu
Finished `dev` profile in 18.17s (7 warnings in existing Windows code)

cargo clippy --offline --workspace --all-targets -- -D warnings
Finished `dev` profile in 3.51s
```

Execution evidence is macOS only. Windows cross-check compiled the Unsupported
branch; it did not run Windows code. Linux queue behavior is guarded by a peer
check and a portable saturation test, but has not been executed on Linux here.
The separate path-label commit escapes controls/bidi characters and literal
backslashes while preserving navigation identity; body text policy is retained.
