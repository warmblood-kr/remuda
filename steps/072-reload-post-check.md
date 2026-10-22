# 072 — `remuda reload` checks the new daemon

## Before

After `reloaded:`, the CLI asked only for the new daemon's version and
printed it. A wrong build answering, a host the new daemon did not list, or
an `init.lua` the new build could not load all looked like success. That
last one is real: the user edits `init.lua` after the old daemon started.

## Desired outcome

`reload::post_check` runs after `reloaded:`. The old daemon is gone by then,
so a failed check cannot roll back; the CLI says what is wrong and exits 1.
- `daemon now BUILD`, and BUILD must be the reload's target.
- `sessions: all N host-backed listed`: every host the report names is
  listed and alive.
- `init.lua: loaded`, polled up to 10 s through `remuda.keymap_snapshot()`,
  the daemon's own record of the load.
  - Failure fails the check only if it is new. The CLI records the old
    daemon's `init.lua` error before reloading, and the same error after
    reads "failed, as it did before the reload".
  - A daemon without the status says "status unknown"; that is not a
    failure.

## Expected

`reload_cutover`:
- the handover prints `sessions: all 2 host-backed listed` and
  `init.lua: loaded`;
- an `init.lua` broken after the old daemon loaded it: exit 1,
  `init.lua failed in the new daemon: … broken since the old daemon
  started`, and the session still answers;
- an `init.lua` broken all along: exit 0, `init.lua failed, as it did
  before the reload: …`.

## Actual

```
$ cargo test -p remuda-native --test reload_cutover
test result: ok. 7 passed; 0 failed

# red: a new init.lua failure not failing the check
panicked at native/tests/reload_cutover.rs:412:5   (exit 0, not 1)
# red: without comparing with the old daemon's error
panicked at native/tests/reload_cutover.rs:458:5   (exit 1)
```
