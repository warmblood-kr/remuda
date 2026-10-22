# Cluster files: operating `remuda vfs`

`remuda vfs` stores named files that every node in a cluster can read and
write. Each file has a `remuda://` URI. Its content is kept on several nodes
(3 by default), so it outlives any single node. This page covers the commands
an operator needs day to day. The design and the wire format are in
[design-742-cluster-vfs.md](design-742-cluster-vfs.md).

Every `remuda vfs` command runs in the daemon. With no daemon running, it
prints one line saying the daemon is needed and exits 1. It never starts a
daemon on its own.

```sh
remuda vfs put notes.md remuda:///notes.md     # this cluster
remuda vfs get remuda:///notes.md              # print it
remuda vfs ls remuda:///                       # list
remuda vfs stat remuda:///notes.md             # version, size, copies
remuda vfs rm remuda:///notes.md               # delete everywhere
```

## Exit codes

| Code | Meaning |
|---|---|
| 0 | done |
| 1 | failed: bad arguments, no file at that path, no daemon, … |
| 75 | done here, but not yet everywhere: wait or run it again (see below) |

Exit 75 is `EX_TEMPFAIL` from `sysexits.h`. Nothing is rolled back.

## Checking health: `remuda vfs status`

```text
$ remuda vfs status --copies
Files: 1234 live of 10000; index 1450 of 30000 entries (216 deletes remembered)
Conflicts: 3 paths (remuda vfs ls --conflicts)
This node (node-a1b2c3d4): 812 copies, 1.2 GiB; 4 copies waiting to retry
Peers:
  node-e5f6a7b8  up; pulled from here 2m ago; caps: take
  node-c9d0e1f2  down 3h; pulled from here 3h ago; holds back 12 deletes
Settings: replicas 3, tombstone-days 7, stale-days 30
Copies: 1230 files complete; 4 short (first: notes.md 1 of 3, …)
```

- Without `--copies` it reads only what this node knows and answers at
  once. "Holds back N deletes" counts deletes that peer has not pulled from
  here yet; until it does, they are remembered.
- `--copies` also asks each node that should hold a file whether it does
  (for up to 30 s). It exits 75 while any file is short, so a script can
  wait for it. Nodes that did not answer in time are named.
- `--json` prints the same report as JSON.

## Making sure a write has copies: `put --min-copies K`

By default `put` returns once this node has stored the file and told its
peers. They fetch their copies on their own a moment later. If a script
needs the file on more nodes before it goes on:

```sh
remuda vfs put report.pdf remuda:///reports/q3.pdf --min-copies 2
```

- `K` counts this node, and it can be at most the copies wanted
  (`--replicas`, default from `remuda vfs config`).
- `put` waits up to 60 s. If fewer than K nodes hold the file by then, it
  prints `committed here, but only N of K copies confirmed in 60 s; still
  replicating` and exits 75. The write stays; the copies keep coming.
- `--wait` waits for every placement node instead. Use one or the other.
- If no peer could be reached at all, `put` says the file is stored on this
  node only and exits 75.

## Concurrent writes: `ls --conflicts` and `resolve-conflict`

Two nodes that write the same path while they cannot see each other both
keep their write. Every node picks the same winner, which is what `get`
returns, and keeps the other write as a **conflict** with its content.

```sh
remuda vfs ls --conflicts                        # paths with conflicts
remuda vfs stat remuda:///notes.md               # shows each conflict's version
remuda vfs resolve-conflict remuda:///notes.md --keep winner
remuda vfs resolve-conflict remuda:///notes.md --keep 7    # keep version 7
```

