# 081 — List sessions as JSON for native adoption fallback

## Before

`remuda ls` prints only a human table. A native client that needs an existing
session's exact `id` and `instance_id` can reach the daemon with the official
local `Request::List`, but no public CLI exposes that answer: the Lua
`remuda.session.list()` projection is for scripts and does not define a CLI
contract. A consumer forcing a reload or a new daemon would end the running
sessions, which is not acceptable for activation.

## Desired outcome

- `remuda ls --json` prints one JSON array of the existing `SessionSummary`
  wire objects from `client::request(selected_path, Request::List)`. No wrapper,
  renamed field, projection, sorting or ANSI is added; new wire fields pass
  through.
- The selected server remains the one every other CLI verb uses: the path is
  `daemon::socket_path(server)` for the caller's `-s`, and the command uses
  `with_existing_daemon`, so it never falls back to the default daemon or
  starts one.
- Empty selection prints `[]`. A transport or error response prints nothing on
  stdout, writes one error line to stderr and exits nonzero, so a consumer
  never parses a partial array.
- Plain `remuda ls` keeps its exact previous output and exit codes.
- `--json` is parsed by the shared CLI parser; duplicates, values
  (`--json=1`) and stray arguments are usage errors; `--help` makes no request.

## Expected

- `cargo test -p remuda-native --bin remuda` covers flag parsing/refusal, the
  plain-array shape with an instanced and a legacy summary, empty `[]`, and the
  unchanged human line.
- `cargo fmt --all -- --check`, `cargo clippy -p remuda-native --bin remuda --
  -D warnings` and the focused repository checks stay clean.
- A disposable two-daemon probe shows `-s A ls --json` lists only A's session
  with exact `id`/`instance_id`, `-s B` lists only B's, a missing daemon writes
  nothing to stdout, and the old Lua projection still answers beside it.

## Actual

```text
$ cargo test -p remuda-native --bin remuda
running 258 tests
test result: ok. 258 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

$ cargo clippy -p remuda-native --bin remuda -- -D warnings
Finished

$ cargo fmt --all -- --check
FMT_CLEAN

$ python3 scripts/check-comments.py
ok — 2036 doc comment(s) within cap (item 3, module 20)
```

Focused tests added in `bin/remuda.rs`:

- `ls_json_flag_is_optional_and_unambiguous`: `ls` and `ls --json` accepted,
  duplicate/value/stray forms refused, `--help` is help.
- `ls_json_is_a_plain_array_of_the_wire_objects`: parsed array, exact
  `id`/`name`/`instance_id`, omitted instance stays omitted, empty is `[]`, no
  escape bytes.
- `the_human_ls_line_is_unchanged`: exact previous table line and attachment
  suffix.

Disposable two-daemon probe (private `REMUDA_RUNTIME_DIR`, two live sessions,
both daemons stopped and the private root removed afterwards):

```text
$ remuda -s alpha-srv ls --json
[{"alive": true, "attached": false, "human_idle": null, "id": "9129-1",
  "idle": {"nanos": 71391875, "secs": 1},
  "instance_id": "00000000000023a918e5306c0a205518-0000000000000001",
  "mouse_tracking": false, "name": "alpha", ...}]

$ remuda -s beta-srv ls --json
[{"id": "9144-1", "name": "beta",
  "instance_id": "00000000000023b818e5306c0cf15b20-0000000000000001", ...}]

$ remuda -s alpha-srv ls
alpha                  80x24   live  idle 1s

$ remuda -s missing-srv ls --json; echo $?
remuda: no daemon running for "missing-srv" (socket .../missing-srv.sock); ...
1
```

The missing-daemon run wrote an empty stdout and only the stderr line. The two
daemons never cross-listed (`alpha` only under `-s alpha-srv`, `beta` only
under `-s beta-srv`), so the command follows the selected socket with no
default-daemon fallback. The old Lua `remuda.session.list()` call beside it
still returned `{id, instance_id, name}`.

## Limits

This step adds no IPC, no wire change and no adoption decision. A missing
`instance_id` is omitted from the JSON, and the consumer must refuse rather
than invent a fence. `--json` is additive; old binaries keep the table.
