//! What audio bytes are, read from the bytes themselves: the format
//! their magic bytes declare.
//!
//! No audio crate is used and nothing is decoded. A declared type (a
//! file extension, a `Content-Type`) is never trusted where these can
//! answer instead.

/// Sniff a provider-supported audio MIME from the leading bytes.
/// `None` for anything else (callers reject it rather than guess).
#[must_use]
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"ID3") || is_mpeg_frame(bytes) {
        Some("audio/mpeg")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        Some("audio/wav")
    } else {
        None
    }
}

/// The file extension an audio clip of `mime` is saved under, for the
/// formats [`sniff_mime`] knows.
#[must_use]
pub fn extension_for(mime: &str) -> Option<&'static str> {
    match mime {
        "audio/mpeg" => Some("mp3"),
        "audio/wav" => Some("wav"),
        _ => None,
    }
}

/// Whether the bytes begin with an MPEG audio frame header. It needs
/// three bytes: the sync, a version and layer byte, and a bitrate and
/// sample-rate index byte. The reserved version and layer values, the
/// invalid bitrate index `1111` and the reserved sample rate `11` are
/// kept out, which leaves ADTS AAC (`FF F1`) and JPEG (`FF D8`)
/// behind.
fn is_mpeg_frame(bytes: &[u8]) -> bool {
    if bytes.len() < 3 {
        return false;
    }
    let (b0, b1, b2) = (bytes[0], bytes[1], bytes[2]);
    b0 == 0xFF
        && b1 & 0xE0 == 0xE0
        && (b1 >> 3) & 0b11 != 0b01
        && (b1 >> 1) & 0b11 != 0b00
        && b2 >> 4 != 0b1111
        && (b2 >> 2) & 0b11 != 0b11
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_mp3_from_an_id3_tag_and_a_frame_header() {
        assert_eq!(
            sniff_mime(b"ID3\x04\x00\x00\x00\x00\x00\x00"),
            Some("audio/mpeg")
        );
        assert_eq!(sniff_mime(&[0xFF, 0xFB, 0x90, 0x64]), Some("audio/mpeg"));
    }

    #[test]
    fn rejects_headers_that_are_not_mpeg_audio() {
        assert_eq!(sniff_mime(&[0xFF, 0xF1, 0x50, 0x80]), None);
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), None);
        assert_eq!(sniff_mime(&[0xFF, 0xEB, 0x90, 0x64]), None);
        assert_eq!(sniff_mime(&[0xFF, 0xFB, 0xF0, 0x64]), None);
        assert_eq!(sniff_mime(&[0xFF, 0xFB, 0x9C, 0x64]), None);
        assert_eq!(sniff_mime(&[0xFF, 0xFB]), None);
    }

    #[test]
    fn sniffs_wav_and_leaves_webp_an_image() {
        let wav = b"RIFF\0\0\0\0WAVEfmt \x10\x00\x00\x00";
        assert_eq!(sniff_mime(wav), Some("audio/wav"));
        let webp = b"RIFF\0\0\0\0WEBPVP8 ";
        assert_eq!(sniff_mime(webp), None);
        assert_eq!(crate::image::sniff_mime(webp), Some("image/webp"));
    }

    #[test]
    fn text_and_empty_bytes_are_nothing() {
        assert_eq!(sniff_mime(b"plain text"), None);
        assert_eq!(sniff_mime(b""), None);
    }

    #[test]
    fn every_sniffed_type_has_an_extension() {
        let samples: [(&[u8], &str); 2] = [
            (b"ID3\x04\x00\x00\x00\x00\x00\x00", "mp3"),
            (b"RIFF\0\0\0\0WAVEfmt \x10\x00\x00\x00", "wav"),
        ];
        for (bytes, extension) in samples {
            let mime = sniff_mime(bytes).unwrap();
            assert_eq!(extension_for(mime), Some(extension), "{mime}");
        }
        assert_eq!(extension_for("text/plain"), None);
    }
}
