# 059 — keep the cluster TUI responsive on a stalled peer

## Before

The cluster TUI (`remuda cluster remote`, the cluster session list) sometimes
moves its cursor very slowly. The screen content is not the cause: the remote
poller already fetches it on worker threads, and the UI copies a snapshot.
Three other things are:

- **Remote calls run on the UI thread.** Remote Input, InputLine,
  ResizeSession and Close each open a new connection to the peer and wait for
  the reply between drawing a frame and reading the next key. The wait is up
  to 1 s, or up to 8 s for InputLine, and failures are retried.
- **A failed resize triggers another one.** The pane size depends on notice
  rows and on the tree width. A peer that goes stale gets a longer label,
  which changes the pane size, which sends a resize to that peer. When the
  resize fails, its error notice changes the pane height and starts a new
  resize. The 1 s throttle counts from the start of the attempt, so after a
  1 s timeout the next attempt starts at once.
- **Keys mode holds keys while the user types.** A key buffer is sent only
  after 40 ms with no new key, but the loop wakes only on input, on a remote
  update, or after 250 ms. When keys arrive less than 40 ms apart nothing is
  sent; when they arrive less than 250 ms apart nothing is sent until an
  unrelated wakeup.

Measured with a probe that runs the loop body against a fake peer (160x48,
a key every 33 ms):

```text
blackholed peer (1 s timeout): frame 1010 ms, 6 keys handled in 6 s,
                               key latency max 5.4 s, 116 keys queued
keys mode, a key every 120 ms for 4 s: one send, after typing stopped
```

## Desired outcome

- Remote Input, InputLine, ResizeSession and Close run on a background worker,
  one per node and in FIFO order, so a slow or stalled peer never blocks
  drawing or key handling. A finished call wakes the loop, and the UI applies
  its result through the existing queue and notice code. The queue already
  allows one batch in flight at a time, so ordering and retry rules do not
  change.
- A failed resize backs off per session, whatever the size: 1 s, then 2 s,
  4 s, and so on up to 30 s, counted from when the call finished. Only one
  resize is in flight at a time.
- Buffered keys are sent no later than 40 ms after the first buffered key, and
  the loop wakes at that time even while keys keep coming.
- Local daemon calls are unchanged.

## Expected

- With a peer that blocks until the test releases it, `send_pending`, close
  and resize return immediately and leave the request in flight. After the
  release, the next drain applies the reply: the batch is Sent, the Close
  notice appears, or the resize is recorded.
- A failing remote resize is not resent when only the size changes; it is
  resent after the backoff.
- Keys typed every 10 ms for 200 ms start a send before typing stops.
- The probe from the investigation shows the blackholed-peer case at the
  healthy frame time, with no key backlog.

## Actual

The investigation probe, rerun on this change (release build, 160x48, the
same fake peers):

```text
A healthy nav, rtt 0: frame_mean=2.55ms keys_handled=122 key_latency_mean=1ms max=4ms backlog_at_end=0 remote_resize_calls=1 remote_input_calls=0
B slow peer nav, rtt 300ms ok: frame_mean=3.06ms keys_handled=122 key_latency_mean=2ms max=5ms backlog_at_end=0 remote_resize_calls=1 remote_input_calls=0
D blackholed peer nav, 1s timeout err: frame_mean=2.77ms keys_handled=122 key_latency_mean=1ms max=5ms backlog_at_end=0 remote_resize_calls=3 remote_input_calls=0
H peer blackholed at 2s, nav 33ms: frame_mean=2.93ms keys_handled=122 key_latency_mean=1ms max=5ms backlog_at_end=0 remote_resize_calls=3 remote_input_calls=0
I keys-mode typing 120ms, rtt 0: frame_mean=2.82ms keys_handled=34 key_latency_mean=3ms max=5ms backlog_at_end=0 remote_resize_calls=1 remote_input_calls=34
J keys-mode typing 120ms, rtt 150ms: frame_mean=2.69ms keys_handled=34 key_latency_mean=2ms max=5ms backlog_at_end=0 remote_resize_calls=1 remote_input_calls=18
K keys-mode typing 120ms, rtt 400ms: frame_mean=2.98ms keys_handled=34 key_latency_mean=2ms max=5ms backlog_at_end=0 remote_resize_calls=1 remote_input_calls=10
M keys-mode typing 300ms, rtt 400ms: frame_mean=2.67ms keys_handled=14 key_latency_mean=1ms max=3ms backlog_at_end=0 remote_resize_calls=1 remote_input_calls=8
L keys-mode typing 120ms, peer blackholed at 2s: frame_mean=2.57ms keys_handled=34 key_latency_mean=2ms max=5ms backlog_at_end=0 remote_resize_calls=1 remote_input_calls=15
```

Before this change the same probe gave `D blackholed peer`: frame 1010 ms,
6 keys handled, key latency max 5.4 s, 116 keys queued; `I-K keys mode`: one
send, at 4.97 s.

```text
cargo test -p remuda-native --lib cluster_tui
test result: ok. 159 passed; 0 failed
cargo test -p remuda-native --test cluster_tui
test result: ok. 4 passed; 0 failed
cargo test --workspace --all-targets --no-fail-fast: 2254 passed; 0 failed; 10 ignored
cargo clippy --workspace --all-targets -- -D warnings: Finished
cargo fmt --all --check: ok
```

Red checks: with the old rules put back (a key buffer timed from its last
key; the resize throttle keyed on session and size), two new tests fail for
the stated reason:

```text
failed_remote_resize_backs_off_even_when_its_notice_changes_the_size
  panicked: a new size must not bypass the backoff
steady_remote_typing_is_sent_while_keys_keep_arriving
  panicked: typing was held until it stopped
```

The first run of the full suite also caught a change in loop order: starting
due batches before sending let a local send to a stopped daemon block before
the "Input sending" frame was drawn (`native/tests/cluster_tui.rs`
`stopped_daemon_input_becomes_uncertain_and_later_input_recovers`). The loop
keeps its old order; instead the input wait ends at once when a remote batch
is waiting to be handed off.
