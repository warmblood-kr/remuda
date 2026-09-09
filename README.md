# remuda

**re·mu·da** \ ri-ˈmü-də, -ˈmyü- \ — *the herd of horses from which those to be
used for the day are chosen.* From American Spanish, "relay of horses"; from
`remudar`, to exchange.

An orchestration core for coding-agent sessions. You keep a herd running, you
attach to one and ride it, you swap to another. The herd outlives any single
ride.

## Status

First slice. Three joints are here because they cannot be retrofitted later:

- **The agent lives behind a trait.** A scripted double and a different vendor
  are the *same* substitution, so one seam buys both. The entire test suite runs
  on a machine that has never installed a coding agent, opened a pty, or touched
  the network.
- **The clock is injected.** Nothing calls a global clock. Idle timeouts and
  interleaving are testable without sleeping.
- **Body-and-Enter cannot be split.** Not by discipline — by the shape of the
  API. There is no public raw write, and `Session` has no `resize` method at
  all, so an attaching viewer *cannot* shrink the pty out from under a running
  agent.

## Layering

```
  policy / orchestration      pure. no syscalls. compiles to wasm32.
  ---------------------- AgentProcess ------------------------------
  host: pty, processes, terminal emulation   native only, `native` feature
```

The boundary is checked by the compiler:

```sh
cargo check --target wasm32-unknown-unknown --no-default-features
```

That gate is real but **partial**, and the measurement is recorded in
`Cargo.toml`: it catches `std::os::unix::*` and host crates that cannot build
for wasm — the axis a pty crate sits on — and it does *not* catch a bare
`Command`, `fs`, or `Instant` call, because wasm32 ships std stubs that compile
and fail only at runtime. Closing that second axis needs a lint, not a target.

## Build

```sh
cargo test                                                    # 9 invariants
cargo check --target wasm32-unknown-unknown --no-default-features
```

## License

MIT. Copyright (c) 2026 Warmblood Co., Ltd.
