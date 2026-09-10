# remuda

![A small herd of horses waiting in a rope corral at first light, with one
already saddled and stepped forward](docs/remuda.jpg)

**tmux for coding agents** — a terminal orchestrator with a programmable layer.

**re·mu·da** \ ri-ˈmü-də, -ˈmyü- \ — *the herd of horses from which those to be
used for the day are chosen.* From American Spanish, "relay of horses"; from
`remudar`, to exchange.

An orchestration core for coding-agent sessions. You keep a herd running, you
attach to one and ride it, you swap to another. The herd outlives any single
ride.

## Install

**Linux and macOS** (x86_64, and Apple Silicon):

```sh
curl -fsSL https://warmblood-kr.github.io/remuda/install.sh | sh
```

It downloads a signed-by-checksum tarball, verifies it against the release's
`SHA256SUMS`, and lands `remuda` in `~/.local/bin`. Set `REMUDA_INSTALL_DIR` to
put it elsewhere.

**Windows: not yet.** The binary does not compile for it — the daemon speaks
over a unix socket and the terminal handling is termios and `TIOCGWINSZ`. That
is real porting work, not a build flag, and the compiler's own verdict is
recorded in [`steps/009-versioning-and-install.md`](steps/009-versioning-and-install.md).
There is deliberately no `install.ps1`: an installer for a binary that cannot
exist is worse than none.

### Channels

Two, in the shape rustup uses:

| channel | built from | version string |
|---|---|---|
| `stable` (default) | a `vX.Y.Z` tag | `0.1.0` |
| `nightly` | every commit on `main` | `0.1.0-nightly.20260910.abc1234` |

```sh
curl -fsSL https://warmblood-kr.github.io/remuda/install.sh | REMUDA_CHANNEL=nightly sh
```

The chosen channel is remembered in `$XDG_DATA_HOME/remuda/channel`, so
upgrading stays on the track you picked:

```sh
remuda upgrade                      # follow the channel you installed
remuda upgrade --channel stable     # switch tracks
```

`remuda upgrade` re-runs that same install script — one download-and-verify path
rather than two — and lands the new binary by rename, so upgrading while a
daemon is running is safe.

Every command checks [`latest.json`](https://warmblood-kr.github.io/remuda/latest.json)
at most once a day and prints one line to stderr when you are behind. The fetch
happens in a detached child that no command waits for, so a slow or absent
network costs nothing; the cost is that a notice can arrive one run late.
`REMUDA_NO_UPDATE_CHECK=1` turns it off.

## Programmable

The daemon holds **one Lua interpreter for its whole lifetime**, so a script is
not a one-shot subprocess. It keeps state between calls and drives the same herd
the CLI drives.

```lua
remuda.new("reviewer", {"claude"})
remuda.send("reviewer", "review the diff on this branch\n")
print(remuda.capture("reviewer"))
```

The same herd is reachable over MCP — `new`, `ls`, `send`, `capture` — so an
agent can drive other agents. `attach` is deliberately absent there: handing a
real terminal to something that has none can only fail.

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
cargo check -p remuda-core --target wasm32-unknown-unknown
```

That gate is real but **partial**, and the measurement is recorded in
`Cargo.toml`: it catches `std::os::unix::*` and host crates that cannot build
for wasm — the axis a pty crate sits on — and it does *not* catch a bare
`Command`, `fs`, or `Instant` call, because wasm32 ships std stubs that compile
and fail only at runtime. Closing that second axis needs a lint, not a target.

## Build

```sh
cargo test --workspace --all-targets                          # 55 tests
cargo check -p remuda-core --target wasm32-unknown-unknown    # the boundary gate
```

## License

MIT. Copyright (c) 2026 Warmblood Co., Ltd.
