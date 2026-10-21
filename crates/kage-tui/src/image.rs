//! Turn image bytes (a file, a drag-drop path, or the OS clipboard)
//! into a [`kage_core::Content::Image`]-ready attachment.
//!
//! The model/provider/session layers already handle image content;
//! this module is just the acquisition+encoding step the TUI input
//! needs. It deliberately uses no image crate: the format is sniffed
//! from magic bytes (PNG/JPEG/GIF/WebP - what the providers accept)
//! and the payload is base64 of the original bytes, unmodified.

use std::path::Path;

use base64::Engine as _;
use kage_core::MediaSource;

/// Largest image accepted, in bytes. Beyond this the base64 payload
/// bloats the request and providers reject it anyway; refuse early
/// with a clear message instead of failing mid-turn.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// One image queued on the prompt, ready to become a
/// [`kage_core::Content::Image`] block on submit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachedImage {
    /// Inline base64 source handed to the provider.
    pub source: MediaSource,
    /// Sniffed MIME type (e.g. `image/png`).
    pub mime: String,
    /// Short human label for the input/conversation placeholder
    /// (a filename, or `clipboard`).
    pub label: String,
    /// Decoded byte length, shown in the placeholder.
    pub bytes: usize,
}

impl AttachedImage {
    /// `[image: image/png, 42 KB (shot.png)]`-style one-liner shown
    /// in the conversation so an attachment is never silent.
    #[must_use]
    pub fn placeholder(&self) -> String {
        format!(
            "[image: {}, {} ({})]",
            self.mime,
            human_bytes(self.bytes),
            self.label
        )
    }

    /// Compact `label mime size` for the inline prompt marker, e.g.
    /// `shot.png image/png 42 KB`. No brackets - the caller wraps it
    /// in the `[image #N ...]` marker.
    #[must_use]
    pub fn summary(&self) -> String {
        format!("{} {} {}", self.label, self.mime, human_bytes(self.bytes))
    }
}

pub use kage_core::image::sniff_mime;

/// Build an attachment from raw image bytes (clipboard or file).
///
/// # Errors
///
/// Returns a user-facing message when the data is empty, larger than
/// [`MAX_IMAGE_BYTES`], or not a recognised image format.
pub fn from_bytes(bytes: &[u8], label: impl Into<String>) -> Result<AttachedImage, String> {
    if bytes.is_empty() {
        return Err("image is empty".to_owned());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image is {} (max {})",
            human_bytes(bytes.len()),
            human_bytes(MAX_IMAGE_BYTES)
        ));
    }
    let mime = sniff_mime(bytes)
        .ok_or_else(|| "unrecognised image format (need png/jpeg/gif/webp)".to_owned())?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(AttachedImage {
        source: MediaSource::Base64 { data },
        mime: mime.to_owned(),
        label: label.into(),
        bytes: bytes.len(),
    })
}

/// Read an image file and build an attachment, labelled by file name.
///
/// # Errors
///
/// Returns a user-facing message when the file cannot be read or
/// [`from_bytes`] rejects its contents. A file that is not an image
/// is pointed at `@` mentions, which include text files.
pub fn load_path(path: &Path) -> Result<AttachedImage, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let label = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("image")
        .to_owned();
    if !bytes.is_empty() && sniff_mime(&bytes).is_none() {
        return Err(format!(
            "{} is not a png, jpeg, gif or webp image. To include a text file, \
             mention it as @{} in the prompt.",
            path.display(),
            path.display()
        ));
    }
    from_bytes(&bytes, label)
}

/// Whether `text` is a plausible single filesystem path to an
/// existing image (used to treat a pasted/dragged path as an image
/// rather than literal prompt text). Tolerates surrounding quotes,
/// a leading `file://` URL with percent-encoded names (the form a
/// browser drag hands over, including the Windows `file:///C:/...`
/// shape), and backslash-escaped spaces from drag-drop.
#[must_use]
pub fn path_if_image(text: &str) -> Option<std::path::PathBuf> {
    let t = kage_core::fsutil::unquote_and_trim(text);
    if t.is_empty() || t.contains('\n') {
        return None;
    }
    if let Some(rest) = t.strip_prefix("file://") {
        let decoded = percent_decode(rest)?;
        let path = std::path::PathBuf::from(without_slash_before_drive(&decoded));
        return path_if_image_file(&path).then_some(path);
    }
    let direct = kage_core::fsutil::expand_tilde(std::path::Path::new(t));
    if path_if_image_file(&direct) {
        return Some(direct);
    }
    if t.contains("\\ ") {
        let unescaped =
            kage_core::fsutil::expand_tilde(std::path::Path::new(&t.replace("\\ ", " ")));
        if path_if_image_file(&unescaped) {
            return Some(unescaped);
        }
    }
    None
}

