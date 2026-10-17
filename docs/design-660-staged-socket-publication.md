# Staged daemon socket publication (#660)

Historical design-review scope (docs-only revision): this was a proposal with
no implementation or runtime validation at that time. Code references are at
`aeed1a881994b19e00cf22f0a521a12dc18e90c0`, not PR #674's head.
**Fact** means inspected source/documentation; **proposal** and **inference**
describe intended behavior and its assumptions. The review input was the SEC
review of #674 (head 0334208d). This revision incorporates the three MUSTs and
five SHOULDs from the independent SEC design review (delta APPROVE); owner scope
acceptance remains outstanding.

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

**Proposed contract:** publication of this transaction's endpoint implies a
validated descriptor, exclusive-bind provenance among **cooperating processes**,
a socket-type node receipt, final permissions, and installed cleanup ownership.
There is no portable fd-to-filesystem-inode proof against a same-UID writer.
Before commit, unknown metadata or cleanup failures abort publication and may
leave reported residue; they never authorize deleting unverified entries.
After commit, failed empty-staging-directory removal reports residue without
undoing publication; fatal startup failures use terminal published cleanup.
Total metadata failure, persistent cleanup errors
and SIGKILL preclude guaranteed deletion. Publication is atomic namespace
visibility, not application readiness; readiness requires a real protocol reply.

**Acceptance gate:** Butler/owner must explicitly accept the advisory identity
and residue limits before SEC-674 closure is claimed. This review does not
provide that acceptance. If strict fd-derived filesystem identity or complete
deletion remains mandatory, this recommendation is gated; neither SEC-674 MUST
is reported fully closed under that strict scope.

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

**Proposal:** keep `ListenerIdentity` (fd-derived type/dev/ino) separate from
`NodeIdentity` (filesystem type/dev/ino). Validate AF_UNIX via
`getsockname`/sockaddr family, SOCK_STREAM via `SO_TYPE`, and listening state
after listen where supported; do not require macOS `SO_DOMAIN`. Capture node
metadata without following the leaf, relative to the owned staging fd.
Require CLOEXEC for listener, directory handles and self-check/test streams;
preserve nonblocking accept and blocking request streams
(`daemon.rs:1322–1327`, interprocess `c_wrappers.rs:174–190`). Import does not
perform validation and its accepted streams default to blocking.

Private connect/accept and renamed-public-route checks are **platform acceptance
tests**, not production protocol handshakes before workers exist. Tests use
bounded connect/accept deadlines, then a real protocol reply after the loop
starts. After rename, `getsockname` may retain the staging address; never require
it to equal the public spelling (see
[XNU Unix address handling](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/uipc_usrreq.c)).

**Platform limits (SEC-708):** CLOEXEC is atomic at socket creation only on
Linux/Android (`SOCK_CLOEXEC`); elsewhere `socket`+`fcntl` leaves a fork/exec
window, so atomic creation is not certified there. On macOS/BSD an accepted
stream inherits the listener's `O_NONBLOCK`; the accept loop must normalize
request streams to blocking (`daemon.rs`, `set_nonblocking(false)`).

**Inference/limit:** successful exclusive bind in a fresh namespace establishes
the node's provenance among cooperating processes; directory-relative metadata
does not prove fd/node equality against a hostile same-UID writer. If strict
fd-derived filesystem identity is mandatory, implementation is gated on a
supported per-platform primitive; this portable proposal cannot promise it.
Receipts compare node type plus `(dev, ino)`. Filesystems may reuse an inode
after unlink, so a replacement created after removal could theoretically reuse
the same pair and type before the next check. The receipt is therefore an
advisory change detector, not a generation-proof token; it does not claim to
defeat inode reuse or a hostile same-UID writer.

## Transaction and existing lifecycle

**Proposal:**

1. Preflight complete public/staging **encoded byte** lengths, separators and
   NUL, before socket/staging mutation or stale removal. Prefer checking before
   lock acquisition; existing acquisition can create runtime directories and
   `.lock` (`daemon.rs:793–808`). Limits (`ipc.rs:63–80`) are 107 usable bytes on
   Linux / 103 on other configured Unix targets. A dirfd does not shorten bind's
   sockaddr. Report the shorter-runtime-path remedy.
