//! What image bytes are, read from the bytes themselves: the format
//! their magic bytes declare.
//!
//! No image crate is used and nothing is decoded. A declared type (a
//! file extension, a `Content-Type`, a `data:` URI prefix) is never
//! trusted where these can answer instead.

/// Sniff a provider-supported image MIME from the leading bytes.
/// `None` for anything else (callers reject it rather than guess).
#[must_use]
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The file extension an image of `mime` is saved under, for the
/// formats [`sniff_mime`] knows.
#[must_use]
pub fn extension_for(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

/// `n` bytes for display: `512 B`, `12 KB`, `1.5 MB`.
#[must_use]
pub fn human_bytes(n: usize) -> String {
    if n >= 1024 * 1024 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a display size needs no exact bytes"
        )]
        let mb = n as f64 / (1024.0 * 1024.0);
        format!("{mb:.1} MB")
    } else if n >= 1024 {
        format!("{} KB", n / 1024)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_known_formats_and_rejects_others() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00];
        assert_eq!(sniff_mime(&png), Some("image/png"));
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0x00]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a....."), Some("image/gif"));
        let webp = b"RIFF\0\0\0\0WEBPVP8 ";
        assert_eq!(sniff_mime(webp), Some("image/webp"));
        assert_eq!(sniff_mime(b"not an image"), None);
        assert_eq!(sniff_mime(b""), None);
    }

    #[test]
    fn every_sniffed_type_has_an_extension() {
        for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
            assert!(extension_for(mime).is_some(), "{mime}");
        }
        assert_eq!(extension_for("image/jpeg"), Some("jpg"));
        assert_eq!(extension_for("text/plain"), None);
    }

    #[test]
    fn byte_counts_read_in_the_largest_whole_unit() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(12 * 1024 + 300), "12 KB");
        assert_eq!(human_bytes(1536 * 1024), "1.5 MB");
    }
}
