# 013 — a daemon that says which build it is

`remuda upgrade` replaces the binary and cannot touch a daemon already running.
This is the message that names that, the verb that fixes it, and the handshake
that catches it next time.

## Before

정수님 ran `remuda upgrade`, then `remuda`, got the TUI, pressed Enter at the
start prompt, and saw:

```
remuda: bad request: invalid type: null, expected a string at line 1 column 19
```

His daemon was built from `618cda4^`. `618cda4` changed `Request::New`'s `name`
from `String` to `Option<String>` (`core/src/protocol.rs`), so the new client
sends `"name":null` and the old deserializer refuses it. Column 19 is the end of
`null` in `{"New":{"name":null`.

Three things were wrong, and only the first is about that one field.

**A — the message named the wrong thing.** A serde column number describes the
byte where a *parser* gave up. Nothing on that line says "daemon", "build", or
"restart", and nothing tells the reader that the fix takes one command. The real
cause is not in the request at all: it is that two processes on this machine were
built from different commits.

**B — there was no verb for it.** `remuda upgrade` says, in `README.md`, that it
"lands the new binary by rename, so upgrading while a daemon is running is safe."
That is true of the *file* and false of the *process*: the daemon outlives every
client by design (`native/src/daemon.rs:5-9`), so it keeps running yesterday's
code until something stops it. The only cure was `pkill -f 'remuda daemon'`,
which appears in no help text.

**C — the skew was invisible until it broke.** `Request::List` is a unit variant
carrying no fields, so a stale daemon answers it perfectly. `remuda ls` printed a
correct herd and the TUI painted a flawless first screen. Skew surfaces only on
the first request whose *shape* changed — which means "it connected, so it
matches" is wrong by construction, and the moment it does surface is the moment
work is already in flight.

## Desired outcome

A user on a stale daemon reads a sentence that names the cause and the cure, in
that order, and runs one command. Nobody is told to `pkill` anything. Nobody
learns about the mismatch from a column number.

And the *next* protocol change does not need diagnosing at all: the client asks
the daemon which build it is before anything else, and says so.

## Expected

1. `client::interpret` turns a `bad request:` reply — and an unparseable reply —
   into one line that leads with `remuda restart`. Every other refusal reaches
   the user in the daemon's own words, untouched.
2. `remuda restart [-f]` stops this server's daemon. With a live herd it names
   every session it would kill and refuses without a `y` or `-f`.
3. `Request::Version` answers with `dist::VERSION`; the client asks once per run
   and prints one line when the answer is not its own.
4. An old daemon receiving `Version` or `Shutdown` refuses the unknown variant
   cleanly — it must not hang, crash, or say something worse than before — and
   that refusal is the input to (1).

## Actual

The reproduction is a daemon built from `618cda4^` (`7c69628`) and a client built
from this branch, each stamped with the version string its nightly release would
have injected, isolated by `REMUDA_RUNTIME_DIR=/tmp/rskew`.

**What an old daemon does with the two new variants** — checked before relying on
it, because the whole design of (1) rests on the answer:

```
  "Version"    -> {"Error":"bad request: unknown variant `Version`, expected one of `List`, `New`, `SendLine`, `Send`, `Capture`, `Attach`, `Close`, `Eval` at line 1 column 9"}
  "Shutdown"   -> {"Error":"bad request: unknown variant `Shutdown`, expected one of `List`, `New`, `SendLine`, `Send`, `Capture`, `Attach`, `Close`, `Eval` at line 1 column 10"}
  "List"       -> {"Sessions":[]}
--- stale daemon alive after both new variants? yes
--- and against a CURRENT daemon, for the positive control:
  "Version"    -> {"Value":"0.1.0-nightly.20260910075618.dd776f5"}
  "Shutdown"   -> "Ok"
--- current daemon alive after Shutdown? no
```

An unknown variant is a plain refusal on the same `bad request:` path, so it
arrives at the client wearing the shape (1) already handles. The daemon survives
it, which is what makes a version probe safe to send blind.

**Before** — `main` at `8d0be31`, same two processes:

```
  the daemon: remuda 0.1.0-nightly.20260910062426.10a8913   (618cda4^ = 7c69628)
  the client: remuda 0.1.0-nightly.20260910075618.dd776f5

$ remuda ls
no sessions

$ remuda run echo hi          # what Enter at the TUI start prompt sends
remuda: bad request: invalid type: null, expected a string at line 1 column 19
exit=1

$ remuda restart
remuda — a pty manager you can attach to
[… the whole USAGE text, exit=1 …]
```

