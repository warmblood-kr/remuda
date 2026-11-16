# 089 — Read a Markdown document as prose

## Before

step 086 taught the document browser to read `.html`/`.htm` through an inert
text/link projection, but only those two extensions. A `.md` file — the format
most remuda documents are written in — is therefore displayed as source: `#`
markers, `**bold**`, and `[text](uri)` remain literal, and a local link is not
selectable. The same browser shows prose for HTML and markup for Markdown, which
is a difference by accident, not a contract.

## Desired outcome

Open `docs/guide.md`, read its heading, paragraphs, quote, list items and code
block as prose with no Markdown delimiters, press Tab to select `next.md`, press
Enter to read it, and press Esc to return to the prior document with its link
selection intact. Image references show an explicit `[image: alt]` placeholder
because this reader never loads image data.

## Expected

`native/src/document_markdown.rs` projects Markdown into the existing
`document_html::Document` shape, so link selection, address inspection,
history, refresh fencing and the rendered receipt keep one implementation.
`Browser.page.projection` replaces the HTML-only field name; plain text still
takes the raw path and `.html`/`.htm` still use `document_html::parse`.

The projection keeps the source's line shape instead of re-flowing: ATX headings
lose their markers (a single `#` sets the title), fenced code (```` ``` ```` /
`~~~`) is copied verbatim with no inline processing, `>` becomes a `| ` quote
line, list markers stay visible and `*`/`+` bullets become `-`, thematic breaks
become a blank line, reference definitions are hidden while their links resolve,
code spans are copied without backticks, emphasis delimiters are removed, and
inline links become `label [n]`. Every link resolves through the same
`document_html::resolve`, so relative, root and `remuda:///` targets work and
external/scheme/fragment/query/unescaped-control targets stay inert with a
reason. Images become `[image]` / `[image: alt]`; embedded HTML is left literal,
never executed or parsed.

Limits: the existing 1 MiB document read cap, 4092 output rows (explicit error,
matching HTML's block cap) and 128 links. Terminal controls and bidi overrides
are stripped before display; no new dependency, protocol, listener or CLI is
added.

## Actual

```text
cargo test --offline -p remuda-native --lib document_markdown -- --test-threads=1
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 1946 filtered out

cargo test --offline -p remuda-native --lib documents:: -- --test-threads=1
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 1938 filtered out

cargo test --offline -p remuda-native --lib cluster_tui::tests:: -- --test-threads=1
test result: ok. 192 passed; 0 failed; 0 ignored; 0 measured; 1758 filtered out

env TMPDIR=/private/tmp cargo test --offline -p remuda-native --test cluster_tui documents_read_markdown -- --test-threads=1
test documents_read_markdown_as_prose_and_follow_its_local_link ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 11 filtered out

env TMPDIR=/private/tmp cargo test --offline -p remuda-native --test cluster_tui -- --test-threads=1
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; finished in 54.33s

cargo fmt --all -- --check: passed
cargo clippy --offline --workspace --all-targets -- -D warnings: passed
```

The integration test started a private daemon, published `docs/guide.md` and
`docs/next.md` through the existing VFS request, opened the browser with `d`,
entered `docs/`, read `Read this next [1].` with no `# Guide` or `**this**` on
screen, selected the link with Tab (status showed
`remuda:///docs/next.md`), pressed Enter to show `Second page prose.`, and
pressed Esc to return to the guide before closing the browser. The unit tests
cover heading/fence/quote/list projection, inline and reference links including
a blocked `javascript:` target and an image placeholder, literal fallbacks,
control/bidi stripping, and both limits. In the end-to-end test and the model
test, the guide and its target share one directory so selection does not depend
on directory-first ordering.

Limitations, stated plainly: this is not CommonMark — setext headings, tables,
task-list checkboxes and HTML blocks are left literal; emphasis pairs must match
in delimiter length and do not cross a code span; an image is a placeholder, not
a preview; no GUI/WebView is included, and a GUI must project and acknowledge
its own frame. Runtime evidence is macOS only. The prior checkout, live
sessions, shared daemon and user screen were not touched, and nothing was
merged, deployed or published.
