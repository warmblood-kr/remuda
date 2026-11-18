# 094 — One line per node and session

## Before

A node row stacked its label, fingerprint and admitted state on three lines.
A session row stacked the name, an optional validated detail and a status line,
and selecting a session added a `SELECTED` banner line. At the 160–200 px
sidebar this wasted most of the panel and pushed sessions off screen. The
approved mockup shows one node line and one indented session line with its
status and a selection background.

## Desired outcome

Each node is one line (name, short identity hint, state) and each session is
one line (marker, name, compact status), with the detail line only when the
daemon supplies one. Selection is a background; the keyboard cursor keeps its
own border and marker. Long names and narrow panels stay single-line by
ellipsis, never by wrapping into a tall row. Selection identity, click target
and scroll/reveal behavior are unchanged.

## Expected

- Session status is `running`, `exited` or `unavailable`, plus `·A` when
  attached; the eight-character instance stays in the selection identity, not
  in the visual row.
- Name and status share one flex line; the name keeps a readable `min_w(56)`
  while status shrinks to at most 96 px, both `whitespace_nowrap` and
  `text_ellipsis`; the first own-framebuffer capture showed that `min_w(0)`
  starved the name to zero width and hard-clipped it, so this is now bounded.
- The peer node shows its label and state on one line; the fingerprint stays in
  the tree/routing key and is no longer a visual column. While nodes are
  loading the state reads `loading`, not `unavailable`.
- The indent moves to the row padding (`16 + depth × TREE_INDENT_PX`) so the
  flex row has a real width; the detail line aligns under the name.
- `selected-session` remains a small marker inside the selected row, so the
  existing hook survives while selection no longer adds a line.
- Peer node: name, first eight fingerprint characters and admitted/revoked/
  unavailable state on one line with its own `node-peer-state-N` selector.
- Tests: name/status share a vertical band at 1000 px, selected and unselected
  rows have equal height, the node is one line, and at 420 px with a very long
  Unicode name the name and status stay inside the sidebar.

## Actual

Built and run in this stage: full GUI bin suite **160 passed / 0 failed /
8 ignored** (79.70 s) after the legibility fix, plus the semantic, cursor,
indent, fallback and focus filters. An own-framebuffer capture with the
instrumented GPUI fork (`/private/tmp/gui-capture-fac8f202`, non-release)
confirmed one-line density and exposed the zero-width name clipping fixed
above; the corrected capture is kept in `/private/tmp/capture-fac8f202.png`.
The capture is app-framebuffer evidence, not a physical-display or input test.
