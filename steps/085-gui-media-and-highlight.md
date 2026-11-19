# 085 — GUI media and highlight primitives

## Before

The GUI has no way to turn VFS image bytes into a renderable image under a
bound, and no inert tokenizer for code shown in a document. Both are needed by
the document pane but neither depends on it.

## Desired outcome

Two self-contained modules in the GUI library: bounded PNG/JPEG decoding
(magic-byte sniffing, dimension and allocation caps) and an inert tokenizer that
maps a display line to byte ranges without running or loading anything. The
library surface exists so tests can reach them.

## Expected

- The GUI workspace checks cleanly with only the two new dependencies
  (`image`, `smallvec`).
- Media and highlight unit tests pass without a window.

## Actual

`cargo check --locked --manifest-path gui/Cargo.toml --all-targets` finished
with no warnings from this crate.

```text
cargo test --locked --manifest-path gui/Cargo.toml --lib media
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 4 filtered out; finished in 0.03s

cargo test --locked --manifest-path gui/Cargo.toml --lib highlight
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s
```
