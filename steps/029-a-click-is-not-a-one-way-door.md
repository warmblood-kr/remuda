# 029 — a click is not a one-way door

정수님 tested steps/028's mouse-click PR immediately and hit the hole it
left, verbatim: "클릭해서 세션 안으로 커서가 들어가면, 바깥 세션 목록을 클릭해도
아무 반응이 없네요? 세션 목록을 클릭하면 클릭된 그 다른 세션으로 이동할걸
기대했어요." (Once a click takes the cursor into a session, clicking the
list outside has no effect — he expected clicking a different session to
switch straight to it, the way terminals traditionally behave.)

steps/028 built exactly what it specified: mouse capture on only while the
list has focus, off the instant a session was entered. That spec makes the
mouse a one-way door — in, never back out by clicking. The split into ⑴
(list click) and ⑵ (forward-into-session) turns out not to be separable:
once capture has to stay on while attached, a click landing inside the
session pane needs a defined behaviour, which is ⑵ itself.

## Before

Two distinct bugs, at two different layers, both reachable the moment a
session was attached:

1. **Mouse capture turned off on attach.** `sync_mouse_capture` disabled
   `EnableMouseCapture` the instant `ui.focus` left `Focus::List`, so no
   `Event::Mouse` ever reached `run` again until the keyboard detached.
2. **Even bypassing that, the hold itself never moved.** `refresh`'s hold
   logic was `if focus == List { drop } else if held.is_none() { take }` —
   once a hold existed, nothing ever re-took it for a *different* name.
   Worse: `follow_focus(held's name)` ran first and would have snapped
   `ui.selected` back to the already-held session on the very next tick,
   silently undoing any attempt to point `ui.selected` elsewhere.

Bug 2 is the one that actually decides whether this fix works, and it is
proven directly against the pre-fix commit, not just reasoned about — see
"Negative control" below.

## Desired outcome

1. Mouse capture stays enabled for the whole `run`, not just while the list
   has focus — a click has to reach `on_mouse` from either pane.
2. A click in the list column switches straight to that session — REQUIRED,
   regardless of whether another session is already attached.
3. A click in the session pane forwards as a real SGR mouse report to the
   child, translated into the child's own pty-cell coordinates — chosen (⒝)
   because the encoder needed (`core::keys::mouse`) already existed for the
   Lua `click` verb, making it no more work than swallowing the click (⒜)
   silently.
4. The actual `Hold` — not just `Ui` state — follows a session switch
   immediately, via a new `reconcile_hold`, so `refresh`'s own reordering
   correction (`follow_focus`) never fights a fresh, deliberate choice.
5. #026's synchronized-output frame and #027's cursor caret are untouched.

## Expected

- Attach session A, click session B's list row: focus and the real held
  attach both move to B — not just `Ui.selected`.
- A left click inside the session pane reaches the child at its own
  coordinates, not remuda's screen coordinates.
