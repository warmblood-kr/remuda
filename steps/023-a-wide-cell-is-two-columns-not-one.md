# 023 — a wide cell is two columns, not one

## Before

`steps/018` and `steps/020` deliberately stubbed `Cell::width()` at `1` for
every cell, both `char` and `StyledCell`, so colour could ship without also
solving CJK display width the same night. 정수님 (Korean, Hangul) later found
a mechanism for a separate "flicker" complaint: Windows Terminal →
Compatibility → Text measurement mode, toggled between Grapheme Clusters and
wcswidth. His result — "다소 줄었습니다. 여전하긴 합니다" (somewhat reduced,
still persists) — is a partial improvement, not a fix, and it moved a
setting on the *receiving* terminal, not anything in this code. That is
consistent with the same underlying disagreement (our column bookkeeping
assumes every character is 1 column wide) surfacing differently depending on
how the terminal itself measures what we send it — not a separate quirk.

The pane's live path (`refresh()` → `capture_styled` → `crop_styled` →
`render_styled`, confirmed by grepping every caller of `render`/`crop` in
`native/src/tui.rs`: only test code still calls the plain, `char`-based
pair) already gets one `StyledCell` per **physical** `vt100` column,
including a wide character's blank continuation column
(`native/src/pty.rs`'s `screen_cells`, iterating `0..cols`). Two bugs
followed from `width() == 1` everywhere on that grid:

1. The continuation column's placeholder text was a literal `" "` ("a space
   keeps column alignment intact" — true only if every cell really is 1
   column). Emitting that space after a wide glyph that already draws 2
   columns adds a real, extra, unaccounted-for column to the byte stream on
   every wide character in the row.
2. `Viewport::crop_row`'s cumulative column math, and `visible_width`'s
   plain per-`char` count used for right-padding, both under-count a wide
   glyph's true footprint by 1 — misaligning cropping, panning, and padding
   by 1 column per wide character, compounding across a row.

## Desired outcome

1. `StyledCell` gains `wide: bool` — true for a wide cell, carrying the full
   glyph; false and empty-text for its continuation. `PtyAgent::screen_cells`
   reads this from `vt100::Cell::is_wide`/`is_wide_continuation` — `vt100`
   already computed it correctly (via `unicode-width`, the same crate this
   step reaches for) to emulate a real terminal's cursor advance; nothing is
   re-derived.
2. `impl Cell for StyledCell` answers `2` for a wide cell, `0` for its empty
   continuation, `1` otherwise — the exact physical columns `vt100` gave
   them. `impl Cell for char` (the plain, pane-dead path) is untouched and
   stays wrong on purpose, matching `steps/018`/`020`'s stance for that path.
3. `Viewport::crop_row`'s width sum drops its `.max(1)` clamp (a no-op for
   `char`, load-bearing now for `StyledCell`'s `0`), and `crop_styled`'s
   cut-marker no longer blindly pops one `Vec` entry — it pops until it has
   freed at least 1 real column, so popping a zero-width continuation alone
   can never leave the `→` marker overflowing the pane.
4. `visible_width` (padding for the live styled row) counts each character's
   real display width via `unicode-width`, the same crate `vt100` itself
   uses for this judgment, pinned to the same major version.
