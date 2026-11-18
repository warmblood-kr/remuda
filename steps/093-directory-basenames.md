# 093 — Directory basenames and the current VFS path

## Before

A directory pane repeated the full path in every row (`docs/guide/note.md`),
which is noisy and pushes the file name out of the narrow panel. The pane's
top line was the generic status text (`ready · docs/`), not an explicit VFS
location. The parent row from 091 already showed `..`.

## Desired outcome

Each directory row shows only the file or folder name, and the pane shows the
current directory path at the top. Selection, opening, stale-generation checks
and parent navigation keep the full VFS path; only the display is compact.

## Expected

- `Entry::display_name()` returns the label override (`..`) or the escaped last
  path component; `Entry::display_label()` keeps the full escaped path for the
  TUI listing.
- The GUI directory header (`vfs-location-left/right`) shows
  `remuda:///<current path>`, including the root (`remuda:///`), a trailing
  slash for directories and escaped Unicode/control characters.
- Click validation still compares the captured full `Entry::path` and
  `directory` flag against the current listing, and `open_selected`/history
  still navigate by that full path.
- `basename_label` is shared and unit-tested for root, trailing slash, nested
  paths, Unicode, escaped control characters and a backslash.
- A real-daemon test enters `a/` and `b/` with the same `note.md` basename and
  reaches `a/note.md` by mouse and `b/note.md` by Enter; a Unicode folder shows
  its path in the header and keeps its long row inside the pane.

## Actual

Verified on the stage-B commit: full GUI bin suite 158 passed / 0 failed /
8 ignored; native `documents::tests` 19 passed; targeted document, link, image
and focus filters pass.
