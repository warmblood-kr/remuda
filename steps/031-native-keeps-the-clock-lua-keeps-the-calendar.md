# 031 — native keeps the clock, Lua keeps the calendar

정수님 asked for a three-layer shape where "타이머도 있어야겠죠" — a timer belongs
to the programming layer, not the policy layer. The boundary that makes that
safe, stated as a rule rather than a feeling: 「N초마다 깨워준다」는 remuda가
알아도 되지만 「그때 컴팩션을 돌려라」는 remuda가 알면 안 된다. This step builds
exactly that split, and records three things about it that are true today but
invisible from reading the code — written here because the next reader who
needs them will be building on this file's own foundation, not reading a
session's private notes that were never pushed.

## Before

No periodic mechanism existed anywhere in remuda. `Eval`, `-e`, the REPL, and
MCP's `run_script` all converge on one synchronous, single-threaded,
unbounded-queue Lua image (`image.rs`) — a script runs when a caller asks,
never on its own schedule. A caller wanting "do X every N seconds" had two bad
options: hold a connection open and sleep-loop from outside, or block the
image forever in a Lua `while true do ... sleep ... end`, which stalls every
other caller (the REPL, `-e`, any other script) for as long as it runs.
Multiple independent extensions wanting their own periodic callback, at their
own interval, had no way to share one clock without knowing about each other.

## Desired outcome

A background clock, owned entirely by native, wakes the Lua image once per
fixed period and asks it only "what's due now?" — never "run compaction."
Multiple Lua-side registrants share this one clock via `remuda.schedule(spec)`,
mirroring the existing `remuda.tool(spec)` shape: each schedule stores its own
`every` (seconds) and its own `last_run`, and decides for itself whether it is
due, given only the timestamp native hands it. The clock must never block the
daemon and must never let unbounded work queue up behind a hung callback — and
a wedged callback should be a readable fact, not a silent leak.

## Expected

- `Ticker` (`native/src/tick.rs`) holds at most one in-flight Lua job at a
  time: each period it `try_recv()`s on the previous job's receiver; if
  answered, it resets and submits the next; if not yet answered, it skips this
  period rather than submitting a second concurrent job, and counts the skip.
  Queue depth in front of the image from this source never exceeds one extra
  job, no matter how long a schedule runs.
- `remuda.schedule(spec)` validates `name`/`every`/`run` and stores into
  `remuda.schedules[name]`; `remuda._run_due_schedules(now)` iterates that
  table once per tick and fires any schedule whose own interval has elapsed
  since its own last run. Native never compares an individual interval itself.
- The scheduler is **periodic**, not fire-once: a registered schedule keeps
  firing on every subsequent tick for as long as the daemon runs, not just the
  first time it becomes due.
- The two skip counters (`consecutive_skips`, `total_skips`) are readable from
  Lua via `remuda.schedule_skips()` — and therefore, via `mcp.rs`'s existing
  "an unknown tool name routes into Lua" behaviour, from MCP too, with no
  further wiring. No threshold, no alarm, no formatting: the counters are only
  an escalation *hook-point*, deliberately left unbuilt.

Three properties are true of this design but do not show up in a diff, so they
are recorded here rather than left for someone to rediscover:

1. **`TICK_PERIOD` is an unconditional wakeup rate, not only a latency
   ceiling.** Native hands Lua the current time every period regardless of
   whether any schedule is registered — a daemon with zero schedules still
   gets one Lua job per `TICK_PERIOD`, forever. Lowering the period buys
   faster schedules and pays for it with a background job at that same new
   rate in *every* daemon. Not fixed here: the in-flight guard already bounds
   the cost, and a lazy-start ticker would need Lua to signal Rust on
   registration, more machinery than the cost justifies — but the tradeoff
   should be visible to whoever changes the constant, not discovered by them.

2. **A tick period is simultaneously a hook's latency ceiling and its
   resolution floor.** A state transition that fully reverts within one
   period (idle → busy → idle) is invisible to anything sampling once per
   period — as far as a periodic scan can tell, it never happened. This fails
   asymmetrically: the sessions whose transitions coalesce fastest are the
   *busy* ones, so idle-gated housekeeping (a future compaction check, for
   instance) degrades hardest on exactly the sessions it exists to help.

3. **The reap-then-snapshot ordering precondition, for a future idle/exit scan
   built on this same tick.** No such scan exists yet — this tick only drives
   `remuda.schedule`, nothing here observes session identity — but the moment
   one is built, it needs to tell "the same session, still idle" from "a
   session that died and a new one reused its name" within one tick period:
   the ABA problem for a census keyed only on name. That distinction is
   achievable with **no core or native change**, purely as scan-local
   bookkeeping, *provided* two things hold — both true today, both confirmed
   directly against `core/src/registry.rs` while writing this step:

   - `Registry::register()` rejects a duplicate name outright while a
     dead-but-unreaped entry still occupies it
     (`registry.rs:81-83`: `if sessions.contains_key(session.name()) { return
     Err(session); }`) — a name can never silently refer to two different
     processes at once.
   - The scan's live-name snapshot for a given tick must be taken **after**
     calling `Registry::reap()` for that same tick, never before.
     `reap()` (`registry.rs:135-146`) removes every dead-but-unreaped entry
     and returns their names; if the snapshot is taken first, a session that
     exits and is immediately replaced by a new one under the same name
     *within the same tick* reads as one continuous session across the gap —
     the ABA failure, and it fails silently, producing a plausible but wrong
     idle duration rather than an error.

   Given both hold, the rule for whoever builds the scan is: **a name absent
   from the immediately-preceding tick's live snapshot has no prior idle
   history** — treat it as freshly seen, never as a continuation. That claim
   is false, and silently so, the instant the snapshot predates that tick's
   `reap()` call. This is a note to build correctly on, not a change made
   now — the same "wait until there is code to hang it on" reasoning
   `steps/014-a-tool-registry.md:292-295` uses for
   `~/.config/remuda/init.lua`.

No config, no persistence: like `remuda.tools`
(`steps/014-a-tool-registry.md:292-295`), a registered schedule is in-memory
only and does not survive a daemon restart. Same ceiling, not solved here, on
purpose.

## Actual

```
$ cargo test -p remuda-native --lib tick::
running 1 test
test tick::tests::a_still_running_callback_is_skipped_not_queued_behind ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 87 filtered out; finished in 0.00s

$ cargo test --test daemon a_registered_schedule_actually_fires_through_a_real_daemon --manifest-path native/Cargo.toml
running 1 test
test a_registered_schedule_actually_fires_through_a_real_daemon ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 11 filtered out; finished in 2.02s

$ remuda -s skiptest -e 'return remuda.schedule_skips()'
{consecutive = 0, total = 0}
$ remuda -s skiptest -e 'local t = remuda.schedule_skips(); return type(t.consecutive) .. "," .. type(t.total) .. "," .. tostring(t.consecutive) .. "," .. tostring(t.total)'
number,number,0,0

$ cargo test --workspace
test result: ok. 88 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (native lib)
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (native/tests/daemon.rs)
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out    (native/tests/script.rs)
...all suites green, workspace-wide

$ cargo check -p remuda-core --target wasm32-unknown-unknown
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.03s
$ git status --short -- core/
(empty — no core/ files touched by this step)
```

The e2e test was strengthened during this step from asserting "fired at least
once" to asserting "fired at least twice" — a scheduler that fires once and
then stops is a real failure mode for the intended first consumer (a periodic
compaction check), and the earlier assertion would have passed it silently.
