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

pending — implementation follows
