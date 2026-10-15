# 041 — a standalone experimental host outlives its controller

## Before

Merged preparation #715 has no executable or actual controller-process handoff.
Component tests cannot prove that process/session/terminal/input ownership stays
alive when a separate controller is killed. Startup ACK errors also must not
terminate a host whose private manifest was already committed.

## Desired outcome

An explicitly opt-in macOS/Linux prototype owns one Session independently, serves
private authenticated fenced RPC, and supports coherent paged current-screen
capture, input outcome recovery, resize, confirmed close/stop. The default daemon,
UI, cluster and existing sessions remain unchanged. Windows/full registry/Butler
recovery and rollout are subsequent required gates in issue713.

## Expected

Kill the fixture's actual controller process with SIGKILL after admission and
mid-feed after a child-computed first effect. Reconnect to the same immutable IDs
and child PID, preserve styled terminal/modes/geometry, finish the complete act,
and query/retry its stable request identity without executing it again. Wrong
identity/View mutation/partial snapshot ACK must refuse. A full preauth worker
budget must not stall PTY parsing. Wait status and native EOF/hangup must remain
separate from descendant lifetime. Missing startup ACK preserves a committed host;
a same-private-root retry refuses before another child starts.

## Actual

A negative control restored fatal startup-ACK propagation. With the parent read
end closed before launch, the host disappeared and the recovery assertion failed;
restoring independent best-effort ACK delivery made the same test pass.

```text
test lost_launch_ack_preserves_committed_host_and_same_root_retry_cannot_duplicate ... FAILED
isolated client failed: exit status: 1
```

The standalone test harness uses env-cleared synthetic children and fresh private
scratch endpoints; it never contacts a live daemon/pane or launches a real agent.
The initial seven process fixtures passed:

```text
running 7 tests
test descendant_lifetime_and_terminal_end_are_distinct_from_direct_child_exit ... ok
test preauthentication_admission_is_bounded_without_stalling_pty_parser ... ok
test lost_launch_ack_preserves_committed_host_and_same_root_retry_cannot_duplicate ... ok
test view_role_wrong_manifest_and_incomplete_snapshot_ack_do_not_mutate_host ... ok
test closing_terminal_handles_does_not_invent_process_exit_or_allow_stop ... ok
test controller_sigkill_preserves_child_identity_terminal_and_admitted_input_result ... ok
test controller_sigkill_after_first_effect_preserves_remainder_and_dedup ... ok

test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.80s
```

Fixture development found that assuming immediate EOF after stdio close/ctty
manipulation was invalid on this macOS host. The native fixture now verifies its
stdio descriptors are actually closed while the process remains alive, without
inventing an EOF assumption. A separate descendant heartbeat proves direct-child
exit cannot be treated as all descendants ended. Output end reason/drained
generation are explicit. Passing tests are author evidence, not runtime/rollout
approval; independent exact-head review and supported-platform CI remain required.

Independent review found that the client requested required capabilities but
did not verify them in the authenticated Opened response. An authenticated mock
omits each Control requirement in turn and observes that no Acquire/Close request
is sent. Before the fix it caught the actual Acquire; after checking every
requested capability it passes, alongside the original process proofs.

```text
missing_required_capability_refuses_before_any_controller_mutation ... FAILED
client sent a request after peer omitted inspect

missing_required_capability_refuses_before_any_controller_mutation ... ok
test result: ok. 38 passed; 0 failed
```
