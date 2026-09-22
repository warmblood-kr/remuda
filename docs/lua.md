# Lua extensions

Remuda keeps one Lua image for the lifetime of its daemon. Rust exposes the
small host surface, while Lua defines the extension vocabulary on top of it.
Every built-in and registered tool writes metadata to the same runtime
registry; the reference output is generated from that registry at request
time.

## Generate the reference

```sh
remuda doc
remuda doc --format markdown
remuda doc --format json
remuda extension info --format markdown
```

The default output is reStructuredText. `extension info` is an alias for
`doc`, and the JSON form is intended for project-site tooling and other
consumers that need structured metadata. No separate extension catalog is
maintained.

The runtime registry also covers words added with `remuda.tool`, so an
extension can document itself when it registers its function:

```lua
remuda.tool("review", "Review a session", function(name)
  return remuda.capture(name)
end, "review(name) -> string")
```

See [the design notes](design.md#one-registry-for-every-word) for the registry
model and [the project source](https://github.com/warmblood-kr/remuda) for the
current implementation.
