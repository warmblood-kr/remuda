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

`remuda.session.new(name?, argv?, cwd?, env?)` and its `remuda.new` alias
return two values: the chosen session name first, then that launch's
`instance_id`. Existing single-value assignments still receive the name.
The ID comes from the same registry insertion that starts the session, so it
continues to identify that launch if another session later reuses its name. A
failed launch raises an error and returns no ID.

Compatibility: `return new(...)` as a final function result, or `new(...)` as the last
argument of a call, now passes both values. Over MCP, `run_script` joins
multiple return values with a tab, so a script that returned `new(...)` now
yields `name<TAB>instance_id` instead of `name`. To keep the old text, wrap the
call: `return (new(...))`, or assign `local name = new(...)` first. The ID is
not a secret and proves the launch, not that the session is still running.

On Windows, everything the session's program starts after it is put in its
job ends when the session is reaped or closed, or when the daemon stops. A
session that cannot be put in a job is not started. The one gap is the instant
between program creation and job assignment; issue #614 tracks closing it with
atomic job-list process creation (`CreateProcessW` with
`PROC_THREAD_ATTRIBUTE_JOB_LIST`). A process that must outlive the session has to start outside remuda. While
listed, processes in the job are `session` callers for `remuda.caller()`,
including ones whose parent has exited.

## Screen capture

`remuda.capture(name)` returns the current screen as plain text.
`remuda.capture_styled(name)` returns screen rows as arrays of `{text, dim}`
spans, plus a `cursor` table with `row`, `col`, and `visible` fields:

```lua
local screen = remuda.capture_styled("work")
local cursor = screen.cursor
-- row and col are 1-based; visible says whether the terminal displays the cursor.
print(cursor.row, cursor.col, cursor.visible)
for _, row in ipairs(screen.rows) do
  for _, span in ipairs(row) do
    print(span.text, span.dim)
  end
end
```

The `dim` field preserves terminal dim styling, which can identify placeholder
or suggestion text. Cursor position and styled spans are observations only;
mods interpret them according to the prompt or terminal application they know.

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
validates the repository's `extension.toml`, and atomically stores its package
under `${XDG_DATA_HOME:-$HOME/.local/share}/remuda/mods`. The package contains
Lua files and may include inert regular files listed in `assets`, an array of
checkout-relative paths inside the package root (for example,
`assets = ["packages/butler/matrix_relay.py"]`). Assets must not be symlinks
or directories and are limited to 64 assets, 1 MiB each and 8 MiB in total. An
asset with a setuid, setgid or sticky mode bit is rejected. Remuda copies their
bytes and file mode (group and other write bits are cleared) but never loads or
executes them. Other non-Lua package files remain rejected. Add
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
command handler; the core does not embed a mod-specific parser. A mod may hand
its declaration to `remuda.cli.parse`; the handler still receives the raw word
list.

`remuda mod update NAME` and `remuda mod update --all` reuse each installed
mod's recorded GitHub source and ref, validate the new checkout, and replace
the old copy only after validation succeeds.
Without `--reload`, update the mod on disk and restart the daemon when
convenient.
`remuda mod remove NAME` removes an installed mod directory. It does not
restart a daemon or stop sessions; already-loaded Lua definitions remain live
until the next daemon restart.

## Atomic file writes

`remuda.fs.write_atomic(path, bytes, options?)` replaces one file from trusted Lua code.
It writes a unique temporary file beside the target, syncs the bytes, and
renames the temporary file over the target. Unix also syncs the parent
directory. The target path is replaced as a directory entry, so a symlink at
that path is not followed. The parent directory must already exist.

On success the function returns `true, nil`; on an I/O error it returns
`nil, error`. By default, new files use mode `0644` filtered through the
process umask, and existing regular-file permissions are preserved. Pass
`{ private = true }` to make the file owner-only (mode `0600` on Unix, an
owner-only ACL on Windows), including when replacing an existing file; it is
applied at temporary-file creation.
The options table accepts only the boolean `private` key; unknown keys or a
non-boolean value raise a Lua argument error.
This word does not restrict paths: the Lua runtime already provides trusted
scripts with `io.open` and `os.rename`.
On Windows 10 version 1903 and later those C-library words (`os.getenv`,
`io.open`, `os.rename`, `os.remove`) read and return paths in UTF-8, the same
bytes the `remuda.*` words use; on older Windows they use the system code page,
so a non-ASCII path from one side is not valid on the other.
Bytes that are not valid UTF-8, given to `io.open` or `os.rename` on Windows,
are replaced by the system with U+FFFD, so two different invalid byte strings
can name the same file: do not treat byte-distinct paths as file-distinct there.

