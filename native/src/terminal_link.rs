//! Portable targets for terminal output and viewers.

use remuda_core::vfs::VfsUri;

#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Drive(VfsUri),
    Web(url::Url),
}

impl Target {
    pub fn parse(uri: &str) -> Option<Self> {
        if uri.len() > 2048 || uri.contains(';') || uri.chars().any(char::is_control) {
            return None;
        }
        if uri.starts_with("remuda://") {
            return VfsUri::parse(uri).ok().map(Self::Drive);
        }
        if !(uri.starts_with("https://") || uri.starts_with("http://"))
            || uri.chars().any(|ch| ch.is_whitespace() || ch == '\\')
        {
            return None;
        }
        let url = url::Url::parse(uri).ok()?;
        (url.host().is_some() && url.username().is_empty() && url.password().is_none())
            .then_some(Self::Web(url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_target_preserves_cluster_authority() {
        let Some(Target::Drive(uri)) = Target::parse("remuda://team/docs/guide.md") else {
            panic!("expected Drive target");
        };
        assert_eq!(uri.path, "docs/guide.md");
        assert!(uri.check_cluster("team").is_ok());
        assert!(uri.check_cluster("other").is_err());
    }

    #[test]
    fn web_target_preserves_document_anchor() {
        let Some(Target::Web(url)) = Target::parse("https://example.org/guide#keyboard") else {
            panic!("expected web target");
        };
        assert_eq!(url.fragment(), Some("keyboard"));
    }
}

#[cfg(test)]
#[test]
fn osc_targets_follow_their_cells_through_overwrite_and_scrollback() {
    let uri = "remuda:///guide.md";
    let mut parser = vt100::Parser::new(2, 10, 3);
    parser.process(format!("\x1b]8;;{uri}\x1b\\AB\x1b]8;;\x1b\\C").as_bytes());
    assert_eq!(parser.screen().cell(0, 0).unwrap().hyperlink(), Some(uri));
    assert_eq!(parser.screen().cell(0, 2).unwrap().hyperlink(), None);
    parser.process(b"\rX\r\n\r\n");
    parser.screen_mut().set_scrollback(1);
    let overwritten = parser.screen().cell(0, 0).unwrap();
    assert_eq!(overwritten.contents(), "X");
    assert_eq!(overwritten.hyperlink(), None);
    assert_eq!(parser.screen().cell(0, 1).unwrap().hyperlink(), Some(uri));
}

#[cfg(test)]
#[test]
fn parsed_links_pass_through_styled_wire_and_render_with_the_same_width() {
    use remuda_core::{
        agent::Size,
        protocol::{collapse_runs, expand_runs, StyledRun},
    };
    let uri = "https://github.com/org/repo";
    let mut parser = vt100::Parser::new(2, 5, 0);
    parser.process(format!("\x1b]8;;{uri}\x07Hi\x1b]8;;\x07!").as_bytes());
    let cells = crate::pty::styled_cells(parser.screen(), Size::new(5, 2));
    let serialized = serde_json::to_string(&collapse_runs(&cells[0][..3])).unwrap();
    let runs: Vec<StyledRun> = serde_json::from_str(&serialized).unwrap();
    let rendered = crate::tui::render_styled_row(&expand_runs(&runs));
    assert_eq!(rendered, format!("\x1b]8;;{uri}\x1b\\Hi\x1b]8;;\x1b\\!"));
    assert_eq!(crate::tui::visible_width(&rendered), 3);
}
