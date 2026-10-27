//! Hand-built `multipart/form-data` bodies.
//!
//! ureq's multipart feature would pull in `mime_guess`, so the parts an
//! image edit sends are assembled here instead: a boundary of 128
//! random bits, text and file parts with CRLF line ends, and a closing
//! boundary. The body is not scanned for the boundary; the chance of
//! the uploaded bytes drawing it is negligible.

use std::time::{SystemTime, UNIX_EPOCH};

/// A `multipart/form-data` body under construction.
pub(crate) struct Form {
    boundary: String,
    body: Vec<u8>,
}

impl Form {
    /// A form whose boundary is `kage-` and 32 lowercase hex digits
    /// drawn from 16 random bytes. When the system randomness fails,
    /// the digits come from the clock's nanoseconds and the process id
    /// instead.
    pub(crate) fn new() -> Self {
        Self {
            boundary: format!("kage-{:032x}", u128::from_be_bytes(boundary_bytes())),
            body: Vec::new(),
        }
    }

    /// Append a text part.
    pub(crate) fn text(&mut self, name: &str, value: &str) {
        self.part(name, None, None, value.as_bytes());
    }

    /// Append a file part, naming its file name and content type.
    pub(crate) fn file(&mut self, name: &str, filename: &str, mime: &str, bytes: &[u8]) {
        self.part(name, Some(filename), Some(mime), bytes);
    }

    /// Append one part: the opening boundary, the disposition (plus a
    /// content type for a file), a blank line, the value, and a line
    /// end.
    fn part(&mut self, name: &str, filename: Option<&str>, mime: Option<&str>, value: &[u8]) {
        let mut head = match filename {
            Some(filename) => format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{name}\"; \
                 filename=\"{filename}\"",
                self.boundary
            ),
            None => format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{name}\"",
                self.boundary
            ),
        };
        if let Some(mime) = mime {
            head.push_str("\r\nContent-Type: ");
            head.push_str(mime);
        }
        head.push_str("\r\n\r\n");
        self.body.extend_from_slice(head.as_bytes());
        self.body.extend_from_slice(value);
        self.body.extend_from_slice(b"\r\n");
    }

    /// Close the body and return the content type header value, which
    /// names the boundary, and the body itself.
    pub(crate) fn finish(mut self) -> (String, Vec<u8>) {
        self.body
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        (
            format!("multipart/form-data; boundary={}", self.boundary),
            self.body,
        )
    }
}

/// 16 bytes for a boundary: random, or the clock's nanoseconds and the
/// process id when the random source fails.
fn boundary_bytes() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_ok() {
        return bytes;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::from(since.subsec_nanos()));
    bytes[..8].copy_from_slice(&nanos.to_be_bytes());
    bytes[12..].copy_from_slice(&std::process::id().to_be_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_file_parts_are_the_expected_bytes() {
        let mut form = Form::new();
        form.text("model", "image:max");
        form.file("image", "image.png", "image/png", b"bytes");
        let (content_type, body) = form.finish();
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        assert!(boundary.starts_with("kage-"), "{boundary}");
        assert_eq!(boundary[5..].len(), 32);
        assert!(
            boundary[5..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{boundary}"
        );
        let expected = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"model\"\r\n\r\n\
             image:max\r\n\
             --{boundary}\r\n\
             Content-Disposition: form-data; name=\"image\"; filename=\"image.png\"\r\n\
             Content-Type: image/png\r\n\r\n\
             bytes\r\n\
             --{boundary}--\r\n"
        );
        assert_eq!(body, expected.as_bytes());
    }

    #[test]
    fn file_bytes_with_a_boundary_start_and_nuls_survive() {
        let file: &[u8] = b"a\r\n--\0\0b";
        let mut form = Form::new();
        form.file("image", "image.png", "image/png", file);
        let (content_type, body) = form.finish();
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        let expected = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"image\"; filename=\"image.png\"\r\n\
             Content-Type: image/png\r\n\r\n\
             a\r\n--\0\0b\r\n\
             --{boundary}--\r\n"
        );
        assert_eq!(body, expected.as_bytes());
    }

    #[test]
    fn two_forms_draw_different_boundaries() {
        let (first, _) = Form::new().finish();
        let (second, _) = Form::new().finish();
        assert_ne!(first, second);
    }
}
