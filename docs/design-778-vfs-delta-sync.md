# VFS index: change cursors instead of whole-index digests

Status: built. Steps 3–6 of the 2026-10-20 VFS reference research
("report-vfs-reference-2026-10-20", section 7) are on main. This page keeps
the reasons and the decisions. For the behaviour as it is, see
[design-742-cluster-vfs.md](design-742-cluster-vfs.md).

| Step | What | PRs |
|---|---|---|
| 1–2 | SQLite `vfs/index.db`, schema runner, CLI in the daemon, reload fence | #791, #794–#796, #806 |
| 3 | change sequence, index id, per-peer cursors | #818 |
| 4 | version vectors, kept conflicts, CLI and Lua surface | #826, #827 |
| 5 | tombstone GC, stale horizon | #829, #830 |
| 6 | `put --min-copies`, repair queue, trim, drain | #835–#838 |
| fixes | migration race (#841), test for a newer index (#839) | |

## The problem

Before step 3 a node pulled each peer's index every 60–70 s. It sent a
digest of the whole index, and if the digests differed at all, it pulled the
whole index in pages of 256 entries, each page built from a full load. At
10k entries that was about 40 full scans per peer per tick, and one new file
cost every node a full pull from every peer.

## Step 3: change sequence, index id, per-peer cursors

Borrowed from Syncthing's delta index exchange.

- **Schema** (index.db step 2, with down SQL): `entries.seq` (unique), a
  `peers` table (`fp`, `index_id`, `pulled_seq`, `acked_seq`, `last_seen`),
  and `meta` keys `index_id` and `next_seq`.
- **Bumping seq:** every row change, local or merged, takes `next_seq` in the
  same transaction. A node relays what it learned, so gossip needs no full
  mesh.
- **Index id:** changes only when the database is recreated, never on a
  migration. A different id means "my sequence restarted": full pull.
- **Cursor:** the puller merges a page and moves `pulled_seq` in one
  transaction. Merges are idempotent, so pulling a row twice is harmless.
- **Wire:** `ClusterVfsChanges { index_id, since, limit }` answered by
  `ClusterVfsChangesPage { index_id, entries_json, last_seq, more, reset, … }`
  (at most 256 rows). The server records `acked_seq = since` for the caller.
  An unchanged tick costs one request and one indexed query.
- **Mixed versions (as built):** a peer that does not answer
  `ClusterVfsChanges` gets the old digest pull and is asked again after an
  hour (the plan said "until the daemon restarts"). Older pullers keep using
  `ClusterVfsIndexSync`.
- **Downgrade:** `remuda vfs index-downgrade VERSION --yes` runs the stored
  down SQL after a backup. `remuda vfs export-index` writes `index.json`
  for a build from before SQLite.

## Step 4: concurrent writes are kept

Before: two nodes writing one path while apart both chose version = max + 1;
the tie-break dropped one write and GC deleted its blob a day later, silently.

- Each entry carries `vv` (serde default), one counter per member. A local
  write sets its own counter to the highest seen for the path, plus one.
- If one vector dominates, it wins. Otherwise the writes were concurrent: the
  old (version, writer, …) order still picks the winner, so every node
  agrees, and the loser goes into the entry's `conflicts` (at most 16) with
  its blob kept.
- **Settle by write** (accepted 2026-10-22): a local write takes a vector
  that covers the entry and its conflicts, so the next write settles them on
  every node. No resolution marker.
- Surface: `remuda vfs ls --conflicts`, `stat`, `remuda vfs resolve-conflict
  URI --keep winner|VERSION`, Lua `stat().conflicts`.
- Entries without `vv` (older nodes) merge last-write-wins.

Owner decision (2026-10-21): a conflicts table with loser blobs kept, not
conflict-copy paths.

## Step 5: tombstone GC by causal stability

- A tombstone goes once every other admitted member's `acked_seq` is past it
  and it is `tombstone_days` old (default 7). The newest version and
  tombstones with conflicts stay. Removal compares seq, so a newer write is
  never removed.
- A member whose last pull is older than `stale_days` (default 30) stops
  holding tombstones back. When it pulls again from below a collected
  tombstone, the page says `stale` with `seen_seq`: it keeps only its own
  writes after `seen_seq`, drops the rest and pulls again, logging one line.
  Its writes merge under step 4, so a deleted file cannot come back unseen.
- An older member never acks, so it blocks GC. That is safe.
- A concurrent delete that loses to a write is not kept as a conflict.

Owner decision: 7-day minimum and 30-day stale horizon, both configurable
(`remuda vfs config --tombstone-days`, `--stale-days`).

## Step 6: placement and put guarantees

- **`put --min-copies K`** (#835): K is 1 to the replicas wanted; not with
  `--wait`. It waits up to 60 s for K holders, this node included. Short:
  "committed here, but only N of K copies confirmed …; still replicating",
  exit 75. Nothing is rolled back.
- **Repair queue** (#836): in memory, not the planned `repair` table (leader
  approved; a restart just starts fresh). Never-tried copies go first; a
  failed copy waits 30 s, doubling to 1 h; at most 256 per pass.
  `remuda vfs sync` skips the wait.
- **Trim** (#837, owner option a): only copies the repair pass fetched are
  marked as stand-ins (`vfs/stand-ins/<sha256>`). A stand-in is deleted once
  this node is outside the placement and `BlobStat` confirms every placement
  node. Writer copies, conflict blobs and read caches stay; `resolve` clears
  the mark.
- **Drain** (#838): `remuda vfs drain [--timeout SECS]` (1–110, default
  100) asks each placement node without this one to take what it lacks
  (`ClusterVfsTake`, at most 64 hashes), until `BlobStat` confirms or the
  timeout (exit 75). It is sent only to peers whose change pages list the
  `take` cap; others are named "too old". A take lapses after 1 h; taken
  copies are not stand-ins. Unlike the plan, drain revokes nothing: it says
  to run `remuda cluster revoke NODE` on another node.
- **Leave:** `remuda cluster leave` asks a peer to revoke it
  (`ClusterLeave`); the peer revokes only the caller, after `BlobStat`
  confirms every placement without it, and audits it. The leaver then moves
  its cluster folder to `cluster.left-DATE`, so a rejoin starts empty.

## Follow-ups built

- Squashing old migration steps (#851), `remuda cluster leave` (#863), and
  an N/N+1 test of the VFS across `remuda reload`
  (`vfs_writes_survive_a_reload_to_the_older_build_and_back`).
