# 088 — Acknowledge only a frame containing the document body

## Before

ci-work independently reproduced a false rendered receipt at three terminal
rows on 1a193263. The receipt gate used rows >= 3, while Browser reserved three
rows for header/status/help. The actual PTY showed no document body.

## Desired outcome

Keep the request received while a three-row terminal cannot show its body.
Resize that same PTY to four rows, display the requested document body, then
acknowledge rendered with the same instance/sequence/URI. Preserve the normal
direct-open path and session draft behavior.

## Expected

Use the actual final frame passed to successful terminal presentation, rather
than a second estimate of available rows. Browser::body_in_frame uses the same
body_room layout calculation as Browser::lines and inspects only body rows,
excluding header, status and help. Address inspection, zero width, blank rows
or text with no display width do not provide body evidence. Existing narrow
glyph fallback '?' is displayed body content. A blank document/blank viewport
remains received until a body glyph is visible; no false completion is emitted.
The existing matching-generation, successful-load and write/flush gates remain.

## Actual

```text
Before fix, actual private daemon + three-row PTY:
direct_open_waits_for_body_after_three_to_four_row_resize ... FAILED
assertion failed: false presentation acknowledgement
left: String("rendered")
right: "received"
test result: FAILED. 0 passed; 1 failed; finished in 3.19s

After fix, same actual daemon/PTY scenario:
direct_open_waits_for_body_after_three_to_four_row_resize ... ok
test result: ok. 1 passed; 0 failed; finished in 5.37s
Full private-daemon/PTY cluster_tui suite:
test result: ok. 11 passed; 0 failed; finished in 48.67s
documents:: unit suite:
test result: ok. 11 passed; 0 failed; finished in 0.08s
cargo clippy --offline --workspace --all-targets -- -D warnings: passed
fmt, diff, comments, steps and principles checks: passed
```

The regression holds a real master PTY, checks received and absent ACTUAL_BODY
for two seconds at three rows, resizes to four rows, observes ACTUAL_BODY, then
checks rendered and the original instance/sequence/URI. It uses the real VFS
and document worker. The original normal direct-open/draft test remains.
Unit coverage checks 0/3/4 rows, zero/narrow/normal width, address inspection
and blank body. An initial unit expectation that a wide glyph was invisible
at width one was corrected after observing the existing '?' fallback.
No deployment or shared daemon/user screen changes were made.
