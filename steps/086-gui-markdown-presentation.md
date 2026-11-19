# 086 — GUI Markdown presentation

## Before

The native Markdown projection (step 083) flattens styling so every adapter can
read the same text, but the GUI reader needs block and inline structure back.

## Desired outcome

A bounded, inert presentation model for already-bounded Markdown: headings,
fenced code, quotes, lists, thematic breaks, paragraphs and inline emphasis,
code, links and image placeholders. It never loads a link or image and leaves
unknown constructs literal.

## Expected

- The GUI workspace checks cleanly.
- The module's unit tests pass without a window or daemon.

## Actual

```text
cargo test --locked --manifest-path gui/Cargo.toml --lib markdown
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 6 filtered out; finished in 0.10s
```

`cargo check --locked --manifest-path gui/Cargo.toml --all-targets` finished with
no warnings from this crate.
