# 087 — Image budget reopens after navigation

## Before

The document browser shares an 8-entry pane cache, a 32-entry workspace cache
and 128 MiB of decoded image memory. When a pane reached its entry cap or the
workspace entry budget was full, `request_image` set `image_limited` and
refused every later image. `release_images` ran only on refresh, cancel and
pane drop: opening another image document or following a link left the old
Ready/Failed entries and reservations in place. After enough image browsing a
tab could therefore refuse every new image until it was explicitly refreshed
or closed, even though the old images were no longer on screen. The UI also
called every refusal "workspace image budget reached", hiding whether the
pane cap or the workspace cache was responsible and what the user could do.

## Desired outcome

Leaving an image document releases that pane's image cache and opens the new
image normally, while the current document keeps its own images. Refusals name
the actual limit and an action that can release it. Every existing VFS,
encoded-byte, pixel, decoded-memory and entry bound stays in place; no limit
is raised.

## Expected

- `open_document`, `open_path` (files and directories), `open_selected` and a
  successful `back` release the pane's images and reset `image_limited` before
  navigating. A new image document or Markdown page can then load its own
  images.
- The active document does not evict images it is still displaying. A split
  tab or another workspace tab is independent and keeps its own reservations.
- The pane cap reports `this tab already holds 8 images; open a document with
  fewer images`. The workspace entry cap reports the 32-entry cache and says
  to close another image tab or refresh this document. Decoded-memory refusal
  names the 128 MiB workspace budget and the same action.
- A failed entry is released when the user leaves the document, so navigating
  back can retry it. Within one document the existing 8-entry bound still
  applies; a page with more images stays bounded rather than looping.
- No PNG/JPEG bound or VFS transfer bound changes. The image worker still
  serializes decodes and reserves decoded RGBA against the workspace budget.

## Actual

```text
# macOS, image-budget checkout on 5e6db270, existing shared target reused
cargo test --offline --bin remuda-gui -- --test-threads=1
test result: ok. 130 passed; 0 failed; 8 ignored; 0 measured; finished in 76.10s
# includes:
#   navigating_to_a_second_large_image_releases_the_first_reservation ... ok
#   navigating_after_the_pane_image_cap_reopens_the_new_image ... ok
#   many_images_are_bounded / decoded_image_memory_is_shared_by_all_tabs ... ok

cargo test --offline --lib
test result: ok. 18 passed; 0 failed; 0 ignored
# navigation_releases_cached_images_and_clears_the_limit ... ok
# image_limit_messages_name_the_limit_and_an_action ... ok

cargo test --offline --test workspace_flow
test result: ok. 1 passed; 0 failed; 0 ignored
cargo test --offline --test document_flow
test result: ok. 1 passed; 0 failed; 0 ignored

# private daemon + two documents + inline PNG + real terminal, patched worker
test tests::real_terminal_and_two_documents_share_workspace ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; finished in 36.42s

# the same acceptance test with the delivered candidate worker binary
# /private/tmp/remuda-final-gui-target/debug/remuda-gui, SHA-256
# 743ef218982eccb092f260869d7fb0e8b7b173ca814253ac885d67bbfee671f1
test tests::real_terminal_and_two_documents_share_workspace ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; finished in 34.64s

```

The large-image test uses two 1000x1000 PNGs (4,000,000 decoded bytes each)
against a 4 MiB workspace budget: loading the second image releases the first
reservation first, the budget returns to zero, and the second image reaches
`Ready`. The pane-cap test fills a tab with 12 missing image paths, confirms
the 8-entry limit, then navigates to a valid PNG and requires it to load with
`image_limited == false`.

The failing base was reproduced from the code path, not from the delivered
binary: `open_document`/`open_path`/`open_selected`/`back` did not call
`release_images`, and `image_limited` was a sticky boolean. `743ef218982e` is
the delivered binary's SHA-256 prefix (built 15:44); the source snapshot
5e6db270 is 16:35, so exact source identity with that binary is not claimed.
The GUI suite and smoke are evidence for this checkout.

## Limitations

- The per-pane 8-image bound and the workspace 32-entry/128 MiB bounds are
  unchanged. A single document with more than 8 images still shows only the
  bounded subset; the new message says to open a document with fewer images.
- Closing another tab releases its claims through pane drop, but an active
  pane whose limit was already set may need a navigation or refresh to retry;
  the message names both actions. Cross-tab LRU eviction is not added.
- Large encoded files above 8 MiB, images above 8192 px or 16M pixels, and
  decoded images above the workspace budget remain refused by the existing
  limits. This change does not add a large-image bypass.
- macOS headless GUI/VFS evidence; no physical-display or Windows/Linux claim.
