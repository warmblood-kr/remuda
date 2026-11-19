//! Pure state for the read-only permissions overlay.
//!
//! The TUI owns keys, threads and painting; this module owns the list state so
//! selection bounds, unavailable-vs-empty wording and detail rendering can be
//! tested without a terminal.

use crate::native_permissions::{Row, Scan};

#[derive(Debug)]
pub struct Overlay {
    scan: Scan,
    selected: usize,
    scroll: usize,
}

impl Overlay {
    pub fn new(scan: Scan) -> Self {
        Self {
            scan,
            selected: 0,
            scroll: 0,
        }
    }

    pub fn scan(&self) -> &Scan {
        &self.scan
    }

    pub fn rows(&self) -> &[Row] {
        &self.scan.rows
    }

    pub fn is_empty(&self) -> bool {
        self.scan.rows.is_empty()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn unavailable(&self) -> usize {
        self.scan
            .bindings
            .iter()
            .filter(|binding| binding.status == "unavailable")
            .count()
    }

    /// A short banner. "No pending" is only claimed when every binding is
    /// reachable; an error is never rendered as an empty list.
    pub fn summary(&self) -> String {
        if self.is_empty() {
            if self.unavailable() == 0 {
                return "No pending permission requests.".into();
            }
            return format!(
                "Permission status unavailable ({} binding{}).",
                self.unavailable(),
                if self.unavailable() == 1 { "" } else { "s" }
            );
        }
        let pending = self.scan.rows.len();
        if self.unavailable() == 0 {
            return format!(
                "{pending} pending request{}.",
                if pending == 1 { "" } else { "s" }
            );
        }
        format!(
            "{pending} pending / {} binding{} unavailable.",
            self.unavailable(),
            if self.unavailable() == 1 { "" } else { "s" }
        )
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.scan.rows.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.scan.rows.len() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, last as isize) as usize;
    }

    pub fn select(&mut self, index: usize) {
        if !self.scan.rows.is_empty() {
            self.selected = index.min(self.scan.rows.len() - 1);
        }
    }

    /// Keep the selection inside a `height`-row window.
    pub fn scroll_to(&mut self, height: usize) {
        if height == 0 || self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.scan.rows.get(self.selected)
    }

    /// Why other bindings could not be read. Rendered next to the rows too,
    /// so a partial list never hides which sessions are missing from it.
    pub fn unavailable_lines(&self, limit: usize) -> Vec<String> {
        self.scan
            .bindings
            .iter()
            .filter(|binding| binding.status != "ok")
            .take(limit)
            .map(|binding| {
                format!(
                    "{}: unavailable ({})",
                    binding.session,
                    binding.reason.unwrap_or("?")
                )
            })
            .collect()
    }

    /// One line per visible row, with a `>` marker for the selection. The
    /// window follows the selection: a marker outside it would hide the row
    /// the detail block is describing.
    pub fn lines(&self, height: usize, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        if self.scan.rows.is_empty() {
            lines.push(self.summary());
            lines.extend(self.unavailable_lines(usize::MAX));
            return lines;
        }
        let height = height.max(1);
        let start = if self.selected < self.scroll {
            self.selected
        } else if self.selected >= self.scroll + height {
            self.selected + 1 - height
        } else {
            self.scroll
        };
        for (index, row) in self.scan.rows.iter().enumerate().skip(start).take(height) {
            let marker = if index == self.selected { ">" } else { " " };
            lines.push(truncate(
                &format!(
                    "{marker} {}  {}  {}  {}",
                    row.session,
                    row.action,
                    if row.resources.is_empty() {
                        "-".to_string()
                    } else {
                        row.resources.join("; ")
                    },
                    row.status
                ),
                width,
            ));
        }
        lines
    }

    /// Detail lines for the selected request. Read-only: this is the data a
    /// decision key would need to show, but no decision is sent from here.
    pub fn detail(&self) -> Vec<String> {
        let Some(row) = self.selected_row() else {
            return Vec::new();
        };
        let mut lines = vec![
            format!("Request: {}", row.id),
            format!("Session: {} ({})", row.session, row.opencode_session),
            format!("Action: {}", row.action),
            format!("Resources: {}", row.resources.join(", ")),
            format!("Status: {}", row.status),
            format!("Decision options: {}", row.decision),
        ];
        if let Some(message) = &row.message {
            lines.push(format!("Message: {message}"));
        }
        lines
    }
}

