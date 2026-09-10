# 011 — A REPL you can edit, and tables you can read

## Before

Two defects, both found by 정수님 using the thing rather than reading it. They
are unrelated in the code and identical in kind: the Lua surface is the part a
person touches with their hands, and both of these make it hostile to touch.

**Arrow keys print escapes.** `repl()` in `native/src/bin/remuda.rs` was a bare
`stdin.read_line` loop, with a doc comment that named the choice out loud:

```
/// A line-at-a-time REPL against the image. Deliberately not a readline — no
/// history, completion or raw mode — so a piped heredoc works too.
```

So `<up>` puts `^[[A` on the line, a typo has to be retyped from the start, and
nothing survives from one `remuda repl` to the next. The reason given —
"so a piped heredoc works" — turns out not to be a trade at all; see Expected #1.

**Every table prints as `<table>`.** His session, verbatim:

```
> c = {a, 1, 'a', 1, "a", 2}
> c
<table>
> c[a]
1
> c['a']
nil
```

He was reduced to probing one key at a time, and the two probes he chose are
exactly the ones the rendering cannot distinguish: `c[a]` is `c[2]` and
`c['a']` is a string key. A REPL whose values are opaque is a REPL you cannot
use to answer a question about a value.

The cause is `render()` in `native/src/image.rs`, and its reason is good:

```
/// One value as a Lua user expects to see it: `tostring` semantics, minus the
/// address on tables and functions — an address differs every run, so an
/// expected output could not be written down.
```

That is principle 9's requirement acting on the renderer, and it must survive
the fix. Putting the address back would trade an unreadable value for an
unwritable expectation. **Contents are both more useful and more deterministic
than an address** — which is why this step is a fix and not a trade.

## Desired outcome

**A REPL with hands.** Arrow keys move, the line is editable in place, history
works within a session and across sessions, `Ctrl-C` abandons a line without
killing the image's accumulated state, and `Ctrl-D` on an empty line still
exits. History lives at `$XDG_STATE_HOME/remuda/` (`~/.local/state/remuda/`
when that is unset), and a state directory that is missing, unwritable, or
un-derivable costs the history and never the REPL.

**Tables that show their contents**, in a form a person reads as Lua and a test
can assert on:

- strings quoted *inside* a table, so `1` and `"1"` cannot be confused — the
  exact confusion in his transcript. Top level keeps `tostring` semantics, so
  `remuda -e "s"` and `print(s)` do not suddenly grow quotes.
- deterministic order: array part in index order, then every other key sorted.
  Lua's hash order varies between runs; an expected output must not.
- bounded in depth and in element count, elided with `…`, so a big or deep
  table cannot flood a terminal or hang the loop.
- cycle-safe: `t = {}; t.self = t` renders, it does not recurse.
- functions, userdata and threads keep `<function>`-style forms. Only tables
  gain contents, because only tables have contents worth showing.

### Deliberately out of scope

- Completion, multi-line editing, and syntax highlighting in the REPL. `<tab>`
  is not bound to anything; that is a separate design question (complete on
  what — globals, table fields, session names?) and none of it is what he hit.
- A `tostring`/`__tostring` metamethod hook for tables. It would make output
  depend on user code, which is the opposite of what `render` is for.
- Any change to the protocol. The REPL still sends `Request::Eval`.

## Expected

Written before building, so the result can contradict it:

1. `rustyline` will handle a non-tty stdin by reading plain lines, so
   `printf … | remuda repl` keeps working and the doc comment's stated trade
   was never a real one. If this is wrong, the whole approach is wrong, because
   the pipe path is used by this repo's own step 007 evidence.
2. The rendering change will be ~60 lines in `image.rs` and touch nothing else,
   because `render` is private and has exactly two callers (`eval` and the
   captured `print`).
3. `v1.lua` will pass unchanged. It asserts on `type(...)` and on string
   `find`, never on the text of a rendered table.
4. Cycle safety will need the tables on the *current path*, not every table
   visited — a diamond (`{leaf, leaf}`) is finite and must render twice, not as
   `<cycle>`.
5. `remuda -e "({})"` printing `<table>` is captured in step 007's Actual. That
   line becomes stale, and this is the change that stales it — expected, and
   recorded here rather than edited there.
6. The three arms already in `main` that need no daemon (`--version`, `help`,
   `upgrade`) are untouched; only `repl` changes, plus one new dependency.

## Actual

### The transcript that started it, replayed against this build

```
$ printf "a = 2\nc = {a, 1, 'a', 1, \"a\", 2}\nc\nc[a]\nc['a']\n" | remuda repl
{2, 1, "a", 1, "a", 2}
1
nil
```

