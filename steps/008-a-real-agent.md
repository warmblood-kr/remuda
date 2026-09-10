# 008 — a real agent, end to end

정수님, 2026-09-10: *"그러면 그 lua 런타임 안에서 실제로 세션에 클로드 코드를
띄우고, 그 목록을 관리하고, 프롬프트를 입력하고, 결과화면을 읽어오는걸 합시다."*
And, approving the plan to measure before splitting repos: *"좋아요. 맞는 말들이네요.
승인합니다."*

This step **builds nothing on purpose.** It is a measurement: point the
vocabulary at a real agent and write down where it falls short. Every verb it
needs already exists (`new` `ls` `send` `capture` `close`), so anything that
fails here is a gap in the design, not a missing feature — and a gap found this
way has a name, which a gap imagined at a whiteboard does not.

## Before

Seven steps of vocabulary, and every one of them was proven against `sh`, `cat`,
or a `ScriptedAgent` fixture. Those are obedient: they echo what you type, they
finish, and they never repaint. A coding agent does none of that — it boots for
seconds, redraws a full-screen TUI, wraps to the terminal width it was given,
and answers on its own schedule.

So the honest statement of what is known is narrower than it looks:

```
proven against   sh · cat · printf · ScriptedAgent      (obedient, line-oriented)
never run        any full-screen TUI, any real agent    (repaints, boots slowly)
```

The specific thing missing is **readiness**. Every other verb answers a question
about a moment (`what is on screen`, `send these bytes`); none answers *"has it
finished, is it my turn"*. A caller can only poll `capture` and squint at the
text, which puts a screen-scraping heuristic in every script — the exact job the
manager exists to absorb.

## Desired outcome

One Claude Code session, driven from Lua through the daemon's image, from spawn
to answer to close — with the transcript captured, not described. Afterwards
either:

- it works end to end, and the vocabulary is proven against the thing it was
  built for; or
- it does not, and each failure has a **name** — a missing verb, a wrong
  default, a wrong assumption — which is what the next step is for.

Both outcomes are worth the same. This step is not allowed to succeed by
lowering the bar to something obedient.

## Expected

Written before running, so they can be contradicted:

1. **`remuda.new` from inside the image gets the wrong terminal size.**
   `script::bindings` calls `crate::terminal_size()`, which ioctls *its own*
   stdout — and inside the daemon that is `Stdio::null()`. So the ioctl fails
   and `Size::default()` (the 20×5 floor) is used. A full-screen TUI in 20
   columns will be unusable. The CLI does not have this bug, because there the
   binding runs in a terminal.

2. **The boot is slow enough that the first `capture` shows nothing useful** —
   the pty exists before the program has painted.

3. **`send` lands the prompt but may not submit it.** `SendLine` appends a
   newline, which is Enter; a TUI that distinguishes Enter from paste, or that
   swallows input typed before it is ready, will drop it.

4. **Readiness has no verb, and this is where it bites.** Deciding "the answer
   is complete" will require sleeping a guessed number of seconds and hoping.

5. **`ls` will show the session alive with a nonzero idle**, and `close` will
   end it — these two are exercised by step 006's tests and should just work.

6. **Nothing in the daemon will crash**, because the image never touches the
   pty: it goes back out over the socket like any other client.

## Actual

**It works end to end.** Spawn, wizard, prompt, answer, close — all of it driven
from Lua, through the daemon's image, against Claude Code v2.1.267.

```
$ remuda -e 'remuda.new("agent", {"claude"})'
$ remuda -e 'remuda.key("agent", "<down>"); remuda.key("agent", "RET")'
$ remuda -e 'remuda.send("agent", "Reply with exactly the word PONG and nothing else.")'
$ remuda -e 'remuda.capture("agent")'

 ▐▛███▛█   Claude Code v2.1.267
▝▜██████▀  Opus 5 with high effort · Claude Max
  ▝▝ ▝▝    ~/projects/pty-core-wip

⚠ 1 MCP server needs authentication · run /mcp

❯ Reply with exactly the word PONG and nothing else.

● PONG

✻ Cooked for 1s · done 9:04 AM
```

