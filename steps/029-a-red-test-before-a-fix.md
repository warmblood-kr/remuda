# 029 — a red test before a fix

PR #25's new test — attach session A, drop A's `Hold`, immediately attach a
different session B — hung `cargo test --workspace --all-targets` on
`windows-latest` for 45+ minutes (a human cancelled it by hand) against a
baseline of ~90 seconds for every other PR tonight. The identical suite
passed on Linux in under a minute. This step does not touch that mechanism —
it isolates the smallest reproduction and makes CI unable to silently burn
hours on it again, before any fix is attempted.

## Before

`Hold::drop` (`native/src/client.rs:229-237`) and the daemon's own `attach()`
hangup path (`native/src/daemon.rs:342-375`) release a session by calling
`ipc::wake(&stream)` to force-unblock a thread parked in a blocking `read()`.
On Windows, `wake()` calls `CancelIoEx` on the stored `stream`'s pipe handle
(`native/src/ipc.rs:62-74`). In both call sites, the thread actually blocked
in `read()` was built from `stream.try_clone()` — and on Windows,
`try_clone()` (the `interprocess` crate) calls the real Win32
`DuplicateHandle`, producing a genuinely different handle value referencing
the same pipe object. `CancelIoEx` cancels pending I/O for the specific
handle value it is given.

Git history confirms this exact mechanism (`Hold`/`hold()`/`Drop for Hold`,
introduced in `d7f2fc0`) has never been exercised by any test before PR #25's
new one — `native/tests/` has zero references to `Hold`, `ipc::wake`, or
`CancelIoEx`. `steps/010-windows.md:271-274` names "`ipc::wake` on Windows
has no test" as a known, accepted gap from the day the mechanism shipped, but
never discusses handle *identity* — only the `shutdown`-vs-`CancelIoEx`
platform fork at a higher level. Whether a duplicated handle breaks
`CancelIoEx`'s targeting has never been discussed, tested, or measured
anywhere in this repository.

CI itself had no guard against this: `test-windows` and `test` carried no
`timeout-minutes`, so a genuine hang burns GitHub's 6-hour job default in
silence rather than failing visibly.

## Desired outcome

1. A minimal, isolated test reproducing PR #25's exact sequence (drop one
   session's `Hold`, immediately hold a different one), independent of any
   TUI/mouse code — this is a core IPC question, not a mouse one.
2. Each stage watchdog-timed on its own thread (`Drop` cannot be timed out
   in place), so a real hang fails the test in seconds with a message naming
   which stage stalled, instead of hanging the test process itself.
3. `timeout-minutes: 15` on both `test` and `test-windows` CI jobs, so this
   class of mistake cannot recur regardless of what this investigation
   concludes. **Not included in this PR** — GitHub refuses to accept a push
   touching `.github/workflows/*` from a token without the `workflow` OAuth
   scope, and this session's `gh` auth does not have it. This run is watched
   manually (a human/session cancels it if it clearly hangs) as the
   substitute for the missing job-level cap; the workflow change itself
   needs someone with that scope, separately.
4. The prediction, stated here before the branch is pushed and CI runs:
   **if the `CancelIoEx`/duplicated-handle mechanism described above is the
   real cause, this test should FAIL (a clean timeout, not a hang) on
   `windows-latest`, and PASS on `ubuntu-latest`.** A green Windows result
   here is recorded as "not reproduced this run," not as the mechanism
   being ruled out — this is a timing-dependent hypothesis, not a
   deterministic one, and a single green run does not disprove it.

## Expected

- `dropping_one_sessions_hold_then_holding_another_does_not_hang`
  (`native/src/tui.rs`) passes quickly on Linux (negative control).
- On Windows, per the prediction above: a timeout failure naming "STAGE 1"
  (the drop itself never returns) or "STAGE 2" (the drop returns, but taking
  a fresh hold on the other session does not) — or, if the hypothesis is
  wrong or does not manifest this run, a passing test.
- Neither platform's job runs past its new 15-minute cap regardless of
  outcome.