`ls` is the shape of defect C: perfect, and about a daemon that cannot run a
session.

**After** — this branch:

```
  the daemon: remuda 0.1.0-nightly.20260910062426.10a8913   (618cda4^ = 7c69628)
  the client: remuda 0.1.0-nightly.20260910075618.dd776f5

$ remuda ls
remuda: the daemon is not this build — `remuda restart` replaces it, and its sessions and Lua image go with it. It is from a build that predates this handshake; this command is 0.1.0-nightly.20260910075618.dd776f5
no sessions

$ remuda run echo hi          # what Enter at the TUI start prompt sends
remuda: the daemon is not this build — `remuda restart` replaces it, and its sessions and Lua image go with it. It is from a build that predates this handshake; this command is 0.1.0-nightly.20260910075618.dd776f5
remuda: the daemon is not this build — run `remuda restart`, then this again. This command is 0.1.0-nightly.20260910075618.dd776f5; the daemon: it could not read the request: invalid type: null, expected a string at line 1 column 19

$ remuda restart
remuda: stopped the daemon for "skew" — the next command starts a fresh one

$ remuda ls                   # the next command starts a fresh daemon
remuda: started a daemon for "skew"
no sessions
```

`ls` now carries the warning, so defect C is closed *for a daemon that can be
asked*. The stale one cannot be, and says so in those words.

**The TUI, which is the path 정수님 actually took.** The client is run inside a
remuda session of the fixed build — the same trick `steps/012` used — and driven
with `remuda.key('tui', 'RET')`, then captured:

```
remuda · skew                           │
                                        │
  the herd is empty.                    │
                                        │
…
remuda: the daemon is not this build — run `remuda restart`, then this again. T→
```

That last line is the one that mattered, and it is what fixed the wording. The
footer is one row of `cols` and crops the tail; the first draft led with the
diagnosis and produced

```
remuda: the daemon is running a different build than this command (remuda 0.1.0→
```

— accurate, and with the only actionable half of the sentence off the right edge.
The cure goes first now, and `the_cure_survives_an_eighty_column_crop` in
`native/src/client.rs` is the test that keeps it there.

**Gates**, on this branch:

```
$ cargo test --workspace --all-targets
test result: ok. 21 passed;  ok. 10 passed;  ok. 5 passed;  ok. 39 passed;
test result: ok. 9 passed;   ok. 8 passed;   ok. 4 passed;  ok. 4 passed;
$ cargo clippy --workspace --all-targets -- -D warnings     Finished
$ python3 scripts/check-comments.py    ok — 170 doc comment(s) within cap
$ python3 scripts/check-install.py     ok — 3 target(s) built and offered
$ python3 scripts/check-principles.py  ok — 14 principles
$ python3 scripts/check-workflows.py   ok — 3 workflow file(s) parse
$ python3 scripts/check-steps.py       ok — 13 step(s)
```

## Ceilings

**`restart` needs two mechanisms, and one is a back door.** `Request::Shutdown`
is the door, but no deployed daemon has it — which is the entire problem this
step exists for. The lever those daemons *do* leave is their Lua image, and
`Eval { code: "os.exit(0)" }` ends the same process. Measured: it works, and it
is the only reason `remuda restart` is true advice for the population that needs
it today. It is also a lifecycle operation smuggled through the scripting door,
and it would break silently if the image were ever sandboxed. Drop the fallback
after one release, the way `lua_script_refusal` is scheduled to go.

**Two local `cargo build`s report the same version.** `dist::VERSION` falls back
to `CARGO_PKG_VERSION` when `REMUDA_VERSION` is not injected, so the handshake is
silent between two development builds of different commits — the exact case a
contributor hits. The `bad request:` path (1) still catches those, one request
later. A protocol fingerprint rather than a version string would close it; that
is a bigger change than this incident earns.

**A field ADDED with `#[serde(default)]` is still invisible.** serde ignores
unknown fields by default, so a new optional field sent to an old daemon produces
no error at all — just quietly different behaviour. That is the case the
handshake exists for and the `bad request:` message cannot see, and it is why the
probe is proactive rather than only reactive.

**The probe costs one round trip per command** when a daemon is already running,
and none when we are about to start one ourselves (it would be us). On a local
socket that is well under a millisecond; it has not been measured on Windows'
named pipes.

**Mismatch warns, it does not refuse.** A hard refusal would strand someone
mid-work behind a version string, which is its own incident — and most skews are
harmless, since a unit variant and an unchanged field cross versions fine. The
warning names the cure and the command proceeds.
