//! Read-only VFS documents for the native Browser. All decoding runs off the UI thread.

use gpui::{div, img, prelude::*, px, rgb, AnyElement, App, RenderImage, Window};
use image::{Frame, ImageFormat, ImageReader, RgbaImage};
use remuda_native::document_html::{self, Target};
use remuda_native::document_markdown::LineStyle;
use std::{io::Cursor, sync::Arc};

const MAX_TEXT_BYTES: usize = 1024 * 1024;
const RENDER_PIXELS: f64 = 8_000_000.0;
const RENDER_SIDE: f64 = 8192.0;
const PNG_MAX_WIDTH: u32 = 65_536;
const PNG_MAX_PIXELS: u64 = 160_000_000;

#[derive(Clone)]
pub(crate) struct Document {
    pub title: String,
    body: Body,
}

#[derive(Clone)]
enum Body {
    Text(document_html::Document, Vec<InlineImage>, Vec<LineStyle>),
    Image(Raster),
}

#[derive(Clone)]
struct InlineImage {
    source: document_html::Image,
    preview: Option<Raster>,
}

#[derive(Clone)]
struct Raster {
    image: Arc<RenderImage>,
    width: u32,
    height: u32,
}

impl Raster {
    fn load(bytes: &[u8], format: ImageFormat, pixels: f64) -> Result<Self, String> {
        let raster = if format == ImageFormat::Png {
            decode_png(bytes, pixels)?
        } else {
            decode_buffered(bytes, format, pixels)?
        };
        let (width, height) = raster.dimensions();
        let image = RenderImage::new(smallvec::smallvec![Frame::new(raster)]);
        Ok(Self {
            image: Arc::new(image),
            width,
            height,
        })
    }

    fn render(&self, viewport_width: f32, zoom: f32) -> AnyElement {
        let (width, height) =
            presentation_dimensions(self.width, self.height, viewport_width, zoom);
        div()
            .min_w(px(width))
            .child(img(self.image.clone()).w(px(width)).h(px(height)))
            .into_any_element()
    }
}

pub(crate) fn decode(path: &str, bytes: &[u8]) -> Result<Document, String> {
    let filename = path.rsplit('/').next().unwrap_or(path);
    if let Some(format) = raster_format(bytes) {
        return Ok(Document {
            title: filename.into(),
            body: Body::Image(Raster::load(bytes, format, RENDER_PIXELS)?),
        });
    }
    if bytes.len() > MAX_TEXT_BYTES {
        return Err("This document is too large for text preview".into());
    }
    let lower = filename.to_ascii_lowercase();
    if [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|ext| lower.ends_with(ext))
    {
        return Err("This image is damaged or uses an unsupported format".into());
    }
    let source =
        std::str::from_utf8(bytes).map_err(|_| "This document format is unsupported".to_owned())?;
    let (mut projected, mut images, mut styles) =
        if lower.ends_with(".html") || lower.ends_with(".htm") {
            let (document, images) = document_html::parse_with_images(path, source)?;
            (document, images, Vec::new())
        } else {
            remuda_native::document_markdown::parse_styled(path, source)?
        };
    let title = if projected.title.is_empty() {
        filename.into()
    } else {
        projected.title.clone()
    };
    if projected.lines.first() == Some(&title) {
        projected.lines.remove(0);
        if !styles.is_empty() {
            styles.remove(0);
        }
        for image in &mut images {
            image.line = image.line.saturating_sub(1);
        }
    }
    let images = images
        .into_iter()
        .map(|source| InlineImage {
            source,
            preview: None,
        })
        .collect();
    Ok(Document {
        title,
        body: Body::Text(projected, images, styles),
    })
}

fn raster_format(bytes: &[u8]) -> Option<ImageFormat> {
    image::guess_format(bytes).ok().filter(|format| {
        matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
        )
    })
}

impl Document {
    /// Release uploaded textures when replacing this document. CPU pixels
    /// are reclaimed when the document and any in-flight references drop.
    pub(crate) fn release_images(&self, window: &mut Window) {
        match &self.body {
            Body::Image(raster) => {
                let _ = window.drop_image(raster.image.clone());
            }
            Body::Text(_, assets, _) => {
                for raster in assets.iter().filter_map(|asset| asset.preview.as_ref()) {
                    let _ = window.drop_image(raster.image.clone());
                }
            }
        }
    }

