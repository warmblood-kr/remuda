# Experimental standalone host (#713)

The opt-in `remuda-session-host` binary is a macOS/Linux prototype. The normal
Remuda daemon/CLI/UI/cluster launch path does not call it. Other platforms refuse
before spawning a child. Windows, registry/Butler recovery, high-level adapter and
mixed-version rollout remain required before feature delivery. Existing live
sessions remain owned by their original daemon; this binary cannot rescue them.

## Ownership and launch

`run PRIVATE_DIRECTORY` reads one bounded length-prefixed Launch record from
bootstrap stdin (name, argv, cwd, declared agent kind, geometry, explicit env).
No inherited env is copied into the agent. A fresh private endpoint is bound,
one Owned Session is staged, and a private versioned manifest is atomically
committed before a startup ACK. Failed precommit setup terminates the staged
child. Lost or stalled ACK delivery does not destroy or block a committed host:
one bounded ACK task holds no Session/parser/inventory lock. The manifest is
recovery intent for this explicit root, not a global transactional inventory.
Retry at the same root refuses instead of launching a duplicate.

Host runs in its own process session. A separate controller may be SIGKILLed
without closing PTY/parser/child/input ownership. Session+instance+host IDs are
unchanged across reconnection. The host reports its actual build version;
new executable on disk does not update an already-running host.

## Private RPC contract

Kernel UID verification and Noise precede Open. Open negotiates protocol/caps and
binds Control to the expected immutable host/session/instance loaded from trusted
private intent, not echoed from the remote server. A View connection cannot mutate
through that connection. Both roles currently possess the same full capability:
this is API intent, not separate read-only credential authorization. Same-UID
code is outside the isolation boundary.

At most16 accepted preauth+authenticated workers are admitted; counting precedes
thread/crypto dispatch. Failed handshakes and dropped clients release permits.
One admitted input act runs outside connection/parser locks. Accepted/admitted is
distinct from a retained completed delivery/submission outcome. Disconnect puts
the controller into Draining until full act/follow-up settles. Unknown writes are
never replayed automatically. Resize/close use the same mutation reservation;
stop requires quiescence, confirmed direct-child exit and ended output source.

Current RPC includes inspect, acquire/release, whole input/recover, resize/close,
paged current-screen snapshot/ACK and stop. This is not the complete attachment/
history API or daemon Session backend seam. Server cannot access Owned.session
as an unfenced RPC escape hatch.

## Snapshot and lifecycle

One immutable snapshot allocation per host (32MiB cap) has global monotonic ID,
terminal generation and geometry from one parser cut. Pages are12KiB before
base64. At most one snapshot per connection; explicit matching ID+generation ACK
requires every page sent. Sent pages alone are not baseline ACK. TTL/disconnect
invalidates the pending cut; later requests must capture fresh state. Client
buffers/validates all pages and ACK before exposing the assembled screen, never
paints an arbitrary stream suffix after loss. Slow socket writes hold no PTY or
parser lock. Listener readiness uses poll instead of a fixed5ms idle accept loop.

Direct-child wait, descendant lifetime and PTY end are separate facts. Output end
is Open/Eof/Hangup/ReadFailure with a retained drained generation. Native PTYs can
hang up while a descendant remains alive; closing stdio need not immediately end
a native PTY. Only the sole waiter establishes process exit. A read failure is
unavailable health, never invented exit zero. Direct-child close is not arbitrary
Unix descendant containment. A stopped host currently loses in-memory history/
outcomes; durable exit/tombstones belong to the pending inventory integration.

## Evidence and remaining gates

Synthetic separate-process tests cover controller SIGKILL after admission and
after the first effect during a feed pause, same IDs/PID/color/paste state,
completed-outcome retry without duplicate computed effects, resize, explicit
close/stop, lost startup ACK, same-root duplicate refusal, View/wrong-instance
refusal, complete-vs-incomplete snapshot ACK, full preauth cap with continuing
PTY generations, descendant/terminal-end truth and closed-stdio liveness.

Independent exact-head review/CI remains required. Additional gates: real
cross-UID and startup/thread-failure fixtures, full snapshot expiry/overflow soak,
parser/aggregate RSS+CPU/fairness measurements, Windows lifecycle/authentication,
full logical attach/pending operations, authoritative registry/owner metadata and
companion Butler principal/mail/Matrix recovery. No live rollout, automatic updater,
host replacement or zero-loss/exactly-once application-execution claim follows.
