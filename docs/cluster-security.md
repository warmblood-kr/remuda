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
account, root, or a process that can modify the user's state directory.

## Registry replication

Registry replication runs over Noise IK with the sender's static public key
pinned in the receiver's current registry. A request is accepted only from an
admitted member, and updates carry the authenticated sender fingerprint
separately from their JSON payload. Registry pages contain at most 32 entries
and 48 KiB; registry files remain capped at 1 MiB and 1024 entries. Registry
entry and update envelopes carry format major/minor versions. Unknown optional
entry fields are preserved, relayed, and included in the digest, with a cap of
8 fields and 1 KiB of canonical JSON per entry. Unknown major versions are
rejected with a typed format error. Invalid entries are dropped individually
and counted; malformed pages are rejected. A replication operation has a 90
second deadline, allowing a full 1024-entry push and fetch under the peer rate
limit. The listener limits replication to 60 requests per minute per
authenticated peer, with a burst of 10. A four worker pool keeps network work
off the daemon thread and coalesces queued work to one latest operation per
peer.

When same-version, same-origin variants conflict, merge keeps one whole
optional-field map (preferring more fields, then the lexicographically larger
canonical JSON map); it never unions maps past the per-entry cap. If a
revocation would exceed the aggregate registry byte cap, optional fields and
receiver-local `delivered_by` metadata are cleared as needed to persist the
tombstone.

Endpoints are optional routing hints. Only a node may change its own endpoint;
until an endpoint is known, that peer is skipped. A peer tombstone for the
receiver's own key is dropped and logged. Tombstones merge monotonically and
win over admissions regardless of version. Revoking an admitting member does
not cascade to the entries it previously admitted.

