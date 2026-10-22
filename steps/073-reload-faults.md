# 073 — Reload under faults: one daemon serves, never two

## Before

Step 068 tested a candidate that dies or hangs before serving. Untested:
two reloads at once, reloads in a row, and a new daemon dying after it
took over. One of these was a bug. After a successful handover the old
daemon stays up to 10 s to answer requests it had already accepted. A
`Reload` that arrived on one of those connections started a second
handover, and gave the same socket and lock to a second new daemon.

## Desired outcome

- Once a handover succeeds, the old daemon refuses every further reload:
  "this daemon already handed its socket to a new daemon and is exiting;
  run the command again to reach the new one".
- Two reloads at once: one goes through, and the other is refused with
  "another reload is already in progress" or the message above.
- Reloads chain: a daemon that came from a reload reloads again.
- If the new daemon dies after taking over, the hosts live on, and the
  next daemon to start finds them and drives them.

## Expected

`native/tests/reload_faults.rs`:
- a `Reload` sent, after the handover, on a connection the old daemon
  accepted before it is refused, and the lock still names the first new
  daemon;
- two concurrent `remuda reload`: exactly one `reloaded:`, the other
  refused, one daemon serving, the session answers;
- two reloads in a row: a new pid each time, each previous daemon exits,
  and the session answers;
- SIGKILL to the new daemon, then a fresh daemon: the session's screen and
  input work.

## Actual

```
$ cargo test -p remuda-native --test reload_faults
test result: ok. 4 passed; 0 failed

# red: before the fix
the old daemon handed the socket over a second time:
{"Value":"reloaded: 0.1.0+git.9e93b7968b56 now serves this socket …"}
```
