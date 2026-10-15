# 040 — cluster input editing and submission

## Before

The cluster composer sends text plus CR as one raw Input burst, while regular
SendLine uses text delivery followed by Return after screen observation. The
draft clips at the right edge and its inserted cursor marker shifts suffix
glyphs. Emacs start/end/kill-tail controls are absent.

## Desired outcome

Reuse the session text/Return path through an additive idempotent InputLine.
Consume a composer's trailing CR and deliver one Return without an automatic
second Return. Wrap/scroll the draft around a cursor that does not move its
glyphs; edit grapheme boundaries with registry-owned Emacs controls. Resize the active
terminal to the visible pane through an instance-pinned ResizeSession request,
including short viewports, preserving ordinary size defaults.

## Expected

Protocol roundtrips preserve old Input bytes. Instance, control, size and
dedup checks still apply. A scripted terminal receives text before one Return;
a retry writes neither again. Long Unicode drafts remain visible and editable.
Paste inserts at the cursor. Invalid/oversized drafts queue nothing and remain
editable; raw input transport retains its old behavior. Rendered pane dimensions
match the resize request in stacked and split layouts. Unchanged sizes are sent
once; window/draft/instance changes propagate, stale instances cannot resize a
replacement, and read-only/ended sessions are skipped.
Pasted CRLF and tab characters remain visible as control glyphs in the draft,
and the queued `InputLine` contains the same pasted characters. A receiver's
pre-dispatch `invalid remote request` response is reported as unsupported with
no retry, raw-input fallback or partial-delivery warning.

## Actual

Bypassing the line-submit mode with the old raw-batch path made the Return
oracle fail, demonstrating that it observes actual delivered bytes:

```text
left: [104, 101, 108, 108, 111]
right: [104, 101, 108, 108, 111, 13]
test result: FAILED. 0 passed; 1 failed
```

With the line path restored, captured daemon-free checks include:

```text
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.09s
test result: ok. 148 passed; 0 failed; 0 ignored; 0 measured; 1036 filtered out; finished in 3.14s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1182 filtered out; finished in 0.00s
```

Regression tests also cover CRLF/tab paste display and delivery, plus an old
receiver's exact `invalid remote request` refusal for `InputLine` and
`ResizeSession`. The refusal produces a clear unsupported result without
retries, raw-input fallback, or a partial-delivery warning.

These are core input tests, pure/mock cluster tests (including composer, sender
and queue), and resize protocol/control tests. The full core suite, keymap
tests, line protocol/control tests and all-target Clippy also pass. The
scripted rendered-terminal test checks separate text and Return writes after
echo, exactly one Return even on a composer newline, and no writes on retry.
Pane tests compare resize dimensions with rendered body rows and divider
columns, then exercise draft/layout/instance changes and failed retries.

No daemon or live agent was launched. New requests require an upgraded
receiver and have no write-API fallback. Remote composed lines are limited to
one 12 KiB batch; raw paste retains chunking. Local line submission can pause UI
updates during bounded screen observation. Remote resize requires control;
multiple viewers may compete for dimensions, and size is not restored on exit.
