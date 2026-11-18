# 090 — Hierarchy review fixes on the delivered GUI base

## Before

`6512aa7d` added the validated hierarchy sidecar to the 3c821 GUI base, and its
source review found usability and safety gaps rather than data errors:

- a hierarchy level indented `depth * 2` logical pixels, so a child row was
  visually almost level with its parent;
- the sidebar did not scroll, so a long tree pushed rows past the window with
  no way to reach them, and the keyboard cursor could move off screen;
- the keyboard cursor had no visible marker, so keyboard navigation was
  invisible until Enter selected something;
- the local view and `List` fallback requests used the client's default
  timeout; a daemon that accepted the connection but never answered could
  outlive the worker's 8 s watchdog, losing the fallback;
- the Enter handler for a peer row fell back to the *local* session list when
  the peer cache missed, so a remote index could select an unrelated local
  session;
- a flat fallback list looked identical to a hierarchy, so a user could not
  tell that hierarchy was unavailable.

## Desired outcome

Hierarchy is visible at every window size, the tree is reachable by wheel and
keyboard, the keyboard cursor is visible and distinct from selection, a silent
daemon still reaches the flat fallback, a remote row can never resolve to a
local session, and a flat fallback says so. Liveness stays `alive`; `state`,
`depth` and `detail` remain advisory display fields and never affect routing,
selection identity or permissions.

## Expected

- `TREE_INDENT_PX = 12.0` and `TREE_DETAIL_OFFSET_PX = 3.0` name the visual
  hierarchy step; three nested levels stay inside a 420 px window.
- The sidebar owns a `ScrollHandle` (`overflow_y_scroll` + `track_scroll`), so
  the wheel scrolls the tree. Moving the keyboard cursor calls
  `scroll_to_item` on the matching direct child.
- `TREE_FIXED_HEADER_CHILDREN = 3` (title, section label, local node) plus the
  conditional helper notice; `sidebar_child_indices` reproduces the rendered
  child order including the fallback and empty notices that are not keyboard
  targets, so the reveal index matches layout.
- `bounded_request` caps one connect/send/read at `VIEW_REQUEST_TIMEOUT = 3 s`;
  the view request and the `List` fallback therefore fit under the 8 s worker
  watchdog even when the daemon stalls.
- `tree_session_at` resolves only `(peer, index)` against that peer's rows and
  refuses on a cache miss or unavailable peer; it never indexes local rows.
- A row without the instance-bound sidecar shows
  `Hierarchy unavailable · flat session list` under the local node and under
  each expanded peer; structured rows show no notice. Rows stay selectable.
- The keyboard cursor has its own `-cursor` marker and row border, separate
  from `SELECTED`, so selection and focus are distinguishable.
- Document, image, input/focus guards, selection identity and the flat payload
  are unchanged.

## Actual

Source-only on top of `6512aa7d` while the CLI worker held the build slot, so
none of the new tests have run yet. `rustfmt --check` parses the file and
`git diff --check` is clean. The build-slot plan is: rebuild `remuda` and
`remuda-session-host` from this exact revision (matching build ids), run the
targeted filters below, then one full GUI bin suite, the private document/image
smoke with both the candidate and the preserved 743 worker, and the peer live
smoke.

New tests, source-only:

- `tree_session_at_never_uses_local_rows_for_a_peer`
- `sidebar_child_indices_match_conditional_rows`
- `bounded_request_gives_up_before_the_worker_watchdog`
- `keyboard_cursor_is_visible_and_scrolls_into_view`
- `narrow_tree_indent_keeps_levels_visible`
- `flat_fallback_notice_is_shown_for_unstructured_rows`