    pub(crate) fn is_image(&self) -> bool {
        matches!(self.body, Body::Image(_))
    }

    /// Asset requests are optional for the document's first paint. Call on the
    /// read worker so an image never delays rendering the document text.
    pub(crate) fn asset_paths(&self) -> impl Iterator<Item = &str> {
        let images = match &self.body {
            Body::Text(_, images, _) => images.as_slice(),
            Body::Image(_) => &[],
        };
        images
            .iter()
            .filter_map(|image| match &image.source.target {
                Target::Vfs(path) if !path.is_empty() && !path.ends_with('/') => {
                    Some(path.as_str())
                }
                _ => None,
            })
    }

    pub(crate) fn load_assets(&mut self, mut fetch: impl FnMut(&str) -> Option<Vec<u8>>) {
        // Keep inline images inexpensive. Any other reference can still be
        // opened in the full document panel.
        const INLINE_PREVIEWS: usize = 4;
        const INLINE_PIXELS: f64 = 1_250_000.0;
        let Body::Text(_, images, _) = &mut self.body else {
            return;
        };
        let mut previews = std::collections::HashMap::<String, Option<Raster>>::new();
        for image in images {
            let Target::Vfs(path) = &image.source.target else {
                continue;
            };
            if path.is_empty()
                || path.ends_with('/')
                || previews.len() >= INLINE_PREVIEWS && !previews.contains_key(path)
            {
                continue;
            }
            image.preview = previews
                .entry(path.clone())
                .or_insert_with(|| {
                    let bytes = fetch(path)?;
                    Raster::load(&bytes, raster_format(&bytes)?, INLINE_PIXELS).ok()
                })
                .clone();
        }
    }

    pub(crate) fn render(
        &self,
        viewport_width: f32,
        zoom: f32,
        open: impl Fn(String, &mut Window, &mut App) + Clone + 'static,
    ) -> AnyElement {
        match &self.body {
            Body::Image(image) => image.render(viewport_width, zoom),
            Body::Text(document, images, styles) => {
                // Render only the inert VFS projection. Treat link targets as
                // typed local paths; the receiver rechecks read authorization.
                let mut body = div().flex().flex_col().gap_2();
                let mut pending = images.iter().enumerate().peekable();
                for (line_index, line) in document.lines.iter().enumerate() {
                    let style = styles.get(line_index).copied().unwrap_or_default();
                    let block = div().min_h(px(12.)).child(line.clone());
                    let block = match style {
                        LineStyle::Heading(level) => block
                            .pt_2()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_size(px(if level <= 2 { 20. } else { 16. })),
                        LineStyle::Quote => block
                            .pl_3()
                            .border_l_2()
                            .border_color(rgb(0xa8b5c2))
                            .text_color(rgb(0x465565)),
                        LineStyle::Code => block
                            .px_3()
                            .bg(rgb(0xe8edf1))
                            .font_family(if cfg!(windows) { "Consolas" } else { "Menlo" }),
                        LineStyle::Body => block,
                    };
                    body = body.child(block);
                    while pending
                        .peek()
                        .is_some_and(|(_, image)| image.source.line <= line_index)
                    {
                        let (index, image) = pending.next().unwrap();
                        body =
                            body.child(render_inline(index, image, viewport_width, open.clone()));
                    }
                }
                for (index, image) in pending {
                    body = body.child(render_inline(index, image, viewport_width, open.clone()));
                }
                body.when(!document.links.is_empty(), |view| {
                    view.child(
                        div().flex().flex_col().gap_2().children(
                            document
                                .links
                                .iter()
                                .enumerate()
                                .filter_map(|(index, link)| {
                                    let Target::Vfs(path) = &link.target else {
                                        return None;
                                    };
                                    let path = path.clone();
                                    let label = format!("[{}] {}", index + 1, link.label);
                                    let open = open.clone();
                                    Some(
                                        div()
                                            .id(("browser-document-link", index))
                                            .cursor_pointer()
                                            .text_color(rgb(0x235495))
                                            .underline()
                                            .child(label)
                                            .on_click(move |_, window, cx| {
                                                open(path.clone(), window, cx)
                                            }),
                                    )
                                }),
                        ),
                    )
                })
                .into_any_element()
            }
        }
    }
}

