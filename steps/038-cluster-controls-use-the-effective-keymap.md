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
unbinding the old exit. Narrowly translating only the numeric Ctrl-4 terminal alias before encoding
makes it forward Ctrl-backslash's byte. Captured daemon-free verification:

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

Review caught command normalization changing raw Ctrl-H to DEL, Ctrl-J to CR,
and dropping Ctrl-@/Space. A new raw-mode regression first captured:

```text
left: [127, 13, 27, 127, 27, 13]
right: [8, 10, 0, 0, 27, 8, 27, 10, 27, 0, 27, 0]
```

Forwarding now sends original events to the terminal encoder with only numeric
Ctrl-4 translated to Ctrl-backslash. Command canonicalization stays in lookup.
The regression covers BS/LF/NUL, Alt combinations, remapped numeric exit
passthrough, and interception of the new exit.
