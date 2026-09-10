# 022 — two cheap fixes for one keystroke

정수님 reported 0.3-0.5s between a keystroke and its echo inside a focused
session. The draw cadence is not it — `steps/017` already settled that a key
forces an immediate refresh regardless of the slow tick. What a forced
refresh actually does, every keystroke, is the suspect: two sequential IPC
round trips (`list`, then `capture_styled`), one of which `steps/020` made
much fatter.

Two things were found on the per-keystroke path, one in each round trip.
Both are cheap; neither is verified to be *the* 300-500ms by itself — that
would need a Windows instrument this diagnosis does not have. Both are
worth doing regardless, so both are in this one change rather than picking
one to test first.

## Before

**The herd relist.** `refresh()` (`native/src/tui.rs`) called `list(path)`
unconditionally on every forced wake, including one forced by a keystroke
already inside a held session — a keystroke that cannot itself add or
remove a session. That is one full IPC round trip per keystroke for
information that, in that specific case, cannot have changed.

**The per-cell wire encoding.** `steps/020` gave `Response::StyledScreen` a
`Vec<Vec<StyledCell>>` — one full 8-field JSON object per cell, regardless
of whether that cell carries any style at all. Measured on a real captured
80x24 screen (one coloured prompt line, everything else default): 224,689
bytes. A real screen does not have 1,920 independently-styled cells; it has
a handful of style runs. `capture_styled` is called on every forced wake —
every keystroke inside a held session included.

## Desired outcome

1. `refresh` skips the herd relist specifically when the forced wake came
   from `Action::Type` — a keystroke already forwarded to a held session's
   pty. `Action::Start`/`Action::Kill` only ever fire from `Focus::List`,
   never from `Action::Type`'s `Focus::Session` path (`Ui::session_key`
   returns only `Action::Nothing` or `Action::Type`), so neither can be
   starved by this skip: a `Start`/`Kill`-forced wake never sets the skip
   flag, and still relists exactly as before. The tick (`TICK`, unforced)
   also still relists every 250ms regardless, so a herd change from a
   second client is seen at most one tick later than today — a form of
   staleness that already exists.
2. The wire carries runs, not cells: `Response::StyledScreen` is now
   `Vec<Vec<StyledRun>>`, a run being one style's worth of consecutive
   characters. `PtyAgent`/`AgentProcess::screen_cells`, `Cell`/`Viewport`,
   `crop_styled`/`render_styled` and their existing tests are all untouched
   — the daemon collapses cells into runs only when building the response
   (`native/src/daemon.rs`), and `capture_styled` expands runs straight
   back into `Vec<Vec<StyledCell>>` on the way in, before anything else
   (`crop_styled`, `render_styled`) sees the result. A run's character count
   always equals its cell count, since every `StyledCell` is one `char`
   wide by construction (CJK width is still deliberately wrong —
   `steps/018`/`020`) — collapsing and expanding lose nothing.
3. `steps/021`'s honesty is unaffected: a transport or decode failure on
   either request still surfaces through `Ui::notice`, not a silently-empty
   result. This change only touches how a *successful* `StyledScreen` is
   shaped on the wire.
4. `#10` (the CPU/redraw fix) and `#14` (styled cells) are named plainly as
   trading against each other: `#10` made the per-keystroke path cheap to
   hit every time; `#14` then put a much larger payload on exactly that
   path. This is a consequence of two changes already shipped, not a fresh,
   unrelated bug.

## Expected

- `tui::tests::refresh_skips_the_herd_relist_only_when_asked`: a
  `skip_list=true` refresh does not see a herd change made between calls; a
  `skip_list=false` refresh — the only kind a `Start`/`Kill`-forced wake
  ever runs — still does.
- `protocol::tests::collapsing_into_runs_and_expanding_back_changes_nothing_visible`:
  `expand_runs(collapse_runs(row)) == row` for a row with several style
  groups.
- `daemon::tests::collapsing_to_runs_shrinks_the_wire_size_a_real_screen_produces`:
  a representative 80x24 screen's run-collapsed JSON is under a tenth the
  size of its per-cell JSON, on the real `serde_json` wire encoding.
- The full existing suite, `cargo fmt`, `cargo clippy` and every
  `scripts/check-*.py` gate stay clean; nothing about `crop`, `render`, the
  plain `Capture` path, or either prior styled-cell oracle changes.

## Actual

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result"
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 65 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.11s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s

$ cargo fmt --all --check                                    (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings       Finished, no warnings
```

Measured, real screen captured through the actual `PtyAgent::screen_cells`
+ `collapse_runs` pipeline (not synthetic): an 80x24 screen with one real
coloured prompt line serialises to 224,689 bytes per-cell, 5,101 bytes
run-collapsed — a 44.0x reduction. A plain, uncoloured 80x24 screen (all
cells default): 224,689 bytes per-cell, 4,753 bytes run-collapsed — 47.3x.
The gate scripts, run after both fixes:

```
$ python3 scripts/check-comments.py     ok — 241 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-steps.py        ok — 22 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-principles.py   ok — 14 principles, every named mechanism exists (17 CI jobs, 11 denied paths, 249 test fns seen)
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse, 17 job(s) defined
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
```

No CI gate was added, no new denied path.

## Not verified

- **Whether either fix moves 정수님's actual reported latency.** Both are
  real, measured, and cheap; neither has been run on his machine, and
  neither can be until someone does. The µs-scale timings behind this
  investigation were all taken on a Linux unix socket — a named pipe's
  per-write and per-connect costs are a different OS primitive, with real
  known behaviour (AV/EDR interception scanning pipe traffic) this
  instrument does not exercise at all. Say plainly: this removes a real,
  transport-independent 113x payload multiplier and a real unconditional
  extra round trip per keystroke; it does not, on its own, establish that
  either was the cause of the reported 300-500ms.
- **`vt100`'s own `screen_cells()` cost**, as distinct from JSON
  serialisation cost — building the `Vec<Vec<StyledCell>>` grid from
  `vt100`'s parser was not isolated or measured here, only the wire
  encoding of the result.
- **`version_skew`'s own transport-failure branch** (`rem.rs`'s
  `Err(_) => None`), flagged in `steps/021`, remains open and untouched.
- **Windows.** No Windows host is available; nothing here has run there.
