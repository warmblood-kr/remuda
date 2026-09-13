# 033 — an in-process call becomes a round trip when it crosses into Lua

The buffer/window migration (`steps/015`, `remuda.buffer`/`remuda.window`,
then the session list and the right-hand preview each re-expressed through
them) was checked against `steps/017`, `022`, `026a-c` and `031` for
regressions, and passed a byte-identical oracle at every step. Two real
regressions still slipped through, both invisible to that oracle because it
only ever asks "does the output look the same", never "how many round trips
did it cost to make it look the same".

## Before

**A: a per-keystroke sync the `skip_list` guard didn't cover.**
`window_shown_session` (`tui.rs`) — the `Eval` that syncs the window to the
selected session and reads back what it now shows — ran on every `refresh()`
call, including a `Type`-forced one (`skip_list=true`), the exact fast path
`steps/017`/`022` exist to keep to one round trip. A `Type`-forced refresh
cost two daemon requests (the sync, then the capture) instead of one.

**B: a loopback the migration introduced without meaning to.**
`tools.lua`'s `_refresh_sessions_buffer` — called once per non-skip_list
`refresh()`, itself already inside the daemon's own Lua image — calls
`remuda.ls()` to build the list. `remuda.ls()`'s binding (`script.rs`) asked
over the wire (`client::request` → a fresh socket connect back to the
daemon's own listener) rather than reading the `Registry` it was already
running next to. `tui.rs`'s own direct `list(path)` call, plus this loopback,
meant one non-skip_list refresh sent `Request::List` twice.

Both were measured, not guessed: a new `Counters` registry
(`tick.rs`, generalized from the ticker's existing skip counter) records
named counts at the daemon's one dispatch point (`daemon.rs::handle`),
exposed to a test — and to any Lua caller, `remuda.request_counts()` — as
one object. Two tests read deltas from it across one real `refresh()` call
and failed exactly as predicted before this step: 2 requests for a
`Type`-forced refresh with nothing changed, 2 `Request::List` for one tick
refresh.

## Desired outcome

A `Type`-forced refresh costs exactly one daemon request; a tick refresh
sends exactly one `Request::List`. Every existing byte-identical oracle
literal stays byte-identical — this step changes how cheaply the same output
is reached, never the output.

## Expected

- Fix A: `window_shown_session`'s `Eval` runs only when `!skip_list`. The
  premise this depends on — a `Type`-forced wake can never follow a
  selection change without a non-skip_list refresh landing in between first
  — holds structurally: `Ui::session_key` (the only source of
  `Action::Type`) never touches `Ui::selected`, and every action that does
  (`browse_key`'s Up/Down, a list-row click) never returns `Action::Type`;
  `run()` also forces an immediate non-skip_list-eligible refresh
  (`force_refresh = true`) after every single action. The last-synced target
  is cached (`shown: Option<String>`, owned by `run()` alongside
  `held`/`painted`) and reused as-is on the fast path.
- Fix B: `remuda.ls()`'s binding reads the daemon's `Registry` in-process —
  the same `reap`-then-`list` the wire handler already does, minus the
  socket round trip. Rejected: serializing `tui.rs`'s already-fetched list
  into the `Eval` as data instead. That would need a new Rust-to-Lua
  encoding for `SessionSummary`, change `_refresh_sessions_buffer`'s
  signature, and only fix the one call site `tui.rs` happens to use — every
  other `remuda.ls()` caller (a script, a schedule, a future MCP tool) would
  keep paying the round trip. Reading the registry directly fixes all of
  them, and gives up nothing the loopback was actually using: the image only
  ever runs inside its own daemon (`image.rs`'s own prior doc comment already
  said so).
- `tui::tests::a_type_forced_refresh_with_unchanged_selection_costs_one_daemon_request`
  and `tui::tests::a_tick_refresh_sends_exactly_one_request_list` (both added
  RED in the immediately preceding commit) turn green.
- Every existing byte-identical oracle test is untouched, and stays passing.

## Actual

```
$ cargo test --lib -- a_type_forced_refresh_with_unchanged_selection_costs_one_daemon_request a_tick_refresh_sends_exactly_one_request_list --test-threads=1 2>&1 | grep -E "^test |test result"
test tui::tests::a_tick_refresh_sends_exactly_one_request_list ... ok
test tui::tests::a_type_forced_refresh_with_unchanged_selection_costs_one_daemon_request ... ok
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 95 filtered out; finished in 0.04s

$ cargo test --workspace 2>&1 | grep -E "^test result"
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 27 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 97 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.12s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 0 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.01s
test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.78s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

$ cargo fmt --all -- --check
(clean)

$ cargo clippy --workspace --all-targets -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.47s

$ python3 scripts/check-comments.py
ok — 325 doc comment(s) within cap (item 3, module 20)

$ git diff HEAD~1 HEAD --stat -- native/src/tui.rs | tail -1
 native/src/tui.rs | 122 ++++++++++++++++++++++++++++++++++++++++++++++--------
$ git diff HEAD~1 HEAD -- native/src/tui.rs | grep -c '\x1b\['
0
(no byte-identical oracle literal appears in the diff — every hunk is
confined to refresh()/run() and the four tests that call refresh() directly)
```

## The general shape, for the next migration

Both regressions share one shape worth naming: **an in-process call becomes
a socket round trip the moment it crosses into Lua.** Nothing about moving
logic into Lua *requires* that cost — `remuda.ls()` proves the wire path was
a convenience (reuse `ask`/`client::request` like every other binding), not
a necessity, since the daemon and its own image are always the same process
(`image.rs`). The check worth adding to the next migration of this shape:
for a binding that only ever runs inside its own daemon, does it *need* the
wire, or is it reaching for the wire out of habit because every sibling
binding does?

The other lesson is about the oracle itself, not the bug: a byte-identical
oracle proves *sameness of output*. It cannot see the *cost of reaching*
that sameness — two round trips producing the same bytes as one pass the
same oracle that one round trip does. Neither regression here changed a
single byte `render_styled` produced; both only changed how many requests it
took to produce them. A migration that must preserve performance needs a
second oracle, one that counts, alongside the one that compares bytes.
