# 062 — each daemon starts its own build's session host

## Before

`host_executable` (`native/src/session_host/hosted.rs`) resolved
`remuda-session-host` beside `current_exe()` at every spawn and never checked
which build it was. Hosts outlive daemons, and the installer swaps binaries
by rename while a daemon runs. So between an install and the daemon's
restart, the old daemon started hosts of the new build. After a rollback, it
would face hosts newer than itself. This is the sshd 9.8 failure pattern
(an old listener exec'd the newly installed binary). Item G1 of the
`remuda reload` gap list (#713).

## Desired outcome

- `remuda-session-host version` prints the host's build
  (`REMUDA_BUILD_VERSION`, the same string as `remuda --version`).
- The daemon starts only a host that reports the daemon's own build. It
  first tries `remuda-session-host-<build>` beside itself, then the plain
  name. The answer is cached per file (device, inode, size, ctime). A host
  that cannot answer (older than this step) counts as an unknown build.
- If neither fits, `new` is refused at once with both paths and builds named,
  before any inventory reservation, so nothing is left behind.
- The daemon passes `REMUDA_SESSION_HOST_BUILD=<build>` to every host it
  starts. A host of another build exits with status 3 before it reads its
  bootstrap. The daemon now notices a host that ends before it publishes
  and fails at once, instead of waiting out the 10 s publication deadline.
- `install.sh` keeps each host that can say its build as
  `remuda-session-host-<build>`, the installed host before the new one
  replaces it. The three most recently installed are kept (each install
  touches the ones it keeps, so a reinstall of an older build survives).

## Expected

- `native/tests/session_host_build.rs`: a real daemon from a private bin
  directory keeps starting its own host after a fake newer host lands under
  the plain name. With its versioned copy also gone, it refuses within 5 s
  and writes nothing. A host swapped after the check makes `new` fail fast
  with "is not this daemon's build". A real host of another build exits 3.
- Unit tests in `host_build.rs`: lookup order, refusal text, unknown build,
  unsafe build strings, re-probe after a rename, a hung host.
- `scripts/check-install-pair.sh`: kept hosts across seven installs,
  including pruning, a reinstall of an older build, and a host that cannot
  say its build.

## Actual

```
$ cargo test -p remuda-native --test session_host_build
test result: ok. 3 passed; 0 failed

$ cargo test -p remuda-native --lib host_build
test result: ok. 8 passed; 0 failed

# host_executable back to "beside current_exe", no early-exit check:
panicked at native/tests/session_host_build.rs:123:36:
daemon request: Custom { kind: TimedOut, error: "timed out after 10s waiting for daemon response ...
test result: FAILED. 1 passed; 2 failed

$ sh scripts/check-install-pair.sh
ok — install.sh lands the remuda/session-host pair, keeps the previous pair as .prev, and keeps versioned hosts

# with the step-060 install.sh:
check-install-pair: pair after a hostless release: kept hosts are '', want '1 2 4'
check-install-pair: versioned host: remuda-session-host-1+b is missing or not executable
```

Limits: `install.ps1` keeps no versioned host (Windows has no host yet,
#214). A host swapped in the instant between the check and the spawn leaves
its launch reservation in the inventory, like any other failed start; a
reservation cannot be retired yet. A daemon whose versioned copy was pruned
refuses new hosted sessions until it is restarted.
