# 012 — a front door

Typing `remuda` opens the herd. Ten verbs become four. Leaving a session looks
like leaving.

## Before

정수님 upgraded, ran `remuda`, and got a subcommand manual. He then ran
`remuda new claude-code`, attached, could not tell whose shell he was in, typed
`exit` twice, and his terminal closed. That is **two** defects, and both are in
the source rather than in his reading of it.

**A — he never started claude.** `bin/remuda.rs` parsed `["new", name, rest @ ..]`,
so `claude-code` bound to `name` and `rest` was empty; `daemon::spawn` then
substitutes `$SHELL` for an empty argv. He got a bare shell *named* `claude-code`
and nothing said so. The mandatory leading name eats the first word — which is
exactly the word a person types the program in.

**B — leaving is invisible.** `client::attach` returned `Ok(())` for a detach and
for a death alike, and remuda never entered the alternate screen, so the last
frame the inner shell drew stayed on the display byte for byte. He *was* back in
his own shell after the first `exit`. The second went to his login shell.

Underneath both: the command line was the machine's vocabulary served as the
front page. `send`, `capture`, `close` are precise, name-addressed and
non-interactive — correct for a script, and never designed for a person.

And the picker is not a new idea. `steps/001-pty-manager-daemon.md:1-5` records
the original instruction — *"with session list so that user can select a session
to attach"* — and it shipped as `ls` + `attach`, i.e. the human does the
selecting by reading a name and retyping it.

## Desired outcome

1. `remuda` with no arguments opens a full-screen TUI: sessions on the left, the
   selected one previewed on the right, Enter to ride it. Usage text when either
   end is not a tty, because this also runs in CI and in pipes.
2. `remuda run <argv…>` creates and rides in one act, argv first. Bug A becomes
   unreachable: there is no leading positional to eat the program name.
3. The CLI keeps a verb only if it **needs a terminal**, must **survive the
   shell's own quoting**, or **renders for a human** in a way the Lua image
   cannot. Ten verbs → four. Nothing is lost; `remuda -e` reaches all of it.
4. Riding happens inside the alternate screen, and coming back says which way
   you left and how to get back.

Flat by ruling — 정수님, this change: 「TUI 왼쪽 목록은 session 목록을 나타냄.
오른쪽 pane은 선택된 세션 하나를 통으로 전체화면으로 보여줌. tmux처럼 화면 분할할
필요 없다고 봄.」 No panes. That also protects invariant 2: a split resizes its
neighbours, so it would need `Session::resize`, and the README calls that one of
the three joints that cannot be retrofitted. None was added.

The cut is authorised now rather than deprecated over a release: 「줄이는것도
지금 해도 됩니다. 릴리즈하거나 한게 아니라서 아무도 쓰는 사람이 없고 저도 안쓰고
있어요.」

| verb | needs a terminal | survives quoting | renders for a human | verdict |
|---|---|---|---|---|
| `run` | **yes** | argv | — | **CLI**, new |
| `attach` | **yes** | no | — | **CLI** — and nowhere else can work |
| `ls` | no | no | **yes**, an aligned table | **CLI** |
| `send` | no | **yes, decisive** | no | **CLI** |
| `new` | no | — | — | Lua — subsumed by `run` |
| `close` | no | no | no | Lua — the TUI's `x` covers the human |
| `capture` | no | no | no — the image prints the same string | Lua |
| `insert` `key` `click` | no | — | — | Lua (already were) |

`attach` is settled by a mechanism, not by taste: Lua's `attach` runs **inside
the daemon**, whose stdin is `Stdio::null()`, so raw mode cannot be entered
there. `send` is decisive on quoting because the text a person sends an agent is
prose — apostrophes, `$` — and via `-e` it crosses two quoting layers, shell then
Lua, where a bug fails silently and wrongly.

## Expected

- `remuda` into a pipe still prints usage; into a terminal it draws two panes.
- `remuda run sh` (×3) yields sessions `sh`, `sh-2`, `sh-3`; `New` answers with
  the name it chose, because otherwise `run` cannot attach to what it just made.
- `remuda run build.lua` refuses with the `remuda lua` spelling rather than
  falling back — principle 5's instinct, applied to a compatibility shim.
- Ctrl-\ out of a ride restores the user's own screen and prints how to return.
- An inner shell exiting restores the same screen and says the session is kept.
- `v1.lua` keeps passing: it calls `remuda.new(name, {"sh"})` as a bare
  statement and asserts nothing about its return, so `Ok → nil` becoming
  `Value → string` leaves it green. Run it; do not reason about it twice.

## Actual

`remuda` with no tty on either end — usage, not a hang:

```
$ ./target/debug/remuda | head -8
remuda — a pty manager you can attach to

  remuda                        open the herd (a terminal is required)
  remuda run [-n name] <argv…>  start a program and ride it, in one act
  remuda attach <name>          hand this terminal over; Ctrl-\ detaches
  remuda ls                     list sessions
  remuda send <name> <text>     deliver one instruction (body + Enter)
```

The daemon now says it started one, and `run` refuses a Lua script by name:

```
$ remuda ls
remuda: started a daemon for "default"
no sessions

$ remuda run /tmp/build.lua
remuda: `run` executes a program. For a Lua script: remuda lua /tmp/build.lua
$ echo $?
1

$ remuda lua /tmp/build.lua
1
```

Names generate themselves, and the cut verbs are still there in the image:

```
$ remuda -e 'remuda.new(nil, {"sh"})'
sh
$ remuda -e 'remuda.new(nil, {"sh"})'
sh-2
$ remuda ls
sh                     80x24   live  idle 0s
sh-2                   80x24   live  idle 0s
```

### The TUI, drawn into a real pty

There is no terminal in this environment, so the TUI was driven **by remuda
itself**: a session was spawned running the built binary, and its screen read
back with `capture`. Every frame below is a real pty's grid, not a mock-up.

```
$ remuda -e 'remuda.new("tui", {"…/target/debug/remuda"})'
tui
$ remuda -e 'print(remuda.capture("tui"))'
remuda · default│alpha · 80×24 · live
▸ alpha         │$
  bravo         │
  tui           │
                │
↑↓ select   ⏎ ride   n new   x kill   q quit
```

`<down>` moves the selection and the preview follows it:

```
$ remuda -e 'remuda.key("tui","<down>")'
$ remuda -e 'print(remuda.capture("tui"))'
remuda · default│bravo · 80×24 · live
  alpha         │$
▸ bravo         │
  tui           │
```

`Enter` rides it — no chrome, keys reach the pty, and the herd knows who holds
it. `42-ridden` rather than an echo of the question, per principle 4:

```
$ remuda -e 'remuda.key("tui","RET")'
$ remuda -e 'remuda.insert("tui","echo $((6*7))-ridden\n")'
$ remuda -e 'print(remuda.capture("tui"))'
$ echo $((6*7))-ridden
42-ridden
$

$ remuda ls
alpha                  80x24   live  idle 13s
bravo                  80x24   live  idle 13s   attached
tui                    80x24   live  idle 1s
```

Ctrl-\ (0x1C) returns to the list, cursor where it was left:

```
$ remuda -e 'remuda.insert("tui","\28")'
$ remuda -e 'print(remuda.capture("tui"))'
remuda · default│bravo · 80×24 · live
  alpha         │$
▸ bravo         │
  tui           │
```

An empty herd opens the prompt prefilled and says what it will run — one Enter,
and the session names itself after the program:

```
remuda · default                        │
                                        │
  the herd is empty.                    │
                                        │
start: /usr/bin/zsh▏   ⏎ run · esc cancel
```

```
remuda · default│zsh · 80×24 · live · showing 63 cols — h/l pans
▸ zsh           │[oh-my-zsh] virtualenvwrapper plugin: Cannot find virtualenvwra→
                │➜  pty-core-wip git:(usability-rework) ✗
↑↓ select   ⏎ ride   n new   x kill   q quit
```

`x` asks before killing a live session, and `y` does it:

```
kill sh? it is running — y / n
```

A 101-character line in a 63-column pane: the title band says how much is
showing, every cut row carries `→`, and `l` pans into what was cut — the crop is
never allowed to read as absence:

```
remuda · default│sh · 80×24 · live · showing 63 cols — h/l pans
▸ sh            │n" | tr " " x
                │xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx→

  … after five presses of `l` …

remuda · default│sh · 80×24 · live
▸ sh            │
                │xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

### The incident, run again

An outer pty holding a plain shell; the commands below were typed into it for
real. `remuda run -n rider sh`, then Ctrl-\ — the alternate screen gives the
user's own scrollback back, and the line prints in their restored shell:

```
$ …/remuda run -n rider sh
remuda: detached from rider — still running, `remuda attach rider` to go back
$
```

Then `remuda run sh`, and `exit` inside it — the half that closed his terminal:

```
$ …/remuda run sh
$ exit

