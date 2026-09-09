# 003 — the manager, reachable from inside a session

정수님, 2026-09-10 00:56: *"그 다음에는, claude-code-ide.el 같은, 그 pty manager에서
claude code를 구동하고, pty manager와 mcp 등으로 연결하는 장치를 두고."*

And, said *before* the build order and easy to file under "later" — it is not:

정수님, 2026-09-10 00:37: *"우리 remuda도 그렇게 여러 노드간의 클러스터를 염두에 두면
좋겠어요. … 꼭 데몬을 구동하여 실제 세션들을 가지고 있지 않은, 순수 리모트 터미널
단말로서의 노드 — 웹 단말이 될 수도 있고 — 가, A노드, B노드 어느 노드던지 터미널
세션을 가져와서 실행하고 다시 detach할 수 있도록."*

## Before

Step 002 gave a script *outside* the manager the five operations. The program we
actually want to run **inside** a session — claude code — still cannot see the
manager at all. It has a terminal and nothing else. It cannot ask what other
sessions exist, cannot start one, cannot read what another is showing.

That gap is the whole reason cc-butler needs Emacs today. Emacs is doing two
unrelated jobs at once: it hosts the ptys, *and* it runs an MCP server that the
Claude inside a session calls back into (`list_claude_sessions`,
`send_to_session`, `read_session_output`). Steps 001–002 replaced the first job.
Take Emacs away right now and the second job disappears with it — the sessions
survive, and the orchestration does not.

So the manager is invisible from the inside, and that is the last thing holding
the old host in place.

## Desired outcome

`remuda mcp` speaks MCP over stdio and exposes the protocol operations as tools.
A claude code running inside a remuda session, configured with that server, can
list sessions, start one, send a line, and read a screen — the same vocabulary
the CLI and Lua already have, reached from a third direction.

The property that matters is the same one that mattered in 002, and it should
cost nothing again:

**A third surface must not widen the API.** The tools are exactly `Request`.
There is no raw write and no resize to expose, because the type has no such
variant. Invariant 3 is still enforced by the daemon at call time, so a session
that tries to type into a human-held session is refused exactly as the CLI is —
including when it is an LLM doing the trying, which is the case this layer
actually introduces.

A refusal must arrive as an **MCP error**, not as a successful result with empty
content. This is the same failure the shell had (`$(remuda capture nosuch)` → `""`)
and that 002 fixed for Lua, and it matters more here: a model reading an empty
capture will narrate a plausible story about an idle session rather than stop.

### The constraint I am keeping, without building it

The cluster line above is not a later layer's problem. It says a node with **no
sessions of its own** must be able to reach a session on another node. That does
not mean building a transport now — it means not writing anything that makes it
harder later. Concretely, one thing:

**A session name is an address.** Today it resolves to "that name on the local
daemon". `Request::Capture { name }` already carries it as opaque data, so the
day a name can be qualified by a node, nothing in the protocol has to change
shape — only the resolver. So this step adds no second addressing scheme, no
implicit "current session", and no tool that means something different depending
on where it runs.

I am stating this because 002 filed the multi-node question under "the day a
script arrives from another node", and re-reading the source shows he raised it
**before** giving the build order. Deferring the work was right; filing it as
someone else's layer was not.

### What is deliberately not built

- **No network transport.** stdio only. The tunnel and the node registry are
  their own step, with their own trust boundary.
- **No special way to launch claude code.** `remuda new alpha claude` already
  works — this step is *only* the callback. If the answer needed a bespoke spawn
  path, the pty layer would be wrong.
- **Not the whole MCP spec.** `initialize`, `tools/list`, `tools/call`. Anything
  a real client turns out to need gets added when a real client fails without it,
  not before.
- **No self-send guard.** A session can name itself and type into its own
  terminal. That is a strange thing to do, not a dangerous one, and inventing a
  rule against it now would be forbidding an operation rather than removing one —
  the mistake principle 6 exists to prevent. If it turns out to bite, it bites
  visibly and gets an incident.

## Expected

1. `remuda mcp` answers `initialize` with a protocol version and server info, and
   a client can complete the handshake.
2. `tools/list` returns exactly the protocol operations — asserted against a
   declared constant in both directions, the same shape as `script::BINDINGS`, so
   a tool added without deciding to fails the suite.
3. `tools/call` on `capture` returns the live screen of a running session; on
   `new`/`send` it takes effect on that session and the effect is visible from a
   second, independent read.
4. A call naming a session that does not exist comes back as an MCP **error**,
   and carries the daemon's own words.
5. Nothing in the tool surface can write raw bytes or resize — inherited from
   `Request` having no such variant, exactly as in 002, and re-broken only by
   binding something that is not a `Request`.

## Actual

### The manager, reached from a client that is not a person

A real `remuda mcp` process, driven by JSON-RPC on stdin. The session was
started separately (`remuda new demo -- sh`) — this step added no spawn path.

```
$ { initialize; notifications/initialized; send; capture; capture-a-typo; } | remuda mcp

id=1  initialize → remuda 0.0.0, protocol 2025-06-18
id=2  isError=False  ok
id=3  isError=False  ['42-from-a-model', '$ ']
id=4  isError=True   no such session: typo
```

Expectations 1, 3 and 4 met. `42` is arithmetic the shell performed, so it
cannot be the pty echoing the command back (PRINCIPLES.md §4) — the send really
landed and the capture really read the result of it. Four ids in, four replies
out: the notification drew none, which is the one place an extra line would
desynchronise the stream for every message after it.

