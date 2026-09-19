# 034 — birth environment is not per-request environment

## Before

`packages/butler/init.lua` read `REMUDA_BUTLER_TOKEN`/`REMUDA_BUTLER_CONFIG`
via `os.getenv`. That call reads the *daemon process's own* environment,
fixed forever at whichever moment first birthed that daemon — never
anything a later CLI client passes on its own command line (the IPC
`Request::Eval` carries only source text, not env).

Reproduced for real, on this build, before the fix:

```
$ env -u REMUDA_BUTLER_TOKEN -u REMUDA_BUTLER_CONFIG -u XDG_CONFIG_HOME \
    HOME=$SCRATCH/home REMUDA_RUNTIME_DIR=$SCRATCH/runtime REMUDA_NO_UPDATE_CHECK=1 \
    ./target/debug/remuda -s repro ls
remuda: started a daemon for "repro"
no sessions

$ env -u REMUDA_BUTLER_TOKEN -u REMUDA_BUTLER_CONFIG REMUDA_RUNTIME_DIR=$SCRATCH/runtime \
    REMUDA_NO_UPDATE_CHECK=1 ./target/debug/remuda -s repro exec butler
remuda: runtime error: remuda-butler needs REMUDA_BUTLER_TOKEN and REMUDA_BUTLER_CONFIG set
```

Even with the token/config files sitting right there on disk, a *later*
call carrying the right env can never reach the daemon that was already
born without it — the daemon's own environment is fixed at birth, and
nothing short of killing it changes that. Concretely, this is the shape
under `docs/install-butler.sh`'s systemd timer / launchd agent: the ongoing
supervision is "poll `remuda ls`, if `butler` is absent, `exec butler`". If
anything else — a colleague's own shell touching `remuda` first after boot,
before the timer's first tick — births the daemon without butler's env
vars, every subsequent poll fails identically, forever, with no self-heal.
Because the poller runs under a timer, not a terminal, the only place this
surfaces is the systemd journal; to a colleague watching `remuda ls`, it is
indistinguishable from silence.

## Desired outcome

`exec butler` finds its token/config regardless of what first birthed the
daemon, as long as the files sit at a conventional path under `HOME` — a
value present in essentially every process's environment (the known
exception: a systemd *system* unit with `User=` set but no PAM session, or
a process launched via `env -i`), unlike a butler-specific var only a
caller who already knows about butler would set; when it's genuinely
absent, `resolve_path` fails loudly by name rather than guessing.
`REMUDA_BUTLER_TOKEN`/`REMUDA_BUTLER_CONFIG` remain a supported override,
so every existing test that sets them via `Daemon::spawn_with_env` keeps
working unchanged. A missing or malformed file still fails loudly,
immediately, naming the exact path tried — never a silent bad value.

## Expected

- `packages/butler/init.lua`: `token_path`/`config_path` resolve to
  `${REMUDA_BUTLER_TOKEN}` / `${REMUDA_BUTLER_CONFIG}` if set (override,
  checked first), else `${XDG_CONFIG_HOME:-$HOME/.config}/remuda/butler/{token,config}`
  — mirroring `docs/install-butler.sh`'s own default exactly, computed the
  same way in both places so they can't drift. Each resolved path is
  verified openable (`io.open`, closed right after) before anything else
  runs; a missing file or an unset `HOME` with no override both `error(...,
  0)` by name, mentioning the override var.
- `docs/install-butler.sh`: after its existing validation/`chmod 600`, copy
  the resolved `token_file`/`config_file` to the conventional path if an
  override pointed elsewhere (no-op when already there). The generated
  systemd `.service` and launchd `.plist` carry **no** `REMUDA_BUTLER_TOKEN`/
  `REMUDA_BUTLER_CONFIG` at all — the persistent unit needs no butler-specific
  environment now that the files are guaranteed to be at the path `init.lua`
  looks for by default.
- New regression coverage in `native/tests/daemon.rs`: a daemon born with
  **zero** butler env vars but the files present at the conventional path
  under a scratch `HOME` — `exec butler` must succeed. A sibling test: a
  daemon born with a scratch `HOME` that has no token file and no override —
  `exec butler` must fail loudly, before ever writing the `.mcp.json`
  companion file.
- Every existing test that calls `butler_config()` + `Daemon::spawn_with_env`
  with the env vars set explicitly keeps passing unchanged.

## Actual

Red, on the unmodified `init.lua` (fix stashed), same scenario as the new
regression test:

