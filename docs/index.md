![A small herd of horses waiting in a rope corral at first light, with one
already saddled and stepped forward](remuda.jpg)

## tmux for coding agents

A terminal orchestrator with a programmable layer. You keep a herd of agent
sessions running, attach to one and ride it, swap to another. The herd outlives
any single ride.

### Programmable

The daemon holds one Lua interpreter for its whole lifetime, so a script keeps
state between calls and drives the same herd the CLI drives.

```lua
remuda.new("reviewer", {"claude"})
remuda.send("reviewer", "review the diff on this branch\n")
print(remuda.capture("reviewer"))
```

The complete Lua reference is generated from the live runtime registry:
[Lua extension documentation](lua.md). Butler's local session manager and
optional Matrix bridge are documented on the [remuda-butler project
site](https://warmblood-kr.github.io/remuda-butler/).

The [generated Lua reference](lua-reference.html) is rendered on the Lua page;
the [RST source](lua-reference.rst) is also available to documentation tools.

The same [Lua extensions page](lua.md) documents the live mod inventory
exposed by `remuda mod list`.

The same herd is reachable over MCP — `new`, `ls`, `send`, `capture` — so an
agent can drive other agents.

### Design notes

The reasoning behind each slice is written down as it was built, including the
predictions that turned out wrong:

- [PRINCIPLES.md](https://github.com/warmblood-kr/remuda/blob/main/PRINCIPLES.md)
  — the rules, each naming the mechanism that enforces it
- [design.md](design.md) — the current shape of the scripting surface, kept
  in present tense and rewritten in place as it changes
- [steps/](https://github.com/warmblood-kr/remuda/tree/main/steps) — one
  document per slice: what was wanted, what was expected, what actually happened
