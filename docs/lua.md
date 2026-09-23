# Lua mods

Remuda keeps one Lua image for the lifetime of its daemon. Rust exposes the
small host surface, while Lua defines the extension vocabulary on top of it.
Every installed mod and registered tool writes metadata to the same runtime
registry; the reference output is generated from that registry at request
time.

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
partially mutate the live state. If any declaration or migration fails, the
current version stays active. The entry is evaluated in a private declaration
environment; a declaration cannot call remuda APIs or write daemon globals
while it is being staged. Lifecycle mods declare hooks and tools in the
returned table; do not register them at file scope or from `initialize`.
Reload replaces only that mod's owned hooks and tools, so removed handlers do
not linger and other mods remain registered.

The runtime registry also covers words added with `remuda.tool`, so an
extension can document itself when it registers its function:

```lua
remuda.tool("review", "Review a session", function(name)
  return remuda.capture(name)
end, "review(name) -> string")
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
