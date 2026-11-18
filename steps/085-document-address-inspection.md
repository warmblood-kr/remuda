# 085 — Inspect a complete VFS address in a narrow terminal

## Before

Long paths with a shared prefix are silently clipped to identical labels.
Escaping control/bidi characters does not distinguish suffixes outside the
viewport. There is no way to inspect the selected full address.

## Desired outcome

Mark clipped path labels with an ellipsis. Press `a` to inspect the selected
file/directory URI, or the current document URI, without fetching or navigating.
Wrap the entire address and scroll with arrows; `a` or Esc returns to the same
selection and page. Preserve the actual VFS identity and session input isolation.

## Expected

The inspection view explicitly uses ASCII escapes for non-ASCII/control
characters and literal backslashes, so even one-column terminals can show every
code point. It retains the complete address without the body display-row cap.
Ordinary listing labels and body text keep their existing display policies.
The address action is first in the default document help for discoverability.

## Actual

Two long shared-prefix targets with different suffixes have marked truncated
labels and can each be fully inspected. A regression reconstructs the complete
escaped URI using widths 1, 2, 8 and 20 and heights 1, 2 and 3, including Unicode
and bidi code points. It verifies selection/identity preservation, no requests,
Esc return and harmless zero-size rendering. Real private-daemon/PTY execution
opens the address view, checks the URI, returns to the document and completes
existing paste/draft/deadline recovery checks.

```text
cargo test --offline -p remuda-native --lib documents:: -- --test-threads=1
test result: ok. 6 passed; 0 failed; finished in 0.05s

env TMPDIR=/private/tmp cargo test --offline -p remuda-native --test cluster_tui documents_browse_plain_text_and_return_without_sending_browser_input -- --test-threads=1
test result: ok. 1 passed; 0 failed; finished in 12.29s

cargo clippy --offline --workspace --all-targets -- -D warnings
Finished `dev` profile in 3.52s
```

This adds an inspection view rather than unique abbreviations for every listing
row. Execution evidence is macOS; no GUI changes or Windows runtime claim.

### Refresh selection race follow-up

A delayed refresh could replace the selected item behind the inspection view.
A deterministic regression first failed with actual `docs/B` versus inspected
`docs/A`. Entering inspection now invalidates the in-flight generation and
clears queued refreshes while preserving the current page and selection.
The old worker remains bounded and is drained normally; it cannot replace the
inspected snapshot. A paused refresh is reported on return with `r` to refresh.
This preserves the inspected path even if it was removed from the new listing;
opening a subsequently deleted path reports the normal read error instead of
opening a different item.

The regression covers reordered/removed entries, active/queued refreshes and
replies before/after leaving inspection, then presses Enter and asserts the
original document location. This is a deterministic state test; the PTY test
remains an assembled-path smoke check rather than a controlled reorder test.

```text
cargo test --offline -p remuda-native --lib documents:: -- --test-threads=1
test result: ok. 7 passed; 0 failed; finished in 0.05s

env TMPDIR=/private/tmp cargo test --offline -p remuda-native --test cluster_tui documents_browse_plain_text_and_return_without_sending_browser_input -- --test-threads=1
test result: ok. 1 passed; 0 failed; finished in 12.10s
```