`c` now answers the question he was probing for one key at a time: it is a
six-element array whose second element is `1` and which has no `'a'` key at
all. Both of his probes are still exactly right, and now redundant.

### Every rendering shape, from the real binary

```
$ remuda -e "({})"
{}
$ remuda -e "({1, 2, 3})"
{1, 2, 3}
$ remuda -e "({1, '1'})"
{1, "1"}
$ remuda -e "({name = 'x', 1, 2})"
{1, 2, name = "x"}
$ remuda -e "({b = 2, a = 1, [3.5] = true, ['two words'] = 'k'})"
{[3.5] = true, a = 1, b = 2, ["two words"] = "k"}
$ remuda -e "({{1, {2, {3, {4, {5}}}}}})"
{{1, {2, {3, {…}}}}}
$ remuda -e "(function() local t = {} t.self = t t.n = 1 return t end)()"
{n = 1, self = <cycle>}
$ remuda -e "(function() local t = {} for i = 1, 100 do t[i] = i end return t end)()"
{1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, …}
$ remuda -e "(function() local leaf = {'shared'} return {leaf, leaf} end)()"
{{"shared"}, {"shared"}}
$ remuda -e "print"
<function>
$ remuda -e "coroutine.create(function() end)"
<thread>
$ remuda -e "'a bare string is still bare'"
a bare string is still bare
$ remuda -e "print({1, 'two'})"
{1, "two"}
```

Expected #4 held: `{leaf, leaf}` is a diamond, not a cycle, and renders twice.

The ordering claim is the one that has to be *measured* rather than read, since
Lua's hash order is what varies. Five separate processes, one image, `sort -u`
collapsing them:

```
$ remuda -e "d = {zulu=1, alpha=2, mike=3, [7]=4, bravo=5}"
$ for _ in 1 2 3 4 5; do remuda -e "d"; done | sort -u
{[7] = 4, alpha = 2, bravo = 5, mike = 3, zulu = 1}
```

One line out, so the five agreed. `[7]` sorts with the numbers and ahead of the
strings; `["two words"]` above sorts by its *value*, between `b` and `zulu`, so
bracketing does not reorder anything.

### The REPL, driven through a real pty

A REPL is interactive, so this was driven by **remuda itself** — a bash session
in the daemon, running the new binary's `repl`, with `remuda.key(...)` putting
the bytes a terminal actually sends onto its pty. That covers everything below
non-interactively except the one thing it cannot: nobody watched the cursor
move between keystrokes. What is asserted is the screen after each key.

```
### 1. start the repl, type a line, run it
sh$ …/target/release/remuda repl
> 1+1
2
> 

### 2. <up> recalls the last line instead of printing an escape
-- screen after <up>, before RET:
2
> 1+1
-- after RET:
> 1+1
2
> 

### 3. <left> <left> then a keystroke edits mid-line: 10+2 becomes 100+2
-- before RET:
> 100+2
-- after RET:
102
> 

### 4. C-c abandons the line, keeps the repl and the image's state
> this line is garbage
> kept
still here
> 

### 5. C-d on an empty line exits back to the shell
sh$ echo back-in-bash-$((6*7))
back-in-bash-42
sh$ 
```

Step 2 is the defect itself: the screen holds `> 1+1`, not `> ^[[A`. Step 3 is
in-line editing — two `<left>`s and a `0` turn `10+2` into `100+2`, which is
only possible if the cursor moved without the text scrolling away. Step 4 is
the one worth stating plainly: `kept` was set through `remuda -e` *between*
REPL lines, and it is still there after `C-c`, so `C-c` cancelled a line and
not the image. Step 5 lands back in bash, and `42` cannot come from the echo of
the question.

History across processes, in the same session — a second `remuda repl` process,
first `<up>`:

```
### 6. history persisted: a NEW repl process recalls line 1 with <up>
sh$ …/target/release/remuda repl
> kept
```

`kept` was the last line typed into the *previous* process.

### Where history lands, including when it cannot

```
### the $HOME fallback (no XDG_STATE_HOME set)
$ cat ~/.local/state/remuda/repl-history
#V2
a = 2
c = {a, 1, 'a', 1, "a", 2}
c
c[a]
c['a']
1+1
100+2
kept

### XDG_STATE_HOME wins when it is set
$ printf 'x = 11\nx * 2\n' | XDG_STATE_HOME=/tmp/rmdwt2/state remuda repl
22
$ find /tmp/rmdwt2/state -type f
/tmp/rmdwt2/state/remuda/repl-history
$ cat /tmp/rmdwt2/state/remuda/repl-history
#V2
x = 11
x * 2

### an unwritable state dir: history is lost, the repl is not
$ chmod a-w /tmp/rmdwt3/state
$ printf '"survived"\n' | XDG_STATE_HOME=/tmp/rmdwt3/state remuda repl
survived
exit=0
$ ls -A /tmp/rmdwt3/state | wc -l
0

### HOME unset and no XDG_STATE_HOME: nowhere to store it, still starts
$ printf '"no home either"\n' | env -u HOME remuda repl
no home either
exit=0
```

