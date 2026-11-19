# 092 — GUI Markdown blocks

## Before

Inline spans exist (step 091), but a reader needs the document's structure:
headings, fenced code, quotes, lists, thematic breaks and paragraphs. Building
blocks does not need the document pane, the VFS or the window.

## Desired outcome

A bounded, inert block model: bounded Markdown source becomes blocks with their
markers and code lines, and each text block's spans come from the inline
contract. Unknown constructs stay literal, links keep only label and target,
and nothing loads or executes.

## Expected

- The GUI crate checks cleanly with all targets.
- The block tests and the inherited inline tests pass; both sides keep their
  bounds coverage.

## Actual

```text
cargo test --locked --manifest-path gui/Cargo.toml --lib
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.08s
```

The 15 are media 3, highlight 4, inline 3 and block 5.
`cargo check --locked --manifest-path gui/Cargo.toml --all-targets` finishes
with no warnings from this crate.
