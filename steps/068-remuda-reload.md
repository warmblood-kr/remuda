# 068 — `remuda reload`

## Before

Step 066 let `remuda reload --check` hear what a candidate daemon (C) saw.
Step 067 let C take over a listener and socket lock handed to it. Nothing
in the running daemon (O) handed them over, so `remuda reload` without
`--check` was refused. Upgrading still meant `remuda stop`, and every
client saw the socket vanish.

## Desired outcome

- `remuda reload [--binary PATH]` runs the check. Any blocker (an
  in-process session, a host C cannot drive) is reported, and O does
  nothing else.
- With no blocker, O:
  1. drains host-backed work for up to 10 s. What is still running is
     resolved by the next daemon, as after a stop.
  2. stops its cluster listener, releasing the listener host lock for C.
  3. pauses its accept loop. A pause returns only once the loop has seen it,
     so no accept is in progress. New clients wait in the backlog.
  4. sends `Take` with the listener and the lock, and waits for `Serving`.
- **Commit:** on `Serving`, O answers `reloaded: BUILD now serves this
  socket`. It waits up to 10 s for requests it accepted before the pause to
  be answered, then exits 0. It never unlinks the socket, because C owns
  it. The CLI then asks the socket for its version and prints `daemon now
  BUILD`.
- **Rollback:** on `Failed`, C's exit, or no `Serving` within the deadline
  (30 s), O kills C, then:
  - claims the hosts again under a newer epoch, which fences C;
  - reopens host work;
  - writes its pid back into the lock;
  - resumes accepting;
  - restarts the cluster listener.

  O answers "reload failed and was rolled back; this daemon still serves".
- The client waits up to 90 s for a reload, and 45 s for a check.
- **Mixed versions:** every pair of N and N+1 either reloads, or refuses
  with a reason and leaves the session working. The refusals: a build from
  before reload, another handoff version, or an older daemon that only
  checks. None advises `remuda stop`.

## Expected

- `native/tests/reload_cutover.rs` (real daemons):
  - **Handover:** two hosted sessions. A client lists sessions in a loop
    throughout, and none of its requests fails. The old daemon exits 0.
    The lock names a new pid that leads its own session. Both sessions keep
    their screens and take input.
  - **Accepted request:** a deferred Lua eval that O accepted before the
    handover, and answers 2 s later, still gets its answer.
  - **Blocker:** an in-process session blocks, and nothing changes.
  - **Candidate dies:** it exits after claiming the hosts, before serving.
    The reload exits 1. The same daemon holds the lock, drives the host and
    starts a new session, and no candidate is left.
  - **Candidate hangs:** it is killed at the 5 s test deadline, with the same
    rollback checks.
- `reload::` and `accept::` unit tests:
  - a pause returns once the loop stops accepting;
  - resume starts it again;
  - a pause with no loop times out;
  - a reopened gate admits again.
- `session_host_daemon::n_plus_one_*`: a reload, not only a check, in both
  directions.

## Actual

```
$ cargo test -p remuda-native --test reload_cutover
test result: ok. 5 passed; 0 failed

$ REMUDA_COMPAT_PREVIOUS=<step 067 build> cargo test -p remuda-native --test session_host_daemon -- --ignored --nocapture reload
reloaded: 0.1.0+git.dd784c8dd319 now serves this socket
daemon now 0.1.0+git.dd784c8dd319
remuda: reload without --check is not implemented yet; ...
test result: ok. 4 passed; 0 failed

# red: rollback without reclaiming the hosts
left: Error("agent io error: session owner unavailable: Recovering")
# red: rollback without reopening host work (hang test)
left: Error("agent io error: session owner unavailable: Recovering")
# red: the old daemon not exiting after the handover
the old daemon did not exit cleanly
# red: exiting without waiting for accepted requests (first seen in CI)
Ok(Error("the daemon hung up without answering"))
```

Limits:
- A request O accepted that takes longer than 10 s is cut when O exits.
  New connections are never lost.
- Lua state (timers, pending replies) does not move. C loads `init.lua`
  afresh.
- The test faults (`REMUDA_TEST_RELOAD_FAULT`) are read only by a
  candidate, and only after `Take`.