### The surface, and the tool that is deliberately absent

```
$ tools/list  →  capture · ls · new · send
```

Four, not five. `attach` is missing on purpose and the test says so in words:
attach exists to hand a terminal to a human, an MCP client has no terminal, and
the stdio it would seize is the JSON-RPC channel itself. Its precondition cannot
be met here, so exposing it would be exposing something that can only fail. That
is not principle 6 being bent — nothing was forbidden; an operation simply has no
meaning on this transport, and `TOOLS` records the decision so it stays one.

Expectation 2 met, in both directions, the same shape as `script::BINDINGS`.

### The suite, with every new guard watched failing

```
$ cargo test --workspace --all-targets
test result: ok. 16 passed  (core invariants)
test result: ok.  4 passed  (daemon, over a real socket)
test result: ok.  8 passed  (live pty sessions)
test result: ok.  3 passed  (the Lua runtime)
test result: ok.  4 passed  (MCP)
test result: ok.  1 passed  (system clock)
```

Each new guard was shown catching the thing it exists for:

```
planted: a fifth tool, "smuggled", in descriptors()
  → the_tool_surface_is_exactly_the_decided_set   FAILED
  → the_real_binary_completes_a_handshake_over_stdio FAILED
    a_refusal_is_an_error_not_an_empty_success    ok
    a_tool_call_moves_a_real_session              ok

planted: Response::Error returned as tool_text (success) instead of tool_error
  → a_refusal_is_an_error_not_an_empty_success    FAILED
    (other three ok)

planted: a notification answered anyway (id.unwrap_or(Null) instead of id?)
  → the_real_binary_completes_a_handshake_over_stdio FAILED
    (other three ok)
```

The first defect fails **two** tests, and that is worth stating rather than
tidying: the stdio test counts the served tools against `TOOLS` independently, so
the surface claim has two witnesses that do not share a code path. The other two
defects fail exactly one each.

### Expectation 5, inherited again — and invariant 3 with it

Nothing in the tool surface can write raw bytes or resize. Not re-tested here;
it is the same composition as 002. The surface test establishes that MCP reaches
the daemon only through the four names, and step 001 established that `Request`
has no such variant to bind. The claim breaks the moment someone builds a tool
that is not a `Request`, which is what the surface test exists to catch.

Invariant 3 is inherited the same way and I want to be exact about it, because
the module docs make a claim that sounds tested and is not: `send` here compiles
to the identical `Request::SendLine` the CLI sends, so a session typing into a
human-held session is refused by the daemon
(`daemon::orchestrated_input_is_refused_while_a_human_is_attached`). What is new
in this layer is only *who* can now attempt it. That is an argument, not a
measurement, and it is written down as one.

### What the discipline caught this time: a test that would have passed while testing nothing

Step 001 turned "it seems to work" into two live bugs; 002 caught one bad error
message. This step caught something worse than either, and it was caught by
writing the test before believing it.

The stdio test needs the child process to talk to *our* daemon. I wrote
`.env("REMUDA_SOCKET", &path)` from memory. There is no such variable — the
binary resolves its socket from `REMUDA_RUNTIME_DIR`, and joins `remuda/` and
`default.sock` onto it:

```
daemon.rs:29-38   REMUDA_RUNTIME_DIR → XDG_RUNTIME_DIR → /tmp/remuda-$USER
                  … then .join("remuda").join("default.sock")
```

With the wrong variable name the child would have ignored the test daemon
entirely and connected to **the developer's own running daemon**. The test would
have gone green — the handshake succeeds against any daemon — while asserting
nothing about the code under test, and its last check (that our socket is still
answering) would have been true for an unrelated reason. `a-control-that-cannot-
fail-is-not-a-control`, in the test suite rather than the code.

What found it was not care. It was refusing to write an env var from memory and
going to read `socket_path` instead. The general form is the same one that keeps
recurring across this repo: **the thing you are confident about is the thing you
did not measure**, and confidence about your own prior work is the least examined
kind.

### One thing 002 got wrong about scope, now corrected

002's non-goals said the sandbox question "changes the day a script arrives from
another node over the transport layer" — filed as a later layer's problem.
Re-reading the source shows 정수님 raised the cluster shape **before** giving the
build order, and asked for it to be kept in mind from the start:

> *"우리 remuda도 그렇게 여러 노드간의 클러스터를 염두에 두면 좋겠어요. … 꼭 데몬을
> 구동하여 실제 세션들을 가지고 있지 않은, 순수 리모트 터미널 단말로서의 노드가,
> A노드, B노드 어느 노드던지 터미널 세션을 가져와서 실행하고 다시 detach할 수 있도록."*

Deferring the *work* was right. Filing it as someone else's layer was not, and it
is the sort of error that only shows up as a rewrite. This step builds no
transport and keeps one invariant instead: a session name is an address, opaque
to the protocol, so qualifying it by node later changes the resolver and not the
shape of any message. No second addressing scheme, no implicit "current
session", no tool whose meaning depends on where it runs.

### No new principle

`PRINCIPLES.md` requires an incident, and the env-var slip is one — but it is an
instance of §2 (a guard nobody watched go red is a claim), not a new rule. It is
recorded here, where it happened, and the four planted defects above are the §2
practice that would have caught it in any case.
