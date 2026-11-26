use gpui::{div, prelude::*, px, rgb, Div, MouseButton, MouseMoveEvent, Stateful};

const DIVIDER_WIDTH: f32 = 8.;
const MIN_PANE_WIDTH: f32 = 180.;

pub(crate) struct DocumentSplit {
    terminal_fraction: f32,
    drag: Option<(f32, f32)>,
}

impl Default for DocumentSplit {
    fn default() -> Self {
        Self {
            terminal_fraction: 0.5,
            drag: None,
        }
    }
}

impl DocumentSplit {
    pub(crate) fn widths(&self, available: f32) -> (f32, f32) {
        let content = (available - DIVIDER_WIDTH).max(0.);
        let min = MIN_PANE_WIDTH.min(content / 2.);
        let terminal = (content * self.terminal_fraction).clamp(min, content - min);
        (terminal, content - terminal)
    }

    pub(crate) fn start(&mut self, x: f32, available: f32) {
        self.drag = Some((x, self.widths(available).0));
    }

    pub(crate) fn mouse_move(&mut self, event: &MouseMoveEvent, available: f32) -> bool {
        let Some((origin, terminal)) = self.drag else {
            return false;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            return self.stop();
        }
        let content = available - DIVIDER_WIDTH;
        if content <= 0. {
            return false;
        }
        let min = (MIN_PANE_WIDTH / content).min(0.5);
        let fraction =
            ((terminal + f32::from(event.position.x) - origin) / content).clamp(min, 1. - min);
        let changed = fraction != self.terminal_fraction;
        self.terminal_fraction = fraction;
        changed
    }

    pub(crate) fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub(crate) fn stop(&mut self) -> bool {
        self.drag.take().is_some()
    }

    pub(crate) fn divider() -> Stateful<Div> {
        div()
            .id("document-split-divider")
            .debug_selector(|| "document-split-divider".into())
            .w(px(DIVIDER_WIDTH))
            .flex_none()
            .cursor_col_resize()
            .bg(rgb(0x354055))
            .hover(|divider| divider.bg(rgb(0x799ebe)))
    }
}
