//! `CommandKind::Image` and `CommandKind::Media`: open or save media
//! the model produced in a session, named by the sha256 of its bytes
//! or else the newest. `/image` sees images only; `/media` also sees
//! audio clips.
//!
//! The media is read back from the session file, which puts blob bytes
//! back inline, so this works during a run as well as between runs. A
//! save never replaces a file: a name already taken gets `-2`, `-3`,
//! and so on.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use kage_core::protocol::ImageAction;
use kage_core::{Content, MediaSource, Modalities, Modality, Role, SessionId};
use kage_session::{SessionEntry, SessionReader};

/// The most numbered names tried before a save gives up.
const MAX_SUFFIX: u32 = 1000;

impl super::Dispatcher {
    /// `/image`: open or save the newest image the model produced.
    pub(super) fn image(
        &self,
        id: SessionId,
        action: ImageAction,
        sha256: Option<&str>,
        dest: Option<&Path>,
    ) {
        self.open_media(id, action, sha256, dest, &[Modality::Image], "image");
    }

    /// `/media`: open or save the newest image or audio clip the model
    /// or one of its tools produced.
    pub(super) fn media(
        &self,
        id: SessionId,
        action: ImageAction,
        sha256: Option<&str>,
        dest: Option<&Path>,
    ) {
        self.open_media(
            id,
            action,
            sha256,
            dest,
            &[Modality::Image, Modality::Audio],
            "media",
        );
    }

    /// Shared body of [`Self::image`] and [`Self::media`]: find the
    /// newest block of one of `kinds`, or the one `sha256` names, and
    /// act on it. `noun` names the command in the messages.
    fn open_media(
        &self,
        id: SessionId,
        action: ImageAction,
        sha256: Option<&str>,
        dest: Option<&Path>,
        kinds: &[Modality],
        noun: &str,
    ) {
        let session = &self.sessions[&id];
        let Some(src) = session.path.as_deref() else {
            self.error(id, format!("{noun}: the session is not recorded"));
            return;
        };
        let Some((mime, bytes)) = produced_media(src, sha256, kinds) else {
            let missing = if sha256.is_some() {
                format!("{noun}: the session holds no such {noun}")
            } else {
                format!("{noun}: the model has produced no {noun} in this session")
            };
            self.info(id, missing);
            return;
        };
        let result = match action {
            ImageAction::Save => save(&session.workdir, dest, &mime, &bytes).map(|path| {
                if mime.starts_with("audio/") {
                    format!("saved the clip to {}", path.display())
                } else {
                    format!("saved the image to {}", path.display())
                }
            }),
            ImageAction::Open => {
                open(&mime, &bytes).map(|path| format!("opened {}", path.display()))
            }
        };
        match result {
            Ok(message) => self.info(id, message),
            Err(err) => self.error(id, format!("{noun}: {err}")),
        }
    }
}

/// The MIME type and bytes of a media block of one of `kinds` that
/// the model or one of its tools produced in the session at `path`:
/// the one whose bytes hash to `sha256`, or the last one.
fn produced_media(
    path: &Path,
    sha256: Option<&str>,
    kinds: &[Modality],
) -> Option<(String, Vec<u8>)> {
    let kinds = Modalities::of(kinds);
    let decode = |data: &str| base64::engine::general_purpose::STANDARD.decode(data).ok();
    let mut newest = None;
    for entry in SessionReader::iter(path).ok()?.flatten() {
        let SessionEntry::Message(message) = entry else {
            continue;
        };
        // The model's own media and its tools' count; attached ones
        // are the user's.
        if !matches!(message.message.role, Role::Assistant | Role::ToolResult) {
            continue;
        }
        for block in &message.message.content {
            let (mime, data) = match block {
                Content::Image {
                    source: MediaSource::Base64 { data },
                    mime,
                } if kinds.contains(Modality::Image) => (mime, data),
                Content::Audio {
                    source: MediaSource::Base64 { data },
                    mime,
                } if kinds.contains(Modality::Audio) => (mime, data),
                _ => continue,
            };
            match sha256 {
                Some(wanted) => {
                    if let Some(bytes) = decode(data)
                        && kage_session::BlobStore::digest(&bytes) == wanted
                    {
                        return Some((mime.clone(), bytes));
                    }
                }
                None => newest = Some((mime.clone(), data.clone())),
            }
        }
    }
    let (mime, data) = newest?;
    Some((mime, decode(&data)?))
}

