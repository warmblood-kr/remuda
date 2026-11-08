# Cluster files: operating `remuda vfs`

`remuda vfs` stores named files that every node in a cluster can read and
write. Each file has a `remuda://` URI. Its content is kept on several nodes
(3 by default), so it outlives any single node. This page covers the commands
an operator needs day to day. The design and the wire format are in
[design-742-cluster-vfs.md](design-742-cluster-vfs.md).

Every `remuda vfs` command runs in the daemon, except `serve`, which runs in
the CLI process and asks the daemon for each request (see [Reading in a
browser](#reading-in-a-browser-remuda-vfs-serve)). With no daemon running, any
of them prints one line saying the daemon is needed and exits 1. None starts a
daemon on its own.

```sh
remuda vfs put notes.md remuda:///notes.md     # this cluster
remuda vfs get remuda:///notes.md              # print it
remuda vfs ls remuda:///                       # immediate children
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

## Listing: `remuda vfs ls`

`ls` shows one directory's immediate children. A subdirectory prints as `a/`,
a file as its name in that directory:

```sh
remuda vfs ls remuda:///design/2026-11-05/
#          assets/
#       1229  v3      d9298a10d1b0  index.html
```

`-R` (or `--recursive`) lists every descendant file with its full path, which
is what `ls` did before the flag existed:

```sh
remuda vfs ls -R remuda:///design/2026-11-05/
#   …  design/2026-11-05/assets/site.css
#   …  design/2026-11-05/index.html
```

The trailing slash is optional: `ls remuda:///a` and `ls remuda:///a/` list the
same children, unless a file is named exactly `a`; then the form without the
slash shows that file and the form with it shows the children. `ls` on a file
path shows that file. Directories are not stored objects: they exist only as
the prefixes of stored files, so an empty directory cannot exist.

`--json` file rows keep every field they had and add `"kind": "file"`; a
directory row is `{"kind": "dir", "path": "a/"}` with the full cluster path.
A script that expected every row to be a file must check `kind` or pass `-R`,
which returns only files. `--conflicts` stays recursive but now stops at the
prefix's components: `ls --conflicts remuda:///a` no longer includes `a2/`.

## Publishing a directory: `remuda vfs publish`

A page is usually several files. `publish` uploads a directory as one named
release and marks it complete only after every file is in:

```sh
remuda vfs publish site remuda:///design --release 2026-11-04
# Published 12 files (1.4 MiB) as 2026-11-04 to remuda:///design/2026-11-04/.
```

- Regular files only; a symlink is skipped and named, never followed. At most
  500 files, 32 MiB each and 1 GiB per release, and publishing stops if the
  machine lacks room for the source plus a 256 MiB reserve.
- Each file is one ordinary `put` to `PREFIX/NAME/<relative path>`, so
  `--replicas` settings and `--min-copies K` behave exactly as they do for one
  file.
- `PREFIX/NAME/COMPLETE` is written **last**, and only if every file arrived.
  A release with no `COMPLETE` is unfinished: its files are there, but the
  release is not. `--pointer` also writes `PREFIX/CURRENT` with the release
  name after the marker, so a viewer has one stable path to open.
- A name that already has files is refused, so a release is not overwritten.
  That check is cooperative: a partitioned node can hide a release from it.
- `--dry-run` prints the plan and writes nothing.
- A source file that changes between the plan and the upload aborts the run
  rather than publishing a mix; open files and quit your editor first. Unix
  catches a *replacement* by file identity, but a same-inode rewrite with size
  and mtime restored is not detectable from metadata on any platform, and
  elsewhere a same-size rewrite can pass: publish assumes a quiet source. A
  link swapped in after the plan, at the file or any parent, changes the
  resolved path and is refused while it stays in place; a swap restored before
  the final check is only detectable with Unix identity.

Exit codes: 0 done; 1 a file failed, the source changed, `COMPLETE` could not
be written, or `--pointer` could not write `CURRENT` — the message says which,
so a 1 alone does not prove `COMPLETE` is absent. Fix the cause and publish a
**new** `--release` name (the old prefix keeps the files it received; `remuda
vfs ls remuda:///design/NAME/` shows them). When the message says the marker
could not be confirmed, check `remuda vfs stat remuda:///design/NAME/COMPLETE`;
if it is there the release is usable. With `--pointer`, also `remuda vfs get
remuda:///design/CURRENT` and confirm its value names your release, since an
older pointer can already exist. If the marker is missing, publish a new name.

75 means everything was written
here but the copies wanted are not all there yet: `COMPLETE` exists on this
node and the release is usable, so wait for the peers rather than republishing.
A file `put` that returns 75 does not stop the run — its bytes are here — so
publish finishes the release and reports 75 at the end.

`CURRENT` is written only after `COMPLETE`, and it is a plain file: `remuda vfs
get remuda:///design/CURRENT` prints the release name. `vfs serve` does not
resolve it, so open `/NAME/` explicitly. The free-space reserve is checked on
the filesystem holding `DIR`; the store's own disk belongs to the daemon.

## Reading in a browser: `remuda vfs serve`

```sh
remuda vfs serve --root design
# Serving remuda:///design at http://127.0.0.1:53412/ (read-only; scripts blocked)
```

Open the printed URL. Every file under the prefix is a page; a directory is a
listing with links, so a design artifact that loads `site.css`, an image or a
second page renders with its assets. `--root` defaults to `/` (the whole
store). `--addr` defaults to `127.0.0.1:0`: loopback on a free port.

The bridge is read-only: `GET` and `HEAD` only, no cookies, no upload, nothing
written back to the store. Stored HTML is treated as untrusted by default:
`Content-Security-Policy: sandbox` gives it an opaque origin and no script, so
links, stylesheets and images work but a page cannot make its own requests or
read another file. A link you click still navigates the tab, so its target sees
a normal GET; it gets no access to the bridge page or the store. The same
policy covers XML and XHTML documents (a browser parses those as documents and
can run their script) and every other type, so no file is served as an
unpoliced document.

`--allow-scripts` drops that guard for artifacts you wrote and trust. The page
then runs as the loopback origin: it can read every file under the bridge, use
that origin's storage, open popups, and put what it reads into an outbound
navigation. Workers stay disabled, because a worker would bypass the page's
`connect-src` and could send bytes anywhere. Treat a served tree as readable by
anyone who can run code in it.

- A non-loopback `--addr` is refused unless you also pass `--allow-remote`.
  That flag publishes cluster files to the network, so it is asked for twice on
  purpose.
- Paths are percent-decoded once and then checked by the same rules as `put`.
  `..`, `.`, NUL and control characters are refused; the prefix is a component
  boundary, so `--root design` never lists `design-private`.
- Files up to 32 MiB are served. A larger file answers `413`; reading it needs
  `remuda vfs get`.
- `ETag` is the file's sha256, so a reload after a cluster write is cheap. The
  `X-Remuda-Version` response header says which version was served.
- The port has no authentication, and loopback is not private: **another OS
  account on the same machine can find the port and read the served tree**, with
  no daemon credentials. That is the documented consequence of "keep it on
  loopback"; run it on a machine and account where that is acceptable, and
  `--root` only what you would show them. One viewer is one process, and Ctrl-C
  closes it.

## For tools: `remuda vfs read`

`remuda vfs read URI [--max-bytes N]` prints one JSON envelope and exits 0
whenever it produced one:

```json
{"ok": true, "path": "design/index.html", "version": 4, "sha256": "…", "size": 12, "content": "…base64…"}
{"ok": false, "code": "not_found", "detail": "no file at design/index.html"}
```

The code is the same one Lua sees (`not_found`, `too_large`, `no_cluster`,
`unreachable`, `corrupt`, `wrong_cluster`, `bad_uri`, `io`). `--max-bytes`
answers `too_large` from the index without reading the file. This is what
`serve` calls; a script that shells out to `get` should use it instead.

## Giving back copies a node fetched: `remuda vfs release`

`get`, `read` and `resolve` keep the copy on the node that read it, so a node
that only looks at files slowly holds a copy of everything it opened. `release`
gives those back:

```sh
remuda vfs release --dry-run     # what would go, and why the rest stays
remuda vfs release
```

It removes a copy only when this node owes no copy of it (placement does not
name this node), this node did not write it, it is not a conflict's content,
and every node that should hold it has confirmed one. The last copy is never
removed: if no other node can take it, the copy stays and the command says so.
Exit 0 either way; the counts are in the output and in `--json`.

`--older-than SECS` keeps blob files younger than that (it is the file's age,
not the last read); the default is 0. Custody is a peer's answer that it holds
a file of that size, so release trusts admitted peers to be honest about that.
Releasing is not a delete: the file stays in the store and any node can fetch
it again.

A copy `remuda vfs resolve` handed to a tool is **pinned** and is not released:
`resolve` promises that path stays valid. The report counts those as `resolved`,
and `--include-resolved` gives them back when you know the tool is done.

```sh
remuda vfs resolve remuda:///butler/guidance/cluster.md   # pin a copy on this node
remuda vfs release --include-resolved                     # give pinned copies back
```

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
- `Old stores:` appears when `cluster leave` or `init --new-identity` set a
  store aside in `cluster.left-DATE/vfs`. Nothing deletes them; remove a
  folder yourself once you no longer need its files.

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
