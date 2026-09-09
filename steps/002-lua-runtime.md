# 002 — a programming runtime, with the atomic functions wired to it

정수님, 2026-09-10: *"그 다음에는 이 pty manager layer에 programming runtime을
심어서 코드를 실행할 수 있게 만들고 atomic function들을 물려서 연결합니다. 일종의
programmable tmux 같은 컨셉이랄까요?"*

## Before

Step 001 left five operations — `List`, `New`, `SendLine`, `Capture`, `Attach` —
defined as pure messages in `core/src/protocol.rs` and reachable from the CLI one
invocation at a time. They compose only through the shell:

```
remuda new build && remuda send build "make" && remuda capture build | grep -q PASS
```

That works, and it is missing the thing a pty manager exists for. **A shell
pipeline cannot react to what a session shows.** There is no way to send a line,
wait until the prompt comes back, decide what to send next, and loop — which is
the entire job of driving a program through a terminal. Every interesting use of
this tool is a *conditional sequence over screen contents*, and the CLI can
express only one unconditional step per process.

Two smaller problems ride along:

- each invocation is a fresh connect, so a hundred-step sequence is a hundred
  daemon connections;
- an error survives `&&` but not command substitution — `$(remuda capture x)` on
  a missing session yields an empty string, and a script built on it proceeds on
  a false premise rather than stopping.

## Desired outcome

`remuda run <script.lua>` executes a script with the atomic functions bound, so
the loop above becomes expressible:

- a script can `send`, `capture`, test what came back, and send again;
- a failure is **raised**, not returned — a script that does not check an error
  dies at the error instead of continuing on nothing;
- the whole sequence runs over connections the script does not manage.

And one property matters more than any of that:

**Embedding a general-purpose language must not widen the API.** The vocabulary
handed to Lua is exactly `Request` — the surface that already has no raw write
and no resize. A script gets Turing-completeness over *which* operations run in
*what order*; it gets no operation the CLI does not already have. Invariant 3 is
enforced by the daemon at call time, not by caller discipline, so a script that
sends into a human-held session is refused the same way the CLI is.

That is principle 6 collecting its rent: because the dangerous operations were
removed from the API rather than forbidden by rule, an entire scripting language
could be pointed at it without re-auditing anything.

### What is deliberately not built

- **No sandbox.** `remuda run script.lua` is as trusted as `sh script.sh` — the
  user runs their own file on their own machine. That changes the day a script
  arrives from another node over the transport layer; the restricted stdlib
  belongs in *that* change, where the boundary appears, not speculatively here.
- **No `wait_for` helper in Rust.** `sleep` is bound and the loop is written in
  Lua. Building the connective tissue in the host would be doing in Rust the
  exact thing the guest language was embedded to express.

## Expected

1. A script creates a session, sends a command, polls until the output appears,
   and prints it — the conditional loop the shell could not express.
2. `send` to a session that does not exist raises a Lua error carrying the
   daemon's own words, and execution **stops** rather than continuing.
3. The bound surface is exactly the five protocol operations plus `sleep`. A
   test asserts the set of names, so adding a sixth operation without deciding
   to fails.
4. `capture` returns a string, `ls` a table of session records, `new`/`send`
   nothing — and a script can branch on what `capture` returned.
5. Nothing in the Lua surface can write raw bytes or resize a terminal, because
   `Request` has no such variant to bind.

## Actual

### The loop the shell could not express

`build.lua` starts a session, sends a command, polls until the answer appears,
**branches on what came back**, and lists what it created. `wait` is written in
Lua, not bound from Rust.

```lua
remuda.new("build", {"sh"})
remuda.send("build", "echo $((6*7))-ready")

local function wait(name, pattern)
  for _ = 1, 500 do
    local screen = remuda.capture(name)
    if screen:find(pattern) then return screen end
    remuda.sleep(0.02)
  end
  error("never saw " .. pattern .. " in " .. name)
end

wait("build", "42%-ready")

if remuda.capture("build"):find("42%-ready") then
  remuda.send("build", "echo $((11*11))-and-the-next-step-depended-on-it")
end
wait("build", "121%-and")

for _, s in ipairs(remuda.ls()) do
  print(string.format("%-10s %dx%d  alive=%s", s.name, s.cols, s.rows, tostring(s.alive)))
end
```

