# 035 — a loader that blocks its own listener never loads

## Before

The only way to run `packages/butler/init.lua` was the client verb `remuda
exec <name>`, which round-trips over IPC to an **already-running** daemon
(`exec_command`, native/src/bin/remuda.rs, via `script::run_source`). Nothing
made a fresh daemon (after a kill, `restart -f`, or a reboot) run it
automatically — `docs/install-butler.sh`'s own poller
(`native/tests/daemon.rs::a_daemon_restart_does_not_relaunch_the_butler_session`)
exists specifically because of this gap, polling `remuda ls` every 15s and
re-running `remuda exec butler` by hand.

There was also no `remuda.exec(name)` Lua binding — package resolution
(`"butler" => include_str!(...)`) lived only in the bin crate
(`native/src/bin/remuda.rs`'s `builtin_package`), unreachable from the lib
crate where the daemon itself boots.

## Desired outcome

A generic `~/.config/remuda/init.lua` (XDG: `$XDG_CONFIG_HOME` else
`$HOME/.config`) that the daemon itself reads and evaluates once,
automatically, every time a fresh daemon boots — mirroring how
Neovim/Hammerspoon/WezTerm auto-load a user init file. Absence is silent (a
fresh install behaves exactly like today). A broken file is reported to
stderr but can never poison the image the way an `Err` inside
`Image::spawn`'s own `ready` chain would (`image.rs`) — that chain is allowed
to poison forever only because `tools.lua` is compiled-in and controlled by
this repo; a user's own file must degrade to "no butler session", never brick
every future eval on their daemon.

## Expected

- `native/src/packages.rs` (new, lib crate): one `pub fn builtin(name) ->
  Option<&'static str>` — the single table both `remuda exec` (CLI, over IPC)
  and the new `remuda.exec()` Lua binding resolve package names through.
  `native/src/bin/remuda.rs`'s `builtin_package` now delegates to it in one
  line instead of duplicating the match.
- `remuda.exec(name)` binding (`native/src/script.rs`): resolves via
  `packages::builtin`, raises `"no such package: {name}"` (byte-for-byte the
  CLI's own wording) on `None`, else `lua.load(source).set_name(...).exec()`
  — a plain nested call on the same `Lua`, reentrant-safe. Added to
  `BINDINGS`/`_registry` so the existing bound-surface tests
  (`the_bound_surface_is_exactly_the_protocols`, `every_word_has_a_registry_entry`)
  cover it without modification to their own logic.
- `native/src/daemon.rs`: `user_config_path()` (mirrors `remuda.rs`'s
  `history_path` shape, not `dist.rs`'s `base_dir` — a relative-path fallback
  is fine for a cache, wrong for a file read and executed as code) and
  `load_user_config(image)`, which reads the file (silent on absence) and
  `image.eval`s it (a separate, later call — never inside `ready`).
- `docs/install-butler.sh`: writes `$config_home/remuda/init.lua` (sibling to
  `remuda/butler/`) containing `remuda.exec("butler")`, then a real functional
  verification — kill/restart the daemon, confirm `remuda ls` shows `butler`
  again with **no** second `exec butler` call.
- `scripts/check-butler-path-convention.py`: extended with a second two-way
  check (`install-butler.sh`'s write target vs. `daemon.rs`'s
  `user_config_path()`), alongside the existing butler-token/config one.
- `native/tests/daemon.rs`: four new tests (below).

## A real deadlock, found by actually running the test (not assumed)

The obvious placement — call `load_user_config(&image)` synchronously, right
after `Image::spawn`, **before** `spawn_ticker`/`listener.incoming()` starts —
compiles clean and looks deterministic ("the very first `remuda ls` already
sees butler"). It hangs forever the moment the loaded file does what butler's
real `init.lua` actually does: call `remuda.new(...)`. Every one of those
bindings (`new`, `send`, `close`, `attach`, `capture`, ...) is `ask()` in
script.rs — a **real IPC round trip back to this daemon's own socket**,
answered only by the `listener.incoming()` accept loop. Calling the loader
inline, before that loop starts, blocks the one thread that could ever accept
and answer that very request. `connect()` on a unix socket succeeds against
the `listen()` backlog even with no `accept()` yet — so this is a genuine
deadlock, not merely slow, confirmed by a real hung test process (see
transcript below) that had to be `kill -9`'d.

Fixed by running `load_user_config` on its own thread, started right after
`Image::spawn` and *concurrently with* `spawn_ticker`/`listener.incoming()`,
rather than blocking `serve()` before the accept loop exists. This costs the
"the very first `remuda ls` is guaranteed to already see butler" determinism
claim (the load can now lag an instant behind the daemon's first accepted
connection) — but that claim was unsafe the moment the loaded script does
anything that loops back over the socket, which is the entire point of
auto-loading butler. Every new test below retries against `PATIENCE` (10s)
rather than asserting on the very first `remuda ls`, which is the correct
instrument for an admittedly-not-instantaneous load.

## Actual

The hang, caught live (had to be killed out-of-band; this is the actual
transcript, not a paraphrase):

```
$ cargo test --test daemon auto_loads_the_user_config -- --nocapture --test-threads=1
running 1 test
test a_fresh_daemon_auto_loads_the_user_config_and_registers_butler_with_no_human_hand ...
[hung past the harness's own timeout; killed via SIGKILL]
```

Fixed (loader moved to its own thread), same test:

```
$ cargo test --test daemon auto_loads_the_user_config -- --nocapture --test-threads=1
running 1 test
test a_fresh_daemon_auto_loads_the_user_config_and_registers_butler_with_no_human_hand ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 44 filtered out; finished in 0.05s
```

Red-then-green on the whole feature (production code stashed, new tests kept,
matching this repo's own `steps/034` discipline):

```
$ git stash push --include-untracked -- native/src/bin/remuda.rs native/src/daemon.rs \
    native/src/lib.rs native/src/script.rs native/src/packages.rs
$ cargo build --tests   # compiles clean against the OLD code -- the new
                         # tests only reach the feature via Lua/CLI, no new
                         # Rust symbol reference
$ cargo test --test daemon auto_loads_the_user_config -- --nocapture --test-threads=1
running 1 test
test a_fresh_daemon_auto_loads_the_user_config_and_registers_butler_with_no_human_hand ...
thread '...' panicked at native/tests/daemon.rs:2980:9:
the daemon never auto-registered a butler session from its own boot-time config load
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 44 filtered out; finished in 10.06s

$ cargo test --test daemon no_user_config_never_auto_registers_butler -- --test-threads=1
test result: ok. 1 passed   # negative control passes even without the feature, by design
$ cargo test --test daemon a_broken_user_config_is_reported -- --test-threads=1
test result: ok. 1 passed   # ditto -- nothing reads the file at all yet

$ git stash pop
$ cargo test --test daemon -- --test-threads=1 \
    auto_loads_the_user_config no_user_config_never_auto_registers_butler \
    a_broken_user_config_is_reported
running 3 tests
test a_broken_user_config_is_reported_but_never_bricks_the_daemon ... ok
test a_fresh_daemon_auto_loads_the_user_config_and_registers_butler_with_no_human_hand ... ok
test a_fresh_daemon_with_no_user_config_never_auto_registers_butler ... ok
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 42 filtered out; finished in 1.28s
```

`docs/install-butler.sh`, run for real against an isolated throwaway `HOME`
(scratch dir under this session's scratchpad; a short `/tmp/rb-rt.XXXXXX`
runtime dir, only because a unix `sun_path` is ~108 bytes and the scratchpad
path is longer — same reasoning as `native/tests/daemon.rs`'s own
`scratch_dir` comment), a stand-in `claude` (`exec sleep 300`, since no real
`claude`/Matrix homeserver is available or wanted here), and
`REMUDA_BUTLER_TOKEN_FILE`/`REMUDA_BUTLER_CONFIG_FILE` pointing at scratch
fixtures — never the real `$HOME`:

```
$ env HOME="$SCRATCH/home" PATH="$SCRATCH/bin:$PATH" \
    REMUDA_RUNTIME_DIR="$SHORT_RUNTIME" \
    REMUDA_BUTLER_TOKEN_FILE="$SCRATCH/fixtures/token" \
    REMUDA_BUTLER_CONFIG_FILE="$SCRATCH/fixtures/config" \
    REMUDA_NO_UPDATE_CHECK=1 sh docs/install-butler.sh
install-butler.sh: copied .../fixtures/token to .../home/.config/remuda/butler/token (init.lua's default lookup)
install-butler.sh: copied .../fixtures/config to .../home/.config/remuda/butler/config (init.lua's default lookup)
install-butler.sh: registering butler in the running daemon (this starts one if none is up)...
install-butler.sh: butler registered for this run.
install-butler.sh: wrote .../home/.config/remuda/init.lua
install-butler.sh: restarting the daemon to verify the new loader actually re-registers butler...
remuda: stopped the daemon for "default" — the next command starts a fresh one
remuda: started a daemon for "default"
install-butler.sh: confirmed: butler came back automatically after a daemon restart, with no 'remuda exec butler' call.
install-butler.sh: wrote .../home/.config/remuda/butler-poll.sh
install-butler.sh: wrote .../home/.config/systemd/user/remuda-butler.service
install-butler.sh: wrote .../home/.config/systemd/user/remuda-butler.timer
install-butler.sh: daemon-reload done. Two more steps, run these yourself:
$ echo EXIT=$?
EXIT=0
```

The generated `init.lua` matched the exact content specified (verified by
`cat`); teardown confirmed by PID, not by belief:

```
$ ps -eo pid,ppid,args | grep -F 'target/debug/remuda -s default daemon'
2771337 ... /.../target/debug/remuda -s default daemon
$ kill -9 2771337 2771344   # daemon + its stand-in "sleep 300" claude child
$ ps -p 2771337 -p 2771344; echo exit=$?
    PID TTY          TIME CMD
exit=1
```

The real host's own systemd user session was checked, not assumed
untouched — no remuda unit was ever loaded there, and the real `$HOME` was
never written to (the script's `unit_dir`/`config_home` came entirely from
the overridden `HOME`):

```
$ systemctl --user list-units 'remuda*' --all
0 loaded units listed.
$ systemctl --user list-timers 'remuda*' --all
0 timers listed.
$ ls ~/.config/remuda
ls: cannot access '/home/toracle/.config/remuda': No such file or directory
```

Full gate, all green:

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "FAILED|error\[|error:|test result"
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 27 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 107 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.17s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 0 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 45 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.19s
test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.74s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.63s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.39s

$ cargo fmt --all -- --check
$ shellcheck docs/install-butler.sh
$ cargo clippy --workspace --all-targets -- -D warnings
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.59s

$ python3 scripts/check-butler-path-convention.py
ok — install-butler.sh and init.lua agree: $HOME/.config fallback, segments ['/remuda/butler/config', '/remuda/butler/token']
ok — install-butler.sh and daemon.rs agree: $HOME/.config fallback, init.lua at $config_home/remuda/init.lua
$ python3 scripts/check-comments.py
ok — 357 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-install.py
ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
$ python3 scripts/check-principles.py
ok — 14 principles, every named mechanism exists (21 CI jobs, 11 denied paths, 412 test fns seen)
$ python3 scripts/check-steps.py
ok — 34 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-workflows.py
ok — 3 workflow file(s) parse, 21 job(s) defined
```

### Follow-up: the installer's own post-restart check was still single-shot

The "Known ceilings" bullet above originally claimed "every test and the
installer's own verification account for this with a bounded retry" — true
for `native/tests/daemon.rs`, false for `docs/install-butler.sh` as first
shipped in this step: its post-restart check was one `if ! ... remuda ls |
awk ...`, no loop, no sleep. A real colleague on a slower/more loaded machine
could see a spurious `die` even though the loader would have registered
butler a moment later. Fixed by giving that one check the same bounded-retry
discipline as the Rust tests: 20 attempts, 500ms apart (10s total, the same
order of magnitude as `PATIENCE`), `die`ing with the original message only on
the final attempt.

Reproduced for real, not just argued: `native/src/daemon.rs`'s
`load_user_config` was temporarily patched to sleep 800ms (gated behind a
throwaway env var, `REMUDA_TEST_ARTIFICIAL_LOADER_DELAY_MS`, reverted before
committing — `git diff --stat` after the revert shows only
`docs/install-butler.sh` touched) to widen the normally sub-millisecond race
window enough to observe it on purpose. Same scratch recipe as above (`HOME`
under this session's scratchpad, short `REMUDA_RUNTIME_DIR`, stand-in
`claude` shim, scratch token/config fixtures):

```
$ env HOME="$SCRATCH/home-old" PATH="$SCRATCH/bin:$PATH" \
    REMUDA_RUNTIME_DIR="$SHORT_RUNTIME" \
    REMUDA_BUTLER_TOKEN_FILE="$SCRATCH/fixtures/token" \
    REMUDA_BUTLER_CONFIG_FILE="$SCRATCH/fixtures/config" \
    REMUDA_NO_UPDATE_CHECK=1 \
    REMUDA_TEST_ARTIFICIAL_LOADER_DELAY_MS=1 \
    sh install-butler-OLD.sh   # the single-shot check, as first shipped
install-butler.sh: registering butler in the running daemon (this starts one if none is up)...
install-butler.sh: butler registered for this run.
install-butler.sh: wrote .../home-old/.config/remuda/init.lua
install-butler.sh: restarting the daemon to verify the new loader actually re-registers butler...
remuda: stopped the daemon for "default" — the next command starts a fresh one
remuda: started a daemon for "default"
install-butler.sh: butler did not come back on its own after a daemon restart -- the boot-time loader (.../init.lua) did not work; this build may predate it (try 'remuda upgrade' and re-run this installer)
$ echo EXIT=$?
EXIT=1
```

A spurious failure: butler's process (the stand-in `claude`, `sleep 300`)
showed up under the daemon moments later, confirmed by PID, then killed:

```
$ ps -eo pid,ppid,args | grep -F 'target/debug/remuda -s default daemon'
2813913 ... target/debug/remuda -s default daemon
$ ps -eo pid,ppid,args | grep 'sleep 300'
2813958 2813913 sleep 300
$ kill -9 2813913 2813958
$ ps -p 2813913 -p 2813958; echo exit=$?
    PID TTY          TIME CMD
exit=1
```

Same artificial delay, the fixed retry-loop version, run three times for
reliability, all green:

```
$ env HOME="$SCRATCH/home-new" PATH="$SCRATCH/bin:$PATH" \
    REMUDA_RUNTIME_DIR="$SHORT_RUNTIME" \
    REMUDA_BUTLER_TOKEN_FILE="$SCRATCH/fixtures/token" \
    REMUDA_BUTLER_CONFIG_FILE="$SCRATCH/fixtures/config" \
    REMUDA_NO_UPDATE_CHECK=1 \
    REMUDA_TEST_ARTIFICIAL_LOADER_DELAY_MS=1 \
    sh install-butler-NEW.sh   # the bounded-retry check
...
install-butler.sh: restarting the daemon to verify the new loader actually re-registers butler...
remuda: stopped the daemon for "default" — the next command starts a fresh one
remuda: started a daemon for "default"
install-butler.sh: confirmed: butler came back automatically after a daemon restart, with no 'remuda exec butler' call.
...
$ echo EXIT=$?
EXIT=0
$ # repeated two more times against fresh scratch $HOMEs, same artificial delay
run 1 exit=0
run 2 exit=0
run 3 exit=0
```

Teardown confirmed by PID for every daemon (and its stand-in `claude` child)
spawned across all of the above runs — none left running:

```
$ ps -eo pid,ppid,args | grep -F 'target/debug/remuda -s default daemon'
2814931 ...
2815313 ...
2815447 ...
2815582 ...
$ for pid in 2814931 2815313 2815447 2815582; do pgrep -P "$pid"; done
2814963
2815351
2815490
2815616
$ kill -9 2814931 2814963 2815313 2815351 2815447 2815490 2815582 2815616
$ ps -p 2814931 -p 2814963 -p 2815313 -p 2815351 -p 2815447 -p 2815490 -p 2815582 -p 2815616
    PID TTY          TIME CMD
$ echo exit=$?
exit=1
```

A final real run against the actual (non-patched) fixed installer, no
artificial delay, passes on the first attempt as expected (the race window
without the artificial widening is normally sub-millisecond):

```
$ env HOME="$SCRATCH/home-final" PATH="$SCRATCH/bin:$PATH" \
    REMUDA_RUNTIME_DIR="$SHORT_RUNTIME" \
    REMUDA_BUTLER_TOKEN_FILE="$SCRATCH/fixtures/token" \
    REMUDA_BUTLER_CONFIG_FILE="$SCRATCH/fixtures/config" \
    REMUDA_NO_UPDATE_CHECK=1 sh docs/install-butler.sh
...
install-butler.sh: confirmed: butler came back automatically after a daemon restart, with no 'remuda exec butler' call.
...
$ echo EXIT=$?
EXIT=0
$ ps -eo pid,ppid,args | grep -F 'target/debug/remuda -s default daemon'
2817834 ...
$ pgrep -P 2817834
2817841
$ kill -9 2817834 2817841
$ ps -p 2817834 -p 2817841; echo exit=$?
    PID TTY          TIME CMD
exit=1
```

The real host's own systemd user session and `$HOME` were checked after
every run above, not assumed untouched:

```
$ systemctl --user list-units 'remuda*' --all
0 loaded units listed.
$ systemctl --user list-timers 'remuda*' --all
0 timers listed.
$ ls ~/.config/remuda
ls: cannot access '/home/toracle/.config/remuda': No such file or directory
```

Full gate, re-run after reverting the temporary artificial-delay
instrumentation (`git diff --stat` at this point shows only
`docs/install-butler.sh` changed), all still green:

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "FAILED|error\[|error:|test result"
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 27 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 107 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.19s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 0 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 45 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.20s
test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.74s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.73s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s

$ cargo fmt --all -- --check
$ shellcheck docs/install-butler.sh
$ cargo clippy --workspace --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.52s

$ python3 scripts/check-butler-path-convention.py
ok — install-butler.sh and init.lua agree: $HOME/.config fallback, segments ['/remuda/butler/config', '/remuda/butler/token']
ok — install-butler.sh and daemon.rs agree: $HOME/.config fallback, init.lua at $config_home/remuda/init.lua
$ python3 scripts/check-comments.py
ok — 357 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-install.py
ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
$ python3 scripts/check-principles.py
ok — 14 principles, every named mechanism exists (21 CI jobs, 11 denied paths, 412 test fns seen)
$ python3 scripts/check-steps.py
ok — 35 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-workflows.py
ok — 3 workflow file(s) parse, 21 job(s) defined
```

## Known ceilings

- The poller (`butler-poll.sh`/timer/plist) is now redundant for the
  *daemon-restart* scenario specifically — butler is back the instant the
  next command lazily starts a fresh daemon, not up to 15s later. It remains
  necessary for two other reasons the loader does not cover: (1) nothing else
  causes a daemon to exist at all after a reboot — the loader only ever runs
  as *part of* a daemon boot, it cannot trigger one — and (2) it is a safety
  net if the loader itself ever fails (a typo, a stale binary). Named in
  `docs/install-butler.sh`'s own top comment, not re-argued there.
- An older `remuda` binary (built before this loader existed) silently
  ignores `$config_home/remuda/init.lua` — not a trap, a fact: nothing reads
  the file, so nothing runs, same as if it were absent.
- The loader runs off `serve()`'s own thread, concurrently with
  `listener.incoming()` starting — not strictly *before* it. A `remuda ls`
  issued in the narrow window between a fresh daemon accepting its first
  connection and the loader's own `remuda.new` call finishing could
  legitimately not see butler yet. Every test and the installer's own
  verification account for this with a bounded retry, never a single
  immediate check — `native/tests/daemon.rs` polls every ~50ms against a
  10s `PATIENCE` deadline; `docs/install-butler.sh`'s post-restart check
  (originally shipped as a single-shot check in this same step — see the
  follow-up transcript in `## Actual` below) polls every 500ms, up to 20
  attempts (10s total), the same order of magnitude as `PATIENCE` for the
  identical race.
- The reboot leg (`OnBootSec=10s` actually firing, `loginctl enable-linger`
  surviving a real logout/reboot) is a pre-existing, already-named limitation
  in `docs/install-butler.sh`, untouched by this change.
- `scripts/check-butler-path-convention.py`'s new second check is a regex
  extraction, same caveat as its first: it does not execute either language,
  so a sufficiently creative rewrite of either side's literal could still
  slip past it undetected — matching the existing check's own stated scope.

## Not verified

- The reboot leg is still structurally untestable on this shared host (same
  as every prior step touching `install-butler.sh`).
- macOS/launchd was not exercised live — this box is Linux; the `.plist`
  branch was not touched by this change at all (only the Linux/systemd branch
  gained the new `init.lua` write + verification step, and both branches
  share the same `config_home`-derived write target, so the divergence risk
  is in the shared code, already covered by the scratch run above).
- No real `claude` CLI or real Matrix homeserver was exercised anywhere in
  this step, matching the restraint the rest of this suite already uses (no
  real credentials, no public-CI Anthropic auth) — every reproduction used
  either the repo's own test-mode hooks (`_butler_argv`, `_butler_skip_relay`,
  baked into the auto-loaded file itself for the Rust tests) or, for the
  installer's live probe, a stand-in `claude` shim.
