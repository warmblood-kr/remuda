# Reloading the live daemon safely

A checklist for moving your running daemon onto a new build with
`remuda reload` (Unix) without losing sessions. [reload.md](reload.md)
says what a reload keeps and what starts fresh.

## Once, before the first reload

These need one ordinary restart, which ends today's sessions. After that,
every update is a reload.

1. **The daemon must know `remuda reload`.** A daemon from an older build
   cannot hand over: `remuda reload --check` says it "predates `remuda
   reload`". Run `remuda upgrade` to install a current build. The running
   daemon keeps its old build until the restart in step 2.
2. **Sessions must be host-backed.** Today this is opt-in. Start the daemon
   with `REMUDA_EXPERIMENTAL_SESSION_HOST=1` in its environment (#843
   proposes making it the default). A session started without it lives
   inside the daemon and ends with it. A reload refuses while one exists.
   Stop the old daemon (`remuda stop`), set the variable where the daemon
   is started, then start your sessions again.
3. **Butler must survive a reload.** The installed Butler needs
   remuda-butler #547 (takes over the home lock from the old daemon) and
   #549 (adopts its live sessions in the new image). Both merged on
   2026-10-22. Without them, after a reload Butler stays in standby and
   closes and relaunches its root session.
   - Check: `ls ~/.local/share/remuda/mods/butler/packages/butler/roster.lua`
     must exist.
   - Update: `remuda mod update butler --reload`.

## Before each reload

4. Install the new build, but keep the daemon on its current one:
   `remuda upgrade --no-reload`.
5. Run `remuda reload --check`. It changes nothing.
   - `ready: a reload into BUILD would keep every host`, plus one
     `host NAME: now X; candidate sees Y` line per session: go ahead.
   - `blocked: …` with `blocker:` lines: fix each one first.
     - An in-process session: close it, or wait until it is done.
     - `mod NAME: REASON`: the mod is busy (a launch, a running task).
       Wait and check again.
     - The new build cannot drive a host, or speaks another handoff
       version: do not reload into that build.
   - `warning: index.db is at schema N but BUILD reads up to M`: you are
     going back to an older build. Sessions are fine, but its VFS refuses
     every call until you reload forward again. To use the VFS on it, run
     `remuda vfs index-downgrade M --yes` first (it backs up the index).
6. If you edited `init.lua` since the daemon started, check it loads. The
   new daemon runs it again, and a broken one is reported after the reload,
   when it can no longer be undone.

## Run it

7. Run `remuda reload`. (`remuda upgrade` without `--no-reload` installs
   and reloads in one go. If a blocker stops the reload, the old daemon
   keeps serving and the message says why.) A good run prints:
   ```
   reloaded: BUILD now serves this socket
   host NAME: now BUILD; candidate sees BUILD
   daemon now BUILD
   sessions: all N host-backed listed
   init.lua: loaded
   ```
   and exits 0. If a session's host had already died before the reload,
   the sessions line reads `sessions: 1 of 2 host-backed reachable;
   unreachable before the reload too: NAME`. That session was lost
   earlier, not by the reload, and the run still exits 0.
8. While it runs (usually under a second, up to about 10 s if sessions are
   busy), input to a session may be refused with "session owner
   unavailable: Recovering". Send it again. Input that was answered `Ok` is
   never lost.
   A terminal attached to a session drops out, because attach goes through
   the daemon. The session keeps running: run `remuda attach NAME` again.
   The line it prints on the way out may say the session ended; it did not.
9. If it exits 1:
   - With "reload failed and was rolled back": nothing changed. The old
     daemon still serves every session. The message says why.
   - With "the reload went through, but the new daemon failed a check":
     the new daemon serves your sessions. Fix what the line above names,
     for example `init.lua`.

## What keeps its old build

A session's host keeps running the build it started with. The new daemon
drives it; CI checks this on every change against the previous build. New
sessions get the new build's host. A session moves onto the new host build
only when you start it again.

Typing into a session with `remuda attach` needs a host that takes attach
input (#808). A session whose host is older shows its screen, but typing
prints "read-only view: this session's host is too old to accept input",
and no key reaches it. `remuda send` still works. Start the session again
to type into it. The first reload after upgrading from a build without
attach input leaves every running session like this.

## What starts fresh

Lua state lives in the daemon, so the new daemon starts it again from
`init.lua` and your mods: globals, timers, and tools registered at runtime.
A reply still waiting 10 s after the handover is told to run the command
again. Processes started from Lua (`remuda.process`) are stopped.

## If something goes wrong

- **The new daemon died after taking over.** Your sessions keep running in
  their hosts. A command that starts a daemon (for example `remuda -e 'return 1'`
  or `remuda run`) starts one that finds them again. `remuda ls` does not
  start one; it says no daemon is running.
- **Do not use `remuda stop` to update.** It ends host-backed sessions too.
  `remuda stop --keep-sessions` leaves them running for the next daemon.
