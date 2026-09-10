# 019 — a third readout: styled cells, croppable

`steps/018` migrated the plain-text preview onto `Viewport`/`Cell`, proved it
a no-op, and deliberately punted two things: the pane still shows no colour,
and CJK display width is still wrong. This step builds the first of those
two — colour — riding on the same abstraction, without touching the second.

## Before

The TUI pane reads a session via `Request::Capture` → `screen_text()` →
`vt100`'s `parser.screen().contents()`, which discards every SGR sequence —
`steps/018`'s diagnosis. `attach`'s initial repaint keeps colour
(`screen_bytes()` → `contents_formatted()`), but that path is a
whole-terminal repaint that cannot be cropped into a side-by-side pane. There
is no styled-cell type anywhere in `core`, no wire request/response for one,
and no renderer that could turn styled cells back into SGR.

## Desired outcome

1. A new, additive readout: `AgentProcess::screen_cells`, a
   `Request::CaptureStyled`/`Response::StyledScreen` round trip, and a
   `registry.screen_cells`/`session.screen_cells` pair mirroring the existing
   `screen_text` plumbing exactly.
2. `PtyAgent` answers it from `vt100`'s own cell grid (colour, bold, dim,
   italic, underline, inverse), not from re-parsing `screen_text`'s output —
   that reparse is exactly what discards the colour today.
3. The TUI's own preview column rides `StyledCell` through the existing
   `Viewport::crop` (already generic over `Cell`), and a new
   `render_styled_row` turns cropped cells back into minimal SGR.
4. The plain/colourless path (`Request::Capture`, `screen_text`, the free
   `crop` function, and `pub fn render`) is untouched — every existing
   caller (`script.rs`'s `capture()` builtin, `mcp.rs`'s `"capture"` command,
   every test asserting an exact frame string) keeps working exactly as
   before, and the `steps/018` byte-identity oracle keeps passing unmodified.
5. CJK display width stays wrong on purpose, same as `steps/018`:
   `impl Cell for StyledCell { fn width(&self) -> u16 { 1 } }`, matching
   `impl Cell for char`. Fixing it is a separate, later change against the
   same trait.

## Expected

- `pty::tests::screen_cells_carries_colour_that_screen_text_discards` spawns
  a real child emitting `\x1b[31mred\x1b[0m`, and shows the cells under
  "red" report `Color::Idx(1)` while a cell after the reset reports
  `Color::Default` — the value confirmed empirically, not assumed, since SGR
  31 could in principle have been surfaced by `vt100` under a different
  index.
- `tui::tests::the_styled_crop_matches_plain_crop_when_every_cell_is_default`
  sweeps the same 7 screens and the same exhaustive `cols`/`pan`/`rows`
  range as `steps/018`'s oracle, and shows the styled path degrades to
  byte-identical output when nothing is styled.
- `tui::tests::render_styled_row_only_emits_sgr_on_a_style_change` shows a
  row with two style groups emits exactly 2 style-change resets plus one
  trailing reset, not one reset per cell.
- The full existing suite still passes unchanged, including
  `pty::tests::screen_text_strips_colour_that_screen_bytes_keeps` and
  `tui::tests::the_viewport_migration_reproduces_crop_byte_for_byte`.

## Actual

```
$ cargo test -p remuda-native --lib pty::tests::screen_cells_carries_colour_that_screen_text_discards -- --nocapture

running 1 test
test pty::tests::screen_cells_carries_colour_that_screen_text_discards ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 60 filtered out; finished in 0.01s
```

Empirically confirmed the mapping before writing the assertion: `printf
'\x1b[31mred\x1b[0m'` (SGR 31, "red") surfaces from `vt100` as
`Color::Idx(1)`, not some other index — this is what the test now asserts
rather than assumes.

```
$ cargo test -p remuda-native --lib tui:: 2>&1 | tail -10

test tui::tests::render_styled_row_only_emits_sgr_on_a_style_change ... ok
test tui::tests::the_crop_anchors_bottom_left_and_admits_what_it_cut ... ok
test tui::tests::the_detach_byte_is_recognised_however_crossterm_spells_it ... ok
test tui::tests::the_preview_claims_what_the_widest_session_needs ... ok
test tui::tests::the_prompt_edits_and_starts_what_it_shows ... ok
test tui::tests::the_selection_stops_at_both_ends ... ok
test tui::tests::the_frame_says_what_it_is_showing ... ok
test tui::tests::the_preview_starts_at_the_sessions_own_first_row ... ok
test tui::tests::the_styled_crop_matches_plain_crop_when_every_cell_is_default ... ok
test tui::tests::the_viewport_migration_reproduces_crop_byte_for_byte ... ok

test result: ok. 34 passed; 0 failed; 0 ignored; 0 measured; 27 filtered out; finished in 0.09s
```

Full suite and gates on this branch:

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result"
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 61 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.09s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s

$ cargo fmt --all --check                                    (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings       Finished, no warnings
$ python3 scripts/check-comments.py     ok — 231 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-steps.py        ok — 18 step(s), each with before, desired, expected and captured actual   (pre-019; this file added after)
$ python3 scripts/check-principles.py   ok — 14 principles, every named mechanism exists (14 CI jobs, 11 denied paths, 242 test fns seen)
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse, 14 job(s) defined
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
```

No CI gate was added, and no new denied path — `gates-can-fail`/`PRINCIPLES.md`
gain no new plant.

## Not verified

- **Never seen on a real terminal by a human.** Every measurement here is
  from a pty this process owns, driven synthetically (`printf` writing raw
  SGR into a `PtyAgent`). Nobody has looked at the rendered pane.
- **The colour mapping itself.** `Idx` → standard/bright ANSI 16-colour
  (`\x1b[3{n}m`/`\x1b[9{n-8}m` and the `4`/`10` background equivalents) and
  `Rgb` → truecolor (`\x1b[38;2;r;g;bm`) are built from the ANSI/vt100 spec,
  not verified against a real terminal's rendering.
- **Windows.** No Windows host is available. Nothing in this step — the new
  wire messages, `PtyAgent::screen_cells`, or the renderer — has run or been
  seen on Windows.
- **Terminal colour-capability detection.** The TUI does not detect whether
  the viewer's terminal understands 16-colour, 256-colour or truecolor, and
  does not degrade what it sends — it assumes the viewer's terminal
  understands the SGR it is given.
- **CJK display width.** Still deliberately wrong, unchanged from
  `steps/018`: `impl Cell for StyledCell` answers `1` for every cell, same
  as `impl Cell for char`.
