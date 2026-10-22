//! Content types from the leading bytes, never from what a client claims.

/// A recognised file type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sniffed {
    /// JPEG.
    Jpeg,
    /// PNG.
    Png,
    /// GIF.
    Gif,
    /// WebP.
    Webp,
    /// HEIC/HEIF.
    Heic,
    /// PDF.
    Pdf,
}

impl Sniffed {
    /// The media type.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Heic => "image/heic",
            Self::Pdf => "application/pdf",
        }
    }

    /// Whether it is a picture.
    #[must_use]
    pub const fn is_image(self) -> bool {
        !matches!(self, Self::Pdf)
    }
}

/// What `bytes` is, by magic bytes; `None` for anything else.
#[must_use]
pub fn detect(bytes: &[u8]) -> Option<Sniffed> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(Sniffed::Jpeg);
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(Sniffed::Png);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(Sniffed::Gif);
    }
    if bytes.starts_with(b"%PDF-") {
        return Some(Sniffed::Pdf);
    }
    let brand = bytes.get(8..12)?;
    if bytes.starts_with(b"RIFF") && brand == b"WEBP" {
        return Some(Sniffed::Webp);
    }
    if bytes.get(4..8) == Some(b"ftyp")
        && [
            &b"heic"[..],
            b"heix",
            b"hevc",
            b"mif1",
            b"msf1",
            b"heim",
            b"heis",
        ]
        .contains(&brand)
    {
        return Some(Sniffed::Heic);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_each_type() {
        assert_eq!(
            detect(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Some(Sniffed::Jpeg)
        );
        assert_eq!(detect(b"\x89PNG\r\n\x1a\n\0\0\0\0\0\0"), Some(Sniffed::Png));
        assert_eq!(detect(b"GIF89a\0\0\0\0\0\0"), Some(Sniffed::Gif));
        assert_eq!(detect(b"%PDF-1.7\n\0\0\0\0"), Some(Sniffed::Pdf));
        assert_eq!(detect(b"RIFF\0\0\0\0WEBPVP8 "), Some(Sniffed::Webp));
        assert_eq!(detect(b"\0\0\0\x18ftypheic\0\0\0\0"), Some(Sniffed::Heic));
    }

    #[test]
    fn rejects_programs_and_short_input() {
        assert_eq!(detect(b"MZ\x90\0\x03\0\0\0\x04\0\0\0\xFF\xFF\0\0"), None);
        assert_eq!(detect(b""), None);
        assert_eq!(detect(b"%PDF"), None);
        assert!(Sniffed::Png.is_image() && !Sniffed::Pdf.is_image());
    }
}
