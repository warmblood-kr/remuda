# 074 — Reload under faults: typing, a dead host, the cluster listener

## Before

Step 073 covered reloads that race and daemons that die. Three things a
live daemon meets were untested: input arriving all through the reload, a
session whose host had already died, and the cluster listener. One was a
bug: the post-check said "sessions: all 2 host-backed listed" when one of
the two hosts was dead. It counted the session as listed because it was
`alive`, though the daemon listed it `unavailable`.

## Desired outcome

- Every `SendLine` sent during a reload is answered `Ok` and runs, or is
  refused with "session owner unavailable: Recovering" while the old
  daemon drains. None is lost or fails another way.
- A host that was unreachable before the reload does not fail it. The
  post-check names the session instead of claiming it moved:
  `sessions: 1 of 2 host-backed reachable; unreachable before the reload
  too: NAME`. A host reachable before and not after still fails the check.
- The cluster listener ends up on whichever daemon serves: the new one
  after a reload, the old one after a rollback.

## Expected

- `reload_faults`: eight typists through a reload, where every
  acknowledged line reaches the shell; a SIGKILLed host, then a reload
  that goes through and names it, while the live session answers.
- `reload_vfs_fence`: the listener's port accepts after a reload and
  after a rolled-back one. Its cleanup now kills by checked pid
  (`reap.rs`), not with `pkill -f`.

## Actual

```
$ cargo test -p remuda-native --test reload_faults --test reload_vfs_fence
test result: ok. 8 passed; 0 failed
test result: ok. 3 passed; 0 failed

# red: post-check before the fix
sessions: all 2 host-backed listed
# red: rollback without restarting the listener
Some("exit-before-serving"): nothing listens on 127.0.0.1:49602 after the reload
```
