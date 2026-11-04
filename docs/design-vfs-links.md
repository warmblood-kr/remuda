# Path → origin index instead of shared bodies: design analysis

Owner's proposal: the cluster shares a `path → (origin node, origin path)`
index and reads fetch from the origin, instead of moving file bodies into a
replicated store. This compares it with what `remuda vfs` does today (#742,
#778) and names what each part would have to become.

## What exists today

`remuda:///PATH` names a **snapshot**, not a file on a machine. `put` copies
the bytes to a sha256-addressed blob on this node, records
`path → {sha256, version, version-vector, replicas}` in a SQLite index,
and tells peers. Placement picks 3 admitted nodes by hash; repair pulls a
missing copy when a holder dies; a delete leaves a tombstone that travels
with the index. Readers never talk to a writer: any holder serves the bytes.
So today: the index is a *catalogue*, and the bodies are the system of record.

The proposal inverts that: the index is a *location directory*, and the origin
node's filesystem is the system of record.

## Prior art, and what it actually guarantees

| System | Directory holds | Where bytes live | What happens when a node dies |
|---|---|---|---|
| OpenAFS VLDB | volume → file servers | replicated volumes, per-volume release | client retries another listed server; a volume with one copy is unavailable |
| GlusterFS (DHT) | nothing central — location computed from a hash of the path | brick filesystems, replica sets | a file on a dead brick is unavailable until its replica or the brick returns |
| CephFS | inode/dentry metadata in MDS, journaled; data as objects in RADOS | RADOS pools, replicated or erasure-coded | metadata stays available; data availability is the pool's job |
| IPNS | signed name → immutable CID record with a TTL | wherever the content is pinned | the name resolves, the content may not; resolution ≠ availability |

Two lessons repeat. First, **a directory alone never guarantees availability**:
AFS and Gluster both keep the bytes on nodes and lose access when those nodes
are gone; IPNS resolves a name to a CID that may have no host; CephFS only
works because RADOS is a separate replication layer. Second, the systems that
stay coherent put the *mutable* part in one place — the MDS journal in CephFS,
the VLDB in AFS — and never let two origins own one name at once.

That last point is the real difference from today's store — and the place the
first draft of this note was too optimistic. Declaring one origin does not
make concurrent registration impossible: two partitioned nodes can each
register the same path against their own index copy. Ownership therefore has
to be a *versioned* fact, not a local compare-and-set: kind, origin
fingerprint and a registration generation go into the same record the version
vector already orders, `publish`/`relink`/`unlink` carry an expected
generation, and a stale writer is refused instead of merging. The origin is
bound to its authenticated fingerprint, never to a display label, and old
builds must refuse an unknown entry kind rather than quietly treating it as
managed. The owner then becomes a single point of failure for that path —
stated plainly rather than hidden.

## Two entry kinds, one namespace

The owner's split of "register an existing file" from "create a new file in
the VFS" is the right seam. It becomes two entry kinds under one path space:

- **managed** (today's `put`): the bytes are copied in, replicated, and the
  store answers for them forever. Availability is the store's problem.
- **linked** (new): the index records `origin_fp`, `origin_path`, and the
  observed `{mtime, size, sha256, version}`. Reads go to the origin. The store
  never claims the bytes are available; it can say only *where* and *as of
  when*.

`ls`, `stat`, guidance loading and `vfs serve` would show `linked` entries as
such, with `origin up/down` and the age of the last observation. A viewer
would report "origin offline" instead of a timeout.

## The dimensions the owner named

**Register vs create.** `register LOCAL_PATH remuda:///PATH` adds a linked
entry; nothing is copied. `put` stays managed. A linked entry can be
`publish`d (copy now → managed) or `unlink`ed (drop the pointer, touch no
file). A managed entry can become linked only by `unlink` + `register`, never
silently.

**Out-of-band change or delete at the origin.** The origin is authoritative,
so this is normal operation, not corruption: `stat` reports `origin_missing`
or `origin_changed` (digest differs from the last observation) and reads
return the *current* bytes with the new digest. A change bumps the entry
version with the origin node as writer. A delete at the origin makes the
entry unavailable but does not delete the path from the index until someone
runs `unlink` — otherwise a temporarily unmounted disk would look like a
cluster-wide delete. That distinction (missing vs deleted) is the single most
important thing a pointer design must not collapse.

It does not replace delete propagation. `unlink` still needs the shipped
tombstone, change cursor and stale-reset machinery, or a returning origin can
republish an old generation after everyone forgot it. Two more rules: an
unanswered origin is *unknown*, never "missing"; and a mutable read can tear
if the file is replaced mid-read, so the origin must open one descriptor and
either hand back the exact bytes its digest describes or refuse and retry.

**Node offline.** Availability is exactly the origin's. The index makes this
*observable*: `vfs ls --unavailable` lists linked entries whose origin is
down, so a human sees the blast radius. Optional `--cache` copies bytes for
read-only offline use, marked as a cache, never as a replica, and evictable —
otherwise the pointer store quietly becomes a body store with worse
guarantees.

**Versioning and concurrent modification.** One origin per path means writes
are single-writer by construction; the version vector can stay, with only the
origin incrementing it. A second writer is refused at registration (path
already owned) rather than resolved by conflicts. A stale reader can be told
`version N` and `observed_at T`; there is no conflict machinery to keep.

**Permissions.** Today any admitted node reads and writes everything. With
links, a peer asks for a *path on another machine*, which is a different
privilege and must not be handed out by the index. A cluster capability bit is
only protocol negotiation, not authorization: the origin needs its own export
table. `remuda vfs export PATH` (run by the origin's operator) creates an
export id bound to that fingerprint and generation; a `LinkRead` carries the
export id, never an arbitrary `origin_path`, and the origin resolves only ids
in its own table, checks the caller is an admitted member, and opens a regular
file with a symlink-safe descriptor-relative open, bounded by size and offset.
The index may name a path; only a local export may make it readable. Per-peer
ACLs can come later; per-export scoping cannot.

**Path collisions.** Registering `remuda:///x` when `x` is managed must fail
with a clear error, and the reverse too. Registration is a compare-and-set on
the index: owner may change only with an explicit `relink --from OLD_FP`.
Two origins racing to register the same path resolve by the same rule as
today's writes (higher version vector wins), and the loser is told, not
retried forever.

**Replication and failure recovery.** Linked entries get none — that is their
point — and the docs must say so in one line: *a linked path survives exactly
as long as its origin and that file*. When an origin is lost for good, reads
are **refused** with `origin_unavailable`; the index keeps the last observed
digest and time for diagnosis, and nothing is promoted automatically, because
a cache cannot prove it holds the latest bytes. An operator may `publish`
chosen bytes into a new managed generation. Guidance and design artifacts
therefore default to managed; live working trees get linked.

## What it would cost to build

New peer requests: `LinkRegister`, `LinkStat`, `LinkRead`, `LinkDrop`; a new
entry field set (`kind`, `origin_fp`, `origin_path`, `observed_at`); origin-
side path confinement (a shared allowlist of directories, resolved under the
origin's account, never `..`); a capability bit for "may serve linked reads";
and index schema step N+1. The `vfs serve` bridge would need one new status
(`502 origin offline`) and must never present a stale cache as fresh.

## Recommendation

Do not replace the body store. Add linked entries beside it, with the rule
that anything the cluster must be able to read when a node is down stays
managed.

1. Managed stays the default and the only thing guidance and design artifacts
   use.
2. Ship `register`/`unlink`, the versioned kind/origin/generation record, the
   origin's export table and an export-scoped `LinkRead` **together**. A
   pointer index without an authorization story is not shippable, and
   register without read is a feature nobody can use.
3. Defer caching entirely. It is a separate design with its own eviction and
   freshness rules, and `vfs release` (#887) already gives it a tool.
4. Keep Lua's `remuda.vfs` contract local-only and non-blocking: a linked
   entry's `read` is a network call, so Lua either gets `origin_unavailable`
   or an explicitly cached copy, never a hidden blocking fetch.

If step 2 cannot land soon, doing nothing is a good outcome: today's store
already answers every requirement except "do not copy the bytes", and a
half-built pointer layer would trade a known availability model for a worse
one.
