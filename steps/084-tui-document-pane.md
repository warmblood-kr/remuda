# 084 — TUI document pane

## Before

`main` has the document projection (step 083) but no way to look at a document.
A user in the cluster TUI can only see sessions; VFS documents are reachable
from other clients or not at all. The projection's browse, back, link and
inspection behaviour is therefore untested against a real key path.

## Desired outcome

Pressing `d` on the cluster tree opens a document browser for the local node,
with `cluster.documents` and `documents.*` keybindings for address, links,
open, back, close, refresh, up and down. While it is open, keys and paste go to
the browser instead of the session composer, the session draft is preserved,
and closing (or pressing the composer's `M-d` from a focused composer) returns
to the same selection with the draft intact.

## Expected

- The pane renders from the same `documents::Browser` the GUI will use, with
  the parent row enabled so a nested directory can be left upward.
- The keymap declares the nine `documents.*` actions and no others gain them.
- Paste while the pane is open appends nothing to the composer and enqueues no
  input.
- Closing the pane restores the tree selection and the composer draft.

## Actual

`cargo test --locked -p remuda-native --lib cluster_tui`:

```text
test result: ok. 233 passed; 0 failed; 0 ignored; 0 measured; 1724 filtered out; finished in 20.64s
```

`cargo test --locked -p remuda-native --lib keybindings`:

```text
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 1944 filtered out; finished in 0.05s
```

`cargo test --locked -p remuda-native --test cluster_tui` with a short TMPDIR:

```text
test result: FAILED. 5 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.11s
```

The failure is `stopped_daemon_input_becomes_uncertain_and_later_input_recovers`
waiting for the shell prompt; it reproduces on the parent commit with these
changes stashed, so it is not caused by this step.
