# Remuda API vocabulary proposal

## Summary

Remuda already has the right atoms: protocol `Request` variants, Rust-bound Lua operations, pure Lua tools, and Butler workflows. Their names currently sit together on `remuda`, while reusable mod interfaces are often hidden behind `_`. Make the vocabulary visible in layers: atomic session operations under `remuda.session`, higher-level terminal interactions under `remuda.input` and `remuda.screen`, reusable framework words under `remuda.hook`, `remuda.schedule`, `remuda.process`, `remuda.module`, and `remuda.tool`, and Butler's supported interface under `remuda.butler`. Keep implementation state and dispatch machinery private. For each promotion, add the new name first, retain the old spelling as a deprecated alias with a notice, then remove only in a later breaking API version.

Owner approval #206 fixes the first implementation scope: the `remuda.session` table remains callable through `__call`, its established nested words join `list/new/close/attach`, and only the named Butler vocabulary in the approved list below becomes public this round. The resize operation is added as another `remuda.session` primitive, with bounded dimensions.

## Inventory and classification

**Primitive** means a small operation or data word with no higher policy bundled into it. **Composite** means it composes primitive/composite words into a reusable behavior. **Internal mechanism** means runtime state, dispatch, lifecycle, test seam, or implementation helper; it is not a supported caller vocabulary. Existing functions are classified as they behave today, before any promotion.

### Core Lua surface

The inventory below covers every name in `native/src/script.rs` `BINDINGS` (79 names), including the names added by `native/src/tools.lua`. Purely local Lua functions are implementation details and are outside the public `remuda.*` surface. The public table and registry should eventually be derived from these classifications.

