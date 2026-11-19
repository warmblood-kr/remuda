# 097 — Keep a remote list through a blip, expire it honestly

## Before

The cluster tree polls each admitted peer every 2 s with backoff. Two display
rules made an intermittent collection failure flicker:

- A known node with a last good list became `Unreachable` after the second
  failed poll (about two seconds) and `Reachable` again on the next success.
  The glyph and the words flipped `● → ? → x → ●` for one blip.
- A remote rendered view expired after `REMOTE_VIEW_TTL = 5 s`. While the peer
  was briefly unreachable no new view could be requested, so the structured
  rows fell back to plain `List` rows and then repainted when the peer
  answered — a second, row-level flicker.
- Retained rows never expired. An outage that lasted days kept showing the last
  list under an `x` glyph, with no boundary between "a blip" and "no data".

The stale rows could also authorize input: keys mode, the composer and the
close confirmation only checked that the exact session identity was still
listed, not whether its node's collection was succeeding.

## Desired outcome

A transient failure keeps the last successful list, labelled stale with its
age. A fresh success replaces the list and the rendered view in one step. A
node whose last success is older than a bounded retention window drops the
rows and reads unreachable, so a prolonged outage cannot pass for a live herd.
Retained rows are for reading; they never authorize keys, a line or a close.
A new node identity (new registry fingerprint) starts with no inherited rows.

## Expected

- `REMOTE_RETENTION = 180 s` (three minutes) is the default per-node window.
  It is long against a 2 s poll / 30 s backoff and short enough to surface a
  real outage. `RemoteNode::with_retention` lets a caller shorten it; tests use
  a short window instead of waiting.
- `RemoteNodeSnapshot` carries `last_updated_at` (the last successful list, not
  a relabel) and `expired`. `Some` with no sessions is a successful empty
  answer; `None` with sessions empty means no collection ever succeeded.
- A failed poll keeps `Stale` while the list is retained; at or past retention
  `snapshot_at` reports `Unreachable`, clears `sessions`, and keeps
  `last_sync_age` so the row can say how old the data is.
- A successful list — including an empty one — sets `Reachable`, replaces
  `sessions` wholesale and resets the error. Expiry is computed from the wall
  clock on every snapshot, so a stalled poller cannot keep rows looking fresh.
- The node row shows `? Ns` while stale and `x Ns` while expired; the pane
  detail says `unreachable · no fresh data for Ns`.
- A remote view is kept while its identities still match the retained list and
  is dropped together with the rows at expiry. A slow reply that still matches
  is accepted instead of degrading the rows to a plain fallback.
- `stale_action_refusal` gates keys mode, the composer, buffered keys, the
  close confirmation and the confirmed close on `Reachable`. A refusal restores
  the composer draft and says which node is stale and for how long.
- `remote_synced` clears a composer target whose exact session is gone, so
  expiry cannot leave a reusable authorization behind.
- Node identity changes are covered by the existing registry reconcile (keys
  differ) and a test asserts the new fingerprint inherits no timestamp or rows.

## Actual

Source `feat/cluster-remote-retention` (base 33f9da8) and the main port
`port/cluster-remote-retention-main` (base 3c1a4d8); candidate untouched. The
main port needed one import-line resolution (`REMOTE_VIEW_TTL`/`WHEEL_ROWS`).

```
$ cargo test -p remuda-native --lib cluster_remote::      # main port
test result: ok. 37 passed; 0 failed; 0 ignored; 0 measured; 1886 filtered out

$ cargo test -p remuda-native --lib cluster_tui::          # main port
test result: ok. 235 passed; 0 failed; 0 ignored; 0 measured; 1688 filtered out

$ cargo clippy -p remuda-native --lib -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 5.81s

$ cargo test -p remuda-native --test cluster_remote       # main port
test result: FAILED. 30 passed; 6 failed; 3 ignored; 0 measured; 0 filtered out

$ cargo test -p remuda-native --test cluster_tui          # main port
test result: FAILED. 0 passed; 6 failed; 0 ignored; 0 measured; 0 filtered out
```

All 12 integration failures reproduce individually on unmodified main 3c1a4d8
at daemon session creation (`Request::New` returns non-`Value`) before any
remote-list code runs; they are environmental on this runner, not regressions.
The real-clock probe passed in 188.36 s: `Reachable` rows=1 → `Stale` at 2.02 s
with rows=1 → expired at 180.01 s with rows=0 → `Reachable` rows=0 (successful
empty) → `Reachable` rows=1 after a new session.
