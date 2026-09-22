# Design

This is the shape of remuda's scripting surface, in present tense — what it
is and why, not a log of how it got here. History belongs in `git log` and
`steps/`; this file gets rewritten in place as the shape changes.

## Two layers

Rust holds the engine: sessions, ptys, the daemon, the wire protocol. It
knows nothing about vocabulary — no notion of a "tool," a "schedule," or a
named buffer. Lua holds the vocabulary: `remuda.*` is a language built on top
of a small set of Rust-bound primitives (`send`, `capture`, `ls`, and their
siblings), the same split `PRINCIPLES.md` draws between policy and host.
Building that vocabulary in Rust would mean rebuilding and redeploying every
time it grows; building it in Lua means a new word costs neither.

## One registry for every word

A word is any name a script can call on the `remuda` table or reach through
`remuda.tools`, regardless of which layer defines it: a Rust binding
(`remuda.ls`), a plain Lua function (`remuda.schedule`), or something
registered dynamically through `remuda.tool`. All three write into the same
place, `remuda._registry`, keyed by name, each entry carrying `name`,
`about`, and `signature`.

Rust populates its own bindings' entries before `tools.lua` loads
(`script.rs`'s `WORDS` table, written into `remuda._registry` by
`registry_bindings`). `tools.lua` populates the rest as it defines each word,
through a small `register(name, about, signature)` helper — and `remuda.tool`
calls that helper itself, so anything registered as an MCP tool documents
itself for free, with no second place to remember it.

`remuda doc` renders the registry as a sorted, human-readable manual
(`remuda._registry_dump`) — one line per word, generated from the same data
the registry holds, never hand-maintained. A CI test walks every name in
`script::BINDINGS` and every key of `remuda.tools` and fails if any of them
has no registry entry, so a word that exists but was never documented is a
build failure, not a gap someone notices later.

`remuda.*` stays the canonical spelling for every word. A bare-name alias — a
global that skips the `remuda.` prefix — is something only a handful of the
highest-frequency verbs would ever earn, decided one at a time rather than
granted by default; `send`, `new`, and `ls` never get one, since a bare verb
that common is also common enough to collide with something a script already
defined. No bare-name alias exists today; candidates would be read-only verbs
such as `capture`/`sleep`, added the day a real ergonomic complaint appears
rather than speculatively.

## Session ≠ buffer

A session is a live process this daemon runs — `remuda.ls()`'s own rows,
`remuda.send`/`capture`/`key` and their siblings all name one by its session
name. A buffer is Lua-owned text with no process behind it, created with
`remuda.buffer.new(name)` and holding nothing but a name and a string.
Nothing about a session is a buffer and nothing about a buffer is a session,
even where they share a name.

`remuda.session(name)` is a handle onto an existing session. `session.buffer`
is a convenience — the buffer named after that session, created on first
access — never the session itself; `buffer.set(name, text)` reaches the same
buffer without going through a session handle at all, for a caller that only
ever had the name. `session.is_busy` exists because a session is a process
that can be doing work; a buffer cannot be, so it never carries `is_busy` or
a `context_left`, no matter whose content it holds.

## A schedule's identity is its handle, never its name

`remuda.schedule(spec)` returns a handle; `remuda.cancel(handle)` is the only
way to remove it. `spec.name` is an optional label — useful for a person
reading a listing, never used to look a schedule up or to decide whether two
registrations are "the same" one. Two schedules may share a label, or carry
none at all, and neither fact changes whether they coexist: the registry is
keyed by the handle itself, so a second `remuda.schedule{name = "x", ...}`
never replaces a first one also labelled `"x"` the way redefining a
`remuda.tool` word does on purpose. Cancelling is idempotent — cancelling an
already-cancelled or unrecognized handle is silent, not an error, since
"this is no longer registered" is exactly the state a caller was asking for.

`remuda.schedule_fires()` returns `{[name]=n}`, a shallow copy counting how
many times each NAMED schedule's `run` has actually fired (registered or
skipped ticks don't count). An unnamed schedule is never a key at all — no
consecutive-skip variant either, `remuda.schedule_skips()` already covers that.

## Hooks, on Emacs's augroup model

`remuda.on(event, fn, {group = ...})` registers a callback under a named
event; `remuda.emit(event, ...)` runs every callback registered for that
event, in the order they were added. `group` is optional to register with —
naming one costs nothing — but required to clear by:
`remuda.clear_hooks({group = ...})` removes every hook in that group, across
every event it touched, and refuses to run without a group at all. An
augroup clears as a unit for the same reason a schedule's identity is its
handle and not its name: without it, one extension's cleanup could reach
another's hooks, or a script's own second `require` of itself could double
every hook it thought it was replacing.

