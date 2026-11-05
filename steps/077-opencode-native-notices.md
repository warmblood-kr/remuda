# 077 — OpenCode native notices on a fixed launch-owned session

## Before

OpenCode members launch as `opencode --standalone <cwd>`; Butler delivers
automatic notices by writing into the PTY. `remuda.ls()` Lua rows expose
`instance_id` but not the existing `SessionSummary.id`. A private-server probe
showed the fixed-session API, Basic auth and `delivery=queue` prompt surface.
Codex pre-create + remote TUI resume failed on 0.166 and stays unbound.

## Desired outcome

- `remuda _opencode_tui --binding PATH [--status PATH] [--cwd DIR]` starts an owned loopback `opencode serve`, reads its URL and
  password, creates one fixed `ses...` conversation before the TUI attaches,
  then runs the original TUI with `--server URL --session ID`.
- The launcher writes a private 0600 manifest: schema/version, parent nonce,
  `REMUDA_SESSION_ID`, wrapper/server pid/generation, endpoint, fixed
  conversation, Basic auth reference. It never asserts the opaque instance id.
  The manifest is removed and only owned children stopped on exit/failure;
  a parent watcher stops the server group. No `codex_tui.rs` change.
- `remuda _agent_notice` reads a frozen JSON request from stdin (binding path,
  expected nonce/session/instance, request id, text, message ids), validates
  private ownership/no symlink, nonce/session/target, generated-notice grammar
  and ids, then POSTs `delivery=queue` with `resume=true` to the fixed session.
  It returns bounded typed JSON (`accepted_queue`, `definitely_not_written`,
  `uncertain`) and never prints endpoint, password or arbitrary text.
- `remuda.ls()` Lua rows gain the existing `SessionSummary.id`; `v13.lua` and
  the `api_versions.rs` table cover it. Frozen fixtures, including v5, are
  unchanged. `v12` remains reserved for PR889.
- Native capability is Unix-only: `--capability` reports
  `native_notice:false` elsewhere, the wrapper and helper refuse before any
  spawn or write (`unsupported_platform`), and a non-Unix `server_alive` is
  never true. Butler keeps an unbound PTY there.
- Unit tests cover argv/auth header, manifest schema/ownership/nonce/target,
  notice grammar, status classification and the Lua id projection.
  No production install/reload, no merge, no Codex adapter.

## Expected

- `cargo test -p remuda-native --lib opencode_tui` and
  `... --lib agent_notice`: new unit tests pass.
- `cargo test -p remuda-native --test api_versions`: v1-v11 unchanged plus
  v13 id fixture pass.
- A disposable launcher probe (no real model turn) shows the manifest,
  fixed session id, TUI argv and cleanup.
- Red checks: a non-matching nonce/session, a symlinked or non-0600 manifest,
  a stale pid/target, an invalid notice id, and a 409/timeout classification.

## Actual

```text
cargo test -p remuda-native --bin remuda
  145 passed; 0 failed
cargo test -p remuda-native --test api_versions
  1 passed; 0 failed (22.8s; v1-v11 frozen, v13 ok)
cargo clippy -p remuda-native --bin remuda -- -D warnings
  Finished
cargo fmt --all -- --check
  clean
```

Review fixes folded in after QA/lead read the first checkpoint:

- Auth: OpenCode resolves `OPENCODE_PASSWORD` before
  `OPENCODE_SERVER_PASSWORD`, so both server and TUI children get the
  generated value under BOTH names, overriding an inherited primary.
  A unit test seeds a conflicting inherited `OPENCODE_PASSWORD` and asserts
  the child sees the generated value.
- Startup fence: the parent watcher starts immediately after the owned server
  spawn, before URL parsing, `/api/info`, session creation or manifest
  publication. This narrows the uncovered interval; it does not close it. The wrapper blocks on the watcher's `ready` line, and the
  watcher self-checks `getppid` before acknowledging and kills the owned group
  when the wrapper is already gone.
- Identity: `generation` is no longer random. It is the log-encoded start
  time pair of the live wrapper and server (`wrapper_start-server_start`,
  microsecond precision). `_agent_notice` requires the wrapper to be live, the
  server's parent to be that wrapper, and the generation to match both live
  start times. Dead wrapper, orphaned server, and a recycled pid are typed
  refusals.
- Conversation: `ses_` plus exactly 32 lowercase hex characters, checked
  before it is interpolated into a URL path.
- `v13.lua` uses `remuda.new(name)` (portable default shell) and closes its
  own session; it no longer depends on `sh` (Windows job may lack it).

