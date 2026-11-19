# 088 — No-Python gate reads command position

## Before

`check-no-python.sh` rejects any file that contains `python`, `pip` or `uv` as
a token anywhere, so it also rejects data: a language tag in a Markdown fence, a
`"python"` match arm in a tokenizer, a `no-python` CI job name, a `python_token`
variable. The GUI library carries exactly those literals, so every PR that adds
it fails a gate whose purpose is to stop execution.

## Desired outcome

The gate still rejects Python execution in all its planted forms, but a token
that is only data passes: a quoted language tag, a match arm, a job name, or a
name built from split quotes. Classification is by command position and by an
execution sink, not by the token appearing on a line.

## Expected

- Every planted bypass in `scripts/test-no-python.sh` stays rejected, including
  `Command::new("...")`, `subprocess.run([...])`, `sh -c "..."`, `eval "..."`
  and a quoted command piped to `sh`.
- A clean fixture with `Some("python")`, `tokens(line, "python")`, a
  ```` ```python ```` fence, a `no-python` job and a `python_token` variable
  passes.
- The real tree passes, and so does the GUI tree that used to fail.

## Actual

New green and red cases were added to `scripts/test-no-python.sh`; the real tree
and the defined-but-unopened GUI branch both pass:

```text
$ bash scripts/check-no-python.sh
ok — Python files and invocations match the documented temporary allowlist

$ sh scripts/test-no-python.sh
ok — Python files and invocations match the documented temporary allowlist
ok - every planted #605 bypass is rejected
```

The GUI tree (`gui-lineage/6c-gui-documents`, the same `highlight.rs` and
`markdown.rs` as the other GUI slices) was checked with this script copied in and
no longer reports any line:

```text
$ bash scripts/check-no-python.sh
ok — Python files and invocations match the documented temporary allowlist
```
