# PR #297 security merge audit

This repairs the merge in the cluster transport files. The comparison uses the main-side parent `6bd3c0d` (the main snapshot merged by PR #297) and feature-side parent `5790b96`. The current branch also contains later main history. Full test-name inventories are in [`sec-297-test-inventory/`](sec-297-test-inventory/).

## Per-file test-name diff

Counts below include unit and integration tests in the six files that carried tests relevant to the dropped merge delta. The inventory files contain every test name from each source revision.

| File | Main `6bd3c0d` | Feature `5790b96` | Candidate | Main names missing | Feature names missing |
| --- | ---: | ---: | ---: | --- | --- |
| `native/src/daemon.rs` | 18 | 9 | 19 | none | none |
| `native/src/net/listener.rs` | 38 | 31 | 44 | none | `remote_input_is_refused_until_pty_timeout_merges` (obsolete; #252's PTY write-timeout protection has merged, and Input is now gated by local control plus a per-peer rate limiter) |
| `native/src/net/frame.rs` | 4 | 3 | 4 | none | none |
| `native/src/net/cluster_client.rs` | 1 | 0 | 1 | none | none |
| `native/tests/cluster_client.rs` | 13 | 8 | 13 | none | none |
| `native/tests/cluster_frame.rs` | 3 | 4 | 4 | none | none |
| **Total** | **77** | **55** | **85** | **none** | **one intentional obsolete test** |

The candidate also adds `native/src/net/listener.rs::cluster_registry_verbs_are_admitted_while_input_remains_control_gated`.

Inventory files:

- [`main-6bd3c0d.txt`](sec-297-test-inventory/main-6bd3c0d.txt)
- [`feature-5790b96.txt`](sec-297-test-inventory/feature-5790b96.txt)
- [`candidate-head.txt`](sec-297-test-inventory/candidate-head.txt)

## Merge contents

- `net/listener.rs`: retains join admission, registry replication dispatch and revision observation, the #274 `ControlSource` gate for Input and Close, and the 256 KiB/s per-peer Input limiter with at most 1,024 tracked peers. ClusterRegistrySync and ClusterRegistryUpdate are decoded under the same frame-size cap and admitted after the member authorization check; they bypass only the local remote-front allowlist, which intentionally refuses cluster-protocol verbs. The new listener test covers this boundary and confirms Input remains control-gated.
- `net/frame.rs`: retains low-order DH checks for request keys and rejects low-order responder ephemeral keys in Noise message 2.
- `net/cluster_client.rs`: checks the pinned static key before connecting and bounds HTTP body parsing.
- `daemon.rs` and `tests/cluster_client.rs`: restore the main-side concurrency, retry, attach-input, and client security tests.

## Gates

Final gates passed:

- `cargo fmt --check`
- `git diff --check`
- `cargo clippy --workspace --all-targets`
- `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets`
- `env TMPDIR=/private/tmp/pr95-test-tmp cargo test --workspace` (all passed; project-ignored tests remain ignored)
