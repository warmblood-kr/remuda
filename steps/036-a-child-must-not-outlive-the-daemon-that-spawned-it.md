# 036 — a child must not outlive the daemon that spawned it

## Before

`native/src/process.rs`'s `Processes::spawn` (`remuda.process{}`) launches a
plain-pipe child with bare `std::process::Command` — no process-group
isolation, no `pre_exec` hook, no signal handling anywhere in this crate.
Measured live (prior rounds of this same investigation, PR #67): a
`remuda.process{}` child **always** survives daemon death — SIGKILL, SIGTERM,
or a clean `Request::Shutdown` (`std::process::exit(0)`, which skips every
destructor and runs no cleanup code at all) — reparented to the nearest
subreaper as a real orphan.

`native/src/pty.rs`'s `PtyAgent::spawn` (real interactive sessions, via
`portable_pty::CommandBuilder`) already dies with the daemon in every mode
tested, but by kernel accident: the daemon holds the pty master fd, any
daemon exit closes it, the kernel SIGHUPs the pty's session-leader child, and
the default action for SIGHUP is termination. Zero remuda code causes this,
and nothing pinned it — a future change to how `pty.rs` holds `_master`
could silently remove the protection with no test to catch it.

## Desired outcome

Every `remuda.process{}` child dies with its daemon on Linux, via a real
kernel mechanism (`PR_SET_PDEATHSIG`) rather than daemon cooperation — so it
also covers the death modes where no daemon code runs at all (SIGKILL,
SIGTERM). A clean `Request::Shutdown` additionally sweeps each tracked
child's whole process group, reaching a grandchild the direct child already
forked (PDEATHSIG only ever targets the one pid it was registered on). Every
platform this round does not build for (macOS, Windows) says so loudly, once,
rather than silently leaking. The one thing that already works (`pty.rs`'s
kernel-accident death) gets a real regression test, and the one thing this
round fixes (`process.rs`'s leak) gets a real test with a genuine
red-then-green negative control, not an assumed one.

## Expected

- `native/src/child_guard.rs` (new): `Guard` enum (`LinuxPdeathsig` /
  `Unimplemented(&'static str)`), `harden(&mut Command) -> Guard` —
  Linux: `process_group(0)` + a `pre_exec` hook that calls
  `prctl(PR_SET_PDEATHSIG, SIGKILL)`, then self-checks `getppid()` against the
  daemon's pid at registration time and self-`SIGKILL`s on a lost race (the
  parent already gone between `fork()` and the `prctl()` call). Other
  platforms: an unimplemented arm plus a one-time `eprintln!`.
  `documented_pty_hangup_accident()` — a no-op marker called from `pty.rs`'s
  spawn site so the decision not to guard that path is grep-able.
- `native/Cargo.toml`: `libc` added `[target.'cfg(target_os = "linux")'.dependencies]`
  only, pinned to the version already resolved transitively (`0.2.189`).
- `native/src/process.rs`: `spawn()` calls `child_guard::harden` on the
  `Command` before `.spawn()`. New `Processes::killpg(id)` — best-effort,
  Linux-only `killpg(pid, SIGKILL)` on a tracked child's group, ESRCH
  tolerated.
- `native/src/script.rs`: new `_process_killpg` Lua word (underscore-prefixed
  like `_process_spawn`/`_process_drain`), added to both `BINDINGS` (now 49
  entries, alphabetically ordered) and the `WORDS` registry table — required
  by the existing `the_bound_surface_is_exactly_the_protocols` and
  `every_word_has_a_registry_entry` tests, which assert the live `remuda`
  table and `_registry` match `BINDINGS` exactly, in both directions.
- `native/src/daemon.rs`: `Request::Shutdown` now calls a new
  `reap_processes_before_exit(image)` (split out to stay under the
  100-line/function clippy cap) before `std::process::exit(0)` — one
  `image.eval` running `for _, id in ipairs(remuda.processes()) do
  remuda._process_killpg(id) end`. No IPC round trip and no deadlock risk:
  `processes()`/`_process_killpg` are plain synchronous Rust functions, not
  `ask()`-shaped calls back into the daemon's own socket (unlike
  `remuda.new`/`send`/etc., which is what made steps/035's inline loader
  deadlock).
- `native/src/pty.rs`: `documented_pty_hangup_accident()` called right after
  `pair.slave.spawn_command(command)`.
- `native/tests/daemon.rs`: two new tests (below).
- `native/tests/pty_survives_daemon_death.rs` (new): one test pinning the
  pty kernel-accident death.

