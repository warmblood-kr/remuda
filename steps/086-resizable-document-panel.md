# 086 — Resizable and collapsible document panel

## Before

The document panel appended after the session/terminal area at a fixed 42% of
the viewport. There was no divider to drag, no way to collapse the panel
without closing the workspace, and closing documents had no explicit test that
the attached terminal/session process survived. At the smallest tested window
the panel still owned the document controls, but keyboard focus was not
re-verified after resizing.

## Desired outcome

Keep the terminal and the document panel on one screen, with a visible divider
the user can drag to resize the panel. Let the user collapse the panel to a
narrow rail and restore it without losing open tabs, document state or the
attached session. Keep document keyboard focus usable at the smallest window,
and prove with an isolated daemon session that closing the document panel does
not stop the attached session process.

## Expected

- `LocalGui` stores a panel fraction (default 0.42), collapsed flag and drag
  flag. The panel width is `fraction × viewport`, clamped to at least 180 px and
  to leave at least 120 px for the session area.
- A `document-divider` (6 px, resize cursor) starts a drag on left mouse down;
  root mouse-move updates the fraction from the pointer x position, clamped
  0.18..0.62, and mouse-up ends the drag. The panel renders at the resulting
  width.
- Keyboard path: with the document panel focused, `d` moves focus to the
  divider. Left/Right resize by 16 px, Home/End clamp to 0.18/0.62, `c`
  collapses and moves focus to the collapse control, and Enter/Space on that
  control restores the panel and returns focus to the divider. The collapse
  control is focusable and has the same behavior on click.
- `document-collapse` hides the panel and shows a `document-rail` with
  `document-restore`; both preserve the `Workspace` and its tabs. Reopening a
  closed workspace resets the panel to expanded.
- At 420×320 the divider, pane frame and controls remain laid out; clicking the
  frame then pressing a key still reaches the active pane. Focus returns to the
  session input/collapse control when collapsing and to the divider when
  restoring, so the next key can resize again.
- Closing documents (`documents-toggle`) keeps `LocalGui.terminal` alive and the
  `terminal-screen` painted. With a private daemon and a real session, the
  session process is still `alive` after closing documents.
- The divider/rail live in `main.rs` around the existing single document panel
  call; the document module still owns tabs, content and controls.

## Actual

```text
cargo test --offline --manifest-path gui/Cargo.toml --bin remuda-gui
  58 passed; 5 ignored

cargo test --offline --manifest-path gui/Cargo.toml
  lib: 13 passed
  document_flow: 1 passed
  workspace_flow: 1 passed

cargo clippy --offline --manifest-path gui/Cargo.toml --all-targets -- -D warnings
  Finished

cargo check --offline --manifest-path gui/Cargo.toml \
  --target x86_64-pc-windows-gnu --tests
  Finished
```

`documents_pane_is_laid_out_and_operable_at_default_and_small_sizes` drags
`document-divider` 80 px left and asserts `document-workspace` widens; clicks
the frame, presses `d` and asserts the divider focus handle is focused; presses
Left and asserts the panel widens, Home/End and asserts fraction 0.18/0.62 and
bounds, `c` and asserts collapse plus collapse-control focus, Enter and asserts
restore plus divider focus and unchanged tabs; it also clicks collapse/restore,
resizes to 420×320, clicks the frame and asserts one keystroke reaches the
active pane, then closes documents and checks the session area remains.

`closing_documents_keeps_the_attached_session_process` starts a private daemon,
starts a real `sleep 60` session through `remuda run`, attaches the GUI terminal
to that session, closes documents through `documents-toggle`, asserts
`terminal-screen` is painted, waits 250 ms, and re-lists the private daemon to
assert the session is still alive. The daemon helper stops its private daemon
and host-backed sessions on drop; no live session or socket is used.

## Limits

- TestWindow keyboard simulation is the evidence; physical key events on a
  desktop keyboard are not separately exercised. Resizing uses a fixed 16 px
  step, not a user-configurable one.
- The panel fraction is not persisted across application restarts.
- The session-survival test runs on macOS with a private daemon and a real PTY;
  Windows remains a cross-compile check, not a runtime test.
- Collapse hides the whole document panel; individual pane collapse is not
  offered.
