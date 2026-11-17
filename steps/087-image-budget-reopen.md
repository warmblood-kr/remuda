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

- Navigation is detected by the browser load generation, not by the stale
  `Browser::back` root flag. `open_document`, `open_path` (files and
  directories), `open_selected` and a history-moving `back` release the pane's
  images and reset the limit after a real location change. Invalid paths,
  blocked links, loading no-ops and a cancelled address inspection preserve
  the current images. Same-path re-entry still starts a new load and releases
  the old cache.
- The active document does not evict images it is still displaying. A split
  tab or another workspace tab is independent and keeps its own reservations.
- The pane cap reports `this tab already holds 8 images; open a document with
  fewer images`. The workspace entry cap reports the 32-entry cache and says
  to close another image tab or refresh this document. Decoded-memory refusal
  names the 128 MiB workspace budget and the same action.
- A failed entry is released when the user leaves the document, so navigating
  back can retry it. Within one document the existing 8-entry bound still
  applies; a page with more images stays bounded rather than looping.
- While any pane in the workspace has a stale flight, `pump_images` in every
  pane holds new decodes. A worker already decoding when its tab becomes stale
  finishes without waiting and returns `Retry` instead of keeping a
  reservation. A worker that starts while a stale reservation is still
  pending exits before decoding. The read, the reservation decision and
  dropping a refused buffer all happen under the decode lock, so the
  one-decoded-buffer peak is unchanged; only the result send and retry
  queuing happen outside it.
- An exit-before-decode Retry carries no bytes and does not consume the one
  transient memory retry. A Retry seen while a stale reservation exists is
  requeued as a state transition (pump stays gated, so it cannot
  busy-loop), and it also does not consume that retry. After the stale reservations drain, the
  image starts again. If it still fails with no stale reservation and the
  bytes do not fit, the exact 128 MiB failure is final for this document.
  A late stale reservation is dropped when its result is drained, so a stale
  worker never waits for its own stale count or blocks a started decode.
- `refresh` also compares the browser generation: refreshing while address
  inspection is active is a no-op and preserves the current images.
- No PNG/JPEG bound or VFS transfer bound changes. The image worker still
  serializes decodes and reserves decoded RGBA against the workspace budget;
  the reservation is taken before the decode lock is released.

## Actual

Runs on 2026-11-17/18, macOS aarch64, headless GPUI `TestWindow` (no physical
display), shared target reused with `CARGO_INCREMENTAL=0`.

```text
cargo test --lib (gui)
test result: ok. 28 passed; 0 failed; 0 ignored

cargo test --bin remuda-gui -- --test-threads=1
test result: ok. 131 passed; 0 failed; 8 ignored; finished in 77.84s

cargo test --test workspace_flow
test result: ok. 1 passed; 0 failed; 0 ignored
cargo test --test document_flow
test result: ok. 1 passed; 0 failed; 0 ignored

cargo test -p remuda-native --lib documents::tests
test result: ok. 16 passed; 0 failed; 0 ignored

# The whole native lib also ran: 1932 passed; 25 failed; 8 ignored.
# All 25 failures are tui::tui_unit::* / tui::visual_mode_tests::* with
# session/window fixture assertions ("New answers with the name it gave the
# session", etc.), outside the changed files. The documents module passed;
# this slice does not claim the whole native suite is green.

# Targeted tests, all ok:
# no_op_navigation_and_cancelled_address_keep_the_current_images
# history_back_releases_the_previous_document_images
# stale_flight_blocks_new_decode_until_it_is_polled
# stale_flight_in_one_tab_blocks_decodes_in_another_tab
# retry_result_is_requeued_and_admitted_after_stale_clears
# started_worker_returns_retry_without_waiting_for_its_own_stale_count
# stale_worker_exits_before_decoding
# reservation_is_taken_under_the_decode_lock_before_the_next_read
# stale_reservation_is_released_when_its_result_is_polled
# pane_drop_releases_a_buffered_reservation_and_the_stale_gate
# image_limit_messages_name_the_limit_and_an_action
# navigation_in_one_tab_keeps_the_other_tabs_image
# navigating_to_a_second_large_image_releases_the_first_reservation
# navigating_after_the_pane_image_cap_reopens_the_new_image
# decoded_image_memory_is_shared_by_all_tabs

# private daemon + two documents + inline PNG + real terminal, patched worker
test tests::real_terminal_and_two_documents_share_workspace ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; finished in 34.78s

# the same acceptance test against the delivered candidate worker binary
# /private/tmp/remuda-final-gui-target/debug/remuda-gui, SHA-256
# 743ef218982eccb092f260869d7fb0e8b7b173ca814253ac885d67bbfee671f1
test tests::real_terminal_and_two_documents_share_workspace ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; finished in 34.76s
```

The delivered image-budget binary's SHA-256 and internal build id are in the
delivery report. The two review blockers were reproduced as failing tests and
then fixed: the first was an assertion that dropped the reservations it was
counting; the second was a closed-pane flight that could hold bytes after the
stale gate cleared. The worker now sends its result under the decode lock and
pane drop releases flights/reservations before the stale gate.

Source provenance: 743ef218982e is the delivered candidate binary's SHA-256
prefix (build 0.1.0+git.56f99a02ede3-dirty, mtime 15:44), not a git commit.
The recorded staging snapshot 5e6db270 is 16:35, so exact source identity with
the 15:44 binary is not claimed.

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
- Back restores a historical page and then reloads its images from the current
  VFS version; historical image bytes are not guaranteed to be identical.
- The full native lib suite has 25 pre-existing TUI/session-harness failures
  outside this change; only the documents module and the GUI suites are
  claimed here.
- macOS headless GUI/VFS evidence; no physical-display or Windows/Linux claim.
