# 082 — Resolve the OpenCode executable once for the owned TUI

## Before

`remuda _opencode_tui` started both its app-server and its TUI with
`Command::new("opencode")`. The ordinary OpenCode launcher instead resolves the
program first (`system.find_command`, then `$HOME/.opencode/bin/opencode`) and
starts that absolute path. On the long-running production daemon, PATH does not
contain `$HOME/.opencode/bin`, so the capability probe passed (it spawns
nothing), a native plan was selected, and the wrapper then exited in the same
second with `cannot start OpenCode server: No such file or directory (os error
2)`. No manifest was published and the new row was never live, so the join
reported the generic `missing_row_identity`.

## Desired outcome

- Resolve `opencode` exactly once at wrapper startup, before any binding or
  spawn, using `remuda_native::find_command::find_on_path` for PATH priority.
- If PATH has no usable entry (`NotFound`) or PATH is absent (`NoPath`), try
  `$HOME/.opencode/bin/opencode` once and accept it only when HOME is absolute
  and the candidate is a regular executable absolute file (`opencode.exe` when
  the platform suffix applies). A bare-name refusal is not retried.
- Pass that one absolute `PathBuf` to both `server_command` and `tui_command`;
  no bare `opencode` remains in either command. No new flag, environment
  variable, config key or resolver framework.
- If neither location is usable, fail before any spawn with one line naming
  both locations and a `Next:` step. Do not start a different program, a shell
  lookup or the PTY fallback.

## Expected

- Focused tests drive the resolver directly: PATH winner, HOME fallback after
  `NotFound`, HOME fallback after `NoPath`, neither (including no HOME) with no
  usable path, and bare-name refusal never trying HOME.
- `server_command` and `tui_command` expose the same absolute
  `get_program()`, with the existing server/TUI arguments unchanged.
- The old-PATH repro moves from `No such file or directory` to the configured
  fallback (visible through a stub executable) and still publishes no manifest
  when no OpenCode is usable.
- `cargo test -p remuda-native --bin remuda`, clippy `-D warnings`, fmt and the
  focused repository checks stay clean.

## Actual

```text
$ cargo test -p remuda-native --bin remuda
running 260 tests
test result: ok. 260 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

$ cargo clippy -p remuda-native --bin remuda -- -D warnings
Finished

$ cargo fmt --all -- --check
FMT_CLEAN

$ python3 scripts/check-comments.py
ok — 2037 doc comment(s) within cap (item 3, module 20)
```

Focused tests in `opencode_tui::tests`:

- `opencode_resolution_prefers_path_then_the_per_user_install`: PATH hit used
  unchanged; `NotFound` and `NoPath` both fall back to
  `/home/u/.opencode/bin/opencode`; neither/absent HOME, relative/empty HOME
  (even with an accepting predicate) and `BareName` fail without a fallback.
- `both_commands_start_the_same_absolute_executable`: both commands report
  `get_program() == /opt/bin/opencode`, with `serve --hostname 127.0.0.1 --port
  0` and `--server URL --session ID CWD` unchanged.

Old-PATH probe (exact daemon PATH from PID 35331, private 0700 binding dirs,
stub executables that print a marker and exit 7/8; no production daemon or
session touched, no manifest in any case):

```text
PATH contains stub, HOME also has a different stub:
  markers=PATH_STUB      rc=1  OpenCode server did not publish its URL

PATH has no opencode, HOME has the fallback stub:
  markers=HOME_STUB      rc=1  OpenCode server did not publish its URL

PATH has no opencode, HOME has no fallback:
  rc=1  opencode is not on PATH and $HOME/.opencode/bin/opencode is not executable.
        Next: install OpenCode or add its folder to PATH.   (no manifest)
```

## Limits

This is launcher parity, not a service-path change: a daemon process keeps its
own PATH, but the wrapper it spawns is the new CLI and now resolves the same
absolute program the ordinary launcher uses. The repaired-environment canary
and any default-environment canary remain separate owner/QA runs; this step
does not retry the failed task or touch the production daemon.
