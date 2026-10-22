//! Server-side thumbnails and blurhash placeholders.

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder as _, ImageReader, imageops::FilterType};

use crate::MediaError;

/// A small JPEG preview of an image.
#[derive(Debug, Clone)]
pub struct Thumbnail {
    /// JPEG bytes (always `image/jpeg`).
    pub jpeg: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// A ~30 character placeholder the UI paints while the image loads.
    pub blurhash: String,
}

/// Cap on decoded pixels (a 50-megapixel photo is fine; a decompression bomb is not).
const MAX_PIXELS: u64 = 100_000_000;

fn decode(bytes: &[u8]) -> Result<DynamicImage, MediaError> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| MediaError::Image(error.to_string()))?;
    let mut decoder = reader
        .into_decoder()
        .map_err(|error| MediaError::Image(error.to_string()))?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(MediaError::Image(
            "image is too large to process".to_owned(),
        ));
    }
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|error| MediaError::Image(error.to_string()))?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    Ok(image)
}

/// Decode `bytes`, honour its EXIF orientation, fit it inside `max_edge` x `max_edge`
/// (never enlarging) and encode a JPEG plus its blurhash.
///
/// # Errors
/// [`MediaError::Image`] when the bytes are not a decodable image.
pub fn thumbnail(bytes: &[u8], max_edge: u32) -> Result<Thumbnail, MediaError> {
    let image = decode(bytes)?;
    let small = if image.width() > max_edge || image.height() > max_edge {
        image.resize(max_edge, max_edge, FilterType::Triangle)
    } else {
        image
    };
    // JPEG has no alpha: flatten onto RGB.
    let rgb = DynamicImage::ImageRgb8(small.to_rgb8());
    let mut jpeg = Vec::new();
    rgb.write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
        .map_err(|error| MediaError::Image(error.to_string()))?;
    // Blurhash needs few pixels: hash a 32px version.
    let tiny = rgb.resize(32, 32, FilterType::Triangle).to_rgba8();
    let blurhash = blurhash::encode(4, 3, tiny.width(), tiny.height(), tiny.as_raw())
        .map_err(|error| MediaError::Image(error.to_string()))?;
    Ok(Thumbnail {
        jpeg,
        width: rgb.width(),
        height: rgb.height(),
        blurhash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            width,
            height,
            image::Rgb([200, 30, 30]),
        ));
        let mut out = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("encode");
        out
    }

    #[test]
    fn a_big_image_is_fitted_inside_the_edge() {
        let thumb = thumbnail(&png(1000, 500), 320).expect("thumb");
        assert_eq!((thumb.width, thumb.height), (320, 160));
        assert_eq!(&thumb.jpeg[..3], &[0xFF, 0xD8, 0xFF]);
        assert!(!thumb.blurhash.is_empty());
    }

    #[test]
    fn a_small_image_is_never_enlarged() {
        let thumb = thumbnail(&png(100, 50), 320).expect("thumb");
        assert_eq!((thumb.width, thumb.height), (100, 50));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(thumbnail(b"not an image", 320).is_err());
    }
}
