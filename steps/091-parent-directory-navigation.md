# 091 — Move to the parent directory from a listing

## Before

A document pane is a one-directory flat listing: `Browser.page.entries` holds the
immediate children of the current VFS directory, and clicking one navigates into
it. There is no expandable tree, so a separate parent row is not a duplicate of
any tree control.

`Back` is history, not a parent move. Opening `docs/guide/` from a document link
and pressing Back returns to that document. Going one directory up had no
control at all, so a user who entered a deep folder had to retrace links or go
back to the root and descend again.

The VFS root is the empty path. `Browser::navigate` validates every target with
`vfs::validate_path`, but no UI asked it for a parent, so there was nothing to
keep inside the root either.

## Desired outcome

A non-root directory listing shows a `..` row at the top. Clicking it, or
selecting it with the keyboard and pressing Enter, opens the lexical parent.
At the root there is no row. Navigation stays inside the VFS, document render,
image, selection, scroll and focus behavior are unchanged, and the TUI can opt
into the same row without another implementation.

## Expected

- `parent_directory` strips one path component: `docs/` → root (`""`),
  `docs/guide/` → `docs/`, `a/b/c/` → `a/b/`; `""` and `/` have no parent.
- The parent row is a normal `Entry` (real parent path, `directory = true`) with
  the display label `..`, prepended after a listing arrives. Selection,
  arrow keys, Enter, click validation, scroll-to-selection and history
  therefore use the existing paths with no second cursor.
- The first real entry stays selected after entering a directory, so Enter
  still opens the row the user already expects; Up then Enter reaches `..`.
- The row is injected only when parent navigation is enabled and the page
  does not already carry one, so a restored history page keeps its layout.
- `Entry::display_label` is shared by the GUI list and the TUI frame; the TUI
  therefore needs no renderer change if it later enables the row.
- A parent target is re-validated by `navigate`; at the root no target exists.

## Actual

Source-only until the build slot runs the tests. The GUI `Pane` enables the
option; the TUI leaves it off so its verified listing shape is unchanged in
this candidate.

New tests:

- `documents::tests::parent_directory_stops_at_the_root` (native unit)
- `document_workspace_view::tests::parent_row_moves_up_one_level_and_stops_at_root`
  (root has no row, docs shows `..` with the first real entry selected, click
  goes to root, keyboard Up+Enter goes `docs/guide/` → `docs/`, second `..`
  stops at the root)
- the existing `vfs_long_listing_scroll_click_and_history` fixture now opens
  `inside.md` at index 1 because index 0 is the parent row.
