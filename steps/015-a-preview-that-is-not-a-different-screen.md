# 015 — a preview that is not a different screen

Three small things the owner asked for after using the TUI: drop the preview's
title band, so that browse and ride show the same screen at the same place, and
prefill the new-session prompt with PowerShell on Windows.

## Before

정수님, after riding the TUI:

**A — a number that can only become wrong.** The preview's title band read
`alpha · 80×24 · live`. That size is the session's, fixed at creation
(`Size` is immutable and `Session` has no `resize` — PRINCIPLES §6), so it does
not track the terminal now looking at it. It is not merely stale after a resize:
it can never change, so the only thing it can do over a session's life is become
false. Showing a number that silently goes wrong is worse than showing nothing.

**B — entering a session looked like arriving somewhere else.** The title band
existed in browse and not in ride, so pressing Enter shifted the session's own
screen up by one row and deleted a line above it. His words for it: *"is this
new? what happened?"* — a preview is supposed to be the same screen, smaller.

Both are one line of code. `tui.rs`'s `render` wrote `preview_title` into the
first row of the preview column, and everything below it started at row 2:

```
remuda · default│alpha · 80×24 · live        ← browse
▸ alpha         │$ echo $((6*7))-landed
  tui           │42-landed

$ echo $((6*7))-landed                       ← ride, same session
42-landed
```

**C — Windows prefills `cmd.exe`.** `daemon::default_shell` was
`SHELL` → `COMSPEC` → `cmd.exe`, and `%COMSPEC%` on Windows always names
`cmd.exe`, so the Windows answer was cmd twice over. PowerShell is what a
Windows user opens.

## Desired outcome

1. The preview pane has no header of its own. Row 1 of the preview column is
   row 1 of the session's screen — the same row the ride puts it on.
2. The crop notice survives that removal. `showing N cols — h/l pans` moves to
   the footer, which is the only band left that is not the session's own screen.
   A crop that reads as absence is the failure this repo keeps re-finding.
3. On Windows the new-session prompt prefills PowerShell. On unix nothing
   changes: `$SHELL` is a person's own choice and this program does not improve
   on it.

`powershell.exe`, not `pwsh.exe`. Windows PowerShell 5.1 is in-box on every
supported Windows; PowerShell 7 is a separate install. The failure being avoided
is a prompt prefilled with a binary that is not on the machine, and `pwsh` is
exactly that failure on a default install — worse than the `cmd.exe` it replaces.
`$SHELL` still wins where it is set, so a Windows user in an environment that
sets it (git-bash) is unaffected.

**Out of scope, deliberately.** The owner also raised making the session list
permanently visible with a focus model instead of browse→Enter→ride. That is a
modality change awaiting his go-ahead and nothing here anticipates it: the two
modes, the keys, and `Ui::on_key` are untouched.

## Expected

- `render` draws `body = rows - 1` preview lines starting at terminal row 1; the
  left column keeps its `remuda · <server>` header, so the list loses no slot.
- Browse and ride, captured from the same session with nothing else changed,
  show the same first line at the same row.
- A preview wider than its pane still says so — in the footer now — and every
  cut row still carries `→`.
- The size is not deleted from the product, only from a band that cannot keep it
  true: `remuda ls` still prints it, where it is asked for rather than implied.
- `shell_or_default(None)` is `powershell.exe` on Windows and `sh` on unix;
  a configured shell is returned untouched on both.
- On `windows-latest` the prefilled shell is **run**, not spelled: the test
  spawns it and asks for `6*7`. A string comparison would pass for a binary that
  does not exist, which is the whole defect.

## Actual

The TUI was driven by remuda itself — a session running the built binary, keys
sent with `remuda.key`, screens read back with `capture` — the technique from
`steps/012` and `steps/013`. `REMUDA_RUNTIME_DIR` isolates both daemons from the
one in live use on this machine. `42-landed` rather than an echo of the
question, per PRINCIPLES §4.

