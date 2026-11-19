# 086 — GUI Markdown pipe tables

## Before

`gui/src/markdown.rs` left pipe tables as literal paragraph lines, so the GUI
document reader showed pipes, delimiter rows and alignment syntax as text.
Table-cell images were invisible to the image scan because it only walked a
block's top-level spans.

## Desired outcome

The bounded GUI Markdown model recognises GFM pipe tables at a block boundary,
keeps per-column alignment and inline styles, rejects malformed tables back to
literal text, and exposes cell spans to existing consumers.

## Expected

- Header and delimiter rows must have the same column count; otherwise the
  source lines stay paragraphs.
- `\|` is cell content even inside code spans and emphasis; edge pipes are
  optional; short rows pad and long rows truncate.
- A table ends at a blank line or another block; a plain body line is a
  one-cell row; a header-only table is valid.
- `MAX_TABLE_COLUMNS` and block budgets fall back to literal text without
  content loss.

## Actual

`cargo test --locked --manifest-path gui/Cargo.toml --lib markdown::`:

```text
running 17 tests
...
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 24 filtered out
```

Nine table tests plus the existing eight parser tests pass. `cargo fmt --check`
is clean. The GUI renderer (S2) and strikethrough (S3) follow separately.
