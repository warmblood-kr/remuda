//! Split geometry for the whole-node peer (RemotePort) list.
//!
//! This is an adapter, not a second policy: the Lua composition has already
//! validated an axis and a list extent (`cluster_ui::Geometry`). Here those
//! cells become three zero-based, half-open rectangles over the frame body so
//! paint, capture, caret, hit tests, drag, history and resize all agree on one
//! answer. Pure by design: no terminal IO, so tests can call it directly.
//!
//! The last frame row is the footer and is never part of the body. Title and
//! empty-state rows are list content, so they need no reserved rows here.

use crate::cluster_ui::{Axis, Geometry, Viewport};

/// A rectangle in frame cells. `x`/`y` are zero-based; `w`/`h` are extents;
/// the rectangle is half-open, so the right and bottom edges are excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    /// Whether a zero-based frame cell is inside this rect. Widened on purpose:
    /// `x + w` can exceed `u16::MAX` for a full-width rect.
    pub(crate) fn contains(&self, x: u16, y: u16) -> bool {
        let (x, y) = (u32::from(x), u32::from(y));
        let (left, top) = (u32::from(self.x), u32::from(self.y));
        x >= left && x < left + u32::from(self.w) && y >= top && y < top + u32::from(self.h)
    }
}

/// One resolved frame: the list leaf (when drawn), the preview leaf and the
/// one-cell divider between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PeerGeometry {
    pub axis: Axis,
    pub list: Option<Rect>,
    pub preview: Rect,
    pub divider: Option<Rect>,
    /// The validated layout no longer fits this viewport (a resize outran
    /// recomposition, or a caller used different dimensions). Paint the
    /// fullscreen preview and recompose/validate instead of inventing a split.
    pub stale: bool,
}

impl PeerGeometry {
    /// The body with no list: keys mode, or a layout that does not fit.
    fn fullscreen(axis: Axis, cols: u16, body_h: u16, stale: bool) -> Self {
        Self {
            axis,
            list: None,
            preview: Rect {
                x: 0,
                y: 0,
                w: cols,
                h: body_h,
            },
            divider: None,
            stale,
        }
    }
}

