# Lua mods

Remuda keeps one Lua image for the lifetime of its daemon. Rust exposes the
small host surface, while Lua defines the extension vocabulary on top of it.
Every installed mod and registered tool writes metadata to the same runtime
registry; the reference output is generated from that registry at request
time.

The session API is available under `remuda.session`. Calling
`remuda.session(name)` returns a handle onto the named session; the namespace
also provides `list`, `new`, `close`, and `attach`. `remuda.session.list()`
reaps exited sessions unless `REMUDA_KEEP_EXITED=1` is set in the daemon's
environment. The older top-level `remuda.new`, `remuda.close`, `remuda.ls`,
and `remuda.attach` names are deprecated aliases. To suppress their deprecation
notices, set `REMUDA_SUPPRESS_DEPRECATIONS=1` in the daemon's environment when
the daemon starts; setting it only on a CLI process cannot change the
environment of a running daemon.

## Generate the reference

```sh
remuda doc
remuda doc --format markdown
remuda doc --format json
remuda mod info butler --format markdown
remuda mod list --format json
remuda mod install warmblood-kr/remuda-butler
remuda mod install OWNER/REPO --reload
remuda mod test path/to/remuda-mod
remuda mod update butler --reload
remuda mod update --all
remuda mod remove butler
```

The default output is reStructuredText, and the JSON form is intended for
project-site tooling and other consumers that need structured metadata.

`remuda mod list` reports installed mod manifests with name, version, API,
entry, lifecycle API, source, and installation status. `remuda mod info NAME`
shows one manifest. Both use the same RST/Markdown/JSON format selector.

`remuda mod install OWNER/REPO` accepts a GitHub shorthand or HTTPS URL,
validates the repository's `extension.toml`, and atomically stores its Lua
package under `${XDG_DATA_HOME:-$HOME/.local/share}/remuda/mods`. Add
`--ref REF` to select a branch, tag, or commit. Installation never changes a
live Lua image by default. Add `--reload` to ask the running daemon to replace
the installed lifecycle-managed mod in its existing Lua image. `remuda mod
update NAME --reload` does the same after updating one mod. Batch update with
`--all --reload` is intentionally refused before any files change; update and
reload mods individually so each mod's lifecycle support and reload result are
clear. A failed in-process reload leaves that mod's previous code, initialized
state, hooks, and tools active, while the validated update remains installed on
disk for inspection or a later retry.
When a manifest declares `command = "NAME"`, `remuda NAME` loads that
mod explicitly and opens the regular Remuda screen when attached to a terminal.
Use `remuda NAME --headless` to load it without opening the screen. Further
words (`remuda NAME ...`) are dispatched to the already-loaded mod's Lua
command handler; the core does not embed a mod-specific parser.

`remuda mod update NAME` and `remuda mod update --all` reuse each installed
mod's recorded GitHub source and ref, validate the new checkout, and replace
the old copy only after validation succeeds.
Without `--reload`, update the mod on disk and restart the daemon when
convenient.
`remuda mod remove NAME` removes an installed mod directory. It does not
restart a daemon or stop sessions; already-loaded Lua definitions remain live
until the next daemon restart.

A manifest can declare the mods it needs: `requires = { butler = ">=0.4, <0.5" }`.
- **Constraints:** each is a comma-separated list of `>=`, `>`, `<=`, `<` and `=`, all of which must hold. `*` means any version. An installed version is compared on its numeric core, so `0.1.0-nightly.X` counts as `0.1.0`.
- **Install:** `mod install` refuses a mod whose requirement is missing or too old, and names it.
- **Activation:** `exec` and `reload` check the whole chain first. They then activate the lifecycle mods it needs, hosts before guests with ties broken by name, and leave an already active host alone. A requires cycle is refused, with its path, before anything activates.
- **Remove:** `mod remove NAME` refuses while an installed mod requires NAME, and lists those mods.

An extension repository declares `api = "remuda-lua-v1"` in its
`extension.toml`. `remuda mod test PATH` is the deterministic local check: it
validates the manifest, package paths, symlinks, Lua-only contents, and Lua
syntax without installing or mutating a daemon. Integration tests should run
the same mod in an isolated `XDG_DATA_HOME`, then use the real Remuda daemon
and host bindings; unit tests can use fixture implementations of the small
`remuda` API surface. Keep the API string pinned until a deliberate host
compatibility version is published.

## In-process mod lifecycle

