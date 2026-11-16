# 083 — A shared inert HTML/Markdown/text document projection

## Before

This line (the GUI-capable `feat/gui-local-live-session` tree) has no document
browser at all: there is no `documents` module, no HTML or Markdown projection
and no `scraper` dependency. Documents can only be read as raw bytes by hand,
so a GUI or TUI that wants to show one has nothing to share.

## Desired outcome

The browser core from the document-browser line (commit a06e6e5c) is ported
unchanged in meaning, so this tree can list VFS directories and read plain text,
HTML and Markdown as inert, bounded text with local links.

## Expected

`native/src/documents.rs`, `document_html` (+`links`) and `document_markdown`
are copied from a06e6e5c, with one call-site adaptation: the candidate exposes
`client::request_with_timeout` instead of `request_with_deadline`. The pinned
`scraper 0.29.0` dependency is added with default features disabled and `MPL-2.0`
is added to the closed license allow-list in `deny.toml` (step 086 did the same
on the other line). Plain text keeps its 1 MiB read cap; HTML keeps its 128 KiB
cap, inert element set and 20,000-node/128-link limits; Markdown keeps its
bounded inline work budget, first-wins reference definitions, sanitized shared
labels and the "caller bounds the source" contract. No new daemon protocol.

## Actual

```text
cargo test --offline -p remuda-native --lib document_markdown -- --test-threads=1
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 1943 filtered out; finished in 0.17s

cargo test --offline -p remuda-native --lib documents:: -- --test-threads=1
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 1941 filtered out; finished in 0.05s

cargo test --offline -p remuda-native --lib document_html:: -- --test-threads=1
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 1948 filtered out; finished in 0.06s

cargo clippy --offline --workspace --all-targets -- -D warnings \
  -A clippy::type_complexity -A clippy::cognitive_complexity -A clippy::too_many_lines
Finished `dev` profile [unoptimized + debuginfo] target(s) in 10.22s
```

The three allowed lints fire only in files this port does not touch
(`native/src/cluster_tui.rs:452`, `native/src/session_host/human_wire_tests.rs`)
and are present at the candidate's HEAD; `git status` shows no modification to
them. Runtime evidence is macOS only.