fn render_inline(
    index: usize,
    image: &InlineImage,
    viewport_width: f32,
    open: impl Fn(String, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let label = if image.source.label.is_empty() {
        "Open image".to_owned()
    } else {
        image.source.label.clone()
    };
    if let Target::Vfs(path) = &image.source.target {
        let path = path.clone();
        let content = image.preview.as_ref().map_or_else(
            || {
                div()
                    .text_color(rgb(0x235495))
                    .underline()
                    .child(label.clone())
                    .into_any_element()
            },
            |preview| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(preview.render(viewport_width, 1.))
                    .child(div().text_sm().child(label.clone()))
                    .into_any_element()
            },
        );
        div()
            .id(("browser-inline-image", index))
            .cursor_pointer()
            .child(content)
            .on_click(move |_, window, app| open(path.clone(), window, app))
            .into_any_element()
    } else {
        div().text_sm().child(label).into_any_element()
    }
}

fn presentation_dimensions(width: u32, height: u32, viewport_width: f32, zoom: f32) -> (f32, f32) {
    let width = width as f32;
    let height = height as f32;
    let scale = (viewport_width.max(1.) / width).min(1.) * zoom.clamp(0.25, 8.);
    (width * scale, height * scale)
}

fn preview_dimensions(width: u32, height: u32, pixels: f64) -> (u32, u32) {
    let (width, height) = (f64::from(width), f64::from(height));
    let scale = 1.0_f64
        .min(RENDER_SIDE / width.max(height))
        .min((pixels / (width * height)).sqrt());
    (
        (width * scale).floor().max(1.) as u32,
        (height * scale).floor().max(1.) as u32,
    )
}

/// Non-interlaced PNGs are sampled a row at a time, so tall or high-resolution
/// images need only a bounded display buffer plus one decoded row at a time.
fn decode_png(bytes: &[u8], pixels: f64) -> Result<RgbaImage, String> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: 32 * 1024 * 1024,
    });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    let source = decoder.read_header_info().map_err(image_error)?;
    let (width, height) = (source.width, source.height);
    if width > PNG_MAX_WIDTH || u64::from(width) * u64::from(height) > PNG_MAX_PIXELS {
        return Err("This PNG's dimensions exceed the decoder's supported range".into());
    }
    if source.interlaced {
        return decode_buffered(bytes, ImageFormat::Png, pixels);
    }
    let mut reader = decoder.read_info().map_err(image_error)?;
    if reader.info().animation_control.is_some() {
        // APNG's first frame can be smaller than the canvas.
        return decode_buffered(bytes, ImageFormat::Png, pixels);
    }
    let channels = reader.output_color_type().0.samples();
    let (out_width, out_height) = preview_dimensions(width, height, pixels);
    let mut pixels = RgbaImage::new(out_width, out_height);
    let mut output_y = 0;
    for source_y in 0..height {
        let row = reader
            .next_row()
            .map_err(image_error)?
            .ok_or_else(|| "PNG ended before its declared height".to_owned())?;
        if output_y >= out_height || sample(output_y, height, out_height) != source_y {
            continue;
        }
        if row.data().len() != width as usize * channels {
            return Err("PNG row length does not match its declared width".into());
        }
        for x in 0..out_width {
            let source_x = sample(x, width, out_width) as usize;
            let data = &row.data()[source_x * channels..(source_x + 1) * channels];
            let color = match data {
                [luma] => [*luma, *luma, *luma, 255],
                [luma, alpha] => [*luma, *luma, *luma, *alpha],
                [red, green, blue] => [*blue, *green, *red, 255],
                [red, green, blue, alpha] => [*blue, *green, *red, *alpha],
                _ => return Err("This PNG color format is unsupported".into()),
            };
            pixels.put_pixel(x, output_y, image::Rgba(color));
        }
        output_y += 1;
    }
    reader.finish().map_err(image_error)?;
    Ok(pixels)
}

fn sample(index: u32, source: u32, target: u32) -> u32 {
    (((2 * u64::from(index) + 1) * u64::from(source)) / (2 * u64::from(target))) as u32
}

fn decode_buffered(bytes: &[u8], format: ImageFormat, pixels: f64) -> Result<RgbaImage, String> {
    let open = || {
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(384 * 1024 * 1024);
        reader.limits(limits);
        reader
    };
    let (width, height) = open().into_dimensions().map_err(image_error)?;
    if u64::from(width) * u64::from(height) > 64_000_000 {
        return Err("This image's dimensions exceed the decoder's supported range".into());
    }
    let (out_width, out_height) = preview_dimensions(width, height, pixels);
    let image = open().decode().map_err(image_error)?;
    let mut rgba = if (width, height) == (out_width, out_height) {
        image.into_rgba8()
    } else {
        image.thumbnail_exact(out_width, out_height).into_rgba8()
    };
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(rgba)
}

