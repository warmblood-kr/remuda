# 021 — a discarded error is a hidden one

## Before

정수님's TUI session pane went blank on a nightly containing `steps/020`'s
styled capture. `remuda restart` fixed it — session and colour both came
back — confirming the cause was a daemon from an older build than the
client talking to it. That is one incident, but the code defect it exposed
is general, and this step is about the defect, not the incident.

`steps/013` built `version_skew`: on a build mismatch, the daemon's own
reply is turned into a named diagnosis (*"the daemon is not this build —
run `remuda restart`..."*) and put where a TUI user reads it, because
`eprintln!` is lost under the alternate screen. That mechanism is intact
and does its job — it produced exactly that footer line tonight.

But two functions the pane actually calls never got to it. `native/src/
tui.rs`'s `list` and `capture_styled` each did:

```rust
match client::request(path, &request) {
    Ok(Response::Sessions(sessions)) => sessions,   // or StyledScreen(cells)
    _ => Vec::new(),
}
```

A `Response::Error` — the daemon's own words, already computed correctly,
already worded usefully by `client::request`'s `interpret()` layer for the
skew case — fell into `_` and became an empty `Vec`, identical on screen to
"this session has produced no output" or "the herd has no sessions". The
comment on `capture_styled` said as much outright: *"Empty grid on any
error, matching `capture`'s empty-string convention."* `capture()`'s
original convention (String, not `Vec`, and long dead) was inherited
verbatim by `steps/020`'s replacement, onto the one call path the pane now
depends on entirely.

This is not a missing error path. Every layer below the client did its
job — the daemon sent a specific, well-formed reason. The top layer had the
value and threw it away.

## Desired outcome

1. `list` and `capture_styled` return `Result<T, String>`, matching the
   shape `start`/`kill` already use in the same file: `Ok(Response::Error(reason))
   => Err(reason)`, and any other non-matching reply or transport failure
   formatted rather than discarded (`other => Err(format!("{other:?}"))`).
   No new type, no error hierarchy — the pattern already lives here twice.
2. The failure reaches the screen through the channel that already exists
   for exactly this: `Ui::notice`, rendered in the footer, which already
   carries a hold refusal, a start/kill failure, and the version-skew
   notice itself. A transport or protocol failure on `list`/`capture_styled`
   sets it the same way.
3. A herd or a screen that fails to load is visually distinguishable from
   one that is genuinely empty: `list`'s failure now keeps the last known
   herd rather than blanking it, and reports why in the footer; a capture
   failure leaves the pane as it was and names the session and the reason
   in the footer, rather than painting either as silently empty.
4. Tests exercise `list` and `capture_styled` themselves — the functions the
   pane's own `refresh()` calls — not a bypassed stand-in, so a passing test
   actually proves something about the code the pane runs.

## Expected

- `tui::tests::list_reports_a_transport_failure_instead_of_an_empty_herd`:
  no daemon listening, `list(&path)` returns `Err`, not `Ok(vec![])`.
- `tui::tests::capture_styled_reports_the_daemons_own_words_instead_of_an_empty_grid`:
  a real daemon (`daemon::serve`, the same function the shipped binary
  runs), asked for a session it never created, returns its own real
  `Response::Error("no such session: ...")` — the test asserts that exact
  wording survives to the caller, not a generic replacement.
- The full existing suite passes unchanged; nothing about `render`, `crop`,
  the plain `Capture` path, or `steps/020`'s styled-crop oracle is touched.

## Actual

```
$ cargo test -p remuda-native --lib tui:: -- --nocapture 2>&1 | tail -6
test tui::tests::capture_styled_reports_the_daemons_own_words_instead_of_an_empty_grid ... ok
test tui::tests::the_viewport_migration_reproduces_crop_byte_for_byte ... ok
test tui::tests::the_styled_crop_matches_plain_crop_when_every_cell_is_default ... ok

test result: ok. 36 passed; 0 failed; 0 ignored; 0 measured; 27 filtered out; finished in 0.10s
```

Full suite and gates on this branch:

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result"
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 63 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s

$ cargo fmt --all --check                                    (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings       Finished, no warnings
$ python3 scripts/check-comments.py     ok — 235 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-steps.py        ok — 21 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-principles.py   ok — 14 principles, every named mechanism exists (17 CI jobs, 11 denied paths, 245 test fns seen)
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse, 17 job(s) defined
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
```

No CI gate was added by this step, and no new denied path — the 17 (up from
16 as last measured in `steps/020`) is a base that moved between branches,
not anything added here.

## Not verified

- **Whether 정수님's specific daemon predated `steps/020`'s `CaptureStyled`
  wire message.** `remuda restart` fixing it is strong evidence, but the
  exact prior build was not independently inspected — out of scope for a
  code-level fix that is correct regardless of which build it was.
- **`version_skew`'s own transport-failure branch.** `rem.rs`'s
  `version_skew` returns `None` (no notice at all) when the daemon fails to
  answer `Request::Version` at the transport level, on the reasoning that
  nothing was listening yet and a fresh one will start as this build. Left
  open, not fixed here: is that branch actually reachable against a real
  old daemon, given how such a daemon behaves on other requests?
- **Any swallow-site outside `native/src/tui.rs`.** Not searched; this step
  fixes the two the incident actually implicated.
