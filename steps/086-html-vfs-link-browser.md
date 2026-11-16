# 086 — Read HTML and follow a relative VFS link

## Before

HTML is displayed as source text. Users cannot follow its links inside the
document browser. Existing plain reading, address inspection, history and late
reply fences must remain intact.

## Desired outcome

Open docs/index.html, read its title and paragraphs with decoded entities,
select a relative link with Tab/Shift-Tab, inspect its full target with `a`,
press Enter to read docs/sub/next.html, and Esc back to the prior document and
selection. Exit to the original session draft. Unsupported links remain inert
with an explanation; browser input/paste never reaches the session.

## Expected

Use a real HTML5 parser, not tag stripping. scraper 0.29.0 is pinned with default
features disabled; it uses html5ever 0.40.1 and ego-tree 0.11.1. The shared native
projection contains a title, paragraph lines and labeled typed links. GUI can
reuse address resolution and navigation semantics without adopting a TUI DOM.
No GUI renderer/WebView or image/resource loading is included.

Only .html/.htm files are projected. Scripts, style, templates, hidden elements,
iframes, SVG and MathML subtrees are omitted; no JS/CSS/forms are executed. HTML
source is capped at 128 KiB before parsing; the resulting tree is rejected above
20,000 nodes, links above 128, and text above 4092 block rows. Plain files retain
their existing 1 MiB limit. These are resource limits, not a parser CPU deadline.

Relative, root and remuda:/// local VFS targets decode percent encoding once,
normalize dot segments, reject root escape and validate the final path. No base
element can redirect resolution. External schemes/authorities, controls,
backslashes, fragments and queries are blocked with reasons. Fragment support
is deferred; hashes are never silently discarded. Tab cycles references while
arrows scroll; Enter activates the selected link, and history retains scroll
and link selection. Full address inspection targets the same selected link and
keeps the existing refresh-generation fence.

## Actual

```text
cargo test --offline -p remuda-native --lib document_html:: -- --test-threads=1
test result: ok. 5 passed; 0 failed
cargo test --offline -p remuda-native --lib documents:: -- --test-threads=1
test result: ok. 10 passed; 0 failed; finished in 0.06s
cargo test --offline -p remuda-native --lib keybindings:: -- --test-threads=1
test result: ok. 11 passed; 0 failed; finished in 0.14s
cargo test --offline -p remuda-native --lib cluster_tui::tests:: -- --test-threads=1
test result: ok. 192 passed; 0 failed; finished in 8.46s
env TMPDIR=/private/tmp cargo test --offline -p remuda-native --test cluster_tui -- --test-threads=1
test result: ok. 9 passed; 0 failed; finished in 40.39s
cargo deny --offline check bans licenses sources
bans ok, licenses ok, sources ok (existing duplicate-version warnings)
cargo check --offline -p remuda-native --lib --target x86_64-pc-windows-gnu
Finished `dev` profile in 8.85s (7 warnings in unchanged code)
```

The new private-daemon/PTY test followed the complete HTML path above, checked
an inert fragment, returned the draft and checked paste isolation. Initial test
assumptions about directory-first ordering and waiting for the address status
versus the inspection screen were corrected to match observed behavior. Unit
tests cover malformed HTML/entity handling, unsafe links, limits, Back and an
HTML refresh replacing an inspected link. Runtime evidence is macOS only;
Windows document requests remain explicitly unsupported. No remote-only blob,
GUI, image, fragment jump, Markdown rendering or deployment was exercised.

Dependency fetch added 23 locked packages. Their manifest licenses are ISC,
MIT/Apache-2.0, or MPL-2.0; the latter covers cssparser, cssparser-macros,
dtoa-short and selectors. deny.toml explicitly permits adding a new non-GPL
license to its closed list, so MPL-2.0 was added. cargo-deny 0.20.2 was installed
under a temporary tools root and the license/source/bans gate passed.

Sources: [scraper](https://docs.rs/scraper/0.29.0/scraper/),
[html5ever](https://docs.rs/html5ever/0.40.1/html5ever/).
The RcDom alternative was excluded because its own
[documentation](https://docs.rs/markup5ever_rcdom/latest/markup5ever_rcdom/)
warns against production use with untrusted inputs. Exact package manifests and
license files were inspected after fetching; Cargo.lock records checksums.
