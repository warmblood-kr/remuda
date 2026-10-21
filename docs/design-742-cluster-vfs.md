# Cluster virtual file store (#742)

A cluster-wide store of named files. Any admitted node can read and write it.
Each file's content is a sha256-addressed blob kept on several nodes, and names
are stable `remuda://` URIs. The store is additive: existing per-node paths
and commands do not change.

## URIs and paths

```
remuda://<cluster>/<path>     remuda:///<path>   (empty cluster = this cluster)
```

- `<cluster>` is the node's configured cluster name (`vfs.json`, default
  `default`). A URI naming another cluster is refused with a clear error. The
  cluster has no id of its own today, so the name is the user's label.
- `<path>` is `/`-separated, 1–1024 bytes, each component 1–255 bytes. It may
  not use empty, `.` or `..` components, NUL, `\`, or control characters.
  Paths are case-sensitive. Percent-encoding is not interpreted.

The parser, path rules, placement and merge rules are pure code in
`remuda-core::vfs`, so every node computes the same answers.

## Storage format (per node)

Inside the private cluster directory (`$XDG_STATE_HOME/remuda/cluster/vfs`,
created with the same hardened helpers as the rest of `cluster/`):

```
vfs/blobs/<aa>/<64-hex>   immutable content; name = sha256(content)
vfs/index.db              name index in SQLite, one row per entry (#778)
vfs/index.db-wal, -shm    SQLite's write-ahead log and its shared index
vfs/index.json            format-1 index of older remuda; imported once
vfs/vfs.json              local settings: cluster name, default replicas
vfs/synced_at             unix time a peer index was last merged here
vfs/identity.lock         the cluster StateLock, held for index writes
```

The index is a SQLite database (`index.db`, WAL mode, `PRAGMA user_version`
1). A write changes only its own rows, in one transaction. All three SQLite
files are 0600 (owner-only ACL on Windows). On first open, an existing
`index.json` is imported in the transaction that creates the table; the JSON
file is then left as it was and no longer read. Peers see no change: entries,
digest and messages are the same as before.

Downgrade: an older remuda reads only `index.json`, which still holds the
index as it was at the upgrade. `VfsStore::export_legacy_index` writes it
again from the database first (the CLI for it comes with the daemon-owned
step).

Blobs are written to a temporary file, hashed, then renamed into place, so a
blob file is never partial. A read verifies the hash again before returning.
The largest file is 64 MiB.

Index entry (one per path, kept on every node):

```json
{"path":"notes/a.md","sha256":"…","size":12,"replicas":3,
 "version":7,"writer":"SHA256:…","deleted":false,"mtime":1760000000}
```

Merge rule: last writer wins. The higher `(version, writer)` pair wins. A write
uses `version = max(local index versions) + 1`, a Lamport clock. Deleting
writes a tombstone (`deleted: true`) with the same rule, so a delete cannot be
undone by an older replica. At most 10,000 live files. Tombstones are not
removed yet, so they do not count toward that cap; the whole index, files and
tombstones together, is capped at 30,000 entries. A delete always fits, since
it replaces a live entry.

## Replica placement

Each entry stores its replication factor `R` (default 3, `--replicas N`,
1–16). Placement is rendezvous hashing: each candidate node scores
`fnv1a64(node_fp ‖ sha256)`, and the top `R` scores hold the blob. Candidates
are admitted nodes that this node does not consider down. With fewer
candidates than `R`, every candidate holds a copy. Every node computes the same
order from the replicated registry, so no placement table is stored.

## Transport

Four new peer requests on the existing Noise cluster channel (`cluster
listen`/`join`). The listener handles them directly, like registry sync. They
are refused through the remote-control front and use their own per-peer rate
limit.

| Request | Answer |
|---|---|
| `ClusterVfsIndexSync {digest, offset}` | page of 256 entries, or `unchanged` |
| `ClusterVfsIndexUpdate {entries_json}` | ack and index digest |
| `ClusterVfsBlobStat {hashes}` | which of ≤ 64 hashes this node holds |
| `ClusterVfsBlobGet {sha256, offset}` | ≤ 1 MiB chunk, total size |

Content moves by pull only. A request is limited to about 64 KB, so a push
would need many small requests. A response may be 4 MiB, so a chunked pull is
cheap. The sender hashes its copy before it serves the first chunk and drops
a damaged copy; the receiver hashes the assembled blob and drops it on a
mismatch.

## Operations

- **put** (CLI, no daemon needed): store the blob locally, write the index
  entry, push the entry to admitted peers. With `--wait`, it then waits until
  `R` placement nodes report the blob through `BlobStat`. When the node
  knows peers but reached none of them, put says the file is stored on this
  node only and exits 75 (EX_TEMPFAIL); the `Told N of M peers` line stays.
- **get / resolve**: read the local blob if present. Otherwise ask the
  placement nodes in order, then any other admitted peer, and verify the hash.
  `resolve` keeps the blob locally and prints its file path for tools that
  need a per-node path. If no replica is reachable, the error names the URI and
  the nodes tried.
- **ls / stat / rm**: read the index or write a tombstone.

## Failure handling and re-replication

The daemon's existing anti-entropy tick (`cluster/replication.rs`) also runs a
VFS pass:

1. Pull the index from each admitted peer and merge it. Index digests make an
   unchanged peer cost one small request.
2. Update peer health: a peer that has failed every request for longer than
   the grace period (default 120 s) counts as **down**. A revoked node is gone
   at once.
3. Repair: for each live entry, compute placement over nodes not down. If this
   node is in the set and lacks the blob, pull it from any holder.

When a node is lost, the next-ranked node joins the placement on its next pass
and pulls a copy, which restores `R` copies. When the lost node comes back,
placement returns to the original set. The extra copy stays (a correct state);
trimming copies is future work.

The writer always keeps its own copy, even outside the placement. GC removes a
blob that no live entry references and whose file is older than 24 h. The age
check covers a put that has written its blob but not yet its entry.

## Lua (daemon, read-only)

`remuda.vfs.stat(uri)`, `remuda.vfs.read(uri)` and `remuda.vfs.list(prefix)`
answer from the local index and cached copies only and never wait on the
network. A failure returns `nil, "code: detail"` with code `no_cluster`,
`not_found`, `not_local`, `bad_uri`, `wrong_cluster`, `corrupt` or `io`. `stat`
also returns entries this node holds no copy of (`local = false`) and
`synced_at`, so a caller can judge staleness. Cluster guidance (#741) reads
`remuda:///butler/guidance/cluster.md` this way.

## Phases

1. Core rules (URI, paths, placement, merge) and the local store. Unit tests.
2. Peer requests, index replication, remote get, the repair pass, CLI and Lua.
   Live tests run three daemons on loopback
   (`native/tests/cluster_vfs_live.rs`): put on A and read on B and C, a newer
   write and a delete reach every node, and killing a holder makes the
   outsider pull a copy.

## Not in v1

Striping with parity, partial reads, streaming writes over 64 MiB, per-path
access control (any admitted node may read and write everything), trimming
copies, and Windows ACL hardening of `vfs/` beyond the shared helpers.
