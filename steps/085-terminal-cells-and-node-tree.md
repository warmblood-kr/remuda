# 085 — Terminal cells and a node/session tree

## Before

`gui/src/terminal.rs` opened the attach PTY and its vt100 parser at a fixed
80x24 and never resized either. `Terminal::screen()` returned
`screen().contents()`, which discards every SGR attribute and the wide-cell
facts, and the sidebar rendered that string with `whitespace_nowrap` inside a
plain div. A window narrower or wider than 80 columns therefore kept the child's
old grid, clipped long lines instead of letting the child wrap at the pane's
width, and lost colours, backgrounds, bold/inverse/underline, wide emoji and
combining-cell alignment. The read-only preview had a separate plain-string path (`preview::plain`)
with the same style loss, so the first, non-attach screen could lose colour even
when the attach screen was fixed.

The sidebar was a node chooser followed by one flat list: only the selected
node's sessions were shown, and selecting a peer replaced the previous node's
children, so the TUI's node > session hierarchy was not visible at a glance.

## Desired outcome

Draw the attach screen as the terminal grid the child actually owns, at the
pane's measured columns and rows, with per-cell colours and attributes preserved;
expand/collapse nodes independently so several nodes' sessions can be seen and
selected in one view, without loosening the existing selection identity or
attach confirmation.

## Expected

- `Terminal` exposes a `Frame` of cells (`text`, fg/bg, bold/dim/italic/
  underline/inverse, wide, continuation) and resizes both the PTY (SIGWINCH) and
  the vt100 grid. A renderer measures the monospace cell from the same font it
  draws, asks for `floor(width/cell_w) x floor(height/line_h)`, and the cell divs
  use one or two cells per vt100 wide flag. No GPUI text rewrap is involved; the
  child supplies wrapping at the PTY width.
- ANSI 16/256/RGB colours, backgrounds, inverse, bold, italic, underline and
  dim survive into the rendered cells. Wide emoji and CJK occupy two cells and
  their continuation cell is skipped; combining marks remain in their cell.
- The tree renders `This device` and every peer as a node row with a chevron.
  Clicking the label selects the node and opens it; the chevron collapses or
  reopens it without hiding other nodes. Peer sessions are cached per
  fingerprint, so expanding one node does not clear another. Session identity
  remains the existing `Selection{peer,name,instance}` and `selection_current`
  gate; clicking a session under any visible node activates that node and keeps
  attach confirmation unchanged.
- The active node keeps the historical `session-N` selectors; other expanded
  nodes use a node-specific prefix so a multi-node tree has no duplicate
  selectors. Existing local/peer tests stay valid.
- The read-only preview carries `session_preview`'s structured cells through the
  worker payload and live `Sync`, applying the same control/secret filtering as
  the plain text but keeping fg/bg/attributes/wide flags. It renders through the
  same cell renderer, so the first screen has styles too; its captured width is
  not resized (a snapshot must not SIGWINCH the observed session).
- Default foreground and background resolve separately (`#e8eaf0` text on
  `#101216` background), explicit ANSI foreground/background keep their own
  colors, and inverse swaps the resolved pair rather than both defaults.
- Collapsing/expanding uses the chevron independently of the label; opening a
  peer starts its own refresh immediately (not only after selecting the label).
- The terminal frame's cursor is drawn as a cell overlay when visible, hidden
  when the child hides it, and a continuation-cell cursor highlights the wide
  glyph's first cell. The read-only preview has no live caret (`frame_from_styled`
  sets cursor hidden) because it is a snapshot, not an input target.
- Render cost is bounded: a grid is clamped to 20,000 cells by trimming rows,
  never columns, so a 500-column window keeps its width while a 500x300 request
  cannot create 150,000 cell elements. The PTY/parser resize holds the parser
  lock across `master.resize`, so the reader cannot parse new-width output into
  the old grid.
- Per-peer caches fail closed: `selection_current` requires the cache for that
  exact fingerprint and rejects a missing cache, revoked/stale peer or failed
  refresh instead of borrowing another peer's rows. The nodes result drops
  caches for peers that are no longer current, and a failed peer refresh
  invalidates that peer's cache.
- The tree is keyboard reachable: the sidebar owns focus; up/down move the flat
  node/session cursor, right expands a peer, left collapses the nearest node,
  enter selects the node or session under the cursor. Node/session rows show the
  real admitted/revoked/unavailable and running/exited/attached plus the first
  eight instance characters; no liveness is inferred beyond those facts.
- Selecting This device after a peer clears the remote selection, preview,
  pending reply and selected peer's cache, so no old remote frame remains the
  active target.
- The terminal/preview is no longer hidden when documents are open: the terminal
  target and the document pane can be visible in the same window. The exact
  resizable split is left to integration with the document worker.
- The preview worker grows an explicit ABI 2: text plus compact styled runs
  (`{t,f,b,s}` with short keys and a flags byte). ABI 1 keeps its JSON string
  payload and the untagged legacy form stays a bare string, so an older worker
  still renders as plain text and an unknown ABI fails loudly instead of
  silently downgrading. The new parent asks for ABI 2 and accepts v1/legacy
  string replies as text-only.