/// Writes `bytes` to `dest` (relative to `workdir`) or to a new file
/// under `workdir/generated/`, taking a free name, and returns where.
pub(super) fn save(
    workdir: &Path,
    dest: Option<&Path>,
    mime: &str,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    let target = dest.map_or_else(
        || workdir.join(generated_name(mime)),
        |dest| workdir.join(dest),
    );
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let stem = target
        .file_stem()
        .map_or_else(|| "image".into(), |s| s.to_string_lossy().into_owned());
    let ext = target.extension().map_or_else(
        || extension(mime).to_owned(),
        |e| e.to_string_lossy().into_owned(),
    );
    let dir = target.parent().unwrap_or(workdir);
    for n in 1..=MAX_SUFFIX {
        let name = if n == 1 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem}-{n}.{ext}")
        };
        let path = dir.join(name);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        match file {
            Ok(mut file) => {
                std::io::Write::write_all(&mut file, bytes)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(format!("{}: {err}", path.display())),
        }
    }
    Err(format!(
        "{} and {MAX_SUFFIX} numbered names are taken",
        target.display()
    ))
}

/// Copies the image into the cache under a name with its extension, so
/// the viewer knows its type, and opens it there.
fn open(mime: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let dir = crate::cache_root()?.join("media");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = format!(
        "{}.{}",
        &kage_session::BlobStore::digest(bytes)[..16],
        extension(mime)
    );
    let path = dir.join(name);
    if !path.is_file() {
        kage_core::fsutil::atomic_write_private(&path, bytes)
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    crate::opener::open(path.as_os_str())?;
    Ok(path)
}

/// The workspace-relative name of a fresh file under `generated/`,
/// stamped with the current time, named for `mime`'s type.
pub(super) fn generated_name(mime: &str) -> String {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    format!("generated/{stamp}.{}", extension(mime))
}

/// The file extension for an image or audio MIME type.
fn extension(mime: &str) -> &str {
    if let Some(ext) = kage_core::audio::extension_for(mime) {
        return ext;
    }
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/svg+xml" => "svg",
        other => other
            .strip_prefix("image/")
            .filter(|ext| !ext.is_empty() && ext.bytes().all(|b| b.is_ascii_alphanumeric()))
            .unwrap_or("png"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use base64::Engine as _;
    use kage_core::{Content, MediaSource, Message, Modality, Role};
    use kage_session::{EntryId, FORMAT_VERSION, Header, MessageEntry, SessionEntry, SessionId};

    use super::{extension, generated_name, produced_media, save};

    fn entry(role: Role, block: Content) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: chrono::Utc::now(),
            message: Arc::new(Message::new(role, vec![block], None)),
            usage: None,
        })
    }

    fn image(role: Role, bytes: &[u8]) -> SessionEntry {
        entry(
            role,
            Content::Image {
                source: MediaSource::Base64 {
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                },
                mime: "image/png".into(),
            },
        )
    }

    fn clip(role: Role, bytes: &[u8]) -> SessionEntry {
        entry(
            role,
            Content::Audio {
                source: MediaSource::Base64 {
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                },
                mime: "audio/mpeg".into(),
            },
        )
    }

    fn session_header(cwd: &Path) -> Header {
        Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: chrono::Utc::now(),
            cwd: cwd.to_path_buf(),
            model: "m".into(),
            system_prompt: String::new(),
            parent_session: None,
            parent_entry: None,
        }
    }

    #[test]
    fn the_image_is_found_by_its_hash_or_as_the_newest_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut writer =
            kage_session::SessionWriter::create(&path, session_header(dir.path())).unwrap();
        writer.append(&image(Role::Assistant, b"first")).unwrap();
        writer.append(&image(Role::Assistant, b"second")).unwrap();
        writer.append(&image(Role::ToolResult, b"tooled")).unwrap();
        writer.append(&image(Role::User, b"attached")).unwrap();
        drop(writer);

        assert_eq!(
            produced_media(&path, None, &[Modality::Image]).unwrap().1,
            b"tooled",
            "a tool's image counts"
        );
        let first = kage_session::BlobStore::digest(b"first");
        assert_eq!(
            produced_media(&path, Some(&first), &[Modality::Image])
                .unwrap()
                .1,
            b"first"
        );
        let attached = kage_session::BlobStore::digest(b"attached");
        assert!(
            produced_media(&path, Some(&attached), &[Modality::Image]).is_none(),
            "only the model's own"
        );
    }

    #[test]
    fn media_spans_images_and_clips_by_newest_or_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut writer =
            kage_session::SessionWriter::create(&path, session_header(dir.path())).unwrap();
        writer.append(&image(Role::Assistant, b"\x89PNG")).unwrap();
        writer.append(&clip(Role::ToolResult, b"ID3 clip")).unwrap();
        drop(writer);

        let both = [Modality::Image, Modality::Audio];
        let found = produced_media(&path, None, &both).unwrap();
        assert_eq!(
            (found.0.as_str(), found.1.as_slice()),
            ("audio/mpeg", b"ID3 clip".as_slice()),
            "the clip is newer"
        );
        assert_eq!(
            produced_media(&path, None, &[Modality::Image]).unwrap().1,
            b"\x89PNG",
            "an image-only query ignores the clip"
        );
        let clip_hash = kage_session::BlobStore::digest(b"ID3 clip");
        assert_eq!(
            produced_media(&path, Some(&clip_hash), &both).unwrap().1,
            b"ID3 clip",
            "the hash names the clip over the newer rule"
        );
    }

    #[test]
    fn media_takes_the_newest_allowed_block_and_skips_user_clips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut writer =
            kage_session::SessionWriter::create(&path, session_header(dir.path())).unwrap();
        writer.append(&clip(Role::Assistant, b"ID3 old")).unwrap();
        writer.append(&clip(Role::User, b"attached clip")).unwrap();
        writer
            .append(&image(Role::ToolResult, b"\x89PNG new"))
            .unwrap();
        drop(writer);

        let found = produced_media(&path, None, &[Modality::Image, Modality::Audio]).unwrap();
        assert_eq!(
            (found.0.as_str(), found.1.as_slice()),
            ("image/png", b"\x89PNG new".as_slice()),
            "the newest allowed block wins"
        );
        let attached = kage_session::BlobStore::digest(b"attached clip");
        assert!(
            produced_media(&path, Some(&attached), &[Modality::Image, Modality::Audio]).is_none(),
            "a user's clip is never produced"
        );
    }

    #[test]
    fn a_clip_saves_under_generated_with_the_mp3_extension() {
        let dir = tempfile::tempdir().unwrap();
        let saved = save(dir.path(), None, "audio/mpeg", b"ID3 clip").unwrap();
        assert!(saved.starts_with(dir.path().join("generated")), "{saved:?}");
        assert_eq!(saved.extension().unwrap(), "mp3");
        assert_eq!(std::fs::read(&saved).unwrap(), b"ID3 clip");
    }

    #[test]
    fn a_save_never_replaces_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = save(
            dir.path(),
            Some("art/cat.png".as_ref()),
            "image/png",
            b"one",
        )
        .unwrap();
        let second = save(
            dir.path(),
            Some("art/cat.png".as_ref()),
            "image/png",
            b"two",
        )
        .unwrap();
        assert_eq!(first, dir.path().join("art/cat.png"));
        assert_eq!(second, dir.path().join("art/cat-2.png"));
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        let fresh = save(dir.path(), None, "image/webp", b"x").unwrap();
        assert!(fresh.starts_with(dir.path().join("generated")), "{fresh:?}");
        assert_eq!(fresh.extension().unwrap(), "webp");
    }

    #[test]
    fn extensions_follow_the_type() {
        assert_eq!(extension("image/png"), "png");
        assert_eq!(extension("image/jpeg"), "jpg");
        assert_eq!(extension("image/svg+xml"), "svg");
        assert_eq!(extension("image/../x"), "png");
        assert_eq!(extension("audio/mpeg"), "mp3");
        assert_eq!(extension("audio/wav"), "wav");
    }

    #[test]
    fn generated_name_lands_under_generated_named_for_the_type() {
        let name = generated_name("audio/mpeg");
        assert!(name.starts_with("generated/"), "{name}");
        assert_eq!(
            std::path::Path::new(&name)
                .extension()
                .and_then(|ext| ext.to_str()),
            Some("mp3")
        );
    }
}
