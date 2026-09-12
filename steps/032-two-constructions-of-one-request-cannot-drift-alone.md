# 032 — two constructions of one request cannot drift alone

`Request::New` gained `cwd`/`env` fields on the wire (an earlier step) and the
daemon already applies both when it spawns the child process. What was
missing was a way for a caller to actually set them — and there turned out to
be two, separate, independently-written call sites that build a
`Request::New`, and both had quietly hardcoded `cwd: None, env: None` since
the wire fields were added.

## Before

- `native/src/script.rs`'s Lua binding, `remuda.new(name, argv)` — what
  `Request::Eval`, `-e`, the REPL, and any Lua script call — took exactly two
  positional arguments and built `Request::New` with `cwd: None, env: None`
  hardcoded in `new_request()`. No Lua syntax existed to set either field.
- `native/src/mcp.rs`'s MCP `new` tool — a SEPARATE code path, not a wrapper
  around the Lua binding. `new` is one of the five names in `TOOLS`
  (`mcp.rs:31`) that the MCP frame serves itself in Rust; per its own comment,
  `tools/call` only "dispatches into the image [Lua] for anything `TOOLS`
  does not name." An MCP client calling the typed `new` tool never reaches
  Lua at all — it hit this arm directly, which also hardcoded
  `cwd: None, env: None`, and its JSON schema declared no such properties.
- Nothing connected these two constructions to each other or to the
  `Request::New` struct itself. A field could be added to the wire type and
  reach one surface, both, or neither, and nothing would fail.

## Desired outcome

Both real callers — a Lua script, and an MCP client using the typed `new`
tool — can set `cwd` and `env` when starting a session, and a structural test
makes it impossible for the two constructions to silently disagree on which
fields they carry again.

## Expected

- `remuda.new(name, argv, cwd, env)`: two new trailing, optional positional
  Lua arguments. Purely additive — every existing two-argument call site
  keeps working unchanged, since Lua supplies `nil` for arguments a caller
  omits. `env` is a Lua table of string keys to string values, converted to a
  Rust `HashMap<String, String>` by a small helper (`lua_env_to_wire`,
  mirroring `lua_steps_to_wire`'s existing conversion style) — no such
  Lua-table-to-`HashMap` conversion existed anywhere in the tree before this;
  `remuda.tool(spec)` and `dir_bindings()`, the two closest-looking
  precedents, were checked and neither actually does this (`remuda.tool`
  never crosses a table's contents into Rust at all; `dir_bindings` never
  takes a table, only scalar strings).
- MCP's `new` tool: `cwd` (a string) and `env` (an object of string→string)
  added to its `inputSchema.properties`, and to the `Request::New`
  construction in its `call()` match arm. `new` STAYS a hard-coded, typed
  `TOOLS` entry — it is not removed from `TOOLS` to fall through to Lua. That
  would look like a root-cause fix and is a regression: the typed schema is
  what an MCP client reads for discovery, and dropping it trades a public
  contract for an internal tidiness nobody asked for.
- `env` stays additive, as it already was: the daemon's own environment, then
  a default `TERM` if unset, then the caller's map layered on top — nothing
  is cleared. This step does not touch `daemon.rs::spawn()` at all; that
  ordering was already correct.
- A new test, `the_new_tools_schema_matches_what_the_wire_actually_carries`
  (`native/tests/mcp.rs`), makes the two constructions structurally
  comparable instead of independently trusted: it serializes a real
  `Request::New` value with `serde_json::to_value` and reads the field names
  straight off the wire type (minus `size`, the one field that is
  server-computed rather than caller-supplied, named as a constant rather
  than a magic string), then queries the running daemon's own `tools/list`
  — the same way a real MCP client would — for the `new` tool's declared
  schema keys, and asserts the two sets are equal. A negative control (drop
  one field from the expectation, confirm the comparison then fails) rules
  out a vacuous pass, mirroring `the_bound_surface_is_exactly_the_protocols`'s
  existing both-directions pattern for the Lua table.
- Two end-to-end tests observe the **launched process's own view** — its own
  `pwd`, its own resolved environment variable — never the request/response
  round trip and never typed input a pty would just echo back
  (`PRINCIPLES.md` §4): both spawn `["sh", "-c", "pwd && echo $PROBE_VAR"]`
  directly as argv, so the output on screen is the process's own doing, not
  an echo of anything sent to it. `remuda_new_can_set_cwd_and_env_on_the_launched_process`
  (`native/tests/script.rs`) drives this through actual Lua source via
  `Request::Eval`; `a_tool_call_can_set_cwd_and_env_on_the_launched_process`
  (`native/tests/mcp.rs`) drives it through the MCP `new` tool. Before this
  step, `cwd` had exactly one such observation
  (`a_session_launches_into_the_cwd_it_is_given`, from an earlier step) and
  `env` had zero — `env`'s wire-level `spawn()` support had never been
  observed end-to-end by anything in the suite, on either surface.

## Actual

```
$ cargo test --workspace 2>&1 | grep -E "cwd_and_env|schema_matches|a_session_launches_into_the_cwd"
test a_session_launches_into_the_cwd_it_is_given ... ok
test the_new_tools_schema_matches_what_the_wire_actually_carries ... ok
test a_tool_call_can_set_cwd_and_env_on_the_launched_process ... ok
test remuda_new_can_set_cwd_and_env_on_the_launched_process ... ok

$ cargo fmt --all -- --check
(clean)

$ cargo clippy --workspace --all-targets -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.13s

$ python3 scripts/check-comments.py
ok — 307 doc comment(s) within cap (item 3, module 20)

$ python3 scripts/check-steps.py
ok — 31 step(s), each with before, desired, expected and captured actual

$ cargo check -p remuda-core --target wasm32-unknown-unknown
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.03s
$ git status --short -- core/
(empty — no core/ files touched)
$ git diff --stat -- native/src/daemon.rs
(empty — daemon.rs untouched, spawn()'s existing cwd/env application was already correct)
$ grep -n "TOOLS: \[" native/src/mcp.rs
pub const TOOLS: [&str; 5] = ["capture", "ls", "new", "run_script", "send"];
(unchanged — "new" stays a hard-coded, typed tool)
```

While writing the Lua end-to-end test, embedding the target directory's raw
path (`.display()`) directly into a Lua source string turned out to be
unsafe on Windows: a Windows path contains `\`, which Lua would parse as an
escape sequence inside a double-quoted string literal, breaking on
`test-windows` specifically. Fixed by escaping `\` and `"` before
interpolating the path into the Lua source — the same class of Windows-path
surprise this step's own predecessor
(`a_session_launches_into_the_cwd_it_is_given`) already hit once, from the
opposite direction (there, `pwd`'s own rendering of the path differed by
platform; here, the path being *sent in* had to survive being embedded as
Lua source).
