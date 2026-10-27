//! `CommandKind::Image`: open or save an image the model produced in a
//! session, named by the sha256 of its bytes or else the newest.
//!
//! The image is read back from the session file, which puts blob bytes
//! back inline, so this works during a run as well as between runs. A
//! save never replaces a file: a name already taken gets `-2`, `-3`,
//! and so on.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use kage_core::protocol::ImageAction;
use kage_core::{Content, MediaSource, Role, SessionId};
use kage_session::{SessionEntry, SessionReader};

/// The most numbered names tried before a save gives up.
const MAX_SUFFIX: u32 = 1000;

impl super::Dispatcher {
    pub(super) fn image(
        &self,
        id: SessionId,
        action: ImageAction,
        sha256: Option<&str>,
        dest: Option<&Path>,
    ) {
        let session = &self.sessions[&id];
        let Some(src) = session.path.as_deref() else {
            self.error(id, "image: the session is not recorded".to_owned());
            return;
        };
        let Some((mime, bytes)) = produced_image(src, sha256) else {
            let missing = if sha256.is_some() {
                "image: the session holds no such image"
            } else {
                "image: the model has produced no image in this session"
            };
            self.info(id, missing.to_owned());
            return;
        };
        let result = match action {
            ImageAction::Save => save(&session.workdir, dest, &mime, &bytes)
                .map(|path| format!("saved the image to {}", path.display())),
            ImageAction::Open => {
                open(&mime, &bytes).map(|path| format!("opened {}", path.display()))
            }
        };
        match result {
            Ok(message) => self.info(id, message),
            Err(err) => self.error(id, format!("image: {err}")),
        }
    }
}

/// The MIME type and bytes of an image the model or one of its tools
/// produced in the session at `path`: the one whose bytes hash to
/// `sha256`, or the last one.
fn produced_image(path: &Path, sha256: Option<&str>) -> Option<(String, Vec<u8>)> {
    let decode = |data: &str| base64::engine::general_purpose::STANDARD.decode(data).ok();
    let mut newest = None;
    for entry in SessionReader::iter(path).ok()?.flatten() {
        let SessionEntry::Message(message) = entry else {
            continue;
        };
        // The model's own images and its tools' count; attached ones
        // are the user's.
        if !matches!(message.message.role, Role::Assistant | Role::ToolResult) {
            continue;
        }
        for block in &message.message.content {
            let Content::Image {
                source: MediaSource::Base64 { data },
                mime,
            } = block
            else {
                continue;
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
    use std::sync::Arc;

    use base64::Engine as _;
    use kage_core::{Content, MediaSource, Message, Role};
    use kage_session::{EntryId, FORMAT_VERSION, Header, MessageEntry, SessionEntry, SessionId};

    use super::{extension, generated_name, produced_image, save};

    fn image(role: Role, bytes: &[u8]) -> SessionEntry {
        let block = Content::Image {
            source: MediaSource::Base64 {
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
            mime: "image/png".into(),
        };
        SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: chrono::Utc::now(),
            message: Arc::new(Message::new(role, vec![block], None)),
            usage: None,
        })
    }

    #[test]
    fn the_image_is_found_by_its_hash_or_as_the_newest_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: chrono::Utc::now(),
            cwd: dir.path().to_path_buf(),
            model: "m".into(),
            system_prompt: String::new(),
            parent_session: None,
            parent_entry: None,
        };
        let mut writer = kage_session::SessionWriter::create(&path, header).unwrap();
        writer.append(&image(Role::Assistant, b"first")).unwrap();
        writer.append(&image(Role::Assistant, b"second")).unwrap();
        writer.append(&image(Role::ToolResult, b"tooled")).unwrap();
        writer.append(&image(Role::User, b"attached")).unwrap();
        drop(writer);

        assert_eq!(
            produced_image(&path, None).unwrap().1,
            b"tooled",
            "a tool's image counts"
        );
        let first = kage_session::BlobStore::digest(b"first");
        assert_eq!(produced_image(&path, Some(&first)).unwrap().1, b"first");
        let attached = kage_session::BlobStore::digest(b"attached");
        assert!(
            produced_image(&path, Some(&attached)).is_none(),
            "only the model's own"
        );
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
