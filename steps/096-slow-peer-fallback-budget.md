# 096 — Slow peer: per-view limit and a real fallback budget

## Before

`peer_worker` bounded the whole peer fetch (discovery, resolve, view, flat
`List`, second uniqueness check) at `PEER_REQUEST_BUDGET = 6 s`. The review
found the gap: the view request had no individual limit, so a slow peer could
spend 5 s on `RenderedSessionList`, leaving at most 1 s for the `List`
fallback. The fallback then timed out even though the overall budget was not
exhausted, and the GUI kept stale rows unnecessarily.

## Desired outcome

A slow view leaves a usable, measured amount of the whole budget for the flat
fallback. If the view answers, its rows are used; if it stalls past its own
limit, the fallback still gets the remaining budget; if the whole budget is
gone, the worker returns nothing and the GUI keeps its previous rows.

## Expected

- `PEER_VIEW_TIMEOUT = 3 s` bounds the view request (min of that and the
  remaining whole-fetch budget), below `PEER_REQUEST_BUDGET = 6 s`, which stays
  below the 8 s worker watchdog.
- The view result is `Option<Vec<Session>>`; a timeout, transport error or a
  non-view reply falls through to `List`, not to a worker failure.
- The `List` request is bounded by the budget remaining from one `Instant`
  start, not by a fresh 3 s or the client default. A zero remaining budget
  returns no rows immediately.
- A worker timeout (the view and the fallback both missed their slices) is
  reported as a timeout, not a refusal: the GUI keeps the cached rows, sets
  `remote_error`, and shows `cached · refresh failed` above them. A definite
  transport failure still invalidates that peer's cache, as before.
- The stale notice is part of `sidebar_child_indices`, so keyboard reveal
  indices stay aligned when it appears.
- A timed-out fetch writes no reply of its own; a later reply is still
  discarded by the existing generation check.
- Tests: view-within-its-limit then fallback-with-remaining-budget succeeds;
  the constants keep
  `PEER_VIEW_TIMEOUT < PEER_REQUEST_BUDGET < provider::TIMEOUT`; a timeout
  keeps cached rows and marks them stale; the stale notice keeps reveal
  indices aligned; the render test shows the cached row plus the notice; the
  existing definite-failure test still invalidates.

## Actual

Source-only until the build slot returns; the long-name slice `f4a3027f` is
the current candidate and is built first. The slow-peer change is a separate
commit and is verified with its own targeted and full runs before the final
binary and capture are reported.
