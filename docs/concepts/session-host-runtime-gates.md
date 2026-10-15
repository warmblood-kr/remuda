# Isolated runtime preparation gates (#713)

This increment remains inactive: no host binary, daemon adoption, registry rebuild,
cluster upgrade or production restart yet. A Unix HostChild backend and Owned
Session exercise lifecycle/terminal/input contracts with synthetic PTYs. Windows
ConPTY/job creation and identity checks remain required work, not a supported-path
claim. These gates remain open through the usable cross-platform host and adapter.

## Implemented preparation

HostChild retains bounded terminal/parser state, shares the existing bounded PTY
input worker, answers DSR at parser event positions and continuously drains with no
viewer. A dedicated sole reaper owns wait status independently of Session's alive
latch and of output EOF. terminate returns success only after direct-child wait is
confirmed; a wait error is unknown, never fabricated exit zero. Unix process trees
escaping the direct child/process group are not claimed contained.

Owned contains one core::Session and the input coordinator. Admission stores the
whole validated act and payload identity. A once-only ActId claim retrieves that
stored payload before any effect; duplicate/stale workers cannot execute again.
Controller release during a feed pause stays Draining until the full act and any
late worker/follow-up settle. Rich delivery/submission/clear/error outcomes are
retained within a 4 KiB serialized reply budget, counted without building an
oversized temporary buffer. Oversized replies are explicitly indeterminate.

The host terminal caps current cells at65,536, geometry at1000x500 subject to that
cell product, retained history at1000 rows/1,000,000 cells, work per parse at4096
bytes and pending DSR positions at32. Unsupported OSC strings over4096 bytes are
counted and discarded through BEL/ST/CAN/SUB. DCS/SOS/PM/APC pass through pinned
vte's non-accumulating states. vt1000.16.2/vte0.15.0 are pinned exactly because this
boundary relies on their escape state rules. Dependency changes need a new audit.
DSR queue failures/overflow are explicit degraded health, not healthy lossless I/O.
These component caps do not prove an aggregate RSS/latency bound.

OutputWakeup tracks receiver lifetime separately from pending state, so quiet
subscriber drop can reclaim a slot without emitting a false output notification.
A pending-wake/drop negative control failed before this fix; 100 quiet registration
cycles exercise bounded cleanup.

## Transport draft (not adoption approval)

Channel uses existing snow NoiseNNpsk0 with private per-host key, bounded ciphertext
and absolute nonblocking poll-based handshake/frame budgets. Framing/crypto/I/O
errors consume the connection; never resume a partially written frame or Noise
nonce stream. One-byte drip and tamper tests exercise this behavior. Empty encrypted
payloads refuse symmetrically. Encrypted payload max is65,519 bytes, not the64KiB
outer frame limit. Large snapshots require bounded identified pages and full ACK.

Unix endpoint preparation requires a new private0700 leaf, no-follow owner-only
regular capability file32 bytes/one link, private socket and trusted ancestor owner/
rename permissions. Root-owned sticky temp dirs are allowed. Both peers verify
kernel euid before Noise. Connect uses a nonblocking deadline. Same-UID renames,
credential reading/debugging and control are outside this isolation boundary.
No existing endpoint is rebound/removed automatically. CLOEXEC checks cover held
listener/directory descriptors; launch-inheritance/race fixtures remain integration
work. The supplied wrong-expected-UID test is not a genuine cross-UID process test.

Before exposure: bounded connection/handshake admission; immutable expected host+
session/role binding in Hello; complete snapshot paging/resync; all partial-write,
replay/backlog/drip and filesystem-negative fixtures; Windows endpoint/reply parity;
thread/startup failure cleanup; cancellation/reconnect soak; aggregate/fairness
measurements. Do not mistake passing preparation tests for completed transport.

## Delivery gates

Controller-process SIGKILL/reconnect with same child/Session/terminal/input outcomes;
usable attach/input/resize/close/exit; host discovery and registry/metadata continuity;
high-level Session backend seam; companion Butler proven-principal/team/mail/Matrix
recovery; N/N+1 cluster/host compatibility and rollback; independent exact-head
review and CI on macOS/Linux/Windows. No live legacy adoption or zero-loss/exactly-once
application execution claim follows from this increment.
