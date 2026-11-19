# 089 — GUI image decoding under a bound

## Before

The GUI has no way to turn VFS image bytes into something GPUI can draw, and no
bound on what an image may cost. The document pane cannot show an image without
it, but decoding does not depend on the pane, the Markdown model or the window.

## Desired outcome

A self-contained module that decodes PNG and JPEG under dimension, pixel and
allocation caps, by magic bytes rather than by file name, and returns a short
message for the alt/error view. The GUI crate gets a library surface so tests
can reach it without a window.

## Expected

- The GUI crate checks cleanly with all targets, adding only `image` and
  `smallvec`.
- The module's three unit tests pass with no window and no daemon.

## Actual

```text
cargo test --locked --manifest-path gui/Cargo.toml --lib media
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 4 filtered out; finished in 0.03s
```

`cargo check --locked --manifest-path gui/Cargo.toml --all-targets` finishes
with no warnings from this crate. The lock update is the resolution of the
existing manifest for these two dependencies; no other dependency changes.