The first block is a positive control the run produced by accident and is worth
keeping: the daemon holding that session was started before `XDG_STATE_HOME`
was exported, so it took the `$HOME` branch — and the file it wrote contains
the piped transcript from the very top of this document. Both branches were
exercised by a real process, not by reading the `match`.

### Expected #1 held, and the trade it named was never a trade

```
$ printf 'fleet = {}\nfleet.count = 3\nfleet.count\n' | remuda repl
3
```

One difference from the old behaviour, and it is a behaviour change worth
naming rather than burying: when stdin is not a terminal, rustyline does not
echo the `> ` prompt. Step 007's captured evidence shows the old loop printing
`> 3\n> > 13\n> ` into a pipe. Piped output is now just the values. No test
depended on the prompts; a script grepping this path gets cleaner input than
before, and nothing gets less.

### The gates

```
$ cargo test --workspace --all-targets
     Running tests/invariants.rs        test result: ok. 21 passed
     Running tests/keys.rs              test result: ok. 10 passed
     Running unittests src/lib.rs       test result: ok. 14 passed   ← was 4; 10 are new
     Running tests/daemon.rs            test result: ok. 4 passed
     Running tests/live_sessions.rs     test result: ok. 8 passed
     Running tests/mcp.rs               test result: ok. 4 passed
     Running tests/script.rs            test result: ok. 4 passed    ← v1.lua, unchanged
$ cargo fmt --all --check                                            (clean)
$ cargo clippy --workspace --all-targets -- -D warnings              (clean)
$ cargo check -p remuda-core --target wasm32-unknown-unknown         (clean)
$ python3 scripts/check-comments.py
ok — 121 doc comment(s) within cap (item 3, module 20)
$ python3 scripts/check-principles.py
ok — 13 principles, every named mechanism exists (13 CI jobs, 11 denied paths, 130 test fns seen)
$ python3 scripts/check-install.py
ok — 2 target(s) built and offered: aarch64-apple-darwin, x86_64-unknown-linux-gnu
$ python3 scripts/check-workflows.py
ok — 3 workflow file(s) parse, 13 job(s) defined
$ python3 scripts/check-steps.py
ok — 10 step(s), each with before, desired, expected and captured actual
```

That last line counts this document, so it was captured after writing it —
which is also why the number is 10 and not the 9 a reader of `main` would see.

Expected #3 held — `every_frozen_api_version_still_runs` passes untouched.
Expected #2 was optimistic: the rendering is ~85 lines in `image.rs`, not ~60,
and the extra is the key-shape function (`name = v` versus `["a b"] = v`),
which was not in the estimate at all.

The ten new tests are the ones that would go red if this rots: the owner's own
table, `{1, "1"}`, the sort, array-before-keys, the cycle, the diamond, the two
caps, the empty table, and scalars keeping `tostring` semantics.

### What this step did NOT do

**No new gate, so no `gates-can-fail` entry.** Principle 2 requires a negative
control for a *mechanism*; this change adds tests, not a guard, and the
existing `test` job already fails when they fail. `gates-can-fail` is
unchanged, and deliberately.

**Nothing enforces that the rendering stays deterministic** beyond the sort
test. A future `__pairs`-aware or metatable-aware rendering could reintroduce
run-to-run variation and the suite would not notice, because the tests build
their tables from literals. The honest statement of the limit: what is checked
is that *these* tables render *this* way, not that all tables render stably.

**`<tab>` does nothing.** rustyline's default completer is the no-op one and
was left that way — see Desired's out-of-scope note.

### One dependency added

`rustyline 14.0.0`, in `remuda-native` only. It is the conventional Rust
readline and the one that covers all four of the required behaviours (editing,
in-session history, persisted history, and `Ctrl-C`/`Ctrl-D` as distinct
outcomes) without a line of terminal code here. Writing raw-mode line editing
by hand is a well-known several-hundred-line job with a long tail of terminal
quirks, and this repo has no reason to own that tail. `remuda-core` is
untouched, so the wasm boundary is unaffected — checked above rather than
assumed.