- The shared v1 `expand_runs` path is untouched: existing peers and the TUI keep
  its historical one-cell-per-scalar expansion. The GUI's ABI 2 reply is decoded
  by an opt-in `preview::expand_clusters` that keeps combining marks, ZWJ
  sequences, variation selectors, emoji modifiers and regional-indicator pairs
  in one `StyledCell`, so the GUI preview does not move a combining mark into
  the next column.
- Structured preview sanitizing is cluster-aware at the real strict wire shape.
  Strict decode expands a run to one scalar cell, so the tag-sequence check runs
  across the whole row; a legitimate ZWJ and a complete subdivision-flag tag
  sequence survive even when their scalars are in separate cells, while
  controls, bidi overrides and stray/hidden format characters are still
  dropped. Strict decode emits no wide continuation cells and empties a
  control/bidi-only cell, so every empty cell at this stage becomes one blank
  column (`" "`); it cannot be mistaken for a continuation and shift what
  follows.
- The parent asks for ABI 2, and on any preview/sync failure retries the same
  read-only request with ABI 1 exactly once, then remembers ABI 1 for later
  ticks. Input is never queued or replayed by this negotiation. If both fail,
  the existing restart/helper error path remains.
- Text-only replies (ABI 1, legacy strings, or the oversized-grid fallback) set
  `preview_cells` to `None`; the preview panel also refuses a zero-column grid
  and renders its text fallback, so an empty grid cannot reach a `cells[..1]`
  slice.
- The document pane reports whether its poll changed the page; an idle open
  pane no longer forces a full preview/cell rebuild every 150ms. The shared
  browser API is additive: `Browser::poll_changed() -> bool` and
  `Browser::poll()` keeps its old no-return behaviour; the GUI's
  `Pane::poll_changed()`/`Pane::cancel()` are thin wrappers. Both are hand-merge
  points for the document worker's image/cache/navigation work, not a change to
  its semantics.
- The structured reply is bounded by the existing worker stdout cap, not by
  raising it. If the compact runs would exceed the cap, the reply falls back to
  text only instead of truncating a row. The read-only preview renders at most
  the same 20,000-cell bound as the attach terminal and says how many rows are
  hidden; the 150ms timer repaints a static preview only when the frame or a
  status actually changed (an attached terminal still repaints from its live
  parser).
- A failed nodes refresh hides every cached peer's children and clears the
  remote preview/selection, so an old running/exited row cannot sit beside an
  unavailable node.

## Actual

```text
cargo test --offline --bin remuda-gui -- --test-threads=1
test result: ok. 76 passed; 0 failed; 6 ignored; 0 measured; 0 filtered out; finished in 45.00s
# plus preview module: 14 passed/1 ignored (ABI v1/v2/legacy, real strict-sync
#   Unicode path, cluster round trip, fallback negotiation, size limits)
# plus 6 terminal parser tests (ANSI, 256/bg/inverse, wrap, wide/combining,
#   default colors, filtered-empty vs wide continuation)
# plus remuda-core: 93 passed (expand_runs cluster cases, StyledScreen runs)

cargo clippy --offline --all-targets -- -D warnings   (in gui/)
Finished `dev` profile [unoptimized + debuginfo] target(s)

# isolated assembled path: private daemon at /private/tmp/gui-assembled-PID,
# REMUDA_RUNTIME_DIR/XDG state, one /bin/sh session, real remuda-gui worker
target/debug/remuda -s default -e "remuda.new('local-gui-proof', {'/bin/sh'})"
REMUDA_GUI_ACCEPTANCE_INPUT=1 ... --ignored --exact tests::real_local_daemon_reaches_session_row
test result: ok. 1 passed; 0 failed; 0 ignored; finished in 18.72s
```

Shared-wire compatibility: `core/src/protocol.rs` is byte-identical to base
56f99a02 (`git diff 56f99a02 -- core/src/protocol.rs` is empty), so the v1
`.chars()` expansion and its "exact inverse" doc claim were removed from this
branch, not redefined. Native fixtures pin the legacy result: an existing
`Response::StyledScreen` JSON with `e`+U+0301, a wide glyph and a ZWJ family
still expands to 8 scalar cells and `tui::render_styled_row` produces the same
bytes. The opt-in ABI 2 decoder is where clusters are reassembled; GUI tests
cover that round trip, including wide flags.

