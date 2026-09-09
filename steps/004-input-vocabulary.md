# 004 — the input vocabulary, at the Lua level

정수님, 2026-09-10: *"emacs는 (insert-char "...") 처럼, 버퍼에 대한 io를 핸들링하는
펀더멘탈한 함수들도 제공합니다. 우리도 pty에 대해서 lua단에서 pty에 글자 입력, 문자열
입력, 안시코드 입력, 마우스 입력, 출력 결과 (화면 상태) 가져오기, 출력 결과 스트리밍
받기 등 여러 기본함수들을 제공할 필요가 있겠습니다."*

Six things are named. Five are input and screen state and are this step. The
sixth — **streaming output** — is a different shape (a subscription, not a
request and a reply) and is step 005, so that this one does not become two
changes wearing one number.

## Before

Lua can put exactly one kind of thing into a session: a whole instruction, with
Enter appended by the daemon. That is `remuda.send(name, text)`, and it is the
only door.

So a script cannot press an arrow key. It cannot send Ctrl-C. It cannot type
into a prompt and leave the line un-submitted the way a person does when they
are still deciding. It cannot click. Anything with a cursor in it — a pager, a
picker, an editor, a confirmation dialog, claude code itself — is undriveable,
because every one of those is steered by keys that are not a line of text.

The Emacs comparison in the instruction is exact. `insert-char` exists at the
bottom and everything else is built out of it; remuda has only the coarse verb
and nothing underneath it to compose from.

## Desired outcome

Lua gets the input vocabulary a terminal actually has:

```lua
remuda.insert(s, "hello")        -- bytes, no Enter. the insert-char analogue
remuda.insert(s, "\27[A")        -- arbitrary ANSI, because a Lua string is bytes
remuda.key(s, "up")              -- named keys, encoded for us
remuda.key(s, "C-c")             -- and modifiers
remuda.click(s, 40, 12)          -- mouse, SGR-encoded
remuda.capture(s)                -- unchanged; screen state already existed
```

### The invariant is generalized, not dropped — this is the whole design

Invariant 1 says `Session` exposes **no public raw write**, and `PRINCIPLES.md`
§6 cites it as the model case of removing a dangerous operation instead of
forbidding it. Read plainly, this step breaks that.

It does not, and the reason is worth being exact about, because getting it wrong
in either direction is expensive. The invariant's own sentence says what it is
protecting:

> "so no caller can send a body, lose the lock, and have another writer's Enter
> submit it"

The property is **atomicity of one input act**. It is not the absence of raw
bytes. `send_line` is one atom that happens to be spelled *text + CR*. The
correct generalization is therefore an atom that carries **any** bytes:

```
Session::send(&[u8])        one lock acquisition. the whole burst, or none of it.
Session::send_line(&str)    = send(text + "\r"), keeping the CR decision in one place
```

A caller can now compose any keystroke and still cannot be interleaved
mid-burst. `concurrent_send_lines_never_interleave` extends to bursts unchanged,
and its negative control still separates.

What §6 needs is not a repeal but a corrected illustration: the operation that
does not exist is not "a raw write", it is **a divisible write** — one that takes
the lock, returns, and lets a second writer land inside the act. That operation
still does not exist, and cannot be written, because there is no method that
hands out the lock mid-burst.

### What is actually given up, stated plainly

Before this step the fleet could not leave a half-typed line at a prompt. Now it
can: `remuda.insert(s, "rm -rf /")` sits there un-submitted until something
sends a CR.

That is a real new footgun and I am not going to describe it as safe. It is,
however, the same class as `sh script.sh` — a script that does a bad thing on
purpose — and not the class that caused the incident this design came from. The
incident was *two writers interleaving*, which stays impossible, and *orchestrated
input landing in a session a person was driving*, which invariant 3 still refuses
outright.

### One protocol variant, many encoders

The four input kinds in the instruction — character, string, ANSI, mouse — are
not four operations. They are one operation and four **encoders**. So:

