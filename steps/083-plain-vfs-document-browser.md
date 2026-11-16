# 083 — Read VFS documents from the cluster browser

## Before

The cluster tree navigates sessions and sends composer/raw-key input, while VFS
documents require a separate CLI or HTTP bridge. There is no document focus,
history or text reader in the cluster TUI. The requested first slice is a
complete list → read → back → session-return path; HTML/WebView and notification
panels remain later work.

## Desired outcome

Press `d` from the session tree, or `Alt-d` from a composer, to browse the VFS.
Open an immediate child directory or UTF-8 document with Enter, scroll with
arrows, use Esc to go back and `q` to restore session focus and the saved draft.
Browser keys and paste never become session input. Replies from an abandoned
navigation never replace the restored page.

## Expected

Use local daemon `Vfs` requests, immediate-child `ls --json`, bounded `read`
envelopes and one asynchronous request at a time. Share address/navigation
state independently of the TUI adapter. Cap documents at 1 MiB, history at 32
pages and visible text at 4096 rows with a limit marker. Display read errors
and offer back/retry. Preserve existing session navigation and delivery.

The browser is read-only; fetching a remote blob may cache it locally. A
three-second UI request deadline does not cancel work already in the daemon.
HTML/Markdown are displayed as source; image rendering is not implemented.

## Actual

The original focus acceptance failed because `d` did not open Documents, then
passed after integration. The private-daemon/PTY test traversed `docs/one.txt`,
read its fixture, ignored bracketed paste and browser keys, restored and
submitted the original draft, and checked captured session output. It also
SIGSTOPped the private daemon, started a read, went Back, resumed the daemon,
and observed that the late reply did not reopen the document.

```text
cargo test --offline -p remuda-native --lib cluster_tui::tests::
test result: ok. 192 passed; 0 failed; 0 ignored; 0 measured; 1734 filtered out; finished in 3.17s

cargo test --offline -p remuda-native --lib keybindings::
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 1915 filtered out; finished in 0.03s

cargo test --offline -p remuda-native --lib documents::
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 1923 filtered out; finished in 0.01s

env TMPDIR=/private/tmp cargo test --offline -p remuda-native --test cluster_tui -- --test-threads=1
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 26.04s

cargo clippy --offline --workspace --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 10.08s
```

The library prerequisite was also tested at its own commit: 3 document tests
passed. Full integration exposed old prompt/status whitespace assertions in
the stopped-daemon test; its prompt failure was reproduced before the TUI
integration and the expectations were updated to existing product output.
Clippy required factoring key dispatch and naming the existing roster tuple.
macOS tests use a short TMPDIR for Unix socket paths. No full workspace test
suite, Windows runtime, GUI/WebView, remote-only blob PTY path or deployment was
exercised. The document panel is currently a focused full-screen TUI mode;
notification integration and simultaneous panels remain separate slices.
