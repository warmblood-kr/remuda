# 080 — Adopt an existing OpenCode server/TUI session

## Before

OpenCode members launch as `opencode --standalone <cwd>`; automatic notices go
through the PTY. An owner may already have a real TUI, loopback `opencode
serve` and fixed session A running before Remuda is involved. The owned
`_opencode_tui` wrapper covers only sessions it starts itself: it spawns the
server, creates a synthetic `ses_`+32-hex session, publishes an
`opencode-http-v1` manifest and owns teardown. There is no surface to point
native delivery at that pre-existing A, so adopting it would otherwise require
a restart or a PTY write. Standalone without an accessible endpoint is not
usable, and starting `serve`/ACP is not adoption.

## Desired outcome

- `remuda _opencode_adopt` reads one strict stdin request
  `{version,binding,nonce,session,endpoint,conversation,auth{scheme,user,password}}`.
  The credential appears only on stdin, never in argv, status or a journal.
- The endpoint must be exactly `http://127.0.0.1:PORT`. Adoption performs only
  credentialed `GET /api/info` and `GET /api/session/A` with a bounded request:
  the returned pid/version/urls must name the declared exact endpoint, the
  local process start for that pid must corroborate the generation, and
  `data.id` must equal A. Adoption never POSTs, creates, resumes, spawns or
  kills anything.
- On success it writes one `opencode-http-adopt-v1` manifest
  (`O_EXCL`, 0600 under a private 0700 parent) with schema, nonce, session,
  server pid/version/start generation, endpoint, fixed conversation and Basic
  auth. It has no wrapper pid, ancestry, instance or synthetic process claim.
  A preexisting binding is never overwritten. Write failure removes only the
  inode this call created.
- `_agent_notice` keeps the frozen Request1 shape and the owned
  `opencode-http-v1` schema unchanged. The manifest protocol selects one strict
  schema; the adopted parser also rejects unknown fields. For an adopted
  binding it rechecks nonce/session, the exact endpoint, the local server
  pid/start generation, then the same `/api/info` identity and exact GET of A,
  rechecks the generation at the last boundary immediately before one
  `delivery=queue,resume=true` POST. `accepted_queue` needs the exact
  receipt; ambiguity is `uncertain`; nothing is retried.
- Refusals are typed and happen before any POST: wrong auth, wrong node,
  missing/mismatched target, stale/recycled generation, invalid binding. The
  helper has no cleanup path for the adopted server, TUI or session.
- The default 2.2.5 session id is `ses_` + 12 lowercase hex + 14 base62
  (`^ses_[0-9a-f]{12}[0-9A-Za-z]{14}$`, 30 chars total); the owned wrapper's
  synthetic 32-hex id is also a real `Session.ID`. The adopted parser accepts
  the bounded URL-safe `ses_` subset and relies on exact credentialed GET
  equality for target proof, not a narrower invented upstream rule.

## Expected

- `cargo test -p remuda-native --bin remuda` covers request/manifest strictness,
  exact endpoint and conversation subsets, generation/staleness, no POST or
  retry during adoption, preexisting-binding refusal, partial-write cleanup,
  protocol discrimination without weakening either schema, adopted delivery
  receipt classification, default 2.2.5 and synthetic-32-hex fixtures.
- `cargo fmt --all -- --check`, `cargo clippy -p remuda-native --bin remuda --
  -D warnings`, and the focused repository checks stay clean.
- One bounded disposable original TUI/server started before adoption with a
  local mock proves: adopt then one queue accepted in fixed A, draft preserved,
  unchanged TUI/server pids, UI B not retargeted; bad auth, wrong target and
  stale generation publish/accept nothing; zero PTY writes.
- No production account, input, credential, install, merge or rollout.

## Actual

```text
$ cargo test -p remuda-native --bin remuda
running 174 tests
test result: ok. 174 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

$ cargo clippy -p remuda-native --bin remuda -- -D warnings
Finished

$ cargo fmt --all -- --check
FMT_CLEAN
```

Focused unit coverage added in this step:

- `opencode_adopt::tests`: exact IPv4 loopback endpoint (refuses `[::1]`,
  `localhost`, https, path, query, userinfo, port 0), bounded URL-safe session
  subset including the actual 2.2.5 fixture
  `ses_e54f46499ffeglY6MpLaInY8Tt`-shaped ids and synthetic 32-hex, process-start
  generation and staleness, strict request/manifest rejection, adoption with
  scripted HTTP, wrong auth/node/target publishing nothing, GET-only/no-retry,
  preexisting binding refused before any request, stale-between-adoption-GETs
  publishing nothing, unknown request/nested-auth fields refused by direct
  deserialization, failed write removing only its own partial inode, and no
  wrapper/instance fields.
- `agent_notice::tests`: protocol selects one strict schema; a tag-only edit
  cannot turn an owned manifest into an adopted one and extra wrapper fields
  are refused; adopted delivery reuses the exact target and receipt rules;
  adopted validation happens before any HTTP request, and an injected
  last-pre-POST fence refuses `server_gone`/`stale_binding` with zero POST while
  an unchanged generation still sends exactly one correlated POST.
- Owned-schema, canonical-notice, owned process-generation and delivery
  regressions remain in the same 174-test binary above.