## New directory creation

`remuda.fs.mkdir_new(path)` creates one directory without creating its parent.
The path must be absolute and must not end in a path separator. On Unix, new
directories use mode `0700`.

The function returns `true` only when this call created the directory. It
returns `nil, "exists"` if the target already exists, including when it is any
kind of symlink; treat that result as **not Butler-created**. The caller must
also check that the path is under its own home before trusting the directory.
Other failures return `nil, error`.

## Mod databases (`remuda.db`)

`remuda.db.open(name [, opts])` opens a SQLite database that belongs to the
mod whose code is running (#778). In mod `butler`, `remuda.db.open("notes")`
opens `<state>/remuda/db/butler/notes.db` and `open("inbox")` opens
`butler/inbox.db`. `name` is a plain identifier, never a path. Outside a
mod's own code (`initialize`, `commands`, `start`, or its hooks, tools and
schedules), for example in `remuda -e` or the REPL, it returns
`nil, "no_mod: ..."`. Keep the handle in the mod's state.

```lua
start = function(state)
  state.db = assert(remuda.db.open("notes"))
  assert(state.db:batch("CREATE TABLE IF NOT EXISTS note (id INTEGER PRIMARY KEY, body TEXT)"))
  assert(state.db:exec("INSERT INTO note (body) VALUES (?)", {"hello"}))
  for _, row in ipairs(assert(state.db:query("SELECT id, body FROM note"))) do
    print(row.id, row.body)
  end
end
```

A handle has `query(sql, params) -> rows`, `exec(sql, params) -> changes`,
`batch(sql) -> true` (several statements, no parameters), `limits()` and
`close()`. `params` is an array of nil, boolean, number or string; a string
that is not UTF-8 is stored as a blob. Rows are tables keyed by column name,
and SQL NULL is a missing key.

The files are private (0600 in a 0700 directory; the owner-only ACL on
Windows) and in WAL mode. Lua is not a sandbox, so the limits guard against
mistakes that would stall or bloat the daemon, not against hostile code:

| Limit | Default | Most `opts` may ask for | Failure |
|---|---|---|---|
| `max_bytes`, database size | 64 MiB | 1 GiB | `too_large: ...` |
| `timeout_ms`, per statement | 1000 | 10000 | `timeout: ...` |
| `max_rows`, per query | 10000 | 100000 | `too_many_rows: ...` |

`ATTACH`, `DETACH` (and so `VACUUM INTO`) and PRAGMAs outside a short list
(`user_version`, `table_info`, `index_list`, `integrity_check`, and a few
other read-only ones; it may read `user_version` but not set it) fail with
`denied: ...`, so a handle cannot reach another file, the VFS index
included, or undo its limits. Other failures are `sql: ...`, `io: ...` and,
after `close()`, `closed: ...`.

### Schema migrations

A mod ships its schema as numbered plain-SQL steps and lists them in
`assets`, like other inert files:

```
packages/butler/migrations/notes/0001_create.up.sql
packages/butler/migrations/notes/0001_create.down.sql
packages/butler/migrations/notes/0002_tags.up.sql    -- no down: irreversible
```

`remuda.db.open("notes", { migrations = true })` reads
`migrations/notes/` from the installed mod and applies the steps the
database does not have yet. A mod may instead pass the steps inline:
`migrations = { { name = "create", up = "...", down = "..." }, ... }`, where
entry `i` is step `i`.

- Steps are numbered 1..N with no gaps (`NNNN_name.up.sql`, optional
  `NNNN_name.down.sql`). An empty file is an error; a step that is meant to
  do nothing holds an SQL comment.
- All pending steps run in one transaction, together with the new
  `user_version`. If a step fails or the daemon dies midway, the database
  keeps its old schema. Foreign keys are checked before commit.
- Never edit a step that has shipped. The sha256 of each applied step is
  stored, and an edited step fails the open with `drift: ...`. Add a new
  step instead.
- A step may not use ATTACH, BEGIN/COMMIT/SAVEPOINT, PRAGMAs other than
  `foreign_key_check`, or the `_remuda_*` tables (`denied: ...`). To change
  a column in a way `ALTER TABLE` cannot, rebuild the table: create
  `new_t`, copy the rows, drop `t`, then rename `new_t` to `t`.