```
$ remuda run build.lua
build      80x24  alive=true
exit 0

$ remuda capture build | tail -4
$ 42-ready
$ echo $((11*11))-and-the-next-step-depended-on-it
121-and-the-next-step-depended-on-it
$
```

Expectations 1 and 4 met. `42` and `121` cannot come from the pty echoing the
commands, so the screen shows the shell ran — and the second command exists only
because the script read the first one's answer.

### A refusal stops the script

```lua
remuda.send("build", "echo BEFORE-THE-ERROR")
remuda.send("typo-in-the-name", "echo hi")
remuda.send("build", "echo AFTER-THE-ERROR")
```

```
$ remuda run oops.lua
remuda: runtime error: no such session: typo-in-the-name
stack traceback:
	[C]: in field 'send'
	/tmp/rd-demo/oops.lua:2: in main chunk
exit 1

$ remuda capture build | grep -E 'BEFORE-THE-ERROR|AFTER-THE-ERROR'
$ echo BEFORE-THE-ERROR
BEFORE-THE-ERROR
```

Expectation 2 met, and the proof is the absence: `AFTER-THE-ERROR` is not on the
screen, so line 3 never ran. The daemon's own words survive into Lua with the
file and the line attached.

### The suite, with both new guards watched failing

```
$ cargo test --workspace --all-targets
test result: ok. 16 passed  (core invariants)
test result: ok.  4 passed  (daemon, over a real socket)
test result: ok.  8 passed  (live pty sessions)
test result: ok.  3 passed  (the Lua runtime)
test result: ok.  1 passed  (system clock)
```

A guard nobody has watched go red is a claim (PRINCIPLES.md §2), so each new one
was shown catching the thing it exists for:

```
planted: a seventh binding, table.set("smuggled", …)
  → the_bound_surface_is_exactly_the_protocols   FAILED
    a_script_reacts_to_what_a_session_shows      ok
    a_refusal_stops_the_script_instead_of_...    ok

planted: Response::Error returned as a string instead of raised
    the_bound_surface_is_exactly_the_protocols   ok
    a_script_reacts_to_what_a_session_shows      ok
  → a_refusal_stops_the_script_instead_of_...    FAILED
```

Each defect fails exactly one test, so neither is passing by accident of
something else. Expectation 3 met.

### Expectation 5, and what it actually rests on

Nothing in the Lua surface can write raw bytes or resize — but this was **not
re-tested here**, and saying so is the point. It is a composition of two facts
already proved elsewhere: the surface test above establishes that Lua reaches
the daemon only through the six bound names, and step 001 established that
`Request` has no raw-write and no resize variant. The claim is inherited, not
re-measured, and it would break the moment someone binds something that is not a
`Request` — which is the case the surface test exists to catch.

The same reasoning covers invariant 3: `remuda.send` compiles to the identical
`Request::SendLine` the CLI sends, so a script sending into a human-held session
is refused by the daemon, not by the script behaving.

### What the discipline caught this time: one bad error message, not a bug

Step 001 turned "it seems to work" into two live bugs. This step produced none —
every expectation held on the first run. The honest report is that the only thing
the captured-output requirement caught was cosmetic, and it caught it because the
output was *pasted* rather than described:

```
before:  [string "/tmp/rd-demo/oops.lua"]:2: in main chunk
after:   /tmp/rd-demo/oops.lua:2: in main chunk
```

Lua marks a chunk name as a filename with a leading `@`. Without it a traceback
*quotes* the path; with it, it *names* it — the form an editor and a person both
jump from. A one-character fix that a written summary ("the traceback names the
file and line") would have reported as already true.

Small, and worth recording as the weak case: the value of pasting output is not
only that it catches bugs. It is that it is the only version of the report that
can disagree with what you meant to say.

### No new principle

`PRINCIPLES.md` requires an incident before a principle, and nothing went wrong
here. The binding-set test is a guard, not a principle — it is the mechanism that
keeps §6 ("remove the dangerous operation") true as the API grows, and it is
recorded where it acts, in `BINDINGS`.
