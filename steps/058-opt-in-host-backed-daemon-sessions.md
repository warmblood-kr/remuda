# 058 — opt-in host-backed daemon sessions

## Before

`HostSession` (step057) could drive a separate host through
`SessionOperations`, but nothing in the daemon used it. `new` always spawned
an in-process `PtyAgent`. The `Registry` held only local `Session`s, and
`SessionHandle::remote` was test-only. When the daemon died, the PTY master
died with it, so every session ended.

## Desired outcome

- **Opt-in only.** With `REMUDA_EXPERIMENTAL_SESSION_HOST=1` in the daemon's
  environment, `new` reserves a launch in this daemon's private inventory
  (`<socket>.hosts/`, mode 0700). It then starts a detached
  `remuda-session-host run-reserved` with a cleared environment, waits for
  publication, and registers a checked handle. Without the variable nothing
  changes: sessions stay in-process, and no inventory is created.
- **Registry acceptance.** `SessionHandle::hosted` is the checked production
  constructor. `Registry` keeps hosted handles in a separate map that shares
  one name space with local sessions, and legacy `get` never returns one.
  `send`, `send_line`, `clear_input`, `feed`, `resize`, `capture`, `close`
  and `ls` route to the hosted handle. Unknown outcomes keep their no-retry
  meaning.
- **Discovery after a restart.** At startup, with or without the opt-in,
  the daemon claims its inventory writer fence and re-registers every
  published host. Exited ones are torn down. A host that is gone stays
  listed and answers `Unavailable`; it is never dropped or respawned.
- **Close.** Close ends the child, or accepts a host that is provably gone
  (no listener), and records a durable close mark. A background teardown
  then resolves any pending act by its key, retires retained results, stops
  the host (skipped when it is already gone) and retires the entry. A restart
  tears marked entries down instead of registering them again.
- **Earlier acts.** Before new input, an act that an earlier daemon reserved
  is resolved by its original key, never re-issued. Its result stays
  retained for `recover`. Only while it is still unresolved does new input
  get `Indeterminate`.
- **Spawn failures.** A missing host binary or a name that is still held is
  refused before any reservation. A child that exits at once is torn down.
  A host that is late to publish is reported as possibly started.
- **Fences.** `resize`, `terminate` and the host `Stop` check the writer
  fence before taking the lease (check, then act; the host does not see the
  fence). An unreachable host is refused before any reservation. After 5 s
  without a listener, a reserved act returns `Indeterminate`. A host that
  answers Busy, for example while it runs the admitted act, is not counted.
- Styled capture, sync, mouse state, attach and sequenced input refuse
  host-backed sessions with "not supported yet".

Out of scope: attach, output wakeups and Lua output hooks, and automatic
reaping of exited hosted sessions (they stay listed until closed). An entry
whose host died before its exit was published cannot be retired yet. It
stays hidden but holds its name until an owner-fenced abandon exists. Daemon
drain and handover are in `docs/design-713-daemon-drain.md`. Windows (#214)
and host replace/rollback are out.

## Expected

- Without the opt-in, `new` runs in-process: no host process starts and no
  inventory is created.
- With the opt-in, the daemon is SIGKILLed. The child and host keep running.
  A restarted daemon, started without the opt-in, lists the session and
  shows the same child pid. A new line lands exactly once.
- After the host is SIGKILLed, `capture` and `send` answer `Unavailable`.
  After another daemon restart, the session is still listed and still
  `Unavailable`.
- Close stops the host process and frees the name for reuse. A session
  closed just before a daemon SIGKILL does not come back, and its host
  stops.
- A session whose host was SIGKILLed can still be closed, and a restart does
  not bring it back.
- An old owner's `resize` and `terminate` are refused after a newer claim.
- A 6 s lease held by another controller is Busy, and the act completes.
- An act the old daemon left running is resolved by key; the next line lands
  once and the old result is still recoverable.
- A host stopped before a crash is still retired by a new daemon.

## Actual

```text
cargo test -p remuda-native --test session_host_daemon   (10 repeated runs)
test result: ok. 4 passed; 0 failed; finished in 3.50-3.87s
cargo test -p remuda-native --lib session_host::remote::tests   (25 runs)
test result: ok. 11 passed; 0 failed; 1 ignored; finished in 11.1-11.3s
cargo test -p remuda-native --lib session_host   (3 runs)
test result: ok. 148 passed; 0 failed; 5 ignored; finished in 66.3-66.8s
cargo test -p remuda-native --test session_host_process
test result: ok. 8 passed; 0 failed
cargo test -p remuda-core: 167 passed; 0 failed
cargo test --workspace --all-targets: 2231 passed; 1 failed; 10 ignored
  (cluster_cli_golden fresh_state_cli_join: port 7441 is held by a live
   daemon on the test machine; unrelated to this change)
cargo clippy --workspace --all-targets -- -D warnings: Finished
```

The daemon in these tests is the built `remuda` binary, run as its own
process with a private runtime directory, and SIGKILLed. The host is the
built `remuda-session-host`. No installed binary or live daemon is touched.
Red checks: the Busy test fails when Busy counts as unreachable, and the
crash-after-Stop test fails without the gone-host check. Refs #713.
