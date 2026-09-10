# 017 — gate the work, not just the write

정수님 got remuda running on Windows for the first time and hit a machine-wide
slowdown: *"in window Terminal app, screen seems frequently refreshing. even
it consumes CPU and make whole system slow. and when I detach screen, system
comes normal."*

## Before

The child shell staying fine after detach already narrows the cause to
remuda's own draw path, not the child process — `run`'s loop in
`native/src/tui.rs`.

That loop did a full relist (`list(path)`), an IPC round-trip to the daemon
for a whole screen snapshot (`capture(path, name)`), a terminal-size query
and a rebuilt frame string — **unconditionally, on every wake of the input
poll**, key or not. Only the terminal *write* was gated on change (`if frame
!= painted`). While a session has focus, that poll wakes every `TICK_TYPING`
(40ms), so an **idle** attached session cost ~25 of those full cycles a
second, forever.

Two things made this easy to miss reading the file cold:

- **The comment lied by omission, not by falsehood.** The line at the write
  site said "repaint only on change" — true of the write, silent about
  the work above it. A reader trusts a comment that describes intent; this
  one accurately described a *narrower* scope than the code actually had,
  and nothing marked the gap.
- **The comment's own number was stale.** It also said the frame was written
  "four times a second" — true of `TICK` (250ms) alone, from before
  `TICK_TYPING` (40ms, 6.25× faster) existed on this same hot path. A rate
  that was true when written and never updated when a faster path was added
  is the same failure as a status line nobody revisits.

**25/sec is a floor, not a ceiling.** `crossterm::event::poll(tick)` is a
*timeout* — any input event wakes the loop early. So actually typing drives
the rate *higher* than the idle floor: each keystroke is its own extra wake,
one full cycle on top of the timer. The report — worst while typing, normal
after detach — is exactly what that predicts, and the opposite of what "it's
just a 40ms timer" would predict.

This draw *count* is platform-independent: it is exactly as high on a Linux
box as it would be on Windows. Windows likely turns each of those IPC
round-trips (a named pipe) and console writes (ConPTY) into something far
more expensive than the Unix equivalent — that multiplier is not measured,
only the call count is, and there is no Windows host to measure it on.

## Desired outcome

An idle attached session redraws at the same slow cadence the list-focus
case already uses (`TICK`, 250ms), not on every keyboard-poll wake. A
keystroke still gets an immediate refresh — echoing it must not feel
slower than it does today.

## Expected

- `should_refresh(forced, since_last)` — a small pure function — returns
  `true` right after a key changed state, or once `TICK` has elapsed since
  the last refresh; `false` otherwise.
- `run`'s loop calls the expensive cycle (now `refresh`, extracted verbatim
  from the old loop body) only when `should_refresh` says so.
- A test simulating one idle second of `TICK_TYPING` (40ms) wakeups with no
  key ever pressed shows the refresh count capped near `TICK`'s cadence
  (≤5/s), not the poll's (25/s) — and, run against the pre-fix logic
  (`should_refresh` always `true`), shows 25.
- `cargo test --workspace` and `cargo clippy --all-targets` both stay green.

## Actual

Verified by hand that the regression test fails for the predicted reason
before the fix, not just that it passes after: temporarily replaced
`should_refresh`'s body with an unconditional `true` (the pre-fix shape),
reran the one test, saw it fail, then restored the real implementation.

```
$ cargo test -p remuda-native --lib tui::tests::an_idle_focused_session_refreshes_on_the_slow_tick_not_every_poll_wake -- --nocapture

running 1 test

thread 'tui::tests::an_idle_focused_session_refreshes_on_the_slow_tick_not_every_poll_wake' panicked at native/src/tui.rs:657:9:
an idle session must not redo the full IPC cycle on every 40ms poll wake — got 25 refreshes in one simulated second; the pre-fix behavior gives 25
test tui::tests::an_idle_focused_session_refreshes_on_the_slow_tick_not_every_poll_wake ... FAILED

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 54 filtered out; finished in 0.00s
```

Restored the fix and reran the full suite and gates on this branch:

```
$ cargo test --workspace 2>&1 | grep -E "^test result"
test result: ok. 0 passed;   ok. 21 passed;  ok. 10 passed;  ok. 5 passed;
test result: ok. 56 passed;  ok. 0 passed;   ok. 9 passed;   ok. 8 passed;
test result: ok. 7 passed;   ok. 4 passed;
$ cargo fmt --all --check                                   (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings      Finished
$ python3 scripts/check-comments.py     ok — 204 doc comment(s) within cap
$ python3 scripts/check-steps.py        ok — 16 step(s)
$ python3 scripts/check-principles.py   ok — 14 principles
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered
```

No CI gate was added, so `gates-can-fail` gains no new plant.

## Not verified

- **The reported CPU/system-slowdown symptom itself, on Windows.** No
  Windows host is available. What's verified is that the mechanism the
  report described — constant redraw while attached, an idle floor
  independent of the child's output — is no longer present in the code, and
  that the draw *count* this fixes is measured, not the platform-specific
  cost multiplier.
- **The keystroke-driven rate.** Unchanged by this fix on purpose — one
  refresh per key, proportional to typing speed, was never the bug; only
  the unconditional idle floor was.
