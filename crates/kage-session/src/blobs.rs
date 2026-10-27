//! The blob store: media bytes kept beside the session files instead of
//! inline in them.
//!
//! A session file names a blob by the sha256 of its bytes
//! ([`MediaSource::Blob`]). The bytes live at
//! `<sessions>/blobs/sha256/<first two hex digits>/<hash>`, shared by
//! every session in the directory, so a fork or the same media sent
//! twice costs nothing more. Blobs are private to the user, like the
//! session files.
//!
//! [`crate::SessionWriter::append`] moves inline media of at least
//! [`INLINE_MAX`] bytes here and writes the reference, and
//! [`crate::SessionReader`] puts the bytes back, so a message in memory
//! always holds its media. A blob that has gone missing stays a
//! reference, which every consumer shows as unavailable media.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use kage_core::{Content, MediaSource, Message};
use sha2::{Digest as _, Sha256};

/// The largest media kept inline in a session file, in decoded bytes.
pub const INLINE_MAX: usize = 16 * 1024;

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// Replaces the media source of `block` in place when it is an image
/// or an audio clip, keeping its kind and mime. Images and clips move
/// through the blob store the same way, so externalize and hydrate
/// rebuild blocks through this one helper.
fn set_source(block: &mut Content, source: MediaSource) {
    if let Content::Image { source: slot, .. } | Content::Audio { source: slot, .. } = block {
        *slot = source;
    }
}