/// Resolve the frame's list/divider/preview rects from the validated layout.
/// The result adapts to `viewport`, so a resize resolves before any paint; a
/// layout that no longer fits returns the fullscreen preview with `stale`.
pub(crate) fn resolve(
    viewport: Viewport,
    validated: &Geometry,
    list_visible: bool,
) -> PeerGeometry {
    let cols = viewport.cols;
    let body_h = viewport.rows.saturating_sub(1);
    let axis = validated.axis;
    if !list_visible {
        return PeerGeometry::fullscreen(axis, cols, body_h, false);
    }
    let n = validated.list_cells;
    match axis {
        Axis::Horizontal => {
            // The validated list + divider + one preview column must still fit.
            if n == 0 || body_h == 0 || u32::from(n) + 2 > u32::from(cols) {
                return PeerGeometry::fullscreen(axis, cols, body_h, true);
            }
            let preview_x = n.saturating_add(1);
            PeerGeometry {
                axis,
                list: Some(Rect {
                    x: 0,
                    y: 0,
                    w: n,
                    h: body_h,
                }),
                divider: Some(Rect {
                    x: n,
                    y: 0,
                    w: 1,
                    h: body_h,
                }),
                preview: Rect {
                    x: preview_x,
                    y: 0,
                    w: cols.saturating_sub(preview_x),
                    h: body_h,
                },
                stale: false,
            }
        }
        Axis::Vertical => {
            // The validated list + divider + one preview row must still fit.
            if n == 0 || u32::from(n) + 2 > u32::from(body_h) {
                return PeerGeometry::fullscreen(axis, cols, body_h, true);
            }
            let preview_y = n.saturating_add(1);
            PeerGeometry {
                axis,
                list: Some(Rect {
                    x: 0,
                    y: 0,
                    w: cols,
                    h: n,
                }),
                divider: Some(Rect {
                    x: 0,
                    y: n,
                    w: cols,
                    h: 1,
                }),
                preview: Rect {
                    x: 0,
                    y: preview_y,
                    w: cols,
                    h: body_h.saturating_sub(preview_y),
                },
                stale: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport(cols: u16, rows: u16) -> Viewport {
        Viewport::new(cols, rows, rows.saturating_sub(1))
            .with_preview_chrome(0)
            .with_gutter(0)
    }

    fn geometry(axis: Axis, list_cells: u16) -> Geometry {
        Geometry {
            axis,
            list_cells,
            divider: match axis {
                Axis::Horizontal => "│".into(),
                Axis::Vertical => "─".into(),
            },
        }
    }

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn horizontal_splits_width_and_keeps_the_body_height() {
        let resolved = resolve(viewport(80, 24), &geometry(Axis::Horizontal, 30), true);
        assert_eq!(resolved.axis, Axis::Horizontal);
        assert_eq!(resolved.list, Some(rect(0, 0, 30, 23)));
        assert_eq!(resolved.divider, Some(rect(30, 0, 1, 23)));
        assert_eq!(resolved.preview, rect(31, 0, 49, 23));
    }

    #[test]
    fn vertical_splits_height_and_keeps_the_body_width() {
        let resolved = resolve(viewport(80, 24), &geometry(Axis::Vertical, 8), true);
        assert_eq!(resolved.list, Some(rect(0, 0, 80, 8)));
        assert_eq!(resolved.divider, Some(rect(0, 8, 80, 1)));
        assert_eq!(resolved.preview, rect(0, 9, 80, 14));
    }

    #[test]
    fn both_axes_agree_at_eighty_and_forty_columns() {
        for cols in [80, 40] {
            let horizontal = resolve(viewport(cols, 24), &geometry(Axis::Horizontal, 24), true);
            assert_eq!(horizontal.list.unwrap().h, 23);
            assert_eq!(horizontal.preview.h, 23);
            assert_eq!(horizontal.preview.w, cols - 25);

            let vertical = resolve(viewport(cols, 24), &geometry(Axis::Vertical, 10), true);
            assert_eq!(vertical.list.unwrap().w, cols);
            assert_eq!(vertical.divider.unwrap().w, cols);
            assert_eq!(vertical.preview.w, cols);
            assert_eq!(vertical.preview.y, 11);
            assert_eq!(vertical.preview.h, 12);
        }
    }

    #[test]
    fn the_qa_viewport_places_the_divider_on_its_own_row() {
        // 80x24, body 23, list 5: preview starts on row 7 (1-based) and is 17
        // rows tall; cursor local (2,3) is terminal (3,10) via this origin.
        let resolved = resolve(viewport(80, 24), &geometry(Axis::Vertical, 5), true);
        assert_eq!(resolved.list, Some(rect(0, 0, 80, 5)));
        assert_eq!(resolved.divider, Some(rect(0, 5, 80, 1)));
        assert_eq!(resolved.preview, rect(0, 6, 80, 17));
        assert_eq!(
            (resolved.preview.x + 2 + 1, resolved.preview.y + 3 + 1),
            (3, 10)
        );
    }

    #[test]
    fn a_hidden_list_is_a_fullscreen_preview() {
        let resolved = resolve(viewport(80, 24), &geometry(Axis::Vertical, 8), false);
        assert_eq!(resolved.list, None);
        assert_eq!(resolved.divider, None);
        assert_eq!(resolved.preview, rect(0, 0, 80, 23));
    }

    #[test]
    fn an_oversized_list_never_underflows_and_marks_itself_stale() {
        // A stale composition can outlive a shrink. Never invent a narrower
        // split than the validator approved: drop the list and ask for a
        // recompose.
        let horizontal = resolve(
            viewport(20, 24),
            &geometry(Axis::Horizontal, u16::MAX),
            true,
        );
        assert!(horizontal.stale);
        assert_eq!(horizontal.list, None);
        assert_eq!(horizontal.divider, None);
        assert_eq!(horizontal.preview, rect(0, 0, 20, 23));

        let vertical = resolve(viewport(80, 10), &geometry(Axis::Vertical, u16::MAX), true);
        assert!(vertical.stale);
        assert_eq!(vertical.list, None);
        assert_eq!(vertical.preview, rect(0, 0, 80, 9));
    }

    #[test]
    fn no_room_falls_back_to_the_fullscreen_preview() {
        for (cols, rows, axis, cells) in [
            (2, 24, Axis::Horizontal, 1),
            (1, 24, Axis::Horizontal, 1),
            (80, 2, Axis::Vertical, 1),
            (80, 1, Axis::Vertical, 1),
            (80, 24, Axis::Horizontal, 0),
            (80, 24, Axis::Vertical, 0),
        ] {
            let resolved = resolve(viewport(cols, rows), &geometry(axis, cells), true);
            assert!(resolved.stale, "{axis:?} {cols}x{rows} {cells}");
            assert_eq!(resolved.list, None, "{axis:?} {cols}x{rows} {cells}");
            assert_eq!(resolved.divider, None);
            assert_eq!(resolved.preview.w, cols);
            assert_eq!(resolved.preview.h, rows.saturating_sub(1));
        }
    }

    #[test]
    fn a_hidden_list_is_never_stale() {
        let resolved = resolve(viewport(80, 2), &geometry(Axis::Vertical, 40), false);
        assert!(!resolved.stale);
        assert_eq!(resolved.list, None);
        assert_eq!(resolved.preview, rect(0, 0, 80, 1));
    }

    #[test]
    fn a_zero_width_viewport_has_no_content_and_does_not_claim_stale_when_hidden() {
        let shown = resolve(viewport(0, 0), &geometry(Axis::Horizontal, 1), true);
        assert!(shown.stale);
        assert_eq!(shown.list, None);
        assert_eq!(shown.divider, None);
        assert_eq!(shown.preview, rect(0, 0, 0, 0));

        let hidden = resolve(viewport(0, 0), &geometry(Axis::Horizontal, 1), false);
        assert!(!hidden.stale, "keys mode is intentional, not transitional");
        assert_eq!(hidden.preview, rect(0, 0, 0, 0));

        let vertical = resolve(viewport(0, 0), &geometry(Axis::Vertical, 1), true);
        assert!(vertical.stale);
        assert_eq!(vertical.preview, rect(0, 0, 0, 0));
    }

    #[test]
    fn contains_is_half_open_and_does_not_wrap() {
        let whole = rect(0, 0, u16::MAX, u16::MAX);
        assert!(whole.contains(0, 0));
        assert!(whole.contains(u16::MAX - 1, u16::MAX - 1));
        assert!(!whole.contains(u16::MAX, u16::MAX), "right/bottom is open");
        // A rect at the far corner must not wrap its own extent away.
        assert!(rect(u16::MAX, u16::MAX, 1, 1).contains(u16::MAX, u16::MAX));

        let cell = rect(5, 7, 1, 1);
        assert!(cell.contains(5, 7));
        for outside in [(6, 7), (5, 8), (4, 7), (5, 6)] {
            assert!(!cell.contains(outside.0, outside.1), "{outside:?}");
        }
    }

    #[test]
    fn the_three_rects_tile_the_body_without_overlap() {
        for axis in [Axis::Horizontal, Axis::Vertical] {
            let resolved = resolve(viewport(40, 24), &geometry(axis, 12), true);
            let list = resolved.list.unwrap();
            let divider = resolved.divider.unwrap();
            let preview = resolved.preview;
            if axis == Axis::Horizontal {
                assert_eq!(list.x + list.w, divider.x);
                assert_eq!(divider.x + divider.w, preview.x);
                assert_eq!(preview.x + preview.w, 40);
                assert_eq!(preview.h, 23);
            } else {
                assert_eq!(list.y + list.h, divider.y);
                assert_eq!(divider.y + divider.h, preview.y);
                assert_eq!(preview.y + preview.h, 23);
                assert_eq!(preview.w, 40);
            }
            assert!(divider.w >= 1 && divider.h >= 1);
            assert!(divider.w == 1 || divider.h == 1, "one divider cell thick");
            for y in 0..23u16 {
                for x in [0u16, 39] {
                    let hits = [
                        list.contains(x, y),
                        divider.contains(x, y),
                        preview.contains(x, y),
                    ];
                    assert_eq!(
                        hits.iter().filter(|hit| **hit).count(),
                        1,
                        "{axis:?} {x},{y}"
                    );
                }
            }
        }
    }
}
