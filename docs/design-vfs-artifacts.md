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
| Offline node | a reader needs one holder up; repair pulls a copy to the next ranked node when a holder is gone |
| Concurrent writes | both survive; every node picks the same winner; up to 16 conflicts per path, `resolve-conflict` settles |
| Delete | a tombstone travels with the index; GC forgets it after `--tombstone-days` once every node has it |
| Membership change | `drain` before `leave`, `revoke`, old stores set aside |
| Integrity | a blob is verified against its digest before it is served or replicated |
| Browser read | #886: loopback HTTP bridge, sandboxed HTML/XML/SVG, one prefix |
| Disk | #887: `vfs release` returns copies a node only read |

## What this does **not** guarantee

These are design limits, not bugs to file:

1. **Availability of a file whose holders are all down.** Replication covers
   node loss, not a partition that isolates every holder. There is no fallback
   and no "eventually" promise; `status --copies` names the short files.
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

## The smallest layer worth adding

Everything needed to publish and read is shipped except one thing: **a page is
usually several files**. `vfs put` takes one file, so publishing an artifact
directory is a shell loop with no summary, no partial-failure story and no way
to make the whole tree appear at once.

`remuda vfs put --recursive DIR remuda:///PREFIX/` closes that gap with no wire
change and no new storage:

- walks `DIR`, regular files only (symlinks are skipped and named), refuses to
  cross a mount? no — keeps it simple: skips symlinks;
- stores each file under `PREFIX/<relative path>`, one `put` request per file,
  so the daemon contract is unchanged;
- stops at 500 files or 64 MiB each and says so rather than half-doing it;
- prints `Published N files (X MiB) to remuda:///PREFIX/; M failed`, lists the
  failures, and exits 75 if any file was stored only here (same meaning as
  `put`);
- does not delete: a removed source file leaves a stale managed file until
  `vfs rm`, because "the directory changed" is not evidence about the cluster.

That is the whole addition: a publisher, not a new consistency model.

## Staged plan

1. **Now (shipped):** store, replication, conflicts, tombstones, `vfs serve`,
   `vfs release`.
2. **Next (small, this design):** `vfs put --recursive` + a `vfs ls --tree`
   that sorts by path, so an artifact directory is publishable and reviewable
   in two commands.
3. **Operational:** publish the cluster guidance file the owner approves
   (`remuda:///butler/guidance/cluster.md`) — no code.
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
