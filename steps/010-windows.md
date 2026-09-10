# 010 — Windows

정수님, 2026-09-10, after being told the platform was not supported:
*"windows는 아직 지원하지 않는다고요? 지원이 필요할 것 같은데요...?"*

## Before

Step 009 recorded the compiler's verdict: remuda does not build for Windows, and
the wall is `nix` plus unix sockets, not a build flag. `README.md` said "Windows:
not yet", `docs/install.sh` said an installer for a binary that cannot exist is
worse than none, and the release matrix had no Windows arm.

Four surfaces held it there:

```
IPC        std::os::unix::net::{UnixStream, UnixListener}
           native/src/{client.rs, daemon.rs, bin/remuda.rs}
RAW MODE   nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr}   native/src/client.rs
SIZE       nix::libc::ioctl(…, TIOCGWINSZ, …)                     native/src/lib.rs
PTY        portable-pty — already cross-platform (ConPTY)
```

Two more only showed up once those were traced: `dist.rs` shells out to `sh` for
both the upgrade and the once-a-day update check, and `daemon::spawn` defaults a
bare `new` to `$SHELL`.

And nobody working on this has a Windows machine, so nothing here can be checked
locally. That is the shape of the problem, not an aside: the deliverable is not
code that looks right, it is a green job on a real Windows runner.

## Desired outcome

`remuda` builds and passes its whole suite on `windows-latest`; a release asset
exists for `x86_64-pc-windows-msvc`; and `irm … | iex` installs it the way
`curl … | sh` installs the others — same checksum verification, same channel
file, same land-by-rename.

The architecture must not pay for it. `remuda-core` stays pure and still builds
for `wasm32-unknown-unknown`, every platform surface stays inside
`remuda-native`, and `gates-can-fail` keeps proving each guard can go red —
including the two new ones.

One code path where there can be one. A cfg fork per platform is the fallback,
not the plan: two forks drift, and the thing this repo has been burned by twice
is two files that are supposed to agree and are not checked against each other.

## Expected

1. **`interprocess`'s local sockets carry the whole IPC surface** — a unix socket
   on unix, a named pipe on Windows — so `client.rs` and `daemon.rs` keep their
   shape. `try_clone` exists on both, which is what the two-pump attach needs.
2. **One thing will not abstract: a named pipe has no `shutdown`.** The daemon
   wakes a thread blocked on `read` by half-closing the socket under it. Expect
   to need a platform-specific "stop waiting" and to isolate it in one function.
3. **crossterm replaces both `nix` uses**, and with them the crate's only
   `unsafe` block. `nix` survives for at most a call or two, gated to unix.
4. **`mlua` with vendored lua54 builds under MSVC** on `windows-latest`.
5. **The tests will need work.** They spawn `sh` and assert on `$((6*7))`, and
   they hand-build socket paths that only resemble what the binary derives.
6. **`check-install.py` will fail the moment a Windows target is added**, because
   `install.sh` cannot offer it — a `uname` case arm does not run on Windows. The
   platform list has to become two disjoint halves, both checked.

## Actual

**Expectation 1 held, and better than expected.** `interprocess::local_socket`
carries `try_clone`, so the daemon's two-pump attach is unchanged. The address
stays a `Path` on both platforms: `GenericFilePath` passes a `\\.\pipe\…` name
through verbatim, so `daemon::socket_path` forks once, in one function, and
nothing downstream knows.

**Expectation 2 held.** `set_recv_timeout` is `Unsupported` on named pipes —
`interprocess-2.4.4/src/os/windows/named_pipe/local_socket/stream.rs:53` returns
`no_timeouts()` — so a timeout is not the way out either. What is: the crate
opens pipes `FILE_FLAG_OVERLAPPED`, so the blocked read is a pending overlapped
operation and `CancelIoEx` cancels exactly it. That is the whole fork, in
`ipc::wake`, eleven lines.

**Expectation 3 held.** `nix` is down to one call, `shutdown(2)`, under
`[target.'cfg(unix)'.dependencies]`, and `terminal_size()` lost its `unsafe`
block:

```console
$ git diff --stat origin/main -- native/src/lib.rs native/Cargo.toml
 native/Cargo.toml   | 24 ++++++++++++++++++++----
 native/src/lib.rs   | 19 ++++++-------------
$ grep -c unsafe native/src/lib.rs
0
```

**Expectation 5 held, partly.** The socket paths were the real problem and are
fixed by a seam rather than a cfg: `daemon::socket_path_in(dir, server)` is the
shipped derivation with the runtime directory supplied, and the tests call it
instead of building `dir/remuda/default.sock` by hand. `sh` turned out to be on
the `windows-latest` image (Git for Windows), so `$((6*7))` still cannot appear
in the echo of its own question, principle 4 survives untouched, and **`v1.lua`
was never edited** — the frozen API script runs on Windows exactly as written.

