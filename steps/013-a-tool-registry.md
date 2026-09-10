# 013 — a tool registry: the MCP server becomes a frame

An MCP tool is a Lua function marked exported. `run_script` is the door for
everything that does not have a name yet. And the comment that refused an eval
tool is replaced rather than deleted, because its **premise** was corrected.

## Before

`mcp.rs` served four tools — `capture`, `ls`, `new`, `send` — hard-coded in
`descriptors()` and dispatched by a `match` in `call()`. Adding a fifth meant
editing Rust, rebuilding, and shipping a binary. And at `mcp.rs:141-145` a
comment refused an eval tool outright:

> `TOOLS` exposes no `eval`, deliberately: **MCP is the door for the agent
> running *inside* a session**, and handing that agent the image would let it
> rewrite the manager holding it. … adding an eval tool becomes **a decision
> made here instead of one inherited**.

That premise was wrong. 정수님, 2026-09-10 (matrix `butler-x600`, thread
`$9j8AGs0LRaasmj0WbyNMu0D-RI0YEzqD2aGrdVYBc4g`), typed:

> 「아, 이건, ramuda를 제어하는 **외부의 어느 에이전트**를 말한거였어요. ramuda
> 내부의 세션들을 염두에 둔건 아니었어요.」

and then settled the question the comment had left open:

> 「**내부에서 스스로를 변경할 수 있어도 돼요.** 제 생각엔. … 저는, 바깥에서도,
> 안쪽에서도 RunScript 부를 수 있어도 될 것 같은데. **claude-code-ide.el 도, emacs
> eval을 할 수 있게 해줘요.**」 … 「작업용 데몬과 제어용 데몬을 분리할 필요 없을 것
> 같아요.」

and named the shape this step builds:

> 「claude-code-ide도 emacs-tools 라는 **기본 틀을 제공하는 것처럼**, 그래서 거기서
> **함수 추가해가면서 구동할 수 있는 것처럼**, 일종의, MCP server는 제공을 하고,
> **필요에 따라서 tool을 추가해나갈 수 있도록.**」

The design this replaces (an earlier §3.7) shrank MCP to `RunScript` alone, and
its stated cost was **discoverability**: with one tool, an agent has no way to
learn what it may call, so the function list has to be crammed into a tool
description. That is a document, not an interface. A registry removes the cost.

## Desired outcome

1. `run_script` evaluates Lua in the daemon's one long-lived image — the same
   interpreter `remuda -e`, a script and the REPL share. One daemon; no
   control/worker split. Reachable from outside remuda and from inside a
   session alike.
2. A Lua function marked exported appears in `tools/list` with its name,
   description and arguments, and `tools/call` dispatches into it — with **no
   rebuild** between defining it and calling it.
3. The Lua surface is a **vocabulary that accumulates**: each word is callable
   and carries its own description, so the next tool can be written out of the
   last rather than starting from the primitives again.
4. At least one real tool ships *through* the registry rather than in Rust, so
   the path is exercised end to end instead of merely present.
5. The eval-refusal comment is **replaced**, not deleted — a silent deletion
   leaves the next reader obeying a rule that no longer exists.

**Not in scope, by ruling.** Self-modification is allowed and must not be
blocked, so no guard was added — a caller may redefine `remuda.send` and this
step does nothing about it. Making such a redefinition *visible* (Emacs solves
the same property with `describe-function` telling you about advice, not with a
wall) is a direction, not yet a design. The seam it would hook into is named
under **Actual** below.

## Expected

- `tools/list` on a fresh daemon returns `TOOLS` + `wait_for`, each with an
  object `inputSchema` and a description a model can choose from.
- A tool defined through `run_script` in one `remuda mcp` process is listed and
  callable from a **second, separate** `remuda mcp` process — because the
  registry lives in the daemon's image, not in the MCP process.
- A required argument that is omitted is refused; an optional one may be left
  out. An unknown tool is `isError: true` with the registry's own words.
- An argument string is escaped into the generated Lua, not interpolated: a
  value that is Lua source must come back as text, not run.
- `wait_for` answers with a matching screen and **fails** on a deadline rather
  than answering with a screen that does not match.
