# 090 — Inert code tokens for GUI documents

## Before

The Markdown presentation model will show fenced code, but the GUI has no way
to colour a line without running or loading it. Tokenizing is independent of
the pane, the image decoder and the window.

## Desired outcome

A bounded tokenizer that maps one already-projected display line to byte ranges
with a kind (keyword, string, comment, number). It runs nothing, loads nothing
and changes no text; the view maps a kind to a colour and draws the same bytes.

## Expected

- The GUI crate checks cleanly with all targets and no new dependencies.
- The module's four unit tests pass with no window and no daemon.

## Actual

```text
cargo test --locked --manifest-path gui/Cargo.toml --lib highlight
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s
```

`cargo check --locked --manifest-path gui/Cargo.toml --all-targets` finishes
with no warnings from this crate.
