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

**Enforced by:** the type system (the method does not exist) · test
`concurrent_send_lines_never_interleave` and its negative control
`control_unlocked_writers_do_interleave`

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

---

## Adding a principle

1. It must come from something that **actually happened here**. Name the
   incident and the date. A principle without an incident is a preference.
2. Fill **Enforced by:** with a mechanism that exists, or write
   `nothing — discipline only` and say why no mechanism fits. Do not write a
   promise.
3. If you add a mechanism, add its negative control to `gates-can-fail` in the
   same change — otherwise principle 2 is violated by the act of adding it.
