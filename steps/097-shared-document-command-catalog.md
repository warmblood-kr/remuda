# 097 — One spelling for the document command vocabulary

## Before

`cluster.documents` is declared in Lua and validated against a hand-written
array inside `native/src/keybindings.rs`. Any second renderer would have to copy
that array or invent its own, and a copy drifts silently: the validation would
keep passing while the two lists disagreed.

## Desired outcome

The vocabulary has one home. The Lua declaration is validated against it, and a
renderer that has no terminal event can resolve the same snapshot row.

## Expected

- `core::document_commands` names the context and the nine actions, in catalog
  order; `native/src/keybindings.rs` validates against that list instead of its
  own copy.
- `Snapshot::resolve_canonical` is the single resolution step; `resolve`
  delegates to it, so a named key and a terminal event cannot diverge.
- `Snapshot::built_in_defaults` builds the snapshot from the embedded Lua
  (`keybindings.lua` + `tools.lua`), so a caller without a daemon — and a test —
  resolves the real declaration rather than a copy.
- No TUI behavior changes: the same nine names, the same keys, the same help.

## Not in this step

The GUI representation (document pane resolving through this catalog, plus
`documents.close`/`documents.address` in the window) belongs to the GUI document
slice, where `gui/src/documents.rs` and `gui/src/document_workspace_view.rs`
exist. This step is the buildable prerequisite: it adds no GUI file and changes
no visible TUI key.

## Actual

Verbatim on base `21f022b`, in `work/keymap-21f022b`:

```
$ cargo test -p remuda-native --lib keybindings
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 1944 filtered out

$ cargo test -p remuda-native --lib documents
test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 1938 filtered out

$ cargo check -p remuda-native --bin remuda
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.53s
```

Full native library suite: 1924 passed / 26 failed; a clean `21f022b` run gave
1924 / 26 with an identical failure list, so nothing new fails here. The GUI
build on this base (`cargo check --all-targets` in `gui/`) is clean, which shows
the changed native library does not break the existing GUI.

Rebase note: on this base the declaration sits after `cluster.search`; the arm is
replaced in place, never merged with the old position, and
`grep -c '"cluster.documents" =>' native/src/keybindings.rs` must be 1 because two
arms compile (`unreachable_pattern`) instead of failing.
