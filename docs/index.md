![A small herd of horses waiting in a rope corral at first light, with one
already saddled and stepped forward](remuda.jpg)

## tmux for coding agents

A terminal orchestrator with a programmable layer. You keep a herd of agent
sessions running, attach to one and ride it, swap to another. The herd outlives
any single ride.

[Source on GitHub](https://github.com/warmblood-kr/remuda)

### Programmable

The daemon holds one Lua interpreter for its whole lifetime, so a script keeps
state between calls and drives the same herd the CLI drives.

```lua
remuda.new("reviewer", {"claude"})
remuda.send("reviewer", "review the diff on this branch\n")
print(remuda.capture("reviewer"))
```

The same herd is reachable over MCP — `new`, `ls`, `send`, `capture` — so an
agent can drive other agents.

### Design notes

The reasoning behind each slice is written down as it was built, including the
predictions that turned out wrong:

- [PRINCIPLES.md](https://github.com/warmblood-kr/remuda/blob/main/PRINCIPLES.md)
  — the rules, each naming the mechanism that enforces it
- [steps/](https://github.com/warmblood-kr/remuda/tree/main/steps) — one
  document per slice: what was wanted, what was expected, what actually happened
