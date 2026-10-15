# 042 — prepare Windows creation-time containment and checked wait

## Before

The standalone host refuses Windows. Existing portable-pty0.9.0 starts ConPTY
children before the daemon assigns a Job, and its wait/kill implementations
cannot establish checked host exit semantics. No public creation-time hook exists.

## Desired outcome

A narrowly pinned optional vendor hook supplies JOB_LIST alongside PSEUDOCONSOLE
at CreateProcessW, reusing quoting/env/cwd and leaving ordinary spawn unchanged.
An inactive experimental wrapper retains the original process handle, verifies
Job membership and distinguishes checked wait/pending/error from exit code259.
Windows host launch remains disabled; caller Job/drain ownership is explicit.

## Expected

Genuine Windows fixtures must verify membership before side effects or immediate
grandchild creation, Unicode/empty/quoted arguments and cwd/env, 2MiB independent
output drain, actual exit259, Job last-close killing only contained descendants,
checked direct termination and rejection of a valid non-Job handle. Injected
wait/query/terminate errors cannot invent exit or success. Unix host proofs and
lint remain green. Native Windows CI is required; cross compilation is narrower.

## Actual

The optional hook, wrapper, fault-injection tests and two native process fixtures
type-check for x86_64-pc-windows-gnu through a scratch minimal harness that includes
the same source and Windows bindings. Windows-target clippy with warnings/size/
complexity denied also passes. This macOS author cannot execute Windows binaries;
native CI execution is still required before this increment can be accepted.

```text
Checking portable-pty v0.9.0 (vendor/portable-pty)
Checking remuda-windows-typecheck (scratch harness)
Finished `dev` profile

cargo test -p remuda-native --lib session_host
test result: ok. 38 passed; 0 failed

cargo clippy --workspace --all-targets -- -D warnings
Finished `dev` profile
```

Native Windows reproduction uses cargo test -p remuda-native --lib
session_host::windows_process and cargo test -p remuda-native --test
session_host_windows. Synthetic test processes use private scratch paths and
finite lifetimes; they never contact a production daemon or real agent.

The first genuine Windows run passed all three wait/error unit tests but both
new process fixtures timed out awaiting their first marker. The synthetic drain
discarded every byte and never answered any cursor query; the upstream constructor
enables PSEUDOCONSOLE_INHERIT_CURSOR, a documented possible hang without replies.
This is a hypothesis for the timeout, not yet proved by the native rerun. The harness now
answers that initial request through a bounded independent writer and retains a
512-byte diagnostic tail plus retained-process wait state on failure. The valid
non-Job refusal fixture now uses a fresh ConPTY to avoid a previous-child state
confound. Native rerun is pending; this is not recorded as a positive result yet.

```text
native_invalid_handles_surface_wait_query_and_termination_errors ... ok
only_a_signaled_wait_allows_exit_query_and_exit259_is_final ... ok
failed_termination_requires_observed_natural_exit_not_synthetic_success ... ok
checked_direct_termination_observes_real_exit_and_invalid_job_never_runs_child ... FAILED
creation_contains_immediate_descendant_and_retains_unicode_exit259_and_drained_output ... FAILED
synthetic fixture timed out
```
