# 016 — a list that stays, and a session that closes itself

Three things the owner asked for after living with the TUI: the herd stays on
screen while you work in a session, a session that ends goes away by itself, and
an empty herd invites instead of putting a shell path in a prompt you did not
open.

## Before

**A — the TUI is modal, and the mode is the whole screen.** `steps/012` shipped
browse (a list beside a read-only preview) and ride (`client::attach`, the
session owning the terminal). Enter left one for the other. 정수님 wants the
shape tmux gave him: the list *stays*, and the selected session is live beside
it.

The obstacle he raised himself is that the two modes have incompatible
keyboards. In the list `n`, `x` and `q` are commands; inside a shell they must
be plain text, and typing `x` must never kill anything. His worry about the tmux
answer — a prefix key — is that a press-then-press sequence gets mangled
crossing several nested TTYs, which is exactly what a fleet of agents inside
agents is.

**B — a session that ends stays listed forever.** `core/src/protocol.rs`'s
`Close` said *"Death alone does not remove: an exited session stays listed until
this"*, and `bin/remuda.rs:281` gave the reason: the last screen is the evidence
for why it died. That is a real reason, and it is not what he wants by default:

> when the shell inside exits or the terminal closes, the default should be that
> the entry goes away by itself.

`Registry::reap()` — "drop every session whose process has exited" — has existed
since the first slice and was called by nothing.

**C — the first screen prefills a shell path into a prompt nobody opened.**
`Ui::new` opened `Mode::Prompt(shell)` on an empty herd. He ran remuda for the
first time, found `start: /bin/zsh` sitting at the bottom, and could not place
it — *because afterwards that prompt only appears when you press `n`*. The
comment defending it names the failure it was avoiding, and that reason still
holds: an empty screen that says nothing, or a shell spawned silently, is half
of the incident `steps/012` is named after.

## Desired outcome

1. **One screen, and focus moves.** The list is always on the left, the selected
   session always on the right. `Enter` points the keyboard at the session;
   `Ctrl-\` points it back. With focus on the session every key including `x`
   and `q` is typed into the pty and nothing else.

   No prefix key, and not because one would be hard: **a prefix exists to open a
   namespace of commands, and by the owner's own no-panes ruling there is
   exactly one command from inside a session.** One command needs one key. One
   keypress is also one byte, so it carries none of the inter-key timing that is
   what actually breaks prefix sequences across nested ttys.

   `Ctrl-\` and not a new key: it is already `client::DETACH`, already what
   `remuda attach` leaves by, and already has a test named for it.

2. **Focus is drawn, never remembered.** The border between the panes is heavy
   and reversed when the session has the keyboard and thin when the list does,
   and the footer names it in words. This is the improvement over tmux, whose
   prefix state is invisible by construction.

3. **Focus is exclusive, not a peek.** It takes the same `Attach` guard a ride
   takes, so orchestrated input is refused while a person types — PRINCIPLES §6
   invariant 3, which is otherwise engaged by `remuda attach` and would be blind
   in the mode people now spend their time in.

4. **A session that ends closes itself.** `List` reaps, so an exited session
   stops being listed at the moment the answer is next looked at.
   `REMUDA_KEEP_EXITED=1` in the daemon's environment keeps it. The middle
   option — auto-close only on exit status 0 — was offered and he chose the
   unconditional close plus a setting, so the evidence-keeping rationale
   survives as the opt-out rather than as the default.

5. **An empty herd invites.** It says what it is and what to press, opens no
   prompt on the user's behalf, and still spawns nothing on its own. `n` opens
   the prompt prefilled — which is also what makes the prompt the same wherever
   it came from, and that sameness is the half of his complaint that the word
   "confusing" was pointing at.

**Where the setting lives, and why.** An environment variable read by the
daemon, not a config file: this program has four already (`REMUDA_RUNTIME_DIR`,
`REMUDA_NO_UPDATE_CHECK`, `REMUDA_CHANNEL`, `REMUDA_INSTALL_DIR`) and no config
file at all, and one more knob is not the moment to invent a format, a search
path and a precedence order. It is read in the **daemon** because the daemon
owns the sessions and outlives every client; the cost is that changing it takes
a `remuda restart`, which is the same cost `REMUDA_RUNTIME_DIR` already has.

**And a size decision that ⑴ forces.** A session started from the TUI was sized
to the whole terminal, defended by a comment saying the preview is transient and
the pty's size is permanent. That was true while a ride existed to show it at
full width. Now the pane *is* where it lives for life, and nothing can resize a
pty (PRINCIPLES §6, invariant 2) — so sizing it to the terminal would guarantee
a crop that the list can never give back. It is sized to the pane, and
`Size::new`'s 80×24 floor still applies.

## Expected

- With focus on the session, `x`, `q`, `n` and `y` arrive at the shell as the
  characters `xqny`: no kill confirmation, no quit, no new-session prompt.
- `Ctrl-\` returns focus and those same keys are commands again.
- The detach key is recognised however crossterm spells it. ⚠ crossterm 0.29
  reports the byte 0x1C as `C-4`, because a terminal sends that byte for Ctrl-\
  and Ctrl-4 alike — accepting only `C-\` would leave the key dead on unix.
- While focused the session shows as `attached`, and `remuda send` into it is
  refused with *a human is attached to this session* and leaves no bytes behind.
- If the viewer dies while focused, the guard dies with it — the step 012
  deadlock shape, checked rather than assumed.
- `sh`, then `exit`: the session disappears from the list, focus lands back on
  the list, and the empty-herd invitation is what is on screen.
- The same with `REMUDA_KEEP_EXITED=1`: the session is listed `dead` and its
  last screen is still readable.
- `remuda run sh` then `exit` says which of those two happened — asked of the
  daemon, since only the daemon knows how it was started.
- An empty herd shows `the herd is empty.` and `press n to start a session.`,
  and contains no `start: ` prompt.
- A session started from the prompt fits its pane exactly, and the frame after
  it exists does not crop it.

## Actual

The TUI was driven by remuda itself — a session running the built binary, keys
sent with `remuda.key`/`remuda.insert`, screens read back with `capture` — the
technique from `steps/012`, `013` and `015`. `REMUDA_RUNTIME_DIR=/tmp/rmv`
isolated every daemon here from anything else on the machine, and the driving
daemon runs inside a 120×30 pty so the TUI has a real terminal of that size.
Every block below is one section of one scripted run, pasted in order; trailing
spaces are stripped and blank rows are elided where marked, nothing else.

**1 — an empty herd, first run.** It says what it is, says what to press, and
has opened nothing:

```
remuda · subj                           │
                                        │
  the herd is empty.                    │
                                        │
  press n to start a session.           │
                                        │
                       … 23 blank rows …
