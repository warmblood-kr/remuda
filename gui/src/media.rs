//! Bounded decoding for the GUI image reader.
//!
//! Only PNG and JPEG are accepted, by magic bytes rather than by file name.
//! Dimensions, total pixels and decoder allocation are capped before the image
//! reaches GPUI; a failure returns a short message for the alt/error view.
//! Nothing here fetches a URL or executes content.

use gpui::RenderImage;
use image::{Frame, ImageReader, Limits};
use std::{io::Cursor, sync::Arc};

/// Largest encoded image the VFS read path will return.
pub const MAX_MEDIA_BYTES: usize = 8 * 1024 * 1024;
/// Widest or tallest accepted image.
pub const MAX_DIMENSION: u32 = 8192;
/// Most pixels decoded into memory.
pub const MAX_PIXELS: u64 = 16_000_000;
/// Decoder allocation cap. The preflight pixel check bounds the output at
/// 64 MiB; one RGBA->BGRA conversion can transiently hold two such buffers.
const DECODE_ALLOC_BYTES: u64 = 160 * 1024 * 1024;
const _: () = assert!(MAX_PIXELS * 4 * 2 <= DECODE_ALLOC_BYTES);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
}

#[derive(Clone, Debug)]
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    /// Pixels in GPUI's BGRA order.
    pub bgra: Vec<u8>,
}

pub fn format(bytes: &[u8]) -> Option<Format> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Format::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(Format::Jpeg)
    } else {
        None
    }
}

pub fn extension_is_image(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg")
}

/// Decode PNG/JPEG bytes with every resource bound applied before allocation.
pub fn decode(bytes: &[u8]) -> Result<Decoded, String> {
    if bytes.is_empty() {
        return Err("empty image".into());
    }
    if bytes.len() > MAX_MEDIA_BYTES {
        return Err("image is larger than 8 MiB".into());
    }
    if format(bytes).is_none() {
        return Err("only PNG and JPEG images are supported".into());
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(DECODE_ALLOC_BYTES);
    // Read only the header first: a small compressed file can claim huge
    // dimensions, so the pixel bound must be checked before decoding pixels.
    let mut header = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "image format could not be read".to_owned())?;
    header.limits(limits.clone());
    let (width, height) = header
        .into_dimensions()
        .map_err(|error| format!("image header failed: {error}"))?;
    check_dimensions(width, height)?;
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "image format could not be read".to_owned())?;
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| format!("image decode failed: {error}"))?;
    let (width, height) = (image.width(), image.height());
    check_dimensions(width, height)?;
    let mut rgba = image.into_rgba8();
    for pixel in rgba.chunks_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(Decoded {
        width,
        height,
        bgra: rgba.into_raw(),
    })
}

/// Turn decoded BGRA pixels into GPUI's upload type. The caller keeps the Arc.
pub fn render_image(decoded: Decoded) -> Option<(Arc<RenderImage>, u32, u32)> {
    let width = decoded.width;
    let height = decoded.height;
    let buffer = image::ImageBuffer::from_raw(width, height, decoded.bgra)?;
    let image = RenderImage::new(smallvec::smallvec![Frame::new(buffer)]);
    Some((Arc::new(image), width, height))
}

fn check_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("image has an empty size".into());
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(format!(
            "image is {width}×{height}, above the {MAX_DIMENSION} px dimension limit"
        ));
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!(
            "image is {width}×{height}, above the {MAX_PIXELS} pixel limit"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([1, 2, 3, 255]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn png_decodes_to_bgra_with_bounds() {
        let decoded = decode(&png(2, 3)).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 3));
        assert_eq!(decoded.bgra.len(), 2 * 3 * 4);
        assert_eq!(&decoded.bgra[..4], &[3, 2, 1, 255]);
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for byte in data {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    /// Rewrite a real one-pixel PNG's IHDR dimensions and CRC. The compressed
    /// payload stays tiny, so only a pre-decode header check can refuse it
    /// without allocating the claimed pixels.
    fn png_with_claimed_size(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = png(1, 1);
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&crc.to_be_bytes());
        bytes
    }

    #[test]
    fn header_dimensions_are_checked_before_pixel_decoding() {
        let compressed_claim = png_with_claimed_size(5000, 4000);
        let error = decode(&compressed_claim).unwrap_err();
        assert!(error.contains("pixel limit"), "{error}");
        assert!(decode(&png_with_claimed_size(9000, 100)).is_err());
    }

    #[test]
    fn non_images_and_oversized_headers_are_refused() {
        assert!(decode(b"not an image").is_err());
        assert!(decode(&[0x89, b'P', b'N', b'G', 0, 0]).is_err());
        let huge = png(9000, 1);
        assert!(decode(&huge).is_err());
        let many = png(1, 1);
        assert!(decode(&many).is_ok());
        let mut oversized = many.clone();
        oversized.resize(MAX_MEDIA_BYTES + 1, 0);
        assert!(decode(&oversized).is_err());
        assert!(!extension_is_image("a.txt"));
        assert!(extension_is_image("a.PNG") && extension_is_image("a.jpeg"));
    }
}
