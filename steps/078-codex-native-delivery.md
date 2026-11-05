# 078 — Codex native notices: helper and explicit adoption

## Before

Codex members deliver Butler notices by writing into the PTY. Installed
`codex-cli 0.166.0` exposes experimental `thread/read`, `thread/loaded/list`
and `thread/queue/add`, but the earlier spike used a raw RPC client, not a
product helper. There is no adoption surface, no pinned peer/socket identity,
and no correlated admission receipt. `thread/queue/start` and `--resume` are
not used.

## Desired outcome

- `remuda _codex_adopt` reads one strict stdin request
  `{version,binding,nonce,session,socket,thread}`; no endpoint in argv. It
  connects, checks the declared path fingerprint before and after connect plus
  the connected-FD peer uid/pid/process-start, runs a non-mutating preflight
  (`initialize` + exact `0.166.0` + root-user `thread/read` +
  `thread/loaded/list`), and only then publishes `pin.json` and a strict
  `codex-app-server-v1` manifest (O_EXCL, 0600, 0700 parent).
- `remuda _codex_notice` reads the frozen Request1 stdin shape
  (`version,binding,nonce,session,instance,request_id,text,message_ids`),
  loads that manifest and pin, revalidates the pin, repeats the same
  preflight, and sends exactly one add-only `thread/queue/add` with explicit
  `{type:"text",text,"text_elements":[]}`.
- `accepted_queue` needs the correlated receipt: matching RPC id, non-empty
  submission id, echoed client id, and exactly one text input with
  `text_elements=[]`. Pre-add failure is `definitely_not_written`; any
  post-attempt error, mixed result+error, wrong id or EOF is `uncertain`. No
  retry, no queue/start, no resume, no PTY fallback.
- A genuinely absent publication leaf under a valid private parent is
  `definitely_not_written/binding_missing`; a missing/invalid parent,
  malformed, wrong-owner or dangling leaf stays `invalid_binding`.
- `remuda _codex_tui --binding FILE` (optional) starts the owning raw UDS
  tunnel: the TUI connects downstream, bytes are forwarded unchanged, and only
  the first `thread/start` request plus its single exact-id response are
  inspected. The captured root thread is published through the same adoption
  preflight. The nonce comes from `REMUDA_BUTLER_BINDING_NONCE` and the session
  is the core-assigned `REMUDA_SESSION_ID` of the actual session row (no alias
  variable); later `/new`
  requests do not retarget A. Capture failure keeps forwarding but publishes
  nothing.
- Only macOS is advertised until Linux is measured.

## Expected

- `codex_native` tests: captured 0.166 fixtures, exact version parsing,
  root-user read shape, UUID-string loaded list with cursor, single-input
  receipt, wrong-id/mixed-envelope/EOF transport (each exactly one add), and
  adoption preflight/pin/publication.
- `agent_notice` regression: `binding_missing` only for a validated private
  parent plus `openat` ENOENT.
- `cargo test -p remuda-native --bin remuda`, clippy `-D warnings`, and the
  repository checks stay clean.
- Real 0.166 smoke: adoption against a live app-server with a non-live UUID
  rejects at preflight and publishes nothing; a notice with no binding is
  `binding_missing`.
- Positive private mock/TUI smoke: a real TUI thread and draft, product adopt
  then notice, `accepted_queue`, MOCK_ACK rendered, draft preserved, one turn.

## Actual

```text
$ cargo test -q -p remuda-native --bin remuda
test result: ok. 188 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.54s

$ cargo clippy -q -p remuda-native --bin remuda -- -D warnings
(no output)

$ python3 scripts/check-comments.py
ok — 1884 doc comment(s) within cap (item 3, module 20)
```

Real installed `codex-cli 0.166.0`, fresh private app-server, declared socket,
non-live UUID, pinned product binary
`sha256 a009ed08e923a51e5c390f78a68e7d94e0223852f89865b9f3790838d68162fe`:

```text
socket up: /private/tmp/codex-daemon-501/845b54489ab978cc6bb8cdbd8aa05ccb086cd63b7b2984b2ef02d0db39b6da1e
ADOPT: {"outcome":"unavailable","reason":"thread_mismatch"}
FILES:
NOTICE: {"outcome":"definitely_not_written","reason":"binding_missing"}
```

The adopt preflight ran initialize and the exact-version check, rejected the
UUID at `thread/read`, and published no pin or manifest. The notice had no
binding, so it never contacted the endpoint.