- `remuda.tool` refuses a spec with no name, no `run`, an `about` too short to
  choose from, or a `needs` naming an argument it never describes.
- `run_script` with empty `code` is refused: `load("")` succeeds and returns
  nothing, which would answer an obvious mistake with a successful empty result.

## The Lua surface

```lua
remuda.tool{
  name  = "wait_for",
  about = "Wait until a session's screen matches a Lua pattern …",
  args  = { session = "The session to watch.", pattern = "A Lua pattern …" },
  needs = { "session", "pattern" },   -- everything else is optional
  run   = function(a) … end,
}
```

`remuda.tool` returns the word and files it at `remuda.tools[name]`. A word is
callable — `remuda.tools.wait_for{session = s, pattern = p}` is a normal call
from any script, `-e`, or REPL line — and it carries `name`, `about`, `args`,
`needs` and `run` as fields. That is what makes this a vocabulary rather than a
side table of closures: the next tool is written out of the last one.

The frame is `native/src/tools.lua`, loaded into the image at startup. Written
in Lua rather than Rust on purpose — building the frame's own vocabulary in the
host would be doing there the job the guest was embedded to do, the same
argument that put `steps/005`'s polling helper in Lua.

## Actual

The frame's list, from the real binary over stdio:

```
$ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize",…}' \
                '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | remuda mcp
serverInfo: {'name': 'remuda', 'version': '0.1.0'}
  capture    | ['session']                     | required= ['session']
  ls         | []                              | required= None
  new        | ['command', 'name']             | required= None
  run_script | ['code']                        | required= ['code']
  send       | ['session', 'text']             | required= ['session', 'text']
  wait_for   | ['pattern', 'seconds', 'session'] | required= ['session', 'pattern']
```

**A tool defined in one MCP process, used by another.** Process A defines
`herd` through `run_script` and exits. Process B is a fresh `remuda mcp` that
never saw the definition — the registry is in the daemon's image, so it is
already there:

```
=== process A: define a tool over MCP ===
OK  registered tool herd

=== process B (a second, separate mcp process) ===
TOOLS: capture ls new run_script send herd wait_for
OK  agent
OK  ok
OK  echo $((6*7))-live-mcp
    $ 42-live-mcp
    $
OK  agent idle 0s
```

`42-live-mcp` rather than an echo of the question, per principle 4. The fourth
reply is `wait_for` — a tool defined in Lua — answering with the screen it
waited for; the sixth is `herd`, defined at runtime over MCP minutes earlier.

The three refusals from the same process B, in the same order they were asked:

```
ERR runtime error: agent never matched "never%-appears" within 0.4s. last screen:
    echo $((6*7))-live-mcp
    $ 42-live-mcp
ERR runtime error: wait_for needs `pattern`
ERR runtime error: no such tool: no_such_thing
```

**Inside and outside alike (ruling ②).** From a shell running *inside* a remuda
session, through the same `remuda mcp` door, redefining base vocabulary:

```
$ sh /tmp/…/inside.sh 2>&1 | tail -c 400
{"id":1,"jsonrpc":"2.0","result":{"content":[{"text":"redefined remuda.send from
 inside, and nothing stopped me","type":"text"}],"isError":false}}

$ remuda -e 'return "send is now a Lua closure: " .. tostring(remuda.send)'
send is now a Lua closure: function: 0x7f64ac0197f0
```

Nothing blocked it, which is the ruling. Nothing recorded it either, which is
the open direction — **the seam is `remuda.tool`'s neighbourhood in
`tools.lua`**: it is already the one place a word is filed, so a
`remuda.tools` sibling table holding `{previous, when, by}` would cover words.
Base bindings (`remuda.send`) are not filed anywhere, and the cheapest hook for
those is an `__newindex` metamethod on the `remuda` table itself, set in
`tools.lua` right after the bindings exist and before any caller runs. Neither
was built.

### The suite, and the mutants

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result|Running"
     Running unittests src/lib.rs                (core)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/invariants.rs
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/keys.rs
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/names.rs
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running unittests src/lib.rs                (native, incl. the Lua-literal encoder)
test result: ok. 36 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running unittests src/bin/remuda.rs
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/daemon.rs
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/live_sessions.rs
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/mcp.rs                        ← 4 → 7
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/script.rs
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Principle 2: a gate that has never gone red is a claim. Four mutants planted,
watched fail, reverted.