[remuda] sh ended — press any key
```

```
$ …/remuda run -n rider sh
remuda: detached from rider — still running, `remuda attach rider` to go back
$ …/remuda run sh
remuda: sh exited — kept as dead, its last screen is in `remuda ls`
remuda: you are back in your own shell
$
```

The name generated itself from argv[0] on the way in:

```
$ remuda ls
outer                  80x24   live  idle 1s
rider                  80x24   live  idle 12s
sh                     80x24   live  idle 1s    attached
```

### A defect found by running it, and not fixed here

The `[remuda] sh ended — press any key` line above is not decoration. Measured
while building this: **when the session ends, `attach` does not return until the
next keystroke, and that keystroke is swallowed.**

```
t+1s: $ exit
t+2s: $ exit
t+3s: $ exit
t+4s: $ exit
--- now one keystroke ---
remuda · default│aaa-x · 80×24 · exited · screen kept, x clears it
▸ aaa-x         │$ exit
```

The cause is `client::attach`'s `keys.join()`, and it is **pre-existing** —
`git show origin/main:native/src/client.rs` has it at line 105, with
`stdin().lock()` at line 64. The key thread blocks in a tty read; `ipc::wake`
shuts the socket, which the key thread is not waiting on. A tty read cannot be
interrupted portably, and the two candidate fixes are both worse than the
symptom in this change: orphaning the thread **deadlocks the next ride**, because
`StdinLock` is a mutex; and `VMIN`/`VTIME` means hand-written termios, which
`steps/010` deliberately removed one commit ago in favour of crossterm.

So the freeze is left in place and made **legible**, which is this step's whole
subject: a stated wait is a different failure from a dead screen. Fixing it
needs an interruptible terminal read on both hosts, and it is its own step.

### The suite, and the gates

```
$ cargo test --workspace --all-targets 2>&1 | grep -E "^test result|Running"
     Running unittests src/lib.rs                (core)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/invariants.rs
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/keys.rs
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/names.rs                      ← new
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running unittests src/lib.rs                (native, incl. the TUI state machine)
test result: ok. 33 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running unittests src/bin/remuda.rs
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/daemon.rs
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/live_sessions.rs
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/mcp.rs
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/script.rs
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

The frozen v1 script was run, not reasoned about — `New` now answers
`Value(name)` where it answered `Ok`, and `v1.lua:30` calls `remuda.new` as a
bare statement, so it stays green:

```
$ cargo test -p remuda-native --test script
running 4 tests
test the_bound_surface_is_exactly_the_protocols ... ok
test a_script_reacts_to_what_a_session_shows ... ok
test every_frozen_api_version_still_runs ... ok
test a_refusal_stops_the_script_instead_of_being_returned ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Every gate, run here:

```
$ python3 scripts/check-comments.py
ok — 158 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-steps.py
ok — 12 step(s), each with before, desired, expected and captured actual
$ python3 scripts/check-principles.py
ok — 14 principles, every named mechanism exists (14 CI jobs, 11 denied paths, 177 test fns seen)
$ python3 scripts/check-workflows.py
ok — 3 workflow file(s) parse, 14 job(s) defined
$ python3 scripts/check-install.py
ok — 3 target(s) built and offered (2 via install.sh, 1 via install.ps1): aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu

$ cargo fmt --all --check
$ echo $?
0
$ cargo clippy --workspace --all-targets -- -D warnings
    Checking remuda-native v0.1.0 (/home/toracle/projects/pty-core-wip/native)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.38s
$ cargo check -p remuda-core --target wasm32-unknown-unknown
    Checking remuda-core v0.1.0 (/home/toracle/projects/pty-core-wip/core)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.24s
```

No new CI gate was added, so `gates-can-fail` gains no step — principle 2 is
satisfied by there being nothing new to control. The `gates-can-fail` job's
existing steps were not touched and its plants still apply: the Lua-rename plant
targets `native/src/script.rs`'s `"insert"`, which this change leaves alone.

## Not verified

Named rather than glossed, because a confident list is worth less than a short
honest one.

- **The TUI under a human's hands.** Every frame above came out of a pty this
  process owns, driven by synthetic keystrokes. Nobody has held the arrow keys,
  resized the window mid-browse, or ridden a session that repaints. The state
  machine (`Ui::on_key`, `layout`, `crop`, `render`) is pure and tested for
  real; the loop around it is not.
- **A terminal resized while the TUI is running.** `render` reads
  `terminal::size()` every tick so it should follow, and it has never been seen
  to. §4.1's size-mismatch case is handled by the title band rather than the
  design's `[enter] ride anyway / [q] back` dialog — the information is on
  screen before Enter is pressed, and no modal was built.
- **`Left::Detached` vs `Left::Exited` as a unit test.** It cannot be one:
  `client::attach` enters raw mode, and the suite has no tty by design. It is
  witnessed in the captures above and nowhere in CI.
- **Anything on Windows.** The TUI has never been drawn on a console host.
  `test-windows` compiles and runs the suite, which now includes the pure TUI
  tests; it does not open the TUI.
- **CJK and wide characters in the preview.** `crop` and `fit` count `char`s,
  and a wide character occupies two cells. `steps/005` records the same
  arithmetic already being wrong in the click layer. Ceiling, not a fix.
- **The nested alternate screen.** An inner program that uses the alternate
  screen itself (vim) pops remuda's early on quit. Never observed here — the
  sessions driven above were all shells.
