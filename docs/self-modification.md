# Self-modification

A running agent can change the environment it runs in without a rebuild or a
daemon restart. The daemon holds one Lua interpreter (the *image*) for its
whole lifetime; everything below is a way to change that image from a session
it hosts, and a note on whether the change outlives the daemon.

Signatures and one-line descriptions for every word come from the live
registry: `remuda doc`, or the [generated reference](lua-reference.html).

## The doors

| Door | What it does |
|---|---|
| MCP `run_script` | Evaluates Lua in the image. An expression answers with its value. |
| `remuda -e CODE` | The same, from a shell: one chunk in the same image. |
| `remuda repl` | The same image, a line at a time. |
| `remuda lua SCRIPT.lua` | Runs a script file in the image. |

All four reach the same interpreter, so a definition made through one is
visible through the others. On the local socket the daemon applies no caller
check before evaluating; see [Boundaries](#boundaries).

## Add or replace an MCP tool

`remuda.tool{...}` defines a word and exports it as an MCP tool. `tools/list`
reflects the registry, so the tool is listed and callable with no rebuild:

```lua
remuda.tool{
  name  = "idle_workers",
  about = "Every live session that has printed nothing for a while, one per line.",
  args  = { seconds = "How idle counts as idle. Default 300." },
  needs = {},
  run   = function(a)
    local out = {}
    for _, s in ipairs(remuda.session.list()) do
      if s.alive and s.output_idle > (tonumber(a.seconds) or 300) then out[#out + 1] = s.name end
    end
    return table.concat(out, "\n")
  end,
}
```

- The argument is one table. `name`, `run` and an `about` of at least 20
  characters are required; `args` maps each argument name to a description,
  and `needs` lists the arguments that are not optional.
- Defining a name again replaces the earlier word. Do not redefine a tool
  that a mod declares: the mod no longer owns it, and its next reload is
  refused with `tool NAME is already registered`.
- A word is also an ordinary Lua call: `remuda.tools.idle_workers{seconds = "60"}`.

## Wrap an existing function

`remuda.advise(path, how, fn, {id, depth})` wraps the function stored at a
`remuda.*` path; `remuda.unadvise(path, id)` removes that one piece of advice.

```lua
remuda.advise("remuda.send", "before", function(name, text)
  print("send -> " .. name)
end, { id = "log-sends" })

remuda.unadvise("remuda.send", "log-sends")
```

- `how` is one of `around`, `before`, `after`, `override`, `filter_args`,
  `filter_return`, `before_while`, `before_until`.
- `id` is required. Advising the same path with the same `id` replaces it.
- Removing the last advice on a path restores the original function.
- Only functions stored under `remuda` have a path. A Lua `local` cannot be
  advised.
- `remuda.advice_list(path?)` and `remuda.advice_member(path, id)` inspect
  what is installed.
- Advice sees calls made from Lua. The MCP `send` tool and the `remuda send`
  command go to the daemon directly and do not pass through `remuda.send`.

The full table of kinds, `depth`, and how a mod declares advice are in
[Advice](lua.md#advice).

## Reload a mod

`remuda.reload(NAME)` re-reads an installed mod's files and replaces its
running copy in the image. The mod's state table is kept and passed through
its `migrations` when `state_version` went up; what the mod declared (hooks,
tools, schedules, advice, among others) is replaced by the new declaration.

```sh
remuda -e "remuda.reload('NAME')"   # files already changed on disk
remuda mod update NAME --reload     # fetch the recorded source, then reload
remuda mod install OWNER/REPO --reload
```

- Reload needs a mod that declares `lifecycle = "remuda-module-v1"`. A legacy
  mod is refused. `remuda exec NAME` reruns its entry file, which can repeat
  registrations it already made; otherwise restart the daemon and load the
  mod again.
- `state_version` cannot move backwards, and each version step needs a
  migration.
- `remuda mod update --all --reload` is disabled. Reload mods one at a time.
- Without `--reload`, an install or update changes only the files. The image
  keeps its old definitions until a `remuda.reload`, or a restart followed by
  loading the mod again. `exec` on a lifecycle mod that is already active
  does nothing.
- There is no file watcher. Nothing is reloaded until something asks.

The declaration format is in [In-process mod lifecycle](lua.md#in-process-mod-lifecycle).

## Volatile or persistent

| Change | Where it lives | Survives a daemon restart |
|---|---|---|
| Globals, tools and advice defined through a door above | daemon memory | no |
| The same definitions written in `~/.config/remuda/init.lua` | disk, evaluated once at daemon boot | yes |
| A mod's declared hooks, tools, schedules and advice | the mod's files under `${XDG_DATA_HOME:-~/.local/share}/remuda/mods/NAME` (Windows: `%LOCALAPPDATA%\remuda\mods\NAME`) | once the mod is loaded again |
| A lifecycle mod's state table | daemon memory | no; it is kept across `remuda.reload` only |

The core writes none of your ad-hoc definitions to disk. To keep one, put it
in a mod or in the config file:

- **`init.lua`** is `$XDG_CONFIG_HOME/remuda/init.lua`, else
  `~/.config/remuda/init.lua`. It is read once, at boot. Editing it changes
  nothing in a running daemon; evaluate the new lines through a door as well.
- **A mod** is the place for anything that should be reloadable. Tools and
  advice a lifecycle mod declares in its returned table are owned by that mod
  and replaced on reload. Advice added from its `start` is owned the same
  way; `initialize` cannot call `remuda.*` at all. Declare tools in the table
  rather than calling `remuda.tool` from `start`.
- **An ad hoc tool is owned by no mod.** No reload replaces it, and a mod
  that declares the same name is refused with `tool NAME is already
  registered`.

A typical loop: try the definition with `run_script` under a scratch name,
and once it works declare it in a lifecycle mod under its real name and
`remuda.reload` that mod.

## Boundaries

- **Local socket.** The daemon does not check who is asking before it
  evaluates Lua. On Unix the daemon sets the socket file to mode `0600`, so the
  boundary is the Unix user.
- **`remuda.caller()` is advisory.** It reports `session`, `outside` or
  `unknown` from process ancestry. Any process of the same user can evaluate
  Lua in the image, so nothing built on `caller()` is an authentication
  boundary.
- **Remote transports refuse `Eval`.** The restricted front used for remote
  and cluster requests never evaluates Lua. The doors above are local.
- **Mods install from GitHub** (`OWNER/REPO` or its HTTPS URL). There is no install from a
  local directory; `remuda mod test PATH` only validates a checkout.

### What remuda does not decide

An agent harness can add limits of its own. Claude Code's auto mode, for
example, has a classifier that may deny an action such as merging a pull
request, granting itself a permission, or driving itself without a human.
Those denials come from Claude Code. They are not part of remuda's design,
and remuda has no setting that changes them
([steps/037](https://github.com/warmblood-kr/remuda/blob/main/steps/037-a-launched-session-may-schedule-its-own-upkeep.md)
records one such case). Do not use a door in this document to redo an action
your harness denied: the denial is the result. Stop and report it to whoever
gave you the task.
