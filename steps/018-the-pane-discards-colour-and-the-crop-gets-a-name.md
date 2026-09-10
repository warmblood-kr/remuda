# 018 — the pane discards colour, and the crop gets a name

Filed alongside `steps/017` as "no colour in Windows Terminal." That title
is wrong, and finding out why it's wrong is most of this step.

## Before

정수님 reported no colour anywhere in Windows Terminal — remuda's own chrome
and the child shell both. The chrome half turned out not to discriminate
anything: reading `render` shows the chrome never emits colour to begin with
(only reverse-video for the focus divider), so "chrome is colourless" was
always going to be true regardless of cause.

He then said, unprompted, what he'd actually been comparing: *"when I see
pane, I couldn't see any color. and when I type enter, it becomes fullscreen
of window frame. it would be an `attach` you mentioned."* He was comparing
the TUI's browsing pane against `remuda attach`, which hands over the whole
terminal. That is the whole diagnosis, and it has no platform dimension.

**The mechanism.** The pane reads a session's screen via `Request::Capture`,
served by `screen_text()` (`native/src/pty.rs`) → `vt100`'s
`parser.screen().contents()` — plain text, no escapes. `attach`'s initial
repaint uses the sibling `screen_bytes()` → `contents_formatted()`, which
keeps SGR. `core/src/agent.rs` already says as much: *"Default: the text,
which is correct but colourless."* Ruled out, tree-wide grep, zero hits
against 8 real `cfg(windows)` hits as a positive control: `ENABLE_VIRTUAL_
TERMINAL_PROCESSING`, `SetConsoleMode`, `GetConsoleMode`, `crossterm::style`.
No console-mode code is involved, because none exists on this path.

Noted so nobody rediscovers it as a third bug: `daemon.rs` sets the child's
`TERM` to `xterm-256color` only when the daemon's own `TERM` is unset or
exactly `dumb`; `COLORTERM` is never set. **Not the cause** —
`screen_text()` strips colour regardless of what the child emits.

**Why the obvious fix is not one.** `attach` can forward raw child bytes
verbatim because it owns the whole terminal — cursor moves and
erase-to-end-of-line in those bytes are computed for the child's full width
and land correctly. A pane cannot: those same bytes are computed for a width
the pane doesn't have, and replayed inside a cropped, side-by-side layout
they walk outside it and corrupt the surrounding UI (`client.rs` says this
outright). The pane asking for an already-interpreted snapshot is the
correct design, not the defect. The colour isn't lost — it's discarded one
step later: `vt100` has already parsed it and is holding it; `contents()`
throws it away on the way out, and `contents_formatted()` keeps it but
returns a whole-terminal repaint, the shape the pane can't consume.

## Desired outcome

1. The report is retitled: the pane discards colour at the capture call, on
   every platform — not a Windows bug.
2. The session→panel conversion `crop`/`fit` already did as three loose
   numbers (row offset, pan, clip) becomes a named class. 정수님, after the
   diagnosis above: *"we have two system, relative system. 두 개의 좌표계.
   panel 안의 session 좌표계, panel 전체 좌표계. inside of session 좌표계, we
   can handle its coordinate as-is, and when we need to draw whole screen,
   we just convert it to panel 전체 좌표계 simply. I think we need to
   introduce some kind of class. don't do inline calculation. introduce
   abstraction."* Then, explicitly: *"proceed to implement cascading
   coordinate systems too."*
3. Sequenced, not simultaneous: migrate the existing plain-text path onto
   that class first, verify it is byte-identical to what `crop` already
   produced, and only *after* that lands does colour ride on it. Building
   both in one change would let a broken pane be blamed on either the new
   abstraction or the new feature.
4. "Cascading" means the type composes — a further layer (a panel's
   rectangle onto the whole terminal) would be a second instance of the same
   class applied to this one's output — not a speculative multi-layer engine
   built for a layer that doesn't exist yet.
5. One more constraint, resolving a real conflict: `crop`/`fit` currently
   count `char`s, not display columns, so a wide (CJK) character — one
   `char`, two columns — already renders and pads wrong today, independent
   of colour, and 정수님 types Korean. If the new class assumed one column
   per cell, migrating onto it would silently fix that bug as a side effect,
   and a migration meant to prove "byte-identical, nothing changed" would
   stop being one — the oracle in (3) couldn't tell a real regression from
   an intended width fix. So: the class asks each cell its own width rather
   than assuming 1, the plain-text path answers "1" on purpose (today's
   wrong-for-CJK answer, kept only so the migration is provably a no-op),
   and fixing that answer is a later, isolated change against the same
   trait, with its own test.