n new   q quit
```

**2 — `n` opens the prompt**, and that is where the shell path lives now:

```
                                        │
start: /usr/bin/zsh▏   ⏎ run · esc cancel
```

**3 — typed over with `sh`, and run.** The session is 80×29: the pane is 79
wide and `Size::new` floors it at 80, so the frame that follows gives it an
80-column preview and the footer carries no crop notice:

```
sh                     80x29   live  idle 1s

remuda · subj                          │$
▸ sh                          live   1s│
                       … 27 blank rows …
↑↓ select   ⏎ enter   n new   x kill   q quit
```

**4 — `Enter` moves the keyboard**, and the screen says so twice: the border
goes heavy (and reversed, which a text capture cannot show) and the footer names
the session. The list has not gone anywhere:

```
remuda · subj                          ┃$
▸ sh                          live ⚑ 2s┃
                                       ┃
▶ sh — every key goes to the session   ctrl-\ back to the list
```

**5 — the test that matters: `x`, `q`, `n`, `y` with focus on the session.**
They land on the shell's input line as text. Nothing was killed, no confirmation
was asked, no prompt opened:

```
remuda · subj                          ┃$ xqny
▸ sh                          live ⚑ 2s┃
▶ sh — every key goes to the session   ctrl-\ back to the list

sh                     80x29   live  idle 2s    attached
```

**6 — and the shell really runs what it is sent.** `42-typed` cannot appear in
the echo of the question (PRINCIPLES §4):

```
remuda · subj                          ┃$ echo $((6*7))-typed
▸ sh                          live ⚑ 4s┃42-typed
                                       ┃$
```

**7 — `Ctrl-\`, the byte 0x1C, comes back.** The border thins, the footer is the
list's again, the `⚑` clears and the session is no longer `attached`:

```
remuda · subj                          │$ echo $((6*7))-typed
▸ sh                          live   4s│42-typed
                                       │$
↑↓ select   ⏎ enter   n new   x kill   q quit

sh                     80x29   live  idle 4s
```

**8 — `x` is a command again, and `n` cancels it** — the same two keys that were
text a moment ago:

```
kill sh? it is running — y / n
↑↓ select   ⏎ enter   n new   x kill   q quit
sh                     80x29   live  idle 5s
```

**9 — orchestrated input is refused while a person types.** Focus holds the same
guard a ride does, so this is PRINCIPLES §6 invariant 3 doing its job in the mode
people now live in — and the refusal left nothing on the screen:

```
sh                     80x29   live  idle 6s    attached
$ remuda -s subj send sh "echo LEAKED"
remuda: a human is attached to this session
exit=1
$ remuda -s subj -e 'print(remuda.capture("sh"))'
$ echo $((6*7))-typed
42-typed
$
```

**10 — `exit`: the session closes itself, and the keyboard comes back** because
there is nothing left to type into:

```
remuda · subj                           │
                                        │
  the herd is empty.                    │
                                        │
  press n to start a session.           │
                                        │
