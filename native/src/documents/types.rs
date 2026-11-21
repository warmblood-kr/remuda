//! Document data types shared by the browser, transport and presentation helpers.

use super::presentation::{basename_label, path_label};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    Directory(String),
    Document(String),
}
impl Location {
    pub fn path(&self) -> &str {
        match self {
            Self::Directory(path) | Self::Document(path) => path,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub directory: bool,
    /// Display override. The synthetic parent row shows `..` while its path
    /// stays the real parent address used for navigation.
    pub label: Option<String>,
}
impl Entry {
    /// The row label with its full identity: the override when present,
    /// otherwise the escaped path. The TUI listing uses this.
    pub fn display_label(&self) -> String {
        self.label.clone().unwrap_or_else(|| path_label(&self.path))
    }
    /// The compact row label: the override when present, otherwise the
    /// escaped last path component. Selection and opening still use `path`.
    pub fn display_name(&self) -> String {
        self.label
            .clone()
            .unwrap_or_else(|| basename_label(&self.path))
    }
}
#[derive(Clone, Debug)]
pub struct Page {
    pub location: Location,
    pub entries: Vec<Entry>,
    pub text: Vec<String>,
    pub position: usize,
    pub identity: Option<(u64, String)>,
    pub error: Option<String>,
    /// Text/link projection for HTML and Markdown; `None` for plain text.
    pub projection: Option<crate::document_html::Document>,
    /// Original Markdown source for a styled GUI reader. Shared by history
    /// clones so a back stack does not copy the same 1 MiB source per entry.
    pub markdown_source: Option<std::sync::Arc<str>>,
    /// The bounded original UTF-8 document, shared by history snapshots.
    pub source_text: Option<std::sync::Arc<str>>,
    pub selected_link: Option<usize>,
}
impl Page {
    pub(crate) fn empty(location: Location) -> Self {
        Self {
            location,
            entries: Vec::new(),
            text: Vec::new(),
            position: 0,
            identity: None,
            error: None,
            projection: None,
            markdown_source: None,
            source_text: None,
            selected_link: None,
        }
    }
}
pub(crate) type Reply = Result<Page, String>;
