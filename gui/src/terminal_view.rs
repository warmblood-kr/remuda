use gpui::{canvas, div, font, prelude::*, px, rgb, AnyElement, FontWeight};
use remuda_core::agent::{Color, Size, StyledCell};
use serde::{Deserialize, Serialize};
use std::{cell::Cell, rc::Rc};

const FONT_SIZE: f32 = 13.;
const LINE_HEIGHT: f32 = 17.;
const FONT: &str = if cfg!(windows) { "Consolas" } else { "Menlo" };
const FG: u32 = 0xe8eaf0;
const BG: u32 = 0x101216;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Screen {
    pub text: String,
    #[serde(default)]
    pub resize_refused: bool,
    #[serde(default)]
    pub scrollback_len: usize,
    pub rows: Vec<Vec<remuda_core::protocol::StyledRun>>,
}

#[cfg(test)]
impl From<&str> for Screen {
    fn from(text: &str) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }
}

impl Screen {
    pub(crate) fn from_cells(mut cells: Vec<Vec<StyledCell>>) -> Result<Self, String> {
        let mut text = String::new();
        let mut rows = Vec::with_capacity(cells.len());
        for row in &mut cells {
            for cell in row.iter_mut() {
                cell.text.retain(|ch| {
                    !ch.is_control()
                        && !remuda_core::protocol::is_secret_prompt_format_or_separator(ch)
                });
                if text.len() + cell.text.len() >= 65_536 {
                    return Err("too large".into());
                }
                text.push_str(&cell.text);
            }
            text.push('\n');
            rows.push(remuda_core::protocol::collapse_runs(row));
        }
        Ok(Self {
            text,
            rows,
            ..Self::default()
        })
    }

    pub(crate) fn view(&self) -> AnyElement {
        let body = div()
            .font_family(FONT)
            .text_size(px(FONT_SIZE))
            .line_height(px(LINE_HEIGHT))
            .text_color(rgb(FG))
            .bg(rgb(BG))
            .whitespace_nowrap()
            .min_w_full();
        if self.rows.is_empty() {
            return body.child(self.text.clone()).into_any_element();
        }
        body.children(self.rows.iter().map(|row| {
            div()
                .flex()
                .h(px(LINE_HEIGHT))
                .children(row.iter().map(|run| {
                    let (fg, bg) = if run.inverse {
                        (color(run.bg, BG), color(run.fg, FG))
                    } else {
                        (color(run.fg, FG), color(run.bg, BG))
                    };
                    div()
                        .flex_none()
                        .text_color(rgb(fg))
                        .bg(rgb(bg))
                        .when(run.bold, |d| d.font_weight(FontWeight::BOLD))
                        .when(run.italic, |d| d.italic())
                        .when(run.underline, |d| d.underline())
                        .child(run.text.clone())
                }))
        }))
        .into_any_element()
    }
}

#[derive(Default)]
pub(crate) struct HistoryScroll {
    pub offset: usize,
    fraction: f32,
}

impl HistoryScroll {
    pub(crate) fn apply(&mut self, delta: gpui::ScrollDelta, limit: usize) -> bool {
        let whole = self.wheel_lines(delta);
        self.shift(whole, limit)
    }

    pub(crate) fn wheel_lines(&mut self, delta: gpui::ScrollDelta) -> isize {
        let lines = f32::from(delta.pixel_delta(px(LINE_HEIGHT)).y) / LINE_HEIGHT;
        self.fraction += lines;
        let whole = self.fraction.trunc() as isize;
        self.fraction -= whole as f32;
        whole
    }

    pub(crate) fn shift(&mut self, whole: isize, limit: usize) -> bool {
        let next = remuda_native::mouse::scroll_offset(self.offset, whole, limit).min(limit);
        std::mem::replace(&mut self.offset, next) != next
    }
}

pub(crate) type Viewport = Rc<Cell<Option<Size>>>;

pub(crate) fn measure(viewport: Viewport) -> impl IntoElement {
    canvas(
        move |bounds, _, cx| {
            let text = cx.text_system();
            let font_id = text.resolve_font(&font(FONT));
            if let Ok(cell) = text.advance(font_id, px(FONT_SIZE), 'M') {
                let cols = (f32::from(bounds.size.width) / f32::from(cell.width)) as u16;
                let rows = (f32::from(bounds.size.height) / LINE_HEIGHT) as u16;
                viewport.set((cols > 0 && rows > 0).then(|| Size::for_viewport(cols, rows)));
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
    .top_0()
    .left_0()
}

fn color(color: Color, default: u32) -> u32 {
    const ANSI: [u32; 16] = [
        0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666,
        0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
    ];
    let rgb = |r: u8, g: u8, b: u8| u32::from_be_bytes([0, r, g, b]);
    match color {
        Color::Default => default,
        Color::Rgb(r, g, b) => rgb(r, g, b),
        Color::Idx(n @ 0..=15) => ANSI[n as usize],
        Color::Idx(n @ 16..=231) => {
            let n = n - 16;
            let level = |n| if n == 0 { 0 } else { 55 + 40 * n };
            rgb(level(n / 36), level(n / 6 % 6), level(n % 6))
        }
        Color::Idx(n) => {
            let gray = 8 + (n - 232) * 10;
            rgb(gray, gray, gray)
        }
    }
}