2. Acquire public-name `SocketLock` for the daemon lifetime (:773–862). Open the
   public parent with no-follow/CLOEXEC and validate **that opened object** using
   existing owner/mode rules (:866–1020). Use this same fd for staging creation,
   receipts, stale removal, rename and public cleanup. Initial live-peer refusal
   probes the address clients use; other than absence/refused-existing
   (`ipc.rs:104–110`), probe errors abort. Leave stale occupancy until preparation
   succeeds. Never delete `.lock`; an existing file/PID text is diagnostic,
   whereas flock ownership ends on fd closure or process death.
3. Preallocate guard bookkeeping, exclusively mkdir a short random `0700`
   staging directory in that parent, and register its creation immediately.
   Capture its directory receipt and open/validate its no-follow CLOEXEC fd.
   Restrictive umask may prevent that open: recover only a verified directory
   through parent-fd-relative no-follow mode repair, then reopen and compare
   receipts. Finalize usable owner mode before contents; leave process-wide
   umask untouched. Unknown acquisition failures preserve residue. Create a raw
   socket under the guard, validate the fd, install signal notification handlers
   before any bind (preserving `daemon.rs:267–271`), bind a short leaf, and capture its
   receipt before other preparation. This avoids the constructor's unguarded
   bind/listen failure. Bind uses the checked full pathname: neither platform
   generally has bindat; no process-wide chdir. Ancestor replacement remains an
   advisory-model limit. Namespace checks must agree with the client address in
   the supported model.
4. Set the staged node to `0600` without following symlinks, recheck its receipt,
   configure nonblocking accept, and listen privately. Import the descriptor
   with reclamation disabled from the outset: interprocess's Unix
   `From<OwnedFd>` has no reclamation (`uds_local_socket/listener.rs:122–128`).
   Preallocate the current-generation state before publish. The permission
   adapter must affect the **filesystem node**, not `fchmod(listener_fd)`.
5. Probe occupied public sockets again outside the lifecycle mutex after private
   preparation and collect a candidate stale socket receipt. Refused connect is
   takeover policy, not universal proof of
   permanent staleness; a noncooperating peer changing listen state is outside
   the guarantee. Reject regular files and all symlinks, including dangling
   ones. Recheck staged source receipt immediately before publication. Use
   directory-relative atomic rename that **fails if the destination exists**:
   Linux `RENAME_NOREPLACE`; macOS
   [`renameatx_np(RENAME_EXCL)`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/rename.2).
   Unexpected occupancy aborts; it is never overwritten. Unsupported kernels
   or filesystems return a clear error, with no ordinary-rename fallback.
6. Under the shared lifecycle mutex, check nonterminal state and expected
   generation, recheck source and candidate-stale receipts, remove only the
   verified stale socket if applicable, perform exclusive rename, then install listener and public
   receipt together by an infallible commit. Retain the existing receipt; no
   public recapture. A successful rename transfers authority from the staged
   guard to the shared holder; failure leaves it staged.
   Remove only the now-empty owned staging directory. Start anti-entropy,
   cluster listener, image and request handling after this owner is available.
   Image startup still sees the public endpoint (`daemon.rs:279–305`).

Verified stale takeover intentionally removes the stale node immediately before
rename; a subsequent rename failure can leave the address absent. Never restore
that stale name over a new occupant. Abort preservation applies to unexpected or
live occupancy and the retained old rebind fd, not resurrection of stale nodes.