fn truncate(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out: String = value.chars().take(width).collect();
    if out.chars().count() == width && value.chars().count() > width {
        out.pop();
        out.push('~');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_permissions::{BindingStatus, Row, Scan};

    fn row(id: &str, session: &str, action: &str) -> Row {
        Row {
            id: id.into(),
            session: session.into(),
            opencode_session: "ses_abc".into(),
            action: action.into(),
            resources: vec!["echo hi".into()],
            message: None,
            status: "pending",
            decision: "once | reject",
        }
    }

    fn scan(rows: Vec<Row>, unavailable: usize) -> Scan {
        let mut bindings = vec![BindingStatus {
            session: "live".into(),
            status: "ok",
            reason: None,
        }];
        for index in 0..unavailable {
            bindings.push(BindingStatus {
                session: format!("stale{index}"),
                status: "unavailable",
                reason: Some("server_pid"),
            });
        }
        Scan { bindings, rows }
    }

    #[test]
    fn empty_and_reachable_is_empty() {
        let overlay = Overlay::new(scan(Vec::new(), 0));
        assert_eq!(overlay.summary(), "No pending permission requests.");
        assert!(overlay.lines(4, 80)[0].starts_with("No pending"));
    }

    #[test]
    fn unavailable_is_never_empty() {
        let overlay = Overlay::new(scan(Vec::new(), 2));
        assert!(overlay.summary().contains("unavailable"));
        assert!(!overlay.summary().contains("No pending"));
        assert!(overlay
            .lines(4, 80)
            .iter()
            .any(|line| line.contains("server_pid")));
    }

    #[test]
    fn selection_is_bounded() {
        let overlay_scan = scan(
            vec![row("per_1", "a", "bash"), row("per_2", "b", "edit")],
            0,
        );
        let mut overlay = Overlay::new(overlay_scan);
        overlay.move_selection(-1);
        assert_eq!(overlay.selected(), 0);
        overlay.move_selection(9);
        assert_eq!(overlay.selected(), 1);
        overlay.select(0);
        assert_eq!(overlay.selected(), 0);
    }

    #[test]
    fn lines_mark_selection_and_truncate() {
        let mut overlay = Overlay::new(scan(
            vec![
                row("per_1", "alpha", "bash"),
                row("per_2", "beta", "edit"),
                row("per_3", "gamma", "read"),
            ],
            0,
        ));
        let lines = overlay.lines(1, 20);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with(">"));
        assert!(lines[0].chars().count() <= 20);
        overlay.select(1);
        overlay.scroll_to(1);
        let lines = overlay.lines(2, 20);
        assert!(lines[0].starts_with(">"));
        assert!(lines[1].starts_with(" "));
    }

    #[test]
    fn detail_has_no_credentials() {
        let overlay = Overlay::new(scan(vec![row("per_1", "alpha", "bash")], 0));
        let detail = overlay.detail().join("\n");
        assert!(detail.contains("per_1"));
        assert!(!detail.contains("password"));
        assert!(!detail.contains("nonce"));
    }

    #[test]
    fn scroll_follows_selection() {
        let rows = (0..5)
            .map(|index| row(&format!("per_{index}"), "s", "bash"))
            .collect();
        let mut overlay = Overlay::new(scan(rows, 0));
        overlay.move_selection(4);
        overlay.scroll_to(2);
        assert_eq!(overlay.scroll(), 3);
        assert_eq!(overlay.lines(2, 80).len(), 2);
    }

    #[test]
    fn the_window_follows_the_selection_without_a_scroll_call() {
        let rows = (0..5)
            .map(|index| row(&format!("per_{index}"), &format!("s{index}"), "bash"))
            .collect();
        let mut overlay = Overlay::new(scan(rows, 0));
        overlay.move_selection(4);
        // The TUI never calls `scroll_to`; the marker must still be on screen.
        let lines = overlay.lines(2, 80);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("s3"));
        assert!(lines[1].starts_with('>'));
        assert!(lines[1].contains("s4"));
    }

    #[test]
    fn unavailable_reasons_stay_visible_next_to_rows() {
        let overlay = Overlay::new(scan(vec![row("per_1", "live", "bash")], 2));
        let lines = overlay.lines(3, 80);
        assert!(lines[0].starts_with('>'));
        assert!(lines[0].contains("live"));
        let reasons = overlay.unavailable_lines(3).join("\n");
        assert!(reasons.contains("stale0"));
        assert!(reasons.contains("server_pid"));
    }
}