/// Whether `path` names an existing file with an image extension.
fn path_if_image_file(path: &Path) -> bool {
    let image_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|e| matches!(e.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp"));
    image_ext && path.is_file()
}

/// A `file:///C:/x.png` URL decodes to `/C:/x.png`; the slash before
/// a drive prefix is not part of the Windows path.
fn without_slash_before_drive(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        path[1..].to_owned()
    } else {
        path.to_owned()
    }
}

/// Decodes the %-escapes of a `file://` URL path. `None` on a
/// truncated or malformed escape, or a path that is not UTF-8 after
/// decoding.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 3 > bytes.len() {
            return None;
        }
        let high = hex_digit(bytes[index + 1])?;
        let low = hex_digit(bytes[index + 2])?;
        out.push(high.wrapping_shl(4) | low);
        index += 3;
    }
    String::from_utf8(out).ok()
}

/// One hexadecimal digit of a %-escape.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Read an image off the OS clipboard and return it PNG-encoded.
///
/// Uses the `arboard` crate (Wayland data-control, X11, macOS,
/// Windows) - reading the clipboard's native image, not shelling out
/// to a tool that may be absent. The clipboard yields raw RGBA, which
/// is re-encoded to PNG so the rest of the pipeline (sniff, base64,
/// provider) is format-uniform.
///
/// # Errors
///
/// A precise, user-facing message for the actual cause: clipboard
/// unavailable (headless / no compositor support), no image on the
/// clipboard (text was copied, or wl-clipboard's data-control
/// protocol is unsupported by the compositor), or a PNG-encode
/// failure. The caller surfaces this so a non-working setup is
/// diagnosable instead of silent.
pub fn clipboard_image() -> Result<Vec<u8>, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
    let img = clipboard
        .get_image()
        .map_err(|e| format!("no image on the clipboard ({e})"))?;
    clipboard_png(img.width, img.height, img.bytes.into_owned())
}

/// Everything [`clipboard_image`] does once the bytes are in hand:
/// validate the dimensions and pixel buffer, then PNG-encode. Free of
/// clipboard access so its error branches are unit-testable.
fn clipboard_png(width: usize, height: usize, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    let width = u32::try_from(width).map_err(|_| "clipboard image too wide".to_owned())?;
    let height = u32::try_from(height).map_err(|_| "clipboard image too tall".to_owned())?;
    let needed = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|len| usize::try_from(len).ok());
    if needed != Some(bytes.len()) {
        return Err("clipboard image had a malformed pixel buffer".to_owned());
    }
    let rgba = image::RgbaImage::from_raw(width, height, bytes)
        .ok_or_else(|| "clipboard image had a malformed pixel buffer".to_owned())?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("encode clipboard image as png: {e}"))?;
    Ok(png)
}