Prediction 1 was **wrong, and wrong in the interesting direction.** The
mechanism is exactly as described — the daemon's stdout is `Stdio::null()`, the
`TIOCGWINSZ` ioctl fails, and `Size::default()` is used — but that default is
`MIN_COLS × MIN_ROWS` = **80×24**, the classic VT100 size, not the tiny floor I
assumed. A real TUI is entirely usable in it.

```
remuda -e 'remuda.new("size-probe", …)'  →  80x24
remuda new size-cli sh                   →  80x24   (this terminal is 80 wide)
```

So the defect is not "unusable" but "**silently not yours**": a session made
from inside the image always gets 80×24 regardless of the caller's terminal,
and the two agree here only by coincidence. Worth a verb later
(`remuda.new(name, argv, {cols=, rows=})`); not worth one now.

Prediction 2 held: 0s is blank, painted by 3s.

**Prediction 3 was wrong, and its replacement is the most valuable thing here.**
`send` was never tested, because a fresh agent does not start at a prompt — it
starts at a **decision**, with the dangerous option pre-selected:

```
 ❯ No, exit
   Yes, I trust this folder

 Enter to confirm · Esc to cancel
```

A `send` here delivers no text at all; the newline lands on the highlighted
default and **kills the agent**. That is `relay-safe-worker-decisions` — the
fleet's own standing rule — appearing as a property of the machine rather than
a discipline in a document. Two verbs handled it, and this is the first time
`key` has been proven against anything but a fixture:

```
remuda.key("agent", "<down>")   →  ❯ moved to "Yes, I trust this folder"
remuda.key("agent", "RET")      →  prompt
```

Once past it, `send` submitted correctly on the first try.

Prediction 4 held, and the shape of the gap is now concrete rather than
suspected. The wait is written in Lua and works:

```lua
local function wait_for(name, pattern, limit)
  for i = 1, limit do
    if remuda.capture(name):find(pattern) then return i end
    remuda.sleep(0.5)
  end
end
```

— but the first pattern I wrote (`"trust this folder"`) returned `nil` after 40
polls, because **the second run in the same directory has no trust dialog at
all**; the folder is already trusted. Positive control, same function, a pattern
that must match:

```
wait_for("agent3", "Claude Code v", 40)  →  3      ← the poll itself works
wait_for("agent2", "trust this folder", 40)  →  nil ← the oracle was wrong
```

So the failure was not the loop; it was that the caller must know what the
screen will say, and the screen is **not a stable contract** — it differs
between the first and later runs, between versions, and between terminal
widths. A wrong guess fails *silently*, returning `nil` after twenty seconds
with no error to distinguish "not ready yet" from "will never match". Readiness
has to come from the session, not from reading its paint.

Prediction 5 held (`ls` → `live · idle 31s · 80x24`; `close` → 0 sessions).
Prediction 6 held: no crash, no deadlock — the image reaches the pty the same
way any client does.

### What this measured that the design could not

- **A real agent's first screen is a decision, not a prompt.** Any manager that
  assumes "spawned ⇒ ready to receive text" will kill the thing it spawned. The
  input vocabulary of step 004 turns out to have been necessary, not decorative.
- **The session inherits the *daemon's* environment, not the caller's.** Visible
  in the transcript: the agent reported an inherited `CLAUDE_CODE_CHILD_SESSION`
  marker from the shell that started the daemon hours earlier. That is correct
  behavior for a daemon and surprising the first time, and it means the daemon's
  launch environment is part of every session's contract.
- **Readiness is the one genuinely missing verb**, and it cannot be built out of
  the ones that exist. Everything else is a spelling of `Send` or a read of a
  moment; this needs the manager to watch output over time — which is what makes
  it the natural home for the `on_output` callback step 007's header already
  warned about (a pump may never call *into* Lua; it must post a job).

### What this step did NOT do

No code changed. Nothing was added to make the test pass, on purpose: the value
of the measurement is that every verb it used was already there, so the gaps it
found are the design's gaps and not a missing feature's.

