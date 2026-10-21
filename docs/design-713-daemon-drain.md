# Daemon drain and handover over a stable session host (#713)

Docs-only design note; nothing here is implemented. The owner chose option A:
cooperative drain and release, with no forced takeover in the host. Code
references are at main `4b4154ca`; `remote.rs` is in PR #757.
**Fact** means inspected source; **proposal** means intended behavior.

## What the host already gives us (facts)

- The physical controller lease is per connection. `Release` with an act in
  flight marks the lease draining; it is freed only when the whole act
  finishes ([`control.rs:76`](../native/src/session_host/control.rs),
  `finish_act` at :86). A dropped connection drains the same way.
- `Acquire` answers `controller_busy` while another connection holds the
  lease (`server.rs:278`). There is no preemption request.
- `Owner::claim` bumps the durable writer epoch
  ([`recovery.rs:30`](../native/src/session_host/recovery.rs)). The old
  daemon's next inventory commit fails with `StaleFence` before any RPC.
- `Owner::resume` (`recovery.rs:88`) adopts a pending act under the new fence,
  acquires the lease, and sends `Recover` with the original key. The host
  answers from its ledger. It never replays input.
- The adapter (`remote.rs`, PR #757) holds the lease only for one act or
  control call, so an idle daemon holds no lease at all.

## Drain command (proposal)

A daemon-side `drain` (an internal command, later used by upgrade) runs:

1. **Stop admitting.** New session operations get `Unavailable(Recovering)`.
   No new reservation is written.
2. **Finish in-flight acts.** Wait for each `HostSession` act lock to clear,
   with a bounded deadline (proposal: the act deadline, 30 s). An act that
   misses the deadline is left as a durable pending reservation. The next
   owner resolves it by key, so nothing is lost and nothing is replayed.
3. **Release leases.** Close every host connection. A connection that holds a
   lease sends `Release` first; the host drains any act still running.
4. **Exit.** Hosts keep running; children and PTYs are untouched. The drain
   never sends `Stop` to a host.

Drain is best effort. A crash at any step is the same as a SIGKILL mid-act,
which the PR #757 acceptance test covers.

## Handover order (proposal)

For the new daemon, per inventory:

1. **Writer claim.** Run `Owner::claim` once per process. From here on, the
   old daemon cannot write the inventory.
2. **Discover.** Read the inventory entries, then open each host with the
   expected identity and endpoint, as `HostSession::open` does.
3. **Acquire.** Take the lease. `controller_busy` is retried with backoff
   while the old daemon drains. It is not a failure.
4. **Recover.** For each pending act, call `resume` with the original key.
   Only after every pending act resolves does the session accept new input.
   Until then, new input gets `Indeterminate(request)`.

The new daemon must not wait for the old one to exit. The writer claim plus
the per-act lease are enough for safety. The drain only shortens the wait.

## Failure cases

| Case | Result |
| --- | --- |
| Old daemon SIGKILLed | Connections drop, the lease drains, and pending acts are recovered by key. |
| Old daemon alive but hung, connection open | Idle connections close after the host's 2 s deadline. A stuck act holds the lease until it finishes. |
| Old daemon alive and still sending acts | Its commits fail with `StaleFence`, so no new act starts. A request already admitted finishes first. |
| Old daemon alive and calling `resize` or `terminate` | Not fenced by the writer claim today: these take the lease directly. The drain must stop them; a later change can check the writer fence first. |
| Act misses the drain deadline | It stays pending; the new owner recovers it once. |

## Deferred: forced takeover

Forced takeover is out of scope. If needed later, it goes behind a new
`controller-preempt-v1` capability. A preempt request would carry a fence
newer than the holder's, and the host would cut the old connection at an
act boundary. Without that capability, the host behaves exactly as today.
This keeps the host a small, stable kernel: drain and handover policy live
in the daemon, not the host.

Also out of scope here: daemon replace/rollback and mixed versions, and
Windows (#214).