/// The blobs of one sessions directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// The store of the directory that holds `session`.
    #[must_use]
    pub fn beside(session: &Path) -> Self {
        let dir = session.parent().unwrap_or(Path::new("."));
        Self {
            root: dir.join("blobs").join("sha256"),
        }
    }

    /// Where the blob `sha256` lives, or `None` for a name that is not a
    /// sha256.
    #[must_use]
    pub fn path(&self, sha256: &str) -> Option<PathBuf> {
        let valid = sha256.len() == 64
            && sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        valid.then(|| self.root.join(&sha256[..2]).join(sha256))
    }

    /// The sha256 of `bytes`, lowercase hex: the name they are stored
    /// under.
    #[must_use]
    pub fn digest(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// Stores `bytes` and returns their sha256. Bytes already stored are
    /// not written again.
    ///
    /// # Errors
    ///
    /// The blob cannot be written.
    pub fn put(&self, bytes: &[u8]) -> std::io::Result<String> {
        let sha256 = Self::digest(bytes);
        let path = self.path(&sha256).expect("a digest is a valid name");
        if !path.is_file() {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            kage_core::fsutil::atomic_write_private(&path, bytes)?;
        }
        Ok(sha256)
    }

    /// The bytes of blob `sha256`, when it is stored and intact.
    #[must_use]
    pub fn get(&self, sha256: &str) -> Option<Vec<u8>> {
        let bytes = std::fs::read(self.path(sha256)?).ok()?;
        (Self::digest(&bytes) == sha256).then_some(bytes)
    }

    /// `message` with every inline media of at least [`INLINE_MAX`] bytes
    /// moved into the store, or `None` when nothing moved. Media that is
    /// not valid base64 stays inline.
    ///
    /// # Errors
    ///
    /// A blob cannot be written.
    pub fn externalize(&self, message: &Message) -> std::io::Result<Option<Message>> {
        let mut out: Option<Message> = None;
        for (i, block) in message.content.iter().enumerate() {
            let (Content::Image {
                source: MediaSource::Base64 { data },
                ..
            }
            | Content::Audio {
                source: MediaSource::Base64 { data },
                ..
            }) = block
            else {
                continue;
            };
            if data.len() / 4 * 3 < INLINE_MAX {
                continue;
            }
            let Ok(bytes) = BASE64.decode(data) else {
                continue;
            };
            let sha256 = self.put(&bytes)?;
            let slot = &mut out.get_or_insert_with(|| message.clone()).content[i];
            set_source(
                slot,
                MediaSource::Blob {
                    sha256,
                    size: bytes.len() as u64,
                },
            );
        }
        Ok(out)
    }

    /// `message` with every blob reference the store still holds turned
    /// back into inline bytes, or `None` when there was none to restore.
    #[must_use]
    pub fn hydrate(&self, message: &Message) -> Option<Message> {
        let mut out: Option<Message> = None;
        for (i, block) in message.content.iter().enumerate() {
            let (Content::Image {
                source: MediaSource::Blob { sha256, .. },
                ..
            }
            | Content::Audio {
                source: MediaSource::Blob { sha256, .. },
                ..
            }) = block
            else {
                continue;
            };
            let Some(bytes) = self.get(sha256) else {
                continue;
            };
            let slot = &mut out.get_or_insert_with(|| message.clone()).content[i];
            set_source(
                slot,
                MediaSource::Base64 {
                    data: BASE64.encode(bytes),
                },
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use kage_core::Role;
    use tempfile::tempdir;

    use super::*;
    use crate::entry::{EntryId, FORMAT_VERSION, Header, MessageEntry, SessionEntry, SessionId};
    use crate::{SessionReader, SessionWriter};

    fn header() -> Header {
        Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/tmp"),
            model: "anthropic:claude".into(),
            system_prompt: "sys".into(),
            parent_session: None,
            parent_entry: None,
        }
    }

    fn image(bytes: &[u8]) -> Content {
        Content::Image {
            source: MediaSource::Base64 {
                data: BASE64.encode(bytes),
            },
            mime: "image/png".into(),
        }
    }

    fn audio(bytes: &[u8]) -> Content {
        Content::Audio {
            source: MediaSource::Base64 {
                data: BASE64.encode(bytes),
            },
            mime: "audio/mpeg".into(),
        }
    }

    fn entry(content: Vec<Content>) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Arc::new(Message::new(Role::Assistant, content, None)),
            usage: None,
        })
    }

    fn messages(path: &Path) -> Vec<Arc<Message>> {
        SessionReader::iter(path)
            .unwrap()
            .filter_map(|entry| match entry.unwrap() {
                SessionEntry::Message(m) => Some(m.message),
                _ => None,
            })
            .collect()
    }

    fn blob_files(dir: &Path) -> usize {
        walk(&dir.join("blobs"))
    }

    fn walk(dir: &Path) -> usize {
        std::fs::read_dir(dir).map_or(0, |entries| {
            entries
                .flatten()
                .map(|e| {
                    if e.path().is_dir() {
                        walk(&e.path())
                    } else {
                        1
                    }
                })
                .sum()
        })
    }

    #[test]
    fn large_media_leave_the_session_file_and_come_back_on_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let big = vec![7u8; INLINE_MAX + 1];
        let small = vec![1u8; 64];
        let mut writer = SessionWriter::create(&path, header()).unwrap();
        let written = entry(vec![image(&big), image(&small)]);
        writer.append(&written).unwrap();
        writer.append(&entry(vec![image(&big)])).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains(&BASE64.encode(&big)),
            "big bytes left the file"
        );
        assert!(
            text.contains(&BASE64.encode(&small)),
            "small bytes stay inline"
        );
        assert!(text.contains("\"kind\":\"blob\""), "{text}");
        assert_eq!(blob_files(dir.path()), 1, "the same bytes are stored once");

        let read = messages(&path);
        let SessionEntry::Message(original) = written else {
            unreachable!()
        };
        assert_eq!(read[0].content, original.message.content);
        assert_eq!(read[1].content[0], image(&big));
    }

    #[test]
    fn audio_moves_to_a_blob_and_back_like_an_image() {
        let dir = tempdir().unwrap();
        let store = BlobStore::beside(&dir.path().join("s.jsonl"));
        let big = vec![5u8; INLINE_MAX + 1];
        let message = Message::new(Role::Assistant, vec![audio(&big)], None);

        let external = store.externalize(&message).unwrap().unwrap();
        let Content::Audio { source, mime } = &external.content[0] else {
            panic!("{external:?}")
        };
        let MediaSource::Blob { sha256, size } = source else {
            panic!("{source:?}")
        };
        assert_eq!(mime, "audio/mpeg");
        assert_eq!(*size, big.len() as u64);
        assert_eq!(store.get(sha256).unwrap(), big);

        assert_eq!(store.hydrate(&external).unwrap(), message);
    }

    #[test]
    fn a_snapshot_written_elsewhere_brings_its_blobs() {
        let from = tempdir().unwrap();
        let to = tempdir().unwrap();
        let src = from.path().join("s.jsonl");
        let big = vec![3u8; INLINE_MAX + 10];
        let mut writer = SessionWriter::create(&src, header()).unwrap();
        writer.append(&entry(vec![image(&big)])).unwrap();
        drop(writer);

        let snapshot = crate::snapshot(&src, |_| true).unwrap();
        assert_eq!(snapshot.blob_refs.len(), 1);
        let dst = to.path().join("copy.jsonl");
        drop(snapshot.write(&dst, header()).unwrap());
        assert_eq!(messages(&dst)[0].content[0], image(&big));
    }

    #[test]
    fn a_snapshot_written_elsewhere_brings_its_audio_blobs() {
        let from = tempdir().unwrap();
        let to = tempdir().unwrap();
        let src = from.path().join("s.jsonl");
        let big = vec![3u8; INLINE_MAX + 10];
        let mut writer = SessionWriter::create(&src, header()).unwrap();
        writer.append(&entry(vec![audio(&big)])).unwrap();
        drop(writer);

        let snapshot = crate::snapshot(&src, |_| true).unwrap();
        assert_eq!(snapshot.blob_refs.len(), 1);
        let dst = to.path().join("copy.jsonl");
        drop(snapshot.write(&dst, header()).unwrap());
        assert_eq!(messages(&dst)[0].content[0], audio(&big));
    }

    #[cfg(unix)]
    #[test]
    fn blobs_are_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempdir().unwrap();
        let store = BlobStore::beside(&dir.path().join("s.jsonl"));
        let sha256 = store.put(b"secret").unwrap();
        let mode = std::fs::metadata(store.path(&sha256).unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }

    #[test]
    fn a_missing_or_altered_blob_reads_as_unavailable() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let big = vec![9u8; INLINE_MAX * 2];
        let mut writer = SessionWriter::create(&path, header()).unwrap();
        writer.append(&entry(vec![image(&big)])).unwrap();
        let store = BlobStore::beside(&path);
        let sha256 = store.put(&big).unwrap();
        std::fs::write(store.path(&sha256).unwrap(), b"tampered").unwrap();

        let read = messages(&path);
        let Content::Image { source, mime } = &read[0].content[0] else {
            panic!("{read:?}")
        };
        assert_eq!(
            source,
            &MediaSource::Blob {
                sha256,
                size: big.len() as u64
            }
        );
        assert!(
            source
                .missing_note(mime)
                .unwrap()
                .contains("no longer stored")
        );
    }

    #[test]
    fn only_a_sha256_names_a_blob() {
        let store = BlobStore::beside(Path::new("/tmp/s.jsonl"));
        for bad in ["", "../x", &"A".repeat(64), &"0".repeat(63)] {
            assert!(store.path(bad).is_none(), "{bad}");
        }
        assert!(store.path(&"0".repeat(64)).is_some());
    }

    #[test]
    fn media_that_is_not_base64_stays_inline() {
        let dir = tempdir().unwrap();
        let store = BlobStore::beside(&dir.path().join("s.jsonl"));
        let message = Message::new(
            Role::User,
            vec![Content::Image {
                source: MediaSource::Base64 {
                    data: "!".repeat(INLINE_MAX * 2),
                },
                mime: "image/png".into(),
            }],
            None,
        );
        assert!(store.externalize(&message).unwrap().is_none());
    }
}
