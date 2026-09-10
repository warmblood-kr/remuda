# 024 — a failed check is not a passed one

## Before

`version_skew` (`native/src/bin/remuda.rs`) warns a person once, up front,
when the daemon they are about to talk to is not the build they are
running — `steps/013`'s handshake. It first confirms *something* answers a
connect at all (`None` if not — nothing to warn about, a fresh daemon of
this build starts next), then sends `Request::Version` and decides:

```rust
let theirs = match remuda_native::client::request(path, &Request::Version) {
    Ok(Response::Value(theirs)) if theirs == dist::VERSION => return None,
    Ok(Response::Value(theirs)) => theirs,
    Err(_) => return None,
    Ok(_) => "from a build that predates this handshake".into(),
};
```

`Err(_) => return None` reads a *failed* version check the same as a
*confirmed match*. The comment justifying it — "it went away between the
connect and the ask; the real request will start one and say so properly"
— is only true for the one case it names: the daemon fully exited. But the
connect just above and `client::request`'s own request/response round trip
are two separate operations (`client::request` opens its own fresh
connection internally); a daemon that is still alive and still mismatched
can fail *this specific* round trip on a transient hiccup — a slow accept,
a momentary transport blip, the kind of thing Windows named pipes are
already known in this project to behave differently about than a Unix
socket. When that happens, the very next thing the program does
(`with_daemon`'s own connect check) can succeed against that same,
still-alive, still-mismatched daemon a moment later — and the person gets
no warning at all, because the failed check already told the code "no
skew," which is not what it observed.

`interpret()` (`native/src/client.rs`) already turns the two *graceful*
failure shapes — an empty reply, a reply that will not parse — into a
worded `Ok(Response::Error(...))`, which `version_skew`'s `Ok(_)` arm
already treats as a skew notice (imprecisely worded, but not silent). The
`Err(_)` arm is reached only by lower-level transport failures: the
request's own internal connect failing, or a raw I/O error writing or
reading the request/response line. Those are exactly the failures a flaky
or loaded transport produces without the daemon ever having left.

## Desired outcome

A version-check failure must say something distinguishable from a
confirmed match — not a different silent fallback, the daemon's own
words where there are any, and an honest "could not tell" where there
are not.

## Expected

`skew_notice`, split out of `version_skew` so the decision is testable
without a socket, turns any `Err` into `Some(...)` naming `remuda restart`
as the cure, instead of the prior `None`. The two already-correct arms
(confirmed mismatch, predates-handshake) are unchanged.

## Actual

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result"
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 68 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.09s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s

$ cargo fmt --all --check                                    (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings       Finished, no warnings
$ cargo check -p remuda-core --target wasm32-unknown-unknown  Finished, clean
$ python3 scripts/check-comments.py     ok — 249 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-steps.py        ok — 23 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-principles.py   ok — 14 principles, every named mechanism exists (17 CI jobs, 11 denied paths, 254 test fns seen)
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse, 17 job(s) defined
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
```

`native/src/bin/remuda.rs::version_skew_tests::a_failed_version_request_is_not_silently_no_skew`
calls `skew_notice(Err(...))` directly — no socket, no daemon, no
fixture — and fails on the old code (`Err(_) => return None` would make
this assert `None.is_some()`, false) and passes on the new.

No CI gate was added, no new denied path.

## Not verified

- **Whether this exact race has ever actually fired for a real user.** The
  scenario (connect succeeds, the subsequent request round trip fails,
  the daemon is still alive moments later) is real and reachable by the
  code's own structure, not manufactured — but no report names it.
- **Windows.** No Windows host is available; the named-pipe transport
  characteristics that make this more plausible there than on a Unix
  socket are cited from earlier findings this session, not measured here.
- Two items explicitly out of scope for this step, unchanged: the list
  column's own `char`-based width math for session names (`steps/023`),
  and Ambiguous-width Unicode (`steps/023`).
