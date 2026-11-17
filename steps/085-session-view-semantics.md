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
distinct because binding is by instance id, not name. The optional sidecar must
never cost an existing view: a view that was acceptable before this change is
still acceptable, with or without the sidecar.

## Expected

`core/src/session_view.rs`:

- `ViewState` = `alive | exited | working | idle | blocked`.
- `ViewSemantics { name, instance_id, depth, state, detail }`, additive and
  optional on the wire.
- `SessionView.semantics` (skipped when empty) and
  `SessionView::with_semantics`; `ValidatedView::semantics()`.
- Two different failure classes:
  - **Structure — fail closed.** `validate_semantics` requires the same length
    and order as `entries`, the same `(name, instance_id)` at each index, no
    duplicates, and `depth <= 20`. A partial or unbound sidecar rejects the
    view as before.
  - **Representation — drop the sidecar only.** `semantics_representable`
    requires each detail to be non-empty, `<= 256` bytes, and made of the same
    plain characters `safe_line` accepts (controls and bidi marks rejected;
    combining marks, joiners and variation selectors are ordinary text). When
    that fails, `validate_for_display` returns the same validated entries and
    sessions with `semantics()` empty. Rendered rows are never lost.
  - **Liveness — drop the sidecar only.** `semantics_consistent` compares each
    state with the authoritative `SessionSummary.alive` for the same
    `(name, instance_id)`: an exited session may only claim `exited`, and a
    live session may claim anything but `exited`. A peer that claims activity
    for a dead instance (or death for a live one) loses the sidecar, never the
    rows. State is display-only and is never identity or authorization.
- `safe_line` and `valid_detail` share `plain_text_char`, so the sidecar can
  never be stricter than the row it accompanies.

`native/src/tools.lua`: `status_cell` also returns the plain state (`exited`
before liveness, then the fresh program status, then decorator activity, then
`alive`), and `render_session_snapshot` emits `semantics` in `ordered` order
with the already-computed `depth`, `state` and `detail`.

`native/src/image.rs`: `render_session_view` reads the optional table, rejects
an unknown state or out-of-range depth, and validates the view as before.
`serialize_bounded_view` keeps two separate limits: `VIEW_INNER_CAP` (32768)
still bounds a rows-only view exactly as before the sidecar existed, and
`VIEW_WIRE_CAP` (32768) bounds the peer body (`view_wire_len` = the view wrapped
in `Response::RenderedSessionList` plus the daemon's newline). The wire cap
decides only whether the sidecar may travel: if the wrapped body would exceed
it, the validated rows are re-serialized without the sidecar. A rows-only view
whose own JSON fits `VIEW_INNER_CAP` still succeeds locally even when its
wrapped body is larger, so pre-sidecar output is preserved; rows whose own JSON
exceeds the inner cap still fail with `session view too large`.

## Actual

PENDING VERIFICATION: source-only, no build or test run (the build slot is
unassigned). Static checks on this commit: `rustfmt --check` clean for the three
changed Rust files, `scripts/check-comments.py` within cap, `git diff --check`
clean. Lua syntax is unverified until a test loads `tools.lua`.

Tests added but not yet run:

- `core/tests/session_view_contract.rs`
  - `accepts_semantics_bound_to_the_same_entries_and_order`
  - `rejects_semantics_that_are_unbound_or_out_of_range` (count, duplicate or
    mismatched identity, depth 21)
  - `accepts_combining_marks_and_joined_emoji_in_details` (decomposed accent,
    ZWJ family, variation selector)
  - `unrepresentable_detail_drops_only_the_sidecar` (C0 control and bidi mark:
    rows and entries preserved, `semantics()` empty)
  - `semantics_state_must_agree_with_authoritative_liveness` (dead+working and
    live+exited drop the sidecar; dead+exited and live+idle keep it)
  - `mismatched_binding_still_fails_closed_with_a_bad_detail`
  - `view_without_semantics_still_validates_and_reports_none`
- `native/src/image.rs`
  - `rendered_session_view_carries_instance_bound_semantics` drives the real
    renderer through `render_session_view`: reordered depth 1, `blocked` from
    decorator activity, `alive` with no detail, `exited` outranking activity.
  - `session_view_json_without_semantics_decodes_for_older_peers`
  - `sidecar_is_dropped_when_it_alone_overflows_the_wire_cap` (70 rows: the
    wrapped body fits without the sidecar and not with it; decoded result keeps
    all rows and no sidecar)
  - `rows_whose_inner_view_exceeds_the_cap_still_fail` (unchanged
    `session view too large`)
  - `rows_only_keeps_the_old_inner_cap_beyond_the_peer_wire_cap` (own JSON fits,
    wrapped body does not: still succeeds locally)
  - `sidecar_is_kept_only_when_the_peer_body_fits` (same view, wire cap minus
    one byte: rows kept, sidecar dropped)
  - `rows_over_the_inner_cap_still_fail` (inner cap alone fails it)
  - `view_wire_len_counts_the_wrapped_response_and_newline` (equality with
    `serde_json::to_vec(Response::RenderedSessionList)` plus the newline, and
    strictly larger than the inner view)
  - `sidecar_contradicting_summary_liveness_is_dropped` (small view, so size is
    not the cause)

Acceptance at the slot: those eight tests, the existing
`session_view_contract` suite, `native` document/TUI tests that render the
list, and the GUI session-list tests. GUI consumption is step 086.

## Limitations

- A rows-only view whose own JSON exceeds 32 KiB still fails, exactly as before;
  the sidecar no longer causes that failure by itself. A rows-only view whose
  wrapped body exceeds the peer cap is still rejected by a remote peer, as it
  was before the sidecar existed; local clients keep accepting it.
- A single unrepresentable detail drops the whole optional sidecar for that
  view. That is deliberate: a partial sidecar would mix trusted and untrusted
  structure. Consumers must treat empty semantics as "no structured data", not
  as a reason to parse the row.
- The TUI still paints `lines`; it does not read the sidecar, so its behavior is
  unchanged.
- No GUI file changes here: `sessions_from_view` keeps parsing for now and is
  replaced in step 086.
- Physical display still unverified; evidence remains headless.
