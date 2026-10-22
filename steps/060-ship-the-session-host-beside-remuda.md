# 060 — ship the session host beside remuda

## Before

`release.yml` built and packaged only `remuda`, and neither `docs/install.sh`
nor `docs/install.ps1` installed `remuda-session-host`. The opt-in host path
(step058) starts the host from beside the daemon's own executable, so on any
installed remuda `REMUDA_EXPERIMENTAL_SESSION_HOST=1` failed with
`session host binary not found`. `host_executable` also never added `.exe`, so
a Windows host could not be found. An upgrade overwrote the old binary with no
way back. This is step 1 of the hot daemon replacement (`remuda reload`) plan.

## Desired outcome

- Every release tarball holds `remuda` and `remuda-session-host` from one
  build (`.exe` on Windows). The package step fails if either is missing.
- Both installers land the pair, the host first, so a new daemon never
  starts beside an old host. Each binary still lands by rename.
- The pair being replaced is kept as `<name>.prev` for rollback. A `.prev`
  pair with no host is still one pair: a stale `remuda-session-host.prev` is
  removed.
- An asset from before the pair (no host) still installs, and removes any
  installed host, so no host from another build is left beside the daemon.
- The daemon looks for `remuda-session-host` plus the platform's executable
  suffix.

## Expected

`scripts/check-install-pair.sh` (CI `cold-install`) and
`scripts/check-install-pair.ps1` (CI `test-windows`) install four fake
releases in order and check every file: a fresh pair, an upgrade with `.prev`,
a release without a host, and a pair after it. The sh check fails on the old
installer. The unit test
`host_binary_is_looked_up_beside_the_daemon_with_the_platform_suffix` covers
the name.

## Actual

```
$ sh scripts/check-install-pair.sh
ok — install.sh lands the remuda/session-host pair and keeps the previous pair as .prev

$ git stash push docs/install.sh && sh scripts/check-install-pair.sh   # old installer
check-install-pair: fresh install: remuda-session-host is missing or not executable
check-install-pair: upgrade: remuda-session-host is missing or not executable
check-install-pair: upgrade: remuda.prev is missing or not executable
check-install-pair: upgrade: remuda-session-host.prev is missing or not executable

$ cargo test -p remuda-native --lib session_host
test session_host::hosted::tests::host_binary_is_looked_up_beside_the_daemon_with_the_platform_suffix ... ok
test result: ok. 149 passed; 0 failed; 5 ignored; 0 measured; 1241 filtered out

$ cargo build --release --bin remuda --bin remuda-session-host && tar -czf a.tar.gz -C target/release remuda remuda-session-host && tar -tzf a.tar.gz
remuda
remuda-session-host
```

`check-install-pair.ps1` was not run locally (no pwsh on this machine). Its
evidence is the `test-windows` CI job.
