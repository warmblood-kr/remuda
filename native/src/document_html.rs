//! A bounded text/link projection of untrusted HTML, shared by UI adapters.

mod links;
pub use links::{resolve, Target};
use scraper::{Html, Node};

pub const MAX_HTML_BYTES: usize = 128 * 1024;
const MAX_NODES: usize = 20_000;
const MAX_LINKS: usize = 128;
pub(crate) const MAX_IMAGES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub label: String,
    pub target: Target,
}

#[derive(Clone, Debug, Default)]
pub struct Document {
    pub title: String,
    pub lines: Vec<String>,
    pub links: Vec<Link>,
}

/// Image reference in the inert projection. UI callers may fetch only
/// authorized VFS targets and decide when to load or render the image.
#[derive(Clone, Debug)]
pub struct Image {
    pub label: String,
    pub target: Target,
    /// Paragraph position in the corresponding projected line list.
    pub line: usize,
}

/// Parse HTML5 into inert title, text blocks and typed local links.
/// This projection executes no scripts, CSS, forms or resource loads.
pub fn parse(path: &str, source: &str) -> Result<Document, String> {
    project(path, source, None)
}

pub fn parse_with_images(path: &str, source: &str) -> Result<(Document, Vec<Image>), String> {
    let mut images = Vec::new();
    let document = project(path, source, Some(&mut images))?;
    Ok((document, images))
}

fn project(
    path: &str,
    source: &str,
    mut images: Option<&mut Vec<Image>>,
) -> Result<Document, String> {
    if source.len() > MAX_HTML_BYTES {
        return Err("HTML exceeds the 128 KiB limit".into());
    }
    let html = Html::parse_document(source);
    if html.tree.nodes().count() > MAX_NODES {
        return Err("HTML has too many nodes".into());
    }
    let mut result = Document::default();
    let mut text = String::new();
    let mut in_title = false;
    let mut active_link = None;
    let mut stack = vec![(html.tree.root(), false, None)];
    while let Some((node, closing, link)) = stack.pop() {
        match node.value() {
            Node::Element(element) => {
                let name = element.name();
                if hidden(name) || element.attr("hidden").is_some() {
                    continue;
                }
                if closing {
                    close(
                        name,
                        link,
                        &mut result,
                        &mut text,
                        &mut in_title,
                        &mut active_link,
                    )?;
                    continue;
                }
                if block(name) {
                    flush(&mut result, &mut text)?;
                }
                if name == "title" {
                    in_title = true;
                }
                if name == "img" {
                    if let (Some(images), Some(src)) = (images.as_deref_mut(), element.attr("src"))
                    {
                        if images.len() < MAX_IMAGES {
                            images.push(Image {
                                label: clean(element.attr("alt").unwrap_or("Image"))
                                    .chars()
                                    .take(512)
                                    .collect(),
                                target: resolve(path, src),
                                line: result.lines.len(),
                            });
                        }
                    }
                }
                let mut link = None;
                if name == "a" {
                    if let Some(href) = element.attr("href") {
                        if result.links.len() == MAX_LINKS {
                            return Err("HTML has too many links".into());
                        }
                        link = Some(result.links.len());
                        active_link = link;
                        result.links.push(Link {
                            label: String::new(),
                            target: resolve(path, href),
                        });
                    }
                }
                stack.push((node, true, link));
                stack.extend(node.children().rev().map(|child| (child, false, None)));
            }
            Node::Text(value) if !closing => {
                if in_title {
                    result.title.push_str(value);
                } else {
                    text.push_str(value);
                    if let Some(index) = active_link {
                        result.links[index].label.push_str(value);
                    }
                }
            }
            Node::Document | Node::Fragment if !closing => {
                stack.extend(node.children().rev().map(|child| (child, false, None)));
            }
            _ => {}
        }
    }
    flush(&mut result, &mut text)?;
    finalize(&mut result, images);
    Ok(result)
}

