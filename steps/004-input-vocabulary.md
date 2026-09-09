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

pending — implementation follows.
