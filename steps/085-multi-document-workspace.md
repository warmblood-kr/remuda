# 085 — Multi-document workspace with tabs, split and rendered Markdown

## Before

The GUI document pane (`gui/src/documents.rs`) held exactly one `Pane`, which
wrapped one `Browser`, and opening documents replaced the terminal area
(`documents_open` hid every session, terminal and preview branch). There was no
way to compare two documents, and the Markdown projection was painted as plain
lines without visual hierarchy. Code files had no colour. GPUI 0.2.2 has no
webview element.

## Desired outcome

One application screen keeps the session tree, the terminal/session area and a
document panel visible together. The panel opens several documents in tabs,
switches and closes them, and shows at least two side by side, each with its own
browser location/history/selection. Markdown gets a native visual hierarchy
(headings, list markers, quotes, fenced code, inline bold/italic/code/link),
local VFS images render directly and from Markdown, and code files get bounded,
inert token colour. No webview, script, external network or arbitrary resource
load is introduced; the TUI keeps its text placeholders.

## Expected

- `Workspace` in `gui/src/documents.rs` owns `Vec<Pane>`, an active index and an
  optional split index; each tab owns its `Browser`. Tabs are capped at 16 and
  the workspace never drops below one tab. Clicking the split pane focuses it
  and keeps the split.
- The document UI lives in `gui/src/document_workspace_view.rs`; `main.rs` only
  declares the module, holds `documents: Option<Workspace>`, implements a
  three-method `DocumentHost` trait, adds one `.child(...)` to the outer row,
  and removes the `documents_open` guards that hid the terminal.
- `gui/src/markdown.rs` parses the bounded Markdown source (from
  `Page.markdown_source`, one shared `Arc<str>` per page) into blocks and spans:
  ATX headings, fences with language, quotes, list marker/indentation, rules,
  paragraphs, inline bold/italic/code/link/image placeholder. Inline links keep
  their raw destination; display text and fenced code pass through the shared
  `strip_terminal_controls`, so control/bidi characters never reach the GUI.
  Image destinations are placeholders only in this step.
- `gui/src/highlight.rs` remains a pure line tokenizer for Python, Rust,
  JavaScript/TypeScript, shell, JSON, TOML and YAML; token ranges only.
- `gui/src/media.rs` accepts PNG/JPEG by magic bytes, caps the encoded image at
  8 MiB, dimensions at 8192 and pixels at 16M, and checks the header dimensions
  before decoding pixels. Decoder allocation is bounded at 160 MiB, enough for
  the 64 MiB output plus one RGBA->BGRA conversion buffer. `native::documents::
  read_media` performs the same bounded VFS read and identity/size checks for
  binary content.
- Image entries and decoded bytes are admitted by one `ImageBudget` shared by
  every tab: at most 8 entries per pane, 32 entries and 128 MiB of decoded
  pixels per workspace, with an entry reserved before a read so failed paths
  cannot grow the map. Actual decodes are serialized by the budget across all
  tabs. The worker reserves the decoded bytes **before** sending the result
  through its channel, and an RAII reservation releases them when the state is
  dropped, a refresh supersedes it, or the receiver is gone; a queued result the
  UI has not polled still counts against the shared budget. A direct `.png`/`.jpg`/`.jpeg` document shows the image with
  `IMAGE · W×H`; Markdown `![alt](local.png)` resolves through the shared VFS
  resolver and renders the local image. Missing, undecodable, unsupported,
  external or over-budget images show alt/error text; nothing is fetched over a
  network. An explicit refresh releases the pane's entries/bytes, resets the
  scan and ignores in-flight results from the previous revision, so changed
  pixels are not reused.
- Rendering maps headings to larger type, list markers to a muted column,
  fenced code to a monospace tinted block with per-line tokens, links to colour
  plus underline, inline code to a tinted run, and bold/italic to font weight
  or style.