## Actual

Clean build and clippy (workspace, all targets, `-D warnings`, the same
invocation `.github/workflows` uses):

```
$ cargo build --manifest-path native/Cargo.toml
   Compiling remuda-native v0.1.0 (.../native)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.58s

$ cargo tree | grep libc | head -1
│   │   ├── libc v0.2.189
# confirms `libc = "0.2"` resolved to the version already in the graph
# transitively, not a second copy.

$ cargo clippy --workspace --all-targets -- -D warnings
    Checking remuda-native v0.1.0 (.../native)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.35s
```

New tests, both green on the first real run:

```
$ cargo test --manifest-path native/Cargo.toml --test daemon reaps -- --nocapture
running 3 tests
test a_clean_shutdown_reaps_a_processs_whole_group_including_a_grandchild ... ok
test a_sigkilled_daemon_reaps_its_direct_process_child_but_not_an_already_forked_grandchild ... ok
test a_session_exited_hook_still_fires_once_when_ls_reaps_before_the_tick ... ok
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 44 filtered out; finished in 1.29s

$ cargo test --manifest-path native/Cargo.toml --test pty_survives_daemon_death -- --nocapture
running 1 test
test a_pty_sessions_child_dies_with_a_sigkilled_daemon ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
```

Genuine negative control on the primary DoD (not assumed): the
`child_guard::harden(&mut command)` call in `process.rs::spawn` was commented
out, live:

```
$ cargo test --manifest-path native/Cargo.toml --test daemon \
    a_sigkilled_daemon_reaps_its_direct_process_child_but_not_an_already_forked_grandchild -- --nocapture
warning: unused import: `crate::child_guard`
thread 'a_sigkilled_daemon_reaps_its_direct_process_child_but_not_an_already_forked_grandchild' (3669722) panicked at native/tests/daemon.rs:1444:9:
the direct child (3669731) outlived a SIGKILLed daemon — child_guard::harden did not reap it
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 46 filtered out; finished in 10.06s
```

The leaked `sh`/`sleep` pair from that run was confirmed and cleaned up by
hand (`pkill -9 -f "sleep 60"`) before restoring the call — it is exactly the
kind of orphan this whole round exists to stop, so it was not left running.
The call was then restored and the same test re-run, green:

```
$ cargo test --manifest-path native/Cargo.toml --test daemon reaps -- --nocapture
running 3 tests
test a_clean_shutdown_reaps_a_processs_whole_group_including_a_grandchild ... ok
test a_sigkilled_daemon_reaps_its_direct_process_child_but_not_an_already_forked_grandchild ... ok
test a_session_exited_hook_still_fires_once_when_ls_reaps_before_the_tick ... ok
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 44 filtered out; finished in 1.29s
```

Full suite, every existing test still passing (107+1+4+2ignored+47+22+8+10+1+9,
zero failures):

