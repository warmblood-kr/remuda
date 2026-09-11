# 030 — a red test before a fix

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

## Same-thread / reconcile_hold isolation (run 5)

Run 4's `capture_styled` isolation test (`00b9a1d`) came back GREEN on
`windows-latest` in 1m33s — "not reproduced this run." Two structural gaps
remained unexamined: (a) every prior diagnostic test does its hold/drop on a
**spawned** thread behind a channel + `recv_timeout` watchdog, while #25's
actual hanging test does every hold/drop on the **test thread itself**,
including an implicit final drop at scope exit; (b) no prior diagnostic test
goes through the production `reconcile_hold`/`Ui` path, which exists only on
#25's branch.

This branch (`investigate/hold-drop-same-thread`, cut from #25's head
`c37a3c3`) cherry-picks the three prior diagnostic commits and adds two more
tests, both instrumented with a stage counter, a completion sentinel whose
`Drop` fires last, and a 60s watchdog thread that writes `STUCK AFTER STAGE
n` to stderr (bypassing libtest capture) and exits 101 — a real hang fails
fast and names the stage, instead of hanging the job:

- `reconcile_hold_switches_the_real_attach_not_just_ui_state_same_thread` —
  #25's own test body, verbatim, with no thread spawned around any of it.
  Stage 1 = refresh done, 2 = attach A done, 3 = switch to B done, 4 =
  idempotent re-attach to B done, 5 = body's last statement reached.
- `hold_a_drop_a_hold_b_drop_b_same_thread` — the bare hold/drop/hold/drop
  sequence, same-thread counterpart of the cross-thread `dropping_one_sessions...`
  test. Stage 1 = hold A, 2 = drop A, 3 = hold B, 4 = drop B.

The prediction for this run is pre-registered as a GitHub comment on PR #26
before this branch is pushed, not in this file, per the corrected protocol —
a server-side comment timestamp proves the order without relying on a second
commit.

**Instrumentation self-check, done before any push:** a 70s
`std::thread::sleep` was temporarily inserted before the `hold B` step of
the same-thread bare-sequence test, run locally with `--test-threads=1`.
Result: `V2 STUCK AFTER STAGE 2`, process exit code 101, in well under the
60s watchdog window. The injected sleep was then removed; all five relevant
tests (the four diagnostic tests plus #25's own original) pass in under
0.1s on Linux. The injection was never committed.

Renumbering note: this file was `steps/029-a-red-test-before-a-fix.md` on
PR #26. Cherry-picking it onto a branch cut from #25's head collides with
#25's own `steps/029-a-click-is-not-a-one-way-door.md` (both PRs claimed
029 independently, since neither existed on the other's branch when
written). Renumbered to 030 on this branch only, to satisfy
`check-steps.py`; #25's and #26's own branches are untouched.

## Run 6 — mechanism reading, before the out-of-process discriminator

Run 5 (above) reproduced a real hang on `windows-latest`: the bare
same-thread `hold A → drop A → hold B → drop B` sequence (`V2`) got
`STUCK AFTER STAGE 1` — inside `drop(hold_a)` itself — while the identical
sequence wrapped in a spawned thread (the cross-thread twin) has now passed
4-for-4 across every run. Two things had to happen before spending the
sixth and last budgeted run: read the actual mechanism rather than assume
it, and confirm that a fix belongs in shared code before touching it.

**A read-only production trace** (`git show 3ccf3b29`, verbatim) confirms
the TUI itself drops its own `Hold` on its creating thread, no different
from `V2`'s shape: `take → client::hold` (`tui.rs:1036`) runs inside
`refresh` (`tui.rs:867`) on the `tui::run` thread (`remuda.rs:48`); Ctrl-\
detach sets `focus = List` (`tui.rs:152-155`) and the *next* `refresh` does
`*held = None` (`tui.rs:865`), dropping it right there; quit
(`Action::Quit => return Ok(())`, `tui.rs:1002`) drops it by falling out of
scope (`tui.rs:962`). Both are the same-thread shape. Yet 정수님 used the
Windows TUI all day — detaching and quitting repeatedly — with no hang
reported. That is a real contradiction to resolve, not a result to explain
away, and it is why this run tests **process locality** before anything
gets called a user-facing bug: every diagnostic test so far, including
`V1`/`V2`, runs the daemon **in-process** (a spawned thread inside the test
binary — `daemon_at`, `tui.rs:2055-2058`). Real usage always runs the
daemon as its own OS **process** (`bin/remuda.rs`'s `start_daemon`,
spawning `remuda -s <server> daemon` and polling for a connect). That
difference has never been tested. Until it is, nothing here is written as
"this hangs for users" — only as "this hangs on Windows CI, in-process,
same-thread" (PR #28, `V2`, 1-for-1 on that specific shape so far).

