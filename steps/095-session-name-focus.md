# 095 — Full session names at any sidebar width

## Before

Compact rows keep one line by truncating the name at the row edge. At the
default 200 px sidebar a 16-character name was clipped mid-name (the earlier
framebuffer capture shows `refactor-runtim`), and there was no way to read the
rest: the tooltip-less row had no full-name surface, and the status word or
marker already owned part of the line. Reducing density or wrapping every row
would undo the one-line layout the mockup asks for.

## Desired outcome

Rows stay one line, but the full name of the row the user is pointing at or
has moved to with the keyboard is always readable, at 1000 px and at the
420 px narrow width. Selection identity, input target and the TUI's own
full-name display stay unchanged.

## Expected

- A sidebar focus line under the `NODES` label shows the full
  `Session.name` of the row under the keyboard cursor, without truncation: it
  wraps instead of clipping, so a 36-character Unicode name is complete at the
  narrow width too.
- `Node(None)` shows `This device`; a peer node shows the peer label without
  the identity quotes; a peer session shows that session's full name.
- Clicking a session row moves `tree_cursor` to that row before activating it,
  so the focus line follows the mouse as well as Up/Down. Activation, input
  focus and `(peer, name, instance)` selection are unchanged.
- The focus line is a header child: `TREE_FIXED_HEADER_CHILDREN` becomes 3
  (title, section label, focus line) and the reveal-indices unit test expects
  `[3, 5, 6, 8]`. The bounds-based reveal test computes the index from the
  helper, so it still checks the painted row.
- No new identity: the line displays `Session.name` / `Peer.label` only; the
  row's full `path` or instance binding is still what opens and routes.
- The TUI already renders full session names from the same `Session.name`;
  this adds no second display name and no truncation in the shared layer.

## Actual

Source-only while the wheel worker owns the build slot. `rustfmt --check`
parses the file and `git diff --check` is clean; none of the new tests has run.

New tests:

- `tests::tree_focus_name_is_the_full_identity_and_follows_the_cursor`
  (local node, long local session, peer label, peer session).
- `tests::sidebar_focus_line_wraps_the_full_long_name_and_follows_mouse`
  (keyboard Down exposes a 36-character Unicode name, the line wraps and stays
  in the sidebar, clicking the second row moves the cursor and shows
  `second`).
- `sidebar_child_indices_match_conditional_rows` updated for the new header
  child.

Build plan when the slot returns: targeted focus/indices tests, full GUI bin
suite, private smoke with the candidate and the preserved 743 worker, peer
live smoke, then an instrumented capture of the focus line at 1000 px and
420 px.