```
$ cargo test --manifest-path native/Cargo.toml 2>&1 | grep "test result"
test result: ok. 107 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.19s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 0 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 47 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.18s
test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.82s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

`cargo fmt --check` (applied, then re-verified clean), and the repo's own
convention checks:

```
$ cargo fmt --manifest-path native/Cargo.toml -- --check
$ python3 scripts/check-comments.py
ok — 362 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-principles.py
ok — 14 principles, every named mechanism exists (21 CI jobs, 11 denied paths, 418 test fns seen)
$ python3 scripts/check-butler-path-convention.py
ok — install-butler.sh and init.lua agree: $HOME/.config fallback, segments ['/remuda/butler/config', '/remuda/butler/token']
ok — install-butler.sh and daemon.rs agree: $HOME/.config fallback, init.lua at $config_home/remuda/init.lua
$ python3 scripts/check-steps.py
ok — 36 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-workflows.py
ok — 3 workflow file(s) parse, 21 job(s) defined
```

## A discovery worth stating plainly: SIGKILL cannot reach a grandchild, and the dispatch's own "Known ceilings" wording says so

The initial plan for the SIGKILL DoD test asserted that **both** the direct
child and an already-forked grandchild disappear after a raw `SIGKILL` of the
daemon. Running it for real showed the grandchild (`sleep 60`, forked by the
`sh -c 'echo $$; sleep 60'` direct child) survives:

```
thread '...' panicked at native/tests/daemon.rs:1421:9:
orphaned after daemon SIGKILL: direct 3662641 alive=false, grandchild 3662644 alive=true
```

This is not a bug in `child_guard.rs` — it is what the module's own doc
comment says will happen: `PR_SET_PDEATHSIG` is registered on the direct
child alone and is cleared across `fork(2)`, so it can never reach a process
it was never registered on. The only mechanism that reaches a grandchild
(`killpg`, via the process group `harden()` also sets up) is wired to
**exactly one** death mode: a clean `Request::Shutdown`. A raw `SIGKILL` of
the daemon runs no daemon code at all — there is nothing to call `killpg`.

Rather than force the original test to pass (which would have meant either
weakening the assertion silently or adding a mechanism out of this round's
scope — a supervisor process watching the daemon's own life, which no code
inside the daemon can do for itself), the test was split into two, each
proving something real:

- `a_sigkilled_daemon_reaps_its_direct_process_child_but_not_an_already_forked_grandchild`
  — proves the actual DoD (the direct child, which is *the* leak named in
  this round's dispatch, is reaped by the kernel alone) and pins the exact
  boundary (the grandchild is still alive ~200ms after the direct child is
  confirmed gone) rather than asserting past it. It cleans up the grandchild
  it deliberately leaves alive, so the test itself does not orphan anything
  on the machine that runs it.
- `a_clean_shutdown_reaps_a_processs_whole_group_including_a_grandchild` —
  proves the one path that *does* reach the grandchild: a real `remuda
  restart` (which sends `Request::Shutdown`), confirming both pids are gone
  afterward.

This matches, rather than contradicts, the "Known ceilings" bullet the
dispatch itself required below.

## Known ceilings

- **SIGTERM and SIGKILL of the daemon still leak a grandchild.** Only a
  clean `Request::Shutdown` runs `reap_processes_before_exit`'s `killpg`
  sweep. SIGTERM has no handler in this codebase (out of scope for this
  round — see Boundaries below) and SIGKILL cannot be handled by any
  process, so neither death mode runs any code that could reach a
  grandchild. Only the direct child is covered on those paths, via kernel
  PDEATHSIG delivery that requires no daemon cooperation.
- **macOS and Windows are named, not implemented.** `child_guard::harden` on
  those platforms returns `Guard::Unimplemented` and prints one `eprintln!`
  warning; a `remuda.process{}` child there still orphans exactly as before
  this round, with no new protection. Building either (a macOS
  `EVFILT_PROC` supervisor, or a Windows Job Object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) was explicitly out of scope.
- **A registration race, however narrow.** `harden`'s `pre_exec` hook
  registers `PR_SET_PDEATHSIG` and then self-checks `getppid()` against the
  daemon's pid, self-`SIGKILL`ing on a lost race — but a grandchild the
  direct child forks *after* the daemon has already died, in the narrow
  window before that direct child itself receives its own PDEATHSIG
  delivery, is not reachable by anything built this round. This is a
  fundamentally different (and narrower) race than the "SIGKILL leaks an
  already-existing grandchild" ceiling above, which is not narrow at all —
  it reproduces every time, as shown above.
- **No SIGTERM handler was added.** Out of scope per the dispatch: adding
  one is a bigger design decision needing async-signal-safety care for
  anything beyond the kernel-level PDEATHSIG mechanism this round already
  uses correctly (all of `harden`'s `pre_exec` closure is
  async-signal-safe: `prctl`, `getppid`, `raise`).
- `_process_killpg` is reachable from any Lua running inside the daemon's own
  image (it's a real, if underscore-prefixed, `remuda` table entry) — nothing
  currently restricts it to the daemon's own shutdown path specifically. This
  matches the existing trust model already stated in `script.rs`'s own module
  doc comment ("`remuda lua script.lua` is as trusted as `sh script.sh`") and
  is not a new gap introduced by this round.

## Not verified

- macOS and Windows were not exercised at all — this machine is Linux only,
  and building either lane was out of scope for this round.
- The registration-race self-check (`getppid() != daemon_pid` inside
  `pre_exec`) was not exercised under an actual lost race live — reproducing
  "the daemon dies in the exact window between `fork()` and the `prctl()`
  call" on demand would need artificial scheduling control this round did
  not build. The code path exists and is reasoned about in `child_guard.rs`'s
  own comments, matching the well-known Linux PDEATHSIG race documented
  upstream, but was not measured directly here.
- SIGTERM of the daemon was not separately tested — no handler exists for
  it, so its behavior is identical to SIGKILL from a cleanup-code
  perspective (no daemon code runs either way), and the SIGKILL test already
  covers that shared behavior.
