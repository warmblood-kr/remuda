# VFS index: change cursors instead of whole-index digests

Status: design for review; no code yet. This covers steps 3–6 of the
2026-10-20 VFS reference research ("report-vfs-reference-2026-10-20",
section 7). Steps 1–2 have landed:

- Most quick wins are in.
- `vfs/index.db` is SQLite (#783, #794).
- The schema runner exists (#796).
- Every `remuda vfs` command runs in the daemon (#787).
- The index is fenced during `remuda reload` (#806).

## The problem

Today a node pulls each peer's index every 60–70 s:

1. It sends a digest of the whole index.
2. If the digests differ at all, it pulls the whole index in pages of 256
   entries.

The server builds every page from a full load of its index. At 10k entries
that is about 40 full scans per peer per tick for as long as anything
differs. A single new file costs every node a full pull from every peer.

## Step 3: change sequence, index id, per-peer cursors

Borrowed from Syncthing's delta index exchange.

**Schema** (index.db step 2, through `sqlite_migrate`; down SQL included):

```sql
ALTER TABLE entries ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
-- every existing row gets a distinct seq, in path order
UPDATE entries SET seq = (SELECT count(*) FROM entries e2 WHERE e2.path <= entries.path);
CREATE UNIQUE INDEX entries_seq ON entries(seq);
CREATE TABLE peers (
  fp           TEXT PRIMARY KEY NOT NULL,  -- node fingerprint
  index_id     TEXT,                       -- their index id when we last pulled
  pulled_seq   INTEGER NOT NULL DEFAULT 0, -- our cursor into them
  acked_seq    INTEGER NOT NULL DEFAULT 0, -- their cursor into us (step 5)
  last_seen    INTEGER
) WITHOUT ROWID;
-- meta: 'index_id' (random 128-bit hex, made when step 2 runs), 'next_seq'
```

**Rules**
- **Bumping seq:** every row change takes `seq = next_seq` and increments
  `next_seq` in the same transaction as the change. That covers local puts
  and deletes and also merged rows. A node therefore relays what it learned,
  so gossip still works without a full mesh.
- **The index id:** a new `index_id` means "my sequence restarted" and
  forces a full pull. It changes only when the database is recreated
  (deleted file, restore from backup), never on a migration.
- **The cursor:** the puller merges a page and advances `pulled_seq` in one
  transaction, so a crash never skips rows. A merge is idempotent, so
  pulling a row again is harmless.

**Wire.** These are new request and response variants. Old ones stay.

```
ClusterVfsChanges { index_id: Option<String>, since: u64, limit: u32 }
ClusterVfsChangesPage { index_id: String, entries_json: String,
                        last_seq: u64, more: bool, reset: bool }
```

- **Server:** it answers rows with `seq > since ORDER BY seq`, up to `limit`
  (at most 256 rows and 3.5 MB).
  - **Reset:** if `index_id` is not its own, or `since` is beyond its
    `next_seq`, it answers `reset: true` with no rows. The puller then
    drops its cursor and starts again at 0.
  - **Ack:** it records `acked_seq = since` for the calling peer, because
    the caller already has every row up to `since`. This is the input to
    step 5.
- **When nothing changed:** a tick costs one request and one indexed
  query. No digest is computed.
- **Concurrent writes:** a row changed while paging gets a higher seq and
  arrives on a later page or tick. Nothing is lost.
- **Push on put and rm stays.** It is a hint that makes peers pull sooner.
- **Mixed versions:** an older peer cannot decode `ClusterVfsChanges`. Any
  answer that is not a `ClusterVfsChangesPage` makes the puller use today's
  digest pull for that peer until the daemon restarts. An older puller keeps
  calling `ClusterVfsIndexSync`, which a newer server still answers. The
  digest is unchanged.
- **Downgrade:** a build that knows only index.db step 1 refuses step 2
  with "upgrade remuda" (tested in #796). New command
  `remuda vfs index-downgrade 1` runs the stored down SQL after a backup,
  like `remuda db downgrade`.

**Tests**
- A 10k-entry index:
  - An unchanged tick sends one request, and the peer runs no full scan.
  - One put moves one row.
- A crash between merging a page and committing it: the next pull covers
  the same rows.
- `reset` after a peer recreates its index.
- Relay: A→B→C with no A–C link.
- A mixed N/N+1 three-daemon run, in the existing compat job.

## Step 4: concurrent writes are kept, not silently dropped

**Today:** two nodes that write the same path while apart both choose
version = own max + 1. The tie-break drops one write, and blob GC deletes
its content about 24 h later. Nobody is told.

**Plan:**
- **Version vector:** each entry gets `vv` (serde default, so old nodes
  ignore it): a sorted list of (node fingerprint, counter) pairs, at most
  one per member. A local write sets its own counter to the highest counter
  it has seen for the path, plus one.
- **Merge rule:**
  - If one vector dominates the other, that entry wins.
  - If neither does, the writes were concurrent. The current
    (version, writer, …) order still picks the winner, so every node agrees.
  - The loser goes into a `conflicts` table, and its blob stays referenced,
    so GC keeps it.
- **Surface:** `remuda vfs ls --conflicts`, and
  `remuda vfs resolve-conflict URI --keep winner|VERSION`. Lua gets
  `stat().conflicts`.
- **Mixed versions:** an entry that arrives without `vv` merges as today
  (last write wins). Detection is complete only once every node is upgraded.

**Owner decision:** keep losers in a `conflicts` table (proposed), or as
conflict-copy paths such as `notes.md.conflict-<time>-<node>`. Copies need
no new commands, but they take cap slots and can pile up.

## Step 5: tombstone GC by causal stability

Tombstones are kept forever today. They have their own cap room (30k), but
the index only grows.

**Plan:**
- **When a tombstone is removed:** only when every admitted member's
  `acked_seq` (from step 3) is at least the tombstone's `seq`, and it is at
  least 7 days old.
- **How it is removed:** with a compare on seq, so a newer write to the same
  path is never removed.
- **Members that stop acking:** a member not seen for 30 days stops blocking
  GC and is marked `stale`.
- **A stale member that returns:** peers answer its pushes with `reset`. It
  drops rows it has not written itself, pulls current state, and offers only
  its own writes made after its last acked cursor. Those merge under step 4
  rules, so they can become conflicts but never resurrect a deleted file
  without notice.
- **Mixed versions:** an older member never acks, so it blocks GC. That is
  safe: nothing is collected until it is upgraded or revoked.

**Owner decision:** the 7-day minimum age and the 30-day stale horizon
(Cassandra's lesson: GC must wait longer than the longest outage that is
expected).

## Step 6: placement and put guarantees

- **Trim:** remove a copy outside the placement only after `BlobStat`
  confirms every placement holder has it. Never remove the last copy.
- **Drain:** `remuda cluster leave` waits until nothing is
  under-replicated, then revokes this node.
- **Repair queue:** a table `repair(sha256, next_try, errors)` with backoff
  replaces the 256-per-pass scan, so a large backlog cannot starve.
- **`put --min-copies K`:** wait until K holders exist, the writer included.
  On timeout, report "committed here, K not reached, still replicating"
  with exit 75. Nothing is rolled back.

## Order and size

Each step ships alone, as one or two draft PRs, with a compat test in CI.

| Step | Scope | Depends on |
|---|---|---|
| 3 | seq, index id, cursors, `ClusterVfsChanges`, index downgrade command | — |
| 4 | vv, conflicts, CLI and Lua surface | 3 (seq relays vv) |
| 5 | tombstone GC, stale reset | 3 (acked cursors), 4 (reset merges) |
| 6 | trim, drain, repair queue, `--min-copies` | — (may go before 4–5) |

## Decisions needed before code

1. Go ahead with step 3 as described.
2. Conflicts as a table (proposed) or as conflict-copy paths.
3. Tombstone GC: a 7-day minimum and a 30-day stale horizon.
4. Whether step 6 goes before 4 and 5. It is independent and the most
   visible to users.
