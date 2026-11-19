//! The document-pane command vocabulary both renderers resolve keys against.
//!
//! Keys and help are declared once, in Lua (`native/src/keybindings.lua`); the
//! TUI dispatches the names it gets back and the GUI resolves its own keystrokes
//! against the same snapshot, so one `remuda.rebind("cluster.documents", ...)`
//! reaches both. The spelling lives in the policy layer because a renderer that
//! invented a second one would resolve nothing, and quietly, at runtime.

/// Key context shared by the TUI document browser and the GUI document pane.
pub const CONTEXT: &str = "cluster.documents";

/// Move the listing/link cursor toward the previous row.
pub const UP: &str = "documents.up";
/// Move the listing/link cursor toward the next row.
pub const DOWN: &str = "documents.down";
/// Open the selected row or link.
pub const OPEN: &str = "documents.open";
/// Return to the previous document, or leave the browser at the root.
pub const BACK: &str = "documents.back";
/// Close the document view; it never closes the underlying session.
pub const CLOSE: &str = "documents.close";
/// Retry the current read.
pub const REFRESH: &str = "documents.refresh";
/// Show the full escaped `remuda:///` address of the selection.
pub const ADDRESS: &str = "documents.address";
/// Select the next link in a rendered projection.
pub const LINK_NEXT: &str = "documents.link.next";
/// Select the previous link in a rendered projection.
pub const LINK_PREVIOUS: &str = "documents.link.previous";

/// Every action a caller may bind in [`CONTEXT`], in catalog order.
pub const ALL: &[&str] = &[
    UP,
    DOWN,
    OPEN,
    BACK,
    CLOSE,
    REFRESH,
    ADDRESS,
    LINK_NEXT,
    LINK_PREVIOUS,
];

#[cfg(test)]
mod tests {
    #[test]
    fn the_catalog_is_unique_and_namespaced() {
        let mut seen = std::collections::HashSet::new();
        for action in super::ALL {
            assert!(seen.insert(*action), "{action} is listed twice");
            assert!(
                action.starts_with("documents."),
                "{action} does not belong to the documents namespace"
            );
        }
        assert_eq!(super::ALL.len(), 9);
    }
}
