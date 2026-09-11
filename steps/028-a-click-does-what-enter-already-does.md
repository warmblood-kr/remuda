# 028 — a click does what Enter already does

정수님, after steps/026 and steps/027 landed: "마우스 클릭도 지원합시다" (let's
support mouse clicks too). Two different builds hide under that one request:
⑴ click a session row in remuda's own list to select/enter it, and ⑵ forward
a click into the attached session (vim, htop, anything reading SGR mouse
reports). This step is ⑴ only — ⑵ without ⑴ still forces the keyboard to
pick a session first, so it is half a feature.

## Before

No mouse handling existed anywhere in the TUI. `crossterm::event::read()` in
`run` only matched `Event::Key`; any `Event::Mouse` fell through the wildcard
arm and was silently dropped. Nothing in the codebase enabled mouse
reporting mode (`\x1b[?1000h`/`\x1b[?1006h`) either — a real click never even
reached the terminal's own event stream.

The only mouse-shaped code that already existed, `core::keys::mouse` (used
by the Lua `click(name, col, row, button)` verb), is unrelated to this task:
it *encodes* a synthetic SGR byte sequence and sends it into an attached
session's pty via `Request::Send` — that is ⑵'s mechanism, programmatic and
one-directional, with no live mouse-event reading involved. It does not do
⑴, and nothing about ⑴ reuses it.

## Desired outcome

1. Mouse reporting (SGR mode: `\x1b[?1000h` for button events, `\x1b[?1006h`
   for extended coordinates — not the legacy X10 encoding, which breaks past
   column 223) is armed while the list has focus, so a real click reaches
   `run` as `crossterm::event::MouseEvent`.
2. `Ui::on_mouse` turns a left-button press inside the list column, on an
   actual session row, into the exact same `Action` `↓`-to-there plus `⏎`
   already produces (`focus_session`) — not a second, parallel selection
   mechanism.
