# 048 — bound typed terminal views and latest-state resync

## Before

The experimental host only exposes an ANSI repaint. It lacks typed styled
history viewports and a snapshot/watcher handoff. A slow connection owns its
snapshot allocation until its socket worker runs again; repeated captures can
monopolize the global allocation. Small synthetic parser geometries are padded
by the normal terminal-size constructor. Exhausted generation is checked after
the OS resize effect.

## Desired outcome

Expose an optional typed latest-state contract, with coherent cells/modes at G,
bounded encoding/pages/wakes, explicit newer-generation resync and independent
expiry/fair admission. Slow viewers must not block PTY drain or control progress.
Keep legacy delta sequencing unchanged and preserve standalone command behavior.

## Expected

Capture exact geometry, styles/wide continuations/wrap/cursor, effective relative
history offset/counts and mouse/paste/application/alternate-screen modes in one
parser cut. Encode outside the parser mutex with a 32MiB in-write cap. State the
pinned fixed-22-byte-cell rendering profile explicitly; excess combining text
can truncate, so this is not universal emulator-fidelity support or rollout.

Use existing 12KiB pages and full exact ID/kind/generation/pages/TTL ACK. Historical
G remains displayable while output advances; ACK returns latest generation and
an explicit resync flag, including EOF/degraded cases. One coalesced wake per
subscriber carries no raw bytes/deltas. Register before capture, sample before
sleep and clear pending before version sample. Bound waits to750ms and watch
admission to40/s/connection; reconnect requires another full snapshot.

One global snapshot allocation has 50ms global and250ms per-connection cooldowns,
16-entry FIFO admission with abandoned-turn expiry, and disconnect cleanup. The
server maintenance loop clears expired bytes/lease independently of blocked
socket I/O. TTL remains2s without page-read extension; maintenance checks every
listener turn (idle wait at most100ms, subject to scheduling). Page encoding is
bounded and outside parser locks. Legacy screen command retries only confirmed
read-only pre-capture busy responses in a bounded admission loop; transport or
mutation ambiguity is never retried by that helper.

Validate generation before OS resize under the same parser lock; uncertain OS
resize reports backend degradation. Test lost-wake window, exact/wrong/expired
ACK, missing capability, continuous output, stopped viewer, FIFO/expiry/allocation,
max escaped grid, explicit combining retention and unchanged legacy oracles.

## Actual

On reviewed base8b1669da, all105 host tests pass; four private helpers are ignored
outside their parent fixture. The eight standalone process tests (original seven plus controlled admission)
and ten relevant PTY regressions pass. Full core133 tests and two compile-fail doctests pass. Twenty-one
new host tests plus the controlled process-admission test cover the contract above. The initial standalone regression was a new
cooldown refusal reaching a legacy client without admission retry; the bounded
read-only helper fixed it without weakening process/snapshot assertions. Synthetic
command fixtures use raw bounded bytes, avoiding unrelated composer-submit waits.

```text
test result: ok. 105 passed; 0 failed; 4 ignored; 0 measured; 1103 filtered out; finished in 66.48s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.48s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 1201 filtered out; finished in 0.07s
synthetic frame G=128 bytes=250150 capture/pages/decode=101112 us parser-lock-wait-plus-copy=653 us copied-G=639
synthetic frame G=2018 bytes=250153 capture/pages/decode=345875 us parser-lock-wait-plus-copy=589 us copied-G=2532
synthetic frame G=3919 bytes=250153 capture/pages/decode=353752 us parser-lock-wait-plus-copy=714 us copied-G=4428
synthetic control resize during output: 177 us
cargo clippy --workspace --all-targets --offline -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.89s
```

Measurements are observations on this synthetic fixture, not hard scheduling or
cross-platform latency guarantees. Relative history offsets move as output
advances; history_total is not a stable row-ID promise. CPU/RSS aggregate-host
soak, full provider/Registry/Butler integration, real Windows host IPC and rolling
upgrade/rollout gates remain. Windows common-code native CI is still required.
No human attachment/controller authority changes, production adoption/restart,
live panes/agents or migration were used. Refs #713.

First ARM CI found an additional raw-RPC fixture assumption: its initial
snapshot required immediate admission despite the new global cooldown. The
fixture now retries only exact snapshot_budget_busy within a bounded admission
loop. Wrong identity/role, incomplete/wrong/duplicate ACK and physical lifetime
assertions stay intact; unrelated errors still fail with the full response.
A controlled two-viewer test proves the helper actually receives budget_busy
while another viewer holds its snapshot, then admits only after release. Runtime
source is unchanged from85737867. Fresh matching ARM/native CI is required after
this test-only correction.
