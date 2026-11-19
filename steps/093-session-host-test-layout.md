# 093 — In-process test daemons find the built session host

## Before

Hosting is on by default on supported Unix systems. A lib test runs the daemon
in-process, so `std::env::current_exe()` is the test binary under
`target/<profile>/deps/`. The host lookup therefore asked for
`deps/remuda-session-host`, which Cargo never writes (it builds the host beside
the profile's `remuda`). Every `Request::New` returned `session host binary not
found`, and on `main` this failed the `tui_unit` tests and the full-suite
`test`/`gates-can-fail` jobs since 2026-11-15.

## Desired outcome

In test builds, when the current executable sits under `deps/`, anchor the host
lookup at the profile's `remuda` binary. The existing build probe still decides
whether that host is usable, so a stale host is refused with the existing
diagnostic. The production path is unchanged.

## Expected

- The new path test passes.
- The formerly failing `tui_unit` and session-host tests pass.
- `cargo fmt --check` is clean, and the full lib suite has no new failures.

## Actual

```text
cargo test --locked -p remuda-native --lib session_host::host_build::tests::a_test_binary_finds_the_host_beside_the_profile_daemon
test result: ok. 1 passed; 0 failed; ...

cargo test --locked -p remuda-native --lib tui_unit
test result: ok. 136 passed; 0 failed; 1 ignored; ... finished in 9.39s

cargo test --locked -p remuda-native --lib session_host::child::tests
test result: ok. 7 passed; 0 failed; ...

cargo test --locked -p remuda-native --lib
test result: FAILED. 1933 passed; 1 failed; 7 ignored; 0 measured; finished in 121.51s
```

The one failure is `session_host::remote::tests::a_takeover_between_completion_and_ack_does_not_wedge_later_writes`
with `BrokenPipe` under the full-suite load; it passes alone (0.50s) and is
outside this change. Before the fix, the same suite failed the `tui_unit` set.
`cargo clippy --locked -p remuda-native --all-targets -- -D warnings` and
`cargo fmt --check` are clean.

Before the change, the reproduced local failure was
`Error("session host binary not found: .../target/debug/deps/remuda-session-host")`.
