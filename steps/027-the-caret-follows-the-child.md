# 027 — the caret follows the child

정수님 tested `steps/026`'s flicker/lag fix on the nightly it shipped in and
confirmed it worked: "wow, it is much better now." His next report, verbatim:
"one more. i can't see cursor caret. but it is an important information too.
can't we support cursor showing?"

## Before

The dispatcher's hypothesis was that `render_styled` paints cells but never
emits a cursor-position-and-show sequence. Reading the code turned up
something one layer deeper: the mechanism did not exist to emit at all.

`AgentProcess::cursor()` and `PtyAgent::cursor()` (`native/src/pty.rs`)
already existed and already answered `vt100`'s `cursor_position()` correctly
— but nothing in `core/src/protocol.rs`'s wire shapes carried a cursor
anywhere. `Request::CaptureStyled`/`Response::StyledScreen` — the one round
trip the TUI makes per refresh for the focused pane — sent only `StyledRun`
rows. The daemon-process/TUI-process split means the TUI has no other way to
learn where the child's cursor is; `registry.cursor()` exists in `core` but
is only ever called from tests and from the (separate, `Attach`-only)
`attach` path, never from anything the TUI's own pane rendering reaches.

Confirmed with a source grep on the pre-fix commit (`4e8d3fd`, `steps/026`'s
own merge): zero occurrences of `?25` (DECTCEM show/hide) anywhere in
`native/src/tui.rs`, and every other hit for the word "cursor" in that file
is either the list's `▸` selection marker or a doc comment — never a real
terminal escape aimed at the pane. The bug was not "wrong values reach the
renderer"; it was "no cursor data reaches the renderer at all".

## Desired outcome

1. `Cursor` (`core/src/agent.rs`) gains `pub visible: bool` — the child's own
   DECTCEM state, read from `vt100::Screen::hide_cursor()` (confirmed by
   reading the pinned `vt100` 0.16.2 source: `hide_cursor()` is
   `self.mode(MODE_HIDE_CURSOR)`, a real tracked mode, not something this
   process would have to re-derive from raw bytes itself).
2. `Response::StyledScreen` carries the session's `Cursor` alongside its rows
   on the same round trip, so the pane's content and its caret can never
   disagree about which frame they came from — no second request, no new
   latency source next to `steps/026`'s fix.
3. `Viewport` (already generic over `Cell`, already the seam `steps/018`
   built for exactly this kind of reuse) gains `map_cursor`: a session-space
   `(row, col)` folds through the same width-by-width column math
   `crop_row` already uses, so a cursor after a wide cell lands after it,
   not on its zero-width continuation — and returns `None` (not a clamped
   guess) when the cursor is scrolled above the bottom-anchored viewport or
   panned past its visible columns.
4. `render_styled` positions the real terminal's caret with
   `\x1b[{row};{col}H\x1b[?25h`, translated from panel-local to the pane's
   actual on-screen offset (list column + divider + header row), or emits an
   explicit `\x1b[?25l` when the child hid its own caret or the cursor is out
   of view. The sequence lands inside the existing `\x1b[?2026h`…`\x1b[?2026l`
   synchronized block from `steps/026`, as the very last thing before the
   end marker — and after, not before, the trailing `\x1b[J`, which must
   still erase from the footer's own cursor position, not from wherever the
   caret is about to be moved to.
5. The list column and its rows are untouched — there is exactly one caret
   sequence per frame, emitted once at the end, so it structurally cannot
   land inside a list row.

## Expected

- `pty::tests::cursor_reports_the_childs_own_hide_request`: a real child that
  emits `\x1b[?25l` produces `cursor.visible == false` — `vt100`'s own
  tracked mode, not an inference from raw bytes.
- `tui::tests::a_focused_cursor_lands_at_its_absolute_screen_position_not_at_origin`:
  session `(2, 3)` with a real 16-wide list column and header row in front of
  it lands at absolute `(3, 21)` — proves the offset math on a pane that is
  not at screen origin, not just the trivial row-0-col-0 case.
