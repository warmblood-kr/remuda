# 067 — A reload candidate takes over the endpoint

## Before

`remuda reload --check` (step 066) started a candidate daemon (C) beside
the running one (O), heard what it saw of every session host, and stopped
it. Nothing could hand C the daemon's socket. A new daemon could only take
over by binding afresh after O was gone, so clients saw the socket vanish.
Clients that had connected but were not yet accepted were lost.

## Desired outcome

- Handoff version 2 adds `Take` (O to C) and `Serving` (C to O).
- `Take` carries two fds as SCM_RIGHTS: O's listening socket, then its
  held socket lock.
  - A passed fd shares the open file. The listener's backlog goes with it,
    and so does the lock's `flock`.
  - O may keep its copies. That is what lets it roll back.
  - Received fds are close-on-exec, and so is C's handoff fd. Sessions
    never inherit them.
- On `Take`, C:
  - calls `setsid`, so it outlives O's session;
  - adopts the lock and writes its own pid in it;
  - skips the "already listening" probe, since that listener is now its own;
  - adopts the published socket node as its own, so a clean stop removes it;
  - rediscovers and claims the session hosts;
  - sends `Serving` just before it starts accepting.
- A take that fails before serving (wrong fd count, unusable fd) sends
  `Failed { reason }` and exits non-zero, having claimed nothing.
- The old daemon's side (pause accept, send `Take`, commit on `Serving`, roll
  back otherwise) is the next step. `remuda reload` without `--check` stays
  refused.

## Expected

- `reload::fds` unit tests:
  - a passed listener keeps a client queued before the handover;
  - a passed lock stays held until every copy closes;
  - a received fd is close-on-exec;
  - a 60 KiB body arrives whole with two fds.
- `native/tests/reload_take.rs`, with the test as O: a real first daemon
  leaves a hosted session and stops cleanly. The test then binds the
  socket and locks the lock file.
  - A client connects and waits in the backlog. A real candidate gets
    `Take` and sends `Serving`, and that client is answered.
  - The lock names the candidate's pid, and the candidate leads its own
    session.
  - The candidate drives the hosted session, and a clean stop removes the
    socket.
  - `Take` with one fd gets `Failed` ("take carried 1 fds, not 2") and a
    non-zero exit. A daemon started next drives the host at once, because
    the candidate claimed nothing.

## Actual

```
$ cargo test -p remuda-native --lib reload::fds
test result: ok. 3 passed; 0 failed

$ cargo test -p remuda-native --test reload_take
test result: ok. 2 passed; 0 failed

# red: without skipping the probe for a handed listener
{"kind":"Failed","reason":"a daemon is already listening at …/s.sock — pick a different name …"}

# red: without writing the pid into the adopted lock
assertion `left == right` failed
  left: "92349"
 right: "92367"
```

Limits:
- A build that speaks handoff 1 (step 066 alone) is refused with
  "speaks handoff version 1, this daemon 2", for checks as well.
- C's cluster listener finds O's listener host lock held, and retries every
  30 s. So it starts up to 30 s after O exits. The next step stops O's
  listener before `Take`.
