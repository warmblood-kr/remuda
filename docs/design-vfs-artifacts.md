# Publishing artifacts on the managed store

Owner decision: the cluster store holds **managed files only**. A file that
already exists on a node is copied in (`vfs put`) and becomes a new managed
file; nothing ever links to a path on someone's disk. This document says what
that buys, what it does not, and the smallest layer worth adding on top.

It replaces an earlier proposal for a `path → (origin node, origin path)`
index. That proposal needed versioned ownership, missing-versus-deleted
tracking, per-export authorization and torn-read rules, and still lost
availability whenever the origin was down. Copying a file in is one command.

## What is already guaranteed (shipped: #742, #778)

| Concern | Behaviour today |
|---|---|
| Storage | immutable blobs named by sha256, in the private cluster directory, 0600 |
| Replication | placement over admitted, non-down nodes; 3 by default, 1–16 per file |
| Write durability | `put` returns once this node stored it and told peers; `--wait` / `--min-copies K` wait for copies (exit 75 if short) |
| Offline node | a reader needs a holder it can reach, whose index entry it already has; repair pulls a copy to the next ranked node when a holder is gone, after the down grace |
| Concurrent writes | both survive; every node picks the same winner; up to 16 conflicts per path, `resolve-conflict` settles |
| Delete | a tombstone travels with the index; GC forgets it after `--tombstone-days` once every node has it |
| Membership change | `drain` before `leave`, `revoke`, old stores set aside |
| Integrity | a blob is verified against its digest before it is served or replicated |

## What this does **not** guarantee

These are design limits, not bugs to file:

1. **Availability of a file whose holders are all down or unreachable.**
   Replication covers node loss, not a partition that isolates every holder,
   and three replicas is a *target*: `put` returns before peers have copied, so
   durability is only as good as the copies confirmed so far. There is no
   fallback and no "eventually" promise; `status --copies` names short files.

1b. **Guidance is worse than a CLI read, on purpose.** `remuda.vfs.read` from
   Lua answers from this node's index and cached bytes and never fetches over
   the network. A freshly joined node therefore reports the cluster guidance
   layer as unavailable even while every holder is healthy, until something
   copies the file there (`remuda vfs get`). Nothing tracks the source, reloads
   on change, or detects a partition. A node that must show guidance needs a
   prefetch step, and this is not something the store fixes for it.
2. **Per-path permission.** Any admitted node may read, write and delete
   anything. The cluster channel is mutually authenticated (Noise), but there
   is no ACL inside it.
3. **Read audit.** `audit.jsonl` records peer *session* writes, not VFS reads.
4. **Encryption at rest beyond the OS.** Blobs are 0600 files; a root or
   same-user process reads them.
5. **Viewer authentication.** `vfs serve` is loopback, unauthenticated; another
   OS account can reach the port (documented in `docs/vfs.md`).
6. **Per-tenant quota.** 10,000 live files / 30,000 index entries per cluster.
7. **Content freshness against the outside world.** A managed file never
   notices that its source changed on disk; publishing again is a new version.
8. **A coherent snapshot of a directory.** Files are independent entries; a
   publish that is half done looks half done, and there is no history, rollback
   or backup semantics.
9. **Author identity.** A verified hash proves integrity, not who wrote it. The
   cluster channel authenticates admitted peers; it says nothing about who
   produced an artifact.
10. **Isolation from other processes of the same user.** 0600 protects against
    other accounts, not against another process running as the same user.
11. **Membership changes are best effort on one side.** `drain` proves custody
    at the moment it asks; `leave` proceeds after a warning when no peer
    answers. A plan that must not lose data has to check `status --copies`
    itself, not trust that a leave ran.

## The smallest layer worth adding

Everything needed to publish and read is shipped except one thing: **a page is
usually several files**, and `vfs put` takes one. A shell loop works but has no
summary, no preflight and no way to tell a reader whether the tree is complete.

`remuda vfs publish DIR remuda:///PREFIX/ --release NAME` is a *nontransactional
uploader with a final pointer*, not an atomic directory publish:

1. Preflight: regular files only (symlinks skipped and named), at most 500
   files, 32 MiB each and 1 GiB per release, a free-space reserve on the
   source's filesystem, and no existing file at the destination under the same
   release name (a release is immutable; publish a new name instead).
2. Upload each file to `PREFIX/NAME/<relative path>`, one `put` request per
   file, `--min-copies` honoured per file.
3. If every file is in, write the completion marker `PREFIX/NAME/COMPLETE`
   **last**. A missing marker is what makes an unfinished release legible; a
   viewer that is pointed at `PREFIX/NAME/` can check it.
4. With `--pointer`, write `PREFIX/CURRENT` (content: the release name) after
   the marker. It is a plain file, not a redirect: `vfs serve` does not resolve
   it, so a viewer opens `/NAME/` explicitly.
5. Say exactly what happened: `Published N files (X MiB) as NAME`, list
   failures, exit 1 when the release cannot be called complete (a file failed,
   `COMPLETE` was refused, or a requested `CURRENT` failed), and 75 when every
   byte is here but the copies wanted are not (a file `put` that returns 75
   counts as uploaded and does not stop the run) — the same 75 `put` uses. A
   transport failure leaves the marker's state unknown, not proven absent, and
   the message says so. Recovery is to check `vfs stat` for `COMPLETE`, then
   `vfs get` `CURRENT` and confirm its value names the intended release (an
   existing pointer can be an older one). A release that already has files is
   refused by the destination check, so a genuinely missing marker means
   publishing a new `--release` name.

What it deliberately does not do: delete a previous release (it stays until
`vfs rm`), roll back a partial upload, guarantee that `COMPLETE` or `CURRENT`
has reached every node when the command exits, or make an in-place update
atomic. A manifest with reader enforcement would make the guarantee stronger;
it is not worth the reader-side changes yet.

## Staged plan

1. **Shipped to main:** store, replication, conflicts, tombstones. **Open PRs:**
   `vfs serve` (#886), `vfs release` (#887/#888).
2. **Built on this design (PR #891, not merged):** `vfs publish` as above. A
   `vfs ls` that groups by path is nice to have and deferred.
3. **Operational:** publish the cluster guidance file the owner approves
   (`remuda:///butler/guidance/cluster.md`) — no code — and give each node a
   prefetch step, because a Lua guidance read never fetches (see 1b).
4. **Only if the owner needs it:** read-only membership (a node that can read
   but not write) and per-path ACLs. Both are wire and index changes; neither
   is promised here.
5. **Later:** a read audit and a per-cluster quota.

Steps 1–3 give the owner exactly the stated outcome — shared guidance, design
artifacts, and a browser viewer — with the consistency model that already
exists. Steps 4–5 are named so nobody assumes they are already true.

## Why not links

A link design looks lighter because it copies nothing. In practice it moves
three hard problems into a store that currently has none: deciding what "the
file changed" means when nobody was watching; authorizing a read of a path on
another machine; and explaining why a path that resolves in the index cannot be
opened. Copying a file in trades disk for a single, already-tested answer. On a
cluster whose nodes are the same household and whose artifacts are megabytes,
that is the right trade.