Any single admitted member can admit new keys and revoke any member
cluster-wide. The `by` field records the claimed original admitting or revoking
member and is never rewritten during relay. The receiver cannot verify this
attribution independently: it trusts `by`, which is not signed, and an admitted
member can attribute an entry to another admitted member. New or changed
entries are accepted only when their claimed origin is currently admitted;
byte-identical entries already held are retained after that origin is revoked.
The receiver records `delivered_by` locally for audit; it is omitted from
replication and the registry digest. Revisit this trust model if clusters span
trust domains. See issue [#282](https://github.com/warmblood-kr/remuda/issues/282)
for the signed admissions migration. Each receiver applies revocation when it
receives it, so propagation can lag between nodes. Noise authentication proves
possession of the pinned static key, not that the member's operator or machine
remains uncompromised.

After a successful join, the new node fetches the pinned issuer's complete registry snapshot
over the authenticated Noise channel and imports it. Re-running `cluster join` is a local
operator action and can apply a fresh bootstrap snapshot again. The issuer is trusted
for this initial view, including entries whose original admitter is not yet in
the joiner's registry. The import still enforces page, entry, optional-field,
and aggregate registry byte caps; after it completes, ordinary origin checks
apply to all replication updates. This trust follows the join decision: an
admitted member already has authority to admit and revoke cluster-wide.

Registry changes are pushed to reachable peers. Short-lived CLI mutations make
a bounded push attempt for up to five seconds and report each peer's result;
unreachable peers do not make the local mutation fail. Each daemon also pulls
from configured admitted peers at startup and about once per minute with
per-process jitter, so offline peers can catch up without a restart.

On Windows the private state lives in `%LOCALAPPDATA%\remuda\cluster`. That
directory and its files are owner-only and protected from inherited access.
The base `%LOCALAPPDATA%\remuda` directory is checked, never changed.

On Windows the cluster refuses, changing nothing, when an account other than
you, SYSTEM or Administrators owns `%LOCALAPPDATA%\remuda` or may write to it.
A managed machine that adds a write entry for a domain group on profile
directories is refused for that reason; the message names the SID.

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

## Response chunking (v2)

A Noise message is capped at 65,535 bytes, so a v1 response (one handshake
message, 65,487 B payload) cannot carry a large screen snapshot. A requester
that sends exactly one `X-Remuda-Chunked: 1` header (any other value, or a
repeated header, means v1) gets a v2 body in the same HTTP response:

```
body   = [u16 BE len][msg2] ( [u16 BE len][Noise transport record] )*
msg2   plaintext = [0x01 ver][u32 BE total_len][chunk0]
record plaintext = data
```

- The handshake and prologue (`remuda-cluster-v1`) are unchanged. After msg2
  both ends switch to transport mode; the record nonce is an implicit counter
  (0, 1, ...), so a reordered, duplicated, dropped, or spliced record fails
  AEAD. Any error rejects the whole response; there is no partial output.
- `total_len` is authenticated (inside msg2) and is the completeness proof:
  the response is complete exactly when the plaintext bytes received equal
  `total_len`. There is no FINAL flag. A clean cut at a record boundary, or
  trailing bytes, is an error.
- Limits: 4 MiB total, 65,519 B per record plaintext, 65 records. The client
  checks `total_len` against the cap before reading further records and bounds
  the HTTP body accordingly; a v1 HTTP body is capped at 65,535 B and its
  Noise plaintext at 65,487 B.
- Compatibility: no header gets v1. An oversized v1 reply is the error
  "cluster response exceeds the Noise frame limit; upgrade the requesting node
  to read it", so a mixed-version pair fails with a clear message, not a wrong
  payload. A new client against an old listener gets v1 (the header is
  ignored) and detects it because v2 msg2 starts with `0x01`, never JSON. An
  unknown version byte is `BadResponse` ("peer uses newer cluster response
  format").
- Resources: a v2 response larger than one frame needs a large-response
  permit (4 global, 1 per peer). When none is free the peer receives a
  `cluster busy` error and retries. Large writes have an absolute 30 s
  deadline, enforced by a watchdog that shuts the socket down, since a
  per-wait send timeout alone can restart on every partial write.
- Memory: the server appends records into the final wire buffer with one
  65,535-byte scratch buffer. Its permit is acquired after dispatch builds the
  serialized payload, so it does not bound payload construction (#595). The
  client reassembles into one capped 4 MiB plaintext buffer, plus one record
  scratch buffer; JSON deserialization can add its own allocations. Neither
  side promises a process-wide memory budget for concurrent callers.
- Requests are not chunked: the listener still caps a request body at 65,535
  bytes (the request-cap inconsistency is tracked in #592).
- Known limits: the built payload is bounded only after it is serialized,
  about 256 MiB worst case (#595); remaining low-severity hardening is in #596.
- The blank-tail trim (#591) stays as a size reducer: v1 peers still hit the
  65,487 B payload cap, and v2 responses over 4 MiB are trimmed before being refused.

## Threat model

An admitted peer can submit bounded Input batches to any session while remote
control is enabled. The listener enforces a 256 KiB/s peer-wide aggregate
budget, and the local daemon enforces its per-session cap. PTY writes have a
bounded timeout. The local flag defaults to enabled because joining a cluster
is a trust decision; disabling it refuses remote Input and Close. A compromised
admitted node can type into and close every session while the setting is on.

These controls do not protect against a compromised local account or host,
which can read the local cluster keys and change the setting.

Remote keys mode adds no wire operation: encoded key events and bracketed
paste bytes use the existing authenticated `Input` request, allowlist,
`allow_remote_control` check, per-peer aggregate limiter, and per-session
budget. Embedded `ESC [ 200 ~` and `ESC [ 201 ~` paste markers are stripped.
The UI adds one bracketed-paste marker pair only when the remote app has enabled
mode 2004; otherwise the filtered text, including LF and CR, is sent raw as it
would be in a local terminal, where each newline may submit a line. The shared
paste filter drops control characters except TAB, LF, and CR.

Auto listener mode selects the default-route RFC 1918 address and binds only
that address. It checks the detected address on the existing five-second
listener observer tick and on reload, closing and rebinding when the IP
changes. If no eligible private address is available, the listener stays off.
A public IP or wildcard bind requires an explicit bind and
`--allow-public`. This keeps the accepted pre-auth availability residual (#341)
on the selected LAN or VPN interface: an attacker with about 64 distinct
source prefixes plus a member whose pre-auth read exceeds 250 ms can still
evict that member, which retries. IPv4 prefixes are /32, so an exposed public
listener would make that residual internet-reachable; auto mode never binds
the wildcard. Joins still require the one-time token described in #332.
