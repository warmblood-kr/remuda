# 085 — GUI document library

## Before

The shared document model exists (step 083) and the TUI reads documents
(step 084), but the GUI has no adapter: it cannot turn a `Browser` into a pane,
render Markdown, decode an image or colour inert code. The GUI window still
serves the old standalone view.

## Desired outcome

The GUI crate exposes a library half of the document pane: bounded `Pane` and
`Workspace` types over `remuda_native::documents`, Markdown rendering, image
decoding and inert code highlighting. It reads no VFS itself and executes
nothing from a document. Tests drive it without a window: one seam test against
the shared projection, and one flow test against a private daemon and real VFS
(list, open Markdown, follow a link, back, refusals and bounds).

## Expected

- The GUI workspace checks cleanly with all targets.
- The seam test passes with no daemon.
- The flow test passes against a private daemon, touching no user daemon.

## Actual

`cargo check --locked --manifest-path gui/Cargo.toml --all-targets` finished
with no warnings from this crate (only the pre-existing dependency
future-incompat note).

`cargo test --locked --manifest-path gui/Cargo.toml --test document_projection_seam`:

```text
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

`cargo test --locked --manifest-path gui/Cargo.toml --test document_flow`
(private daemon, short TMPDIR):

```text
test gui_document_pane_lists_opens_follows_a_link_and_returns ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.61s
```

No window is opened by this step; nothing is user-visible yet.
