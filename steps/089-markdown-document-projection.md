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
matching HTML's block cap) and 128 links. Inline scanning carries a per-line work
budget of eight inspected characters per input character, so unmatched `[`,
backtick and emphasis scans cannot become quadratic; when the budget runs out
the rest of that line stays literal instead of being parsed. A thematic break
requires three or more of one marker and nothing but whitespace, so
`- keep --- text` is a list item, not a vanished line. Reference definitions are
collected only outside fenced blocks, so `[id]: target` inside code cannot
resolve a reference outside the fence; when an id is defined twice the first
destination wins, matching Markdown's first-definition rule. Link labels are
shared data for any adapter, so they are stripped of terminal controls and bidi
overrides and have whitespace collapsed before they reach `Document.links`,
matching the HTML projection's existing cleaning rather than relying on a
particular TUI to clip them later. Terminal controls and bidi overrides are
stripped before display; no new dependency, protocol, listener or CLI is added.

The GUI seam is dry-fitted, not implemented: `gui/` is a separate workspace
crate that depends on `remuda-native`, so `cargo check` there proves the
`Page.html` → `Page.projection` rename compiles for its existing consumers
(none construct `Page` or read `.html`), and `gui/tests/document_projection_seam.rs`
consumes the Markdown/HTML projection and the shared resolver from the GUI
crate. `gui/src` still has no document view, so it neither renders a document
nor emits a presentation receipt.

## Actual

```text
cargo test --offline -p remuda-native --lib document_markdown -- --test-threads=1
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 1946 filtered out

cargo test --offline -p remuda-native --lib documents:: -- --test-threads=1
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 1938 filtered out

cargo test --offline -p remuda-native --lib cluster_tui::tests:: -- --test-threads=1
test result: ok. 192 passed; 0 failed; 0 ignored; 0 measured; 1758 filtered out

env TMPDIR=/private/tmp cargo test --offline -p remuda-native --test cluster_tui documents_read_markdown -- --test-threads=1
test documents_read_markdown_as_prose_and_follow_its_local_link ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 11 filtered out

env TMPDIR=/private/tmp cargo test --offline -p remuda-native --test cluster_tui -- --test-threads=1
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; finished in 50.86s

cd gui && cargo check --offline
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.44s

cd gui && cargo test --offline --test document_projection_seam
test gui_can_consume_the_shared_markdown_and_html_projection ... ok
test result: ok. 1 passed; 0 failed

cargo fmt --all -- --check (workspace and gui): passed
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
control/bidi stripping, both limits, malformed and nested labels, thematic-break
boundaries, a fenced definition that must not resolve outside its block, and
four adversarial long lines (600 KB each of unmatched `*`, `[`, backtick and
mixed delimiters) that finish in 0.18s for the whole module suite instead of
scanning the suffix per delimiter. In the end-to-end test and the model
test, the guide and its target share one directory so selection does not depend
on directory-first ordering.

Limitations, stated plainly: this is not CommonMark — setext headings, tables,
task-list checkboxes and HTML blocks are left literal; emphasis pairs must match
in delimiter length and do not cross a code span; an image is a placeholder, not
a preview; malformed lines beyond the work budget stay literal. Bracket matching
counts nesting but does not interpret escapes, so an unbalanced `[` inside a
malformed label (including a raw `ESC[31m`-style sequence) can let an inner
balanced pair become the link instead; the bytes are never executed and the
label is still sanitized before it is shared. No GUI/WebView
document view is included: the seam compiles and is tested from the GUI crate,
but rendering and frame acknowledgement remain the GUI adapter's own work. Runtime evidence is macOS only. The prior checkout, live
sessions, shared daemon and user screen were not touched, and nothing was
merged, deployed or published.