5. `StyledRun`/`collapse_runs`/`expand_runs` (`steps/022`'s wire compaction)
   carry `wide` through losslessly: a run's grouping key now includes
   wideness, so a run is never a mix of wide and narrow cells.
6. Correct under **either** Windows Terminal measurement mode: this fix
   does not depend on 정수님's setting change, and does not touch it.

## Expected

- `pty::tests::screen_cells_marks_a_wide_hangul_cell_and_its_empty_continuation`:
  a real spawned child printing `안녕!` — `vt100`'s own judgment, not a
  synthetic one — shows cells 0/2 wide with the glyph text, cells 1/3 empty
  and not wide, `!` and a genuinely blank cell both ordinary width 1.
- `tui::tests::styled_cell_width_reflects_wide_and_continuation`: `2`/`0`/`1`
  from the trait `crop_styled`/`render_styled` actually call through.
- `tui::tests::a_cut_after_a_wide_cell_never_overflows_the_pane_width`: the
  exact failure this step removes — a row cut immediately after a wide
  cell's continuation — never exceeds the pane's column budget.
- `protocol::tests::a_wide_cells_flag_survives_the_round_trip_and_its_continuation_vanishes`:
  wideness is not lost to `steps/022`'s run compaction, and a continuation
  never reappears as a phantom cell.
- The full existing suite passes unchanged, including `steps/020`'s
  byte-identical degrade oracle (its cells are all plain, `wide` defaults to
  `false`, so nothing there changes) and the plain `char`-based `crop`/
  `render` path and its own oracle.

## Actual

```
$ cargo test -p remuda-native --lib pty::tests::screen_cells_marks_a_wide_hangul_cell_and_its_empty_continuation -- --nocapture
test pty::tests::screen_cells_marks_a_wide_hangul_cell_and_its_empty_continuation ... ok

$ cargo test -p remuda-native --lib tui:: 2>&1 | tail -8
test tui::tests::styled_cell_width_reflects_wide_and_continuation ... ok
test tui::tests::a_cut_after_a_wide_cell_never_overflows_the_pane_width ... ok
test tui::tests::the_styled_crop_matches_plain_crop_when_every_cell_is_default ... ok
test tui::tests::the_viewport_migration_reproduces_crop_byte_for_byte ... ok

test result: ok. 68 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s

$ cargo test -p remuda-core --lib 2>&1 | tail -6
test protocol::tests::a_wide_cells_flag_survives_the_round_trip_and_its_continuation_vanishes ... ok
test protocol::tests::collapsing_into_runs_and_expanding_back_changes_nothing_visible ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

Full suite and gates on this branch:

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result"
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 68 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.16s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.72s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s

$ cargo fmt --all --check                                    (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings       Finished, no warnings
$ cargo check -p remuda-core --target wasm32-unknown-unknown  Finished, no unicode-width added to core
$ python3 scripts/check-comments.py     ok — 247 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-steps.py        ok — 23 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-principles.py   ok — 14 principles, every named mechanism exists (17 CI jobs, 11 denied paths, 254 test fns seen)
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse, 17 job(s) defined
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
```

No CI gate was added, and no new denied path.

## Not verified

- **Not verified to fix flicker.** 정수님's own observation ("다소
  줄었습니다") is one person's report of one setting change affecting two
  complaints together, on one machine, once. This step fixes the code-side
  column accounting; it does not itself verify a flicker improvement, which
  needs his confirmation on Windows.
- **Windows.** No Windows host is available. Nothing here has run or been
  seen on Windows, in either measurement mode.
- **Ambiguous-width Unicode, named not resolved.** Both `vt100` (internally)
  and this fix's `visible_width` call `unicode_width`'s `.width()` — the
  non-CJK-context method, which treats Unicode's Ambiguous East-Asian-Width
  category (some box-drawing, Cyrillic, Greek, typographic characters) as
  narrow (1 column), not the CJK-context `.width_cjk()` convention (2
  columns). If Windows Terminal's two measurement modes disagree with each
  other, or with this convention, specifically on an Ambiguous-width
  character, this fix does not resolve that. The reported problem is
  Korean text, which is unambiguously Wide in every convention — this is
  what the fix targets and what the two measurements above prove solid.
- **The list column's own width math** (`fit`/`list_row`, for session
  names) still counts by plain `char`, same latent class of bug if a
  session were ever named with wide characters. Not reported, not touched.
- **`version_skew`'s `Err(_) => None` transport-failure branch**
  (`steps/021`'s open question) remains open and untouched.
