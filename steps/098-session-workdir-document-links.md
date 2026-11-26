# 098 — Open documents from a session workdir

## Before

Remote terminal links needed an exact, separately configured file alias.
Document publication required a separately held publisher credential.

## Desired outcome

A session on an unattended node can share a new document from its launch
directory and get a display receipt from the operator's Browser. The source
node can deny reads immediately by session or for the entire node.

## Expected

An admitted TUI and a Browser SSH workspace read the new file from its
source, then lose access after opt-out. An ordinary session sends a
document request through the existing relay, the viewer reads the same
source instance, and the result returns with the original request and
session identity.

## Actual

```text
test remote_tui_click_opens_new_session_workdir_file_then_honors_opt_out ... ok
test workdir_document_publication_reaches_the_browser_with_its_verified_origin ... ok
test result: ok. 72 passed; 0 failed; 7 ignored; 0 measured; 0 filtered out; finished in 8.12s
```

The first test uses real isolated daemons, a terminal UI and the fixed SSH
workspace command over test stdio. The second drives real listeners with a
Browser protocol client. The GUI test suite renders using GPUI's test
platform. Windows GNU GUI binaries and tests were compile-checked on macOS.