- `tui::tests::a_hidden_cursor_never_gets_a_show_sequence`: `visible: false`
  never produces `\x1b[?25h`, and produces an explicit `\x1b[?25l`.
- `tui::tests::a_cursor_scrolled_out_of_the_viewport_is_hidden_not_clamped`:
  more session rows than the pane's body height hides the caret rather than
  clamping it to a wrong row.
- `tui::tests::a_cursor_panned_out_of_view_is_hidden`: a pan past the
  cursor's own column hides it rather than showing it at a wrong column.
- Every assertion above checks the tail of the frame string directly
  (`out.ends_with(...)`), which is also the proof that the caret sequence
  sits inside the sync block, after `\x1b[J`, before `\x1b[?2026l`.
- The full existing suite, `cargo fmt`, `cargo clippy` and every
  `scripts/check-*.py` gate stay clean.

## Actual

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result"
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 78 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.09s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.72s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s

$ cargo fmt --all --check                                    (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings       Finished, no warnings

$ python3 scripts/check-comments.py     ok — 263 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-steps.py        ok — 26 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-principles.py   ok — 14 principles, every named mechanism exists (17 CI jobs, 11 denied paths, 267 test fns seen)
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse, 17 job(s) defined
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
```

The offset-math test's actual emitted tail, captured directly from a
temporary `eprintln!` inside the test run before it was removed:

```
TAIL="t                                   \u{1b}[J\u{1b}[3;21H\u{1b}[?25h\u{1b}[?2026l"
```

**Negative control**: the pre-fix commit (`4e8d3fd`, `steps/026`'s merge)
does not merely fail this test — its `render_styled` has no `cursor`
parameter at all, so the test cannot even be expressed against it. A source
grep on that commit for `?25` inside `native/src/tui.rs` returns nothing:

```
$ git show 4e8d3fd:native/src/tui.rs | grep -n '?25\|cursor'
99:    /// Keep the cursor on a real row after the herd changes underneath it.
130:    /// keyboard back when it is gone. Rows move under the cursor when the herd
297:/// **widest** session needs — from the herd, not the cursor, so the divider does
450:/// The whole frame as one string of text and ANSI cursor moves. Pure on
673:/// from the herd rather than the cursor, so the divider does not jump. Zero
703:    let cursor = if row == ui.selected { "▸" } else { " " };
715:    format!("{cursor} {} {tail}", fit(&session.name, room as u16))
1589:        assert!(frame.contains("▸ claude"), "the cursor is on the first row");
```

Every hit is the list's own `▸` selection marker or a doc comment — none is
a real terminal caret sequence for the pane. The feature was entirely
absent, not merely buggy.

No CI gate was added, no new denied path.

## Not verified

- **Whether this is visible correctly on a real terminal.** Every assertion
  above reads the emitted escape-sequence string; nobody has watched a real
  terminal's caret actually move to the right cell. Built and tested on
  Linux, headless, against synthetic fixtures — no Windows or macOS/ghostty
  host is available here.
- **The single-column edge case where a row is both cut (panned wider than
  the pane) and the cursor sits in the exact rightmost visible column.**
  `crop_styled`'s cut-marker logic can overwrite that column with `→` after
  `locate_cursor` already computed the caret's position there, so the caret
  could in principle coincide with the arrow glyph rather than real content.
  Not specifically tested; judged rare enough (requires the cursor to sit
  exactly at the pane's right edge on a row that is also horizontally
  cropped) not to warrant extra machinery beyond noting it here.
- **정수님's Windows observation (color/flicker/CJK)** stays open and
  untouched — this PR is scoped to the cursor caret only.

## Not this PR

- Ambiguous-width Unicode (`steps/023`'s open item) — untouched.
- Colour/CJK-alignment work — untouched, separate and not dispatched here.