- `Request::Send { name, bytes }` is the only new variant. The protocol still
  never grows a resize, and it still carries session names as opaque addresses
  (003's constraint).
- Key names → bytes and mouse events → bytes are pure functions with no host in
  them, so they live in `remuda-core` (`keys.rs`), under the wasm boundary, and
  are unit-testable without a pty.

Lua gets three new names because that is what reads well at the call site, not
because there are three operations. This is the Emacs shape: one primitive
underneath, a vocabulary on top.

### What is deliberately not built

- **No streaming.** Step 005, named above.
- **No new MCP tools.** The instruction says `lua단에서`, and handing raw
  keystrokes to a model is a different risk conversation than handing them to a
  script the user wrote and ran. `TOOLS` stays four; when a real client needs a
  key, that is the change that argues for it.
- **No CLI verbs.** Same reason. `remuda key …` is trivial to add the day
  something wants it from a shell.
- **No mouse *mode* management.** We encode and send the event. Whether the
  program underneath has enabled mouse reporting is the program's business;
  turning it on for it would be remuda deciding what the agent's terminal is
  configured like.
- **No key-name compatibility promise yet.** The named set below is what the
  common terminals send. If a real TUI disagrees with one of them, that is a
  bug report with a capture in it, not a design axis to generalize now.

## Expected

1. `remuda.insert(s, text)` puts exactly those bytes in and appends nothing — a
   line typed with `insert` is still sitting at the prompt, and only a separate
   CR runs it.
2. `remuda.key(s, "up")` and `remuda.key(s, "C-c")` produce the bytes a real
   terminal produces, asserted against the byte sequences themselves in core, and
   observed end to end by driving a real program that reacts to them.
3. `remuda.click(s, x, y)` produces a well-formed SGR mouse report.
4. An unknown key name is an **error**, not silently nothing — the empty-success
   failure this repo keeps finding, in its newest possible home.
5. `send` remains indivisible for bursts, not just lines: the existing
   interleaving test and its negative control both still hold with byte bursts.
6. `Session` still has no divisible write and no resize — asserted the same way
   §6 already is, by the method not existing.

## Actual

### The vocabulary, driven end to end

`native/tests/api/v1.lua`, run against a real daemon and a real shell. The
arithmetic matters (`PRINCIPLES.md` §4): a pty echoes its input, so `64-v1` can
only appear if the shell actually ran the line.

```
remuda.insert(name, "echo $((8*8))-v1")   -- no Enter. sits at the prompt.
remuda.key(name, "RET")                   -- this is what submits it
wait_for("64-v1")                         -- so `insert` really appended nothing
                                          -- and `key` really pressed Enter
v1 ok
```

Expectations 1 and 2 met, and they are proved by the *same* observation: if
`insert` had appended a CR the line would have run before `key` was called, and
if `key("RET")` produced nothing the line would never have run at all.

### The suite

```
$ cargo test --workspace --all-targets
test result: ok. 17 passed  (core invariants)
test result: ok. 10 passed  (key and mouse spellings)   ← new
test result: ok.  1 passed  (system clock)
test result: ok.  4 passed  (daemon, over a real socket)
test result: ok.  8 passed  (live pty sessions)
test result: ok.  4 passed  (MCP)
test result: ok.  4 passed  (the Lua runtime, incl. the frozen v1 API)
```

Every new guard was watched failing, with the plant shown:

```
planted: send appends "\r" (narrowing the atom back to a line)
  → send_line_writes_the_body_and_its_enter_as_one_burst   FAILED
  → send_appends_nothing_so_a_line_can_be_left_un_submitted FAILED
  → concurrent_send_lines_never_interleave                 FAILED

planted: the attached-check removed from Session::send
  → orchestrated_input_is_refused_while_a_human_is_attached FAILED   (only this)

planted: an unknown key name returns an empty burst instead of None
  → an_unknown_key_is_refused_rather_than_encoded_as_nothing FAILED  (only this)

planted: native/tests/api/v1.lua removed
  → every_frozen_api_version_still_runs                     FAILED   (only this)
```

### Expectation 5, and the negative control that still separates

`concurrent_send_lines_never_interleave` is unchanged in what it claims and
stronger in how it checks. Because `send_line` is now one burst, the predicate
went from "every body write is *followed by* an Enter write" to "every write
*is* a whole instruction". The control that must still go red does:

```
control_unlocked_writers_do_interleave ... ok   (i.e. it still detects interleaving)
```

That control writes a body and its CR under two separate lock acquisitions, so
its writes do not end in CR and the new predicate rejects them. Without that, the
predicate change would have been a loosening wearing a strengthening's clothes.

### Expectation 6, inherited — and the one claim I am not making

`Session` still has no resize and no divisible write. What changed is the
*width* of the atom, not whether it is one. `PRINCIPLES.md` §6's illustration
was corrected in place: the operation that does not exist is a write that a
second sender can land inside, and no method hands the lock back mid-act.

What I am **not** claiming: that the CSI sequences are correct against a real
TUI. They are asserted against the xterm encoding in unit tests, and end to end
only single-byte keys (`RET`) are witnessed, because there is no TUI in the
fixtures to press an arrow at. `C-c` would additionally exercise the pty's
signal handling rather than our bytes, which is why it is not here. If a real
program disagrees about `f5`, that is a bug report with a capture in it.

### 정수님 read the manual with me, and four of my premises were false

The instruction was *"이맥스는 어떤 함수들을 제공하는지 조사해보는 것도 좋을 것
같네요. elisp guide 문서가 있습니다."* I dispatched that survey against
`emacs-30.1/info/elisp.info` on this machine — and **stated four things as fact
in the dispatch that the manual does not contain**:

```
process-send-char exists          → 0 hits.  positive control: process-send-string → 4
the 512-byte pty buffer caveat    → 8 hits for "512", all SHA-512 or tag-table offsets
edmacro notation is in the manual → 0 hits; it lives in the mode's own docstring
node "Converting Representations" → exists, but is unibyte↔multibyte text, not keys
```

Each absence came back with a positive control attached, which is the only
reason "0 hits" was usable as an answer rather than as a broken search.

It changed the code, not just the comments. Emacs has **seven** bare shorthands
(`NUL RET TAB LFD ESC SPC DEL`) and I had implemented five; `NUL` and `LFD` were
missing outright. And the one I had right — Backspace sends 127, not 8 — now
cites the manual's sentence instead of my memory. Being right and having grounds
are different things, and only one of them survives the next edit.

### What the discipline caught this time: a mutation test that could not fail

Four defects were planted. The third one — unknown key returns an empty burst —
came back with **no test failing at all**, which read as "the guard is weak".

It was not. The plant did not compile: `Some(b"")` is `Option<&[u8; 0]>` where
the function returns `Option<&'static [u8]>`. My harness grepped for
`^test .* FAILED$`, and a compile error produces no such line, so a build that
never ran looked exactly like a suite that passed.

```
$ cargo test … | grep -E "^test .* FAILED$"     →  (nothing)      "guard is weak"
$ cargo build …| grep -E "^error"               →  E0308          it never built
```

This is `PRINCIPLES.md` §3 — *a negative control must match the reason, never
just the failure* — turned around: a control that accepts *no* failure also
accepts your own compile error. The rewritten plant asserts the build succeeds
before reading the test list, and then fails exactly one test.

The same `sed` also hit a second `_ => return None,` in `mouse()` that I had not
intended to touch. Both faults are the same one: I aimed the plant by *text*
rather than by *behaviour*.

### Said in passing, filed deliberately

Three things arrived while this step was being built, and 002 already taught me
what happens when I file one of those as a later layer's problem. Recorded here,
not built:

> **정수님, 2026-09-10:** *"우리도 remuda 자체를, 일종의 pty 개발 프레임워크로
> 생각하고, os에 맞물리는 로우레벨 부분은 rust로 작성하지만, 본격적인 로직은 lua로
> 작성함으로써, extensive editor라는 emacs의 기본 철학을 계승하는 것이 어떨까
> 합니다."*

The line this step actually draws is one notch different from "OS-facing vs
logic", and the difference is worth keeping: **Rust holds what a plugin must not
be able to undo.** The three invariants qualify; key encoding does not, and is in
Rust only because it is a primitive the language is *given*, in the sense elisp
is given `insert` by C. Everything above that seam is Lua's, and nothing lives
there yet — correctly, because no higher-order function has been needed.

> **정수님, 2026-09-10:** *"그런 면에서, 스몰톡도 매력적인 언어와 런타임이긴
> 합니다."*

What Smalltalk and Emacs share and remuda does not have: the running image is
inspectable and modifiable *from inside*. `remuda run script.lua` is batch — a
file, a run, an exit. The missing capability has a name (a Lua REPL against the
live daemon, with the registry reachable as Lua values) and a known trap: an
image-based system accumulates state nobody can rebuild from source. Emacs kept
the source in files and the image as derived. Inherit the liveness, keep the
reproducibility on the file side.

### The new principle, and why it is a principle rather than a note

`PRINCIPLES.md` §10 exists because of this step, and the incident is this step
itself: `BINDINGS` grew from six to nine and nothing in the build could tell that
apart from a rename. Measured, with the plant a tidy developer would actually
make — renaming a binding *and* its constant together:

```
the_bound_surface_is_exactly_the_protocols ... ok       ← stays green
every_frozen_api_version_still_runs        ... FAILED
    v1.lua:21: remuda.insert is gone
```

The surface test cannot catch this by construction: both sides moved together.
Only a script written against the old API notices, which is why one is frozen in
`native/tests/api/` and why editing it to pass is the thing forbidden rather than
discouraged. The negative control in `gates-can-fail` plants exactly this and
asserts *both* halves — the green one and the red one.
