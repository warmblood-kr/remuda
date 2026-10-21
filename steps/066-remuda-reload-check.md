# 066 — `remuda reload --check`

## Before

There was no `remuda reload` at all (gap G7 of the plan, #713). Nothing could
start a second daemon beside the running one, so nothing could ask whether
a new build would be able to drive the session hosts that outlive the old
one. Steps 064 and 065 added the drain, the reclaim and a read-only
preflight, but no caller used them.

## Desired outcome

- `remuda reload --check [--binary PATH]` asks the running daemon (O) to
  check a reload into PATH. The default is the CLI's own binary: the freshly
  installed remuda, never the daemon's own, possibly replaced, file.
- O starts a candidate (C) from PATH as `remuda _reload-candidate SOCKET`.
  - C gets its end of a private socketpair on fd 3, plus O's environment,
    working directory and stderr.
  - C sends `Hello { handoff: 1, build }`, then runs the read-only preflight
    and sends `Ready { hosts }` (or `Failed`).
  - C claims nothing, binds nothing and takes no lease.
- O compares C's view with its own and reports `ready:` or `blocked:`, one
  line per blocker, then one line per host.
  - A host O can drive and C cannot is a blocker.
  - So is any in-process session, which a reload would end.
- These all fail fast, and the candidate is then reaped or killed:
  - C exits before its hello (a build from before reload);
  - C sends no hello or ready by the deadline (10 s and 30 s);
  - C speaks another handoff version.
- One reload at a time. Cluster peers refuse `Reload`. On Windows the daemon
  refuses it.
- An older daemon cannot read `Reload`. The CLI says the daemon predates
  reload and that its sessions keep running. It does not pass on the usual
  skew advice (`remuda stop`), which would end them.
- Reload without `--check` is refused for now; the cutover is the next step.

## Expected

- `native/tests/reload_check.rs` (real daemon):
  - two hosted sessions report `ready`, with each host's build on both sides;
  - the same daemon pid serves them afterwards, and no candidate is left;
  - an in-process session blocks;
  - a fake candidate that exits like an old build is refused within 5 s;
  - a hung one is killed at the (test) 3 s deadline;
  - one speaking handoff 99 is refused;
  - one that cannot drive a host blocks with that host named.
- `session_host_daemon::n_plus_one_*` (CI job `host-compat-n-plus-one`,
  real base build): a check from N+1 into N, and N asked by an N+1 CLI. Both
  leave the hosted session working and never advise `remuda stop`.
- Unit tests in `reload.rs`: the comparison and one reload at a time.

## Actual

```
$ cargo test -p remuda-native --test reload_check
test result: ok. 6 passed; 0 failed

$ REMUDA_COMPAT_PREVIOUS=<slice-2 build> cargo test -p remuda-native --test session_host_daemon -- --ignored n_plus_one
test result: ok. 4 passed; 0 failed

# before the CLI handled the older daemon's refusal:
remuda: the daemon is not this build — run `remuda stop`, then this again. ... unknown variant `Reload` ...

# compare() with the host blocker disabled:
panicked at native/tests/reload_check.rs:293:5
```

Limits:
- Nothing is handed over yet.
- C inherits O's environment, so a stale variable in O carries over (the
  Unicorn failure mode). It is kept for now so new sessions get the same
  defaults as under O.