## Actual

Linux, this branch:

```
$ cargo test -p remuda-native --lib tui::tests::dropping_one_sessions_hold_then_holding_another_does_not_hang -- --nocapture
test tui::tests::dropping_one_sessions_hold_then_holding_another_does_not_hang ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 82 filtered out; finished in 0.01s
```

Full suite and gates on this branch:

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result"
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 83 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.08s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s

$ cargo fmt --all --check                                    (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings       Finished, no warnings
$ python3 scripts/check-comments.py     ok — 271 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-steps.py        ok — 29 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-principles.py   ok — 14 principles, every named mechanism exists (17 CI jobs, 11 denied paths, 276 test fns seen)
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse, 17 job(s) defined
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
```

Windows result: pending the CI run this PR opens — recorded in the PR body
once observed, not edited into this section, so the prediction above stays
visibly written before the outcome.

## Not verified

- **This is a diagnostic PR, not a fix.** Nothing in `ipc::wake`, `Hold`, or
  `client.rs`/`daemon.rs` is touched here. If the prediction holds, the fix
  is a separate, later change.
- **Whether `CancelIoEx` truly fails against a duplicated handle on real
  Windows.** The mechanism above is reasoned from reading this repo's source
  and the vendored `interprocess-2.4.4` crate's source — never measured
  until this PR's CI run.
- **Whether the 45-minute #25 hang has this exact cause versus some other
  confound in that test's own harness/teardown.** If this test comes back
  green, the next step is investigating #25's harness ordering specifically,
  with the core IPC question left open, not closed.

## Run 1 result and what followed

Run 1 (this PR's own test above) came back GREEN on `windows-latest` in
1m26s — recorded as "not reproduced this run," not as the mechanism ruled
out, per the prediction's own terms.

Run 2: a rerun of PR #25's original, unmodified test (`gh run rerun` against
the same commit that first hung) was then watched by hand: it **hung again**,
cancelled after 10 minutes. Both the original run and the rerun's own
"has been running for over 60 seconds" watchdog line name the exact same
test, character-for-character:
`tui::tests::reconcile_hold_switches_the_real_attach_not_just_ui_state`.
This is a confirmed, reproduced, deterministic hang on that specific test —
not flakiness, not cross-contamination from a different test in the same
parallel run.

A structural diff between #25's hanging test and this PR's passing one
found: #25's test calls `refresh()` once before its hold/drop/hold
sequence. `refresh()` internally performs a `list()` IPC call AND a
`capture_styled()` IPC call (a full connect/request/respond/disconnect
round trip that reads a session's vt100 screen) AND a `render_styled()`/
conditional stdout write. This PR's test above performs only a bare
`list()` call before its sequence — no `capture_styled`, no render, no
stdout write.

## Isolation test (run 4): does the extra `capture_styled` round trip matter?

(Budget correction: the prediction-only commit below, `9cac046`, also
triggered its own `test-windows` run — 1m21s, previously miscounted as
free. So this section's test is run 4 of 6, not run 3: run 1 = `c12ec69`
above, run 2 = the `gh run rerun` of #25's original test, run 3 = `9cac046`
itself, run 4 = `00b9a1d` below. That leaves 2 of 6 remaining before the
next run.)

**Prediction, stated before this run's test code is written or pushed:**
if the extra `list()` + `capture_styled()` connection churn immediately
before the hold/drop/hold sequence is what changes the outcome (not the
`CancelIoEx`/handle-identity mechanism alone), this test should FAIL (a
clean, stage-labeled timeout) on `windows-latest`. If it passes, the
connection-churn hypothesis is not supported by this run, and whatever
makes #25's test different must be elsewhere — e.g. going through the full
`reconcile_hold`/`Ui`/`refresh` production path specifically (three
`reconcile_hold` calls, not one hold/drop/hold), or genuine
non-determinism.

This commit contains only this prediction — the test code that exercises
it is a separate, later commit, so git history itself shows the prediction
was written first.