Old-binary transition was measured against an actual base-56f99a02 build. Its
ABI 2 preview invocation exits 1 with no stdout/stderr (argument parsing before
daemon access), which is exactly the case the one-shot negotiation handles. The
ignored `real_old_worker_negotiates_abi1_through_provider` test ran against that
binary and a private daemon/session: preview fell back to ABI 1 (text-only,
non-empty) and updated the remembered ABI to 1; sync then used ABI 1 and
returned the real instance. Earlier, both binaries called with ABI 1 returned
identical string preview payloads (1944 bytes) and identical sync
text/version/instance (the new v1 sync adds `cells: []`, which the old
`SyncFrame` ignores). TUI regressions: `tui::` 445 passed/1 ignored,
`cluster_tui::` 238, `cluster_remote::` 33, `session_preview::` 9. One
real-daemon visual test needs `TMPDIR=/tmp` on this host because the default
temp path is too long for its socket name; it passes there and is unrelated to
cells.

Measured structured-reply sizes (one run per cell, the worst practical shape):
120x60 compact 304,622 bytes vs 940,819 for the old per-cell shape; 200x60
compact 507,454 bytes vs 1,567,879, i.e. the old shape exceeded the 1,048,576
byte provider cap and the compact one does not. A 65,536-cell all-distinct
grid is 3+ MB as runs, so `encode_preview` drops the rows (text stays) instead
of crossing the cap.

`terminal::tests` parses real ANSI bytes with vt100 and pins:
`filtered_empty_cell_is_a_blank_not_a_wide_continuation` (a filtered empty cell
stays a one-column blank; only the cell directly after a wide glyph is a
continuation),
`default_foreground_and_background_resolve_by_role_and_inverse_swaps` (default
text and background differ, inverse swaps the pair, explicit ANSI colours keep
their values),
`colors_attributes_and_reset_survive_the_frame` (red, bold, reset),
`background_inverse_and_256_color_are_carried`,
`wrapping_follows_the_pty_width_not_a_text_widget` (5-column wrap), and
`wide_emoji_and_combining_marks_keep_their_cells`.

`terminal_grid_resizes_with_the_window_and_keeps_cell_frames` drives a GPUI
TestWindow with a parser-only terminal, resizes it small -> wide -> small and
asserts the PTY/parser columns and rows follow the measured pane at each size,
the frame dimensions match the final size, and `terminal-row-0` is laid out.
`clicking_session_displays_current_snapshot` supplies styled preview cells and
requires `preview-row-0`, proving the first screen uses the cell path.
`real_strict_sync_wire_keeps_bidi_column_and_emoji_clusters` is the real wire
regression: `Response::Sync` → `decode_sync` (strict scalar cells, bidi cell
emptied) → `structured` → ABI 2 → `read_preview` → `frame_from_styled`. It
requires a ZWJ family and a tag flag to rejoin into two wide cells, keeps the
bidi column as a non-continuation blank, and checks X's position.
`preview_render_is_bounded_and_reports_hidden_rows` lays out a 200x201 preview,
sees row 99 but not row 100, and requires the `preview-truncated` notice.
`nodes_refresh_failure_hides_cached_peer_children` primes a peer cache and
selection, fails the nodes result, and requires the cached children, remote
preview, selection and tree entries to disappear.
`tree_is_keyboard_reachable_and_collapses_with_left` drives the sidebar with
down/down/right/down/enter, selects a peer session and collapses with left.
`tree_expands_several_nodes_and_collapses_one_without_hiding_others` shows local
plus two peers' children at once, collapses peer A's chevron and observes peer
B move up while local and B stay visible, reopens A, then selects B's session
and checks the pinned peer/name.

The assembled run drives the real GUI TestWindow against a private daemon and a
real `/bin/sh` session: row discovery, line input to the session, Sync output,
attach through the GUI, typed terminal output, detach from the terminal pane,
and a daemon List assertion that the session is still `alive` and no longer
`attached` after detach. Only that private daemon, its runtime directory and its
session are removed.

## Limitations

- The terminal and document pane now coexist, but the split is the flex layout's
  automatic vertical division, not a draggable/collapsible divider; that is part
  of the integration with the document worker's pane.
- The preview keeps the captured grid width. A pane narrower than the snapshot
  clips rather than rewrapping, because resizing would change the observed
  session; the attach screen is the resizable one.
- A v1/legacy worker reply is text-only: after ABI 1 fallback the preview has no
  colours or cluster cells. If such a worker supplies a cell grid from the old
  `expand_runs`, a combining or ZWJ sequence arrives as scalar cells; the text
  bytes still render the same, but column alignment around wide/ZWJ glyphs may
  differ. The GUI's own ABI 2 worker fixes that; an older worker cannot be
  retrofitted without swapping its binary.
- Tree keyboard focus is the sidebar focus handle and the cursor has no visual
  highlight yet; node/session selection by mouse remains the primary path.
- The assembled path is verified on this macOS host only; Windows/macOS
  candidate differences beyond the existing boundaries are not exercised. The
  real M1 screen is still not claimed.
- The node rows show the admitted/revoked/unavailable state the GUI already
  knows; they do not infer liveness beyond the session rows' running/exited/
  unavailable status.
- No real M1 screen was observed; TestWindow, parser fixtures and the isolated
  private-daemon path are the evidence. Nothing was merged, pushed or deployed,
  and no other checkout or live session was touched.
