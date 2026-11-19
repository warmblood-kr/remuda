//! Interface dry-fit: the GUI adapter can consume the shared document meaning
//! without a TUI frame. This proves the seam compiles and behaves from the GUI
//! crate; it does not render or acknowledge a GUI document view.

use remuda_native::document_html::{self, Target};
use remuda_native::document_markdown;

#[test]
fn gui_can_consume_the_shared_markdown_and_html_projection() {
    let markdown = document_markdown::parse(
        "docs/guide.md",
        "# Guide\n\nSee [next](sub/next.md) and ![img](a.png).\n",
    )
    .unwrap();
    assert_eq!(markdown.title, "Guide");
    assert_eq!(
        markdown.lines.join("\n"),
        "Guide\n\nSee next [1] and [image: img]."
    );
    assert_eq!(markdown.links.len(), 1);
    assert_eq!(markdown.links[0].label, "next");
    assert_eq!(
        markdown.links[0].target,
        Target::Vfs("docs/sub/next.md".into())
    );

    let html = document_html::parse(
        "docs/index.html",
        "<title>Index</title><p><a href='/next.md'>next</a></p>",
    )
    .unwrap();
    assert_eq!(html.links[0].target, Target::Vfs("next.md".into()));

    // The same resolver decides inertness for a GUI and the TUI.
    assert!(matches!(
        document_html::resolve("docs/index.md", "https://example.com"),
        Target::Blocked(_)
    ));
}
