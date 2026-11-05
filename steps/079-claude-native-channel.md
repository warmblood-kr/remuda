# 079 — Opt-in Claude channel on a private stdio bridge

## Before

Claude members launch with the ordinary `remuda` coordination MCP entry only.
`native/src/mcp.rs` answers request/response frames and cannot push an
asynchronous channel notification, so there is no native pointer path for a
Claude session. No `claude/channel` capability, private ingress, strict
`claude-channel-v1` manifest or Claude notice helper exists. Ordinary MCP
clients, the PTY fallback and the completed guidance work must stay unchanged.

## Desired outcome

- `remuda _claude_channel` is an opt-in MCP stdio server. It negotiates the
  tested legacy `2025-06-18` era, waits for `notifications/initialized`, and
  publishes ingress only after the initialize response reached the writer.
- One stdin reader and one bounded serialized stdout writer. The binding is a
  0700 private directory with a 0600 socket, capability and leaves; the peer
  must be the same user. `bridge.json` is diagnostic only.
- The strict `claude-channel-v1` manifest has exactly the frozen fields; the
  capability secret lives only in its separate 0600 file.
- `remuda _claude_notice` reads the frozen Request1 document, validates the
  strict manifest and capability through `PrivateDir`, checks the socket
  fingerprint, bridge PID+start and connected peer, then sends the private
  ingress request and returns typed JSON.
- Any possible emission is `uncertain`/`channel_emitted_unconfirmed`. Only a
  bridge-proven zero notification byte is `definitely_not_written`. No retry,
  no PTY fallback, no tools, no receipt tool, no permission relay.
- The ordinary `remuda mcp` entry, configuration and PTY path are untouched.

## Expected

- Channel tests cover negotiation, malformed/repeated initialize, premature
  `initialized`, failed/zero-byte initialize response, bounded line reads,
  duplicate message IDs, large ordered ID sets without a count cap, claim
  dedup/release, duplicate startup preservation, replacement preservation,
  symlinked/replaced claims dir, claim fsync failure, and EOF return with a
  blocked writer or a full reply queue.
- `_claude_notice` tests cover `binding_missing` versus `invalid_binding`, the
  exact manifest shape, stale bridge/socket, malformed ACK, duplicate claim,
  and an end-to-end stdio + ingress delivery.
- `cargo test`, `cargo clippy`, `cargo fmt --check` and the repository comment,
  step and principle checks pass.

## Actual

```text
cargo test -p remuda-native --bin remuda claude_channel
test result: ok. 31 passed; 0 failed; 0 ignored; 0 measured; 155 filtered out; finished in 0.15s

cargo test -p remuda-native --bin remuda
test result: ok. 186 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.14s

cargo clippy -p remuda-native --bin remuda --tests
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.67s

cargo fmt --all -- --check
fmt exit: 0

python3 scripts/check-comments.py
ok — 1882 doc comment(s) within cap (item 3, module 20)

python3 scripts/check-steps.py
ok — 78 step(s), each with before, desired, expected and captured actual

python3 scripts/check-principles.py
ok — 14 principles, every named mechanism exists (32 CI jobs, 11 denied paths, 3245 test fns seen)
```

No real custom-Claude opt-in prompt or channel gate was encountered in this
slice; the account hold and the interactive development-channel confirmation
remain unverified and do not block this opt-in server/helper implementation.
No production install, reload, merge or rollout was performed.
