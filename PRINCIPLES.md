# Principles

정수님, 2026-09-10: *"let's establish a principle, and document on it with
discipline. I want this repo to have an order."*

Every principle below carries an **Enforced by:** line naming the machine that
rejects a violation — a CI job, a clippy key, a test. Where nothing enforces it,
the line says `nothing — discipline only`, and that is not a placeholder to be
filled with a promise. It is the honest state, and it is the most useful thing on
the page: it says exactly where this repo is still running on good intentions.

> **A rule you can only remember is a hope. A rule the build checks is a
> control.** Every principle here was written after something actually went
> wrong, and the incident is named. None of them is a preference.

**The document itself is checked.** The `principles-are-documented` CI job
verifies that every mechanism named below actually exists — a renamed job or a
deleted lint turns this file from documentation into fiction, silently, and the
build catches that instead of a future reader.

---

## 1. Boundaries are compiled, not reviewed

The policy layer (`remuda-core`) may not reach the operating system. This is not
a convention: the crate lists no host dependency, so it *cannot* name one, and
the arrow from `remuda-native` back to `remuda-core` is rejected as a cycle.

**Why.** The first cut used a Cargo feature (`native`). A feature is a wall you
can forget to raise. Measured 2026-09-09: with four deliberate leaks planted in
the pure layer, `wasm32-unknown-unknown` caught `std::os::unix::*` (3 errors) and
let `Command`, `fs::read_to_string`, and `Instant` through with **zero** — wasm32
ships std stubs that compile and fail only at runtime. So the crate split closes
the dependency axis, and `core/clippy.toml` closes the hand-written-syscall axis.
Two mechanisms because one measurement showed one gap.

**Enforced by:** CI job `wasm-boundary` · CI job `gates-can-fail` · `core/clippy.toml`

## 2. A gate that has never gone red is a claim, not a control

Every guard must be watched failing. CI plants each violation, asserts the
rejection, and reverts.

**Why.** Otherwise a silenced lint or a dropped dependency edge makes the guard
go quiet, and quiet reads exactly like clean.

**Enforced by:** CI job `gates-can-fail`

## 3. A negative control must match the *reason*, never just the failure

Asserting "it failed" is not enough. Grep for the message the gate itself emits.

**Why.** Measured 2026-09-09, on this job's own first run: appending a dependency
line with `>>` landed it in the trailing `[lints]` table, cargo died on
`unknown variant '../native'` — a TOML syntax error — and the check cheerfully
reported the architecture rule was alive. **A control that accepts any failure
accepts your own typo.** The three reasons now matched by name are
`disallowed type 'std::process::Command'`, `os::unix`, and
`cyclic package dependency`.

**Enforced by:** CI job `gates-can-fail`

## 4. A test must depend on the thing happening, not on its own input

An assertion that would pass whether or not the system did anything is not a
test.

**Why.** Measured 2026-09-09: all six live-pty tests passed in 0.05s, which is
impossible for spawning shells. A pty **echoes its input**, so sending
`echo alpha-landed` puts that string on screen regardless of whether the shell
ran, and `wait_for("alpha-landed")` matched the echo. Six tests were proving
nothing. They now send `echo $((6*7))-landed` and wait for `42-landed`: the
answer cannot appear in the echo of the question.

**Enforced by:** nothing — discipline only. *(No mechanism distinguishes an
assertion that tracks real behaviour from one that tracks its own input. The
tell was wall-clock time, noticed by a human. If you find a mechanism, this line
is where it goes.)*

## 5. No fallback inside a measurement

A measurement must fail when it cannot measure. Never `|| default`.

**Why.** Measured 2026-09-09: the terminal-width test read
`tput cols 2>/dev/null || echo 80`. That test could not fail — without `tput` it
measured its own fallback and reported the pty was 80 columns wide. Removing the
fallback turned it red on CI (the runner has no ncurses), which is the *correct*
outcome and how the defect was found.

**Enforced by:** nothing — discipline only. *(A fallback is legitimate in
production code and fatal in a measurement; no linter knows which it is
reading.)*

## 6. Remove the dangerous operation; do not forbid it

Where a rule would say "never call X", delete X from the API instead.

**Why.** `Session` has no `resize` method, so an attaching viewer cannot shrink
the terminal under a running agent — the defect where attaching resized a shared
session down to the smallest client. `Session` exposes no raw write either, so
body-and-Enter cannot be separated by a second writer. Both are properties of the
API's *shape*, not of caller discipline.

