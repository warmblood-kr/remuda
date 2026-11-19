# 097 — Keep a remote list through a blip, then expire it honestly

## Before

The cluster tree polls each admitted peer every 2 s with backoff. A known node
with a last good list became `Unreachable` after the second failed poll (about
two seconds) and `Reachable` again on the next success, so the glyph and the
words flipped `● → ? → x → ●` for one blip. Retained rows never expired either:
an outage that lasted days kept showing the last list under an `x` glyph, with
no boundary between "a blip" and "no data".

The TUI half is the stacked follow-up PRs on this branch: they consume these
fields, render the retained rows, keep their view, refuse stale actions, and
harden the close/keys boundary.

## Desired outcome

A transient failure keeps the last successful list, labelled stale with its
age. A fresh success replaces the list in one step. A node whose last success
is older than a bounded retention window drops the rows and reads unreachable,
so a prolonged outage cannot pass for a live herd. Retained rows are for
reading; they never authorize keys, a line or a close. A new node identity
starts with no inherited rows.

## Expected

- `REMOTE_RETENTION = 180 s` (three minutes) is the default per-node window.
  It is long against a 2 s poll / 30 s backoff and short enough to surface a
  real outage. `RemoteNode::with_retention` lets a caller shorten it; tests use
  a short window instead of waiting.
- `RemoteNodeSnapshot` carries `last_updated_at`, an absolute wall-clock
  `SystemTime` of the last successful list or session collection, and
  `expired`. `Some` with no sessions is a successful empty answer; `None` means
  no collection ever succeeded. Display age is derived from it at render time;
  a future timestamp from a clock adjustment clamps to `0s`.
- Retention and expiry use a separate monotonic `Instant`, plus the absolute
  collection time as a second bound. While the machine runs the monotonic
  window is authoritative; across suspend (where the monotonic clock stalls on
  some platforms) or a forward wall jump the older absolute time expires the
  rows early. A backward jump can never extend the monotonic window. This is
  deliberately fail-closed: retained rows never authorize an action, and a
  forward jump only hides rows until the next poll succeeds.
- A failed poll keeps `Stale` while the list is retained; at or past retention
  `snapshot_at` reports `Unreachable`, clears `sessions`, and keeps the
  absolute timestamp so the row can say how old the data is.
- A successful list — including an empty one — sets `Reachable`, replaces the
  list wholesale and resets the error.
- Node identity changes are covered by the existing registry reconcile (keys
  differ) and a test asserts the new fingerprint inherits no timestamp or rows.
- The node row shows `? Ns` while stale and `x Ns` while expired; the pane
  detail says `unreachable · no fresh data for Ns`.
- A remote view is kept while its identities still match the retained list and
  is dropped together with the rows at expiry. A slow reply that still matches
  is accepted instead of degrading the rows to a plain fallback.
- `stale_action_refusal` gates keys mode, the composer, buffered keys, the
  close confirmation and the confirmed close on `Reachable`. A node that has
  disappeared from the snapshot is refused too, so a confirmation begun before
  it vanished cannot send. Keys buffered while reachable are re-checked at
  flush; if the node went stale meanwhile every buffer for that node is
  dropped, keys mode ends and the notice says why. A refusal restores the
  composer draft and says which node is stale and for how long. The displayed
  age and expiry read the snapshot's single wall sample, so they cannot
  disagree within a frame.
- `remote_synced` clears a composer target whose exact session is gone, so
  expiry cannot leave a reusable authorization behind.

## Actual

Stacked TUI branch `split/985-tui` on `split/985-core`; both rebased on `main`
`ec278f3` (80c2c0d).
```
$ cargo test -p remuda-native --lib cluster_remote::
test result: ok. 38 passed; 0 failed

$ cargo test -p remuda-native --lib cluster_tui::
test result: ok. 236 passed; 0 failed

$ cargo clippy -p remuda-native --lib -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo]

$ REMUDA_EXPERIMENTAL_SESSION_HOST=0 cargo test -p remuda-native --test cluster_remote
test result: FAILED. 32 passed; 4 failed; 3 ignored (same-env main baseline)
$ REMUDA_EXPERIMENTAL_SESSION_HOST=0 cargo test -p remuda-native --test cluster_tui
test result: FAILED. 5 passed; 1 failed (same-env main baseline)
```
The remaining integration failures are hosted-only or one TUI input test and
fail on unmodified main. The real-clock probe: success `Reachable` rows=1 with
the absolute timestamp; failure `Stale` rows=1 with the same timestamp; expiry
`Unreachable` rows=0; fresh success `Reachable` rows=1 with a newer timestamp;
a synthetic future timestamp renders `0s`.
