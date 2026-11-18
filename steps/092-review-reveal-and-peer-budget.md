# 092 — Reveal index and peer fetch budget

## Before

Two review findings on the delivered-base candidate:

- `TREE_FIXED_HEADER_CHILDREN` counted the local node as a header row. The
  sidebar's direct children are REMUDA (0), the optional helper notice, the
  NODES label, then the local node (index 2 without the notice). The constant 3
  made `scroll_to_item` reveal the row after the keyboard cursor, and the unit
  test only compared the same wrong number against itself.
- Local view/List requests were bounded at 3 s each, but `peer_worker` had no
  whole-request budget: the `ClusterClient` view request and its flat `List`
  fallback run sequentially with the client's default timeouts, which can
  exceed the worker's 8 s watchdog on a slow old peer. The GUI then loses the
  fallback rows instead of keeping the previous rows.

## Desired outcome

The reveal index names the painted child, and a slow peer gives up before the
worker watchdog so the previous rows stay visible.

## Expected

- `TREE_FIXED_HEADER_CHILDREN = 2` (title plus section label); the local node
  is the first tree entry, and the helper notice adds one.
- `sidebar_child_indices` still reproduces conditional notice/empty rows, and a
  test compares `ScrollHandle::bounds_for_item` with the painted node-local and
  cursor rows (accounting for the scroll offset) instead of only checking the
  vector.
- `PEER_REQUEST_BUDGET = 6 s` bounds the whole peer closure (discovery,
  resolve, view, fallback and the second uniqueness check) through
  `bounded_call`; the worker deadline remains 8 s.
- A timed-out peer call returns no rows, so the GUI keeps its previous rows.
- The unit test asserts the budget is below `provider::TIMEOUT` and that a
  5 s call returns `None` within 2 s.

## Actual

Verified on the stage-A commit: full GUI bin suite 156 passed / 0 failed /
8 ignored, targeted index/scroll and budget tests pass.