**Before**, `origin/main` at `5f8a805`, built into a separate target directory
and run against its own daemon:

```
=== before: browse — the preview pane
remuda · default│alpha · 80×24 · live
▸ alpha         │$ echo $((6*7))-landed
  tui           │42-landed
                │$
                │
…
↑↓ select   ⏎ ride   n new   x kill   q quit

=== before: ride — the zoomed view of the same session
$ echo $((6*7))-landed
42-landed
$
```

The first screen row is `alpha · 80×24 · live` in one and
`$ echo $((6*7))-landed` in the other. That is defect B, and the number in it is
defect A.

**After**, this branch, the identical script:

```
=== after: browse — the preview pane
remuda · default│$ echo $((6*7))-landed
▸ alpha         │42-landed
  tui           │$
                │
…
↑↓ select   ⏎ ride   n new   x kill   q quit

=== after: ride — the zoomed view of the same session
$ echo $((6*7))-landed
42-landed
$
```

Same content, same rows, and the line that was only in one of them is gone.

**A preview wider than its pane** — an 80-column session in a 63-column pane.
The notice is in the footer and the cut rows still admit the cut:

```
=== a preview wider than its pane
remuda · default│$ printf "%s" $(seq 1 40 | tr -d "\n") ; echo
▸ alpha         │12345678910111213141516171819202122232425262728293031323334353→
  tui           │$
…
↑↓ select   ⏎ ride   n new   x kill   showing 63 cols — h/l pans   q quit
```

**The size, where it is still asked for:**

```
$ remuda ls
alpha                  80x24   live  idle 1s
```

**The shell decision**, as unit tests. The Windows one is the measurement that
matters and it runs on `windows-latest`, not here:

```
$ cargo test -p remuda-native --lib
test daemon::tests::a_shell_the_person_already_chose_is_never_second_guessed ... ok
test daemon::tests::unix_with_no_shell_set_falls_back_to_sh ... ok
test tui::tests::the_preview_starts_at_the_sessions_own_first_row ... ok
test result: ok. 45 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

**The suite and the gates**, on this branch:

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result"
test result: ok. 0 passed;   ok. 21 passed;  ok. 10 passed;  ok. 5 passed;
test result: ok. 45 passed;  ok. 0 passed;   ok. 9 passed;   ok. 8 passed;
test result: ok. 7 passed;   ok. 4 passed;
$ cargo fmt --all --check                                   (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings      Finished
$ python3 scripts/check-comments.py     ok — 178 doc comment(s) within cap
$ python3 scripts/check-steps.py        ok — 14 step(s)
$ python3 scripts/check-principles.py   ok — 14 principles
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered
```

No CI gate was added, so `gates-can-fail` gains no step — PRINCIPLES §2 is
satisfied by there being nothing new to control. Its existing plants are
untouched: the Lua-rename plant targets `native/src/script.rs`'s `"insert"`,
which this change does not go near.

## Not verified

- **The TUI under a human's hands**, still. Every frame above came out of a pty
  this process owns, driven by synthetic keys. Nobody has watched the preview
  repaint while holding an arrow key.
- **Anything drawn on a Windows console.** `test-windows` proves the prefilled
  shell exists and runs there; it does not open the TUI, which has still never
  been drawn on a console host.
- **A Windows machine where PowerShell is absent.** Nano Server and stripped
  images exist. The claim proven is "present on the CI runner", and the reason
  for choosing 5.1 over 7 is a documented property of Windows rather than a
  measurement made here.
- **`pwsh` when it *is* installed.** A user with PowerShell 7 gets 5.1
  prefilled and must edit one word. Preferring `pwsh` when it is on `PATH` is a
  probe and a fallback for a keystroke; not worth it until someone asks.
- **The preview's top row when the session's screen is taller than the pane.**
  `crop` still anchors bottom-left, so a full 24-row grid previewed in 23 rows
  loses its top line — where a ride shows all of it. That is the pane being
  smaller, not a header, and it is unchanged by this step.
