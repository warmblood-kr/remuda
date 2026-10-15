# Registry continuity design gate (#713)

Source audit: Remuda main 7bfbbdfd / foundations b29048f9; isolated Butler
83066abd70f608f684fe8722a4544af2105b5daf. No production state read.

Core Registry is an in-memory map by name. SessionSummary includes identity,
instance, output version, alive/idle, geometry, attachment/human idle and mouse.
It contains no topic/agent metadata. Session::new_with_id still generates a new
instance ID. A proxy must preserve host identity, not call this constructor and
assume it reconstructs an existing instance.

Daemon startup builds an empty Registry before loading config. Lua modules and
hooks are image state; session_order/session_detail enrich the UI separately.
Cluster AuthorizedNode is membership authority, not session inventory; remote
session lists query the owning node. Do not mix host liveness with membership.

Butler identity.lua97-134 loads agents.jsonl records (ULID/alias/kind/leader,
created/state/ended), then permanently ends absent non-root members with reason
daemon_restart. identity.lua182+ keeps token records only in daemon memory.
main.lua943+ rejects unproven instance bindings. Launch.lua246+ builds agent,
token, parent/children, topic/cwd/telemetry and session-start records in bus memory.
Therefore restoring process IDs or rerunning config alone cannot restore Butler
membership/caller authority. An empty transient list is destructive.

Required inventory/recovery order:

1. Start IPC in explicit Recovering status. Do not expose a successful empty list
or run spawn-on-missing config/timers/Butler reconcile until discovery completes.
2. Load bounded versioned launch reservations/owner metadata by immutable host+
session+instance identity. Authenticated host Inspect supplies process, parser,
input, geometry, output/attachment generations, host-clock durations and exit
facts. A manifest/PID is a hint, never proof of a live owned process.
3. Reconcile collisions, incompatible schemas and unavailable hosts explicitly;
keep legacy entries in their original owner namespace. Never overwrite by name
or infer dead from timeout/auth failure. Preserve unknown optional metadata or
refuse incompatible format without rewriting it.
4. Rebuild registry cache, then load owner modules through a restoration hook.
Butler must restore/revalidate proven instance binding and durable member/topic
metadata before pending actions resume. Existing advisory env/token cannot grant
new native caller identity; never serialize arbitrary Lua closures/timers/secrets.
5. Subscribe/reconcile via snapshot revision+event tail; bounded log wrap requires
new snapshot. Exit tombstones identify immutable instance+final output watermark;
a late old exit never removes a same-name replacement. Adapter drop != exit.

Proposed persistence shape for review (not implemented): one bounded local
inventory transaction file under private storage, coordinated by a short OS file
lock on a separate stable lockfile, never the replaced inventory inode. This is
a cut of committed publications, not necessarily physical lifecycle state.
Store incarnation+revision covers name/launch reservation,
host incarnation/session instance, launch facts, namespaced owner metadata and
bounded exit tombstones/events. Atomically replace/fsync one generation; do not
pretend separate JSON files commit together. Host output drain never waits for
this lock: lifecycle publication uses a bounded pending update/retry with explicit
health failure if persistence cannot commit. Launch success is acknowledged only
after the defined reservation/host-registration commit; host stays discoverable if
controller dies before receiving ACK. Registry scan and snapshot/event revision
must cover sessions created/exited while controller is absent.

A global durable index avoids pretending independent per-host directory scans
have one atomic cut. Per-host manifests remain private endpoint hints. Corrupt or
incompatible index cannot silently become an empty inventory; enter Recovering/
Unavailable and refuse spawn-on-missing. Metadata CAS/fence and identity protect
updates; secret capability files/cluster credentials are never exported in lists.
Crash consistency and power-loss guarantees must be stated separately and tested.

Acceptance: kill controller at every launch/persist/register/ACK boundary; stable
names/IDs/topics/agent/cwd and one original instance; create+exit during gap; stale,
duplicate/corrupt/oversize manifests; name reuse/delayed exit; all local CLI/UI,
remote cluster and Butler lists agree. Test recovery/module reload100x, schema
upgrade/downgrade/interruption, event wrap/tombstone bounds and secret redaction.
No live daemon restart or existing-session migration is authorized by this design.
