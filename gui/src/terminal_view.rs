use gpui::{
    canvas, div, font, prelude::*, px, rgb, AnyElement, Bounds, ClipboardItem, FontWeight,
    HighlightStyle, MouseButton, Pixels, Point, StyledText, TextLayout,
};
use remuda_core::agent::{Color, Size, StyledCell};
use remuda_core::protocol::StyledRun;
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
};
use unicode_segmentation::UnicodeSegmentation;

pub(crate) type LinkHandler = Rc<dyn Fn(&str, &mut gpui::Window, &mut gpui::App)>;

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
    pub rows: Vec<Vec<StyledRun>>,
    #[serde(default)]
    pub wrapped: Vec<bool>,
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

    pub(crate) fn view(&self, selection: &TextSelection, open: Option<LinkHandler>) -> AnyElement {
        let snapshot = selection.0.borrow();
        let frame = snapshot
            .as_ref()
            .filter(|selected| selected.dragging)
            .map_or(self, |selected| &selected.frame);
        let selected = snapshot
            .as_ref()
            .filter(|selected| selected.matches_frame(frame))
            .map(SelectionRange::ordered);
        let body = div()
            .id("selectable-terminal")
            .debug_selector(|| "selectable-terminal".into())
            .relative()
            .font_family(FONT)
            .text_size(px(FONT_SIZE))
            .line_height(px(LINE_HEIGHT))
            .text_color(rgb(FG))
            .bg(rgb(BG))
            .whitespace_nowrap()
            .min_w_full();
        if frame.rows.is_empty() {
            return body.child(frame.text.clone()).into_any_element();
        }
        let bounds = Rc::new(Cell::new(None));
        let rendered_bounds = bounds.clone();
        let mut hit_rows = Vec::with_capacity(frame.rows.len());
        let rendered = frame
            .rows
            .iter()
            .enumerate()
            .map(|(row_index, row)| {
                let mut hit_runs = Vec::with_capacity(row.len());
                let mut offset = 0;
                let elements = row
                    .iter()
                    .enumerate()
                    .filter(|(_, run)| !run.text.is_empty())
                    .map(|(run_index, run)| {
                        let range = offset..offset + run.text.len();
                        offset = range.end;
                        let highlight = selected.and_then(|(start, end)| {
                            row_selection(start, end, row_index, range.clone())
                        });
                        let link = open.as_ref().map(|open| (open, selection));
                        let (element, layout) =
                            render_run(run, range.start, highlight, link, (row_index, run_index));
                        hit_runs.push(layout);
                        element
                    })
                    .collect::<Vec<_>>();
                hit_rows.push(hit_runs);
                div().flex().h(px(LINE_HEIGHT)).children(elements)
            })
            .collect::<Vec<_>>();
        let layout = Rc::new(HitLayout {
            bounds,
            rows: hit_rows,
        });
        let frame = frame.clone();
        let start_selection = selection.clone();
        let move_selection = selection.clone();
        let end_selection = selection.clone();
        let outside_selection = selection.clone();
        let start_layout = layout.clone();
        let move_layout = layout.clone();
        let end_layout = layout.clone();
        body.child(
            canvas(
                move |bounds, _, _| rendered_bounds.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
            .top_0()
            .left_0(),
        )
        .children(rendered)
        .on_mouse_down(MouseButton::Left, move |event, window, _| {
            start_selection.start(
                frame.clone(),
                start_layout.hit(event.position),
                event.position,
            );
            window.refresh();
        })
        .on_mouse_move(move |event, window, _| {
            if event.dragging()
                && move_selection.advance(move_layout.hit(event.position), event.position)
            {
                window.refresh();
            }
        })
        .on_mouse_up(MouseButton::Left, move |event, window, _| {
            if end_selection.finish(end_layout.hit(event.position), event.position) {
                window.refresh();
            }
        })
        .on_mouse_up_out(MouseButton::Left, move |event, window, _| {
            if outside_selection.finish(layout.hit(event.position), event.position) {
                window.refresh();
            }
        })
        .into_any_element()
    }
}

