//! Read-only, single-node cluster tree. Kept separate from the herd TUI so its
//! navigation and rendering cannot change the existing session workflow.

use crate::client::{self, RawMode};
use remuda_core::agent::StyledCell;
use remuda_core::protocol::{expand_runs, Request, Response};
use remuda_core::registry::SessionSummary;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

pub struct ClusterUi {
    node: String,
    sessions: Vec<SessionSummary>,
    selected: usize,
    active: usize,
    expanded: bool,
}

impl ClusterUi {
    pub fn new(node: &str, sessions: Vec<SessionSummary>) -> Self {
        Self {
            node: node.into(),
            sessions,
            selected: 0,
            active: 0,
            expanded: true,
        }
    }

    pub fn render(&self, cols: u16, rows: u16, screen: &str) -> String {
        let width = usize::from(cols.max(1));
        let height = usize::from(rows.max(1));
        let mut frame = Vec::new();
        frame.push("remuda · cluster (1/1 reachable)".into());
        frame.push(format!(
            "{} {}       local · reachable · sync 0s",
            if self.expanded { "▼" } else { "▶" },
            self.node
        ));
        if self.expanded {
            for (index, session) in self.sessions.iter().enumerate() {
                frame.push(format!(
                    "{}{} {:<12} {}",
                    if index == self.selected { ">" } else { " " },
                    "   ",
                    session.name,
                    if session.alive { "live" } else { "ended" }
                ));
            }
        }
        let pane_header = self.sessions.get(self.active).map_or_else(
            || format!("{} · no sessions", self.node),
            |session| {
                format!(
                    "{} / {} · {} · snapshot 0s ago",
                    self.node,
                    session.name,
                    if session.alive { "live" } else { "ended" }
                )
            },
        );
        let divider = "─".repeat(width);
        frame.push(divider);
        frame.push(pane_header);
        let screen_rows = height.saturating_sub(frame.len() + 1);
        frame.extend(screen.lines().take(screen_rows).map(str::to_string));
        frame.push("↑/↓ move · ←/→ collapse/expand · Enter select · q detach".into());
        frame
            .into_iter()
            .map(|line| truncate(&line, width))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(&mut self, code: crossterm::event::KeyCode) -> bool {
        use crossterm::event::KeyCode::*;
        match code {
            Char('q') => return true,
            Up => self.selected = self.selected.saturating_sub(1),
            Down => self.selected = (self.selected + 1).min(self.sessions.len().saturating_sub(1)),
            Left => self.expanded = false,
            Right => self.expanded = true,
            Enter => {
                self.expanded = true;
                self.active = self.selected;
            }
            _ => {}
        }
        false
    }

    fn select_target(&mut self, target: Option<&str>) -> io::Result<()> {
        let Some(target) = target else { return Ok(()) };
        let Some((node, session)) = target.split_once('/') else {
            return Err(io::Error::other("target must be node/session"));
        };
        if node != self.node {
            return Err(io::Error::other(format!(
                "node {node} is not available in the local-only tree"
            )));
        }
        self.selected = self
            .sessions
            .iter()
            .position(|item| item.name == session)
            .ok_or_else(|| {
                io::Error::other(format!("session {session} was not found on {node}"))
            })?;
        self.active = self.selected;
        Ok(())
    }
}

fn truncate(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

fn list(path: &Path) -> io::Result<Vec<SessionSummary>> {
    match client::request(path, &Request::List).map_err(io::Error::other)? {
        Response::Sessions(sessions) => Ok(sessions),
        Response::Error(error) => Err(io::Error::other(error)),
        other => Err(io::Error::other(format!(
            "unexpected List response: {other:?}"
        ))),
    }
}

fn screen(path: &Path, name: &str) -> io::Result<String> {
    let response = client::request(
        path,
        &Request::CaptureStyled {
            name: name.into(),
            scrollback: 0,
        },
    )
    .map_err(io::Error::other)?;
    match response {
        Response::StyledScreen { rows, .. } => Ok(rows
            .iter()
            .map(|row| {
                let cells = expand_runs(row);
                cells
                    .iter()
                    .map(|cell: &StyledCell| cell.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")),
        Response::Error(error) => Err(io::Error::other(error)),
        other => Err(io::Error::other(format!(
            "unexpected CaptureStyled response: {other:?}"
        ))),
    }
}

pub fn run(path: &Path, node: &str, target: Option<&str>) -> io::Result<()> {
    let _raw = RawMode::enable()?;
    run_loop(path, node, target)
}

/// Read one local-only frame through the existing List and CaptureStyled IPC.
pub fn read_frame(
    path: &Path,
    node: &str,
    target: Option<&str>,
    cols: u16,
    rows: u16,
) -> io::Result<String> {
    let mut ui = ClusterUi::new(node, list(path)?);
    ui.select_target(target)?;
    let body = ui
        .sessions
        .get(ui.active)
        .map(|session| screen(path, &session.name))
        .transpose()?
        .unwrap_or_default();
    Ok(ui.render(cols, rows, &body))
}

fn run_loop(path: &Path, node: &str, target: Option<&str>) -> io::Result<()> {
    let mut ui = ClusterUi::new(node, list(path)?);
    ui.select_target(target)?;
    loop {
        let current = list(path).unwrap_or_else(|_| ui.sessions.clone());
        ui.sessions = current;
        ui.selected = ui.selected.min(ui.sessions.len().saturating_sub(1));
        ui.active = ui.active.min(ui.sessions.len().saturating_sub(1));
        let body = ui
            .sessions
            .get(ui.active)
            .and_then(|s| screen(path, &s.name).ok())
            .unwrap_or_default();
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let frame = ui.render(cols, rows, &body);
        crossterm::execute!(
            io::stdout(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        )?;
        write!(io::stdout(), "{frame}")?;
        io::stdout().flush()?;
        if crossterm::event::poll(Duration::from_millis(250))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if key.kind != crossterm::event::KeyEventKind::Press {
                    continue;
                }
                if ui.key(key.code) {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ClusterUi;
    use remuda_core::{SessionSummary, Size};
    use std::time::Duration;

    fn sessions() -> Vec<SessionSummary> {
        vec![SessionSummary {
            name: "dev".into(),
            alive: true,
            idle: Duration::ZERO,
            output_idle: Some(Duration::ZERO),
            size: Size::new(80, 24),
            attached: false,
            human_idle: None,
            mouse_tracking: false,
        }]
    }

    #[test]
    fn local_node_is_expanded_and_tree_header_names_selected_target() {
        let ui = ClusterUi::new("studio", sessions());
        let frame = ui.render(80, 24, "snapshot");
        assert!(frame.contains("▼ studio       local"));
        assert!(frame.contains("    dev          live"));
        assert!(frame.contains("studio / dev · live · snapshot 0s ago"));
    }

    #[test]
    fn arrows_move_the_tree_cursor_and_enter_selects_the_session() {
        let mut sessions = sessions();
        sessions.push(SessionSummary {
            name: "shell".into(),
            alive: true,
            idle: Duration::ZERO,
            output_idle: Some(Duration::ZERO),
            size: Size::new(80, 24),
            attached: false,
            human_idle: None,
            mouse_tracking: false,
        });
        let mut ui = ClusterUi::new("studio", sessions);
        ui.key(crossterm::event::KeyCode::Down);
        assert!(ui.render(80, 24, "").contains("studio / dev · live"));
        ui.key(crossterm::event::KeyCode::Enter);
        assert!(ui.render(80, 24, "").contains("studio / shell · live"));
    }
}
