//! Read-only document primitives: bounded VFS reads, display labels and types.
//!
//! The browsing state machine is added on top of these pieces.

mod presentation;
mod transport;
mod types;

pub use presentation::{basename_label, parent_directory, path_label};
pub use transport::read_media;
pub use types::{Entry, Location, Page};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 4096;

#[cfg(test)]
mod tests {
    use super::presentation::{clip, wrap_text};
    use super::transport::decode;
    use super::*;
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    use serde_json::json;

    #[test]
    fn basename_label_keeps_the_last_component_only() {
        assert_eq!(basename_label(""), "");
        assert_eq!(basename_label("docs/"), "docs");
        assert_eq!(basename_label("docs/readme.md"), "readme.md");
        assert_eq!(basename_label("readme.md"), "readme.md");
        assert_eq!(basename_label("docs/한글 폴더/노트.md"), "노트.md");
        assert_eq!(basename_label(r"docs/a\b.md"), r"a\\b.md");
        assert_eq!(basename_label("docs/a\u{202e}b.md"), r"a\u{202e}b.md");
    }
    #[test]
    fn entry_display_name_is_compact_but_keeps_the_full_path() {
        let entry = Entry {
            path: "docs/guide/note.md".into(),
            directory: false,
            label: None,
        };
        assert_eq!(entry.display_name(), "note.md");
        assert_eq!(entry.display_label(), "docs/guide/note.md");
        assert_eq!(entry.path, "docs/guide/note.md");
        let parent = Entry {
            path: "docs/".into(),
            directory: true,
            label: Some("..".into()),
        };
        assert_eq!(parent.display_name(), "..");
        assert_eq!(parent.display_label(), "..");
    }
    #[test]
    fn parent_directory_stops_at_the_root() {
        assert_eq!(parent_directory(""), None);
        assert_eq!(parent_directory("/"), None);
        assert_eq!(parent_directory("docs"), Some(String::new()));
        assert_eq!(parent_directory("docs/"), Some(String::new()));
        assert_eq!(parent_directory("docs/guide/"), Some("docs/".into()));
        assert_eq!(parent_directory("a/b/c/"), Some("a/b/".into()));
    }
    fn html_page(path: &str, text: &str) -> Page {
        decode(
            &Location::Document(path.into()),
            &json!({
                "ok":true,"path":path,"sha256":"a".repeat(64),"version":1,
                "size":text.len(),"content":BASE64.encode(text)
            }),
        )
        .unwrap()
    }

    #[test]
    fn path_labels_escape_controls_without_changing_navigation_identity() {
        let raw = "docs/a\u{202e}b";
        let page = decode(
            &Location::Directory("docs/".into()),
            &json!([
                {"kind":"file","path":"docs/ab"},
                {"kind":"file","path":raw},
                {"kind":"dir","path":"docs/a\u{2066}b/"}
            ]),
        )
        .unwrap();
        assert!(page.entries.iter().any(|entry| entry.path == raw));
        let labels: Vec<_> = page.entries.iter().map(|e| path_label(&e.path)).collect();
        assert!(labels.contains(&"docs/ab".to_owned()));
        assert!(labels.contains(&r"docs/a\u{202e}b".to_owned()));
        assert!(labels.contains(&r"docs/a\u{2066}b/".to_owned()));
        assert_eq!(path_label("a\nb\x1b\u{009b}\\"), r"a\nb\u{1b}\u{9b}\\");
        assert_ne!(path_label(raw), path_label(r"docs/a\u{202e}b"));
        assert_eq!(clip(raw, 100), "docs/ab");
    }

    #[test]
    fn read_refusal_binary_and_large_content_are_errors() {
        let location = Location::Document("x".into());
        assert!(decode(&location, &json!({"ok":false,"code":"too_large"}))
            .unwrap_err()
            .contains("too_large"));
        for bytes in [vec![0], vec![255], vec![b'x'; MAX_BYTES + 1]] {
            let value = json!({"ok":true,"path":"x","sha256":"a".repeat(64),"version":1,"size":bytes.len(),"content":BASE64.encode(bytes)});
            assert!(decode(&location, &value).is_err());
        }
    }
    #[test]
    fn markdown_pages_carry_their_source_for_a_styled_reader() {
        let page = html_page("docs/guide.md", "# Title\n\nUse `code`.\n");
        assert_eq!(
            page.markdown_source.as_deref(),
            Some("# Title\n\nUse `code`.\n")
        );
        assert_eq!(page.projection.as_ref().unwrap().title, "Title");
        let plain = html_page("docs/notes.txt", "plain text");
        assert!(plain.markdown_source.is_none());
    }

    #[test]
    fn listing_and_text_display_cannot_escape_their_boundaries() {
        let location = Location::Directory("docs/".into());
        assert!(decode(&location, &json!([{"kind":"file","path":"docs/sub/x"}])).is_err());
        assert!(decode(&location, &json!([{"kind":"file","path":"other/x"}])).is_err());
        assert_eq!(clip("ab\x1b[2J界z", 6), "ab[2J");
        assert_eq!(clip("界界z", 4), "界界");
        assert_eq!(wrap_text(&["abcdefgh".into()], 3), ["abc", "def", "gh"]);
    }
}