`on`/`emit`/`clear_hooks` are general-purpose, the same way Emacs's
`add-hook`/`run-hooks` presuppose nothing about which hook variable is being
run — a caller may name its own events and call `emit` at whatever moment
matters to it. remuda itself defines exactly one: `session_exited`, whose one
positional argument is the dead session's name (a Lua string). It fires once
per session the daemon notices has died, regardless of what triggered the
detection — a tick, a `List`, or an `ls()` call.

`remuda.event_counts()` returns `{[event]=n}`, a shallow copy counting every
`emit` call for that name, whether or not any hook is registered for it. A
name never emitted is absent, not zero; no error-count variant either — YAGNI
until a caller shows a need.

## A package is a name and an entry file, nothing more

`remuda exec <name>` runs a package's entry file in the daemon's own living
image — the same image `remuda lua <path>` and `remuda -e <code>` share, not
a fresh interpreter, so what the entry file does is visible to whatever runs
against that daemon afterward. There is no registry: a name either resolves
to an entry file or it doesn't.

Today, resolution is a built-in lookup compiled into the `remuda` binary
itself (`packages/<name>/init.lua`, embedded at compile time), because the
distributed binary carries no source checkout to read a package directory
from at runtime. A package still lives on disk as a real, editable
`packages/<name>/init.lua` file in this repo; only how that file reaches the
running daemon is compiled-in rather than looked up. An install mechanism,
when it exists, changes only that resolution step — cloning a package into a
runtime directory and reading its `init.lua` from disk instead of from the
binary — the entry-file convention itself does not change, so a package
written today keeps working once installing replaces embedding.

The same embedded manifest drives `remuda extension list`. It reports each
embedded package's name, build version, source, and `installed` status in RST,
Markdown, or JSON; the command does not maintain a second package catalog.

## A process's output arrives as events, never as something Lua waits on

`remuda.process{argv=, on_line=, on_exit=}` spawns a plain-pipe child — never
a pty, never a terminal emulator in the way — and delivers its stdout, one
line at a time, as `remuda.emit(on_line, line)` calls; when it exits,
`remuda.emit(on_exit, code)` fires once, after every line already produced
has been delivered. `remuda.kill(id)` ends it; `remuda.processes()` lists
the ids still running. `remuda.process` returns that id directly — there is
no opaque handle table, because nothing about a process needs one: unlike a
schedule, two processes never need to share a label to begin with, so a
plain id is already unambiguous.

Nothing in this call ever runs inside the Image's own thread except the O(1)
bookkeeping of a shared buffer. A separate thread owns the child, reads its
lines, and buffers them; the Image only ever *receives* work, the same
`on(event, fn)`/`emit` pair hooks already exist for. This is what makes
`remuda.process` the general answer the schedule ticker and the socket
handler were already special cases of: something outside the Image posts a
job, the Image runs it whenever its FIFO gets there, and nothing outside
ever blocks waiting for that to happen.

The buffer between the reader thread and the Image is capped, on purpose.
When it fills, the reader thread simply stops reading — the child's own
`write()` blocks against the now-full OS pipe, exactly the way a slow
consumer is supposed to make a fast producer wait. Lines are handed to Lua
in bounded batches, one Image job per batch, and only one such job is ever
outstanding per process: a batch that does not empty the buffer resubmits
itself instead of looping in place, so a single flooding process can never
hold the Image's queue for longer than one batch at a time — the schedule
ticker and any other caller queued behind it still get a turn in between.

remuda has no Matrix-specific code anywhere, and none is planned here: the
network side of a real inbound helper — a Matrix `/sync` long-poll, printing
one JSON line per event — is an ordinary `remuda.process` caller, arriving
in a later change. This primitive's own tests use a small line-printing
helper and know nothing about Matrix, reconnects, or backoff.

## Butler is a local session manager with an optional Matrix bridge

`packages/butler` runs one Claude Code session. It works locally with no
Matrix configuration. When both Matrix credential files are configured, it
also bridges one Matrix room and exposes one reply MCP tool — the same three
shapes as everything above it, composed rather than special-cased. The inbound
side is a small Python `/sync` long-poll (the caller `remuda.process` was built
for), the outbound side is a small bash sender (a `remuda.process` `run`
callback), and the session in between is an ordinary `remuda.new` session
fed with `remuda.send` — nothing about this package needs a new primitive.

### Topics

