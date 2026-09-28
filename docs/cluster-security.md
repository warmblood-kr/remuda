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
