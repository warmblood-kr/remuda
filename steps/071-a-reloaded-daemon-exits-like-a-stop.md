# 071 — After a reload, the old daemon exits like a stop

## Before

After handing over (step 068), the old daemon answered the requests it had
accepted, waiting up to 10 s, and then simply exited. It skipped what a
clean `remuda stop` does:
- a deferred reply (`remuda.pending`) still waiting got "the daemon hung up
  without answering";
- processes its Lua had started (`remuda.process`) were not killed. On
  macOS nothing kills them when the daemon exits, so they ran on beside the
  new daemon's own;
- module stop hooks never ran.

Nothing documented what a reload keeps.

## Desired outcome

- After the 10 s answer window, the old daemon runs a clean stop's teardown
  (`reap_before_exit`), with the sessions and the socket left alone:
  - deferred replies still waiting fail with "daemon reloaded into a new
    build; this reply was lost, run the command again";
  - Lua's processes are killed;
  - modules are stopped.
- That runs only after the commit point, so a rolled-back reload loses
  nothing.
- `docs/reload.md` says what is kept (host-backed sessions, the socket and
  its backlog, accepted requests) and what starts fresh (the Lua image,
  timers, runtime tools, pending replies, Lua processes).

## Expected

- `reload_cutover`: a never-resolved `remuda.pending` and a Lua
  `sleep 300` exist across a reload. The waiting client gets the
  reloaded error, the sleep is gone, and the hosted session answers the new
  daemon.
- A deferred reply that resolves 2 s after the handover is still answered
  (existing test).

## Actual

```
$ cargo test -p remuda-native --test reload_cutover
test result: ok. 6 passed; 0 failed

# red: without the teardown
Ok(Error("the daemon hung up without answering"))
# red: teardown with the stop message
Ok(Error("daemon stopping"))
# red: without killing Lua's processes
the old daemon's Lua process 23372 outlived it
```
