# Staged daemon socket publication (#660)

Design proposal; no implementation or runtime validation. Code references are
at `aeed1a881994b19e00cf22f0a521a12dc18e90c0`, not PR #674's head.
**Fact** means inspected source/documentation; **proposal** and **inference**
describe intended behavior and its assumptions. The full input was
`po-remuda/sec-674/SEC-674-report.txt` (reviewed head `0334208d`).

## Current behavior and required invariant

**Facts:** [`native/src/daemon.rs:258`](../native/src/daemon.rs) holds
`SocketLock`, probes the public address, installs signals, then calls
`ipc::listen` at :272. Preparation at :1362 disables name reclamation before
nonblocking setup and pathname chmod. Capture at :277 follows task startup;
`ListenerTask::start` can wait 30 seconds (:507). Capture (:1066) records
pathname `(dev, ino)` without checking type or correspondence to the listener.
`ipc::listen` probes before stale removal
([`native/src/ipc.rs:116`](../native/src/ipc.rs)); interprocess 2.4.4's
`src/os/unix/c_wrappers.rs:235–252` combines bind and listen (:247–248).
Its reclamation guard is installed only after that helper succeeds
(`src/os/unix/uds_local_socket/listener.rs:32–47`).

**Inference:** moving capture earlier, as #674 does, reduces exposure but cannot
make ownership precede public connectability. Preparation/capture failure can
also leave a named socket without a cleanup guard.

**Proposal:** public connectability must imply that the listener is verified,
socket permissions are final, and a cleanup owner already exists. Publication
is one atomic namespace operation; it is not application readiness. Clients
still need a real protocol reply to establish readiness.

## Prior art

**Facts, not claims that these programs implement this transaction:**

