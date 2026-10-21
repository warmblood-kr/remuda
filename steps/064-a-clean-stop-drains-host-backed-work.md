# 064 — a clean stop drains host-backed work

## Before

The drain in the #758 design was not implemented. On `remuda stop` the daemon
replied, reaped Lua processes and exited at once. An act a session host was
running at that moment (say, a `feed` with a pause) lost its client: the
caller got "the daemon hung up without answering", and the act was left
pending for the next daemon to resolve by key. Nothing stopped new
host-backed work from being admitted during shutdown. Item G4 of the
`remuda reload` gap list (#713); a reload needs the same drain.

## Desired outcome

- A gate (`native/src/session_host/gate.rs`) admits every host-backed act,
  control call (resize, close) and host start. A drain closes it for good:
  new work gets `Unavailable(Recovering)`, before any reservation or lease.
  Reads (inspect, capture, snapshot) are not gated.
- The drain waits until every admitted piece of work has finished, or the
  deadline passes (35 s: the 30 s act deadline plus pauses). What is still
  running at the deadline stays pending and recoverable by key; the daemon
  logs how many.
- A clean `Shutdown` replies, removes the socket, then drains, then exits.
  `remuda stop` returns at once, as before. Hosts are never stopped. An idle
  daemon holds no lease, so after the drain no lease is left held.
- SIGTERM and SIGINT still exit at once; only a clean `Shutdown` drains.

## Expected

- `session_host_drain::a_clean_stop_lets_an_admitted_hosted_act_finish_and_frees_the_host`:
  a feed act pauses 1.5 s, a stop arrives mid-pause. The stop answers and the
  socket goes away within 2 s. The act is answered `Ok`, and the daemon exits
  only after that. The next daemon sees the act's effect and drives the host
  at once (within 2 s, so no lease was left held).
- `remote::a_drained_gate_refuses_acts_and_control_before_any_effect_but_reads_go_on`:
  against a real host process, input, resize and close are refused as
  `Recovering` with no effect and no reservation; inspect and capture work.
- Gate unit tests: a drain waits for passes out and refuses new ones; it
  gives up at its deadline and reports what is left; an idle gate drains at
  once.

## Actual

```
$ cargo test -p remuda-native --test session_host_drain
test result: ok. 1 passed; 0 failed

$ cargo test -p remuda-native --lib gate
test result: ok. 10 passed; 0 failed

# drain_session_hosts removed from the shutdown path:
assertion `left == right` failed: the admitted act was cut
  left: Error("the daemon hung up without answering")
 right: Ok
```

Limits: while a drain runs, the old daemon still holds the socket lock, so
a CLI call in that window that autostarts a new daemon waits up to 3 s for
the lock and may fail. The drain normally lasts only as long as one act.
Pending Lua deferred replies were already failed before the drain
(`shutdown_pending_replies`).
