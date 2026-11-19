# 087 — GUI document adapter

## Before

The GUI has bounded media decoding, inert code tokens and a Markdown
presentation model (steps 085-086), but no pane that ties them to the shared
document model: no tabs, no directory selection, no image reservation.

## Desired outcome

`Pane` and `Workspace` over `remuda_native::documents`: the pane owns a
`Browser` and a bounded image budget, the workspace owns tabs. Neither reads the
VFS itself, executes content or renders a window. A seam test consumes the shared
projection from the GUI crate; a flow test drives a private daemon and the real
VFS through list, open Markdown, follow a link and back.

## Expected

- The GUI workspace checks cleanly with all targets.
- The seam test passes with no daemon.
- The flow test passes against a private daemon only.

## Actual

```text
cargo test --locked --manifest-path gui/Cargo.toml --test document_projection_seam
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

cargo test --locked --manifest-path gui/Cargo.toml --test document_flow
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.39s
```

`cargo check --locked --manifest-path gui/Cargo.toml --all-targets` is clean.
No window is opened and nothing is user-visible yet.