3. A press outside any row (the header, or past the herd's last row), a
   release, or a drag are all no-ops — nothing panics, nothing selects.
4. Mouse capture is OFF whenever a session has focus, so the terminal's own
   mouse handling (drag-to-copy, the child's own scrollback) is untouched
   while attached, and stray reports never fight the keyboard for focus or
   leak into the attached session's own stream. `attach` (the standalone,
   single-session command) never enables capture at all — it has no list to
   click and forwards raw bytes, so a stray mouse report there would land
   inside the child unannounced.
5. The coordinate math is the same problem steps/027's caret fix already
   solved (terminal position → which pane → which row) — reused, not
   reimplemented, wherever the geometry is shared.

## Expected

- A click at the screen position of an actual session row selects that
  session and enters it, exactly as if the keyboard had moved there and
  pressed Enter.
- A click on the header row, or past the herd's last row, changes nothing.
- A bare `Up`/`Drag` mouse event never selects.
- Once a session has focus, `on_mouse` refuses on its own (belt and braces —
  capture is also off in `run` by then, so no such event should even arrive).
- Existing tests, `cargo fmt`, `cargo clippy -D warnings`, and every
  `scripts/check-*.py` gate all stay green.

## Actual

```
$ cargo test -p remuda-native --lib tui::tests::a_click tui::tests::a_release_or_drag_alone_never_selects -- --nocapture
test tui::tests::a_click_is_ignored_once_a_session_has_focus ... ok
test tui::tests::a_click_on_a_list_row_selects_and_enters_it_like_arrow_plus_enter ... ok
test tui::tests::a_click_outside_any_list_row_is_a_no_op ... ok
test tui::tests::a_release_or_drag_alone_never_selects ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 78 filtered out; finished in 0.00s
```

The offset math, concretely: a herd of `["a", "b"]` puts "b" at screen row 3
(1-based: header, "a", "b"). crossterm reports mouse coordinates 0-based, so
the wire event is `MouseEvent { kind: Down(Left), column: 5, row: 2, .. }`.
`on_mouse` adds 1 to both before comparing against the frame's own 1-based
geometry, computes list index `row - 2 = 0`... for row 3 (`row: 2` on the
wire) that is index `1`, matching "b". The test that proves this:

```rust
assert_eq!(ui.on_mouse(click(5, 2), 80, 24), Action::Nothing);
assert_eq!(ui.selected, 1, "clicked the second row");
assert_eq!(ui.focus, Focus::Session, "a click enters, same as Enter");
```

Full suite and gates on this branch:

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result"
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 82 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.73s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s

$ cargo fmt --all --check                                    (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings       Finished, no warnings
$ python3 scripts/check-comments.py     ok — 270 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-steps.py        ok — 28 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-principles.py   ok — 14 principles, every named mechanism exists (17 CI jobs, 11 denied paths, 274 test fns seen)
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse, 17 job(s) defined
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
```

No CI gate was added, no new denied path.

## Negative control

The pre-fix commit (`8a3dd3d`, steps/027's own merge) does not merely fail
these tests — `Ui` has no `on_mouse` method at all, so the tests cannot even
be expressed against it:

```
$ git show 8a3dd3d:native/src/tui.rs | grep -ni "mouse"
$ echo "exit: $?"
exit: 1
```

Zero hits — not one line of `tui.rs` mentioned mouse anything before this
step. The only mouse-shaped code anywhere in the pre-fix tree is
`core::keys::mouse`, the ⑵-flavored SGR encoder the Lua `click` verb uses to
inject a synthetic report into an attached session — confirmed unrelated to
this task's mechanism (see "Before" above).

## What this cost the user (steps/026's own kind of disclosure)

Enabling mouse reporting hands the terminal's *own* click/selection handling
to the program that requested it. While the list has focus, that is remuda
now — so the terminal's native drag-to-select / drag-to-copy over the list
column no longer works on its own; most terminals let a held `Shift`
override this and fall back to native selection, but that is
terminal-dependent, not something this change controls. This cost applies
only while the list has focus — capture is off the moment a session is
attached, so copying text out of a session pane is completely unaffected.

## Things checked, per axis

- **Mouse reporting already enabled somewhere?** No — confirmed by the grep
  above; this step is what first arms it.
- **The existing Lua `click` verb?** `core::keys::mouse` + `script.rs`'s
  `click(name, col, row, button)` binding — encodes a synthetic SGR report
  and sends it into a *named session's* pty via `Request::Send`. This is
  ⑵'s own mechanism (programmatic click-forwarding), not ⑴'s (reading a real
  mouse event to move remuda's own selection). Unrelated; not reused, not
  duplicated.
- **SGR vs legacy X10.** `crossterm::event::EnableMouseCapture` requests SGR
  extended mode itself (mode 1006 alongside 1000) — this codebase never
  hand-rolls the escape sequence, so there was no X10-vs-SGR choice to make
  in this code; crossterm's own parser (confirmed by reading
  `crossterm-0.29.0/src/event/sys/unix/parse.rs`) already normalizes SGR
  reports into 0-based `MouseEvent { column, row }`.
- **Coordinate translation reused, not reinvented.** `on_mouse` reuses the
  same 1-based row/column geometry `render`/`render_styled` already draw
  from (header at row 1, list rows below it, list column `layout()` already
  computes) — the reverse of the forward mapping, not a second
  implementation of it. It does not reuse `Viewport::map_cursor` itself
  (that maps *session-space* cells through a crop/pan; list rows have no
  crop or pan to invert), but shares the same row/column constants.
- **Release/drag and out-of-row clicks.** Explicit no-op branches (see
  "Actual" above) rather than an unhandled fallthrough.
- **Scroll wheel while a session has focus — MEASURED, not just inferred.**
  `sync_mouse_capture` in `run` disables capture the moment `ui.focus`
  leaves `Focus::List` (checked once per loop iteration, so it also catches
  `refresh`'s own `take()` forcing focus back to `List` on a failed hold —
  not just the keyboard-driven paths). With capture off, a scroll event
  never becomes an SGR report in the first place — the terminal handles it
  exactly as it would for any program that never asked for mouse reporting
  (typically native scrollback, terminal-dependent). This is inferred from
  `EnableMouseCapture`/`DisableMouseCapture`'s documented effect and from
  reading crossterm's own escape-sequence emission, not observed on a real
  terminal — no terminal was attached to this Linux-only, synthetic-input
  test run. `MouseCapture`'s own toggle (the actual escape sequences hitting
  a real terminal) is, like `RawMode` beside it, unverified by unit test in
  this codebase — an integration-only concern.

## Not verified

- **No real terminal.** Every assertion here reads `Action`/`Ui` state from
  a synthetic `MouseEvent` built in a test — nobody has clicked a real
  session row in a real terminal (Linux, macOS/ghostty, or Windows).
- **The mouse-capture toggle's actual escape-sequence effect.** `run`'s
  `sync_mouse_capture`/`MouseCapture` write real ANSI to stdout; like
  `RawMode`'s raw-mode/alternate-screen toggling beside it, this is not
  covered by a unit test in this codebase — verifying it needs an actual
  terminal, not a synthetic one.
- **Ambiguous-width Unicode and colour/CJK-alignment** are untouched —
  separate, not dispatched here.
- **⑵ (forwarding clicks into the attached session)** is explicitly out of
  scope for this step.
