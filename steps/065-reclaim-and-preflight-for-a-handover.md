# 065 — reclaim and preflight for a handover

## Before

A daemon claimed its session-host inventory once, lazily, and its writer
fence never changed after that (`Owner.fence`). A reload needs three things
this did not give (gap G6 of the `remuda reload` plan, #713):

- The candidate daemon must check every host before it claims anything.
  Opening `Hosting` claims at once.
- On rollback the old daemon must take its sessions back. It could not claim
  again: one claim per process.
- After a claim the hosts must refuse the daemon just fenced. #771 fences
  writers at the host, but a host learns of a newer writer only when that
  writer's next act reaches it.

## Desired outcome

- `Owner::reclaim` commits a new claim (next epoch, fresh nonce) and
  replaces the owner's fence in place. Every handle sharing the owner writes
  under the new fence. Work under the old fence fails with `StaleFence`,
  never twice.
- `HostSession::announce_writer` sends `AcquireWriter` with the current
  writer and releases the lease at once. The host raises its writer
  high-water even when it answers `controller_busy`. Answers: `Fenced`,
  `Unfenced` (a host from before `writer-fence-v1`), `Superseded`,
  `Unreachable`.
- `Hosting::reclaim` claims again, then announces to every open host, and
  returns each answer.
- `Hosting::preflight` reads the inventory and Inspects every registered
  host, read-only: no claim, no lease. Each entry is `Running { build }`,
  `Settling` (reserved, exited or closing) or `Unreachable(reason)`.
- Nothing in the daemon calls these yet; the reload slice will.

## Expected

- `remote::a_reclaim_fences_a_newer_claimant_at_the_store_and_at_the_host`:
  an old daemon's `Hosting` drives a real host, then a candidate claims and
  the old one is refused. After `reclaim` the host answers `Fenced` and
  refuses the candidate's writer (`superseded_writer`) before the old
  daemon's next act reaches it. The old daemon acts again, the candidate is
  refused, and the host saw exactly `old-1, new-1, old-2`.
- `remote::a_preflight_reads_every_host_without_claiming_or_taking_the_lease`:
  the preflight reports the host's build. The fence is unchanged, and the
  current owner acts at once. Once the host is killed, it reports
  `Unreachable`.

## Actual

```
$ cargo test -p remuda-native --lib -- a_reclaim_fences a_preflight_reads
test result: ok. 2 passed; 0 failed

# reclaim without the announce:
panicked at native/src/session_host/remote_tests.rs:678:5:
assertion failed: matches!(raw.acquire(candidate.owner.writer()).unwrap(), Response::Error ...

# reclaim that commits a claim but keeps the old fence:
panicked at native/src/session_host/remote_tests.rs:669:5:
assertion `left == right` failed
```

Limits: an announce reaches only hosts this daemon has open. A host that is
unreachable at that moment learns of the new writer at its next act, as
before. A host older than `writer-fence-v1` is guarded only by the store
fence.