### The three invariants this buys

**1 — one input act cannot be split by a second writer.** `send` and `send_line`
are the only ways to put input into a session, and each delivers its whole burst
under a single lock acquisition. There is no *divisible* write — no method that
takes the lock and hands it back part-way through an act — so no caller can send
a body, lose the lock, and have another writer's Enter submit it. The Emacs
implementation had exactly this bug shape: a stray submit landed on whatever was
highlighted, and the intended text was swallowed **with both sides reporting
success**.

**2 — nobody can resize the pty.** There is no `resize`, and `Size` is immutable
once constructed. Attaching is meant to be routine — it is how a human logs the
agent in — and in zellij, whose behaviour was measured for this design, attaching
resizes the shared session down to the smallest client. Here an attacher gets a
view and may scroll or crop; the program underneath never sees SIGWINCH.

**3 — raw keystrokes exist only while exactly one human holds the session.** A
human at an attached terminal types Enter themselves, so attaching needs the very
raw write invariant 1 refuses to expose. The two are reconciled by *exclusivity*
rather than by a rule: `attach` hands out an `Attached` guard, at most one at a
time, and `write_raw` lives **only on that guard**. While it is held, `send_line`
returns `AgentError::Attached` instead of queueing. Refusing is the point —
orchestrated input landing in a session a person is driving is the exact fleet
incident this design exists to prevent. "The core is busy" is information the
caller can act on; a silently interleaved keystroke is not.

### Widening an atom does not repeal invariant 1

Invariant 1 was first written as "there is no public raw write", which described
the one atom that existed at the time rather than the property. Widening the atom
to any byte burst (2026-09-10, for the Lua input vocabulary) left the property
untouched, but made the earlier wording read like a repeal.

The property is the *atomicity of one input act*, not the absence of raw bytes. A
burst is written under a single lock acquisition with nothing awaiting inside it,
so a second sender still cannot land in the middle of one. The operation that
does not exist is a **divisible** write, and it still cannot be written. What
genuinely changed is that a half-typed line can now be left sitting at a prompt —
a script doing a deliberate thing, in the same class as `sh script.sh`, and not
the concurrency defect the type was built against.

⇒ When restating this invariant, name **divisibility**, never "raw".

### Widened again, 2026-09-11: an act can be several bursts and a pause

`feed` lets one input act be a sequence of bursts with a pause between them —
for a caller that has to type, wait, then submit, and needs that whole
sequence to still be one act. "Nothing awaiting inside it" still names the
*agent* lock a `Burst` briefly holds to write; `screen_text`/`capture` never
wait longer than one burst takes, pause or no pause. What now spans the whole
act, pause included, is a second lock, `input_lock` — the thing a second
sender cannot land inside at any point in the sequence. That is `feed`
widening the same **divisibility** property a second time, not repealing it.

Attachment is re-checked on *every* burst, not once at the act's start: a
human can `attach` mid-pause, and the next burst must see that and refuse
rather than finish delivering to a session it no longer has any business
touching. `attach` itself takes neither lock, so an act sitting in a pause
cannot make it wait — it simply wins the race, and the act's remaining bursts
lose theirs.

**Enforced by:** the type system (the method does not exist) · test
`concurrent_send_lines_never_interleave` and its negative control
`control_unlocked_writers_do_interleave` · the wider act's own divisibility by
test `feed_bursts_and_pauses_are_one_indivisible_act` and its negative control
`control_separate_calls_with_a_gap_do_interleave` · a screen read never
waiting on a pause, by test `capture_does_not_wait_out_a_feed_pause` · the
attach race, by test
`attaching_during_a_feed_pause_refuses_the_remaining_bursts`

## 7. Time is injected, never read

Nothing in the policy layer calls a global clock. `Clock` is a parameter, and it
is monotonic — every question asked of time here is "how long since", never
"what date is it".

**Why.** Interleaving and timeout behaviour is only testable when a test can
advance time deliberately, and retrofitting injection means rewriting every call
site. Built on day one, before there were call sites.

**Enforced by:** `core/clippy.toml` (`std::time::Instant`, `std::time::SystemTime`,
`std::thread::sleep` are denied) · CI job `lint`

## 8. Set thresholds while the code is small