```
① tools/list ignores the registry     → 3 red (surface, registry, stdio count)
② _call skips the required-arg check  → 1 red (registry)
③ wait_for answers the last screen
   instead of failing on the deadline → 1 red (wait_for's negative control)
④ lua_string stops escaping `"`       → the process DIED mid-test:

     running 1 test
     (no further output, no `test result` line)
```

④ is the one worth reading twice. The hostile argument in that test is
`" .. os.exit() .. "`. With the escape removed it stopped being an argument and
became code, and `os.exit()` took the whole test binary down — the daemon runs
in a thread of it. So the escaping is not decoration: it is what stands between
an MCP client's argument and the image.

### Every gate, run here

```
$ python3 scripts/check-comments.py
ok — 162 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-steps.py
ok — 13 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-principles.py
ok — 14 principles, every named mechanism exists (14 CI jobs, 11 denied paths, 188 test fns seen)
$ python3 scripts/check-workflows.py
ok — 3 workflow file(s) parse, 14 job(s) defined
$ python3 scripts/check-install.py
ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
$ cargo fmt --all --check && echo $?
0
$ cargo clippy --workspace --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.49s
$ cargo check -p remuda-core --target wasm32-unknown-unknown
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.16s
```

`v1.lua` still runs unedited: `remuda.tool`, `remuda.tools`, `_call` and
`_descriptors` are **additions** to the `remuda` table, and the frozen script
reaches only for the names it froze. `BINDINGS` grew from 10 to 14, and
`the_bound_surface_is_exactly_the_protocols` asserts the live table against it
in both directions, negative control included.

No new CI gate was added, so `gates-can-fail` gains no step — principle 2 is
satisfied by there being nothing new to control. Its existing Lua-rename plant
targets `script.rs`'s `"insert"`, which this change leaves alone.

## Known ceilings

Named, not glossed.

- **Every argument is typed `string` in the emitted schema.** MCP clients send
  strings anyway, and `wait_for` says so where it means a number
  (`tonumber(a.seconds) or 30`). A tool wanting a real number or array in its
  schema cannot ask for one yet. The fix is a type in `args`; nothing needed it.
- **A registry error arrives with an mlua traceback stapled to it.** `error(…, 0)`
  drops Lua's position prefix, not mlua's traceback, so `no such tool: x` is
  followed by four lines of `[C]: in function 'error'`. It is the daemon's
  existing `Eval` error path and pre-existing for every Lua error; a model reads
  the first line. Not touched here.
- **`wait_for` blocks the image for its whole wait.** `image.rs`'s header
  already says a long script makes the *next Lua caller* wait — the pty pumps
  and `remuda ls` keep going. But a 30-second `wait_for` over MCP now makes that
  the ordinary case rather than an odd script. If it bites, the fix is not in
  this layer: it is the `on_output` job-posting the header reserves.
- **`tools/list` swallows a broken image.** If the daemon cannot be reached or
  `_descriptors()` raises, the listing quietly falls back to the frame's five.
  `tools/list` has no per-tool error channel, and the same failure is loud on
  the next `tools/call` — but a client that only lists sees a short list, not an
  error.

## Not verified

- **No real MCP client has connected.** Every exchange above was hand-written
  JSON-RPC into `remuda mcp` over a pipe. Claude Code, Cline, and the inspector
  have not been pointed at it, so schema quirks a real client cares about
  (`outputSchema`, `structuredContent`, notification of a changed tool list)
  are untested. In particular **`notifications/tools/list_changed` is not sent**
  when the registry changes — a client that lists once will not learn about a
  tool defined afterwards until it lists again.
- **Nothing on Windows.** The suite runs there in CI, this path included, but
  no `remuda mcp` process has been driven by hand on a console host.
- **Concurrent `run_script` callers.** The image is one thread serving a queue,
  so calls are serialised by construction; that was reasoned about, not raced.
- **A tool that outlives a daemon restart.** The registry lives in the image,
  so it dies with the daemon. Nothing persists or re-loads it, and no
  `~/.config/remuda/init.lua` exists yet — that is the obvious next step and is
  deliberately not in this one.
