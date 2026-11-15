//! Shared, read-only session previews. A preview never grants authority to act.
use crate::tui::SessionPort;
use remuda_core::agent::{Cursor, StyledCell};
use remuda_core::protocol::{expand_runs, Response};

/// Compatibility applies the TUI's historical local/remote distinction.
/// Strict requires proof of the selected instance on every node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityPolicy {
    TuiCompatible,
    Strict,
}

pub struct PreviewFrame {
    pub cells: Vec<Vec<StyledCell>>,
    pub wrapped: Vec<bool>,
    pub cursor: Cursor,
    pub scrollback_len: usize,
    pub scrollback_total: usize,
    pub instance_id: Option<String>,
    pub output_version: Option<u64>,
}

/// Capture a frame from the same local or authenticated peer port as the TUI.
/// Use a background worker: the port may wait for I/O. Identity verification
/// describes this frame only and does not authorize later input or attachment.
pub fn capture<P: SessionPort + ?Sized>(
    port: &P,
    name: &str,
    expected: Option<&str>,
    scrollback: usize,
    policy: IdentityPolicy,
) -> Result<PreviewFrame, String> {
    match port.capture_instance(name, expected, scrollback) {
        Ok(Response::StyledScreen {
            rows,
            wrapped,
            cursor,
            scrollback_len,
            scrollback_total,
            instance_id,
            output_version,
        }) => {
            let remote = port.remote_node().is_some();
            let rejected = match policy {
                IdentityPolicy::TuiCompatible => {
                    remote && (expected.is_none() || instance_id.as_deref() != expected)
                }
                IdentityPolicy::Strict => expected
                    .filter(|id| !id.is_empty())
                    .is_none_or(|id| instance_id.as_deref() != Some(id)),
            };
            if rejected {
                return Err(if policy == IdentityPolicy::TuiCompatible {
                    "peer could not prove the selected preview instance"
                } else {
                    "could not prove the selected preview instance"
                }
                .into());
            }
            if policy == IdentityPolicy::Strict {
                const MAX_ROWS: usize = 512;
                const MAX_ROW_COLUMNS: usize = 512;
                const MAX_CELLS: usize = 65_536;
                const MAX_RUNS: usize = 65_536;
                const MAX_TEXT_BYTES: usize = 262_144;
                let mut runs = 0usize;
                let mut bytes = 0usize;
                let mut cells = 0usize;
                // Older peers can omit wrapped metadata; otherwise it must
                // describe every returned row.
                if rows.len() > MAX_ROWS || (!wrapped.is_empty() && wrapped.len() != rows.len()) {
                    return Err("Preview exceeds display limits".into());
                }
                for row in &rows {
                    runs = runs.saturating_add(row.len());
                    let mut columns = 0usize;
                    for run in row {
                        bytes = bytes.saturating_add(run.text.len());
                        let count = run.text.chars().count();
                        cells = cells.saturating_add(count);
                        for ch in run.text.chars() {
                            let width = unicode_width::UnicodeWidthChar::width(ch)
                                .unwrap_or(1)
                                .max(if run.wide { 2 } else { 1 });
                            columns = columns.saturating_add(width);
                        }
                    }
                    if columns > MAX_ROW_COLUMNS {
                        return Err("Preview exceeds display limits".into());
                    }
                }
                if runs > MAX_RUNS || bytes > MAX_TEXT_BYTES || cells > MAX_CELLS {
                    return Err("Preview exceeds display limits".into());
                }
            }
            let mut cells: Vec<Vec<StyledCell>> = rows.iter().map(|row| expand_runs(row)).collect();
            if remote || policy == IdentityPolicy::Strict {
                for row in &mut cells {
                    for cell in row {
                        cell.text = crate::text::strip_terminal_controls(&cell.text).into_owned();
                    }
                }
            }
            Ok(PreviewFrame {
                cells,
                wrapped,
                // Strict previews are currently text-only. The wire cursor is
                // untrusted and its scrollback/display-column mapping has not
                // been established here, so do not pass it to GUI consumers.
                cursor: if policy == IdentityPolicy::Strict {
                    Cursor {
                        row: 0,
                        col: 0,
                        visible: false,
                    }
                } else {
                    cursor
                },
                scrollback_len,
                scrollback_total,
                instance_id,
                output_version,
            })
        }
        Ok(Response::Error(reason)) if policy == IdentityPolicy::TuiCompatible => Err(reason),
        other if policy == IdentityPolicy::TuiCompatible => Err(format!("{other:?}")),
        _ => Err("Preview unavailable".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::KeyStream;
    use remuda_core::protocol::{collapse_runs, Request, StyledRun};
    use std::io;

    struct Keys;
    impl KeyStream for Keys {
        fn keys(&self, _: &[u8]) -> io::Result<()> {
            Ok(())
        }
    }
    struct Port {
        remote: bool,
        response: Response,
    }
    impl SessionPort for Port {
        type Keys = Keys;
        fn request(&self, _: &Request) -> io::Result<Response> {
            Ok(self.response.clone())
        }
        fn hold(&self, _: &str) -> io::Result<Keys> {
            unreachable!()
        }
        fn probe(&self) -> io::Result<()> {
            Ok(())
        }
        fn probe_verdict(&self, _: &io::Error) -> (bool, bool) {
            (false, false)
        }
        fn label(&self) -> String {
            "test".into()
        }
        fn remote_node(&self) -> Option<String> {
            self.remote.then(|| "peer".into())
        }
    }
    fn run(text: &str, wide: bool) -> StyledRun {
        let mut run = collapse_runs(&[StyledCell {
            text: "x".into(),
            ..Default::default()
        }])
        .remove(0);
        run.text = text.into();
        run.wide = wide;
        run
    }
    fn frame(
        remote: bool,
        instance: Option<&str>,
        rows: Vec<Vec<StyledRun>>,
        wrapped: Vec<bool>,
        cursor: Cursor,
    ) -> Port {
        Port {
            remote,
            response: Response::StyledScreen {
                rows,
                wrapped,
                cursor,
                scrollback_len: 0,
                scrollback_total: 0,
                instance_id: instance.map(str::to_owned),
                output_version: Some(5),
            },
        }
    }
    fn plain(remote: bool, instance: Option<&str>, text: &str) -> Port {
        frame(
            remote,
            instance,
            vec![vec![run(text, false)]],
            vec![false],
            Cursor {
                row: 0,
                col: 0,
                visible: false,
            },
        )
    }
    fn strict(port: &Port) -> Result<PreviewFrame, String> {
        capture(port, "session", Some("one"), 0, IdentityPolicy::Strict)
    }
    #[test]
    fn strict_requires_nonempty_matching_instance_on_either_node() {
        for remote in [false, true] {
            for (expected, actual, allowed) in [
                (Some("one"), Some("one"), true),
                (Some("one"), Some("two"), false),
                (Some("one"), None, false),
                (None, Some("one"), false),
                (Some(""), Some(""), false),
            ] {
                let result = capture(
                    &plain(remote, actual, "frame"),
                    "session",
                    expected,
                    0,
                    IdentityPolicy::Strict,
                );
                assert_eq!(
                    result.is_ok(),
                    allowed,
                    "remote={remote}, expected={expected:?}, actual={actual:?}"
                );
                if let Ok(frame) = result {
                    assert_eq!(frame.instance_id.as_deref(), expected);
                    assert_eq!(frame.output_version, Some(5));
                }
            }
        }
    }
    #[test]
    fn strict_hides_untrusted_error_text_and_tui_retains_it() {
        let port = Port {
            remote: false,
            response: Response::Error("unsafe\u{1b}[31m".into()),
        };
        assert!(matches!(strict(&port), Err(error) if error == "Preview unavailable"));
        assert!(
            matches!(capture(&port, "session", Some("one"), 0, IdentityPolicy::TuiCompatible), Err(error) if error == "unsafe\u{1b}[31m")
        );
    }
    #[test]
    fn strict_frame_limits_and_wrapped_shape() {
        let cursor = Cursor {
            row: u16::MAX,
            col: u16::MAX,
            visible: true,
        };
        let f = |rows, wrapped| frame(false, Some("one"), rows, wrapped, cursor);
        let fit = f(vec![vec![run(&"x".repeat(512), false)]], vec![false]);
        let result = strict(&fit).unwrap();
        assert_eq!(result.cells[0].len(), 512);
        assert!(!result.cursor.visible);
        assert_eq!((result.cursor.row, result.cursor.col), (0, 0));
        assert!(strict(&f(vec![vec![run("x", false)]], vec![])).is_ok());
        for bad in [
            f(vec![vec![run(&"x".repeat(513), false)]], vec![false]),
            f(vec![vec![run(&"x".repeat(257), true)]], vec![false]),
            f(vec![vec![run(&"界".repeat(257), false)]], vec![false]),
            f(vec![vec![run("x", false)]], vec![false, false]),
            f(vec![vec![run(&"x".repeat(512), false)]; 129], vec![]),
            f(vec![vec![]; 513], vec![]),
        ] {
            assert!(strict(&bad).is_err());
        }
        let compat = capture(
            &fit,
            "session",
            Some("one"),
            0,
            IdentityPolicy::TuiCompatible,
        )
        .unwrap();
        assert_eq!(
            (compat.cursor.row, compat.cursor.col, compat.cursor.visible),
            (u16::MAX, u16::MAX, true)
        );
    }
    #[test]
    fn strict_accepts_collapsed_wide_cells_at_column_boundary() {
        let wide = StyledCell {
            text: "界".into(),
            wide: true,
            ..Default::default()
        };
        let continuation = StyledCell {
            text: String::new(),
            ..Default::default()
        };
        let cells: Vec<_> = (0..256)
            .flat_map(|_| [wide.clone(), continuation.clone()])
            .collect();
        let rows = vec![collapse_runs(&cells)];
        let frame = frame(
            false,
            Some("one"),
            rows,
            vec![],
            Cursor {
                row: 0,
                col: 0,
                visible: false,
            },
        );
        let result = strict(&frame).unwrap();
        assert_eq!(result.cells[0].len(), 256);
        assert!(result.cells[0]
            .iter()
            .all(|cell| cell.text == "界" && cell.wide));
    }
    #[test]
    fn strict_filters_local_controls_and_rejects_oversize() {
        let frame = strict(&plain(false, Some("one"), "a\u{1b}b")).unwrap();
        assert_eq!(
            frame.cells[0]
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>(),
            "ab"
        );
        assert!(
            matches!(strict(&plain(true, Some("one"), &"x".repeat(1_048_577))), Err(error) if error == "Preview exceeds display limits")
        );
    }
    #[test]
    fn tui_legacy_local_and_remote_rules_are_preserved() {
        assert!(capture(
            &plain(false, None, "local"),
            "session",
            Some("old"),
            0,
            IdentityPolicy::TuiCompatible
        )
        .is_ok());
        let remote = plain(true, Some("one"), "a\u{1b}b");
        assert!(capture(&remote, "session", None, 0, IdentityPolicy::TuiCompatible).is_err());
        assert!(capture(
            &remote,
            "session",
            Some("two"),
            0,
            IdentityPolicy::TuiCompatible
        )
        .is_err());
        let frame = capture(
            &remote,
            "session",
            Some("one"),
            0,
            IdentityPolicy::TuiCompatible,
        )
        .unwrap();
        assert_eq!(
            frame.cells[0]
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>(),
            "ab"
        );
    }
}
