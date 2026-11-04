# Optional session enrichment

The cluster tree and whole-node peer list use the frontend composition contract
from PR #884 (`design-cluster-ui.md`). Butler runs in a separate package and
image. Its hierarchy and telemetry were never part of `Request::List`; a frontend
cannot obtain remote Butler facts from its own node's roster or session names.

## Publication and transport

Trusted daemon-image Lua may call
`remuda.session.set_display(name, instance_id, facts_or_nil)`. It returns `true`
or raises without changing the previous publication. This is an in-process
binding, not a wire operation or a new remote capability. Existing remote Eval
restrictions and List authentication/authorization remain unchanged. Local Lua
already has daemon authority; a provider name is informational, not verified
identity or authorization.

The version-one typed object supports `provider="butler"`, optional `alias`,
`leader`, `kind`, `model`, `context_used`, `context_window`, and integer
`context_percent` (0–100). Optional `member_id`/`leader_id` are accepted but Butler
does not publish them and frontend Lua never sees them. Unknown fields, invalid
types, controls/bidi, strings over 128 UTF-8 bytes, inconsistent usage and objects
over the conservative 1 KiB encoded bound are rejected. No task, prompt, mail,
cwd, sandbox, command, endpoint, telemetry path or capability is published.

Publication requires the exact nonempty current instance, live and available,
including hosted sessions. It replaces the whole object; nil clears it. The
registry stores at most 4,096 objects. A daemon-owned injected monotonic clock
gives each publication ten seconds. List copies matching unexpired facts only. The metadata cache join invokes no
provider, Lua, file or network read; existing base hosted inspection is unchanged. Removal/replacement and image
generation disposal invalidate publications. An older image cannot publish or
clear a newer image's data; shutdown disposal runs even when stop hooks fail.

`SessionSummary.display` is optional and serde-defaulted. Old peers omit/ignore
it; unsupported or semantically malformed metadata is ignored while retaining
the base session. Invalid JSON and the existing outer protocol limits still
apply. An oversized v1/v2 List first sheds all enrichment, preserving every base
session and its order/identity. If the base itself exceeds the existing limit,
the existing explicit error remains; frame limits are not increased.

## Node and freshness semantics

Local rows consume the local daemon's publication. Remote rows consume only the
authenticated owning node's List. Joins use original wire name plus exact
instance inside that node; aliases and sanitized names never join metadata or
route actions. No controller-local Butler lookup or remote Eval fallback exists.

The wire contains remaining `valid_for_ms` (at most 10,000). Remote and peer
viewers anchor expiry to request start, conservatively counting transit and
selected-screen polling. Sync, cached snapshots and failed polls never renew
metadata. Local List subtracts elapsed RPC time before subsequent aging. The
normal bounded redraw tick removes expired enrichment even without input or a
new network generation. Missing/plugin-disabled/expired fields are omitted,
never inferred as zero or proof that a session is unmanaged. Node reachability
and PTY live/ended/pending status retain their independent canonical meanings.

## Butler opt-in and actual limits

The companion remuda-butler change is required on each publishing node. Set
`remuda._butler_publish_session_display = true` in that node's trusted config
before Butler loads. The default is off: enabling shares these fields with every
peer already authorized to List that node. No live config is changed by this PR.
Publication runs every two seconds. Disabling clears on the next tick; remote
copies may remain until their original ten-second deadline.

Butler publishes only proven instance-bound agents and a leader alias whose
durable parent relationship still matches. Configured model is a launch fact.
Telemetry model/usage require a payload-bound observation record from the owning
Claude statusline or Codex observer. Claude records also bind the exact session instance;
Codex uses a fresh path per launch. Old adapters/files without that record omit
those fields. Samples younger than 60 seconds are eligible; publication TTL can
extend visibility by up to ten seconds. Wall time is anchored once to monotonic
elapsed time while the publisher lives, preventing clock rollback/replay revival.
A new image establishes a new wall origin; it cannot prove freshness across a
wall-clock rollback spanning restart. Uncertain/future samples are omitted.

## Frontend extension and scrolling

Restricted frontend Lua receives only optional sanitized `session.display`
facts, alongside the existing opaque tokens. Override
`decorate_session(session, spans)` in `cluster-ui.lua` to change the default
annotations; or override `compose(model)` for layout policy. Existing VM budgets,
atomic validation, canonical status slots and fallback apply to both.

Rust windows validated rendered rows around the selected target's first row,
including reordered/multirow blocks, resize, filtering and emergency rendering.
A one-row leaf paints the selected row instead of an overflow marker. Peer
scroll/focus behavior and private action/preview routes remain unchanged.
