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
- The manifest records the canonical core session: `REMUDA_SESSION_ID` (the
  durable row id) inherited by the MCP child, never a prelaunch alias.
  `REMUDA_CLAUDE_SESSION` is an explicit standalone-test fallback only; a
  regression test pins that the core id wins and that the alias is not
  required.
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
- A long (>103-byte) binding path publishes the endpoint in a short owned 0700
  runtime socket directory while the capability and manifest stay in the
  binding directory. The helper accepts that exact recorded endpoint, and
  cleanup removes only the owned runtime directory.
- `_claude_notice` tests cover `binding_missing` versus `invalid_binding`, the
  exact manifest shape, stale bridge/socket, a shared or symlinked socket
  parent, malformed ACK, duplicate claim, and an end-to-end stdio + ingress
  delivery.
- `cargo test`, `cargo clippy`, `cargo fmt --check` and the repository comment,
  step and principle checks pass.

## Actual

```text
cargo test -p remuda-native --bin remuda claude_channel
test result: ok. 37 passed; 0 failed; 0 ignored; 0 measured; 155 filtered out; finished in 0.16s

cargo test -p remuda-native --bin remuda
test result: ok. 192 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.61s

cargo clippy -p remuda-native --bin remuda --tests
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.83s

cargo fmt --all -- --check
fmt exit: 0

python3 scripts/check-comments.py
ok — 1985 doc comment(s) within cap (item 3, module 20)

python3 scripts/check-steps.py
ok — 78 step(s), each with before, desired, expected and captured actual

python3 scripts/check-principles.py
ok — 14 principles, every named mechanism exists (32 CI jobs, 11 denied paths, 3363 test fns seen)
```

The authorized local-development TUI retry used an explicit disposable cwd.
It captured and answered the workspace-trust prompt only for that isolated
directory, then captured and answered the actual development-channel
confirmation (`1. I am using this for local development`) for this test only.
One actual `_claude_notice` returned `uncertain` and the same TUI rendered:

```text
← remuda-channel: Butler message tui01 arrived. Read it: MCP butler_inbox (or…
```

The account hold message appeared after the inbound; no model reply was waited
for. The observed `no MCP server configured` status line contradicted the live
manifest and inbound; its cause is not established. Correlation: session
`claude-tui-20261105` (instance id
`0000000000008a0318e5270869ddec80-0000000000000046`), bridge PID/start
`71425`/`1793882964198619`, recorded parent PID/start
`71181`/`1793882943659065`. After exit the stale helper returned exactly
`{"outcome":"definitely_not_written","reason":"bridge_gone"}`. The abrupt
group-kill left the binding and runtime directory behind; that cleanup leak is
an unresolved lifecycle gate, distinct from the proven graceful EOF cleanup.
No shared-root deletion or replay was performed. Full prompts and evidence are
in `docs/evidence/2026-11-05/claude-channel-smoke.md`. No production install,
reload, merge or rollout was performed.