Size limits go in before the code outgrows them.

**Why.** A limit chosen after the fact is a description, not a limit — it gets
set wherever the code already is. These were set at three source files.

**Enforced by:** `core/clippy.toml` (`too-many-arguments-threshold`,
`too-many-lines-threshold`, `cognitive-complexity-threshold`) · CI job `lint`

## 9. State the outcome before building, and capture it after

Every unit of work gets a `steps/NNN-name.md`: **Before** (what is true now and
what is wrong with it), **Desired outcome**, **Expected**, and **Actual** —
where Actual is captured output, not a report of it.

**Why.** 정수님 asked for this on 2026-09-10, and it paid on the same change.
Building the daemon, "it seems to work" became two specific bugs the moment an
expectation was written down first: auto-start swallowed the daemon's stderr and
reported a timeout instead of `path must be shorter than SUN_LEN` — a wall claim
naming the wrong wall — and detaching **deadlocked**, leaving a session locked to
a viewer that had already gone, because the input and output pumps block on
different things and a socket shutdown cannot wake a `recv()`. Both compiled
cleanly and read correctly.

Principle 4 has the same shape one level down: the live-pty tests were caught by
noticing 0.05s was impossible for six shell spawns — an *actual* that failed an
expectation nobody had written. Writing it down is what makes that catch routine
instead of lucky.

The fenced block in **Actual** is the load-bearing part. Prose can be written
from intention; captured output cannot. For a terminal tool, pasted output is
what a screenshot is for a UI.

**Enforced by:** CI job `steps-are-documented` · CI job `gates-can-fail` · `scripts/check-steps.py`

## 10. The embedded language's API is frozen by a script, not by intent

Every version of the Lua surface has a script written against it in
`native/tests/api/`, and CI runs all of them on every commit. Widening the API
means adding `v2.lua`; it never means editing `v1.lua`.

**Why.** 정수님, 2026-09-10: *"그 언어 API 에 대고 사용자들이 자기 함수를 얹어서
설정하거나 플러그인, 워크플로 등을 만들면, 하위호환을 엄격하게 지켜야 합니다."*
The incident is this repo's own step 004, the change that introduced the rule:
`BINDINGS` grew from six names to nine, `send_line`'s write pattern changed
underneath it, and **nothing in the build could have told the difference between
that and a rename.** The existing surface test compares the live table against
`BINDINGS` — so a developer renaming a binding *and* its constant passes it,
which is precisely the change that breaks every script already written.

That is the hole this closes, and the negative control in `gates-can-fail`
plants exactly that rename to prove it: the surface test stays green and the
frozen script goes red.

The freezing is what makes it a control rather than a ceremony. A compatibility
check you may edit to make it pass checks nothing — so if a change genuinely
cannot keep `v1.lua` running, the honest act is to delete the file in the open,
not to adjust it.

**Enforced by:** CI job `test` (runs `every_frozen_api_version_still_runs`) ·
CI job `gates-can-fail` · `native/tests/api/v1.lua`

## 11. The two ends of a download must be checked against each other

The release workflow writes an asset and the install script asks for one. Those
are a YAML matrix and a shell script, so nothing in a normal build compares them
— and when they disagree, the only place it appears is a stranger's terminal,
as a 404, from a one-liner the README promised.

**Why.** Measured 2026-09-10, building this. Two mistakes of exactly that shape,
both invisible to every gate the repo already had and both found only by
installing for real against a locally served release:

```
sha256sum ./*.tar.gz    wrote "<hash>  ./remuda-…"   the installer grepped for "<hash>  remuda-…"
curl … | sh             a pipeline reports SH's status, so a 404 fed an empty
                        script to a shell that exited 0 — a failed upgrade
                        reported as a finished one
```

The first is now a name comparison rather than a pattern match, and the second
lands the script on disk before running it. Neither bug was reachable by reading
the code; both were obvious the instant a real binary was fetched and unpacked.

⇒ The mechanism covers the axis a mechanism *can* cover — that both files name
the same set of platforms and the same asset shape. The rest is the discipline
below, and it is stated separately rather than folded in, because a gate that
covers one axis of a two-axis failure reads as if it covered both.

**Enforced by:** CI job `install-path-is-consistent` · CI job `gates-can-fail` ·
`scripts/check-install.py`

## 12. An installer is only proven by installing

