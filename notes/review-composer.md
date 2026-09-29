# Remote composer review

Reviewed `feat/cluster-remote-composer` at `1ccf831f87acfbbb4e5ada2dad6b4906c911193e` against the CLIENT RULE and its remote Input addendum.

## Finding

- **Medium: a `RemoteControlDisabled` response leaves the target's client ID and next sequence unchanged even though the current batch is marked `Failed`.** `InputQueue::finish` returns the special `QueueEvent::RemoteControlDisabled`; `InputSender::finish` handles it by dropping waiting batches for that node, but does not run `rotate_target` (`native/src/cluster_tui/queue.rs:212-224`, `native/src/cluster_tui/sender.rs:216-233`). The rule requires rotating after every terminal Failed batch. The dropped batches have already consumed locally assigned sequence numbers, while the refusal occurs before dispatch; resuming this target with the old ID can therefore create a sequence gap. Rotate/reset affected per-session targets when disabled, even if the UI also suppresses future sends to that node.
- **Low: the oversize `Response::Error` bypasses the remote Error retry rule.** `send_remote_started` turns the exact `remote Input batch exceeds 12288 bytes` text into terminal `SendOutcome::Error` (`native/src/cluster_tui/sender.rs:195-197`), and its test expects `Failed`. The CLIENT RULE addendum says every listener Error on Input must retry the identical request and end `Uncertain`, never `Failed`. Preserve the requested bug notice while following that ambiguous-delivery policy; the current chunker should make this diagnostic path exceptional.

## Checks

- Remote input is split into chunks of `MAX_REMOTE_INPUT_BATCH_BYTES` (12 KiB); the 64 KiB line cap is enforced before enqueue. Batches remain FIFO.
- Every remote `Response::Error` except the exact oversize refusal maps to `IoFailure`; retries reconstruct the identical request and end as `Uncertain`. Typed `RateLimited` retries the same batch after backoff.
- `Uncertain` and ordinary `Failed` outcomes rotate the target ID and re-sequence its waiting batches from 1. `WrongInstance` drops the matching session's queued batches with a visible notice. `RemoteControlDisabled` drops only that node's queue and presents a notice; other nodes remain queued.
- Oversize refusal becomes a visible `bug:` failure. Production remote transport resolves the registry-pinned peer key and uses the one-shot Noise `ClusterClient`.
- No tests run during this static review.

## Re-check: `9257b5e9a8328456a0312d0055c98a31258ae3e`

- **PASS:** `RemoteControlDisabled` now drops waiting work for the node and rotates that target's client ID, so the next batch starts at sequence 1.
- Oversize `Response::Error` now follows the identical-request retry path and ends `Uncertain`, retaining the `bug:` diagnostic and rotating the ID before the next batch.
- RED-first verification: both added regression tests failed against `1ccf831` (old sequence remained 2; oversize Error did not schedule retry), then passed against `9257b5e`.
- **Verdict: PASS** for both requested fixes.
