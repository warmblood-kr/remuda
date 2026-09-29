# PR9 merge review pre-stage

Prepared from committed `feat/cluster-join` tip `e7676f4` against main `ed8869c` (`ed8869c..e7676f4`: 19 files, +3,885/-37). The feature checkout currently has uncommitted edits in `native/src/bin/remuda.rs`, `native/src/cluster.rs`, `native/src/cluster/registry.rs`, `native/src/cluster/replication.rs`, `native/src/daemon.rs`, `native/src/net/listener.rs`, and `native/tests/cluster_revoke_live.rs`; leave those edits intact and review the resolved merge once m2-write reports.

## Join invariants to verify in the resolution

- [ ] **Full-key pin before connect:** the shown/selected issuer fingerprint must match the invitation's issuer key fingerprint, and the full static public key from the invitation must be used for Noise authentication. Confirm mismatch is rejected before any socket connection or join request; do not trust the fingerprint alone as a substitute for pinning the full key.
- [ ] **Join-only for unknown keys:** an unknown static key may reach only the strict Join request path. All non-Join requests from unknown keys must be rejected before normal daemon dispatch; malformed Join payloads, wrong tokens, and rate-limited attempts must not reach daemon commands.
- [ ] **Atomic token consumption and admission:** token removal must be persisted while holding the cluster state lock before admission; failed admission restores the token before releasing the lock. Successful admission must persist exactly one registry entry and must not permit replay or leave token consumed with no corresponding admission after recoverable refusal.
- [ ] **Endpoint field end to end:** the optional endpoint in the invitation/request must validate as canonical unicast `SocketAddr`, persist in the admitted registry entry, and remain compatible with older registry entries lacking the field. Verify only the node's owner can update an endpoint during registry merges/sync; a relayer cannot override it.
- [ ] **Low-order key rejection:** reject low-order static identity keys during identity/registry validation and reject low-order ephemeral/static DH results during Noise frame open, before request dispatch or admission. Cover malformed encodings and ensure low-order remote peers cannot trigger join or regular command handling.
- [ ] **#274 remote-control gate:** merge the `allow_remote_control` setting gate from main into the cluster Input dispatch path. Confirm remote Input is denied when disabled while local input remains available, with `RemoteControlDisabled` returned through the established protocol response and no input applied.
- [ ] **PR9.5 registry sync collision:** reconcile registry/replication changes in `native/src/cluster.rs`, `native/src/cluster/registry.rs`, `native/src/net/listener.rs`, and `native/tests/cluster_revoke_live.rs`. Preserve admission origin and revocation/tombstone precedence, owner-only endpoint updates, bounded sync/page handling, and immediate propagation after Join/revoke without lost updates or duplicate/conflicting state transitions.

## Fast review focus

Main/PR9 overlap centers on `native/src/net/listener.rs` (Join-only routing plus #274's remote-control gate), `native/src/cluster.rs` and `native/src/cluster/registry.rs` (admission, endpoint, and PR9.5 merge policy), and `native/tests/cluster_revoke_live.rs` (live sync/revoke coverage). Recheck tests for both the join lifecycle and remote-control behavior after the resolution; inspect the final merged diff for dropped changes and conflict-marker remnants before issuing a verdict.

## Partial review of pushed merge `55d0bbf`

- The merge commit has feature parent `e7676f4` and main parent `3d8b460`; the feature diff against main touches 14 files, including all join-specific modules and live revocation coverage. `git diff --check 3d8b460..55d0bbf` is clean.
- Static spot-check confirms the final listener retains the unknown-peer Join-only route and the `allow_remote_control` Input gate (`RemoteControlDisabled`); the tree includes a socket-level test asserting the disabled response does not dispatch Input.
- **Not complete:** I did not finish the full-key/connect ordering, persisted token rollback and registry atomicity, endpoint ownership through registry sync, low-order rejection, or PR9.5 registry-sync/revocation merge review, and did not run PR9 tests. Per owner direction, stop here and resume after restart.
- This is a partial status only; no PASS/FAIL verdict on PR9.
