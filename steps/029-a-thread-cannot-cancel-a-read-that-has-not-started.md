# 029 — a thread cannot cancel a read that has not started

`Hold::drop` (`client.rs:229-237`) and the daemon's own `attach` hangup path
(`daemon.rs:308-384`) both release a session by calling `ipc::wake` to
force-unblock a thread parked reading the connection. On Windows this hangs,
reproducibly, when the drop happens on the same thread that took the hold —
which is exactly what the TUI's own detach and quit do (`git show 3ccf3b29`:
`tui.rs:865`, `tui.rs:962`). This step is the mechanism reading required
before any fix is written. No code changes here.

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
subtlety with the same symptom), and where between 0ms and 1000ms the real
threshold sits for this specific machine's scheduler. The next step
(a *test-only* run, no fix) is designed to measure both: a pause sweep
across several values, and a direct reader-side injection that forces the
race regardless of the caller's own timing — the second is the test that
distinguishes a real fix (which must not depend on any caller-side delay at
all) from a fix that only narrows the window (which would still pass every
pause the sweep tries, for the same reason the un-fixed code already passes
a 1-second one).

## Desired outcome

A structural fix to `ipc::wake`'s Windows path that is correct for an
**immediate** drop — no sleep, retry loop, or "poll until pending" delay
that merely narrows the window rather than closing it. The design carried
into the next step: instead of calling `CancelIoEx` directly from whichever
thread drops the `Hold`, queue the cancel as a Windows **User APC** onto the
*reader* thread itself (`QueueUserAPC`), using the reader thread's own
handle (available on both `client.rs`'s `JoinHandle` and `daemon.rs`'s
`ScopedJoinHandle` via `AsRawHandle`). A queued APC is delivered only when
its target thread performs an alertable wait — and the crate's `ReadFileEx`
call happens *synchronously*, before the reader thread's first such wait,
in the very same function invocation. So the earliest the reader thread can
ever process our queued APC is at or after the point its read was issued,
no matter how long ago the APC was queued or how fast the dropping thread
runs. This is an ordering guarantee from the Win32 APC-queueing contract,
not a timing one — queuing early costs nothing and cannot fire early.

## Expected

Not yet measured. The next step is tests only, no fix, to establish the
sweep and validate the injection hook before any fix code is written.

## Actual

The crate-source claims above, captured directly (not from memory):

```
$ grep -n "CancelIoEx\|CancelSynchronousIo\|CancelIo\b" \
    ~/.cargo/registry/src/*/interprocess-2.4.4/src -r
(no output — zero matches; the crate calls no CancelIo* function itself)

$ grep -n "FILE_FLAG_OVERLAPPED" \
    ~/.cargo/registry/src/*/interprocess-2.4.4/src/os/windows/named_pipe/c_wrappers.rs
16:            CreateFileW, ReOpenFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
164:            FILE_FLAG_OVERLAPPED,
182:    unsafe { ReOpenFile(h.as_raw_handle(), access_flags, NP_SHARE_MODE, FILE_FLAG_OVERLAPPED) }

$ grep -n "ipc::wake" native/src/*.rs
native/src/client.rs:145:            ipc::wake(&stream);
native/src/client.rs:160:    ipc::wake(&stream);
native/src/client.rs:233:        ipc::wake(&self.stream);
native/src/daemon.rs:383:        ipc::wake(&stream);
```

The pause-sweep and reader-injection measurements this mechanism motivates
are captured in the next step (tests only, no fix).

## Not verified

- Whether `QueueUserAPC`-based cancellation is itself free of some other
  Windows-specific subtlety (e.g. APC delivery ordering when multiple APCs
  are queued to one thread) — reasoned from documented Win32 semantics, not
  yet measured on this exact crate's read loop.
- The exact race window's width on `windows-latest`'s scheduler; the sweep
  is designed to bound it, not to prove a universal number.
