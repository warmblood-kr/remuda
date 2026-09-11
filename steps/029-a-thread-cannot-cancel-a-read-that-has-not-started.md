# 029 — a thread cannot cancel a read that has not started

`Hold::drop` (`client.rs:252-261`, `wake`/`stop_reader` call at `:257`) and
the daemon's own `attach` hangup path (`daemon.rs:308-397`, call at `:394`)
both release a session by unblocking a thread parked reading the connection.
On Windows this hangs, reproducibly, when the drop happens on the same
thread that took the hold — which is exactly what the TUI's own detach and
quit do (`git show 3ccf3b29`: `tui.rs:865`, `tui.rs:962`). This step is the
mechanism reading required before any fix is written, and — after a first
design turned out to have a gap (below) — now also records the fix actually
implemented and locally verified. CI confirmation on real `windows-latest`
hardware is the next step (run 9).

## Before

Three earlier diagnostic PRs (#26, #28, #29 — not this repo's step numbers,
GitHub PR numbers) measured, on real `windows-latest` hardware:

1. **Immediate same-thread drop hangs.** `hold()` → `drop()` on the same
   thread, no pause: hung 3-for-3 (in-process ×2, out-of-process ×1),
   always at the same point — inside `drop(hold_a)`/`Hold::drop` itself.
2. **A ~1-second pause passes.** Inside one out-of-process test, `hold(A)` →
   sleep(1s) → `drop(A)` → `hold(B)` → `drop(B)`: the *paused* drop (A)
   succeeded; the *very next, unpaused* drop (B), same test, same process,
   hung. One data point, but a within-run comparison rather than a
   cross-run one.
3. **Cross-thread passes, 5-for-5.** The identical hold/drop sequence, with
   only the drop wrapped in `std::thread::spawn`, has never hung.
4. **Daemon process locality does not matter.** The out-of-process test (a
   real spawned `remuda daemon` subprocess, not an in-process thread) hung
   identically to the in-process ones.

## Mechanism (reasoned from source; the race itself is not yet measured
directly — only its symptom is)

Read from the vendored `interprocess-2.4.4` crate source (confirmed, not
inferred):

- The Windows named-pipe handle is opened `FILE_FLAG_OVERLAPPED` — genuinely
  asynchronous, not a plain blocking handle.
- The crate's blocking `Read::read` is `ReadFileEx` plus an APC completion
  routine, with the calling thread parked in `SleepEx(_, alertable = 1)`
  until ITS OWN thread's APC queue delivers the read's completion. This is
  confirmed source, and it is the reason the fix below works with the grain
  of the crate rather than around it.
- `try_clone` (`Hold`'s `drain` thread gets a cloned stream; `daemon.rs`'s
  key-input pump gets one too) calls `DuplicateHandle` — a new handle value,
  same underlying pipe object. This was the *original* candidate mechanism
  (`steps/010-windows.md:271-274`) and it is retired: the clone is identical
  in every test above, same-thread and cross-thread alike, so it cannot be
  what discriminates between them.
- The crate calls no `CancelIo*` function anywhere; all cancellation is this
  repo's own `ipc::wake`, which calls `CancelIoEx(handle, NULL)` — the
  documented, correct API family for an overlapped handle. `CancelSynchronousIo`
  was checked and does not apply (that lead is closed, see the PR #26/#28
  write-up in `steps/030-a-red-test-before-a-fix.md` on those branches).

**The candidate that fits all three discriminators at once:**
`CancelIoEx` cancels *pending* I/O. A `ReadFileEx` call that has not yet
reached the kernel is not pending, and cancelling before it exists is a
documented no-op with respect to that future read — nothing is "remembered"
for a request that hasn't been issued yet. `hold()` spawns the `drain`
thread and returns immediately, before that thread has necessarily been
scheduled at all, let alone reached its first `ReadFileEx` call.

- **Immediate same-thread hangs** because `drop()` runs right after
  `hold()` returns, with nothing in between to let the freshly spawned
  drain thread get scheduled and issue its read — `wake()` very likely
  fires first, cancels nothing, and the read that starts moments later
  blocks forever.