**First candidate mechanism, already on record before this investigation
began:** `steps/010-windows.md:271-274` — *"`ipc::wake` on Windows has no
test... the `CancelIoEx` half is reasoned from the crate's source, not
measured."* The working theory carried into run 5 was that `hold()`
(`client.rs:195`) hands the `drain` thread a `try_clone()`-derived (i.e.
`DuplicateHandle`-derived) handle, while `Hold::drop` (`client.rs:233`)
calls `ipc::wake`/`CancelIoEx` on the *original*, un-cloned handle — two
distinct `HANDLE` values for the same pipe object.

**Tested against the discriminator, and it does not fit.** The duplicated
handle is present in *every* test to date, same-thread and cross-thread
alike — `hold()` always clones for the drain thread, regardless of which
thread later calls `drop`. A same-thread-vs-cross-thread outcome difference
cannot come from a mechanism that is identical in both cases. A theory has
to explain why the cross-thread wrapping alone flips the result 4-for-4; a
wrong-handle theory does not, and this is written down rather than carried
forward unexamined.

**The specific lead the steward asked to check, checked, and closed:**
is the pipe handle synchronous (non-overlapped), making this
`CancelSynchronousIo` territory rather than `CancelIoEx`'s? Read from the
vendored `interprocess-2.4.4` crate source directly (not inferred):

- The Windows named-pipe handle is opened with `FILE_FLAG_OVERLAPPED`
  (`c_wrappers.rs:157-166` in the vendored crate) — asynchronous, not
  synchronous.
- The crate's blocking `read()` is `ReadFileEx` plus an APC completion
  routine, with the calling thread parked in `SleepEx(_, alertable=1)`
  until its own APC queue delivers the completion (`c_wrappers.rs:91-104,
  114-126, 164-166`) — still the overlapped/APC path, not
  `GetOverlappedResult`/`WaitForSingleObject`, and not a synchronous
  blocking read either.
- The crate itself contains zero calls to any `CancelIo*` function — all
  cancellation is left to the caller, which is this repo's `ipc.rs`.

This closes the synchronous-handle lead: the handle **is** overlapped, so
`CancelIoEx` is the documented, correct API family for it. `ipc.rs`'s own
comment ("this is the API that cancels one") is accurate on this point.
`CancelSynchronousIo` does not apply here and is not the fix.