Butler's own service session has a stable, Remuda-owned workspace at
`${XDG_DATA_HOME:-$HOME/.local/share}/remuda/butler/sessions/butler`. Agent
work belongs elsewhere: `remuda butler topic new NAME` creates
`NAME` below `~/projects` by default and launches its agent with that topic
directory as its cwd. `REMUDA_BUTLER_PROJECT_HOME` overrides that default.

Topics are configured in the trusted user file
`${XDG_CONFIG_HOME:-$HOME/.config}/remuda/butler/topics.lua` (or an explicit
`REMUDA_BUTLER_TOPICS` path). It is read only when a topic is created, so an
absent or broken optional template does not prevent the Butler service from
starting. The file returns a table; a template is ordinary Lua and is free to
write files or run setup commands:

```lua
return {
  project_home = "~/projects",
  templates = {
    monocle = function(topic)
      topic.write("CLAUDE.md", "# Monocle topic\n")
      topic.run({"gh", "repo", "clone", "warmblood-kr/monocle", "."})
    end,
  },
}
```

`remuda butler topic new research --template monocle` creates the directory,
runs the template from inside it, and starts a `claude` session named
`research`. `--agent codex` selects another registered agent builder. With no
template, a topic is simply an empty directory.

Template parsing and setup happen only within `topic new`. A bad Lua template
or a failed setup command reports that topic command as an error; it never
stops the already-running Butler service or its other agents.

### Teams and completed work loops

`remuda butler topic delegate NAME TASK...` creates a topic and makes its
agent a child of the root Butler. It immediately sends `TASK` to that child.
The default child adapter is its leader's adapter, so a Codex Butler creates a
Codex child; `--agent codex` selects it explicitly. `remuda butler sessions`
shows each session's `LEADER` column.

The coordination contract is CLI-first, not an MCP tool schema repeatedly
injected into every agent context. A child reports a completed loop with
`remuda butler send-to-leader RESULT...`. Butler injects
`REMUDA_BUTLER_SESSION_NAME` into every agent process, and its leader is found
from the stored relationship. The result is queued for the leader's `butler inbox` and
emitted as the live `butler/report` hook (`from`, `to`, `text`). A team member
creates a nested child with `remuda butler topic delegate NAME --leader SELF
TASK...`, then directs it with `remuda butler send SELF CHILD MESSAGE...`.

### Local mail

Butler messages have an email-shaped JSON envelope: stable message ID, local
`{host, session}` sender and recipient addresses, subject, UTC creation time,
content type, and a body object reference. The terminal receives only a short
notification with the message ID and sender; `remuda butler inbox NAME` reads
the body. The body is not duplicated into the terminal transcript.

For now, each local send also writes durable files below
`${XDG_DATA_HOME:-$HOME/.local/share}/remuda/butler/mail`: a raw body object,
a JSON envelope, a recipient inbox JSONL entry, and read-state. A new Butler
Lua mailbox reloads unread entries from those files; read entries remain read.
This is local-only materialization, not yet a remote relay or hash worker.
Keeping the envelope and object reference distinct makes those later additions
compatible rather than migratory.

The helper and the session talk to each other through the same line-oriented
channel any `remuda.process` caller gets, so the format has to survive being
squeezed through it: `sender<TAB>escaped-body`, one physical line per Matrix
event, however many lines the original message body contained. The body is
escaped backslash-first, then newline (`\` becomes `\\`, a real newline
becomes `\n`), and unescaped on the way back in with a single left-to-right
pass over each backslash-and-next-character pair — not two independent
passes, which would let an escaped backslash sitting right before a literal
"n" in the original text be misread as a newline escape that was never
there. `remuda.process`'s pipe delivers whole lines and nothing else, so a
message with an embedded newline that arrived as two physical lines would be
indistinguishable from two separate messages; making it one line here is
what keeps that guarantee true one level up.

The helper allowlists a single configured room and a configured set of
human sender mxids, and treats any other room, any sender outside that set,
or its own outbound messages as invisible — not filtered after the fact,
never read in the first place. This is a deliberately narrower scope than
an existing cross-fleet bridge this design draws on, which polls every
joined room on purpose for its own multi-room use case; a bridge that hands
one Claude Code session's shell to whoever is behind the message has to
draw both boundaries — the room and the person — or any member of that room
could drive the session, not just the person it is meant to answer to.

The reply tool is fire-and-forget on purpose: it returns as soon as the
outbound send is queued as a `remuda.process`, not once the message is
actually delivered. Delivery's real outcome — success or the specific
failure — arrives separately, as that process's `on_exit` event, exactly the
way any other `remuda.process` caller finds out how its child actually
ended. A caller that needs to know whether a given reply landed watches for
that event; nothing about the tool's own return value promises it.
