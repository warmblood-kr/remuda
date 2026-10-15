# 047 — reserve identity and publish from the stable host

## Before

The standalone host creates its Session identity after spawning, and only the
controller writer can register inventory observations. Controller death during
birth lacks a scoped self-publication path; replaying a launch after lost ACK
could create a second child. Child exit alone also cannot authorize dropping
unconsumed durable input outcomes.

## Desired outcome

Add an explicit experimental reserved bootstrap. A caller first commits the
exact identity/launch payload/hash, unique endpoint and scoped publisher lease.
The host verifies that reservation, claims the endpoint before any child effect,
preserves the preassigned core instance, and publishes its own readiness before
launch ACK. Keep daemon creation/adoption and live rollout disabled.

## Contract and rationale

Core StartIdentity validates bounded IDs without claiming uniqueness/durability;
the Native store establishes those. Legacy constructors still generate instances.
Reserved spawn injects the exact public session ID/name metadata and preserves
the supplied instance across snapshots, observations and input instance checks.

The lease binds store incarnation, name, immutable host/session/instance, launch
ID and nonce. Bootstrap additionally matches the exact raw launch hash and
endpoint, refuses unsupported required ownership semantics, validates launch and
pre-encodes the bounded manifest before effects. Exclusive endpoint creation
precedes PTY spawn, so racing bootstraps have one winner. Never delete/rebind a
root or infer permission to relaunch from timeout/missing ACK. Failed/ambiguous
birth can remain explicitly unavailable; no automatic root repair is introduced.

Store.publish is separate from the global writer API and returns Revision, not
Fence. Its closed self-publications cover Ready, confirmed direct-child exit and
terminal end. A new controller epoch does not revoke the live host's lease.
Idempotent publication reconciles uncertain replace; wrong bindings/ref revisions
refuse. Additional required schema capability fails closed for older readers;
legacy entries omit the new optional field. Future lifecycle bytes are reserved
in every projected-budget validation, including decode and near-full stores.

Reserve the essential publisher worker before creating the child. After spawn,
the host/server/parser remain alive through publication or ACK failure. One
bounded publication context retries with capped backoff; one independent bounded
ACK task cannot block later lifecycle publication or PTY drain. Pending/faulted
publication refuses control effects while inspection remains available. Stop
requires confirmed exit/EOF publication and an exact same-store/lease read proving
no pending or unconsumed outcomes. Unknown storage refuses Stop. The future
controller must stop while the retained entry still exists before retiring that
entry; this increment does not add an automatic retirement/cleanup caller.

## Expected

Kill actual private launcher parents at endpoint/pre-PTY, post-PTY/pre-manifest,
manifest/pre-Ready and Ready/pre-ACK cuts; release the controlled host and recover
one child with the same identity/PID. A second writer epoch must not prevent
host Ready. Before-spawn death recovers the original reservation. Two bootstraps
and a lost ACK never duplicate the child. Unknown payload/lease/store/schema
refuses before endpoint/child; output drains while effects await Ready. Host Stop
retains an unconsumed result until the exact consumer checkpoint/ACK commits.

## Actual

The seven private bootstrap process tests pass, including actual launcher parent
SIGKILL and authenticated exact-identity cleanup. They launch the normal reserved
runtime through a unit-test executable helper; they do not exercise a production
daemon launch selector. The existing standalone executable process suite also
passes. Three core identity tests, two Native identity tests and five publication
model/disk tests cover constructor parity, wrong authority, independent exit/EOF,
near-full progress and uncertain publication cuts.

```text
cargo test -p remuda-native session_host::birth --lib --offline
7 passed; 0 failed; 1 ignored
cargo test -p remuda-native session_host --lib --offline
84 passed; 0 failed; 3 ignored
cargo test -p remuda-native --test session_host_process --offline
7 passed; 0 failed; 0 ignored
cargo test -p remuda-core --offline
133 passed; 0 failed; 2 compile-fail doctests passed
cargo clippy --workspace --all-targets --offline -- -D warnings
Finished `dev` profile
```

The ignored fixtures run only with their private parent context. Windows pure
model/storage code and tests cross-typecheck; genuine matching Windows CI remains
required. The reserved ConPTY/Windows IPC runtime is still unsupported before
spawn. Same-UID arbitrary code is outside the isolation boundary. These tests
prove individual launcher-process death, not service/container/cgroup or external
Windows Job termination: the reviewed rollout must keep the stable owner outside
the control plane's broader kill scope. Direct-child wait/EOF is not arbitrary
descendant containment. No power-loss, application-consumption or exactly-once
guarantee across a lost host ledger is added. No production restart, live pane,
real Claude/Codex, legacy PID rescue or live migration was used. Refs #713.
