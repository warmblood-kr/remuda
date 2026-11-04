# Giving back copies a node only read (#887)

`get`, `read` and `resolve` store the blob on the node that read it. Nothing
removed it again: `collect_garbage` skips every hash a live entry references,
and the sync pass trims only repair stand-ins. A node that only looks at files
— a `remuda vfs serve` viewer, or a laptop that opens design artifacts — drifts
into holding a copy of everything anyone there opened. On a host with 1.1 GiB
free, that is a slow leak with no brake.

`remuda vfs release [--dry-run] [--older-than SECS] [--json]` is that brake.

## What it removes

A local blob goes only when all of these hold:

1. A live entry names it. Unreferenced blobs belong to garbage collection.
2. This node did not write it, and it is not the content of a kept conflict.
3. Placement, computed over admitted nodes that are not down, does **not**
   include this node — it owes no copy.
4. Placement is not empty. An empty placement means this node cannot see
   anyone who could take the copy, which is not permission to delete it.
5. Every node placement names has confirmed a copy by `BlobStat`, the same
   check the stand-in trim already uses. A node that does not answer keeps its
   copy here; `release` says how many and exits 0.
6. The blob is older than `--older-than`. That is the age of the **blob file**,
   not the last time it was read: `put_blob` keeps an existing file's mtime, so
   a second read of an old copy does not refresh it. The default is 0, because
   a reader that already opened the file keeps reading it on unix and the
   last-copy rule is what protects the data; an operator who hands out paths
   with `resolve` can pass a grace and accept that an unopened resolved path
   may then stop existing.

## Honest peers, and the window left open

Custody is a peer's answer to `ClusterVfsBlobStat`: "I hold a file of this
size". It is not a verified digest, and it does not promise retention. Peers
are admitted, mutually authenticated nodes, so this assumes an honest peer that
is not corrupt; a peer whose copy is damaged can still answer `true`.
Requiring a verified digest on every custody check would make each release a
full read of the file, which is not what a cleanup command should cost.

Eligibility is computed, then peer custody is confirmed, then the unlink
happens. Under the index lock the store re-reads the index and membership and
refuses to unlink if, since the scan, this node wrote the content, a kept
conflict took it, or placement began to name this node. A peer that leaves
between confirmation and unlink is not covered: the guarantee is "every node
placement named, as observed a moment ago, held a copy", not a distributed
lease.

The index is untouched: no version bump, no tombstone, no membership change. A
released copy is not a delete; the entry stays and any node can fetch it again.

## Why explicit

Trimming read copies automatically on every sync pass would quietly undo
`resolve`'s promise ("a tool may hold the path"). A command puts the decision
where the reader is, and makes a viewer node's cleanup a thing an operator can
schedule. An automatic policy (a cache budget, evict oldest non-placed copy)
is a later step; it would want a setting and a size, and it would still need
the six conditions above.

## Commands

```sh
remuda vfs release --dry-run        # what would go, and why the rest stays
remuda vfs release                  # give back what nobody needs here
remuda vfs release --older-than 3600
```

Text reports the count, the bytes freed and the reasons the rest stayed. JSON
has the same numbers as `{dry_run, removed, bytes, placed, written, recent,
last_copy, unconfirmed}`.

## Tests

- `native/tests/cluster_vfs_live.rs`: three nodes; a page written with
  `--replicas 1` is fetched on a node that holds neither copy. Before the
  change, `sync` reports `trimmed 0` and the blob stays (this is the repro).
  After it, `release --older-than 3600` keeps the fresh copy, `release` removes
  it once the placement node confirms, the writer keeps its own copy, and a
  node whose placement peers are all down keeps its copy.
- Unit tests for the report and the empty-grace default.

## Not here

A cache budget, automatic eviction, and releasing by prefix (`release
remuda:///big/…`); the command currently considers every local blob.
