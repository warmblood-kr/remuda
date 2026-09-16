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