| Current name(s) | Class | What it does / proposal |
|---|---|---|
| `ls`, `new`, `close` | Primitive | List, create, and end sessions. Promote to `remuda.session.list/new/close`; keep `ls` as a short deprecated alias if the owner values shell-like convenience. |
| `send`, `insert`, `key`, `click`, `feed` | Primitive | Deliver line, bytes, key, pointer, or timed input steps. Promote to `remuda.input.line/insert/key/click/feed`; `send` remains a deprecated alias for `input.line`. |
| `caller` | Primitive/query | Return `{kind = "session"|"outside"|"unknown", session = name?}` from the daemon's socket peer PID and managed-session ancestry. `session` means positive ancestry evidence; `outside` is unauthenticated and does not imply operator identity; missing, unreadable, or exited peers are `unknown`. This is ADVISORY only: same-UID Lua callers can invoke `remuda -e` or otherwise spoof Lua state, and Windows parent PIDs have the same limitation. Butler policy must not treat this as an authentication boundary. |
| `capture`, `capture_styled` | Primitive | Read plain or styled terminal content. Promote to `remuda.screen.capture/styled`; styled capture includes cursor and style runs. |
| `attach` | Primitive | Give a human the terminal connection. Promote to `remuda.session.attach`. |
| `list_dir`, `mkdir`, `remove_dir_all`, `fs.write_atomic`, `fs.mkdir_new` | Primitive | List, create, remove filesystem directories, and atomically replace a file. `fs.mkdir_new` creates one directory only when the target does not exist and does not create parents. Promote to `remuda.fs.list_dir/mkdir/mkdir_new/remove_tree/write_atomic`; retain `remove_dir_all` alias to preserve its explicit destructive meaning. |
| `sleep`, `fail` | Primitive | Pause the Lua image or deliberately fail its caller. Promote to `remuda.runtime.sleep/fail`. |
| `exec`, `reload` | Composite | Load or lifecycle-reload an installed mod. Promote to `remuda.module.exec/reload`. |
| `input.text`, `input.submit` | Primitive | Deliver one text burst (bracketed paste when mode 2004 is enabled) and submit visible composer text with a separate Return and one bounded retry. |
| `type_text` | Composite | Type text, wait for the minimum settle pause, and submit it through `input.text` and `input.submit`. |
| `expect`, `expect_option` | Composite | Observe a screen, select and perform a matching branch/action; `expect_option` selects from a screen and match set. Promote under `remuda.screen.expect` and `remuda.screen.expect_option`. |
| `buffer`, `buffers` | Existing namespace + registry | `remuda.buffer` is already a namespace table with `new`, `set`, and `list`; `remuda.buffers` is its registry. Preserve these nested words and use them as the in-repo precedent for the proposed namespace tables. |
| `window`, `windows` | Existing namespace + registry | `remuda.window` is already a namespace table with `current`; `remuda.windows` is its registry. Preserve these nested words and use them as the in-repo precedent for the proposed namespace tables. |
| `session` (Lua helper) | Composite | `remuda.session(name)` returns a handle by name, with `buffer` and `is_busy` properties. It does not return Butler session detail; `session_detail` is a separate Butler hook. The approved `remuda.session` namespace is a callable table (`__call` preserves `remuda.session(name)`) with nested `list`, `new`, `close`, `attach`, `resize`, and the existing session-handle words. |
| `schedule`, `cancel`, `schedules` | Composite + registry | Register repeating work, cancel a handle, and inspect schedules. Promote to `remuda.schedule.every/cancel/registry`; `schedule` remains an alias during migration. |
| `tool`, `tools` | Composite + registry | Register an MCP tool word and inspect tool definitions. Promote to `remuda.tool.define/registry`. |
| `on`, `emit`, `emit_until_success`, `emit_until_failure`, `emit_filter` | Composite | Register event handlers and dispatch/broadcast/filter values. Promote to `remuda.hook.on/emit/emit_until_success/emit_until_failure/emit_filter`. |
| `hook_list`, `hooks`, `clear_hooks`, `event_counts` | Composite, registry, or diagnostics | Inspect handlers, legacy hook table, clear registrations, and report counts. Promote list/clear/counts into `remuda.hook`; keep `hooks` as a deprecated read-only registry alias (already deprecated for reading). |
| `advise`, `unadvise`, `advice_member`, `advice_list` | Composite | Wrap/unwrap a function at a named path and inspect advice. Promote to `remuda.advice.add/remove/member/list`. |
| `contribute`, `contributions` | Composite + registry | Add and read named entries at extension points. Promote to `remuda.extension.contribute/contributions`. |
| `extension_command` | Composite | Register a command exposed by a mod. Promote to `remuda.extension.command`. |
| `load_extension_command` | Composite | `exec` + the command table: load the installed mod that owns a command unless its handler is registered. The CLI calls it before dispatch so a first mod subcommand works (#394). |
| `process`, `processes`, `kill` | Composite + query | Start a managed child process, list running process ids, and terminate one. Promote to `remuda.process.start/list/kill`. The low-level `_process_spawn` binding exists but is internal; the public composite validates the spec and wraps it. |
| `request_counts`, `schedule_fires`, `schedule_skips` | Diagnostic composites | Read daemon request counts, schedule firing counts, and ticker skips. Promote to `remuda.diagnostics.request_counts/schedule_fires/schedule_skips`; document these as diagnostics, not control primitives. |

The remaining currently bound names are internal mechanisms. Retain under `_` or move to private closure/registry state; do not promote merely because they are callable today.

| Current name(s) | Class | What it does |
|---|---|---|
| `_registry`, `_registry_dump` | Internal mechanism | Holds metadata for bound and Lua-defined words; `_registry_dump` formats it for inspection. A supported read-only `remuda.runtime.registry` may later replace the dump, but the mutable table must stay private. |
| `_call`, `_descriptors` | Internal mechanism | MCP dispatch and descriptor construction over registered tools. |
| `_dispatch_extension_command`, `_extension_commands` | Internal mechanism | Extension command dispatch table and dispatcher. |
| `_advice_reattach`, `_function_source` | Internal mechanism | Advice restoration after reload and source lookup for hook diagnostics. |
| `_event_counts`, `_schedule_fire_counts` | Internal mechanism | Mutable counters backing public diagnostic readers. |
| `_run_due_schedules` | Internal mechanism | Ticker callback that runs due schedules. |
| `_refresh_sessions_buffer`, `_sync_window_shown` | Internal mechanism | Refreshes the built-in sessions buffer and keeps window selection synchronized. |
| `_mod_launch_options` | Internal mechanism (injected by CLI) | `native/src/bin/remuda.rs` injects launch options and Butler reads them during startup. Keep private as a handoff slot, not a public Lua word. |
| `_process_spawn`, `_process_killpg` | Internal mechanism | Low-level process launch and daemon shutdown reaping. `_process_killpg` is platform-specific. |
| `_activate_module`, `_module_set_field`, `_lifecycle_start_active` | Internal mechanism | Module activation, namespace field ownership, and lifecycle startup bookkeeping. `_activate_module` and `_module_set_field` are removed from `remuda` after transfer into the Lua named registry. |

### Butler mod Lua surface

Source: every `.lua` under `remuda-butler/packages/butler/` at detached `main` `e3d27c2`. This lists all statically spelled `remuda.NAME` references, including state slots. The `_butler_*` wildcard below is explicitly expanded so that each current slot is accounted for. `remuda.butler.project_home` and `.template` are supported setup words; most `_butler_*` functions are currently internal integration points, even where the mod exposes equivalent user-facing MCP tools or CLI commands.

| Current name(s) | Class | What it does / proposal |
|---|---|---|
| `butler.project_home`, `butler.template` | Private setup configuration this round | Set project home and register topic templates. Keep available to Butler setup; do not promote in this round because the owner approved only the compaction units, session/agent metadata, and three messaging words. |
| `session_order`, `session_detail` | Composite/query | Expose Butler-aware ordering and per-session agent metadata. Promote as `remuda.butler.session_order/detail` in this round. |
| `command` (passed to `extension_command`), Butler MCP tools | Composite interface | CLI command and MCP tools for status/sessions, delegation, messaging, inbox, replies, forwarding, reports. Promote only approved message operations (`send`, `inbox`, `reply`) in this round; other workflows remain private until they have at least two callers. |
| `_butler_compaction_gate`, `_butler_compaction_reset_idle`, `_butler_compaction_submit_matches`, `_butler_register_compaction_schedule`, `_butler_compaction_tick`, `_butler_compaction_submit` | Composite policy/workflow | Compaction decisions and submission workflow. Promote exactly `remuda.butler.ctx_level(name)`, `is_idle(name)`, `compact(name)`, and `compaction_policy(name, st)`; the scheduled tick remains private and composes these words. Match the in-progress design named in the brief. |
| `_butler_launch`, `_butler_topic_new`, `_butler_topic_delegate` | Private composites this round | Launch members, create topics, and delegate tasks. Keep private until these words have at least two callers. |
| `_butler_send`, `_butler_inbox`, `_butler_reply` | Composite | Butler messaging workflows. Promote as `remuda.butler.message.send/inbox/reply` in this round. |
| `_butler_forward`, `_butler_report` | Private composites this round | Forward and report workflows remain private until each has at least two callers. |
| `_butler_sessions` | Composite | Butler-managed sessions listing. Promote as `remuda.butler.sessions`; `session_order/detail` provide approved agent metadata. |
| `_butler_notify`, `_butler_deliver_notices`, `_butler_notify_policy`, `_butler_prompt_is_empty` | Composite policy | Decide whether/how to deliver notices to an agent prompt. Keep private this round; these are helper policy/workflow words and do not meet the approved API scope. |
| `_butler_contribute`, `_butler_reconcile`, `_butler_session_exited` | Internal mechanism | Butler extension integration and lifecycle/session reconciliation. Prefer the core `remuda.extension` and lifecycle hooks; keep Butler-specific adapters private. |
| `_butler_resolve`, `_butler_current_agent`, `_butler_command_run`, `_butler_migrate_legacy_mail`, `_butler_inbox_delivery` | Internal mechanism | Resolve identities/callers, run commands, migrate mail, and adapt delivery events. |
| `_butler_matrix_line`, `_butler_matrix_submit`, `_butler_matrix_start`, `_butler_matrix_stop`, `_butler_matrix_sync_exit` | Internal mechanism | Matrix relay process callbacks and lifecycle. |
| `_butler_telemetry_for` | Internal mechanism | Select an agent telemetry adapter. |
| `_butler_mail` | Internal module surface | Mail module table shared between Butler Lua files. It should become a local module return or a specifically scoped `remuda.butler.mail` API only if external consumers are intended. |
| `_butler_agent_builders`, `_butler_agent_startup`, `_butler_agent_support`, `_butler_telemetry_adapters` | Internal registries | Agent construction, startup metadata, common support, telemetry adapters. |
| `_butler_agent_builders`, `_butler_agent_startup` plus each `codex`/`claude` entry | Internal registry values | Adapter contributions used by the launch composite. |
| `_butler_bus`, `_butler_state`, `_butler_compaction_state` | Internal state | Team and compaction state; mutable details remain private. |
| `_butler_mail_config`, `_butler_matrix_config`, `_butler_status_path`, `_butler_reply_src`, `_butler_statusline_src`, `_butler_helper_src`, `_butler_helper_src_override` | Internal configuration | Module configuration/source injection; not caller vocabulary. |
| `_butler_initial_name`, `_butler_name`, `_butler_argv`, `_butler_relay`, `_butler_matrix_relay` | Internal state | Butler and relay session/process identity. |
| `_butler_compaction_cooldown`, `_butler_compaction_enabled`, `_butler_compaction_interval`, `_butler_compaction_schedule`, `_butler_compaction_threshold`, `_butler_compaction_trace_path` | Internal config/state | Compaction schedule, thresholds, tracing, and compatibility state. |
| `_butler_notice_schedule`, `_butler_reconcile_schedule` | Internal state slots | Existing schedule handles reached through `remuda[key]` in Butler reload cleanup; retain as private schedule state. |
| `_butler_notice_human_idle`, `_butler_task_poke_attempts`, `_butler_task_poke_deferrals` | Internal config | Notice and task-poke bounds. |
| `_butler_test_mode`, `_butler_session_trace_path`, `_butler_matrix_trace_path`, `_butler_matrix_restart_attempts`, `_butler_matrix_restart_after_stop`, `_butler_matrix_restart_schedule`, `_butler_matrix_stop_exit_count`, `_butler_matrix_stopping`, `_butler_matrix_legacy_exit_pending`, `_butler_skip_relay`, `_butler_helper_src_override` | Internal test/runtime seams | Test toggles, trace outputs, restart/shutdown state. Keep private; tests may use controlled fixtures or documented test-only hooks. |
| `_butler_new_ulid` | Primitive implementation helper | Generate mail ids. Private because format/storage ownership belongs to mail implementation. |
| `_butler_matrix_config` and `env`, `args` | Internal adapter inputs | Matrix config and caller values exposed through the mod's private execution environment. |

**Exact Butler `_butler_*` inventory (deduplicated source spellings):** `_butler_agent_builders`, `_butler_agent_startup`, `_butler_agent_support`, `_butler_argv`, `_butler_bus`, `_butler_command_run`, `_butler_compaction_cooldown`, `_butler_compaction_enabled`, `_butler_compaction_gate`, `_butler_compaction_interval`, `_butler_compaction_reset_idle`, `_butler_compaction_schedule`, `_butler_compaction_state`, `_butler_compaction_submit`, `_butler_compaction_submit_matches`, `_butler_compaction_threshold`, `_butler_compaction_tick`, `_butler_compaction_trace_path`, `_butler_contribute`, `_butler_current_agent`, `_butler_deliver_notices`, `_butler_forward`, `_butler_helper_src`, `_butler_helper_src_override`, `_butler_inbox`, `_butler_inbox_delivery`, `_butler_initial_name`, `_butler_launch`, `_butler_mail`, `_butler_mail_config`, `_butler_matrix_config`, `_butler_matrix_legacy_exit_pending`, `_butler_matrix_line`, `_butler_matrix_relay`, `_butler_matrix_restart_after_stop`, `_butler_matrix_restart_attempts`, `_butler_matrix_restart_schedule`, `_butler_matrix_start`, `_butler_matrix_stop`, `_butler_matrix_stop_exit_count`, `_butler_matrix_stopping`, `_butler_matrix_submit`, `_butler_matrix_sync_exit`, `_butler_matrix_trace_path`, `_butler_migrate_legacy_mail`, `_butler_name`, `_butler_new_ulid`, `_butler_notice_human_idle`, `_butler_notice_schedule`, `_butler_notify`, `_butler_notify_policy`, `_butler_prompt_is_empty`, `_butler_reconcile`, `_butler_register_compaction_schedule`, `_butler_relay`, `_butler_reconcile_schedule`, `_butler_reply`, `_butler_reply_src`, `_butler_report`, `_butler_resolve`, `_butler_send`, `_butler_session_exited`, `_butler_session_trace_path`, `_butler_sessions`, `_butler_skip_relay`, `_butler_state`, `_butler_status_path`, `_butler_statusline_src`, `_butler_task_poke_attempts`, `_butler_task_poke_deferrals`, `_butler_telemetry_adapters`, `_butler_telemetry_for`, `_butler_test_mode`, `_butler_topic_delegate`, `_butler_topic_new`.

The literal matcher also sees `_butler_` and `_butler_matrix_*` wildcard-like mentions in comments; these are notation, not additional runtime names. Butler's other referenced core words (`cancel`, `capture`, `capture_styled`, `close`, `contribute`, `contributions`, `emit`, `emit_until_success`, `exec`, `extension_command`, `key`, `kill`, `ls`, `mkdir`, `new`, `process`, `processes`, `schedule`, `send`, `session`, `tool`, `type_text`) are covered in the core table. Butler's non-underscored `env` and `args` are ordinary function parameters/local environment values, not public `remuda` members.

### Rust wire Request surface

`core/src/protocol.rs` currently defines every `Request` variant below. This wire protocol is already a vocabulary of indivisible operations; Lua composites should be built from it rather than creating undocumented transport operations.

| `Request` variant | Class | Meaning |
|---|---|---|
| `List` | Primitive | List sessions. |
| `New` | Primitive | Start a session with command, size, cwd, and environment. |
| `SendLine` | Primitive/composite boundary | Deliver one text instruction as an indivisible act. |
| `Input` | Primitive | Submit a user-authored line as one indivisible batch on the restricted remote front. |
| `Send` | Primitive | Deliver a byte burst with nothing appended. |
| `Feed` | Composite transport act | Deliver burst/pause steps as one indivisible act. |
| `Resize` | Primitive | Set terminal size. |
| `Capture` | Primitive | Read plain screen text. |
| `CaptureStyled` | Primitive | Read styled runs, cursor, and scrollback metadata. |
| `MouseState` | Primitive | Read mouse tracking state/encoding. |
| `Attach`, `AttachTracked`, `AttachStatus` | Primitive + lifecycle queries | Take over a session, track a generation, and check if it was superseded. |
| `Close` | Primitive | End a session. |
| `ListDir`, `Mkdir`, `RemoveDirAll` | Primitive | List, create, or recursively remove directories. |
| `Version` | Primitive diagnostic | Read daemon build version. |
| `Shutdown` | Primitive lifecycle | Stop daemon and destroy image/session state. |
| `Eval` | Mechanism | Execute Lua in a persistent image; a trusted runtime door, not an ordinary session operation. |

The protocol is deliberately lower than the Lua word set: Lua `send` uses `SendLine`; `feed` uses `Feed`; `capture_styled` uses `CaptureStyled`; and the public `remuda.input`/`screen` composites stay expressible through these existing variants. The inventory and this table should be reconciled against the exact enum before implementation.

## Proposed namespaces and promotion policy

| Namespace | Words | Rationale |
|---|---|---|
| `remuda.session` | `list`, `new`, `close`, `attach`; callable table for `session(name)` | Session identity and lifecycle; `__call` preserves the existing handle constructor. |
| `remuda.input` | `text`, `submit`, `type_text` | Names text delivery and submission as units, with type-text retained as their composite. |
| `remuda.screen` | `capture`, `styled`, `expect`, `expect_option` | Reading and acting on screen observations. |
| `remuda.fs` | `list_dir`, `mkdir`, `mkdir_new`, `remove_tree`, `write_atomic` | Filesystem primitives named by intent; `mkdir_new` creates exactly one new directory from an absolute path without a trailing separator, and `write_atomic` replaces one target without following a target symlink. |
| `remuda.runtime` | `sleep`, `fail`, `registry` (read-only) | Runtime controls and supported introspection. |
| `remuda.module` | `exec`, `reload` | Module loading and lifecycle. |
| `remuda.hook` | `on`, `emit*`, `list`, `clear`, `counts` | Event registration and dispatch vocabulary. |
| `remuda.advice` | `add`, `remove`, `member`, `list` | Function wrapping and inspection. |
| `remuda.extension` | `command`, `contribute`, `contributions` | Extension points shared across mods. |
| `remuda.schedule` | `every`, `cancel`, `registry`, `fires`, `skips` | Scheduled work and its inspectable state. |
| `remuda.tool` | `define`, `registry` | MCP tool declaration and listing. |
| `remuda.process` | `start`, `list`, `kill` | Managed subprocess lifecycle; no raw process primitive exposed. |
| `remuda.buffer`, `remuda.window` | `new/list/registry`, `current/registry` | UI objects and registries. |
| `remuda.diagnostics` | `request_counts`, `event_counts`, `schedule_fires`, `schedule_skips` | Operational observation separated from control. |
| `remuda.butler` | `ctx_level`, `is_idle`, `compact`, `compaction_policy`, `sessions`, `session_order`, `session_detail`, `message.send/inbox/reply` | Exactly the Butler words approved for public use this round, built on core primitives. Other Butler words remain private until each has at least two callers. |

Promote only the owner-approved Butler words this round: `ctx_level`, `is_idle`, `compact`, `compaction_policy`, sessions/agent metadata, and message `send`, `inbox`, `reply`. Every other Butler word remains private until it has at least two callers. Keep `_registry`'s writable storage, `_call`, `_descriptors`, counters, ticker callbacks, process internals, lifecycle bookkeeping, module-owned mutable state, test hooks, trace paths, adapter tables, and transport callbacks private. The leading underscore marks implementation mechanism, not merely a stylistic preference.

Nested tables are already established in core: `remuda.buffer.new/set/list` and `remuda.window.current` are registered alongside the top-level table names, and the surface test compares top-level `pairs(remuda)` keys against `BINDINGS`, so table keys are already representable. Use them as precedent. The owner approved this callable-table migration in #206. In particular, `remuda.session` becomes a callable namespace table with `__call`, so existing `remuda.session(...)` behavior stays intact while nested words are added.

## Layering: composites built from words

```text
Rust Request / host boundary
├── List, New, Close, Attach, Resize
│   └── remuda.session.list / new / close / attach
├── SendLine, Send, Feed
│   ├── remuda.input.text = one burst, bracketed when mode 2004 is enabled
│   ├── remuda.input.submit = visible composer + separate Return + one retry
│   └── remuda.input.type_text = text + optional settle pause + submit
├── Capture, CaptureStyled
│   └── remuda.screen.capture / styled
│       └── remuda.screen.expect = capture + match + selected action
│           └── remuda.screen.expect_option = expect over candidate matches
├── ListDir, Mkdir, RemoveDirAll
│   └── remuda.fs.list_dir / mkdir / remove_tree
└── Eval (trusted runtime door)
    └── module, hooks, advice, extension, schedule, process, tools
        ├── remuda.module.exec / reload
        ├── remuda.hook.on + emit* + list/clear
        ├── remuda.advice.add/remove over a named function path
        ├── remuda.extension.command/contribute + contributions
        ├── remuda.schedule.every + cancel + due-tick mechanism (private)
        ├── remuda.process.start + list + kill (spawn/drain/killpg private)
        └── remuda.tool.define + registry + dispatch (private)

Core words
└── remuda.butler composites
    ├── identity resolution + session.list/detail/order
    │   └── remuda.butler.sessions / agent metadata
    ├── session capture + telemetry context + prompt parsing
    │   └── ctx_level(name), is_idle(name)
    ├── ctx_level + is_idle + threshold/cooldown state
    │   └── compact(name) -> compaction_policy(name, state) -> private schedule
    ├── session.list + identity metadata
    │   └── remuda.butler.sessions / session_order / session_detail
    └── mail storage + identity resolution + event delivery
        └── remuda.butler.message.send / inbox / reply

Agent launch, topic workflows, forwarding, reporting, and notification policy remain private this round (further public promotion requires 2+ callers).
```

A composite may call other composites; each layer still has a named unit. The schedule's tick, module dispatch, and Butler internals stay mechanisms rather than becoming callable public words simply to complete a diagram.

## Migration by parallel change

1. **Add names in API v5.** Introduce the new nested vocabulary alongside every current spelling. Each deprecated flat alias emits a clear notice once per process per old spelling, naming its replacement. Notices are suppressed in tests/CI with `REMUDA_SUPPRESS_DEPRECATIONS=1`. Keep arguments, results, errors, and timing identical.
2. **Migrate consumers.** Update core Lua, the Butler mod, examples, and tests to use the new names. Keep flat aliases live throughout API v5. Confirm `BINDINGS` and live table assertions reflect both surfaces intentionally.
3. **Freeze each API version.** `native/tests/api/v1.lua` through `v4.lua` are historical compatibility contracts: do not rewrite old assertions to bless a changed meaning. Add `native/tests/api/v5.lua` for the new words; keep v1-v4 green and unchanged. API v5 carries both the new names and deprecated flat aliases.
4. **Preserve frozen caller indirection.** Frozen fixtures can monkeypatch flat words: `native/tests/api/v4.lua` replaces `remuda.key` and asserts that `expect` key actions call it. Therefore, while the frozen API fixture is loaded, composites must resolve through the old flat slot (or a new nested wrapper must dynamically call that slot), rather than capturing/using a nested implementation that bypasses reassignment. Apply the same rule to advice on flat function paths: calls through a promoted name must remain observable through existing advised/replaced flat names for the compatibility period. Frozen fixtures stay unchanged. Deprecation notices should be emitted once per old spelling per process by default; tests/CI set `REMUDA_SUPPRESS_DEPRECATIONS=1` so frozen API output remains stable and quiet.
5. **Guard the protocol-derived surface.** `script.rs` surface test compares `pairs(remuda)` top-level keys against `BINDINGS`; new namespace table keys must therefore be included in `BINDINGS`. The existing `buffer` and `window` tables prove this is supported. Nested members are currently unguarded, so add a nested-surface check that asserts each namespace table’s intended keys. Update registry metadata in the same change as each new binding; preserve the no-accidental-word guarantee.
6. **Remove in API v6.** Remove deprecated flat aliases only in v6, after v5 has carried both spellings and consumers have migrated. Keep v1-v4 unchanged as historical contracts and v5 as the transition contract.

The implementation is split into small PRs, one namespace family per PR, starting with `remuda.session`. Each slice branches from core `origin/main`; add the v5 fixture and make it RED first, while v1-v4 remain green. No two namespace families are combined in one PR.

## Prior art and design basis

- **Emacs Lisp:** public symbols omit the leading underscore convention used for private implementation names; established Lisp libraries use naming conventions to signal internals. Remuda's `_` names can become a consistent private boundary, with selected stable words promoted instead of normalizing every implementation detail.
- **Lua modules:** tables are the language's ordinary namespace/module mechanism. Nested `remuda.session`, `remuda.input`, and `remuda.butler` tables group words without introducing a new object model.
- **Neovim:** `vim.api` provides a structured API surface while `vim.fn` exposes a separate function vocabulary, illustrating that a deliberate table boundary can distinguish concepts/layers. Remuda can use clearer domain nouns rather than carrying over Neovim's compatibility history wholesale.
- **SICP stratified design:** a system is understood and changed through layers whose words form the vocabulary of the next layer. Naming each level makes it possible to replace a composite without obscuring the primitives beneath it.
- **Forth:** small words compose into larger definitions; the dictionary is a vocabulary rather than a bag of opaque operations. Remuda's Lua and Rust can preserve this property by naming both the building blocks and the composites they support.

## Owner decisions recorded (approval #206)

1. **Direction and namespaces approved.** `remuda.session` becomes a callable table via `__call`; `remuda.session("x")` remains valid, with nested `list`, `new`, and the other approved session words.
2. **Butler promotion scope for this round:** `ctx_level`, `is_idle`, `compact`, `compaction_policy`; session listing and agent metadata; messaging `send`, `inbox`, and `reply`. All other Butler words stay private until they have at least two callers.
3. **Deprecation and versions:** once per process per old spelling, suppressed in tests/CI by `REMUDA_SUPPRESS_DEPRECATIONS=1`; new names arrive in API v5; old flat aliases are removed in API v6.
4. **Implementation sequence:** one namespace family per PR, starting with `remuda.session`; each slice starts from core `origin/main`, and its v5 test is written RED first while v1-v4 stay green.