n new   q quit

no sessions
```

**11 — the setting, and its negative control.** The same `exit` sent to two
daemons, one started with `REMUDA_KEEP_EXITED=1` and one without:

```
$ remuda -s subj -e 'remuda.new("gone",{"sh"})'          → gone
$ remuda -s subj -e 'remuda.send("gone","exit")'
$ remuda -s subj ls
no sessions

$ REMUDA_KEEP_EXITED=1 remuda -s keep -e 'remuda.new("kept",{"sh"})'   → kept
$ remuda -s keep -e 'remuda.send("kept","exit")'
$ remuda -s keep ls
kept                   80x24   dead  idle 1s
$ remuda -s keep -e 'print(remuda.capture("kept"))'
$ exit
```

**12 — `remuda run` says which of the two happened**, asked of the daemon rather
than guessed, since only the daemon knows how it was started:

```
$ printf 'exit\n\n' | script -qc "remuda -s subj run sh" /dev/null
remuda: sh exited — the session is gone; REMUDA_KEEP_EXITED=1 in the daemon keeps it

$ printf 'exit\n\n' | script -qc "remuda -s keep run sh" /dev/null
remuda: sh exited — kept as dead, its last screen is in `remuda ls`
```

**13 — the guard does not outlive the viewer.** The TUI process was killed
outright while focused; this is the deadlock shape `steps/012` measured, and it
does not happen:

```
--- focused
sh                     80x24   live  idle 2s    attached
--- viewer killed
sh                     80x24   live  idle 31s
```

**The suite and the gates**, on this branch:

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result"
test result: ok. 0 passed;   ok. 21 passed;  ok. 10 passed;  ok. 5 passed;
test result: ok. 53 passed;  ok. 0 passed;   ok. 9 passed;   ok. 8 passed;
test result: ok. 7 passed;   ok. 4 passed;
$ cargo fmt --all --check                                   (exit 0)
$ cargo clippy --workspace --all-targets -- -D warnings      Finished
$ cargo check -p remuda-core --target wasm32-unknown-unknown Finished
$ python3 scripts/check-comments.py     ok — 199 doc comment(s) within cap
$ python3 scripts/check-steps.py        ok — 16 step(s)
$ python3 scripts/check-principles.py   ok — 14 principles
$ python3 scripts/check-workflows.py    ok — 3 workflow file(s) parse
$ python3 scripts/check-install.py      ok — 3 target(s) built and offered
```

No CI gate was added, so `gates-can-fail` gains no step — PRINCIPLES §2 is
satisfied by there being nothing new to control. Its plants are untouched: the
Lua-rename plant targets `native/src/script.rs`'s `"insert"`, and this change
adds a *name* to `remuda_core::keys` (`<backtab>`) without touching `BINDINGS`,
which is why `v1.lua` stays green rather than being edited.

## Not verified

- **The TUI under a human's hands**, still. Every frame above came out of a pty
  this process owns, driven by synthetic keys. Nobody has held an arrow key down
  in the focused pane or watched the preview repaint at 40ms while an agent
  redraws.
- **The known ceiling, named rather than solved: a program that takes the whole
  screen and every key** — vim, a nested tmux, an agent's own TUI — can swallow
  `Ctrl-\` before remuda sees it. `remuda attach` has always had this ceiling
  and this shares it, not being a second mechanism. Only the sessions driven
  above were shells, so it has not been *seen* either way here.
- **The cursor.** The preview draws the session's grid as text and `Capture`
  carries no cursor, so a focused pane shows your typing (the shell echoes it)
  but not the block cursor. It would need a cursor on the wire — a protocol
  addition and a second round trip per repaint, which is its own step.
- **Panning while focused.** `h` and `l` are text when the session has the
  keyboard, so a cropped pane cannot be panned without `Ctrl-\` first. The
  footer says `showing N cols` in both states; only the list's version offers
  `h/l pans`. Sizing new sessions to the pane makes the crop rare rather than
  impossible — a session made by `remuda run` in a wider terminal still crops.
- **Anything drawn on a Windows console.** Unchanged from `steps/015`: the suite
  runs on `windows-latest` and the TUI has still never been drawn on a console
  host. One new unknown: crossterm's Windows key parsing is not the unix parser
  measured above, so whether Ctrl-\ arrives there as `C-\` or `C-4` is untested
  — both are accepted, which is why this is a gap in evidence and not in cover.
- **A wide-character session in the focused pane.** `crop` and `fit` count
  `char`s and a wide character occupies two cells; unchanged and still wrong,
  and now wrong somewhere people type rather than somewhere they glance.
- **What `REMUDA_KEEP_EXITED` does to a long-running daemon's memory.** Sessions
  now leave on their own, which is the *good* direction; nobody has run a herd
  for a week with the setting on to see what a hundred kept screens cost.