fn render_run(
    run: &StyledRun,
    start: usize,
    selected: Option<Range<usize>>,
    link: Option<(&LinkHandler, &TextSelection)>,
    (row_index, run_index): (usize, usize),
) -> (AnyElement, HitRun) {
    let (fg, bg) = if run.inverse {
        (color(run.bg, BG), color(run.fg, FG))
    } else {
        (color(run.fg, FG), color(run.bg, BG))
    };
    let text = StyledText::new(run.text.clone()).with_highlights(selected.map(|range| {
        (
            range,
            HighlightStyle {
                background_color: Some(rgb(0x386491).into()),
                color: Some(rgb(0xffffff).into()),
                ..HighlightStyle::default()
            },
        )
    }));
    let hit_run = HitRun {
        start,
        graphemes: run
            .text
            .grapheme_indices(true)
            .map(|(byte, _)| byte)
            .chain(std::iter::once(run.text.len()))
            .collect(),
        layout: text.layout().clone(),
    };
    let element = div()
        .flex_none()
        .text_color(rgb(fg))
        .bg(rgb(bg))
        .when(run.bold, |d| d.font_weight(FontWeight::BOLD))
        .when(run.italic, |d| d.italic())
        .when(run.underline, |d| d.underline())
        .child(text);
    let element = match (link, run.hyperlink.as_deref()) {
        (Some((open, selection)), Some(uri))
            if remuda_native::terminal_link::Target::parse(uri).is_some() =>
        {
            let open = open.clone();
            let selection = selection.clone();
            let uri = uri.to_owned();
            element
                .id(gpui::SharedString::from(format!(
                    "terminal-link-{row_index}-{run_index}"
                )))
                .debug_selector(|| format!("terminal-link-{row_index}-{run_index}"))
                .cursor_pointer()
                .underline()
                .when(
                    !run.inverse && run.fg == Color::Default && run.bg == Color::Default,
                    |d| d.text_color(rgb(0x8bc3ff)),
                )
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    let moved = matches!(event, gpui::ClickEvent::Mouse(click)
                        if dragged(click.down.position, click.up.position) || selection.did_drag());
                    if !moved {
                        open(&uri, window, cx);
                    }
                })
                .into_any_element()
        }
        _ => element.into_any_element(),
    };
    (element, hit_run)
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct TextPoint {
    row: usize,
    byte: usize,
}

struct HitRun {
    start: usize,
    graphemes: Vec<usize>,
    layout: TextLayout,
}

struct HitLayout {
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    rows: Vec<Vec<HitRun>>,
}

impl HitLayout {
    fn hit(&self, position: Point<Pixels>) -> TextPoint {
        let top = self.bounds.get().map_or(px(0.), |bounds| bounds.top());
        let row = ((f32::from(position.y - top) / LINE_HEIGHT) as usize).min(self.rows.len() - 1);
        let byte = self.rows[row]
            .iter()
            .flat_map(|run| {
                run.graphemes.iter().filter_map(|&byte| {
                    let point = run.layout.position_for_index(byte)?;
                    Some((run.start + byte, f32::from(point.x - position.x).abs()))
                })
            })
            .min_by(|left, right| left.1.total_cmp(&right.1))
            .map_or(0, |(byte, _)| byte);
        TextPoint { row, byte }
    }
}

fn copy_modifiers(modifiers: gpui::Modifiers) -> bool {
    !modifiers.alt
        && (modifiers.platform || (modifiers.control && (cfg!(windows) || modifiers.shift)))
}

fn dragged(start: Point<Pixels>, now: Point<Pixels>) -> bool {
    let delta = now - start;
    f32::from(delta.x).abs().max(f32::from(delta.y).abs()) >= 3.
}

struct SelectionRange {
    frame: Screen,
    start: TextPoint,
    end: TextPoint,
    origin: Point<Pixels>,
    dragging: bool,
    moved: bool,
}

impl SelectionRange {
    fn ordered(&self) -> (TextPoint, TextPoint) {
        (self.start.min(self.end), self.start.max(self.end))
    }

    fn matches_frame(&self, frame: &Screen) -> bool {
        let (start, end) = self.ordered();
        let range = start.row..=end.row;
        self.frame.rows.get(range.clone()) == frame.rows.get(range.clone())
            && self.frame.wrapped.get(range.clone()) == frame.wrapped.get(range)
    }

    fn text(&self) -> Option<String> {
        let (start, end) = self.ordered();
        if start == end {
            return None;
        }
        let mut text = String::new();
        for row_index in start.row..=end.row {
            let mut row = String::new();
            let mut offset = 0;
            for run in &self.frame.rows[row_index] {
                let range = offset..offset + run.text.len();
                offset = range.end;
                if let Some(bytes) = row_selection(start, end, row_index, range) {
                    row.push_str(&run.text[bytes]);
                }
            }
            if row_index != end.row {
                text.push_str(row.trim_end());
                if !self.frame.wrapped.get(row_index).copied().unwrap_or(false) {
                    text.push('\n');
                }
            } else {
                text.push_str(&row);
            }
        }
        Some(text)
    }
}

fn row_selection(
    start: TextPoint,
    end: TextPoint,
    row: usize,
    run: Range<usize>,
) -> Option<Range<usize>> {
    if !(start.row..=end.row).contains(&row) {
        return None;
    }
    let first = if row == start.row {
        start.byte.max(run.start)
    } else {
        run.start
    };
    let last = if row == end.row {
        end.byte.min(run.end)
    } else {
        run.end
    };
    (first < last).then(|| first - run.start..last - run.start)
}