pub(crate) use kage_core::image::human_bytes;

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];

    #[test]
    fn from_bytes_encodes_base64_and_sets_mime() {
        let att = from_bytes(PNG, "shot.png").unwrap();
        assert_eq!(att.mime, "image/png");
        assert_eq!(att.label, "shot.png");
        assert_eq!(att.bytes, PNG.len());
        match &att.source {
            MediaSource::Base64 { data } => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap();
                assert_eq!(decoded, PNG);
            }
            MediaSource::Url { .. } | MediaSource::Blob { .. } => panic!("expected base64"),
        }
        assert!(att.placeholder().contains("image/png"));
        assert!(att.placeholder().contains("shot.png"));
    }

    #[test]
    fn a_text_file_points_at_an_at_mention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("README.md");
        std::fs::write(&path, "# readme").unwrap();
        let err = load_path(&path).unwrap_err();
        assert!(
            err.contains("is not a png, jpeg, gif or webp image"),
            "{err}"
        );
        assert!(err.contains("README.md in the prompt"), "{err}");
    }

    #[test]
    fn from_bytes_rejects_empty_oversize_and_unknown() {
        assert!(from_bytes(&[], "x").is_err());
        assert!(
            from_bytes(b"plain text data", "x")
                .unwrap_err()
                .contains("unrecognised")
        );
        let big = vec![0u8; MAX_IMAGE_BYTES + 1];
        assert!(from_bytes(&big, "x").unwrap_err().contains("max"));
    }

    #[test]
    fn load_path_round_trips_a_real_file() {
        let dir = std::env::temp_dir().join(format!("kage-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("pic.png");
        std::fs::write(&p, PNG).unwrap();
        let att = load_path(&p).unwrap();
        assert_eq!(att.mime, "image/png");
        assert_eq!(att.label, "pic.png");
        assert_eq!(path_if_image(p.to_str().unwrap()), Some(p.clone()));
        assert_eq!(
            path_if_image(&format!("\"{}\"", p.display())),
            Some(p.clone())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // `clipboard_image()` itself is not unit-tested: acquiring the
    // live OS clipboard via `arboard` has no deterministic behavior
    // in a headless/CI process. Everything after acquisition lives
    // in `clipboard_png`, whose dimension checks, malformed-buffer
    // and encoding error branches are covered below; failures still
    // surface to the user inline via the `:attach` / Ctrl+V path.

    #[test]
    fn clipboard_png_rejects_dimensions_past_u32() {
        assert_eq!(
            clipboard_png(usize::MAX, 1, vec![0; 4]),
            Err("clipboard image too wide".to_owned())
        );
        assert_eq!(
            clipboard_png(1, usize::MAX, vec![0; 4]),
            Err("clipboard image too tall".to_owned())
        );
    }

    #[test]
    fn clipboard_png_rejects_a_malformed_pixel_buffer() {
        let short = Err("clipboard image had a malformed pixel buffer".to_owned());
        assert_eq!(clipboard_png(2, 2, vec![0; 15]), short);
        assert_eq!(clipboard_png(2, 2, vec![0; 17]), short);
    }

    #[test]
    fn clipboard_png_encodes_rgba_pixels_as_png() {
        let png = clipboard_png(1, 1, vec![0xFF, 0x00, 0x00, 0xFF]).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
    }

    #[test]
    fn clipboard_png_maps_an_unencodable_image_to_an_error() {
        // A 0x0 buffer is structurally valid but has no PNG form.
        let err = clipboard_png(0, 0, Vec::new()).unwrap_err();
        assert!(err.starts_with("encode clipboard image as png"), "{err}");
    }

    #[test]
    fn path_if_image_rejects_non_images_and_text() {
        assert_eq!(path_if_image("just a sentence"), None);
        assert_eq!(path_if_image("/nope/missing.png"), None);
        assert_eq!(path_if_image("line one\nline two"), None);
    }

    #[test]
    fn a_percent_encoded_file_url_attaches() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a b.png");
        std::fs::write(&file, PNG).unwrap();
        let url = format!("file://{}/a%20b.png", dir.path().display());
        assert_eq!(path_if_image(&url), Some(file));
    }

    #[test]
    fn a_quoted_file_url_attaches() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a b.png");
        std::fs::write(&file, PNG).unwrap();
        let url = format!("\"file://{}/a%20b.png\"", dir.path().display());
        assert_eq!(path_if_image(&url), Some(file));
    }

    #[test]
    fn a_quoted_and_spaced_unicode_name_attaches() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("caf\u{e9}.png");
        std::fs::write(&file, PNG).unwrap();
        let input = format!(" \"{}\" ", file.display());
        assert_eq!(path_if_image(&input), Some(file));
    }

    #[test]
    fn a_backslash_escaped_space_path_attaches_when_the_unescaped_name_exists() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a b.png");
        std::fs::write(&file, PNG).unwrap();
        let escaped = file.to_str().unwrap().replace(' ', "\\ ");
        assert_eq!(path_if_image(&escaped), Some(file));
    }

    #[cfg(unix)]
    #[test]
    fn a_literal_backslash_space_name_survives_unescaped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a\\ b.png");
        std::fs::write(&file, PNG).unwrap();
        assert_eq!(path_if_image(file.to_str().unwrap()), Some(file));
    }

    #[test]
    fn the_slash_before_a_windows_drive_prefix_is_dropped() {
        assert_eq!(without_slash_before_drive("/C:/x.png"), "C:/x.png");
        assert_eq!(without_slash_before_drive("/home/u/x.png"), "/home/u/x.png");
        assert_eq!(without_slash_before_drive("/x.png"), "/x.png");
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_drive_file_url_attaches() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("w.png");
        std::fs::write(&file, PNG).unwrap();
        let url = format!("file:///{}", file.display().to_string().replace('\\', "/"));
        assert_eq!(path_if_image(&url), Some(file));
    }

    #[test]
    fn a_malformed_file_url_is_not_a_path() {
        assert_eq!(path_if_image("file:///tmp/%ZZ.png"), None);
        assert_eq!(path_if_image("file:///tmp/%2"), None);
    }
}
