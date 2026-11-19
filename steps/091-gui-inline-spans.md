# 091 — Inline spans for GUI Markdown

## Before

A flattened text projection is enough for an adapter to read a document, but a
GUI reader needs the styling back: bold, italic, code, a link's label and
target, an image placeholder. Inline parsing does not need the block model or
the window.

## Desired outcome

A bounded inline contract: one display line becomes styled text runs; a link
keeps its label and target, an image becomes a text placeholder, malformed
delimiters stay literal. Nothing loads a target or runs a fence, and every loop
is bounded by the line length plus a fixed span budget.

## Expected

- The GUI crate checks cleanly with all targets and no new dependencies.
- The inline tests pass with no document and no daemon.

## Actual

```text
cargo test --locked --manifest-path gui/Cargo.toml --lib markdown_inline
test markdown_inline::tests::inline_bold_italic_code_and_links_produce_styled_spans ... ok
test markdown_inline::tests::malformed_inline_markup_stays_literal_and_bounds_hold ... ok
test markdown_inline::tests::multibyte_text_and_escapes_stay_on_character_boundaries ... ok
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 7 filtered out; finished in 0.01s
```

`cargo check --locked --manifest-path gui/Cargo.toml --all-targets` finishes
with no warnings from this crate.
