# 084 — Browse VFS documents in the GUI

## Before

The GPUI app lists nodes and sessions and shows a read-only session snapshot or
an attached terminal. It cannot list documents, open one, follow a link or go
back, so a published design or README cannot be read in the GUI at all.

## Desired outcome

Open the GUI, choose **Documents**, see the local VFS root, select a Markdown
document and read it as prose, press Next link and open it, then Back to the
first document. Loading and error states are visible, unsupported files say so,
and nothing unbounded reaches the window.

## Expected

`gui/src/documents.rs` is a thin pane over `remuda_native::documents::Browser`
(shared projection, link resolution, history). `gui/src/lib.rs` exposes it so
tests and examples can drive it without a window. `LocalGui` gets a `documents`
field, a Documents/Close documents toggle, a status row, a monospace frame of
the browser's own rows, and Up/Down/Open/Back/Prev link/Next link/Refresh
controls plus the same keys when the frame has focus. The existing 150 ms drain
polls the browser; session preview/input/terminal rows are hidden while the
document pane is open, and closing restores them.

Attach is never offered in document mode: its block is gated on
`!documents_open`, and session attach itself is now two-step. "Attach
interactive terminal" only requests confirmation for the exact selected
target; "Confirm attach" performs it and "Cancel" clears it. A selection change
clears the confirmation, and `attach_selected` refuses unless the confirmation
still matches the current selection. `attach_confirm` is the tested state.

Opening the pane moves keyboard focus to the document frame, so arrows,
Enter, Esc, Tab and `r` work without a second click; closing returns focus to
the session line input. `Pane::note_key`/`key_events` record that a key event
actually reached the pane, which the TestWindow test uses as focus evidence.

Layout is responsive instead of hard-coded: the sidebar is 28% of the viewport
clamped to 150-300 px, the right pane is a flex column, the document frame is a
flexible vertically scrollable body, and the control row wraps in a footer, so
controls stay reachable at small sizes. Columns and rows come from
`window.viewport_size()` and remain clamped to the 120-column/48-row caps.

Bounds at the GUI boundary: `Pane::frame` clamps to 120 columns and 48 rows, the
daemon read request is capped at 1 MiB (plain/Markdown) or 128 KiB (HTML), and
the projection caps 4092 rows and 128 links. Binary or non-UTF-8 files show an
explicit "image preview is not implemented" status instead of a decode attempt.

## Actual

```text
cargo build --offline --manifest-path gui/Cargo.toml --bin remuda-gui
Finished `dev` profile [unoptimized + debuginfo] target(s)

env TMPDIR=/private/tmp cargo test --offline --manifest-path gui/Cargo.toml \
  --test document_flow -- --test-threads=1
test gui_document_pane_lists_opens_follows_a_link_and_returns ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; finished in 0.32s

cargo test --offline --manifest-path gui/Cargo.toml --bin remuda-gui -- --test-threads=1
test result: ok. 47 passed; 0 failed; 5 ignored; 0 measured; finished in 41.30s

cargo test --offline -p remuda-native --lib document -- --test-threads=1
test result: ok. 29 passed; 0 failed; 0 ignored; 0 measured; 1930 filtered out; finished in 0.33s
```

The assembled test starts a private daemon in a scratch `REMUDA_RUNTIME_DIR`
with its own HOME/XDG state, initializes a no-listen cluster, publishes
`guide.md`, `next.md` and a non-UTF-8 `image.png` through the real `remuda vfs
put` CLI, and drives the pane: root listing contains both documents, opening
guide.md shows `Read this next [1].` with no `#`/`**`, Next link exposes
`remuda:///next.md`, opening it shows `Second page prose.`, Back restores the
guide, a missing document shows ERROR, the image shows the unsupported notice,
and `frame(usize::MAX, usize::MAX)` stays within 48 rows and 120 columns. The
daemon, its hosts and scratch directory are killed/removed on drop.

The GPUI tests run against a `TestWindow`: `documents_pane_is_laid_out_and_operable_at_default_and_small_sizes`
opens with the pane already present (gpui accumulates debug bounds across
frames, so absence is only asserted for an element the window never painted),
checks the frame and every control has a non-zero clickable area at the default
(maximised) size and after `simulate_resize(420x320)`, clicks Up/Down/Open/Back/
Prev/Next/Refresh, then closes and asserts the session UI (attach, input,
preview) is painted again. `attach_confirmation_targets_the_selected_session`
clicks Attach once, sees the confirmation row, and cancels back to the initial
state; `attach_requires_confirmation_for_the_exact_target` (unit) proves attach
does nothing without a matching confirmation and that a selection change clears
it. `opening_documents_focuses_the_pane_and_closing_restores_session_input` opens
Documents with a click, asserts the pane's focus handle is focused, sends a
`down` keystroke and observes the pane handled it without another click, closes
the pane, asserts focus returned to the session input, then clicks the line
input and types `x` to prove session input still works after documents.

Regression acceptance on this checkout: `gui/tests/peer_live_smoke.py` (two
isolated daemons, no real desktop) printed PASS: GUI node/session clicks, live
output and guarded input reached the isolated peer; wrong instance, unadmitted
and revoked peers were refused. `gui/tests/peer_list_acceptance.sh` fails at
`ack == [1]` (got 114) on this machine, and the same failure reproduces on an
unmodified copy of the candidate, so it is pre-existing and not caused by this
change. The window was not opened on a user desktop (`CGGetActiveDisplayList=0`
per the leader), so pixel smoke was not possible; the remaining evidence is the
built binary and the same `Pane::frame` rows the view renders.
