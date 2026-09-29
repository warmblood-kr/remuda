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

On Windows, `cluster init` and `cluster` status refuse to access identity
storage until owner-only ACL hardening is implemented. Track that work in
warmblood-kr/remuda#214. Windows has no identity-storage ACL hardening in PR6.