Before shipping a change to the release path, serve a real release locally and
run the real install script against it — both channels, and a deliberately
corrupted tarball.

**Why.** Both defects in principle 11 were found this way and could not have
been found otherwise: the checksum-name mismatch needs a `SHA256SUMS` file that
a machine actually wrote, and the pipeline-status bug needs a URL that actually
404s. Reading either line makes it look correct.

**Enforced by:** nothing — discipline only. *(The proof needs a built release
for three platforms and a server; CI has those only on a release run, which is
after the merge that would ship the break. `install-path-is-consistent` catches
the drift axis on every commit, which is the part that can be automated. The
manual rehearsal, and its captured output, is in `steps/009-versioning-and-install.md`.)*

## 13. A gate can fail by being absent, and that looks like nothing at all

§2 guards against a gate that never goes red. This is the worse case: a gate
that never **runs**, while the build still reports.

**Why.** Measured 2026-09-10, found while opening the PR that added §11. A
heredoc body written at column 0 inside an indented `run: |` block **ends the
YAML block scalar it lives in**, so `ci.yml` stopped parsing. GitHub then
started *zero* jobs — and rendered the run as an ordinary red X:

```
completed  failure  comments: cap doc comments…      ci.yml  main  0s   ← zero jobs
completed  failure  readme: lead with what it is…    ci.yml  main  0s
completed  failure  readme: a hero image…            ci.yml  main  0s
completed  success  feat: the input vocabulary…      ci.yml  main  1m15s ← the last real run
```

Every gate in this repository was dead for twelve hours, `gates-can-fail`
included — the job whose whole purpose is to notice a guard going quiet. It
could not, because it was one of the jobs that did not exist. And the change
that broke it was itself a change to `gates-can-fail`.

The tell is the **0s duration and an empty job list**, not the colour. Red meant
"a test failed" to every reader, and the file had simply stopped being a file
GitHub could read.

⇒ The check cannot live in `ci.yml`: a workflow cannot verify that it itself
still parses, because if it does not, the verifying job is not created either.
So `workflow-guard.yml` is a separate, deliberately tiny file. **Its own honest
limit: if that file breaks, nothing catches it** — the recursion stops
somewhere, and the useful move is to stop it at a file nobody edits.

**Enforced by:** CI job `workflows-parse` · CI job `gates-can-fail` ·
`scripts/check-workflows.py`

## 14. A terminal that only reads is half a terminal

`remuda-native` is the terminal emulator for every session it holds. An emulator
that parses output and never answers the terminal protocol's queries is not a
smaller emulator — on some hosts it is a hung one.

**Why.** Measured 2026-09-10, porting to Windows. Everything compiled, the daemon
bound its pipe, `remuda ls` reported the session `live` — and the screen was
blank forever, for `sh`, `cmd` and `powershell` alike. The entire output of the
pty was four bytes:

```
PROBE stream: "\u{1b}[6n"     ← Device Status Report: where is the cursor?
```

ConPTY asks that **before emitting anything** and waits for the reply, because
`portable-pty` opens the pseudoconsole with `PSUEDOCONSOLE_INHERIT_CURSOR`. Unix
ptys never ask, so three years of unix use could not have found it. The reader
thread now answers `ESC[<row>;<col>R` from the grid it already maintains.

The general form: **a pty is a conversation, not a feed.** When a program's
output stops without its process dying, look for a query that went unanswered
before looking for a bug in the pump.

⚠ The first measurement was of the wrong thing and looked identical. Dumping
`screen_bytes()` renders the *grid*, so an empty grid and an empty stream print
the same. The stream had to be teed raw before the four bytes appeared — the
same shape as principle 4, one layer down.

**Enforced by:** nothing — discipline only. *(The unanswered-query state is a
hang, not a wrong value, and it only occurs on hosts that ask. CI job
`test-windows` is what would go red today, because every session-level test
there depends on the reply arriving — but that is coverage of one host, not a
mechanism that would catch the next query we fail to answer.)*

---

## Adding a principle

1. It must come from something that **actually happened here**. Name the
   incident and the date. A principle without an incident is a preference.
2. Fill **Enforced by:** with a mechanism that exists, or write
   `nothing — discipline only` and say why no mechanism fits. Do not write a
   promise.
3. If you add a mechanism, add its negative control to `gates-can-fail` in the
   same change — otherwise principle 2 is violated by the act of adding it.