fn image_error(error: impl std::fmt::Display) -> String {
    format!("Could not read this image: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_png_uses_preview_pixels_instead_of_refusing_the_image() {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 1500, 10_000);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_compression(png::Compression::Fast);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&vec![42; 15_000_000])
            .unwrap();
        let pixels = decode_png(&bytes, RENDER_PIXELS).unwrap();
        assert_eq!(pixels.dimensions(), (1095, 7302));
        assert!(u64::from(pixels.width()) * u64::from(pixels.height()) <= 8_000_000);
        assert_eq!(
            pixels.get_pixel(0, pixels.height() - 1).0,
            [42, 42, 42, 255]
        );
    }

    #[test]
    fn jpeg_above_the_previous_pixel_limit_still_opens() {
        let mut bytes = Vec::new();
        let rgb = image::RgbImage::from_pixel(5100, 3300, image::Rgb([12, 75, 185]));
        rgb.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Jpeg)
            .unwrap();
        let document = decode("docs/large.jpg", &bytes).unwrap();
        let Body::Image(image) = document.body else {
            panic!("expected image")
        };
        assert_eq!(
            (image.width, image.height),
            preview_dimensions(5100, 3300, RENDER_PIXELS)
        );
        let bgra = &image.image.as_bytes(0).unwrap()[..4];
        assert!(bgra[0] > 170 && bgra[1] > 65 && bgra[2] < 25 && bgra[3] == 255);
    }

    #[test]
    fn html_is_an_inert_projection_and_markdown_uses_typed_drive_links() {
        let html = decode(
            "docs/plan.html",
            b"<title>Plan</title><script>hidden</script><h1>Visible</h1>",
        )
        .unwrap();
        assert_eq!(html.title, "Plan");
        let Body::Text(html, ..) = html.body else {
            panic!("expected text projection")
        };
        assert!(html.lines.iter().any(|line| line.contains("Visible")));
        assert!(!html.lines.iter().any(|line| line.contains("hidden")));
        let markdown = decode(
            "docs/plan.md",
            b"# Plan\n[Local](next.md) [External](https://example.com)",
        )
        .unwrap();
        let Body::Text(markdown, ..) = markdown.body else {
            panic!("expected text projection")
        };
        assert_eq!(
            markdown.links[0].target,
            Target::Vfs("docs/next.md".to_owned())
        );
        assert!(matches!(markdown.links[1].target, Target::Blocked(_)));
    }

    #[test]
    fn html_and_markdown_load_only_local_assets_and_share_duplicate_previews() {
        let mut png = Vec::new();
        let image = image::RgbaImage::from_pixel(5, 3, image::Rgba([3, 4, 5, 255]));
        image
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        for (path, source) in [
            (
                "docs/plan.md",
                "![local](image.png)\n![same](image.png)\n![outside](https://example.com/x.png)",
            ),
            (
                "docs/plan.html",
                "<img src='image.png'><img src='image.png'><img src='https://example.com/x.png'>",
            ),
        ] {
            let mut document = decode(path, source.as_bytes()).unwrap();
            assert_eq!(
                document.asset_paths().collect::<Vec<_>>(),
                ["docs/image.png"; 2]
            );
            let mut requests = Vec::new();
            document.load_assets(|path| {
                requests.push(path.to_owned());
                Some(png.clone())
            });
            assert_eq!(requests, ["docs/image.png"]);
            let Body::Text(_, images, _) = document.body else {
                panic!("expected text")
            };
            let first = &images[0].preview.as_ref().unwrap().image;
            let duplicate = &images[1].preview.as_ref().unwrap().image;
            assert!(Arc::ptr_eq(first, duplicate));
            assert!(images[2].preview.is_none());
        }
    }

    #[test]
    fn image_fits_available_width_and_zooms_with_aspect_ratio() {
        assert_eq!(presentation_dimensions(2000, 6000, 400., 1.), (400., 1200.));
        assert_eq!(presentation_dimensions(2000, 6000, 400., 2.), (800., 2400.));
    }
}
