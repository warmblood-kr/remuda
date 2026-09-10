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

One screen and no prefix key. The list is always on the left and the selected
session is always on the right; what moves is **focus**. With focus on the list
the keys drive remuda; with focus on the session every key — `x` and `q`
included — is typed into the pty, and `Ctrl-\` brings focus back. The border
says which, always, which is the part tmux leaves in your head.

```
↑↓ select   ⏎ enter   n new   x kill   h/l pan a cropped preview   q quit
```

One key rather than a prefix, because a prefix exists to open a *namespace* and
there is exactly one command from inside a session. One keypress is also one
byte, so it carries no inter-key timing for a stack of nested ttys to mangle.
`Ctrl-\` is the key `remuda attach` already leaves by, not a second one.

⚠ A program that takes the whole screen and every key — vim, a nested tmux, an
agent's own TUI — can swallow `Ctrl-\` before remuda sees it. `remuda attach`
has always had that ceiling and this shares it.

A session whose program exits closes itself and leaves the list. Set
`REMUDA_KEEP_EXITED=1` in the daemon's environment to keep it listed as `dead`
instead — its last screen is the evidence for why it died.

remuda enters the alternate screen, so **leaving looks like leaving**: your own
scrollback and prompt come back, and a line says how to get back.

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

```powershell
$env:REMUDA_CHANNEL='nightly'; irm https://warmblood-kr.github.io/remuda/install.ps1 | iex
```

⚠ No stable release has been published yet. On this branch, the default
channel above falls back to `nightly` and prints which channel it actually
installed — see [PR #9](https://github.com/warmblood-kr/remuda/pull/9) for
the alternative under review, where the default instead fails with a message
pointing at the `nightly` one-liner.

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

The same herd is reachable over MCP — `new`, `ls`, `send`, `capture`,
`run_script` — so an agent can drive other agents. `attach` is deliberately
absent there: handing a real terminal to something that has none can only fail.

**The MCP server is a frame, not a fixed set.** `run_script` evaluates Lua in
that same image, and a Lua function marked exported becomes a real MCP tool —
listed with its arguments, callable — with no rebuild:

```lua
remuda.tool{
  name  = "idle_workers",
  about = "Every live session that has said nothing for a while, one per line.",
  args  = { seconds = "How idle counts as idle. Default 300." },
  run   = function(a)
    local out = {}
    for _, s in ipairs(remuda.ls()) do
      if s.alive and s.idle > (tonumber(a.seconds) or 300) then out[#out+1] = s.name end
    end
    return table.concat(out, "\n")
  end,
}
```

A word is callable and carries its own description, so the next tool is written
out of the last: `remuda.tools.idle_workers{seconds = "60"}` is a normal call
from any script. `wait_for` ships this way rather than in Rust, so the path is
exercised rather than merely present. [`steps/014`](steps/014-a-tool-registry.md)
has the ruling and the ceilings.

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
cargo test --workspace --all-targets                          # 96 tests
cargo check -p remuda-core --target wasm32-unknown-unknown    # the boundary gate
```

## License

MIT. Copyright (c) 2026 Warmblood Co., Ltd.