- **A pause passes** because it gives the OS scheduler time to actually run
  the drain thread and let it reach `ReadFileEx` before `wake()` ever
  fires — by the time the cancel arrives, there is something to cancel.
- **Cross-thread passes** for the *same* reason, not a different one:
  wrapping the drop in `std::thread::spawn` costs real OS thread-creation
  and scheduling latency before the drop actually runs — that latency acts
  as an incidental pause, wide enough (so far, 5-for-5) to let the
  already-spawned drain thread win the race first. It is not that a
  cross-thread cancel is somehow more powerful than a same-thread one;
  `CancelIoEx` is documented to work across threads on the same handle
  either way. The variable is *when* the cancel fires, never *which
  thread* fires it.
- **Daemon locality does not matter** because the entire race is
  client-local (or, for the daemon's own hangup path, server-local): both
  the drain/pump thread and the thread calling `wake` live in the same
  process regardless of where the *other* end of the pipe runs. The
  daemon's process boundary never enters into this race at all.

**What this reasoning cannot reach without a direct measurement:** whether
this is truly the exact race (rather than some other overlapped-I/O
subtlety with the same symptom), and where the real threshold sits for this
specific machine's scheduler. Run 8 (tests only, no fix) measured both: the
threshold is sharp, not "~1s" as the single earlier data point suggested —
**0ms hangs, 10ms and every larger value in {10, 50, 100, 250, 500, 1000}ms
pass**, on both an in-process and a real out-of-process daemon — and a
direct reader-side injection (delaying the drain thread's first read by 2s
regardless of caller timing) hung same-thread even at "0ms caller pause",
confirming the race is caller-independent, not merely narrow.

## Desired outcome

A structural fix, applied to `Hold::drop` and `daemon.rs`'s attach hangup
path, that is correct for an **immediate** drop — no sleep, retry loop, or
"poll until pending" delay that merely narrows the window rather than
closing it. What follows is the design history: a first candidate that
turned out to have a gap, the invariant that gap named, and the design
actually implemented and locally verified below.

### Gap found in the first fix design, before any of it was implemented

The design this step originally carried forward queued the cancel as a
Windows User APC onto the reader thread itself (`QueueUserAPC`), reasoning
that a queued APC can only run at the reader's next alertable wait, and the
crate's `ReadFileEx` is issued synchronously before that wait — so the
earliest the reader could ever process the APC is at or after its read was
issued. That ordering guarantee is correct *for the first read*. It does
not cover every read after the first.

`CancelIoEx` — however it gets invoked, directly or via a queued APC — acts
on a *pending* I/O operation. It is an **event**, not a **state**: firing it
answers "is something pending right now", not "block all future reads from
here on". A queued APC is delivered inside whichever alertable wait comes
first. If that happens to be the same `SleepEx` in which the drain thread's
*current* read has already completed with real data — the APC and the
completion racing to be "the" event that wakes that `SleepEx` — the
`CancelIoEx` inside the APC finds nothing pending, `read()` returns `Ok(n)`
normally, and the drain loop immediately issues its *next* `ReadFileEx`. The
same hang this whole step exists to fix, now reappears between two reads
instead of before the first one. The original design's "cannot fire early"
guarantee is real, but it only ever covered the *first* read.

Named pattern, second instance in this codebase in one day: **a momentary
signal cannot answer a question about an interval.** The first instance was
`#27`'s `attached: AtomicBool` flag — a single flip could not tell "has *this*
attachment ended" from "has *some* attachment ended", which needed
`attach_generation` instead of a plain flag. Here, a single cancel event
cannot tell "nothing is pending yet" from "nothing is pending anymore,
because a real read just satisfied it and another read is about to start" —
what's actually needed is not a smarter event, but a **persistent state**
the reader consults on its own, before every read, independent of whether
any particular cancel happened to land.

### Required invariant

Any fix must give the drain/pump loop a **sticky stop flag**, set *before*
any cancel and checked by the loop *before every read, not only the first*.
The cancel then only has to handle "a read is currently blocked" — the flag
alone handles "no read should start next", closing exactly the gap the APC
design left open. This applies identically to `daemon.rs`'s key-pump thread
(`daemon.rs:355-365` as of `e6465d3`), which had the same shape and the same
gap: its `scope.spawn` return value was discarded, so nothing could even ask
it whether it had finished.

Two designs satisfy the invariant:

1. **APC + flag.** Keep `QueueUserAPC`, add the sticky flag the drain loop
   checks before each read. Correct, but needs the reader thread's raw
   handle (`AsRawHandle`) at every call site, a new `windows-sys` feature
   (`Win32_System_Threading`), and an `unsafe extern "system"` callback.
   `std::thread::JoinHandle` implements `AsRawHandle`; `std::thread::
   ScopedJoinHandle` (used by `daemon.rs`'s `scope.spawn`) does not — this
   design does not typecheck at the one call site that most needs it
   without an additional workaround.
2. **Flag + retry-until-finished, no APC.** `Drop`/hangup: `stop.store(true,
   SeqCst)`, then `loop { wake(stream); if reader.is_finished() { break };
   sleep(1ms) }`, then `join()`. The flag covers "not yet reading, or
   between two reads"; the retry covers "currently reading, but the first
   cancel arrived before that read was pending" — cheap because `wake`
   (`CancelIoEx`) is a documented no-op on a non-pending read, so retrying
   it costs nothing but a 1ms sleep until the read actually becomes
   cancellable.

**Chosen: (2).** It needs no raw thread handle, no new `windows-sys`
feature, no `unsafe extern "system"` callback, and — unlike (1) — it
typechecks identically for `std::thread::JoinHandle` (`client.rs`'s `Hold`)
and `std::thread::ScopedJoinHandle` (`daemon.rs`'s key pump), since both
expose a plain `is_finished()` with no `AsRawHandle` bound. It is also
easier to argue correct: the flag's coverage is exact (a loop iteration
either sees the flag before starting a read, or that read is the one the
retry loop is entitled to keep cancelling), where (1)'s guarantee — "the APC
cannot run before the first read is issued" — never actually named the gap
between reads until this analysis found it by inspection, which is reason
enough to prefer the design whose correctness argument does not depend on a
similarly-overlooked case elsewhere in the APC-delivery contract.

**Its ceiling:** a bounded busy-wait, sleeping 1ms between `wake` retries,
for as long as the reader thread's current read remains not-yet-cancellable
— expected to be at most a few milliseconds in practice (kernel scheduling
latency, not application-level blocking), but with no OS-documented hard
upper bound. Acceptable here because `Hold::drop` and the attach hangup path
already block their caller until the reader thread exits (`join()`
afterward); the retry loop does not introduce a new blocking wait, only
gives the existing one a bounded-step shape instead of a single blind
`CancelIoEx` call.

### Audit against the named pattern

Checked every call site in the file region this fix is scoped to:

- `client.rs`'s `Hold::drop` (`:255-260`, call at `:257`) — **had the gap,
  now fixed**: the drain loop (`hold_inner`, `:203-240`) now checks a
  `stop: Arc<AtomicBool>` before every read, not just before spawning; `drop`
  calls the new `ipc::stop_reader`.
- `daemon.rs`'s `attach` hangup path (`:308-397`, call at `:394`) — **had
  the same gap, now fixed** identically: the key-pump loop (`:361-374`)
  checks a `stop: AtomicBool` (plain, not `Arc` — the pump is a scoped
  thread borrowing from the enclosing stack frame, so no shared ownership
  is needed) before every read; the hangup call captures the `scope.spawn`
  return value (`key_thread`, previously discarded) so `is_finished()` has
  something to poll.
- `ipc::wake` itself (`:52-76`) — **unchanged**. It remains a single
  best-effort `CancelIoEx`/`shutdown` call; the invariant is enforced by its
  two callers (`stop_reader`'s loop), not by `wake` growing a retry of its
  own, so `wake`'s two other call sites keep their original, narrower
  contract.
- `client::attach()`'s two `ipc::wake` call sites (`client.rs:145`, `:160`)
  — **flagged, not touched.** Same clone-and-wake shape, used by `remuda
  run`/`attach` rather than the TUI's `Hold`, out of this PR's file-region
  scope. If these have the same gap it is a separate, unaudited bug — no
  repo-wide audit was done here.

## Expected

Local (Linux + Windows-target `cargo check`/`clippy`, no real Windows
hardware) verification only, before run 9's CI:
- all 22 tests in `native/tests/hold_drop_race.rs` (19 from run 8, plus a
  null control and an in/out-of-process streaming-drop loop added for this
  step) pass on Linux, where the race does not exist — a regression check,
  not evidence the Windows race is fixed.
- `cargo check`/`cargo clippy --all-targets -D warnings` clean on both the
  native (Linux) target and `x86_64-pc-windows-gnu` (the Windows code paths
  compile and lint clean; they cannot be *executed* here).
- The real claim — that the sticky flag actually closes the race on
  Windows, including the two new tests' pre-fix-red scenarios (between-read
  cancellation via streaming output; the two reader-injection tests) — is
  only established by run 9's `windows-latest` CI, not by anything in this
  local pass.

## Actual

The crate-source claims from before the fix, captured directly (not from
memory) and still accurate — the crate's own read loop did not change:

```
$ grep -n "CancelIoEx\|CancelSynchronousIo\|CancelIo\b" \
    ~/.cargo/registry/src/*/interprocess-2.4.4/src -r
(no output — zero matches; the crate calls no CancelIo* function itself)

$ grep -n "FILE_FLAG_OVERLAPPED" \
    ~/.cargo/registry/src/*/interprocess-2.4.4/src/os/windows/named_pipe/c_wrappers.rs
16:            CreateFileW, ReOpenFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
164:            FILE_FLAG_OVERLAPPED,
182:    unsafe { ReOpenFile(h.as_raw_handle(), access_flags, NP_SHARE_MODE, FILE_FLAG_OVERLAPPED) }
```

Post-fix call sites, captured directly:

```
$ grep -n "ipc::wake\|ipc::stop_reader" native/src/*.rs
native/src/client.rs:145:            ipc::wake(&stream);
native/src/client.rs:160:    ipc::wake(&stream);
native/src/client.rs:257:            ipc::stop_reader(&self.stream, &self.stop, || drain.is_finished());
native/src/daemon.rs:394:        ipc::stop_reader(&stream, stop, || key_thread.is_finished());
```

Local verification, captured directly:

```
$ cargo test -p remuda-native --test hold_drop_race
test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.78s

$ cargo test --workspace
(every target) test result: ok. 0 failed, across all workspace test binaries

$ cargo check -p remuda-native --target x86_64-pc-windows-gnu
Finished `dev` profile [unoptimized + debuginfo] target(s)

$ cargo clippy -p remuda-native --all-targets -- -D warnings
$ cargo clippy -p remuda-native --target x86_64-pc-windows-gnu --all-targets -- -D warnings
(both) Finished `dev` profile [unoptimized + debuginfo] target(s) — no warnings

$ cargo fmt -p remuda-native -- --check
(clean after `cargo fmt` was applied once)
```

## Not verified

- Whether the fixed code actually resolves the race on real `windows-latest`
  hardware — that is run 9's CI, not anything measured locally. Everything
  in this file as of this edit is reasoning plus Linux-only/cross-compile
  local checks.
- Whether the 1ms retry-loop spin in `ipc::stop_reader` has any
  Windows-specific scheduling quirk of its own (e.g. `Sleep`'s granularity
  being coarser than 1ms on some Windows configurations) — expected to only
  ever make the retry loop take slightly longer per iteration, never
  incorrect, but not directly measured.
- `streaming_drop_loop_{in,out}_of_process` and `null_control_no_attach_out_of_process`
  never ran against pre-fix code on real Windows hardware — only run 9's
  post-fix code has. Their green on run 9 shows no regression from adding
  them, not that they can detect the bug they were written to cover. The
  only tests with an actual pre-fix-red-on-Windows, post-fix-green
  discriminating result are `cross_thread_hold_drop_with_injection`,
  `same_thread_hold_drop_with_injection`, `pause_0ms_{in,out}_of_process`,
  and the two `immediate_hold_drop_loop_*` tests (all measured red on run 8,
  green on run 9).
