# 085 — Carry validated session semantics to the GUI

## Before

The daemon already answers `RenderedSessionList` with a `SessionView` whose rows
are ANSI text. The TUI paints those rows; the GUI cannot, so
`gui/src/main.rs` strips SGR itself and rebuilds each row: it takes indentation
from the peer's plain text, drops the TUI's status cell, and paints its own
glyph from `SessionSummary.alive`/`unavailable`. That is a reverse parse of
rendered output, and it cannot show `working`/`idle`/`blocked`, hierarchy depth,
or the detail line that the TUI shows. `SessionSummary.team.depth` is advisory
metadata, not display authority (`core/src/registry.rs` says so).

## Desired outcome

One renderer emits both the painted row and a structured semantic row bound to
the same `(name, instance_id)`. A consumer that cannot paint ANSI reads the
structured row, gets exactly the hierarchy and status the TUI shows, and never
invents a field the renderer did not produce. Replaced same-name instances stay
distinct because binding is by instance id, not name.

## Expected

`core/src/session_view.rs`:

- `ViewState` = `alive | exited | working | idle | blocked`.
- `ViewSemantics { name, instance_id, depth, state, detail }`, additive and
  optional on the wire.
- `SessionView.semantics` (skipped when empty) and
  `SessionView::with_semantics`; `ValidatedView::semantics()`.
- `validate_semantics` requires the same length and order as `entries`, the
  same `(name, instance_id)` at each index, no duplicates, `depth <= 20`, and a
  detail that is non-empty, `<= 256` bytes, control-free and zero-width-free.
  An empty sidecar is accepted so older peers still validate.

`native/src/tools.lua`: `status_cell` also returns the plain state (`exited`
before liveness, then the fresh program status, then decorator activity, then
`alive`), and `render_session_snapshot` emits `semantics` in `ordered` order
with the already-computed `depth`, `state` and `detail`.

`native/src/image.rs`: `render_session_view` reads the optional table, rejects
an unknown state or an out-of-range depth, attaches the rows, and still applies
`validate()` plus the 32 KiB serialized cap.

## Actual

PENDING VERIFICATION: source-only, no build or test run (the build slot is
unassigned). Static checks on this commit: `rustfmt --check` clean for the three
changed Rust files, `scripts/check-comments.py` within cap, `git diff --check`
clean. Lua syntax is unverified until a test loads `tools.lua`.

Tests added but not yet run:

- `core/tests/session_view_contract.rs`
  - `accepts_semantics_bound_to_the_same_entries_and_order`
  - `rejects_semantics_that_are_unbound_out_of_range_or_unsafe` (count,
    duplicate/mismatched identity, depth 21, zero-width detail)
  - `view_without_semantics_still_validates_and_reports_none`
- `native/src/image.rs`
  - `rendered_session_view_carries_instance_bound_semantics` drives the real
    renderer through `render_session_view`: reordered depth 1, `blocked` from
    decorator activity, `alive` with no detail, `exited` outranking activity.
  - `session_view_json_without_semantics_decodes_for_older_peers` serializes a
    view without the sidecar, asserts the field is absent, and decodes it back.

Acceptance at the slot: those five tests, the existing `session_view_contract`
suite, `native` document/TUI tests that render the list, and the GUI session
list tests. GUI consumption is step 086.

## Limitations

- The 32 KiB cap in `render_session_view` counts the serialized view, so the
  sidecar reduces the largest household that fits. Typical crews are far below
  it; a very large list now fails closed with "session view too large" instead
  of dropping fields.
- The TUI still paints `lines`; it does not read the sidecar, so its behavior is
  unchanged.
- No GUI file changes here: `sessions_from_view` keeps parsing for now and is
  replaced in step 086.
- Physical display still unverified; evidence remains headless.