```
$ git stash push -- packages/butler/init.lua
$ cargo test --test daemon butler_exec_finds_credentials_at_the_conventional_path_with_zero_env_vars -- --nocapture
thread 'butler_exec_finds_credentials_at_the_conventional_path_with_zero_env_vars' panicked at native/tests/daemon.rs:2852:5:
expected exec butler to succeed using only the conventional HOME-based path, with no REMUDA_BUTLER_TOKEN/CONFIG at all -- stderr: remuda: runtime error: remuda-butler needs REMUDA_BUTLER_TOKEN and REMUDA_BUTLER_CONFIG set
stack traceback:
	[C]: in ?
	[C]: in function 'error'
	packages/butler/init.lua:155: in main chunk
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 41 filtered out; finished in 0.03s
$ git stash pop
```

Green, with the fix restored:

```
$ cargo test --test daemon butler_exec_ -- --nocapture
running 2 tests
test butler_exec_fails_loudly_with_no_token_file_and_no_env_override ... ok
test butler_exec_finds_credentials_at_the_conventional_path_with_zero_env_vars ... ok
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 40 filtered out; finished in 0.04s
```

Full gate. `cargo fmt --all --check` and `shellcheck` print nothing at all on
success (their absence from the transcript below is the real, empty stdout,
not an omission) — every command shown exited 0:

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "FAILED|error\[|error:|test result"
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 27 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 107 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.16s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 0 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 42 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.09s
test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.73s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.63s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s

$ cargo fmt --all --check
$ cargo clippy --workspace --all-targets -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.04s
$ shellcheck -s sh docs/install-butler.sh
$ python3 scripts/check-comments.py
ok — 353 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-install.py
ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
$ python3 scripts/check-principles.py
ok — 14 principles, every named mechanism exists (20 CI jobs, 11 denied paths, 406 test fns seen)
$ python3 scripts/check-steps.py
ok — 33 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-workflows.py
ok — 3 workflow file(s) parse, 20 job(s) defined
```

The exact poisoning scenario from the bug report, reproduced end to end
against the real built binary (not just the Rust harness), using the
existing test-mode hooks (`remuda -e` to set `_butler_argv`/
`_butler_skip_relay`) to avoid a real `claude`/Matrix spawn:

```
$ env -u REMUDA_BUTLER_TOKEN -u REMUDA_BUTLER_CONFIG -u XDG_CONFIG_HOME \
    HOME=$SCRATCH/home REMUDA_RUNTIME_DIR=$SCRATCH/runtime REMUDA_NO_UPDATE_CHECK=1 \
    ./target/debug/remuda -s repro ls
remuda: started a daemon for "repro"
no sessions

$ env REMUDA_RUNTIME_DIR=$SCRATCH/runtime REMUDA_NO_UPDATE_CHECK=1 \
    ./target/debug/remuda -s repro -e 'remuda._butler_argv = {"sh", "-c", "sleep 0.3; exit 0"}'
$ env REMUDA_RUNTIME_DIR=$SCRATCH/runtime REMUDA_NO_UPDATE_CHECK=1 \
    ./target/debug/remuda -s repro -e 'remuda._butler_skip_relay = true'

$ env -u REMUDA_BUTLER_TOKEN -u REMUDA_BUTLER_CONFIG REMUDA_RUNTIME_DIR=$SCRATCH/runtime \
    REMUDA_NO_UPDATE_CHECK=1 ./target/debug/remuda -s repro exec butler
exec exit=0
```

— where `$SCRATCH/home/.config/remuda/butler/{token,config}` held the
credentials, and this `exec butler` call's own environment carried zero
butler vars, exactly like the daemon that birthed it. Confirmed the session
actually registered, then tore the daemon down and confirmed it gone by PID
(not by exit code):

```
$ env REMUDA_RUNTIME_DIR=$SCRATCH/runtime REMUDA_NO_UPDATE_CHECK=1 ./target/debug/remuda -s repro ls
remuda-fresh           80x24   live  idle 0s
remuda-fresh           80x24   live  idle 0s
no sessions
remuda-fresh           80x24   live  idle 0s

$ ps -eo pid,ppid,args | grep -F 'repro daemon'
2428133     948 /.../target/debug/remuda -s repro daemon

$ env REMUDA_RUNTIME_DIR=$SCRATCH/runtime REMUDA_NO_UPDATE_CHECK=1 ./target/debug/remuda -s repro restart -f
remuda: stopped the daemon for "repro" — the next command starts a fresh one

$ ps -p 2428133; echo "exit=$?"
    PID TTY          TIME CMD