**What explains the discriminator, stated as a reasoned candidate, not a
measured one:** `ReadFileEx`-based cancellation is well documented (Win32
practitioner knowledge, not read from any source in this repo) to be a
no-op with respect to a read that has not yet been *issued* — `CancelIoEx`
cancels *pending* I/O, and a call that has not reached the kernel yet is
not pending. `hold()` spawns the `drain` thread and returns immediately;
the drain thread must still schedule, run, and call `ReadFileEx` before
there is anything to cancel. Wrapping `drop(hold_a)` in
`std::thread::spawn` (as every passing cross-thread test does) costs a new
OS thread's creation and scheduling — plausibly just enough delay for the
already-spawned drain thread to reach its `ReadFileEx` call first. Calling
`drop(hold_a)` inline, immediately after `hold()` returns on the same
thread (as `V2` and the TUI's own detach/quit do), leaves a much tighter
window in which `wake()` can fire before the drain thread's read is
pending — after which nothing will ever cancel it. This is a race, not a
handle-identity bug, and it would explain why CI (many competing test
threads, one process, likely under scheduler pressure) hits it reliably
while a single real `remuda` TUI process detaching after a person has
looked at the screen for a moment might not.

**What this evidence cannot reach:** nothing in this repository or the
vendored crate was instrumented to observe the actual ordering of
`ReadFileEx` issuance versus `CancelIoEx` on real Windows hardware — this
mechanism is argued from documented `CancelIoEx` semantics and the crate's
confirmed use of the overlapped/APC path, not measured. Run 6 does not
test this race directly either; it tests a different, prior question
(process locality, and human-scale timing) that must be answered first,
since a hang that only reproduces in-process would mean nothing here is a
user-facing bug at all.

## Run 6 — the out-of-process discriminator

Two structural questions, tested together in one run since both are free
once the harness exists:

1. **Locality.** Every test to date runs the daemon in-process. Does the
   same-thread hang reproduce against a REAL spawned `remuda -s <server>
   daemon` OS process (`bin/remuda.rs`'s own `start_daemon` shape), or is
   it an in-process test-harness artifact?
2. **Timing.** `V2` drops immediately after taking the hold. A person
   using the TUI looks at the screen for a moment before pressing Ctrl-\.
   Does a 1-second pause between `hold()` and `drop()` change the outcome?

Three new tests, `native/tests/out_of_process_hold.rs`, all against a real
spawned `remuda` binary (`Daemon::spawn`, mirroring `tests/daemon.rs`'s own
real-subprocess helper), each carrying the same stage/watchdog
instrumentation as `V1`/`V2` (now sharing one registry — see below):

- `out_of_process_hold_a_drop_a_hold_b_drop_b_same_thread` — the bare
  same-thread sequence, out-of-process.
- `out_of_process_hold_a_drop_a_hold_b_drop_b_cross_thread` — the
  cross-thread twin, out-of-process, as the paired comparison run
  alongside it.
- `out_of_process_settled_drop_does_not_hang_immediately` — same-thread,
  with a 1-second pause between `hold()` and `drop()`.

**Instrumentation fix — the run 5 blind spot.** Run 5's `V2` watchdog fired
`std::process::exit(101)` on its own timeout, which killed the whole test
binary before `V1`'s own watchdog (also past 60s at that instant) could
report its stage. `V1` and `V2` now register `(name, stage)` into one
shared, process-wide table (`StageRegistry`, `tui.rs`); whichever
watchdog's 60s timer fires first dumps every currently-registered entry
before exiting, so one test's exit can no longer hide a sibling's stage.

**Positive control, Linux, before use anywhere:** a 70s sleep was injected
into both `V1` and `V2` at once and run with `--test-threads=4`. The single
dump that appeared before the process exited named both:
`STUCK: V1 AFTER STAGE 2` and `STUCK: V2 AFTER STAGE 2`. Both injections
were then fully removed and the suite reconfirmed green before anything
was committed.

**Prediction, stated before this run's push, covering every branch:**

1. Out-of-process same-thread hangs → the TUI's own detach/quit path is
   implicated for real use — this is the branch that would need to be
   reconciled with 정수님's contradiction-free all-day usage (most likely
   by the timing-race candidate above: CI's scheduler pressure makes the
   race far more likely to land than an interactive session's natural
   pacing does), not dismissed as impossible.
2. Out-of-process same-thread green while the in-process same-thread test
   (replicated in this same run) still hangs → an in-process test-harness
   artifact. Written up as exactly that; no user-facing claim follows from
   it.
3. The settled-drop variant passes while the immediate-drop variant (either
   locality) hangs → consistent with the timing-race candidate: the hang
   needs a specific, narrow interleaving, and "deterministic in CI, rare at
   human speed" is a coherent, non-contradictory state — it is not read as
   one of the two observations (the CI hang, or 정수님's clean day) being
   wrong.
4. Both the immediate-drop and settled-drop variants hang, in-process or
   out-of-process → a 1-second pause is not sufficient insulation, and
   human timing is not read as protection.

No branch above is pre-registered as requiring either observation (the CI
hang, or the day of unhung real use) to be the wrong one — both are taken
as true, and the job is to find the shape that makes both true at once.

Run budget: this is run 6 of 6, the last budgeted `windows-latest` run.
This run is the discriminator above, not a fix — `client.rs`, `daemon.rs`,
`ipc.rs`, `protocol.rs`, and `session.rs` are untouched here. If this run's
result points at a fix, that fix is a separate PR, and any run beyond this
sixth one needs the steward's explicit extension first.