- A database at a newer version than the mod knows fails the open with
  `too_new: ...`, as does an open without `migrations` on a migrated
  database. Older code never writes to a schema it does not know.
- Migrations get 10 s in total. The size limit is the handle's.

Each step's down SQL is stored in the database when the step runs, so a
step can be undone without the newer mod's files. To go back to an older
version of a mod:

1. Install the older mod. Its `open` now fails with `too_new`.
2. Run `remuda db downgrade MOD NAME VERSION`. It prints what it would undo.
3. Run it again with `--yes`. It writes a private backup,
   `NAME.db.vN-TIME.bak`, next to the database and then runs the downs in
   one transaction. A range that holds an irreversible step is refused
   before anything runs.
4. Reload the mod.

A mod may list at most 64 assets, so a long history needs squashing. Write
one `NNNN_name.base.sql` holding the whole schema as of step NNNN (2 or
more), then delete the step files up to NNNN:

```
packages/butler/migrations/notes/0012_base.base.sql   -- schema as of step 12
packages/butler/migrations/notes/0013_pins.up.sql
```

- A new database runs the base and records it as step NNNN, then runs the
  later steps. It cannot be stepped back below NNNN.
- A database already at NNNN or later upgrades as before; deleted steps are
  not checked for drift.
- A database below NNNN fails the open with `squashed: ...`: open it once
  with an older release of the mod that still has the steps. To avoid this,
  keep the last few step files next to the base (step files must then run
  without gaps from at most NNNN + 1).
- Older remuda builds refuse a database created from a base before changing
  it, and refuse `.base.sql` files.

`remuda db status [MOD]` shows each database's version and steps and
compares them with the installed files. `remuda db upgrade MOD NAME`
applies the installed files without waiting for the mod to open the
database. Like `remuda vfs`, these commands run in the daemon. With no
daemon running, they fail with one line.

## JSON values

`remuda.json.decode(text)` reads at most 8 MiB of UTF-8 JSON and returns
`value, nil` on success or `nil, error` on bad input. Objects become Lua tables
with string keys; arrays become 1-based Lua tables; integers that fit a Lua
integer stay integers, and other finite JSON numbers become Lua numbers.
JSON `null` becomes the sentinel `remuda.json.null`, so it can appear as a
table value without disappearing like Lua `nil`.

Decoded tables carry hidden array/object tags, so even empty arrays and objects
keep their JSON kind when encoded again. For new Lua tables, use
`remuda.json.array(table)` or `remuda.json.object(table)` to tag empty or
ambiguous tables. Nonempty contiguous `1..n` tables encode as arrays; nonempty
string-keyed tables encode as objects. Empty untagged tables, mixed keys,
sparse numeric keys, and tables that cannot be classified as one shape are
rejected. Tagging refuses a table that already has a non-JSON metatable.

`remuda.json.encode(value, { pretty = true })` returns formatted JSON when
requested; the default is compact JSON. It refuses nil, non-finite numbers,
invalid UTF-8 strings or keys, cycles, functions, threads, userdata, and values
that do not form a valid array or object. Decode and encode cap nesting at 64
containers, JSON text/output at 8 MiB, and each document at 100,000 JSON values
to bound memory use. Object key order is not preserved; encoded keys are
sorted. Decode rejects duplicate object keys with
`nil, "duplicate key"` instead of choosing one value. Integer JSON numbers
outside Lua's signed integer range are represented as finite Lua numbers when
possible. Very small non-integer values can underflow to `0.0`, and negative
zero remains the floating-point value `-0.0`.

A manifest can declare the mods it needs: `requires = { butler = ">=0.4, <0.5" }`.
- **Constraints:** each is a comma-separated list of `>=`, `>`, `<=`, `<` and `=`, all of which must hold. `*` means any version. An installed version is compared on its numeric core, so `0.1.0-nightly.X` counts as `0.1.0`.
- **Install:** `mod install` refuses a mod whose requirement is missing or too old, and names it.
- **Activation:** `exec` and `reload` check the whole chain first. They then activate the lifecycle mods it needs, hosts before guests with ties broken by name, and leave an already active host alone. A requires cycle is refused, with its path, before anything activates.
- **Remove:** `mod remove NAME` refuses while an installed mod requires NAME, and lists those mods.