Positive product smoke on the private mock/TUI harness (fresh TUI thread, real
pty draft, product helper, mocked provider):

```text
target={"draft": "DRAFT_SENTINEL", "server_pid": 34722, "socket": "<spike>/upstream.sock", "thread": "01a1aba5-5f21-7eb2-84e3-495ee93d1e8b", "tui_pid": 34732}
argv=remuda _codex_adopt (stdin only); remuda _codex_notice (stdin only)
adopt={"generation":"ea5e6b5c196c8456a0e9cb4573fce90b5ecf460a00139cc20975fd3e642a58c7","outcome":"adopted"}
notice={"outcome":"accepted_queue"}
HELPER_RESULT {"adopt": {"generation": "ea5e6b5c196c8456a0e9cb4573fce90b5ecf460a00139cc20975fd3e642a58c7", "outcome": "adopted"}, "notice": {"outcome": "accepted_queue"}}
QUEUE_ACK_IN_TUI True
DRAFT_PRESERVED True
TURN_COMPLETIONS 1
EVENTS {"helper_accepted": true, "queue_start": false, "ack": true, "busy": false}
helper_binding_files=manifest.json pin.json
binding_binding=codex-app-server-v1
provider_posts=2
```

The pin recorded `peer_uid` 501 (equal to euid), `peer_pid` 34722 (the
app-server), `peer_start_us` 1793881103818294, and socket dev/ino
16777234/47554439. The original TUI rendered MOCK_ACK with the draft intact and
no queue/start. Provider POSTs were 2, the same as the earlier add-only
measurement, so this is still not exactly-once. The add count was not
packet-sniffed because the helper connects directly to the upstream; the helper
has exactly one add call site, no retry path, and one turn completed.

A second bounded run repeated `adopted` + `accepted_queue` (generation
`50cc8135...`) and then SIGKILLed the pty TUI: the spawned app-server stayed
alive (`SERVER_AFTER_TUI_EXIT alive`). TUI exit alone does not stop the server;
the launcher must own stopping its spawned app-server/tunnel. The helper kills
nothing. Artifacts: `spike-codex-tunnel/evidence/2026-11-05/`.

Owning tunnel on the real launcher (`--binding <manifest file>`), pinned binary
`sha256 906d957c...`:

```text
AUTO_BINDING True
NOTICE {"outcome": "accepted_queue"}
QUEUE_ACK_IN_TUI True
DRAFT_PRESERVED True
EXIT_MODE normal
LAUNCHER_SOCKET_GONE True
BINDING_FILES_GONE True
APP_SERVER_GONE True
PROVIDER_POSTS 2
```

The binding was published from the captured first `thread/start` response with
no external adopt. The TUI uses string request ids (`startup-thread-start-...`);
a second startup request (`temporary-structured-...`) was observed and ignored,
so only the first startup thread was bound. Normal exit removed the downstream
socket, binding files and app-server. Adversarial reconnect/ID-reuse and Linux
are not measured.

Tunnel hardening after independent review: both the app-server and downstream
sockets live in a fresh `/tmp/remuda-codex-<pid>-<token>/` owner-0700 runtime
directory, independent of TMPDIR; downstream is chmod 0600, is never removed
before bind and is removed only by its created inode; the request id is registered before the
request bytes reach the app-server; raw chunk forwarding continues when the
bounded observer rejects a frame; EOF before publication latches unavailable;
request ids must be strings; method-bearing or mixed result/error envelopes are
not responses. A header, frame, UTF-8, RSV or oversize failure in either
observer now latches the shared authority as failed while raw forwarding
continues, so an id seen before the failure cannot publish from the other
direction. Cleanup sets the closing fence first, waits briefly for an adopt
already past the fence, removes the binding and the 0600 downstream socket,
and only then stops the app-server. Regressions cover second bootstrap request,
non-string id, error/non-root response, method-bearing matching id, header
overflow, forwarding through a parse failure, and request-id-then-corrupt-frame
followed by a valid matching response publishing nothing.

Kernel peer fixture for a direct spawn: connected peer PID/start equals the
spawned child, uid equals euid, resolved socket is 0600. Two spawns with the
same advertised path produced the same target hash with a new inode and start;
the hash is the advertised path, not HOME/config.

Remaining limits: owning-launch lifecycle/cleanup, Butler durable no-replay
across reload/restart, Linux, and packet-level add accounting are not claimed.
The positive smoke used a mocked provider and a private harness, not a
production install.
