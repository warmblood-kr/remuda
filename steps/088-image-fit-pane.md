# 088 — Images fit the document pane width

## Before

Standalone images used `max_w(680)` / `max_h(480)` and Markdown inline images
used `max_w(240)` / `max_h(180)`. GPUI keeps the decoded intrinsic width when
the max is larger than the pane, so a 480×300 image inside a 296 px document
pane rendered 480 px wide and clipped roughly 38% horizontally. The framebuffer
review captured this after the e511 image candidate.

## Desired outcome

A decoded image is shown at its natural size when it fits, and otherwise scaled
down to the available pane width with its aspect ratio preserved. Resizing the
pane or document panel adjusts the display. Vertical overflow scrolls; the
existing decode, decoded-memory, encoded-size, pixel and VFS bounds are
unchanged. Standalone and Markdown inline paths behave the same way, except that
inline images keep their existing 240 px display cap.

## Expected

- Display width is `min(natural width, pane width, display cap)`; height is
  derived from the decoded aspect ratio. A 480×300 image in a 296 px content
  area is 296×185, not 480×300.
- An image smaller than the pane is not upscaled (a 3×2 image stays 3×2).
- The standalone cap remains 680 px and the Markdown inline cap remains 240 px,
  so a wide pane does not enlarge an image beyond the earlier limits.
- The pane width is derived from the panel width minus root padding, split gap
  and scrollbar allowance; resizing re-runs the same computation. A narrow
  split has no artificial 80 px floor: the product's 160 px minimum panel
  yields 52 px for each pane, below the space actually available. A tall
  image in that pane scrolls vertically.
- Markdown list items subtract their depth indent and 24 px marker column;
  quotes subtract their 8 px padding. Nested list and quote images therefore
  fit the real content width instead of the outer pane width.
- No image bytes are re-decoded for a resize; only GPUI layout changes.
- Image budget, retry, navigation-release, selection, focus, scrolling and
  document behavior are untouched.

## Actual

VERIFIED in the combined focus+image worktree at source `89bd7b1e` (this
evidence-only paragraph does not change code). The first full run exposed two
test-fixture mistakes: the selector div stretched to the pane while the image
child was fitted, so its bounds did not describe the image. The standalone
outer div now carries the fitted width/height too, and both layout tests pass.

```text
cargo test --bin remuda-gui -- --test-threads=1
test result: ok. 142 passed; 0 failed; 8 ignored

# includes:
# document_workspace_view::tests::images_fit_the_pane_width_in_standalone_and_markdown_paths ... ok
# document_workspace_view::tests::split_images_fit_each_pane_at_the_narrowest_width ... ok
# and the eight preview-click/focus guard tests from f2f0bd50 ... ok

cargo test --lib (gui)
test result: ok. 31 passed; 0 failed; 0 ignored

workspace_flow 1 passed; document_flow 1 passed
native documents::tests 16 passed; 0 failed

# private daemon + two documents + inline PNG + real terminal
# combined worker: 1 passed (36.51s)
# candidate 743 worker: 1 passed (35.08s)
```

The final binary's SHA-256 and build id are in the delivery report. The 520
viewport test exercises the 260 panel / 102 px content split. The product's
160 px panel is reachable (minimum document fraction or narrow mode), so a
source-only follow-up adds `split_images_fit_and_scroll_at_the_160px_product_panel`
with real GPUI bounds and vertical scroll; it is not yet compiled or run. The
model assertion `pane_content_width(160, true) == 52` also remains. The framebuffer evidence
remains a forced fixture; physical M1 and the Markdown inline path were checked
in the assembled GPUI TestWindow, not on a physical display.

## Limitations

- No click-to-zoom or full-screen viewer is added; the image is fitted, not
  resampled beyond display.
- The standalone 680 px and inline 240 px caps remain; a very wide pane still
  shows at most those widths.
- Very tall images fit horizontally and scroll vertically rather than being
  height-clamped.