**Nothing predicted the actual bug, and only a runner could have found it.**
Everything compiled, the daemon bound its named pipe, `new` succeeded and `ls`
reported the session `live` — and the screen was blank forever:

```console
$ remuda ls                    # on windows-latest
pcmd                  120x30   live  idle 6s
ppowershell           120x30   live  idle 2s
psh                   120x30   live  idle 10s
$ remuda capture psh | cat -A
$
```

Three different shells, all alive, all silent. The first probe measured the
wrong thing — `screen_bytes()` renders the *grid*, so an empty grid and an empty
stream look identical. Teeing the raw pty stream instead named it in one line:

```console
PROBE stream: "\u{1b}[6n"          ← the entire output of the pty
PROBE after spawn: alive=true bytes=15
PROBE text: ""
```

`ESC[6n` is a Device Status Report: *where is the cursor?* ConPTY asks it
**before emitting anything at all** and waits for the answer, because
`portable-pty` creates the pseudoconsole with `PSUEDOCONSOLE_INHERIT_CURSOR`
(`portable-pty-0.9.0/src/win/psuedocon.rs:84`). remuda never answered, so the
console host never started pumping. On unix nothing asks, so the gap had never
shown.

The fix is what a terminal emulator is supposed to do, and it is not
Windows-specific: the reader thread answers `ESC[<row>;<col>R` from the grid it
is already maintaining. That needed the pty writer to be shared rather than
owned, which is the one structural change in this port — and it does not touch
PRINCIPLES §6 invariant 1, because the reply is a single `write_all` under the
same lock every other write takes, so it still cannot land inside a caller's
burst.

**Expectation 6 held.** `docs/install.ps1` now holds the Windows half of the
platform list and `scripts/check-install.py` checks both halves against the
matrix, in both directions, plus which file a triple is allowed to appear in.

The full local suite, on Linux, after the port — the negative control for "did
this break unix":

```console
$ cargo test --workspace --all-targets
   Compiling remuda-native v0.1.0
    Finished `test` profile [unoptimized + debuginfo] target(s)
     Running tests/invariants.rs
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/keys.rs
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running unittests src/lib.rs (remuda_native)
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/daemon.rs
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/live_sessions.rs
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/mcp.rs
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/script.rs
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

$ cargo check -p remuda-core --target wasm32-unknown-unknown
    Checking remuda-core v0.1.0
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.46s

$ python3 scripts/check-install.py
ok — 4 target(s) built and offered (3 via install.sh, 1 via install.ps1):
aarch64-apple-darwin, x86_64-apple-darwin, x86_64-pc-windows-msvc,
x86_64-unknown-linux-gnu
```

### Windows, on a real runner

`ci` run 34442730165, job `test-windows` (102760965629), `windows-latest`,
`cargo test --workspace --all-targets` — every target, nothing ignored:

```console
     Running tests\invariants.rs
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests\keys.rs
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running unittests src\lib.rs (remuda_native)
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests\daemon.rs
test a_wire_size_below_the_floor_is_clamped_not_honoured ... ok
test a_taken_name_is_refused_in_words ... ok
test sessions_are_listed_and_kept_apart ... ok
test a_human_attaches_through_a_real_terminal_and_detaches_with_ctrl_backslash ... ok
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests\live_sessions.rs
test a_dead_process_reports_exited_rather_than_swallowing_input ... ok
test an_instruction_reaches_a_live_process_and_its_output_comes_back ... ok
test attaching_and_detaching_does_not_disturb_the_agent ... ok
test an_attached_viewer_gets_a_repaint_and_then_a_live_stream ... ok
test the_cursor_reports_a_real_position ... ok
test three_live_sessions_stay_separate ... ok
test detaching_leaves_the_agent_untouched_and_the_core_back_in_charge ... ok
test the_terminal_keeps_the_size_it_was_spawned_with ... ok
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests\mcp.rs
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests\script.rs
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Three of those are worth naming, because they are the port's real evidence:

- `a_human_attaches_through_a_real_terminal_and_detaches_with_ctrl_backslash`
  opens a ConPTY, runs the **shipped binary** inside it, and drives it through
  `crossterm`'s raw mode, a named-pipe attach, keystrokes landing in the far
  session, the core being refused while a human holds it, and Ctrl-\ giving it
  back. Every platform surface in one test.
- `the_terminal_keeps_the_size_it_was_spawned_with` asks the pty itself
  (`stty size`), so the 80 columns are the console host's answer, not ours.
- `every_frozen_api_version_still_runs` runs `native/tests/api/v1.lua`
  **unedited**, on Windows. The v1 contract holds on a platform it predates.

### The release matrix, on real runners

`release` run 34443114624, `workflow_dispatch` on this branch. Two of four
targets are proven by a job that actually ran; two never got a runner:

```console
$ gh run view 34443114624 --json jobs
plan                                                          completed  success
build (x86_64-pc-windows-msvc, windows-latest, remuda.exe)    completed  success
build (x86_64-unknown-linux-gnu, ubuntu-22.04, remuda)        completed  success
build (aarch64-apple-darwin, self-hosted, macOS, ARM64)       queued     ← 11 min, never assigned
build (x86_64-apple-darwin,  self-hosted, macOS, ARM64)       queued     ← 11 min, never assigned

