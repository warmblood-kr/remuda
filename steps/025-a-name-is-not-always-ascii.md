# 025 — a name is not always ASCII

`steps/023` fixed CJK display width in the preview pane's styled-cell grid,
and left the session-list column's own width math untouched, naming it as
the same latent bug class if a session were ever named with wide
characters. This step checks that claim against the actual code instead of
leaving it as a guess, and fixes what it finds.

## Before

`native/src/tui.rs`'s `fit(text, width)` — used to pad or truncate the list
column's session-name field, the list column's whole row, and the footer —
measured width by `text.chars().count()`, not real display width.
`visible_width` (steps/023) already carried a comment saying `fit`'s plain
char count would under-count a wide character by 1; nothing had checked
whether that under-count was actually reachable.

It is. `Registry`'s auto-naming (`slug()`, `core/src/registry.rs`) folds
every non-`[a-z0-9_-]` character to `-`, so a session named by the TUI's own
`start()` action is always ASCII — but `Request::New`'s `name` field is
`Option<String>`, and when `Some`, `native/src/daemon.rs`'s handler uses it
verbatim, with no folding. Two real, unrestricted paths construct it that
way: `native/src/mcp.rs`'s `new` tool takes `args.get("name")` straight from
whatever an MCP client sends, and `native/src/script.rs` names an image's
session from the Lua script's own file path, which can contain non-ASCII
characters (a Korean-named folder, for instance). A session named this way
reaches the live list column through `render_styled` → `list_row` → `fit`.

## Desired outcome

1. `fit` measures and paces using real display width (`visible_width`,
   already in this file since `steps/023`), matching the approach already
   used for the preview pane, consistently.
2. `list_row`'s own `room` calculation, which sizes the name field against
   the visible tail text, uses the same real-width measurement.
3. A test exercises `list_row` itself — what `render_styled` actually calls
   — with a wide-charactered session name, proving the row still occupies
   exactly its column budget rather than overflowing it.

## Expected

- `tui::tests::list_row_with_a_wide_session_name_still_fits_its_column_budget`:
  a session named `안녕하세요` (5 East-Asian-Wide syllables) at a 20-column
  budget produces a row whose real display width (`visible_width`) is
  exactly 20 — not 25, which is what the old `char`-counting code produced
  (verified by hand-reverting the fix locally and re-running this same test:
  it fails with `left: 25, right: 20` on the old code).
- The full existing suite passes unchanged.

## Actual

```
$ cargo test -p remuda-native --lib tui::tests::list_row_with_a_wide_session_name_still_fits_its_column_budget -- --nocapture

running 1 test
test tui::tests::list_row_with_a_wide_session_name_still_fits_its_column_budget ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 68 filtered out; finished in 0.00s
```

Confirmed the test actually catches the bug before trusting it: reverted
`fit` to the old `char`-counting body locally (keeping the new test), reran:

```
thread 'tui::tests::list_row_with_a_wide_session_name_still_fits_its_column_budget' panicked:
assertion `left == right` failed: a wide-named session must still occupy exactly 20 columns: "▸ 안녕하세요             "
  left: 25
 right: 20
```

Full suite and gates on this branch:

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result"
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 69 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.09s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s

$ cargo fmt --all --check                                    (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings       Finished, no warnings
$ cargo check -p remuda-core --target wasm32-unknown-unknown  Finished, clean
$ python3 scripts/check-comments.py     ok — 250 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-steps.py        ok — 25 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-principles.py   ok — 14 principles, every named mechanism exists (17 CI jobs, 11 denied paths, 255 test fns seen)
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse, 17 job(s) defined
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu
```

No CI gate was added, no new denied path.

## Not verified

- **Whether any real session has ever actually been named with a wide
  character.** The paths that make it possible (MCP's `new` tool, a
  non-ASCII Lua script path) are real and unrestricted by construction, not
  hypothetical, but no report names an instance of this actually happening.
- **Windows.** No Windows host is available; nothing here has run there.
- **Ambiguous-width Unicode** (`steps/023`'s open question) — out of scope
  for this step, untouched.