fn finalize(result: &mut Document, images: Option<&mut Vec<Image>>) {
    result.title = clean(&result.title).chars().take(512).collect();
    for link in &mut result.links {
        link.label = clean(&link.label).chars().take(512).collect();
        if link.label.is_empty() {
            link.label = "(link)".into();
        }
    }
    if !result.title.is_empty() {
        result.lines.insert(0, String::new());
        result.lines.insert(0, result.title.clone());
        if let Some(images) = images {
            for image in images {
                image.line += 2;
            }
        }
    }
}

fn close(
    name: &str,
    link: Option<usize>,
    result: &mut Document,
    text: &mut String,
    in_title: &mut bool,
    active_link: &mut Option<usize>,
) -> Result<(), String> {
    if name == "title" {
        *in_title = false;
    }
    if let Some(index) = link {
        text.push_str(&format!(" [{}]", index + 1));
        *active_link = None;
    }
    if block(name) {
        flush(result, text)?;
    }
    Ok(())
}

fn hidden(name: &str) -> bool {
    matches!(
        name,
        "script" | "style" | "template" | "noscript" | "iframe" | "object" | "svg" | "math"
    )
}

fn block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "section"
            | "article"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "br"
            | "li"
            | "pre"
            | "blockquote"
            | "tr"
    )
}

fn clean(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::text::strip_terminal_controls(&collapsed).into_owned()
}

fn flush(result: &mut Document, text: &mut String) -> Result<(), String> {
    let line = clean(text);
    text.clear();
    if !line.is_empty() {
        if result.lines.len() >= 4092 {
            return Err("HTML has too many text blocks".into());
        }
        result.lines.push(line);
        result.lines.push(String::new());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html5_tree_decodes_entities_and_projects_title_blocks_and_links() {
        let parsed = parse("docs/index.html", "<TITLE>Guide &amp; help</TITLE><p>Hello <b>world</b> &lt;3<p><a HREF='sub/next.html'>Next &amp; more</a>").unwrap();
        assert_eq!(parsed.title, "Guide & help");
        assert!(parsed.lines.contains(&"Hello world <3".into()));
        assert!(parsed.lines.contains(&"Next & more [1]".into()));
        assert_eq!(
            parsed.links,
            vec![Link {
                label: "Next & more".into(),
                target: Target::Vfs("docs/sub/next.html".into())
            }]
        );
    }

    #[test]
    fn untrusted_html_is_inert_and_terminal_controls_are_not_emitted() {
        let parsed = parse("index.html", "<base href='https://evil.example/'><script>BAD</script><style>BAD</style><template>BAD</template><iframe>BAD</iframe><div hidden>BAD</div><p>safe&#27;[31m\u{202e}text <a href='javascript:alert(1)'>bad</a>").unwrap();
        let text = parsed.lines.join("\n");
        assert!(!text.contains("BAD"));
        assert!(!text.contains('\x1b') && !text.contains('\u{202e}'));
        assert!(matches!(parsed.links[0].target, Target::Blocked(_)));
        let relative = parse(
            "docs/index.html",
            "<base href='https://evil.example/'><a href='next.html'>next</a>",
        )
        .unwrap();
        assert_eq!(
            relative.links[0].target,
            Target::Vfs("docs/next.html".into())
        );
        let encoded = parse(
            "index.html",
            "<a href='javascript&#58;alert(1)'>x</a><a href='x&#10;y'>y</a>",
        )
        .unwrap();
        assert!(encoded
            .links
            .iter()
            .all(|link| matches!(link.target, Target::Blocked(_))));
    }

    #[test]
    fn source_structure_and_link_limits_are_explicit_errors() {
        assert!(parse("x.html", &"x".repeat(MAX_HTML_BYTES + 1)).is_err());
        assert!(parse("x.html", &"<br>".repeat(MAX_NODES))
            .unwrap_err()
            .contains("nodes"));
        assert!(parse("x.html", &"<a href='x'>x</a>".repeat(MAX_LINKS + 1)).is_err());
        assert!(parse("x.html", &"<p>x</p>".repeat(2100))
            .unwrap_err()
            .contains("text blocks"));
    }
}
