# 026 — a frame that stops flashing

정수님 reported the TUI (nightly `d7558a2`, macOS/ghostty) lagging ~0.3-0.5s
between a keystroke and its echo, and visibly flickering with several
sessions open, worse with more of them. He also said the list's seconds
field is not useful information. Three separate mechanisms, each confirmed
by reading the code that runs today, not assumed from the symptom.

## Before

**Flicker.** `render_styled` (`native/src/tui.rs`) opened every frame with
`"\x1b[H\x1b[2J"` — a full-screen erase — then rewrote every row from
scratch. The only thing gating output was a whole-frame string compare
against the last write (`refresh`, `frame != *painted`); nothing gated the
*erase* itself, and nothing wrapped the write in synchronized output, so a
slow terminal could paint the blanked screen before the redraw landed. Worse,
every keystroke ran `painted.clear(); force_refresh = true;` right after
handling the key — clearing `painted` unconditionally forced the next
`frame != *painted` compare to be true even when the actual frame content
had not changed, guaranteeing a full erase-and-redraw on every key.

**Per-second repaint.** The only time-driven field anywhere in the TUI was
each list row's idle counter, `format!("{state} {flag} {}s", session.idle
.as_secs())`, shown once the list column reached 34 columns. `idle` is
`registry.rs`'s `idle_for()`, which only resets on `send` (session.rs) —
typing into an attached session calls `write_raw`, which never touches it.
So the field was not "is this session alive" (already covered by `state`);
it was uptime, ticking every second regardless of activity, forcing a full
repaint (via the always-erasing `render_styled`) once a second per session
whose row shows it, out of phase across sessions.

**Typing lag.** A key reaches the pty immediately (`client.rs` → `daemon.rs`
→ `pty.rs`), and `Action::Type` already forces an immediate `refresh()` on
the very next loop iteration (`force_refresh = true`, unconditional on any
key). The lag is a race inside that immediate refresh: `capture_styled` can
run before the child's echo has propagated back through the pty into
`vt100`'s screen. That "too early" capture still counts as a completed
refresh — `last_refresh` is reset regardless of what the frame shows — and
the *next* refresh, which would show the by-then-correct screen, was gated
on `should_refresh(false, since_last)` using the fixed `TICK` (250ms), even
while the session had focus. `TICK_TYPING` (40ms) only set the input-poll's
wake granularity (`crossterm::event::poll(tick)`); it did not gate the
refresh retry, so a missed echo could sit for up to ~210ms more than needed.

## Desired outcome

1. `list_row` drops the seconds field entirely. No replacement column is
   invented — which columns the list should show is open with 정수님,
   out of scope here. `state` and the attached flag (`⚑`) are unchanged.
   `idle`/`idle_for` stay exactly as they are on the daemon side: both
   `script.rs`'s `capture()`-adjacent Lua binding and `mcp.rs`'s status
   surface still read it, so it is not dead code, only unused by the TUI now.
2. `render_styled` (the one path actually written to a terminal — `render`,
   its test-only byte-identity twin, is untouched on purpose, same as every
   prior step that touched this file) stops erasing the whole screen. Each
   row is positioned and cleared to end-of-line (`\x1b[K`) instead; the
   footer line does the same, followed by `\x1b[J` to clear anything a
   previous, taller frame left below it (the one case row-local clears
   cannot reach: the terminal shrinking rows between two frames). The whole
   frame is wrapped in synchronized output, `\x1b[?2026h` … `\x1b[?2026l`.
3. The keystroke handler in `run` no longer calls `painted.clear()`. It
   still sets `force_refresh = true` — a key must still trigger an immediate
   refresh attempt — but the existing `frame != *painted` compare in
   `refresh` is left to decide, correctly, whether that attempt's output
   differs from what is already on screen. Nothing about the compare itself
   changed; only the artificial force-mismatch before it did.
4. `should_refresh` takes the tick as a parameter instead of always reading
   the module-level `TICK` constant. `run`'s loop already computes which
   tick to poll on (`TICK_TYPING` while a session has focus, `TICK`
   otherwise, from `steps/017`); that same value now also gates the refresh
   check, not just the poll wake. A late echo is retried at most `TICK_TYPING`
   (~40ms) after the refresh that missed it, not up to `TICK` (250ms) later.
   The list's idle-relist behaviour (`steps/017`'s original regression) is
   unaffected: `run` only switches ticks on `ui.focus`, and the list itself
   is never focused during typing.

## Expected

- `tui::tests::render_styled_never_erases_the_whole_screen`: no `\x1b[2J`
  anywhere in a real `render_styled` frame; `\x1b[K` is used instead.
- `tui::tests::render_styled_wraps_the_frame_in_synchronized_output`: the
  frame starts with `\x1b[?2026h` and ends with `\x1b[?2026l`.
- `tui::tests::the_list_row_carries_no_seconds_counter`: a row built at full
  column width contains no `s` at all — the seconds suffix is gone, not
  merely shortened.
- `tui::tests::an_idle_list_refreshes_on_the_slow_tick_not_every_poll_wake`:
  `steps/017`'s original guard, re-expressed with the tick as an explicit
  argument (`TICK`) instead of implicit — an idle, list-focused herd still
  gets at most ~5 refreshes/second of simulated `TICK_TYPING`-spaced wakes.
- `tui::tests::a_focused_sessions_late_echo_is_retried_within_the_typing_tick`:
  the same unforced check, gated on `TICK_TYPING` instead — proving the new
  behaviour a focused session actually gets is not merely "not worse", but
  ~6x faster on the specific retry that was the reported lag.
- `tui::tests::a_key_forces_an_immediate_refresh_regardless_of_the_tick` and
  `tui::tests::refresh_waits_for_the_slow_tick_when_nothing_forced_it`:
  the pre-existing boundary/forced-refresh guarantees, re-expressed for the
  new signature — unchanged behaviour, confirmed still true.
- The full existing suite, `cargo fmt`, `cargo clippy` and every
  `scripts/check-*.py` gate stay clean.

## Actual

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result"
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 73 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.09s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s

$ cargo fmt --all --check                                    (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings       Finished, no warnings

$ python3 scripts/check-comments.py     ok — 254 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-steps.py        ok — 26 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-principles.py   ok — 14 principles, every named mechanism exists (17 CI jobs, 11 denied paths, 259 test fns seen)
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse, 17 job(s) defined
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
```

No CI gate was added, no new denied path.

## Not verified

- **Whether this resolves 정수님's actual reported lag/flicker numbers.**
  Every mechanism above is real and read directly out of the code that runs
  today, and the fix removes each one at its source (an unconditional erase,
  a per-second repaint trigger, and a too-slow retry gate) rather than
  papering over a symptom. None of it has been measured on a real
  macOS/ghostty terminal — this whole change was built and tested on Linux,
  headless, against synthetic `Ui`/`StyledCell` fixtures. Say plainly: this
  removes three concrete, reproducible-in-code causes of visible redraw
  work; it does not, on its own, establish that removing them is sufficient
  to make 정수님's terminal stop flickering or feel responsive.
- **Terminal support for synchronized output (`?2026`).** A terminal that
  does not implement the mode simply ignores the unknown CSI sequence per
  the DEC private-mode convention — this is not verified against ghostty or
  Windows Terminal specifically, only asserted from the spec's own fallback
  behaviour.
- **정수님's Windows observation (color/flicker/CJK)** stays open and
  untouched — this change addresses his macOS/ghostty report only.
- **Which columns the list should show once `idle` is gone.** Deliberately
  left as an open question with him, not decided here.

## Not this PR

- Detecting or degrading for a terminal's actual colour/synchronized-output
  capability — the TUI still assumes the viewer's terminal understands what
  it is sent, same assumption `steps/020` already named as open.
- Any change to `render` (the plain path) or its byte-identity oracle from
  `steps/018`/`020` — it is test-only and was never the live write path.
