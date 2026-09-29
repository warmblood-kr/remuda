Remuda Lua runtime
==================

advice_list
-----------

``advice_list(path?) -> {{path, id, how, depth, owner}...}`` — Copies of the advice on one path or all, outermost first.

advice_member
-------------

``advice_member(path, id) -> boolean`` — Whether advice with this id is on a path.

advise
------

``advise(path, how, fn, opts) -> nil`` — Wrap the function at a `remuda.*` path. `how`: around|before|after|override|filter_args|filter_return|before_while|before_until. `opts`: `id` (required; same id replaces), `depth` (-100 outermost).

attach
------

``attach(name) -> nil`` — Deprecated alias for `remuda.session.attach`.

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

``close(name) -> nil`` — Deprecated alias for `remuda.session.close`.

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

expect
------

``expect(session, branches, options?) -> handle`` — Watch a session asynchronously. Branches match a Lua pattern or predicate and run a key list or callback; `continue` keeps watching. Options accept a bounded timeout and unknown-screen matcher/callback.

expect_option
-------------

``expect_option(screen, label_predicate) -> number|nil`` — Pick a unique numbered menu option by its label.

extension_command
-----------------

``extension_command(name, handler(args, caller)) -> nil`` — Register a handler for an installed mod command.

fail
----

``fail(message, code?) -> never (default code 1; valid codes are 1..255)`` — Raise a deliberate CLI failure with a message and exit code.

feed
----

``feed(name, steps) -> nil`` — Deliver a sequence of bursts and pauses as one indivisible act.

fs
--

``table`` — Atomic replacement of files for trusted Lua callers.

fs.mkdir_new
------------

``fs.mkdir_new(path) -> true | nil, 'exists' | nil, error`` — Create one new directory without creating parents or trusting an existing path.

fs.write_atomic
---------------

``fs.write_atomic(path, bytes) -> true, nil | nil, error`` — Write bytes through a same-directory temporary file and atomically replace the target.

hook_list
---------

``hook_list(event?) -> {{event, group, id, depth, owner, src, errors, last_error}...}`` — Copies of the registered hooks, for one event or all, in run order.

hooks
-----

``table`` — Deprecated for reading: use `hook_list`. The `remuda.on` table, keyed by event name; it becomes read-only once no mod edits it by hand.

http
----

``http.request(options) -> {cancel()}`` — Start an asynchronous bounded HTTP request; completion is delivered on the Lua image queue.

input
-----

``table`` — Terminal input words for text delivery and submission.

input.submit
------------

``input.submit(session, expect) -> status`` — Submit visible composer text; returns 'submitted' or 'unverified'.

input.text
----------

``input.text(session, text) -> nil`` — Deliver text as one burst, using bracketed paste when enabled by the child.

input.type_text
---------------

``input.type_text(session, text, settle?) -> status`` — Type text, honor the settle pause, then return 'submitted' or 'unverified'.

insert
------

``insert(name, text) -> nil`` — Insert raw bytes into a session with nothing appended.

json
----

``table`` — Bounded JSON conversion for Lua values and UTF-8 JSON text.

json.array
----------

``json.array(table) -> table`` — Tag a dense Lua table as a JSON array, including an empty table.

json.decode
-----------

``json.decode(text) -> value, nil | nil, error`` — Decode strict UTF-8 JSON; repeated object keys and over-limit input return nil, error.

json.encode
-----------

``json.encode(value, options?) -> string`` — Encode a Lua value as bounded JSON; unsupported values raise a clear error.

json.null
---------

``value`` — The sentinel that represents JSON null in Lua tables.

json.object
-----------

``json.object(table) -> table`` — Tag a string-keyed Lua table as a JSON object, including an empty table.

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

``ls() -> {session...}`` — Deprecated alias for `remuda.session.list`.

mkdir
-----

``mkdir(dir) -> nil`` — Create a directory, including its parents.

new
---

``new(name?, argv?, cwd?, env?) -> string`` — Deprecated alias for `remuda.session.new`.

on
--

``on(event, fn, opts?) -> nil`` — Register a callback to run when an event fires. `opts`: `group`, `id` (same group+id replaces), `depth` (-100..100, lower first).

pending
-------

``pending({timeout?, on_cancel?}) -> handle`` — Return a bounded handle for an extension command's deferred result.

process
-------

``process(spec) -> id; process.run(spec) -> {code, stdout, stderr, timed_out}`` — Spawn an asynchronous plain-pipe child; process.run executes argv synchronously with bounded timeout and output.

process.run
-----------

``process.run{argv, stdin?, timeout?} -> {code, stdout, stderr, timed_out, signal?}`` — Run argv directly without a shell; inherits the daemon's environment and working directory. Blocks the Lua image until exit or timeout (default 5s, max 30s), captures each stream up to 1 MiB. Surviving descendants can keep pipes open; at most 16 background output readers are allowed.

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

``schedule(spec) -> handle`` — Register a periodic callback, run every `every` seconds; optional `after` sets the first firing delay from creation. Without it, the first firing depends on daemon uptime.

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

``session(name) -> handle; table {list, new, close, attach, resize}`` — Calling remuda.session(name) returns a handle onto that named session; the namespace also provides list, new, close, attach and resize.

session.attach
--------------

``session.attach(name) -> nil`` — Enter raw mode on a session.

session.close
-------------

``session.close(name) -> nil`` — End a session, live or already self-exited.

session.list
------------

``session.list() -> {session...}`` — List every session in the registry, reaping exited ones unless REMUDA_KEEP_EXITED is set.

session.new
-----------

``session.new(name?, argv?, cwd?, env?) -> string`` — Start a session, defaulting the command to the user's shell.

session.resize
--------------

``session.resize(name, cols, rows) -> true | nil, err`` — Resize a session's terminal (cols 20..1000, rows 24..500).

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

``type_text(session, text, settle?) -> status`` — Type text into a session and submit it with Return; returns 'submitted' or 'unverified'.

unadvise
--------

``unadvise(path, id) -> nil`` — Remove the advice with this id from a path; the last one removed restores the original.

wait_for
--------

``wait_for(session, pattern, seconds?) -> string`` — Wait until a session's screen matches a Lua pattern, then answer with that screen. Fails when the deadline passes instead of answering with a screen that does not match. Use it after `send` rather than guessing a sleep.

window
------

``table`` — Namespace for the current screen window.

windows
-------

``table`` — The `remuda.window` registry table, keyed by window id.
