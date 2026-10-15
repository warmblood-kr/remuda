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
