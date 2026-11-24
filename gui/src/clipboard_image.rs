use gpui::{ClipboardEntry, ClipboardItem, Image, ImageFormat};
use std::io::{Cursor, Write};

const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_DECODE_BYTES: usize = 64 * 1024 * 1024;

pub(crate) fn first_image(item: &ClipboardItem) -> Option<&Image> {
    item.entries().iter().find_map(|entry| match entry {
        ClipboardEntry::Image(image) => Some(image),
        ClipboardEntry::String(_) => None,
    })
}

/// GPUI recognizes Windows PNG and JPEG clipboard formats. Windows may
/// also offer the same screenshot only as a system bitmap.
pub(crate) fn read_fallback() -> Option<ClipboardItem> {
    #[cfg(windows)]
    {
        let bytes = clipboard_win::get_clipboard(clipboard_win::formats::Bitmap).ok()?;
        Some(ClipboardItem::new_image(&Image::from_bytes(
            ImageFormat::Bmp,
            bytes,
        )))
    }
    #[cfg(not(windows))]
    None
}

/// Keep clipboard files until the GUI window closes so a terminal application
/// can still read an image after the user detaches or submits a draft.
#[derive(Default)]
pub(crate) struct ClipboardImages {
    files: Vec<tempfile::NamedTempFile>,
    bytes: usize,
}

impl ClipboardImages {
    pub(crate) fn paste(
        &mut self,
        image: &Image,
        input: &crate::live_session::LiveSession,
    ) -> Result<crate::live_session::Submit, &'static str> {
        let (file, bytes) = private_image(image, 4 * MAX_IMAGE_BYTES - self.bytes)?;
        let path = file
            .path()
            .to_str()
            .filter(|path| !path.chars().any(char::is_control))
            .ok_or("Clipboard image file path is unsupported")?;
        let result = input.paste_image_path(path);
        if result == crate::live_session::Submit::Queued {
            self.bytes += bytes;
            self.files.push(file);
        }
        Ok(result)
    }
}

fn private_image(
    image: &Image,
    disk_remaining: usize,
) -> Result<(tempfile::NamedTempFile, usize), &'static str> {
    if image.bytes.len() > MAX_DECODE_BYTES {
        return Err("Clipboard image exceeds 64 MiB before conversion");
    }
    let converted;
    let (suffix, bytes) = match (image.format, image::guess_format(&image.bytes)) {
        (ImageFormat::Png, Ok(image::ImageFormat::Png)) => (".png", image.bytes.as_slice()),
        (ImageFormat::Jpeg, Ok(image::ImageFormat::Jpeg)) => (".jpg", image.bytes.as_slice()),
        (ImageFormat::Bmp, Ok(image::ImageFormat::Bmp))
        | (ImageFormat::Tiff, Ok(image::ImageFormat::Tiff)) => {
            converted = encode_png(&image.bytes)?;
            (".png", converted.as_slice())
        }
        _ => return Err("Clipboard image format is unsupported"),
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("Clipboard image exceeds 16 MiB");
    }
    if bytes.len() > disk_remaining {
        return Err("Clipboard image storage full · reopen the window to clear it");
    }
    let mut file = tempfile::Builder::new()
        .prefix("remuda-image-")
        .suffix(suffix)
        .tempfile()
        .map_err(|_| "Could not create a private clipboard image file")?;
    file.write_all(bytes)
        .map_err(|_| "Could not write the clipboard image")?;
    Ok((file, bytes.len()))
}

fn encode_png(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let invalid = "Could not decode the clipboard image";
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| invalid)?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(MAX_DECODE_BYTES as u64);
    reader.limits(limits);
    let mut png = Cursor::new(Vec::new());
    reader
        .decode()
        .map_err(|_| invalid)?
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|_| "Could not encode the clipboard image")?;
    Ok(png.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_image_keeps_the_payload_in_a_private_temporary_file() {
        let bytes = b"\x89PNG\r\n\x1a\nfixture";
        let image = Image::from_bytes(ImageFormat::Png, bytes.to_vec());
        assert_eq!(first_image(&ClipboardItem::new_image(&image)), Some(&image));
        let (file, _) = private_image(&image, MAX_IMAGE_BYTES).unwrap();
        let path = file.path().to_owned();
        assert_eq!(path.extension().unwrap(), "png");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        drop(file);
        assert!(!path.exists());
    }

    #[test]
    fn system_bitmap_is_converted_to_a_png_without_losing_pixels() {
        let image = image::RgbImage::from_pixel(1, 1, image::Rgb([25, 50, 75]));
        let mut bitmap = Cursor::new(Vec::new());
        image
            .write_to(&mut bitmap, image::ImageFormat::Bmp)
            .unwrap();
        let (file, _) = private_image(
            &Image::from_bytes(ImageFormat::Bmp, bitmap.into_inner()),
            MAX_IMAGE_BYTES,
        )
        .unwrap();
        assert_eq!(file.path().extension().unwrap(), "png");
        assert_eq!(image::open(file.path()).unwrap().into_rgb8(), image);
    }
}