## Expected

- `pty::tests::screen_text_strips_colour_that_screen_bytes_keeps` spawns a
  real child emitting SGR and shows `screen_text()` has no ESC byte while
  `screen_bytes()` keeps the SGR sequence — the reproduction, with no
  terminal and no Windows host.
- `Cell` is a trait with one method, `width() -> u16`; `impl Cell for char`
  answers `1` for every character.
- `Viewport` holds a row offset, a column offset (pan), a width and a
  height; `Viewport::bottom_anchored` derives the row offset from a source's
  row count the way `crop` always has; `Viewport::crop` is generic over
  `Cell` and returns each row with its own cut flag, leaving marker
  insertion (`→`) to the caller.
- `crop` (the free function) becomes a thin wrapper: turn the screen into
  `Vec<Vec<char>>`, build a `Viewport`, call its `crop`, turn the result
  back into strings with the same marker logic it always had.
- A byte-identity oracle compares `crop` against a frozen copy of its
  pre-migration algorithm across a spread of geometries, and passes.
- The full existing suite still passes unchanged — every test that asserts
  an exact frame string still asserts the same string.

## Actual

```
$ cargo test -p remuda-native --lib pty::tests::screen_text_strips_colour_that_screen_bytes_keeps -- --nocapture

running 1 test
test pty::tests::screen_text_strips_colour_that_screen_bytes_keeps ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 56 filtered out; finished in 0.01s
```

The byte-identity oracle was not trusted on the first pass. Its first
version swept ~8 hand-picked geometries and **passed even with a
deliberately injected off-by-one** in `Viewport::crop_row`'s cut threshold
(`total > offset + width + 1` instead of `+ width`) — the bug only shows at
`total == offset + width + 1` exactly, and none of the 8 picked geometries
happened to land on it:

```
$ cargo test -p remuda-native --lib tui::tests::the_viewport_migration_reproduces_crop_byte_for_byte -- --nocapture
running 1 test
test tui::tests::the_viewport_migration_reproduces_crop_byte_for_byte ... ok    # ← with the bug still in place
```

Rewrote the sweep to be exhaustive through and past both boundaries `crop`
actually branches on (a line exactly fills the pane; the screen exactly
fills the pane) instead of picking geometries by hand, re-ran with the same
injected bug still in place:

```
$ cargo test -p remuda-native --lib tui::tests::the_viewport_migration_reproduces_crop_byte_for_byte -- --nocapture

running 1 test

thread 'tui::tests::the_viewport_migration_reproduces_crop_byte_for_byte' panicked at native/src/tui.rs:987:25:
assertion `left == right` failed: screen="one line, no newline" cols=0 rows=1 pan=19
  left: ([""], false)
 right: (["→"], true)
test tui::tests::the_viewport_migration_reproduces_crop_byte_for_byte ... FAILED
```

Caught immediately, for the exact reason predicted. Reverted the injected
bug and confirmed green, then ran the full suite and gates on this branch:

```
$ cargo test --workspace 2>&1 | grep -E "^test result"
test result: ok. 0 passed;   ok. 21 passed;  ok. 10 passed;  ok. 5 passed;
test result: ok. 55 passed;  ok. 0 passed;   ok. 9 passed;   ok. 8 passed;
test result: ok. 7 passed;   ok. 4 passed;
$ cargo fmt --all --check                                   (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings      Finished
$ python3 scripts/check-comments.py     ok — 205 doc comment(s) within cap
$ python3 scripts/check-steps.py        ok — 17 step(s)
$ python3 scripts/check-principles.py   ok — 14 principles
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered
```

No CI gate was added, so `gates-can-fail` gains no new plant.

## Not verified

- **Anything on a Windows host.** No Windows host is available. The
  diagnosis (a code-path difference, not a platform difference) and this
  migration step are measured on Linux only.
- **Colour rendering itself.** This step is the first half of 정수님's
  sequencing — the migration, proven a no-op. Styled cells riding on
  `Viewport`, and the pane actually showing colour, are the next,
  not-yet-built increment.
- **The CJK display-width fix.** Named above as a real, pre-existing,
  separate bug this abstraction is shaped to absorb later — not fixed here,
  deliberately, because fixing it now would make this migration not a
  no-op and invalidate its own oracle.