- A scroll wheel while attached never reaches the child.
- The pre-fix commit (#028's own merge, `f034f73`) demonstrably keeps the
  stale hold — proven by running the fix's own scenario against it.

## Actual

```
$ cargo test -p remuda-native --lib tui::tests::a_click_on_a_different_list_row_switches_focus_even_while_attached -- --nocapture
test tui::tests::a_click_on_a_different_list_row_switches_focus_even_while_attached ... ok

$ cargo test -p remuda-native --lib tui::tests::a_click_inside_the_session_pane_is_forwarded_to_the_child_as_a_click -- --nocapture
test tui::tests::a_click_inside_the_session_pane_is_forwarded_to_the_child_as_a_click ... ok

$ cargo test -p remuda-native --lib tui::tests::reconcile_hold_switches_the_real_attach_not_just_ui_state -- --nocapture
test tui::tests::reconcile_hold_switches_the_real_attach_not_just_ui_state ... ok

$ cargo test -p remuda-native --lib tui::tests::a_scroll_wheel_while_attached_is_not_forwarded -- --nocapture
test tui::tests::a_scroll_wheel_while_attached_is_not_forwarded ... ok
```

The switch, concretely, at the `Ui` level:

```rust
assert_eq!(ui.on_key(press(KeyCode::Enter)), Action::Focus("a".into()));
assert_eq!(
    ui.on_mouse(click(5, 2), 80, 24),
    Action::Focus("b".into()),
    "row 2 (0-based) is \"b\" — clicking it must switch, not be ignored"
);
```

The forward, concretely — `layout(80, 80)` puts the session pane at screen
columns 18..=80; a click at screen (col 20, row 5) (1-based) becomes pane
position (3, 5). `Size::new(80, 24)`'s pty is one row taller than the
23-row body in an 80×24 terminal, so bottom-anchoring (steps/023) puts the
child's own row at 6, not 5:

```rust
let expected = remuda_core::keys::mouse("left", 3, 6).unwrap();
assert_eq!(ui.on_mouse(event, 80, 24), Action::Type(expected));
```

`reconcile_hold`, against a real daemon and two real sessions — the layer
that actually matters, since `Ui` state alone does not prove the pty
attach moved:

```rust
ui.selected = 0;
reconcile_hold(&path, &mut ui, &mut held, &first);
assert_eq!(held.as_ref().map(|(n, _)| n.as_str()), Some(first.as_str()));

ui.selected = 1;
reconcile_hold(&path, &mut ui, &mut held, &second);
assert_eq!(held.as_ref().map(|(n, _)| n.as_str()), Some(second.as_str()));
```

Full suite and gates on this branch:

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result"
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 87 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.09s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s

$ cargo fmt --all --check                                    (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings       Finished, no warnings
$ python3 scripts/check-comments.py     ok — 277 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-steps.py        ok — 28 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-principles.py   ok — 14 principles, every named mechanism exists (17 CI jobs, 11 denied paths, 281 test fns seen)
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse, 17 job(s) defined
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
```

`check-steps.py` still says 28 — this file becomes the 29th once counted
post-merge; the number above is the pre-merge state on this branch's base.

No CI gate was added, no new denied path.

## Negative control

Run against the pre-fix commit (`f034f73`, steps/028's own merge), in a
throwaway worktree, using ONLY mechanisms that already existed there
(`refresh`, `held`, `take` — `reconcile_hold` does not exist yet):

```rust
// Attach the first, exactly as Enter would.
ui.selected = 0;
ui.focus = Focus::Session;
refresh(&path, "default", &mut ui, &mut held, &mut painted, false).unwrap();
assert_eq!(held.as_ref().unwrap().0, first, "attached to the first");

// What a click on the SECOND row would set, per steps/028's own on_mouse.
ui.selected = 1;
refresh(&path, "default", &mut ui, &mut held, &mut painted, false).unwrap();

assert_eq!(ui.selected().unwrap().name, first, "BUG: selection snapped back");
assert_eq!(held.as_ref().unwrap().0, first, "BUG: the real attach never left the first session");
```

```
$ cargo test -p remuda-native --lib tui::tests::negctrl_switching_selection_while_attached_does_not_switch_the_hold -- --nocapture
test tui::tests::negctrl_switching_selection_while_attached_does_not_switch_the_hold ... ok
```

Both assertions PASS at the pre-fix commit — proving the bug is real and
reachable through the exact code path a click already used, not merely
theoretical. The identical assertions, run against the fixed code, would
fail (selection moves to `second`, the hold follows it) — that reversal is
what steps/029 changes. The throwaway worktree was removed afterward; this
test was never committed to the real history.

## Things checked, per axis

- **Was the wheel-leak concern (from steps/028) real, or precautionary?**
  Precautionary, on inspection — not a real risk given how this codebase is
  built. `crossterm::event::read()` always decodes the outer terminal's raw
  mouse bytes into a structured `MouseEvent` before any of this code runs;
  the child's pty is a completely different file descriptor. There is no
  code path by which a raw escape sequence from the outer terminal could
  ever land in the child's pty — only the bytes `on_mouse` explicitly
  builds and returns as `Action::Type` ever reach `hold.keys(...)`. Keeping
  capture on for the whole run does not change this: `on_mouse` only builds
  bytes for a left-button `Down` (`a_scroll_wheel_while_attached_is_not_forwarded`
  proves a wheel event produces `Action::Nothing`).
- **Is the child actually receiving mouse sequences now?** Yes, for a left
  click landing in the pane — `click_session_pane` builds a real SGR report
  via `remuda_core::keys::mouse("left", child_col, child_row)` and returns
  it as `Action::Type`, which `run`'s existing `Action::Type` arm already
  writes straight to `hold.keys(&bytes)` — no new write path was added.
  Release, drag, and wheel are NOT forwarded (see "Not verified").
- **Region partitioning, unified not tripled.** `on_mouse` computes
  `list_w`/`preview_w`/`body` once (the same `layout()` call #028 already
  used) and branches on which side of the divider the click landed —
  `click_list_row` and `click_session_pane` share that one geometry
  computation rather than each deriving it independently. The pane-click
  path reuses the session's own fixed `Size` (from the herd list, already
  known — the pty never resizes, PRINCIPLES §6) rather than needing a live
  styled-cell capture just to place a click; it does not reuse
  `Viewport::map_cursor` (steps/027) — that inverts a crop/pan over
  *content* cells, and mouse coordinates are raw pty-cell positions with no
  character-width folding to invert — but does share the same bottom-anchor
  row-offset arithmetic (`total_rows.saturating_sub(body)`).
- **Same-work test for ⒜ vs ⒝.** ⒝ turned out to be the smaller/cheaper
  path: `core::keys::mouse` already existed and already bundles a full
  press+release burst; the only new code was the coordinate translation
  ⒜ would have needed anyway (to know a click landed in the pane at all, in
  order to correctly do nothing). Built ⒝ for `Down(Left)` only; wheel,
  drag, and other buttons remain ⒜ (silently swallowed) — see "Not
  verified" for why extending those further was not free the same way.

## Not verified

- **No real terminal.** Every assertion here reads `Action`/`Hold` state
  from a synthetic `MouseEvent` and a real (but headless) daemon on Linux —
  nobody has clicked a real session pane in Windows Terminal, macOS/ghostty,
  or any other real terminal.
- **Drag, release, and non-left buttons landing in the session pane are
  still swallowed (⒜), not forwarded.** `core::keys::mouse` has no
  ready-made encoding for a bare release or a drag motion (it always
  bundles press+release into one "click"), and wheel forwarding would need
  its own encoding path — building full drag-selection or wheel-scroll
  parity with a native terminal is materially more work than the left-click
  case and was not attempted here. A user selecting text by dragging inside
  an attached session, or scrolling a pager like `less` with the wheel,
  will not see that reach the child.
- **A cursor at the exact rightmost column of a horizontally-cropped row
  coinciding with the cut-marker `→`** — the same named-but-unhandled edge
  case from steps/027, still open, unrelated to mouse handling.
- **Ambiguous-width Unicode and colour/CJK-alignment** are untouched —
  separate, not dispatched here.