#[derive(Clone, Default)]
pub(crate) struct TextSelection(Rc<RefCell<Option<SelectionRange>>>);

impl TextSelection {
    pub(crate) fn clear(&self) {
        self.0.borrow_mut().take();
    }

    pub(crate) fn copy(
        &self,
        key: &gpui::Keystroke,
        frame: Option<&Screen>,
        cx: &mut gpui::App,
    ) -> bool {
        let mods = key.modifiers;
        if key.key != "c" || !copy_modifiers(mods) {
            return false;
        }
        let snapshot = self.0.borrow();
        let selected = snapshot
            .as_ref()
            .filter(|selected| frame.is_some_and(|frame| selected.matches_frame(frame)));
        if let Some(text) = selected.and_then(SelectionRange::text) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            return true;
        }
        false
    }

    fn start(&self, frame: Screen, start: TextPoint, origin: Point<Pixels>) {
        *self.0.borrow_mut() = Some(SelectionRange {
            frame,
            start,
            end: start,
            origin,
            dragging: true,
            moved: false,
        });
    }

    fn advance(&self, end: TextPoint, at: Point<Pixels>) -> bool {
        let mut selection = self.0.borrow_mut();
        let Some(current) = selection.as_mut().filter(|s| s.dragging) else {
            return false;
        };
        current.moved |= dragged(current.origin, at);
        current.moved && std::mem::replace(&mut current.end, end) != end
    }

    fn finish(&self, end: TextPoint, at: Point<Pixels>) -> bool {
        self.advance(end, at);
        let mut selection = self.0.borrow_mut();
        if let Some(current) = selection.as_mut().filter(|s| s.dragging) {
            current.dragging = false;
            return true;
        }
        false
    }

    fn did_drag(&self) -> bool {
        self.0.borrow().as_ref().is_some_and(|s| s.moved)
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Render, Window};

    fn row(text: &str) -> Vec<StyledCell> {
        text.graphemes(true)
            .map(|text| StyledCell {
                text: text.into(),
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn selection_joins_soft_wraps_and_preserves_hard_breaks() {
        let mut frame = Screen::from_cells(vec![row("x한"), row("글 "), row("emoji 🌿.")]).unwrap();
        frame.wrapped = vec![true, false, false];
        let state = TextSelection::default();
        state.start(
            frame,
            TextPoint { row: 2, byte: 10 },
            gpui::point(px(0.), px(0.)),
        );
        assert!(state.finish(TextPoint { row: 0, byte: 1 }, gpui::point(px(5.), px(0.))));
        assert_eq!(
            state.0.borrow().as_ref().unwrap().text().as_deref(),
            Some("한글\nemoji 🌿")
        );
    }

    struct TerminalFixture {
        selection: TextSelection,
        screen: Screen,
        focus: gpui::FocusHandle,
        clicks: Rc<Cell<u32>>,
    }

    impl Render for TerminalFixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let clicks = self.clicks.clone();
            let open: LinkHandler = Rc::new(move |_, _, _| clicks.set(clicks.get() + 1));
            div()
                .track_focus(&self.focus)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    if this
                        .selection
                        .copy(&event.keystroke, Some(&this.screen), cx)
                    {
                        cx.stop_propagation();
                    }
                }))
                .child(self.screen.view(&self.selection, Some(open)))
        }
    }

    #[gpui::test]
    fn dragging_across_link_copies_text_without_opening_it(cx: &mut gpui::TestAppContext) {
        let clicks = Rc::new(Cell::new(0));
        let fixture_clicks = clicks.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let focus = cx.focus_handle();
            focus.focus(window, cx);
            let screen = Screen::from_cells(vec![vec![StyledCell {
                text: "한글 🌿 terminal".into(),
                hyperlink: Some("https://example.org/remuda/guide".into()),
                ..Default::default()
            }]])
            .unwrap();
            TerminalFixture {
                selection: TextSelection::default(),
                screen,
                focus,
                clicks: fixture_clicks,
            }
        });
        let link = cx.debug_bounds("terminal-link-0-0").unwrap();
        let from = gpui::point(link.left() + px(1.), link.center().y);
        let to = gpui::point(link.right() - px(1.), link.center().y);
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: from,
            ..Default::default()
        });
        cx.simulate_event(MouseMoveEvent {
            position: to,
            pressed_button: Some(MouseButton::Left),
            ..Default::default()
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: to,
            ..Default::default()
        });
        cx.simulate_keystrokes("ctrl-shift-c");
        assert_eq!(
            cx.read(|app| app.read_from_clipboard().unwrap().text().unwrap()),
            "한글 🌿 terminal"
        );
        assert_eq!(clicks.get(), 0);
        cx.simulate_click(link.center(), gpui::Modifiers::none());
        assert_eq!(clicks.get(), 1);
    }
}
