//! Validated split geometry. The validator knows only physical safety: the
//! two leaves and the divider must fit inside the body area and leave at least
//! one usable preview cell. Presentation policy lives in the embedded Lua.

use super::Viewport;
use serde::Serialize;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Axis {
    Horizontal,
    Vertical,
}

impl Axis {
    pub fn as_str(self) -> &'static str {
        match self {
            Axis::Horizontal => "horizontal",
            Axis::Vertical => "vertical",
        }
    }

    fn default_divider(self) -> &'static str {
        match self {
            Axis::Horizontal => "│",
            Axis::Vertical => "─",
        }
    }
}

/// What Lua asked for, before validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub axis: Axis,
    pub list_cells: u16,
    pub divider: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub axis: Axis,
    /// List leaf columns (horizontal) or rows (vertical), divider excluded.
    pub list_cells: u16,
    pub divider: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GeometryError {
    NoRoom,
    PreviewTooSmall,
    BadDivider,
}

impl std::fmt::Display for GeometryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            GeometryError::NoRoom => "layout does not fit the body",
            GeometryError::PreviewTooSmall => "layout leaves no usable preview",
            GeometryError::BadDivider => "divider must be one printable cell",
        })
    }
}

impl Geometry {
    pub fn list_rows(&self, viewport: &Viewport) -> u16 {
        match self.axis {
            Axis::Horizontal => viewport.body_rows,
            Axis::Vertical => self.list_cells,
        }
    }

    pub fn preview_width(&self, viewport: &Viewport) -> u16 {
        match self.axis {
            Axis::Horizontal => viewport
                .cols
                .saturating_sub(self.list_cells)
                .saturating_sub(1),
            Axis::Vertical => viewport.cols,
        }
    }

    pub fn preview_height(&self, viewport: &Viewport) -> u16 {
        match self.axis {
            Axis::Horizontal => viewport.body_rows,
            Axis::Vertical => viewport
                .body_rows
                .saturating_sub(self.list_cells)
                .saturating_sub(1),
        }
    }

    /// Preview height after the caller's native header.
    pub fn preview_body_height(&self, viewport: &Viewport) -> u16 {
        self.preview_height(viewport)
            .saturating_sub(viewport.preview_chrome_rows)
    }

    pub fn preview_cols(&self, viewport: &Viewport) -> u16 {
        self.preview_width(viewport)
    }
}

pub fn resolve_geometry(viewport: &Viewport, layout: &Layout) -> Result<Geometry, GeometryError> {
    let divider = match layout.divider.as_deref() {
        None => layout.axis.default_divider().to_string(),
        Some("") => layout.axis.default_divider().to_string(),
        Some(text) => {
            let mut graphemes = text.graphemes(true);
            let first = graphemes.next().ok_or(GeometryError::BadDivider)?;
            if graphemes.next().is_some()
                || first.chars().any(char::is_control)
                || first.chars().any(char::is_whitespace)
                || UnicodeWidthStr::width(first) != 1
            {
                return Err(GeometryError::BadDivider);
            }
            text.to_string()
        }
    };
    if layout.list_cells == 0 {
        return Err(GeometryError::NoRoom);
    }
    match layout.axis {
        Axis::Horizontal => {
            // list + divider + at least one preview column
            if u32::from(layout.list_cells) + 2 > u32::from(viewport.cols) {
                return Err(GeometryError::NoRoom);
            }
            if viewport.body_rows == 0 || viewport.body_rows <= viewport.preview_chrome_rows {
                return Err(GeometryError::PreviewTooSmall);
            }
        }
        Axis::Vertical => {
            // list + divider + native preview chrome + at least one preview row
            let needed =
                u32::from(layout.list_cells) + 1 + u32::from(viewport.preview_chrome_rows) + 1;
            if needed > u32::from(viewport.body_rows) {
                return Err(GeometryError::NoRoom);
            }
        }
    }
    Ok(Geometry {
        axis: layout.axis,
        list_cells: layout.list_cells,
        divider,
    })
}