- Each safe link is its own clickable element. The destination is resolved by
  the shared `document_html::resolve` (relative to the displayed document) and
  opened in the same pane through `Pane::open_path`, so the shared Browser
  history provides Back. A blocked target renders muted with `(blocked)` and
  has no click handler. Keyboard `l`/`h` selects the next/previous shared
  Browser link and `Enter` follows it.
- Tests: private-daemon `workspace_flow` (two documents, independent state,
  split, close, bounds); TestWindow tests for tab/split/close/right-pane focus
  and for Markdown block selectors plus heading-over-paragraph height; pure
  Markdown/Highlight unit tests. A real desktop window remains out of scope.

## Actual

```text
cargo test --offline --manifest-path gui/Cargo.toml
  lib: 13 passed
  bin remuda-gui: 57 passed, 5 ignored
  document_flow: 1 passed
  workspace_flow: 1 passed
  acceptance_supervisor: 1 + 1 + 1 + 2 passed, 4/4/4/3 ignored
  build_id: 1 passed, 1 ignored
  nodes_worker: 1 passed
  worker_abi_transition: 0 passed, 1 ignored
  worker_deadline: 1 passed

cargo test --offline -p remuda-native --lib documents::
  13 passed

cargo clippy --offline --manifest-path gui/Cargo.toml --all-targets -- -D warnings
Finished

cargo check --offline --manifest-path gui/Cargo.toml \
  --target x86_64-pc-windows-gnu --tests
Finished

python3 scripts/check-steps.py        ok
python3 scripts/check-comments.py     ok
python3 scripts/check-principles.py   ok
```

The image TestWindow test puts a 3×2 PNG in a private VFS: opening it directly
shows `IMAGE · 3×2` and `document-image-left`; `pics.md` with
`![art](art.png)`, `![bad](missing.png)` and `![ext](https://example.com/x.png)`
renders `md-image-left-1-0`, marks the missing image
`md-image-failed-left-2-0`, and marks the external image
`md-image-blocked-left-3-0`. After replacing `art.png` with a 4×5 PNG and
refreshing, the status becomes `IMAGE · 4×5`; the old 3×2 cache is gone. A
20-missing-image document keeps exactly 8 pane entries and sets the limited
flag. A three-tab test with a 4 MiB shared budget admits at most one 1000×1000
image, keeps budget usage within bytes/entries, and returns to `(0, 0)` after
closing the tabs. A no-poll test queues three 1000×1000 reads under a 4 MiB
budget and observes 4,000,000 bytes already reserved with three entries before
the UI drains the channels; dropping the workspace returns to `(0, 0)`. A
refresh+close race with an in-flight read also returns to `(0, 0)`. The private
VFS read test accepts an 8 MiB payload (nested base64) and refuses the same file
at a 4 MiB limit. Media unit tests decode PNG to BGRA, refuse non-images and an
8 MiB+1 payload,
and rewrite a real 1×1 PNG to claim 5000×4000 (within the dimension bound but
over the pixel bound): the header preflight rejects it with the pixel-limit
message before the compressed payload could be decoded, and `MAX_PIXELS * 4 * 2
<= DECODE_ALLOC_BYTES` records the peak-memory relationship.

The Markdown TestWindow test puts `guide.md` in a private VFS and asserts the
selectors `md-h1-left-0`, `md-p-left-1`, `md-h2-left-2`, `md-list-left-3`,
`md-code-left-5`, the heading height being greater than the paragraph height,
and the parsed span styles for bold/code/link. The link test puts `index.md`
with two local links and one external link beside `one.md`/`two.md`: clicking
each local link navigates and Back returns, the external link is
`md-link-blocked-*` and does not move, and `l`/`Enter` follows a displayed link
through the shared Browser selection. A unit test feeds ANSI/bidi controls
through a heading, link label and fenced code and asserts the displayed text is
clean while the link target is preserved. The workspace test clicks the tab,
split and close controls, asserts both pane selectors and that clicking the
right pane focuses it while keeping `split == Some(left)`.

