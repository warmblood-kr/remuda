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
running 176 tests
test result: ok. 176 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

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
  regressions remain in the same 176-test binary above.

Bounded disposable runtime probe (real installed OpenCode 2.2.5; original
server and original TUI started before adoption; private loopback mock; no
production account, input or credential; private root removed and all owned
pids confirmed gone):

```text
source HEAD / binary:          53dbc89a9d71 / sha256 db5c06347290354a27074baa430643203ea32dc883db84d0b57dbbfa9f34a2fc
server/tui pid+start:          76589 / 76591, both Thu Nov  5 21:57:46 2026
session A (default generator): ses_e54369466ffeyZWgTIpc4FZR0b
_opencode_adopt positive:      {"outcome":"adopted","generation":"65f870ac8f9d2"}
manifest mode:                 0600
first notice:                  {"outcome":"accepted_queue"}
second notice:                 {"outcome":"accepted_queue"}
stale-generation notice:       {"outcome":"definitely_not_written","reason":"stale_binding"}
bad-auth adoption:             {"outcome":"unavailable","reason":"auth"}
wrong-target adoption:         {"outcome":"unavailable","reason":"target_missing"}
30/30 checks passed
```

The probe confirmed: TUI/server pids and start times unchanged across adoption
and both notices; both notices rendered in the original TUI and landed in fixed
A; an unrelated session B created on the same server after adoption received
neither notice (fixed-target evidence, not a UI-navigation proof); a typed
draft was still in the composer and was submitted exactly once only by the
probe's later explicit Enter, not by adoption/notice; bad auth, wrong target
and stale generation published no manifest and produced zero model POSTs and
zero renders. The helper never held the TUI pty: all pty writes were the probe's
draft and explicit Enter. Credentials stayed out of helper stdout/stderr.

## Durability and identity limits

- Publication is `O_EXCL` 0600 create plus write/fsync of the file, matching
  the owned wrapper. It is not a rename-based atomic full-file publish and does
  not fsync the parent directory. A concurrent reader can observe a transient
  partial leaf, which the strict parser refuses as `invalid_binding`; crash
  durability of the directory entry is not claimed.
- Explicit owner declaration authorizes the endpoint/A association. Local
  pid/start is corroboration, not kernel TCP peer identity, remote PID proof or
  historical TUI identity. Fresh Butler row.id/opaque instance/nonce owns
  managed fencing. Same-UID races remain out of scope for this helper.

Rebase note: after #892 merged to main, `389bffc2..8f1f0293` and
`origin/main..HEAD` are patch-identical (`git range-diff` reports `=` for all
seven commits), so the preserved probe binary corresponds to the same code even
though its recorded pre-rebase commit hash differs.