`resolve-conflict` writes the chosen side again, fetching it from a peer if
needed, and prints `Settled PATH: kept …, now version N.` Any ordinary `put`
or `rm` of the path settles its conflicts in the same way. A path keeps at
most 16 conflicts. Nodes running a remuda from before conflicts (#778) do
not detect them; their writes merge as last write wins.

## Taking a node out: `remuda cluster leave`

On the node that leaves for good:

```sh
remuda cluster leave   # asks first; --yes skips the question
```

1. It asks another node to revoke this one. That node first checks that
   every file keeps its copies without this one; if not, it refuses.
2. On a refusal it runs `remuda vfs drain` (below) and asks again.
3. Once a node has revoked it, it moves `<state>/remuda/cluster` to
   `cluster.left-YYYYMMDD` next to it and deletes the node key and
   membership files there. `vfs/` and `audit.jsonl` stay in that folder.

- **Exit 75, nothing changed:** every node that answered refused (files
  still short, or too old to revoke a leaving node). Each reason is printed;
  fix it and run `remuda cluster leave` again.
- **No other node answered** (or this is the only node): it prints a
  warning, skips the drain and the revoke, and still moves the folder. Run
  `remuda cluster revoke NODE` on another node later.
- After it, `remuda cluster init` or `remuda cluster join` starts again with
  an empty store, and the store fills from the other nodes.
- **Do not move `vfs/` back to rejoin.** A rejoined node has a new key, so
  the others pull its whole old index, and a file deleted while it was away
  (once its delete record is collected) would come back. The old `vfs/` is
  for reading files only. `cluster init --new-identity` sets `vfs/` aside
  the same way, to `cluster.left-YYYYMMDD/vfs` (#862).

To do the two halves by hand: `remuda vfs drain` on the node that leaves,
then `remuda cluster revoke NODE --yes` on another node.

- `drain` asks each node that will hold a file once this one is gone to
  take a copy, and waits until they confirm. It prints `Safe to leave: on
  another node, run remuda cluster revoke NODE.` and exits 0.
- If files are still short after `--timeout` seconds (1–110, default 100),
  it lists the first few and exits 75. The next line says what to do:
  - run `remuda vfs drain` again; peers are still copying;
  - upgrade the nodes it names as too old to take copies; or
  - start or join another node, if no other node is up.
- `drain` changes no membership. Until the revoke, the node is still a
  member.

## When a node is down

A node that has not answered for 2 minutes counts as down. The next node in
line for each of its files takes a copy (a **stand-in** copy). When the node
comes back and confirms its copies, the stand-ins are deleted again. Files a
node wrote itself, read with `get`, or handed to a tool with `resolve` are
never deleted this way.

`remuda vfs sync` runs a full pass now instead of waiting for the next tick:

```text
Peers reached 2, unreachable 0; index entries merged 3; copies fetched 1, failed 0, waiting to retry 0, trimmed 1; old blobs removed 0, old deletes forgotten 0.
```

A copy that fails to fetch is tried again after 30 s, then after twice as
long each time, up to an hour. `sync` retries them at once.

## Settings: `remuda vfs config`

```sh
remuda vfs config                         # show
remuda vfs config --replicas 2 --tombstone-days 14
```

| Setting | Default | What it does |
|---|---|---|
| `--cluster NAME` | `default` | the name in `remuda://NAME/PATH` URIs; a URI naming another cluster is refused |
| `--replicas N` | 3 | copies kept of new files (1–16); `put --replicas` overrides it per file |
| `--tombstone-days DAYS` | 7 | how long a delete is remembered once every node has it (1–3650) |
| `--stale-days DAYS` | 30 | after this long without pulling, a node stops holding deletes back (1–3650) |

Settings are per node. Give `--cluster` and the day settings the same value
on every node.

**About deletes.** A deleted file leaves a marker (a tombstone) so the
delete reaches every node. The marker goes once every other node has
pulled it and it is `--tombstone-days` old. A node that is down or runs an
older remuda keeps every marker on its peers until it pulls again, or for
`--stale-days`. A node that comes back after that drops what it missed,
keeps its own new writes, and pulls the rest again, so deleted files do not
come back.

## Limits

- 64 MiB per file; paths up to 1024 bytes.
- 10,000 live files and 30,000 index entries, deletes included.
- 16 conflicts per path.

## Downgrading remuda

- To a build one index schema step older: `remuda vfs index-downgrade
  VERSION` shows what it would undo; add `--yes` to back up and run it.
  Then start the older remuda.
- To a build from before SQLite: run `remuda vfs export-index`, stop the
  daemon, then start the older remuda.

The #778 owner checklist, [vfs-owner-checklist.md](vfs-owner-checklist.md),
walks through a downgrade and back on a scratch node.