`main.rs` change against base `56f99a02`: +42/−136 lines, concentrated in the
trait impl, the outer `.child`/coexistence condition removals and the documents
toggle. The rest of the document UI is in the new module. The terminal/tree
worker's `main.rs` additions (+356/−81) do not overlap these regions.

## Integration interface and conflicts

Owned by this change:

- `gui/src/document_workspace_view.rs` (new), `gui/src/markdown.rs` (new)
- `gui/src/documents.rs`, `gui/src/highlight.rs`
- `LocalGui.documents: Option<Workspace>`, `DocumentHost` impl, the one
  `document_workspace_view::render(...)` child at the end of the outer row
- removal of `!documents_open` from the terminal/preview/input branches and the
  `"Document browser"` title branch

Terminal/tree worker keeps `terminal*`/`input*`/`attach*`, `peers`/`nodes`/
`remote*` and the sidebar. Integration order: apply the terminal/tree work
first, then this patch. Expected conflict points are only:

1. the outer row's final `.child(...)` chain (add the document child),
2. the four `!documents_open` condition lines,
3. the title/label block that named "Document browser",
4. the tests at the end of `main.rs` if both sides add tests there (this change
   moved its document TestWindow tests into the new module, leaving only the
   layout/focus tests).

No mutated `main.rs` document rendering methods remain to conflict; the new
module only needs `DocumentHost` implemented for the host type.

## Webview evaluation and blocker

GPUI 0.2.2 has no webview element. A webview would need `wry` plus WebView2 on
Windows and WKWebView on macOS, an OS-specific runtime, and a
script/resource-interception policy. There is no Windows host here, so Windows
behaviour and an inert-content guarantee could not be demonstrated; "no
arbitrary script or external resource" would become a policy to prove rather
than a property of the renderer. The native text projection, Markdown block
renderer and bounded tokenizer are the implemented alternative.

## Limitations

- Verification uses GPUI `TestWindow` plus a private daemon; a real desktop
  window, monitor/DPI behaviour and font fallback are out of scope.
- GPUI's test window exposes element bounds, not the colour of a text run.
  Heading hierarchy is evidenced by bounds and text model; weight, italic,
  underline and code background are asserted on the style descriptors that feed
  `HighlightStyle`, not by pixel inspection.
- Markdown subset: ATX headings, fences, quotes, list items (one level of
  marker plus indentation), rules, paragraphs, inline bold/italic/code/link and
  local images. Reference-style links, setext headings, tables, nested block
  structure, raw HTML and thematic edge cases stay literal. A non-image
  reference still shows the old `[image: alt]` placeholder.
- Images: PNG/JPEG only, first frame only, no animation, zoom or pan; inline
  images are flex items rather than baseline-aligned glyphs. The renderer keeps
  the decoded GPUI image, not a separate GPU texture cache, and a pane retains
  up to 8 entries per pane but only 32 entries/128 MiB decoded across the whole
  workspace. Decodes are serialized and each decoded buffer is reserved before
  it enters a channel, so the policy peak is the 128 MiB cache plus one bounded
  decode (8 MiB encoded + up to 160 MiB decoder allocation), not a measured
  peak. Results waiting in channels count against the same reservation. Admission fails closed rather than evicting; a full workspace
  shows "image budget reached" until tabs are closed or refreshed. Cached pixels
  are reused on Back; only an explicit refresh invalidates them.
- Windows evidence is cross-compilation; the GPU image path has no Windows
  runtime evidence here. There is no LRU eviction: a workspace that fills its
  image budget refuses new images until a tab is closed or refreshed.
- Code colour is a line tokenizer, not a parser or theme; unknown fence
  languages stay plain.
- Windows evidence is cross-compilation only; the base document request path is
  Unsupported on Windows and no Windows runtime was executed.
- The split shows two panes; more than two simultaneous panes are not offered.