An extension repository declares `api = "remuda-lua-v1"` in its
`extension.toml`. `remuda mod test PATH` is the deterministic local check: it
validates the manifest, package paths, symlinks, Lua contents and syntax, and
declared assets without installing or mutating a daemon. Integration tests should run
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
`commands`, `start`, or one of its declared hooks, tools or schedules).
`hook_list` shows the owner of event hooks.

An optional `commands(state)` hook lets a lifecycle mod register its CLI
handlers without launching its background work. When Remuda loads a mod to
serve one of its subcommands, it runs `commands` and skips `start`; running
`remuda MOD` with no subcommand runs `commands` and then `start` once. A later
bare invocation starts a mod that was previously loaded only for commands.
Mods without `commands` keep the old behavior: loading their subcommand runs
`start` as before. Keep `commands` limited to registration; put launch work in
`start`. A mod may also create new top-level `remuda.*` fields (for example
`function remuda._sample_notify(...) end` in `start`); they are its own.
Assigning a field core defines, or one another mod owns, is an error. A field
left by a legacy script is taken over only in the mod's own namespace,
`remuda._NAME_*` or `remuda.NAME_*`; any other existing field is an error. Reload replaces everything the mod owns,
and a reload whose `start` fails restores the previous set. A field the new
`start` does not recreate is removed along with any advice on it; one it does
recreate keeps its advice. Other imperative effects
(`remuda.schedule`, `remuda.process`, `remuda.new`, and so on) are not owned
and survive reload; the mod must find and reuse or cancel them itself.

An optional `ready(state)` lets the CLI's `remuda exec NAME` wait for an
asynchronous start to finish. It returns `true` when ready, `nil` while it is
still working, or `nil, "message"` when startup failed. A Lua error from the
callback is also a failure. `timeout_ms` on the same declaration sets the
readiness deadline; it defaults to 30000 and must be an integer from 1 through
240000. The CLI checks every 250 ms, issuing a separate short Eval each time.
Failure exits 1 and prints `mod NAME failed to become ready: message`; timeout
exits 124 and prints `mod NAME did not become ready within Ns`. Keep the
callback quick: each Eval still has the daemon's 305-second client deadline.
Ctrl-C during this wait abandons only the CLI wait; it does not cancel startup
or deactivate the module. `start` has already run once, and a later
`remuda exec NAME` resumes checking readiness without running `start` again.
Mods without `ready` keep today's immediate-success behavior. This is a
declaration field alongside `start` and `stop`, not another `remuda.*` word:

```lua
return {
  api = "remuda-module-v1",
  state_version = 1,
  initialize = function() return { connected = false } end,
  start = function(state) connect_async(state) end,
  ready = function(state)
    if state.error then return nil, state.error end
    return state.connected or nil
  end,
  timeout_ms = 45000,
}
```

An optional `stop(state)` runs for the old activation before a reload starts
its replacement, while the old activation still owns its registrations. It
also runs for each active lifecycle mod during a clean daemon shutdown.
Failures are logged and cleanup continues; shutdown waits at most two seconds
for the whole stop pass. A forced daemon termination cannot run Lua stop
callbacks.

An optional `daemon_reload_ready(state)` lets a mod hold off `remuda reload`
(Unix) while it is mid-task, such as launching a session. It returns nil when
a reload is fine now, or a reason. Each reason blocks both `remuda reload
--check` and `remuda reload` as `blocker: mod NAME: reason`, and the daemon
keeps serving. A Lua error counts as a reason. Keep it quick: the daemon waits
at most 2 s for every mod's answer and blocks the reload if it gets none. Mods
without it never block a reload, and older cores ignore the field.

The runtime registry also covers words added with `remuda.tool`, so an
extension can document itself when it registers its function:

```lua
remuda.tool("review", "Review a session", function(name)
  return remuda.capture(name)
end, "review(name) -> string")
```

A tool or mod command called for a session caller (over MCP, or as a mod
command) runs only while that caller is live: the daemon checks the caller's
session name and `instance_id` when it dispatches the call. Admission follows
observed death. A call whose check ran before the daemon recorded the session's
death is admitted and may finish, even if the session dies meanwhile; a call
checked after that is refused as no longer live. The check is advisory and
same-user, not an isolation boundary.

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

The daemon emits `session_output(name, details)` after terminal output changes.
`details.version` is the session's output version. Notifications are coalesced
for up to 50 ms per active session, and a busy Lua image keeps only the latest
pending wake for each session. `remuda.expect` uses this event to check matching
sessions promptly; its periodic tick remains the fallback for deadlines and
sessions whose backend cannot stream output.

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