exit=1
```

The `ls` output flickering between the session and "no sessions" is the
0.3s respawn cycle running for real; `ps -p <the daemon's own pid>`, not a
grep for the scratch path (nothing in this daemon's argv ever names it —
`_butler_skip_relay` was set, so no python3 relay child exists either), is
what actually proves the process is gone, and it is, before the scratch
tree was deleted.

The installer's own copy-to-canonical step, run for real with a
`REMUDA_BUTLER_TOKEN_FILE`/`REMUDA_BUTLER_CONFIG_FILE` pointing at a
non-default vault location, a fake `claude` standing in for the real one
(the probe itself has no test-mode hook, unlike the Rust harness), and a
fake `systemctl` intercepting `--user daemon-reload` so no real systemd
call was made:

```
install-butler.sh: copied /tmp/ibr-2373990/vault/token to /tmp/ibr-2373990/home/.config/remuda/butler/token (init.lua's default lookup)
install-butler.sh: copied /tmp/ibr-2373990/vault/config to /tmp/ibr-2373990/home/.config/remuda/butler/config (init.lua's default lookup)
install-butler.sh: registering butler in the running daemon (this starts one if none is up)...
install-butler.sh: butler registered for this run.
install-butler.sh: wrote /tmp/ibr-2373990/home/.config/remuda/butler-poll.sh
install-butler.sh: wrote /tmp/ibr-2373990/home/.config/systemd/user/remuda-butler.service
install-butler.sh: wrote /tmp/ibr-2373990/home/.config/systemd/user/remuda-butler.timer
install-butler.sh: daemon-reload done. Two more steps, run these yourself:
install exit=0

$ cat /tmp/ibr-2373990/systemctl.log
FAKE systemctl called with: --user daemon-reload

$ stat -c '%a %n' .../butler/token .../butler/config
600 /tmp/ibr-2373990/home/.config/remuda/butler/token
600 /tmp/ibr-2373990/home/.config/remuda/butler/config

$ grep -n REMUDA_BUTLER .../systemd/user/remuda-butler.service; echo "exit=$?"
exit=1
$ grep -n REMUDA_BUTLER .../systemd/user/remuda-butler.timer; echo "exit=$?"
exit=1
```

Zero hits in both generated unit files. Re-run with the default location
(no override, files already at the conventional path) showed the copy
branch skipped entirely — no "copied ... to ..." status line, and the
token/config files' inode+mtime were byte-identical before and after:

```
before: token=7789571 1789836289 config=7789572 1789836289
after: token=7789571 1789836289 config=7789572 1789836289
NO-OP CONFIRMED (inode+mtime unchanged)
```

This run's own daemon and fake-`claude` child were torn down and confirmed
gone by PID, the same way as above:

```
$ ps -eo pid,ppid,args | grep -F "$SCRATCH"
2431424 2431416 /bin/sh $SCRATCH/bin/claude --mcp-config $SCRATCH/vault/config.mcp.json ...

$ ./target/debug/remuda restart -f
remuda: stopped the daemon for "default" — the next command starts a fresh one

$ ps -p 2431416,2431424; echo "exit=$?"
    PID TTY          TIME CMD
exit=1
```

before the scratch tree was deleted. (This is a re-verification, not the
original session's own transcript, so it re-runs the override case above,
not a second, separate default-location daemon — the property being proven,
"the installer's own daemon leaves no residue," is the same either way.)

## Known ceilings

- Every **other** environment variable the daemon's Lua image reads via
  `os.getenv` still has the exact same birth-environment property this step
  fixes only for butler's token/config paths. That is a `remuda`-architecture
  question, not this feature's — named in a comment at the top of
  `docs/install-butler.sh` rather than solved here.
- The launchd `.plist` branch of `docs/install-butler.sh` was verified by
  reading the diff, not by execution — this box is Linux, `uname -s` in the
  script is not overridable from the outside, and there is no macOS runner
  available here. The change removing its `EnvironmentVariables` dict is
  structurally identical to the systemd change that *was* run for real.
- The reboot leg (`OnBootSec=10s` actually firing, `loginctl enable-linger`
  actually surviving a real logout/reboot) was already an open, named
  limitation before this step (see the pre-existing comment in
  `docs/install-butler.sh`) and remains so — untouched by this change.

## Not verified

- No real `claude` CLI or real Matrix homeserver was exercised anywhere in
  this step — every reproduction used the repo's own test-mode hooks
  (`_butler_test_mode`, `_butler_argv`, `_butler_skip_relay`) or, for the
  installer's live probe, a fake `claude` shim, matching the restraint the
  rest of this suite already uses for the same reason (no real credentials,
  no public-CI Anthropic auth).
