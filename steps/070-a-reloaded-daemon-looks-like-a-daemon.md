# 070 — A reloaded daemon looks like a daemon

## Before

After `remuda reload` (step 068), the daemon serving the socket showed in
`ps` as `remuda _reload-candidate /…/s.sock`, not `remuda -s NAME daemon`.
Anything that finds the daemon by its command line, such as `pkill`, a
monitor or a person reading `ps`, missed it or could not place it.

## Desired outcome

- Every build's old daemon still starts a candidate with
  `_reload-candidate SOCKET`, so builds on either side of this change still
  reload into each other.
- A candidate of this build re-executes itself first, before any thread
  starts, as `remuda -s NAME daemon`, where NAME is the socket's stem. The
  socket travels in `REMUDA_RELOAD_CANDIDATE`. The pid, fd 3 and stdio stay.
- The new process clears that variable before doing anything else, so
  neither hosts nor `process.run` children inherit it.
- The variable alone does nothing: it counts only when fd 3 is a socket.
- If the exec fails, the candidate goes on under its verb and says why.

## Expected

- `reload_take`: after `Take` and `Serving`, `ps -o args= -p PID` ends in
  ` -s s daemon`. `process.run{"/usr/bin/env"}` in the new daemon shows
  `REMUDA_RUNTIME_DIR` but not `REMUDA_RELOAD_CANDIDATE`.
- `remuda --version` with a stray `REMUDA_RELOAD_CANDIDATE`, and no handoff
  socket, prints the version.
- The N/N+1 reload tests still reload both ways against #793's build.

## Actual

```
$ cargo test -p remuda-native --test reload_take
test result: ok. 3 passed; 0 failed

# red: without the re-exec
…/target/debug/remuda -s s _reload-candidate /private/tmp/rrt-82453-take/remuda/s.sock
# red: without clearing the variable
panicked at native/tests/reload_take.rs:253:5

# N/N+1 against #793's build, both directions
reloaded: 0.1.0+git.81d568bd47c6-dirty now serves this socket
reloaded: 0.1.0+git.3c140ca1b896 now serves this socket
```
