# 063 — show each session host's build

## Before

Hosts outlive daemons (#713), so one daemon can serve hosts started by
several builds: hosts from before an upgrade keep running. A host already
reports its build in `Inspect` (`Info.host_build`), but the adapter dropped
it, and nothing showed it to the operator. Before a reload or a rollback
there was no way to see which builds still serve sessions. Item G2 of the
`remuda reload` gap list.

## Desired outcome

- `Observation` carries `host_build`: the host's build for a host-backed
  session, `None` for an in-process one.
- New request `SessionHostBuilds` answers one `{name, build}` per
  host-backed session, sorted by name; `build` is `None` for an
  unreachable host. Additive: an older daemon answers `bad request`, and an
  older CLI never sends it. Cluster peers refuse it, like `Version`.
- `remuda version --hosts` prints the CLI build, the daemon's build, and one
  `host NAME BUILD` line per hosted session, marking hosts that are not the
  daemon's build and hosts that are unreachable. It never starts a daemon.
  Plain `remuda version` is unchanged and still never touches the socket.
- Lua gets the answer as `{ {name=..., build=...}, ... }`.

## Expected

- `native/tests/session_host_versions.rs`: two hosted sessions through a
  real daemon; the request and `remuda version --hosts` both name each host
  with its build. With no daemon, `--hosts` fails and starts none.
- `host_build_tests` in the CLI: another build and an unreachable host are
  named; an older daemon is reported as such.

## Actual

```
$ cargo test -p remuda-native --test session_host_versions
test result: ok. 2 passed; 0 failed

$ cargo test -p remuda-native --bin remuda host_build_tests
test result: ok. 2 passed; 0 failed

$ remuda version --help
Print the version of this CLI binary. Example: remuda version

Usage: remuda version [OPTIONS]

Options:
  -h, --help   Print help
      --hosts  Also print the running daemon's build and each session host's build
```

Limits: one `Inspect` round trip per host, in series. No policy acts on the
builds yet; a reload preflight (later step) will.