The optional manifest field `lifecycle = "remuda-module-v1"` opts a mod into
state-preserving reload. Existing manifests without it remain legacy scripts:
`remuda exec NAME` continues to run their entry file, while `remuda.reload(NAME)`
refuses them because rerunning top-level setup could duplicate registrations.
`remuda.exec` and `remuda.reload` both use the lifecycle manager for opted-in
mods.

A lifecycle-managed entry returns a declaration table instead of performing
setup while the file is loaded:

```lua
return {
  api = "remuda-module-v1",
  state_version = 1,
  initialize = function()
    return { handled = 0 }
  end,
  hooks = {
    {
      event = "session_started",
      run = function(state, name)
        state.handled = state.handled + 1
        print(name, state.handled)
      end,
    },
  },
  tools = {
    {
      name = "sample_status",
      about = "Report the number of sessions handled by this mod.",
      run = function(state)
        return tostring(state.handled)
      end,
    },
  },
}
```

`initialize` runs once per daemon image and must return the state table. On a
reload with the same `state_version`, the new handlers receive that same table;
initialization does not run again. A mod that changes its state shape increments
the version and supplies one migration function for each version step. Each
migration receives a copy and must return the next state table:

```lua
state_version = 2,
migrations = {
  [1] = function(old)
    return { handled = old.handled, failures = 0 }
  end,
},
```

Migration copies support nested plain tables, scalar values, and table cycles.
They reject metatables, functions, and userdata so a failed migration cannot
partially mutate the live state. If any declaration, migration or `start`
fails, the current version's hooks, tools and schedules stay active; changes a
failing `start` already made to the state are not undone. The entry is
evaluated in a private declaration environment; a declaration cannot call
remuda APIs or write daemon globals while it is being staged. Lifecycle mods declare hooks and tools in the
returned table; do not register them at file scope or from `initialize`.
Reload replaces only that mod's owned hooks and tools, so removed handlers do
not linger and other mods remain registered.

Periodic work belongs in the declaration too. Each `schedules` entry is
registered like `remuda.schedule`, but its `run` receives the state, and reload
cancels the previous activation's schedules before registering the new ones:

```lua
schedules = {
  { name = "sample_poll", every = 5, run = function(state) state.polls = (state.polls or 0) + 1 end },
},
```

