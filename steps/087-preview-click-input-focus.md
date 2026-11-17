# 087 — Preview click focuses guarded input

## Before

The read-only preview grid owns `preview_focus` for keyboard copy and drag
selection. A click on the current peer's preview therefore left `input_focus`
unfocused: the next key tried to act on the preview instead of reaching the
guarded peer line input. The user had to find and click the narrow line-input
field before typing.

## Desired outcome

A plain click on a current, still-valid preview moves keyboard focus to that
selection's line input without attaching, sending input or creating a live
helper. Dragging keeps the preview focus and the existing copy selection.

## Expected

- Mouse-down records the exact `Selection` and gesture start, and clears the
  dragged flag. Pointer travel of `DRAG_THRESHOLD_PX` marks the gesture as a
  drag, matching the existing selection threshold.
- On release a non-drag click focuses `input_focus` only when the pressed
  `Selection` still equals the current selection and that selection still
  passes `selection_current`. A target changed between press and release, a
  replacement, a stale/revoked/unadmitted peer, or a missing press does not
  take focus or input.
- The text-only fallback (`preview-text`, ABI 1/legacy or an over-limit grid)
  tracks the same preview focus handle and gets the same press/release check
  and click-to-input behavior. Its mouse-down takes focus from the line input,
  so a text drag followed by a key does not type into the previously focused
  input.
- No daemon request is added and no daemon/protocol shape changes. This works
  against the existing daemon and the M1 candidate.
- Document/Markdown/image browsing and the existing preview drag/copy behavior
  are unchanged.

## Actual

```text
# macOS, existing shared target reused, no new target directory
cargo test --offline --bin remuda-gui -- --test-threads=1
test result: ok. 136 passed; 0 failed; 8 ignored; 0 measured; finished in 76.22s
# includes:
#   preview_click_requires_the_same_press_and_release_selection ... ok
#   preview_click_allows_typing_in_selected_peer_line_input ... ok
#   preview_release_after_replacement_does_not_focus_the_new_target ... ok
#   preview_release_after_peer_revocation_does_not_focus_input ... ok
#   text_only_preview_click_focuses_line_input ... ok
#   text_preview_release_after_replacement_does_not_focus_the_new_target ... ok
#   text_preview_drag_does_not_focus_input ... ok
#   text_preview_drag_takes_focus_from_line_input_and_does_not_type ... ok

cargo test --offline --test workspace_flow
test result: ok. 1 passed; 0 failed; 0 ignored
cargo test --offline --test document_flow
test result: ok. 1 passed; 0 failed; 0 ignored

# private daemon + two documents + inline PNG + real terminal, patched worker
test tests::real_terminal_and_two_documents_share_workspace ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; finished in 34.56s

# the same acceptance test with the delivered candidate worker binary
# /private/tmp/remuda-final-gui-target/debug/remuda-gui, SHA-256
# 743ef218982eccb092f260869d7fb0e8b7b173ca814253ac885d67bbfee671f1
test tests::real_terminal_and_two_documents_share_workspace ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; finished in 34.56s

# isolated two-node loopback peer smoke
PASS: GUI node/session clicks, live output and guarded input reached isolated peer;
wrong instance, unadmitted and revoked peer refused for viewing and input
```

One setup-only run first failed 26 document-workspace tests because the base
test helper hardcodes `gui/../target/debug/remuda`; only the build target alias
was missing. A symlink from that path to the already-built shared target fixed
the fixture, and the rerun above passed. No product assertion failed in that
setup run.

Source provenance: 743ef218982e is the SHA-256 prefix of the delivered binary
(build `0.1.0+git.56f99a02ede3-dirty`, mtime 15:44), not a git commit. The
recorded staging snapshot `5e6db270` was made at 16:35 and its tree matches the
current `experience-po/gui-unified` changed files, but exact source identity
with the 15:44 binary is NOT proven. This patch is based on `5e6db270` and is
verified as a behavior change; it does not claim bitwise equivalence to the
binary. The candidate binary was retained and exercised as the acceptance
worker above.

## Limitations

- A click focuses the line input; it does not attach or send a key. The user
  still types explicitly.
- The peer must still be admitted and its instance current at release time; a
  stale preview remains read-only.
- The delivered candidate's exact source revision is unproven (binary SHA prefix
  743ef218982e, built 15:44; snapshot 5e6db270 is 16:35). The candidate worker
  passed the same two-document/image acceptance test, but the full candidate
  GUI code was not rebuilt or physically exercised.
- macOS-only assembled evidence; no physical-display or Windows/Linux claim.
- The GUI full-suite rerun used the existing target through a temporary path
  alias because the base test helper hardcodes its target path; no new target
  directory was created, and the alias was removed after verification.