$ gh run view --job 102762108293          # the Windows build
✓ Run rustup target add x86_64-pc-windows-msvc
✓ build
✓ the binary reports the version it was built with
✓ package
✓ Run actions/upload-artifact@v4
```

**The macOS half is blocked on a setting nobody here can read.** The repo has no
runners of its own, and the org endpoints refuse the token:

```console
$ gh api repos/warmblood-kr/remuda/actions/runners
{"total_count":0,"runners":[]}
$ gh api orgs/warmblood-kr/actions/runner-groups
{"message":"You must be an org admin …","status":"403"}
```

`warmblood-macbook-runner-1` serves `monocle-desktop-app`, which is private.
remuda went public today, and an org runner group does not serve public
repositories unless an admin turns that on — off by default, for the reason in
this workflow's own trigger comment. The signature is identical to the retired
`macos-13` label: `queued`, never assigned, indefinitely. It was **not** worked
around, and the run was cancelled rather than left sitting in the office
runner's queue.

⇒ Until an admin confirms that group allows this repository, merging this branch
would stop macOS assets from being published at all. That is a blocker on the
release half, and it is stated in the PR rather than papered over.

## What is still unproven

Say it plainly rather than implying support:

- **`remuda attach` in a real Windows terminal is untested.** CI drives it
  through a ConPTY the test itself opened, which is genuine evidence that raw
  mode is entered, that keystrokes reach the far session, and that Ctrl-\
  detaches. It is *not* evidence about Windows Terminal or conhost: window
  resize, IME, and the console host's own key handling are outside it.
- **`ipc::wake` on Windows has no test.** The path it exists for is "the agent's
  process dies while a human is attached", and no test covers that on either
  platform. Unix has had `shutdown` there since step 001 and it is unchanged;
  the `CancelIoEx` half is reasoned from the crate's source, not measured.
- **`install.ps1` has not been run against a real release.** Principle 12 says an
  installer is only proven by installing, and that rehearsal needs a Windows
  machine plus a published Windows asset. `check-install.py` covers the drift
  axis and the PowerShell parser covers syntax; neither is the rehearsal.
- **`dist::upgrade` on Windows is untested end to end** for the same reason.
- **Neither macOS target has been built by any runner in this repository since
  `macos-13` was retired.** `aarch64-apple-darwin` last built on `macos-14`
  (run 34440624240, the current nightly); the cross-compiled
  `x86_64-apple-darwin` has **never** been built anywhere — `mlua` vendors and
  compiles Lua from C, and whether that cross-compiles cleanly on Apple Silicon
  is exactly the thing a runner has to answer. Its `--version` assertion is also
  skipped by construction, since the host cannot execute it.
- **`shell_quoting_survives_a_quote_in_the_path` does not run on Windows.** The
  function it tests does not exist there — paths travel to PowerShell as
  environment variables, never quoted into script text. That is the only test
  gated by platform, and it is gated because there is nothing to call.

## Resolution before merge — macOS went back to GitHub's runner

The self-hosted macOS entries above were reverted to `macos-14` and
`x86_64-apple-darwin` was dropped from the matrix, so what merges is exactly
the previously-shipping set plus Windows. Three reasons, in order of weight:

1. **Merging as written would have stopped macOS assets from publishing**, and
   the person who asked for Windows installs on Apple Silicon. `publish` is
   strict by the deliberate argument in `release.yml`, so a macOS job that no
   runner accepts withholds Linux and Windows too.
2. **The self-hosted blocker is not ours to clear.** A runner group must admit a
   public repository; `repos/warmblood-kr/remuda/actions/runners` is
   `total_count: 0` and the org endpoint answers 403 without `admin:org`.
   Guessing which it is, or quietly falling back, would make "allowed" and
   "blocked" look identical — the failure this whole step is about.
3. **The reason for self-hosted was cost, and the cost is zero here.** GitHub's
   billing API for run 34438277779: `billable.MACOS.total_ms = 0`, because
   standard runners are free on public repositories. The office Mac still earns
   its keep on the private repos, where the multiplier is real.

`x86_64-apple-darwin` stays out until some host answers the mlua cross-compile
question. It has never been built; shipping a matrix entry on the strength of
"Xcode has both SDKs" would be the same unmeasured optimism as the line above.
