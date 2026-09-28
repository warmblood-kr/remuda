Remuda Lua runtime
==================

_call
-----

``_call(name, arguments, caller) -> string`` — Dispatch one MCP tools/call by name.

_descriptors
------------

``_descriptors() -> string`` — MCP tool descriptors for everything `remuda.tool` has registered.

_dispatch_extension_command
---------------------------

``_dispatch_extension_command(name, args, caller) -> value`` — Dispatch arguments and caller context to a loaded mod command handler.

_event_counts
-------------

``table`` — Internal event-emit counts, keyed by event name. Read via `event_counts()`.

_extension_commands
-------------------

``table`` — Handlers registered for installed mod commands.

_function_source
----------------

``_function_source(fn) -> string`` — Where a Lua function was defined, as `source:line`; internal, for `hook_list`, since scripts get no `debug` library.

_process_drain
--------------

``_process_drain(id) -> nil`` — Deliver buffered process output as emit events; internal, an Image job only.

_process_killpg
---------------

``_process_killpg(id) -> nil`` — Reap a process's whole process group (Linux only); internal, called by the daemon's own clean-shutdown sweep, not meant for scripts.

_process_spawn
--------------

``_process_spawn(argv, on_line?, on_exit?) -> id`` — Spawn a plain-pipe child process; internal, wrapped by `remuda.process`.

_refresh_sessions_buffer
------------------------

``_refresh_sessions_buffer(width, selected) -> nil`` — Rebuild the *sessions* buffer's content.

_registry
---------

``table`` — The word registry itself: name, about and signature for every bound word.

_registry_dump
--------------

``_registry_dump(format?) -> string`` — Render the live word registry as documentation.

_run_due_schedules
------------------

``_run_due_schedules(now) -> nil`` — Fire every schedule whose interval has elapsed. Called once per native tick.

_schedule_fire_counts
---------------------

``table`` — Internal named-schedule fire counts. Read via `schedule_fires()`.

_sync_window_shown
------------------

``_sync_window_shown(name, selection_changed) -> string`` — Reconcile the current window with the session Rust wants to auto-follow.

attach
------

``attach(name) -> nil`` — Enter raw mode on a session (a no-op inside the daemon's own image).

buffer
------

``table`` — Namespace for creating and listing named text buffers.

buffers
-------

``table`` — The `remuda.buffer` registry table, keyed by buffer name.

cancel
------

``cancel(handle) -> nil`` — Cancel a schedule by the handle `schedule()` returned.

capture
-------

``capture(name) -> string`` — Read a session's current screen as plain text.

capture_styled
--------------

``capture_styled(name) -> {rows, cursor = {row, col, visible}}`` — Read a session's screen as rows of {text, dim} spans, plus its cursor.

clear_hooks
-----------

``clear_hooks(opts) -> nil`` — Remove every hook registered under a group.

click
-----

``click(name, col, row, button?) -> nil`` — Send a mouse click at a terminal cell.

close
-----

``close(name) -> nil`` — End a session, live or already self-exited.

contribute
----------

``contribute(point, id, entry) -> nil`` — Fill an extension point: the same owner can replace its entry; another mod cannot. `entry.order` sorts (default 0).

contributions
-------------

``contributions(point) -> {{id, owner, entry}...}`` — A point's entries as {id, owner, entry} rows, entry a shallow copy, by entry.order then id.

emit
----

``emit(event, ...) -> nil`` — Fire an event, running every hook registered for it.

emit_filter
-----------

``emit_filter(event, value, ...) -> value`` — Thread a value through each hook as `hook(value, ...)`; nil or an error leaves it unchanged.

emit_until_failure
------------------

``emit_until_failure(event, ...) -> boolean`` — Fire an event until a hook returns false (a veto). An erroring hook is no answer, never a veto.

emit_until_success
------------------

``emit_until_success(event, ...) -> value?`` — Fire an event until a hook returns non-nil, and return that value. An erroring hook is no answer.

event_counts
------------

``event_counts() -> {[event]=n}`` — How many times each event has been emitted.

exec
----

``exec(name) -> nil`` — Run an installed mod's entry source, by name, in this same image.

extension_command
-----------------

``extension_command(name, handler(args, caller)) -> nil`` — Register a handler for an installed mod command.

feed
----

``feed(name, steps) -> nil`` — Deliver a sequence of bursts and pauses as one indivisible act.

hook_list
---------

``hook_list(event?) -> {{event, group, id, depth, owner, src, errors, last_error}...}`` — Copies of the registered hooks, for one event or all, in run order.

hooks
-----

``table`` — Deprecated for reading: use `hook_list`. The `remuda.on` table, keyed by event name; it becomes read-only once no mod edits it by hand.

insert
------

``insert(name, text) -> nil`` — Insert raw bytes into a session with nothing appended.

key
---

``key(name, spec) -> nil`` — Press a named key, in Emacs kbd notation.

kill
----

``kill(id) -> nil`` — Terminate a process started with `remuda.process`, by id.

list_dir
--------

``list_dir(dir) -> {string...}`` — List a directory's entries.

ls
--

``ls() -> {session...}`` — List every session in the registry, reaping exited ones unless REMUDA_KEEP_EXITED is set.

mkdir
-----

``mkdir(dir) -> nil`` — Create a directory, including its parents.

new
---

``new(name?, argv?, cwd?, env?) -> string`` — Start a new session, defaulting the command to the user's shell.

on
--

``on(event, fn, opts?) -> nil`` — Register a callback to run when an event fires. `opts`: `group`, `id` (same group+id replaces), `depth` (-100..100, lower first).

process
-------

``process(spec) -> id`` — Spawn a plain-pipe child process; its stdout lines and exit arrive as emit events.

processes
---------

``processes() -> {id...}`` — List the ids of every process started with `remuda.process` that is still running.

reload
------

``reload(name) -> nil`` — Reload a lifecycle-managed mod in this image, preserving state and replacing its registrations.

remove_dir_all
--------------

``remove_dir_all(dir) -> nil`` — Remove a directory and everything under it.

request_counts
--------------

``request_counts() -> string`` — The daemon's own request-dispatch counts (list/eval/capture_styled), counted at the one place every round trip crosses. Use it to measure how many round trips a real operation actually costs.

schedule
--------

``schedule(spec) -> handle`` — Register a periodic callback, run every `every` seconds.

schedule_fires
--------------

``schedule_fires() -> {[name]=n}`` — How many times each named schedule has fired.

schedule_skips
--------------

``schedule_skips() -> string`` — How many periodic-schedule ticks the daemon has skipped because the previous tick's callback was still running, and how many of those skips are still consecutive right now.

schedules
---------

``table`` — The `remuda.schedule` registry table, keyed by the handle `schedule()` returned.

send
----

``send(name, text) -> nil`` — Deliver a line of text to a session, with Enter appended.

session
-------

``session(name) -> session`` — A handle onto an existing session, by name.

sleep
-----

``sleep(seconds) -> nil`` — Block the calling image for a number of seconds.

tool
----

``tool(spec) -> word`` — Define a word and export it as an MCP tool.

tools
-----

``table`` — The `remuda.tool` registry table, keyed by tool name.

type_text
---------

``type_text(session, text, settle?) -> nil`` — Type text into a session and submit it with Return.

wait_for
--------

``wait_for(session, pattern, seconds?) -> string`` — Wait until a session's screen matches a Lua pattern, then answer with that screen. Fails when the deadline passes instead of answering with a screen that does not match. Use it after `send` rather than guessing a sleep.

window
------

``table`` — Namespace for the current screen window.

windows
-------

``table`` — The `remuda.window` registry table, keyed by window id.
