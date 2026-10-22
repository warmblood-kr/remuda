# What `remuda reload` keeps (#713)

To reload your live daemon, follow [reload-checklist.md](reload-checklist.md).

`remuda reload` (Unix) moves a running daemon onto another build. The old
daemon hands its socket and its host-backed sessions to the new one, then
exits. This page says what crosses over and what starts fresh. If anything
goes wrong before the new daemon serves, the old daemon takes everything
back and nothing below applies.

## Kept

- **Host-backed sessions.** Their processes, screens and scrollback keep
  running in their session hosts. The new daemon claims the hosts. An act
  already running (input, resize, close) finishes and is answered. New
  host-backed acts are refused while the old daemon drains, which usually
  takes well under a second; send them again.
- **The socket and the clients waiting on it.** The listening socket passes
  to the new daemon with its backlog, so a client that connects mid-reload
  is answered by whichever daemon accepts it. The socket lock passes too,
  and names the new pid.
- **Requests the old daemon already accepted.** They are answered if they
  finish within 10 s of the handover.

## Not kept: the Lua image starts fresh

The new daemon has its own Lua interpreter and runs `init.lua` again, as
any daemon does at start. Gone with the old interpreter:

- globals and module state;
- timers and schedules (`remuda.after`, `remuda.schedule`);
- tools and words registered at runtime, unless `init.lua` or a mod
  registers them again;
- **pending replies** (`remuda.pending`) still waiting 10 s after the
  handover. Their callers get "daemon reloaded into a new build; this reply
  was lost, run the command again". Asking again reaches the new daemon.

The old daemon then ends what a clean `remuda stop` ends, except the
sessions and the socket:

- processes its Lua started (`remuda.process`) are killed, so they do not
  keep running beside the new daemon's own;
- modules are stopped, and their stop hooks run.

Code that must survive a reload keeps its state outside the image: in the
file store (`remuda vfs`), in a file, or in a session.

## Refused, nothing changes

A reload refuses, and leaves the old daemon as it was, when:

- a session is in-process, not host-backed (it would end);
- the new build cannot drive a host;
- either side is older than `remuda reload`, or they speak different
  handoff versions.

`remuda reload --check` reports these without changing anything.
`remuda upgrade` runs a reload after installing; a refusal there leaves the
old daemon serving and says why.

After a reload, `remuda reload` and `remuda upgrade` both check the new
daemon: it answers as the new build, lists every session it took over, and
loaded `init.lua`. A failed check exits 1 and names what failed. The new
daemon still serves, since the old one is gone.
A session whose host had already died before the reload is named, not
counted as moved, and does not fail the check.
