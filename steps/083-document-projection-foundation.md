# 083 — Document projection foundation

## Before

`main` can list and read arbitrary bytes through the VFS, but nothing in the
crate model turns a document into bounded, addressable lines. The TUI document
workspace and the GUI document pane both depend on such a model; without it a
renderer has to re-implement fetching, size limits, wrapping, link identity and
history on its own, and the two would disagree.

## Desired outcome

One renderer-independent, read-only document model exists in `remuda-native`:
a bounded browse/read/history state machine with plain text, inert HTML and
Markdown projection, typed link targets, and a transport deadline. It has no UI
and no session coupling, so the TUI and the GUI can both consume it and tests
can exercise it without a terminal or a window.

## Expected

- The module compiles on `main` with the existing core VFS/protocol APIs.
- Its unit tests pass without a daemon.
- Requesting a page never blocks past the transport deadline and never fetches
  more than the documented byte and line budgets.
- Unsupported HTML links are visible but neither navigable nor fetched.

## Actual

Each slice was checked on its own commit with
`cargo check --locked -p remuda-native --lib` (no warnings), and its tests:

```text
# client::deadline
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 1950 filtered out; finished in 0.25s
# document_html
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 1951 filtered out; finished in 0.07s
# document_markdown
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 1946 filtered out; finished in 0.18s
# documents::
test result: ok. 19 passed; 0 failed; 0 ignored; 0 measured; 1937 filtered out; finished in 0.04s
```
