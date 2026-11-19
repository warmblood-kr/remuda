# 094 — Self-execing integration test daemons find the host

## Before

Step 093 anchored the host lookup at the profile's `remuda` only in `cfg(test)`
builds. Integration tests compile the library without `cfg(test)`, and several
run the daemon by executing their own test binary under
`target/<profile>/deps/`. Those daemons still resolved the host as
`deps/remuda-session-host` and failed with `session host binary not found`
(`daemon.rs`, `cluster_remote.rs`).

## Desired outcome

Apply the profile anchor whenever the daemon executable is directly under a
directory named `deps`. The existing build probe still validates the host, and
a production install never lives under `deps/`, so its path is unchanged.

## Expected

- The new layout test passes, including the "real daemon keeps its layout" case.
- The integration tests that failed with the missing host pass.
- The full lib suite has no new failure.

## Actual

```text
cargo test --locked -p remuda-native --test daemon a_hostile_session_name_reaches_the_hook_byte_for_byte
test result: ok. 1 passed; 0 failed

cargo test --locked -p remuda-native --test daemon a_human_attaches_through_a_real_terminal_and_detaches_with_ctrl_backslash
test result: ok. 1 passed; 0 failed

cargo test --locked -p remuda-native --lib session_host::host_build::tests::a_test_daemon_under_deps_uses_the_profile_remuda_for_the_host_lookup
test result: ok. 1 passed; 0 failed

cargo test --locked -p remuda-native --lib
test result: FAILED. 1933 passed; 1 failed; 7 ignored; finished in 112.04s
```

The one failure is `session_host::remote::tests::a_takeover_between_completion_and_ack_does_not_wedge_later_writes`
(`BrokenPipe`) under full-suite load; it passes alone and also failed before
this change. Five `cluster_remote` tests fail separately with
`host-backed launch refused: the session name contains a control character`,
which is the host's launch validation, not the path. Neither is folded into this
change.

Before the change, the first case failed with
`Error("session host binary not found: .../target/debug/deps/remuda-session-host")`.