## Periodic schedules

`if remuda.sleep` now raises the migration error instead of returning `nil`;
use `remuda.after` to schedule non-blocking work.

`remuda.schedule({every, after?, name?, run})` runs a callback on the daemon's
periodic tick. `after` is an optional finite, non-negative number of seconds
from schedule creation to the first callback; later callbacks are spaced by
`every` seconds from the preceding firing. The scheduler checks about once per
second, so a callback runs on the first tick at or after its deadline.

Without `after`, the existing first-fire default depends on daemon uptime. The
first callback is due when the daemon's elapsed-time clock reaches `every`:
if the daemon has already been running that long, a new schedule can fire on
the next tick; otherwise it waits until the daemon reaches that uptime. This
default is not a delay measured from schedule creation.

```lua
remuda.schedule({
  name = "refresh",
  every = 60,
  after = 10,
  run = function() refresh_status() end,
})
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

### UI key bindings (session list and cluster UI)

The Lua image owns UI declarations and help metadata. The native UI takes one
validated snapshot after user configuration finishes, then resolves keys and
renders help from that same snapshot. Reopen the UI to pick up later changes.
The session-list browse toggle, the line composer key (`m`,
`session.compose`) and cluster command controls use the registry. The
composer's editing keys are the `cluster.composer` bindings, shared by the
session screen and the cluster tree.
Other list/visual/attach/prompt controls still use existing handlers, pending
migration (see #704). Cluster snapshots come from the local daemon; remote
nodes do not supply key declarations.

In `$XDG_CONFIG_HOME/remuda/init.lua` (otherwise `~/.config/remuda/init.lua`):

```lua
remuda.rebind("session.list.browse", "session.toggle_list", "M-l")
```

This replaces the default `l` binding, and help shows `M-l list`. Plain `l`
still moves right in visual mode and goes to the child in session focus.
`remuda.unbind(context, key)` removes an effective binding. An unbound action
can be declared again, with its help, using:

```lua
remuda.define_key{
  context = "session.list.browse",
  key = "z",
  action = "session.toggle_list",
  help = "list",
}
```

Cluster contexts and native actions:

| Context | Actions |
| --- | --- |
| `cluster.tree` | `cluster.detach`, `cluster.close`, `cluster.search`, `cluster.attention`, `cluster.remote.keys`, `cluster.up`, `cluster.down`, `cluster.left`, `cluster.right`, `cluster.select` |
| `cluster.search` | `cluster.search.clear`, `cluster.search.delete`, plus the same navigation and select actions |
| `cluster.composer` | `cluster.composer.submit`, `.clear`, `.back`, `.detach`, `.left`, `.right`, `.forward`, `.backward`, `.up`, `.down`, `.home`, `.end`, `.home_emacs`, `.end_emacs`, `.kill`, `.backspace`, `.delete` (all use the `cluster.composer` prefix) |
| `cluster.confirm` | `cluster.confirm.accept`, `cluster.confirm.accept_upper` |
| `cluster.remote.keys` | `cluster.remote.back` |
| `cluster.runtime` | `cluster.redraw` |

For example:

```lua
remuda.rebind("cluster.remote.keys", "cluster.remote.back", "M-e")
remuda.rebind("cluster.composer", "cluster.composer.submit", "M-s")
```

The old remote exit chord then forwards to the child like other raw keys;
the new exit chord is intercepted and shown in the remote status row. Raw
control bytes retain their transport identity (BS, LF, NUL and Alt prefixes);
command canonicalization applies only to lookup, and byte encoding translates
only the terminal numeric Ctrl-4 alias. Rebind
composer submit also changes the second-send confirmation for possibly partial
remote input; it retains the existing target and draft safety checks.

Runtime redraw wins outside remote raw mode, then confirmation, raw input,
composer, and search/tree controls resolve in that order. Do not assign a
runtime key you want another context to receive. Text insertion in search and
composer, raw terminal bytes, and paste transport remain native fallbacks.
Cluster command matches now require exact modifiers: modified commands no
longer trigger an unmodified binding. Unmatched Ctrl/Alt characters are ignored
in search/composer; plain characters remain text. Only Press events dispatch,
including redraw (Repeat/Release no longer redraw).

Confirmation keeps separate lower/uppercase accept actions; rebinding one
does not change the other. Unbind the other too if only one acceptance key is
wanted. Every other key cancels; help says this explicitly. Composer `.back`
and `.detach` are likewise independent actions with the same return-to-list
behavior. Ctrl-4/Ctrl-backslash are one normalized key, so rebinding that
single action updates both terminal aliases. Effective help wraps with Unicode
width rather than clipping controls at narrow widths. Extra rows reduce the
available pane area; the remote exit hint is prioritized over a long target.

`remuda.keymap_snapshot()` returns `{generation, ready, diagnostic?, rows}`;
each row contains `context`, `key`, `action`, and `help`. Rows are copies, not
a mutable registry. This slice supports one binding per action. Unknown
contexts/actions/key spellings, duplicate keys/actions and conflicts with
legacy browse commands raise errors before changing the map. Use the existing
Emacs-style spelling with `C-` and `M-` modifiers, single characters and named
keys such as `<left>`. Multi-key chords and explicit Shift modifiers are not
supported. Ctrl-4 normalizes to Ctrl-backslash, as the terminal decoder does.

Lifecycle mod registrations carry the current mod owner, are removed when
replaced, and are restored on failed activation rollback. A mod cannot replace
another mod's effective binding; user `rebind` is explicit replacement. The
user layer takes precedence over mod layers, which take precedence over
defaults. Removing a mod's layer exposes an earlier declaration. Defaults remain in Lua rather than being mirrored in
Rust.

The UI waits up to three seconds for config readiness. A missing registry,
transport failure or readiness timeout produces a visible startup error and
never falls back to a hidden native key table. A failed `init.lua` is recorded
in the snapshot and displayed in the footer; each completed binding operation
remains valid, while a rejected operation leaves its prior map intact.

### Cluster input editing

The cluster composer wraps into up to three rows and scrolls to keep the cursor
visible. The cursor highlights the existing grapheme instead of inserting an
extra marker; Left/Right and erase operate on grapheme boundaries.
Ctrl-A moves to the beginning, Ctrl-E to the end, and Ctrl-K removes the tail.
These keys are Lua declarations and can be rebound or unbound. Bracketed paste
inserts at the draft cursor. Pasted CR, LF and tab boundaries are shown as
visible control glyphs while the draft retains its original characters; raw
remote-key paste retains its existing transport.

Up and Down move between lines of a multi-line draft. On the first line Up
recalls the previous submitted entry, and on the last line Down recalls the next
one; past the newest entry Down restores the draft you were editing. History is
kept per target session, in memory only (never written to disk), holds the last
500 entries, and skips empty submissions and repeats of the previous entry. Up
recalls with the cursor at the end of the text, Down with it at the start, so a
multi-line entry is walked line by line before history continues.

Enter queues an idempotent `InputLine` request pinned to the session instance.
The receiver uses the shared session text/Return path, waits for text to appear
when the terminal exposes a screen, and sends one Return. The composer's existing
trailing CR is consumed. Retries retain the client ID and sequence, so an
acknowledged request is not written again. Acknowledgment confirms input delivery;
it does not prove the child accepted or executed the command. Cluster submission
does not automatically send a second Return when a child inserts a newline.

Local lines retain the 64 KiB input limit; remote composed lines must fit the
12 KiB remote batch limit (including the composer's CR). Over-limit drafts remain
editable. Raw remote paste still uses bounded chunks. Receivers must support
`InputLine`; older receivers refuse it, and the client does not retry through a
different write API. If an older receiver returns its pre-dispatch
`invalid remote request` response, the client reports that the receiver needs
an upgrade and that the line was not submitted; it does not label delivery
uncertain. Screen observation uses an eight-second request deadline.
Local submission can block cluster UI updates while that observation completes.

The selected live session also receives instance-pinned `ResizeSession`
requests matching its displayed pane, including raw remote-key mode.
Layout, window size, draft wrapping, footer, notice and queue changes update
the target dimensions. Unchanged successful sizes are cached; failures display
a notice and retry at most once per second for the same target/size.
Explicit viewport sizes allow short as well as narrow terminals, bounded to
1–1000 columns and 1–500 rows. Ordinary terminal size defaults are unchanged.
Remote resize requires remote control; read-only and ended sessions are skipped.
Receivers must support the new request. An older receiver's pre-dispatch
`invalid remote request` refusal is shown as an upgrade requirement and is not
retried for unchanged geometry. Concurrent viewers can still resize the same
session; there is no exclusive resize ownership or size restoration.

Very small panes may leave a child TUI unusable even though the requested
terminal geometry matches the viewport; one cell is the minimum resize size.
