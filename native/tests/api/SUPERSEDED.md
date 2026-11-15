# Superseded frozen API fixtures

A frozen `vN.lua` is historical evidence: its assertions are never rewritten.
When a capability is deliberately removed, the fixture is renamed to
`vN.lua.superseded` (so the `*.lua` compatibility sweeps stop running it) and
a new `vN+1.lua` validates the current contract instead.

- `v15.lua.superseded` — per-session remote-input opt-out. The enforcement and
  the `remote-input` CLI were removed in #941; trusted peers are admitted and
  the flag is advisory. `v16.lua` covers the advisory behavior and the
  optional team metadata that replaces the old display-only path.
