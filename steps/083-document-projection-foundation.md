# 083 — Document primitives

## Before

`main` can list and read arbitrary bytes through the VFS, but nothing in the
crate turns a document into bounded, addressable lines. Every renderer would
have to re-implement fetching, size limits, wrapping and display escaping, and
the TUI and GUI would disagree.

## Desired outcome

Renderer-independent document primitives in `remuda-native`: typed locations
and pages, bounded VFS fetch/decode for listings, plain text, inert HTML and
Markdown, and shared label/wrap/clip helpers. No UI and no browsing state
machine yet, so the TUI and GUI can build on one decoding contract.

## Expected

- The module compiles on `main` with the existing core VFS/protocol APIs.
- Its unit tests pass without a daemon.
- A read never fetches more than the documented byte and line budgets.

## Actual

`cargo check --locked -p remuda-native --lib` is clean (no warnings), and the
seven primitive tests pass:

```text
running 7 tests
test documents::tests::basename_label_keeps_the_last_component_only ... ok
test documents::tests::entry_display_name_is_compact_but_keeps_the_full_path ... ok
test documents::tests::parent_directory_stops_at_the_root ... ok
test documents::tests::listing_and_text_display_cannot_escape_their_boundaries ... ok
test documents::tests::path_labels_escape_controls_without_changing_navigation_identity ... ok
test documents::tests::markdown_pages_carry_their_source_for_a_styled_reader ... ok
test documents::tests::read_refusal_binary_and_large_content_are_errors ... ok

test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 1937 filtered out
```

The Browser state machine and its twelve navigation/history tests follow in a
separate change.
