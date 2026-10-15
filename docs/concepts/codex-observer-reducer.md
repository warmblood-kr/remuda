# Codex observer reducer: synthetic diagnostic increment

This module is not wired into _codex_tui, a daemon, a status file or Butler.
It opens no connection and starts no process. The existing MODEL/CTX producer,
launch configuration, approval routing and input admission remain unchanged.
There is no new CLI, attach endpoint, credential reader or approval handler.

Source evidence: codex-cli 0.162.1, locally generated with
`codex app-server generate-ts --out PATH`, inspected 2026-10-15. Stable thread
status and metadata APIs are documented at
https://learn.chatgpt.com/docs/app-server. The installed ServerNotification.ts
and v2/ItemGuardianApprovalReview{Started,Completed}Notification.ts explicitly
mark the auto-review payloads UNSTABLE. The fetched official guide does not
establish those event payloads, so support is version-specific and optional.

Observer accepts parsed JSON from a trusted future adapter and returns an
allowlisted request plan plus a serializable redacted diagnostic snapshot.
It authenticates no transport or file. Caller-supplied identity must already be
proven through the registered launch principal, exact Remuda instance and owned
server generation. SelectionEvidence additionally requires a trusted owning-TUI
selection contract and current connection generation; a loaded-list alone
never selects a thread. Such a production selection bridge is not implemented.

Request plans contain only initialize (experimentalApi=false), initialized,
thread/loaded/list (with pagination), and thread/read(includeTurns=false).
There is no thread/resume, subscription/approval-ownership registration,
approval response, turn/start, turn/steer or process/input operation.
Unknown/unsupported methods produce diagnostic uncertainty; absent review
events are unexercised, not proof of absence or lack of support.

Disconnect/reconnect removes selection and current state. Old connection
responses and superseded selection replies are ignored. Turn transitions invalidate older thread status and request reconciliation;
turn observation freshness is independent of status freshness. Late completion
of an older turn cannot invalidate a newer active turn. A state event racing
a metadata snapshot invalidates that snapshot and requires a later clean read.
A connection heartbeat cannot refresh state. Thirty-second producer-local
status/transport expiry is only a diagnostic threshold, never admission policy;
clock reversal is unknown. Future consumer authentication, private atomic file
publication and daemon-receipt expiry remain separate implementation gates.

Thread state, observed turn transitions and historical auto-review receipts are
separate. Child/other thread events do not change the selected primary. Review
keys include owned server generation, thread, turn and review ID. Overlapping
reviews and late completion of turn A cannot clear active turn B. Terminal
review receipts cannot regress to inProgress or complete a turn. Receipt IDs,
enums and receive sequence/generation are retained; actions, commands, rationale,
raw errors and warning bodies are omitted. Unknown notifications preserve
unrelated known fields. Auto-review status parsing is pinned to 0.162.1; future
versions retain method receipt evidence without interpreting unknown payloads.

Run `cargo test -p remuda-native --test codex_observer` and
`cargo clippy -p remuda-native --lib --test codex_observer -- -D warnings`.
Tests cover identity/selection, stale heartbeat, clock reversal, reconnect,
pagination, snapshot races, waiting flags, child exclusion, multiple roots,
overlapping reviews, redaction, unknown methods, unsupported methods and
synthetic UDS request allowlists through initialization/read errors.

The fake UDS test exchanges newline JSON to exercise the reducer's complete
request plan. It is not Codex's WebSocket-over-UDS transport, a live observer
receipt, nor a proof of production event subscription. No real server, model
workload, existing session attachment or production pane is exercised.
Bounded review-receipt retention and request retry budgets remain REQUIRED
integration gates; this synthetic-only reducer does not yet enforce those bounds.
Independent review is required before any future integration or live smoke.
