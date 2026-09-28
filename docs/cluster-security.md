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
separately from their JSON payload. Registry pages contain at most 32 entries;
updates and registry files remain capped at 1 MiB and 1024 entries. The listener
limits replication to 60 requests per minute per authenticated peer, with a
burst of 10. A four worker pool keeps network work off the daemon thread and
coalesces queued work to one latest operation per peer.

Endpoints are optional routing hints. Only a node may change its own endpoint;
until an endpoint is known, that peer is skipped. A peer tombstone for the
receiver's own key is dropped and logged. Tombstones merge monotonically and
win over admissions regardless of version. Entries attributed to a key that
the receiver has revoked are refused; entries already admitted before that
key was revoked are retained without cascading revocation.

These checks limit what one update can change, but cluster members do not
carry separate origin signatures. A compromised admitted member can lie about
the `by` attribution on a relayed admission or revocation; each receiver applies
revocation when it receives it, so propagation can lag between nodes. Noise
authentication proves possession of the pinned static key, not that the
member's operator or machine remains uncompromised.

On Windows, `cluster init` and `cluster` status refuse to access identity
storage until owner-only ACL hardening is implemented. Track that work in
warmblood-kr/remuda#214. Windows has no identity-storage ACL hardening in PR6.
