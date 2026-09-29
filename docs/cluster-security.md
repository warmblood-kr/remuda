# Cluster identity storage

PR6 stores the node's static Noise key in the local cluster state directory. On
Unix, the cluster directory is created with mode 0700, and identity and
registry files are created atomically with mode 0600. Loads use no-follow file
opens and validate ownership and permissions on the opened handle. Identity
private-key buffers are zeroized after use. The registry binds each fingerprint
to its public key and rejects mismatches; merges key membership by the public
key so a revoked key cannot be admitted under another fingerprint.
Initialization holds one exclusive state lock across key initialization and
registry load, merge, and save.

Ancestor directories above the cluster directory are created with the
platform's default permissions; only the cluster directory itself is forced
to mode 0700.

This protects stored identity material from other local users under the normal
Unix ownership model. It does not protect against compromise of the same user
account, root, or a process that can modify the user's state directory. PR6
does not listen on the network or replicate registry updates.

On Windows, `cluster init` and `cluster` status refuse to access identity
storage until owner-only ACL hardening is implemented. Track that work in
warmblood-kr/remuda#214. Windows has no identity-storage ACL hardening in PR6.

## Remote control

Each node stores its local `allow_remote_control` setting in the private
`$XDG_STATE_HOME/remuda/cluster/settings.json` file (or
`$HOME/.local/state/remuda/cluster/settings.json` when `XDG_STATE_HOME` is
unset). `remuda cluster control on|off` changes only this node; the setting is
never replicated. A missing setting defaults to enabled,
which matches the approved all-viewers-may-write cluster behavior. While
enabled, admitted nodes may send input, with a 256 KiB/s aggregate limit per
peer across sessions and client IDs in addition to the existing per-session
limit. `remuda cluster` status shows whether remote control is enabled and
warns that a compromised admitted node can type into and close every session.

Remote Input uses only the idempotent client ID and sequence batch path. The
remote front accepts at most 12 KiB (12,288 bytes) in one batch; remote
senders split larger input into sequential batches at this limit. This keeps
the JSON byte-array encoding below the listener body cap, including batches of
three-digit byte values. An oversized batch receives a typed error before
dispatch. The peer aggregate budget is charged before dispatch; the local daemon
also enforces its per-session byte budget. A disabled node returns the typed
`RemoteControlDisabled` response for both remote Input and remote Close. A
remote Close is accepted only with the current session `instance_id` and
`confirm: true`; a missing confirmation or stale instance is refused. Turning
remote control off refuses both remote Input and confirmed remote Close.

A batch already in dispatch may still be delivered if the setting changes to
off while that batch is in flight. A revoke during dispatch may likewise allow
the Input write before the listener returns an Error; clients should treat that
outcome as possibly delivered. The fixed one-second budget window can allow up
to 2×256 KiB across a window boundary. Identical retries are charged again at
the listener, while the daemon deduplicates them to prevent repeated writes.

## Threat model

An admitted peer can submit bounded Input batches to any session while remote
control is enabled. The listener enforces a 256 KiB/s peer-wide aggregate
budget, and the local daemon enforces its per-session cap. PTY writes have a
bounded timeout. The local flag defaults to enabled because joining a cluster
is a trust decision; disabling it refuses remote Input and Close. A compromised
admitted node can type into and close every session while the setting is on.

These controls do not protect against a compromised local account or host,
which can read the local cluster keys and change the setting.