An optional `start(state)` runs after each activation: the first
`remuda.exec(NAME)` and every `remuda.reload(NAME)`. `remuda.exec` (and a
mod's own no-argument command) on an already-active mod does nothing, so
opening a mod's screen does not restart it. The mod owns its declared hooks,
tools and schedules, and also every `remuda.on` hook and
`remuda.extension_command` registered while its own code runs (`initialize`,
`start`, or one of its declared hooks, tools or schedules). `hook_list` shows
that `owner`. A mod may also create new top-level `remuda.*` fields (for
example `function remuda._sample_notify(...) end` in `start`); they are its own.
Assigning a field core defines, or one another mod owns, is an error. A field
left by a legacy script is taken over only in the mod's own namespace,
`remuda._NAME_*` or `remuda.NAME_*`; any other existing field is an error. Reload replaces everything the mod owns,
and a reload whose `start` fails restores the previous set. A field the new
`start` does not recreate is removed along with any advice on it; one it does
recreate keeps its advice. Other imperative effects
(`remuda.schedule`, `remuda.process`, `remuda.new`, and so on) are not owned
and survive reload; the mod must find and reuse or cancel them itself.

The runtime registry also covers words added with `remuda.tool`, so an
extension can document itself when it registers its function:

```lua
remuda.tool("review", "Review a session", function(name)
  return remuda.capture(name)
end, "review(name) -> string")
```

`remuda.fail(message, code?)` deliberately ends the current evaluation with a
CLI failure. `message` is printed by itself to standard error; `code` defaults
to 1 and must be between 1 and 255. The evaluation stops immediately, so this
is useful for command handlers that need a clean error without a Lua traceback:

```lua
if not ready then remuda.fail("service is not ready") end
```

## Hooks

`remuda.on(event, fn, {group, id, depth})` registers a hook.
- `depth` runs from -100 to 100. Lower depths run first, and hooks with equal depth run in registration order (like Emacs `add-hook`).
- Registering the same `group` and `id` again replaces that hook, so calling `on` twice is safe.
- A declared mod hook may carry `id` and `depth` too.

Four ways to fire an event:

| Call | Result |
|---|---|
| `emit(event, ...)` | Runs every hook. |
| `emit_until_success(event, ...)` | Returns the first non-nil result. |
| `emit_until_failure(event, ...)` | Returns `false` as soon as a hook returns `false` (a veto), otherwise `true`. |
| `emit_filter(event, value, ...)` | Passes `value` through each hook as `hook(value, ...)` and returns the result. A hook returning nil leaves the value unchanged. |

A hook that raises an error is logged with its group and id, and counted on that hook. It never counts as an answer or a veto.

`remuda.hook_list(event?)` returns copies of `{event, group, id, depth, owner, src, errors, last_error}` in run order. Use it to inspect hooks.

`remuda.hooks` is deprecated for reading, and will become read-only once no mod edits it by hand.

```lua
remuda.on("before_send", function(text) return text:gsub("%s+$", "") end,
  { group = "tidy", id = "trim", depth = -10 })
local text = remuda.emit_filter("before_send", "hi  ")  -- "hi"
## Contributions

`remuda.contribute(point, id, entry)` fills an extension point that a host defines (like VS Code `contributes`). The same point and id replaces the earlier entry.

`remuda.contributions(point)` returns `{id, owner, entry}` rows, sorted by `entry.order` (default 0) and then by id. Each `entry` is a shallow copy: changing its fields never reaches the registry, but nested tables are shared. `contribute` stores a shallow copy too.

A lifecycle mod can declare its contributions: `contributes = { [point] = { {id = ..., ...} } }`.
- Declared entries are owned by the mod, and their function fields receive `state` first, like hooks.
- A reload replaces only that mod's entries, and a failed `start` restores the previous ones.
- A malformed declaration is refused before anything changes.
- A declared entry whose point and id belong to another mod is refused, naming both mods. Imperative `contribute` calls made during a mod's lifecycle extent carry that mod's owner, so reload replaces them and a failed `start` rolls them back. A call outside a mod extent has no owner: it can replace an unowned entry, but cannot replace an entry owned by a mod.

```lua
contributes = {
  ["butler.command"] = {{ id = "inbox", order = 20, usage = "inbox [NAME]",
    run = function(state, args, caller) return "..." end }},
}
```

## Advice

`remuda.advise(path, how, fn, {id, depth})` wraps the function stored at a
`remuda.*` path, such as `remuda._butler_notify` or `remuda._butler_mail.queue`,
with Emacs nadvice semantics. Local functions have no path and cannot be advised.

| `how` | Runs |
|---|---|
| `around` | `fn(orig, ...)`; call `orig` to continue |
| `before` / `after` | `fn(...)` before or after the original, keeping its result |
| `override` | `fn(...)` instead of the original |
| `filter_args` / `filter_return` | the original on `fn(...)`'s results, or `fn` on the original's |
| `before_while` / `before_until` | the original only if `fn(...)` is truthy, or falsy (else returns it) |

- `id` is required; advising the same path and `id` again replaces it.
- `depth` runs from -100 (outermost) to 100 (innermost); the default is 0.
- `unadvise(path, id)` removes one; removing the last restores the original.
- `advice_member(path, id)` and `advice_list(path?)` inspect it.
- An error raised inside the chain gains a line per layer,
  `<- advice ID (HOW, depth D) on PATH [OWNER]`.
- Advising a path that holds no function is an error.
- A mod that redefines an advised function when it loads keeps the advice: the
  new definition becomes the original.
- Each installed wrapper runs the chain and original it was built from. Code
  that captured the wrapped function before redefining it
  (`local orig = remuda.f; function remuda.f(...) return orig(...) end`)
  therefore calls the older composition, so the advice runs once around the new
  definition and once more inside `orig`. It never loops back into itself.

A lifecycle mod may declare `advice = {{path, how, id, depth, run}}`; `run`
receives the mod's state first. Declared advice, and advice the mod adds while
its own code runs, is owned like its hooks, so reload replaces it and a failed
`start` restores the previous set.

```lua
remuda.advise("remuda._butler_notify", "around", function(orig, alias, notice)
  if quiet_hours() then return false end
  return orig(alias, notice)
end, { id = "quiet-hours" })
```

## Generated reference

The reference below is extracted from the built Remuda runtime; it is not a
second catalog maintained in the site source.

<iframe
  src="lua-reference.html"
  title="Generated Remuda Lua runtime reference"
  style="width: 100%; min-height: 70rem; border: 1px solid #d8dee4;"
></iframe>

The source remains available as [reStructuredText](lua-reference.rst) for
publishing systems that consume RST directly.
The CLI's `--format markdown` and `--format json` outputs remain available for
other site or tooling integrations.

See [the design notes](design.md#one-registry-for-every-word) for the registry
model and [the project source](https://github.com/warmblood-kr/remuda) for the
current implementation.
