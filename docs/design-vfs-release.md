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
6. The blob is older than `--older-than`. The default is 0: a reader that
   already opened the file keeps reading it on unix, and the last-copy rule is
   what actually protects the data. An operator who hands out paths with
   `resolve` can pass a grace.

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
