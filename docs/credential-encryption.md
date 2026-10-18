# Credential encryption at rest

Issue: warmblood-kr/remuda#746. Remuda's cluster identity (the node's Noise
static private key) is encrypted on disk under a random per-node key. The
daemon reads both files directly, so no read ever prompts, and the design is
the same on macOS, Linux, and Windows: there is no OS keystore dependency.

## Files

| Path (under the state directory `remuda/`) | Holds | Permissions |
| --- | --- | --- |
| `keys/node.key` | the node key | owner-only file in an owner-only directory |
| `cluster/identity.key` | the sealed identity | owner-only file, as before |

The state directory is `$XDG_STATE_HOME/remuda` (default
`~/.local/state/remuda`) on Unix and `%LOCALAPPDATA%\remuda` on Windows.
Owner-only is mode 0700/0600 on Unix and the owner-only ACL of #214/#744 on
Windows; both are checked on every open, and symlinks are refused, exactly
like the other cluster state files.

The node key lives in its own `keys/` directory, beside `cluster/` and never
inside it, so a copy of the cluster state carries no usable secret.

## Node key

- Format: 40 bytes, `RMDNKEY1` then 32 bytes from the OS CSPRNG (`getrandom`).
  The key is random, so no key derivation is needed.
- Created at setup: on the first identity write (`cluster init`, `cluster
  join`), or by the migration below. Creation happens under the cluster
  state lock, so two writers never race. An existing key is never replaced.
- Its id is the first 8 bytes of `SHA-256("remuda node key id v1\0" || key)`.
  The id is not secret; it tells "wrong key" apart from "tampered".

## Sealed file layout

```
offset  size  field
0       8     magic "RMDSEAL1" (format version 1)
8       8     node key id
16      24    nonce
40      n+16  XChaCha20-Poly1305 ciphertext and tag
```

The associated data is the 40-byte header followed by a slot label
(`cluster/identity.key`), so the header cannot be edited and a sealed file
cannot be moved into another credential's slot. A sealed identity is 120
bytes. Decryption either authenticates or fails; unverified bytes are never
returned.

## Nonces

Every write draws a fresh 24-byte random nonce. XChaCha20's 192-bit nonce
makes random nonces safe: a node writes its identity a handful of times in
its life, far below any collision concern. There is no counter to persist
or roll back.

## Reads and errors

- Missing node key with a sealed identity: `InvalidData`, naming both paths
  and the recovery (restore the key, or remove `cluster/` and run `remuda
  cluster init`). It is never `NotFound`, so no caller mistakes it for an
  uninitialized node and mints a new identity over the old one.
- Different node key (the id differs): "encrypted with a different node key".
- Any modified byte: "failed authentication: it was modified or corrupted".

## Migration of plaintext credentials

A node written before #746 has a 64-byte plaintext `identity.key`.

1. On every daemon start, and on `cluster init`, Remuda takes the cluster
   lock and checks `identity.key`. If it is plaintext, Remuda creates the node
   key if needed, seals the same 64 bytes, and atomically replaces the file.
   The rename removes the plaintext file; no temporary or backup copy is
   left. Remuda then reads the file back through the sealed path before it
   reports success.
2. The report is one line per migrated file in the daemon log (and on stderr
   for `cluster init`): `encrypted <path> at rest with node key <path>; the
   plaintext copy was removed`. No secret is printed.
3. The identity and its fingerprint are unchanged, so cluster membership is
   unaffected. Repeat runs are no-ops.
4. Until the migration has run, reads still accept the plaintext form, so a
   node whose migration failed (for example, the lock was busy) keeps working
   and retries at the next start. Every write is sealed.

Limit: replacing a file does not erase old disk blocks (SSD wear leveling,
copy-on-write filesystems, backups, snapshots). If a plaintext identity may
already have been copied, rotate it with `remuda cluster init --new-identity`.

## What this protects, and what it does not

- Protects: a copy of the state directory, a backup, or a synced folder that
  does not include `keys/node.key` reveals no secret.
- Does not protect: a copy that includes `keys/` (keep it out of backups and
  syncs), the same user account, root, or malware running as the user. The
  node key is on the same machine by design, so the unattended daemon can
  read it.
- Later option (not built): supply the node key from an external source at
  daemon start, for operators who want stronger separation.

## Out of scope

- `remuda.system.credential` (Lua) keeps using the OS store where one exists;
  sealing those secrets under the node key is a follow-up.
- Butler's Matrix credential files live in the remuda-butler repository.
- The cluster file store (#742) can reuse this sealing once it holds secrets.

## Tests

- Unit (`native/src/cluster/sealed.rs`, `identity.rs`): round trip; a fresh
  nonce each write; any flipped byte, a truncated file, a wrong slot label,
  and another node's key are each refused; node key permissions and
  stability; a missing node key is not `NotFound`; migration seals once,
  keeps the fingerprint, and leaves no plaintext copy.
- Integration (`native/tests/credential_encryption.rs`): a state directory
  copied without `keys/` contains the private key in no form (raw, hex,
  base64), and the copied node refuses to read or replace its identity; a
  daemon start migrates a plaintext identity once and reports it.