Manifest trust boundary (`native/src/bin/remuda/private_dir.rs`):

- The binding parent is opened once and pinned: canonicalized, every ancestor
  must be root- or self-owned and not group/world writable (sticky excepted),
  and the directory itself must be owned by this user with no group/world
  bits. All later work uses that descriptor, not the path.
- Create is `openat(O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC, 0600)`, so a
  preexisting file is never adopted, truncated or removed. Read and removal
  use `openat(O_NOFOLLOW|O_NONBLOCK)` plus `fstat` on the opened descriptor:
  a symlink leaf or FIFO cannot redirect or block, and the checks cannot be
  raced by swapping the name afterwards.
- Cleanup is identity-based: the created `(dev, ino)` is recorded and the name
  is unlinked only while it still refers to that inode. A replacement file is
  left alone, and a truncated manifest is still removable because removal no
  longer needs valid JSON or the nonce.
- Write/sync failure removes exactly the inode created by that call and
  returns an error; the wrapper publishes nothing and no partial manifest
  survives.
- `_agent_notice` reads the manifest through the same pinned parent and the
  same nofollow/nonblock leaf open.
- Regressions: 0755 parent refused, ancestor symlink resolved then pinned,
  symlink leaf refused, FIFO leaf does not block, preexisting file untouched,
  replacement inode not deleted, failed write leaves no file, created file
  0600 and removable.

Real-wrapper probes (real OpenCode 2.2.5 server, private HOME/config/data,
`opencode --server ... --session ...` child replaced by a sleep shim; the
TUI invocation is NOT claimed as a real TUI run):

- Conflicting inherited `OPENCODE_PASSWORD` + `OPENCODE_SERVER_PASSWORD`:
  manifest written 0600, generation like `65f84cc409ac0-65f84cc4f7bc2`,
  conversation 36 chars; `_agent_notice` -> `{"outcome":"accepted_queue"}`;
  wrong nonce -> `{"outcome":"definitely_not_written","reason":"invalid_binding"}`;
  normal TUI-child exit removed the manifest and stopped the server.
- `SIGKILL` wrapper after publication: watcher stopped the server within 1s;
  stale 0600 manifest stayed; `_agent_notice` -> `definitely_not_written`
  `wrapper_gone`. Nothing was posted.
- `SIGKILL` wrapper during startup (server child observed, manifest not yet
  written): server died within 1s and no manifest appeared.

Real TUI probe (no shim, private HOME/config/data, allocated pty): the wrapper
started the real `opencode --server http://127.0.0.1:59745 --session
ses_afb8a7091e3fda10f38a4be97526a63c <cwd>` attached to the fixed session,
`_agent_notice` returned `{"outcome":"accepted_queue"}`, and the rendered TUI
screen contained `Butler message 01ABC arrived. Read it: MCP butler_inbox (or
remuda butler`. Terminating the TUI made the wrapper exit on its own (code
130) and remove the manifest; no watcher or owned server survived.

Independent QA (read-only, own real original wrapper/server, same generation
throughout): idle and busy queue drafts pass; PTY selection sentinel saved only
in B, then a real helper notice gave A1/B0, wrong nonce gave
`invalid_binding`, and there were zero PTY writes during native delivery or
refusal; startup failure, watched startup death, published death and stale
`wrapper_gone` pass; teardown exited 130 with manifest and server gone. QA
records no functional blocker for the planned matrix, with the pre-watcher
death window and same-UID races explicitly documented rather than claimed
zero-window.

Probes after the pinned-directory change: 0755 binding parent exits 1 before
spawning and writes nothing; a 0600 manifest is created, notice
`accepted_queue`, symlink leaf `invalid_binding`, clean exit removes manifest
and server; post-publication SIGKILL leaves a stale manifest that the helper
refuses (`wrapper_gone`) with no POST; startup SIGKILL leaves no manifest and
no server.

Remaining limits: the server is spawned before the watcher. A wrapper death
after that spawn but before the watcher process exists is UNCOVERED — nothing
is left to kill the server group (milliseconds, and macOS has no PDEATHSIG
equivalent). The watcher's first parent check covers only a death after it has
started and before it acknowledges readiness, not the earlier window. Native
capability is advertised only on macOS and Linux; other Unix and Windows keep
the unbound PTY. Windows ACL/process fencing/cleanup is not implemented and not
cross-checked (only the `x86_64-pc-windows-gnu` target is installed and the
mingw C toolchain is absent, so `cargo check --target` stops in `cc-rs`). No
production install/reload/merge, no `codex_tui.rs` change, no Butler edit.
