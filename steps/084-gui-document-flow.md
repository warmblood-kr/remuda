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
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; finished in 0.36s

cargo test --offline -p remuda-native --lib document -- --test-threads=1
test result: ok. 29 passed; 0 failed; 0 ignored; 0 measured; 1924 filtered out; finished in 0.31s
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

The window itself was not opened on a user desktop: the evidence above is the
built binary plus the pane rows the view renders (the same `Pane::frame` call).
A live window run remains for the owner/leader to perform.