- [ssh-agent 9.9p2](https://github.com/openssh/openssh-portable/blob/V_9_9_P2/ssh-agent.c)
  creates a private temporary directory and restricts socket creation with
  umask, then advertises its address. [sshd 9.9p2](https://github.com/openssh/openssh-portable/blob/V_9_9_P2/sshd.c)
  prepares TCP descriptors before listen; it has no Unix pathname to rename.
- [tmux](https://github.com/tmux/tmux/blob/master/server.c)'s
  `server_create_socket` separates bind/listen and restricts creation with
  umask. Its unlink behavior is not suitable for Remuda's live-peer policy.
- [systemd socket activation](https://github.com/systemd/systemd/blob/main/man/systemd.socket.xml)
  separates endpoint ownership from service startup and provides `SocketMode`
  and `DirectoryMode`. A connectable endpoint may precede service readiness.

**Inference:** borrow private creation and explicit descriptor ownership;
atomic staged publication is Remuda's proposed composition, not an attributed
feature of all this prior art. Avoid changing process-wide umask after threads
exist.

## Options

| Option | Benefit | Cost / remaining gap |
| --- | --- | --- |
| Earlier pathname capture only | Small patch; covers later startup errors | Public bind/listen still precedes capture; no descriptor provenance or initial-error cleanup |
| Separate public bind → verify → chmod → listen | Delays connections; no temporary path overhead | Requires a Unix transport adapter; exposes a mutable pathname before verification; needs its own creation guard |
| Private staging → verify → atomic publication | Hides preparation from the public address; one publication boundary | Temporary directory, path-length budget, platform rename support, and distinct descriptor/node identities |
| Supervisor/socket activation | Supervisor owns descriptor and lifecycle | Deployment/client changes; not a portable standalone daemon solution; supervisor must supply ownership provenance |

**Recommendation:** the private staged transaction, on Linux and macOS first,
with explicit portability and failure gates below. Do not label pathname
capture alone as descriptor verification.

## Identity: a necessary correction to the brief

**Fact:** `fstat(listener_fd)` can verify socket type and record descriptor
`(dev, ino)`, but that pair is not portably the filesystem socket's identity.
Linux allocates the descriptor inode in
[sockfs](https://github.com/torvalds/linux/blob/master/net/socket.c), while
[Unix bind](https://github.com/torvalds/linux/blob/master/net/unix/af_unix.c)
creates a separate filesystem node. Linux's `SIOCUNIXFILE` exposes the bound
node through an fd but requires `CAP_NET_ADMIN`; it is unsuitable for an
ordinary user's daemon. A direct listener-fstat/public-lstat equality check is
therefore not the design.

**Proposal:** keep `ListenerIdentity` (fd-derived type/dev/ino, with AF_UNIX and
SOCK_STREAM checked through the descriptor) separate from `NodeIdentity`
(filesystem type/dev/ino). Record the latter relative to an opened, newly
created private directory, without following the leaf symlink. Verify the
bound address through the fd; test a private connection accepted by this
listener before publication, including after rename in platform validation.

**Inference/limit:** successful exclusive bind in a fresh namespace establishes
the node's provenance among cooperating processes; directory-relative metadata
does not prove fd/node equality against a hostile same-UID writer. If strict
fd-derived filesystem identity is mandatory, implementation is gated on a
supported per-platform primitive; this portable proposal cannot promise it.

## Transaction and existing lifecycle

**Proposal:**

1. Acquire the existing public-name `SocketLock` and retain it for the daemon's
   lifetime (`daemon.rs:773–862`). Pin the validated public parent with an open
   directory fd. Preserve managed `0700` directories and custom-parent policy
   (:866–1020); never unlink the `.lock` file.
2. Refuse a successful public connect. Only the current absence/refused-existing
   policy (`ipc.rs:104–110`) permits takeover; permission and other probe errors
   remain errors. Recheck an observed stale node's type/dev/ino before removing
   it. Never remove a live peer or an arbitrary regular file/symlink as stale.
3. Exclusively create a short random `0700` directory in that same parent,
   register a transaction guard immediately, and pin its directory fd. Create
   a raw Unix socket under the guard, verify the fd, then bind a short leaf.
   Record the node receipt before further fallible preparation. Using a small
   adapter avoids interprocess's unguarded bind/listen constructor failure.
4. Set the staged node to `0600` without following symlinks, recheck its receipt,
   configure nonblocking accept, and listen privately. Import the descriptor
   with reclamation disabled from the outset: interprocess's Unix
   `From<OwnedFd>` has no reclamation (`uds_local_socket/listener.rs:122–128`).
   Allocate the final owner, shared state, and any bookkeeping before publish.
5. Probe the public name again immediately before publication. Use
   directory-relative atomic rename that **fails if the destination exists**:
   Linux `RENAME_NOREPLACE`; macOS
   [`renameatx_np(RENAME_EXCL)`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/rename.2).
   Unexpected occupancy aborts; it is never overwritten. Unsupported kernels
   or filesystems return a clear error, with no ordinary-rename fallback.
6. Commit the guard's location to the public name by an infallible state change;
   retain the already recorded node identity, rather than recapturing there.
   Hold the cleanup-state mutex across rename and commit, so concurrent
   shutdown cannot observe a published node with the old cleanup location.
   Remove only the now-empty owned staging directory. Start anti-entropy,
   cluster listener, image and request handling after this owner is available.
   Image startup still sees the public endpoint (`daemon.rs:279–305`).

Staging must fit the Unix address limit (`ipc.rs:63–80`: 107 Linux / 103 other
bytes). Check both public and staging lengths before creating anything; report
a shorter-runtime-path remedy. Keep staging on the same filesystem; no copy
fallback on `EXDEV`. Verify renamed-socket connection behavior on both target
platforms before enabling the adapter.

**Cleanup proposal:** the guard carries directory receipts, descriptor, node
receipt and staged/published state. Preparation, injected capture-validation,
listen, rename, and later startup errors close the descriptor and unlink only a
matching socket receipt, then remove only a matching empty staging directory.
Neither listener Drop nor capture failure may unconditionally unlink the
public path. If the *first* node metadata read fails, retry a bounded ownership
recovery; without a receipt, preserve the unknown entry and report private
residue. Total metadata failure cannot guarantee both deletion and ownership
safety. This is an explicit limit, not a claim of complete residue removal.

**Rebind and shutdown:** use the same transaction for SIGUSR1. Prepare a new
listener/owner before publication; leave the old pair installed on failure.
Commit the new pair together, never today's install-even-if-refresh-failed
behavior (`daemon.rs:1380–1408`), and preserve queued-old-client handling.
The filesystem cleanup guard reads the recorded identity
(:1084–1127); signal exit (:1309) and protocol shutdown (:2579) call it.
Hosted-session shutdown authorization instead reads daemon/session claims and
peer ancestry (:2530–2566); it must not acquire a new dependency on socket
inode identity.

[#603](https://github.com/warmblood-kr/remuda/issues/603) concerns the SIGUSR1
live-replacement preservation regression, not the cluster host lock. Keep
second-daemon refusal for the same socket; preserve separately the cluster
`hosted by another daemon` status (`daemon.rs:735–752`, test :4533).
The existing #603 test already waits for two accepted daemon probes
(`native/tests/daemon.rs:1098–1118`); retain that synchronization and add the
explicit publication/commit handshake rather than reverting to existence polls.

## Windows and scope

**Fact:** the current Windows transport is a named pipe (`ipc.rs:1–12`);
`SocketLock` is a no-op (:856) and inode capture/cleanup is Unix-only
(`daemon.rs:1066–1097`). A pipe has no filesystem leaf to chmod or atomically
rename. Staged filesystem publication is **N/A to the current transport**.

**Proposal:** preserve Windows behavior in these Unix slices. A separate pipe
lifecycle design would prepare state before `CreateNamedPipe`, apply the
security descriptor at creation, retain the handle as ownership, and assess
[`FILE_FLAG_FIRST_PIPE_INSTANCE`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createnamedpipea)
for first-instance collision refusal. Closing owned handles replaces unlink;
delaying accept alone does not guarantee clients cannot open an existing pipe.
A future Windows AF_UNIX backend would need its own rename/node-identity tests;
Unix filesystem semantics cannot be assumed there.

**Limits:** `0700` excludes other UIDs under the existing directory policy, not
other processes of the same UID. SocketLock is advisory. Metadata-check/unlink,
stale takeover, staged-source replacement, and ancestor-path races remain
against noncooperating same-user writers. No authentication or isolation claim
is added ([`README.md:178–186`](../README.md),
[`docs/self-modification.md:146–152`](self-modification.md)). Crash/SIGKILL and
persistent filesystem errors may leave private residue. Never sweep unknown
staging directories recursively during a later startup.

## Small PR slices and RED tests

All tests use isolated scratch namespaces and explicitly enabled test support,
with bounded reached/release handshakes rather than sleeps or existence polls.
Each slice includes its tests; additions go RED before enabling the behavior,
or through a stated negative control for a new helper.

| Slice | Change | RED evidence required |
| --- | --- | --- |
| 1. Ownership transaction | Split fd/node identities; guard raw creation and preparation; reject non-socket receipts | Inject preparation and capture-validation errors after a receipt: owned node disappears, unrelated sentinel survives. Inject first-metadata failure: no unknown unlink and no public endpoint. Disable receipt checks as negative control for type/replacement tests. |
| 2. Staged publication | Private staging, mode finalization, owner-before-publish, exclusive rename adapters | Hold the explicit pre-verification boundary: public connect fails; release and get a protocol reply with owner already recorded and mode 0600. Current public bind/listen fails this test. Force rename error and occupied destination: destination identity/content survives, staged owned artifacts are cleaned. |
| 3. Rebind and compatibility | Share the transaction with SIGUSR1; commit listener/owner together | Force rebind validation failure: old pair remains usable; current refresh-failure behavior fails. Make #603's replacement readiness explicit, verify live connect and full type/dev/ino through signal and shutdown. Retain queued-client, stale-takeover and concurrent-start regressions (`native/tests/daemon.rs:874, :987, :1084, :1180, :3267`). |
| 4. Platform gates | Linux/macOS rename, address-limit and transport-contract coverage | Renamed endpoint must accept a real reply; unsupported rename and oversized staging path must fail before publication. Negative control restoring overwrite-rename fails destination-preservation test. Run existing hosted-shutdown and Windows named-pipe regressions. |

No tests or Cargo jobs were run for this design-only change. The strict
fd-to-filesystem identity requirement and cleanup under total metadata failure
need explicit scope acceptance before claiming both SEC-674 MUSTs fully closed.
