# 038 — cluster controls use the effective keymap

## Before

The Lua key registry supports the session-list toggle. Cluster commands and
help still contain independent native key literals, so user overrides cannot
change them together. Ctrl-L redraw also accepts non-Press events.

## Desired outcome

Declare cluster tree/search/composer/confirmation/raw-exit/redraw defaults once
in Lua. Resolve native actions and render effective help from the same local
launch-time snapshot, retaining target/input/instance safety and native text,
paste, and raw-byte fallback. Keep the exit key visible at forty columns.

## Expected

Rebinding removes the old command and updates help. Search/composer keep text
fallback. Confirmation accepts only its registered keys and cancels every other
key. Raw mode forwards the old exit after a rebind, including Ctrl-4's terminal
alias, while intercepting the new exit. Redraw respects Press and raw context.
Long Unicode help wraps into reserved rows instead of clipping controls.

## Actual

The new snapshot regression first failed before cluster catalog support:

```text
called `Result::unwrap()` on an `Err` value: "unknown key context: cluster.tree"
```

Tree/confirmation remap tests then failed against the old native handlers.
The raw-alias regression additionally exposed Ctrl-4 being dropped after
unbinding the old exit. Normalizing terminal aliases before encoding makes it
forward Ctrl-backslash's byte. Captured daemon-free verification:

```text
test result: ok. 103 passed; 0 failed; 0 ignored; 0 measured; 988 filtered out; finished in 3.07s
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 1080 filtered out; finished in 0.03s
```

These are the audited `cluster_tui::tests::` and `keybindings::` library
filters under private HOME/XDG/runtime. Composer and confirmation tests also
pass. All-target Clippy and comment/principles/steps/Lua-shell/no-Python checks
pass. The daemon-backed cluster integration fixture is adapted to pass an
actual fetched snapshot to the composer; it is left for CI, not launched
locally. Additional help rows reduce the pane area. Live refresh, other list
controls and alias rename (#705) remain outside this slice. Refs #704.