Keep staging on the same filesystem; no copy fallback on `EXDEV`. **Before any
platform activation**, prove exclusive rename and no-follow permission
finalization on that platform/kernel/filesystem/API combination: Linux
[`fchmodat2(..., AT_SYMLINK_NOFOLLOW)`](https://github.com/torvalds/linux/blob/master/fs/open.c)
is the proposed permission adapter; legacy fchmodat's syscall has no flags.
macOS proposes
[`fchmodat(..., AT_SYMLINK_NOFOLLOW)`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/chmod.2).
Reject non-socket receipts before and after chmod. Unsupported permission or
rename support aborts; no following-chmod or overwrite-rename fallback. These
links establish API shape, not a certified deployment matrix. The enabling PR
must declare its supported kernel/OS/filesystem/API versions and runtime
capability rejection. Fd import,
CLOEXEC, accept/stream mode, path budgets and renamed-endpoint replies are also
prerequisites in the first enabling slice.

## Shared lifecycle and cleanup

**Proposal:** every handler and signal/poll loop references one stable
`Arc<Lifecycle>`; its immutable capability retains the original `SocketLock`,
and its mutex holds `Preparing`, `Running(CurrentGeneration)`, or terminal
`ShuttingDown`. A generation contains the current listener, parent-fd-relative
public receipt/location and generation number. Poll views/retired descriptors
have no independent unlink authority. Older handlers consult this holder's
current receipt; never swap only the loop's Arc and leave their owner obsolete
(today's clones: `daemon.rs:1332`, shutdown owner: :2517).

Rebind receives a borrow of the **already-held** SocketLock capability and the
shared holder; it must not reopen/reacquire the lock. Prepare privately outside
the lifecycle mutex. Under that mutex, reject terminal state or a changed
generation, recheck receipts, rename and infallibly commit the new listener and
receipt together. Only then retire the old listener; existing/queued clients
get a response or clean close. On prepublication failure retain the old fd and
established/queued clients; a deleted old pathname is not still reachable.
Do not repeat today's install-even-if-refresh-failed flow (:1380–1408).

Authorized protocol shutdown, signal shutdown, and postpublication fatal
startup failure set `ShuttingDown` **before cleanup** under the same mutex,
then conditionally clean the current receipt. Later publish attempts abort and
clean only their verified staging artifacts. Terminal state never reopens,
even if replying or task teardown fails. Retired owners cannot clean the current
generation. Filesystem cleanup currently reads identity (:1084–1127); signal
exit (:1309) and protocol shutdown (:2579) must all use this common authority.
Shutdown authorization still uses claims/ancestry (:2530–2566), with unchanged
override precedence and verdict; refusal does not enter terminal state.

**Lock order:** lifetime SocketLock capability → lifecycle mutex. Do not nest
registry, image, request-drain, task-control or other mutexes with lifecycle;
collect authorization inputs first and release their locks. Peer probes,
protocol replies, task joins, draining and reaping occur outside lifecycle.
Only state transitions and a finite set of namespace syscalls occur inside it.
Actual signal handlers continue only to queue notifications (:1414 onward); the loop
does the locking. Concurrent shutdown callers use idempotent terminal cleanup.

**Failure policy:** guard bookkeeping is preallocated. Conditional cleanup uses
the same pinned parent/staging fds and type/dev/ino receipts, never recursive
sweeps. First receipt failure permits one bounded recovery attempt without
relaxing validation; unknown/mismatched entries are preserved. Cleanup errors
report residue without replacing the primary failure. After successful rename,
no rollback to a staging location or public recapture is valid.

| Failure boundary | Ownership-safe action / residue |
| --- | --- |
| mkdir | No created entry on failure; close owned handles. |
| Directory open / first directory receipt | Recover once through verified parent-relative metadata; no usable receipt means preserve unknown directory and report it. |
| Directory mode repair / validation | No contents yet; rmdir only a matching empty directory. Unknown receipt or ineffective privacy aborts. |
| Raw socket creation / fd validation | Close owned fd; remove only verified empty staging directory. |
| bind | Clean a leaf only if a successful bind or recovery yields its receipt; constructor errors never authorize blind unlink. |
| First node receipt | Recover once; if persistent, close listener, leave unknown leaf/private directory, and do not publish. |
| chmod / nonblocking / listen / import | Close owned fd and remove only matching socket leaf and matching empty directory. Import ownership transfers once, without reclamation. |
| Rename error / terminal or generation conflict | Preserve unexpected public occupancy and old generation; clean only verified staging artifacts. Authorized stale removal is not reversed. |
| Commit | Infallible under lifecycle mutex; successful rename irrevocably selects the public receipt. |
| Staging-rmdir after commit | Report private residue and keep the public generation active; never revert its cleanup location. |
| Later task startup | Terminal published abort, conditional public cleanup and task teardown; no legacy-constructor fallback. |
| Unlink / rmdir / cleanup metadata | Preserve unknown/mismatched entries; report residue. No guaranteed deletion under persistent filesystem failure. |

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

**Limits:** private creation assumes effective permissions exclude other UIDs;
mode bits alone do not prove that on ACL-bearing custom parents. Activation must
validate restrictive inherited ACLs or explicitly restrict supported deployment
to environments providing that effective privacy; otherwise abort before bind.
Finalize usable owner permissions without widening others' access. `0700` does
not exclude other processes of the same UID. SocketLock is advisory.
Metadata-check/unlink, stale takeover, staged-source replacement, and ancestor-path races remain
against noncooperating same-user writers. No authentication or isolation claim
is added ([`README.md:178–186`](../README.md),
[`docs/self-modification.md:146–152`](self-modification.md)). Crash/SIGKILL and
persistent filesystem errors may leave private residue. Never sweep unknown
staging directories recursively during a later startup.

## Small PR slices and RED tests

All tests use isolated scratch namespaces and explicitly enabled test support,
with reached/release/**completion** acknowledgments, one finite deadline per
test and failure-safe teardown; no unbounded waits, arbitrary sleeps or existence
polls. Hooks are not compiled into ordinary debug/release binaries. Each slice
includes its tests, shown RED before enabling behavior or against a stated
negative control for inactive helpers.

**Dependency/activation:** 1 → 2 → 3; slices 1–2 leave production activation
off. A test build enabling slice 2 guarantees startup ordering only: legacy
SIGUSR1 rebind is explicitly **not covered**. Slice 3 is the first production
enabling slice and must already include every prerequisite for each enabled
platform: no-clobber rename, no-follow node/directory permissions and effective
privacy, fd validation/import/CLOEXEC, accept/stream modes, path preflight,
renamed-route reply, failure preservation, and shared terminal lifecycle. A
platform stays disabled until its matrix passes. Slice 4 only extends coverage
or enables a new platform with the same gates; it cannot defer prerequisites.

| Slice / active contract | Work and RED evidence | Rollback |
| --- | --- | --- |
| 1. Inactive primitives; production unchanged | Identity receipts, transaction, permission/rename adapters, shared holder. Failure table tests; negative controls bypassing receipts, CLOEXEC, no-follow permissions or no-clobber rename must fail. | Remove inactive helpers; close test fds and clean only verified staging receipts. No publication to undo. |
| 2. Inactive startup integration; test-only startup contract | Forced-window and later-startup-error tests below; real renamed reply and exact/non-ASCII byte budgets. Every platform prerequisite lands here or earlier. SIGUSR1 ordering remains open. | Before rename preserve unexpected/live destination; authorized stale removal is not reversed. Afterward use terminal cleanup of the published receipt. Disable test feature; never resume old staging state or invoke the legacy constructor within a failed transaction. |
| 3. Full advisory lifecycle on qualified platforms | Route SIGUSR1, all handlers and signal shutdown through the shared holder; pass held SocketLock. Both concurrency handshakes below go RED with obsolete-owner/terminal-check negative controls. Preserve #603, queued clients, concurrent starts, hosted shutdown and Windows behavior. | Transaction failure preserves old pair until rename; after rename use current published receipt. Release rollback requires stop/conditional cleanup under the held lock before an older binary starts; document reintroduced SEC-674 gaps. |
| 4. Additional platform/coverage only | Repeat full prerequisite suite for any newly enabled kernel/filesystem/API matrix; unsupported/EXDEV and overwrite-rename controls stay RED. | Disable new platform support; apply slice 3's stop/cleanup release rollback, never downgrade a live generation in place. |

**Required acceptance tests:**

- Hold **after bind, before first node verification**, and separately after
  private listen/before rename: public connect must fail. The old public
  bind/listen constructor control reaches the equivalent first boundary and
  fails this assertion. Release, await commit completion, then require a real
  reply with recorded owner and `0600` node.
- Exercise every failure-table boundary. Persistent first-metadata failure
  spans recovery: descriptor closes, no public endpoint, unknown residue
  survives. Separately verify successful bounded recovery. Force task startup
  failure after publish: terminal cleanup removes only the published receipt.
- Replace the **actual guarded leaf after its receipt exists**, with another
  socket, regular-file and symlink sentinels; require preserved type/dev/ino and
  content/target as applicable. Include directory/mode failure and staged-source
  receipt mismatch. Occupied, unsupported and EXDEV publication must preserve
  destination, including dangling symlink occupancy; no recursive cleanup.
- Test complete encoded staging path at exact/over-limit boundaries, non-ASCII
  byte lengths, restrictive umask and inherited-ACL policy. Verify CLOEXEC and
  accept-nonblocking/request-blocking contracts and real reply after rename.

**Two lifecycle handshake regressions (both normal and replacement-preserving
variants):**

1. A handler accepted before rebind pauses before authorized shutdown. Rebind
   commits generation N+1 and acknowledges completion; release that old handler.
   It must use the shared holder, enter terminal state and clean N+1, not N.
   With a verified later pathname replacement, shutdown must preserve it.
2. Pause rebind immediately before lifecycle lock/publish. Shutdown acquires
   lifecycle, marks terminal, cleans current generation and acknowledges that
   boundary; release rebind. It must abort, close staged fd and clean only staged
   receipts, with no later publication. Preserve an unrelated replacement at
   the public name. Accepted-handler shutdown forces actual thread overlap;
   a transition-level variant invokes the signal-loop shutdown entrypoint.
   The in-process test harness defers exit until abort completion; it does not
   expect the single signal loop to run concurrently with its own rebind call.

Keep #603's two accepted probes and add operation-completion acknowledgment;
retain response-or-clean-close for old queued clients and bounded signal exit
(`native/tests/daemon.rs:874, :987, :1084, :1180, :3267`). Cleanup/commit tests
also hold publication under lifecycle synchronization to prove cleanup cannot
observe an intermediate generation.

Historical design-review scope: no runtime tests or Cargo jobs were run for
that docs-only revision. Static whitespace/link checks did not certify the
platform matrix, RED/GREEN behavior, or the outstanding Butler/owner scope
acceptance.

## Slice 3a (inactive): promote, abort, probe

`startup::open` is no longer `cfg(test)`; it has no production caller and
fault controls compile to a unit type outside tests. `Published::abort`
unlinks through the parent descriptor held since publication, bound to the
published receipt (#731). `probe::qualify` returns a typed `Unsupported` for
rename flags, no-follow chmod, `sun_path` budget (staged path included) and a
parent ACL; it probes with absent names and mutates nothing. Android and the
BSDs are refused by platform without calling any syscall; Windows has no
module. No kernel floor is claimed: Linux qualifies when the calls work.

## Slice 3b (behavior-preserving): lifecycle owner

`SocketOwnership` is now backed by `staged_socket::owner::Owner` over
`Lifecycle`, still on the legacy bind. Signal exit, protocol shutdown and
SIGUSR1 all go through it: terminal first, then `unlink_if_matches` on the
current receipt. A rebind after shutdown began is refused and its node removed;
a refused replacement listener is never installed. Lock order is `SocketLock`,
then the lifecycle lock; nothing nests.

## Slice 3c (activating): `serve_inner` uses staged publication

- Choice, per start (`daemon/publication.rs`): `REMUDA_SOCKET_PUBLICATION=legacy`
  is an operator setting, not a test hook, and wins. Otherwise `probe::qualify`
  decides; a refusal, or an open-time `Unsupported`/`CrossesDevices` error,
  falls back silently to the legacy bind. The reason is logged on stderr and
  returned by `Request::SocketPublication` (`staged` or `legacy: <reason>`).
  `remuda butler doctor` lives outside this repo and can print that value.
- Order: `SocketLock`, live-peer probe, `catch_signals`, stale-socket removal
  by receipt (sockets only; anything else at the path is refused and kept),
  staged open, `Owner::from_published`. The lock is held for process life.
- Closes SEC-674 MUST 1 (endpoint exposed only after its receipt exists: the
  bound/listening windows are private, publication is an exclusive rename) and
  MUST 2 (pre-rename failures clean only receipted staging artifacts and report
  residue; later failure, signals, shutdown and SIGUSR1 all end in
  `Owner::terminate`: terminal first, then `unlink_if_matches`).
- SIGUSR1 (SEC-734 SHOULD, SEC-736 MUST): any `Owner::refresh` failure drops
  the rebound listener and keeps the previous one. `refresh` receipts the
  candidate node first (two attempts), and on any later failure unlinks it by
  that receipt. A candidate that cannot be receipted is not provably ours: it
  stays, the error names it as residue, and it is never adopted. No rebound
  listener is installed without cleanup authority for its node.
- Still advisory: same-UID processes can race any path-based step, and default
  ACLs are only detected on the socket directory, not claimed isolated.
