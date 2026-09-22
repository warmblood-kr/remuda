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
remuda mod test path/to/remuda-mod
remuda mod update butler
remuda mod update --all
remuda mod remove butler
```

The default output is reStructuredText, and the JSON form is intended for
project-site tooling and other consumers that need structured metadata.

`remuda mod list` reports installed mod manifests with name,
version, API, entry, source, and installation status. `remuda mod info NAME`
shows one manifest. Both use the same RST/Markdown/JSON format selector.

`remuda mod install OWNER/REPO` accepts a GitHub shorthand or HTTPS URL,
validates the repository's `extension.toml`, and atomically stores its Lua
package under `${XDG_DATA_HOME:-$HOME/.local/share}/remuda/mods`. Add
`--ref REF` to select a branch, tag, or commit. Installation never changes a
live Lua image, so run `remuda exec NAME` or `remuda restart` to reload it.
When a manifest declares `command = "NAME"`, `remuda NAME` loads that
extension explicitly. Further words (`remuda NAME ...`) are dispatched to the
already-loaded extension's Lua command handler; the core does not embed an
extension-specific parser.

`remuda mod update NAME` and `remuda mod update --all` reuse each installed
mod's recorded GitHub source and ref, validate the new checkout, and replace
the old copy only after validation succeeds.
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
`remuda mod update NAME` and `remuda mod update --all` reuse each installed
mod's recorded GitHub source and ref, validate the new checkout, and replace
the old copy only after validation succeeds.

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
