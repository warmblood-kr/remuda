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

## Use it

```sh
remuda                 # open the herd: a list on the left, the selected session on the right
remuda run claude      # start claude and ride it, in one act — the session names itself
remuda attach claude   # go back to one; Ctrl-\ detaches
remuda ls              # what is running
remuda send claude "don't merge #12 until the migration lands"
```

Five lines, and four of them are optional. A verb is on the command line only if
it **needs a terminal**, must **survive the shell's own quoting**, or **renders
for a human** in a way the Lua image cannot. Everything else — `new`, `close`,
`capture`, `insert`, `key`, `click` — is in Lua, where it costs no front page:

```sh
remuda -e 'remuda.close("build")'
```

Two modes, no prefix key and no config file. **browse** is the list beside a
read-only preview and the keys drive remuda; **ride** is the session owning the
whole terminal and every key going to the pty. `Ctrl-\` is the boundary, and it
is the same key `remuda attach` already used.

```
↑↓ select   ⏎ ride   n new   x kill   h/l pan a cropped preview   q quit
```

remuda enters the alternate screen while you ride, so **leaving looks like
leaving**: your own scrollback and prompt come back, and a line says which way
you left and how to get back.

## Install

**Linux and macOS** (x86_64, and Apple Silicon):

```sh
curl -fsSL https://warmblood-kr.github.io/remuda/install.sh | sh
```

It downloads a signed-by-checksum tarball, verifies it against the release's
`SHA256SUMS`, and lands `remuda` in `~/.local/bin`. Set `REMUDA_INSTALL_DIR` to
put it elsewhere.

**Windows** (x86_64), in PowerShell:

```powershell
irm https://warmblood-kr.github.io/remuda/install.ps1 | iex
```

Same shape: verified against `SHA256SUMS`, landed in `~\.local\bin`, and
`$env:REMUDA_INSTALL_DIR` moves it. The daemon speaks over a named pipe instead
of a unix socket, and the terminal handling is the console API instead of
termios; both live behind one seam, so there is one code path rather than two.

⚠ **What is proven on Windows, and what is not.** CI builds and runs the whole
suite on `windows-latest` — including tests that open real ConPTYs and drive the
shipped binary through raw mode and the Ctrl-\ detach. Nobody has yet run
`remuda attach` by hand in Windows Terminal or conhost, so the interactive
feel — resize behaviour, key handling under a real console host — is
**untested**, not merely undocumented. The TUI added in
[`steps/012`](steps/012-a-front-door.md) is in the same position and one step
further out: it has never been drawn on a Windows console at all. The port and
its evidence are in [`steps/010-windows.md`](steps/010-windows.md).

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
rather than two — and lands the new binary by rename, so upgrading while a daemon
is running does not disturb it.

⚠ **It does not disturb the daemon because it does not replace it.** A daemon
outlives every client on purpose, so after an upgrade the new binary is talking to
a daemon still running the old code, and the first request whose *shape* changed
fails. Every command asks the daemon which build it is and prints one line when
the answer is not its own — but a daemon started before that handshake existed
cannot answer, so the client names the skew from its own side instead, on the
request the old code could not read. Either way the cure is one verb, and it takes
the herd with it:

```sh
remuda restart        # names any live session and asks first; -f skips the ask
```

[`steps/013`](steps/013-a-daemon-that-says-which-build-it-is.md) has the incident,
and what each half does and does not cover.

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
-- new() answers with the name it gave the session. Pass nil for the name and
-- argv[0] supplies one, de-duplicated: claude, claude-2, claude-3.
local who = remuda.new(nil, {"claude"})
remuda.send(who, "review the diff on this branch")
print(remuda.capture(who))
```

`ls()` returns an **array** of rows, not a map — `{v, v}` is a list in Lua and
`{k = v}` is a map, and this is the first table a script author meets:

```lua
-- { [1] = row, [2] = row }.  Each row is a table with string keys.
for _, s in ipairs(remuda.ls()) do
  if s.alive and s.idle > 300 then
    remuda.send(s.name, "status?")
  end
end
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
cargo test --workspace --all-targets                          # 90 tests
cargo check -p remuda-core --target wasm32-unknown-unknown    # the boundary gate
```

## License

MIT. Copyright (c) 2026 Warmblood Co., Ltd.
