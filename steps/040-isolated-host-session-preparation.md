# 040 — test ownership with a real Session before transport adoption

## Before

Inactive foundations had no child/session integration. Draft review reproduced
an OSC cap bypass (ESC LF ] fed8193 bytes to vte despite the4096 filter limit), a
completion-token gap and a lifecycle mismatch: Session's successful terminate
latches dead, preventing later polling by a backend that deferred its wait.
Quiet pending-wake receivers also remained apparently connected after drop.

## Desired outcome

Prepare one host-owned Session, retained whole input acts/results, a bounded
terminal backend and private channel components with meaningful isolated tests.
The increment stays disconnected from daemon/UI/cluster/default launch behavior.
Actual host executable/controller-death/registry/Windows/Butler integration remains
required; this is not feature delivery or an aggregate memory/fairness proof.

## Expected

Independent waiter retains real exit/signal status despite Session's alive latch;
output EOF stays distinct. Host callbacks answer an actual child's DSR; Session
input_text emits actual bracketed-paste bytes. A barrier holds feed in its pause
while controller release refuses a new writer. Exact admitted payload is claimed
once; rich outcomes recover without replay. OSC controls cannot bypass the pinned
parser boundary, and quiet dropped subscribers free their slots. Authenticated
channel failures poison state; absolute frame budgets defeat byte-drip extension.

## Actual

Negative controls observed the intended failures before correction:

```text
controls_inside_escape_cannot_bypass_osc_payload_cap ... FAILED
control 10, payload 8193
agent::wakeup_lifetime_tests::dropped_receiver_is_detected_even_with_a_pending_wake ... FAILED
assertion failed: !signal.wake()
```

Corrected Session-level and ownership tests pass:

```text
running 4 tests
test session_host::owned::lifecycle_tests::host_answers_actual_child_dsr_without_a_controller ... ok
test session_host::owned::lifecycle_tests::session_input_text_obeys_host_bracketed_paste_mode ... ok
test session_host::owned::lifecycle_tests::session_close_retains_signal_status_and_child_is_actually_reaped ... ok
test session_host::owned::tests::entire_feed_and_rich_outcome_survive_controller_release_during_pause ... ok
test result: ok. 4 passed; 0 failed
cargo clippy --offline --workspace --all-targets -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.52s
```

The complete isolated host subset passed36 tests. Whole remuda-core suite passed after receiver-lifetime change. Private socket tests
required sandbox escalation because bind returned Operation not permitted; scratch
endpoint tests passed there. No live daemon/pane or real agent was accessed.

Residual gates are explicit in docs/concepts/session-host-runtime-gates.md and
issue713: transport admission/instance roles/negative fixtures, actual process
handoff, complete operations seam, durable registry and companion Butler recovery,
Windows native lifecycle/authentication, compatibility, fairness and reviewed rollout.
