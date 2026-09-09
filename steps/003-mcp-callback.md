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

pending — implementation follows.
