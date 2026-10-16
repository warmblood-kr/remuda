# 050 — prove terminal lease settlement before host Stop

## Before

The experimental host checks publication Complete and separately reads whether
its durable lease has unconsumed input. Those predicates alone do not constitute
a bound terminal-state proof for an atomic local Stop admission transition.
Human-input integration will permit independent admission and needs a precise
cross-layer contract without filesystem I/O beneath the coordinator lock.

## Desired outcome

Construct narrow settlement evidence from one validated index cut for one exact
immutable lease. Require store/name/full identity/launch/nonce, matching published
Exited generation, successful EOF/Hangup, and no pending or retained input results.
An empty Registered entry, read failure, unknown capability, corrupt cut or
replacement must fail closed. This milestone provides the proof only; runtime
Stop adoption follows in a separate reviewed increment.

## Expected

- A pending input at exit blocks proof; completing it still blocks proof until
  its exact durable consumer checkpoint retires the retained outcome.
- Existing mutations cannot resurrect input for the terminal immutable identity:
  Reserve requires Registered, scoped Ready refuses Exited, Register rejects a
  hosted birth, and completion requires an existing pending reservation.
- Metadata/controller revisions do not invalidate a monotonic terminal fact.
  Retire must follow proof construction; Retire before construction refuses.
  The allocation ledger prevents identity reuse after tombstones wrap.
- Evidence is private and non-serializable, bound to the exact lease and actual
  identity; it is not permission to stop, migrate, replay or reset anything.
- Atomic Stop must later recheck actual positive child exit/EOF, publication,
  no live human grant, no raw retention and physical quiescence, then latch all
  new admission under the host coordinator. No runtime behavior changes here.

## Actual

The six focused pure-model regressions passed using an isolated build target:

```text
cargo test --offline -p remuda-native --lib \
  session_host::inventory::model::publication::settlement_tests \
  --target-dir ../authority-target
running 6 tests
... corrupted_or_future_required_cut_cannot_construct_evidence ... ok
... valid_mutations_cannot_resurrect_retention_and_unrelated_revisions_do_not_revoke_proof ... ok
... proof_needs_one_valid_terminal_cut_not_empty_retention_or_exit_alone ... ok
... every_lease_binding_and_actual_identity_is_exact_and_nontransferable ... ok
... pending_at_exit_and_later_completion_refuse_until_exact_durable_consumption ... ok
... retire_before_proof_refuses_and_tombstone_wrap_never_recycles_old_identity ... ok
test result: ok. 6 passed; 0 failed; finished in 4.12s
```

Broader inventory regressions and workspace lint also passed:

```text
cargo test --offline -p remuda-native --lib session_host::inventory --target-dir ../authority-target
35 passed; 0 failed; 1 ignored; finished in 65.01s
cargo clippy --offline --workspace --all-targets --target-dir ../authority-target -- -D warnings
Finished dev profile in 8.98s
check-steps: 50 unique steps; check-comments: 1004 within cap
cargo fmt --all -- --check; git diff --check: passed
```

No production action, live session access, default adoption or restart. The proof
assumes valid Store mutations and excludes arbitrary rollback/reset and hostile
same-UID rewriting. It cannot rescue a daemon-owned PTY or prove application
consumption. Independent design confirmation precedes incremental source review;
Registry/Butler/input integration, Windows host runtime and replacement/rollback
trials remain separate work under issue #713.
