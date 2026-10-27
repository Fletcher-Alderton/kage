//! The `edit_image` tool: one workspace image edited from a prompt, from
//! the images endpoint `[tools.edit_image]` names.
//!
//! The tool is registered only when that table resolves to an endpoint
//! and a model, so a config without one advertises the same tools as
//! before. Its edited image is the result's media (see [`ToolMedia`]):
//! every client shows it with the call, it stays in the session, and a
//! model that reads images in tool results sees it. A call may also
//! name a `path` in the workspace to save it there at once.
//!
//! The source image is read from the workspace and uploaded, so it is
//! confined to the workspace even when path confinement is off.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use base64::Engine as _;
use kage_core::config::Config;
use kage_core::{Risk, ToolMedia, ToolOutput, ToolUpdate};
use kage_provider::ProviderError;
use kage_provider::images::{self, GeneratedImage, ImageEdit};
use kage_provider::media::{MediaEndpoint, MediaError};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};
use serde::Deserialize;

use super::media_tool::{self, failure, is_size, workspace_path};
use crate::auth::AuthStore;

/// The tool's name, as the model calls it and permission rules name it.
pub(crate) const EDIT_IMAGE_TOOL: &str = "edit_image";

/// The largest source image read and uploaded.
const MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

/// Input shape for the `edit_image` tool.
#[derive(Debug, Deserialize)]
struct EditImageInput {
    image: String,
    prompt: String,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

/// Edit one workspace image from a prompt and hand the result back as
/// the result's media.
#[derive(Debug)]
pub(crate) struct EditImageTool {
    endpoint: MediaEndpoint,
    model: String,
    timeout: Duration,
    /// Whether the answer is still asked for as base64. Cleared for the
    /// rest of the session once a server refuses the field.
    ask_base64: AtomicBool,
}

impl EditImageTool {
    /// The tool `config`'s `[tools.edit_image]` describes, with keys
    /// looked up in the environment and then in `store`.
    ///
    /// # Errors
    ///
    /// Why the table does not describe a usable tool, ready to follow
    /// `[tools.edit_image]` in a message.
    pub(super) fn from_config(config: &Config, store: &AuthStore) -> Result<Self, String> {
        let cfg = &config.tools.edit_image;
        let endpoint = media_tool::endpoint(EDIT_IMAGE_TOOL, cfg, config, store)?;
        let model = media_tool::model(cfg)?;
        let timeout = media_tool::timeout(cfg)?;
        Ok(Self {
            endpoint,
            model,
            timeout,
            ask_base64: AtomicBool::new(true),
        })
    }

    /// Reads the source image named in the call, refusing anything but
    /// a PNG, JPEG or WebP file of at most [`MAX_SOURCE_BYTES`]. The
    /// name is confined to the workspace whatever `confine_paths` says,
    /// because the file is uploaded.
    fn read_source(cx: &ToolContext<'_>, name: &str) -> Result<(Vec<u8>, &'static str), ToolError> {
        let source = workspace_path(cx.workdir(), name)?;
        let too_long = |bytes: u64| {
            ToolError::InvalidInput(format!(
                "{name} is {}; the largest source edit_image reads is {}",
                kage_core::image::human_bytes(usize::try_from(bytes).unwrap_or(usize::MAX)),
                kage_core::image::human_bytes(
                    usize::try_from(MAX_SOURCE_BYTES).unwrap_or(usize::MAX)
                ),
            ))
        };
        let cannot_read = |err: std::io::Error| match err.kind() {
            std::io::ErrorKind::NotFound => {
                ToolError::InvalidInput(format!("{name} does not exist"))
            }
            _ => ToolError::Other(format!("cannot read {}: {err}", source.display())),
        };
        // Opened before it is checked, so the checks describe the file
        // that is read: a FIFO or a link swapped in after the path was
        // resolved is refused rather than waited on or followed.
        let mut file = open_source(&source).map_err(cannot_read)?;
        let meta = file.metadata().map_err(cannot_read)?;
        if !meta.is_file() {
            return Err(ToolError::InvalidInput(format!(
                "{name} is not a regular file"
            )));
        }
        if meta.len() > MAX_SOURCE_BYTES {
            return Err(too_long(meta.len()));
        }
        let length = usize::try_from(meta.len()).unwrap_or(0);
        // The metadata check bounds the common case; the take bounds a
        // file that grew since.
        let mut bytes = Vec::with_capacity(length);
        (&mut file)
            .take(MAX_SOURCE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(cannot_read)?;
        let read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if read > MAX_SOURCE_BYTES {
            return Err(too_long(read));
        }
        let mime = match kage_core::image::sniff_mime(&bytes) {
            Some("image/gif") => {
                return Err(ToolError::InvalidInput(format!(
                    "{name} is a GIF; edit_image takes PNG, JPEG or WebP"
                )));
            }
            Some(mime @ ("image/png" | "image/jpeg" | "image/webp")) => mime,
            _ => {
                return Err(ToolError::InvalidInput(format!(
                    "{name} is not a PNG, JPEG or WebP image"
                )));
            }
        };
        Ok((bytes, mime))
    }

    /// Saves the edited image when `path` is given, and builds the
    /// result text: what was edited, where it was saved or why not, and
    /// the server's note. A failed save keeps the image: it was made and
    /// paid for, and it stays in the session for `/image save`.
    fn text_and_saved(
        cx: &ToolContext<'_>,
        path: Option<&str>,
        image: &GeneratedImage,
        source: &str,
    ) -> (String, Option<PathBuf>) {
        let saved = path.map(|path| {
            media_tool::save_in_workspace(cx.workdir(), path, image.mime, &image.bytes)
        });
        let mut text = format!(
            "Edited {source} ({}, {}). It comes with this result, and the user sees it.",
            image.mime,
            kage_core::image::human_bytes(image.bytes.len())
        );
        let saved = match saved {
            Some(Ok(saved)) => {
                let shown = media_tool::shown_path(cx.workdir(), &saved);
                let _ = write!(text, " Saved as {}.", shown.display());
                Some(saved)
            }
            Some(Err(why)) => {
                let _ = write!(text, " It was not saved: {why}.");
                None
            }
            None => None,
        };
        if let Some(note) = &image.revised_prompt {
            let _ = write!(text, "\nServer note: {note}");
        }
        (text, saved)
    }
}

/// Opens `path` for reading without blocking on a FIFO and, on Unix,
/// without following a symlink in its last component.
fn open_source(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW);
    }
    options.open(path)
}

impl Tool for EditImageTool {
    fn name(&self) -> &'static str {
        EDIT_IMAGE_TOOL
    }

    fn description(&self) -> &'static str {
        "Edit a PNG, JPEG or WebP file in the workspace from a text prompt. The edited image \
         comes back with this result: the user sees it, and `path` also saves it. An image \
         `generate_image` made must first be saved with its `path`, because this tool reads \
         files. Use `read` to look at the source image first."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "image": {
                    "type": "string",
                    "description": "The image to edit: a PNG, JPEG or WebP file in the \
                                    workspace, such as assets/icon.png. It is uploaded to \
                                    the image server and is not changed.",
                },
                "prompt": {
                    "type": "string",
                    "description": "What to change. Say what must stay as it is, and put \
                                    any exact text in quotes.",
                },
                "size": {
                    "type": "string",
                    "description": "Width x height in pixels, such as 1024x1024. A hint: \
                                    without it the server usually keeps the image's size.",
                },
                "path": {
                    "type": "string",
                    "description": "Also save the edited image here, relative to the \
                                    workspace. An existing file is never replaced, the \
                                    source included: the name gets -2, -3 and so on.",
                },
            },
            "required": ["image", "prompt"],
        })
    }

    /// A call uploads a file and may save one in the workspace, so plan
    /// mode and a read-only session refuse it as they refuse `write`.
    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn execution_mode(&self) -> Option<ExecMode> {
        Some(ExecMode::Parallel)
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: EditImageInput = serde_json::from_value(input)?;
        let prompt = input.prompt.trim();
        if prompt.is_empty() {
            return Err(ToolError::InvalidInput("prompt is empty".to_owned()));
        }
        let size = match input.size.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(size) if is_size(size) => Some(size.to_owned()),
            Some(size) => {
                return Err(ToolError::InvalidInput(format!(
                    "size `{size}` is not WIDTHxHEIGHT, such as 1024x1024"
                )));
            }
        };
        let path = input
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty());
        if let Some(path) = path {
            workspace_path(cx.workdir(), path)?;
        }
        let source = input.image.trim();
        if source.is_empty() {
            return Err(ToolError::InvalidInput("image is empty".to_owned()));
        }
        let (bytes, mime) = Self::read_source(cx, source)?;
        if cx.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        cx.update(ToolUpdate {
            content: format!("editing {} with {}", source, self.model),
            structured: None,
        });
        let edit = ImageEdit {
            model: self.model.clone(),
            prompt: prompt.to_owned(),
            size,
            image: bytes,
            mime,
        };
        let image = match images::edit(
            &self.endpoint,
            &edit,
            self.timeout,
            cx.cancel_flag(),
            &self.ask_base64,
        ) {
            Ok(image) => image,
            Err(MediaError {
                error: ProviderError::Cancelled,
                ..
            }) => return Err(ToolError::Cancelled),
            Err(error) => {
                return Ok(failure(
                    &error,
                    self.timeout,
                    "Image edit failed",
                    "the prompt, size or source image",
                ));
            }
        };
        let (text, saved) = Self::text_and_saved(cx, path, &image, source);
        Ok(ToolOutput {
            is_error: false,
            text,
            structured: Some(serde_json::json!({
                "model": self.model,
                "source": source,
                "note": image.revised_prompt,
                "path": saved.as_ref().map(|path| path.display().to_string()),
            })),
            terminate: false,
            media: vec![ToolMedia {
                mime: image.mime.to_owned(),
                data: base64::engine::general_purpose::STANDARD.encode(&image.bytes),
            }],
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use kage_core::CancelFlag;
    use kage_core::config::MediaToolConfig;

    use super::super::image_tool::GENERATE_IMAGE_TOOL;
    use super::super::media_tool::DEFAULT_TIMEOUT;
    use super::super::media_tool::WithMediaTools;
    use super::*;

    /// A PNG cut after its `IHDR` size fields, enough for the sniffer.
    const PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0, 0,
        0, 1, 0, 0, 0, 1,
    ];

    /// A JPEG's leading bytes, enough for the sniffer.
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 0];

    /// A WebP header, enough for the sniffer.
    const WEBP: &[u8] = b"RIFF\x0c\x00\x00\x00WEBPVP8 ";

    /// The edited image the test server answers with, base64-encoded.
    fn png_answer() -> String {
        serde_json::json!({
            "data": [{
                "b64_json": base64::engine::general_purpose::STANDARD.encode(PNG),
                "revised_prompt": "a blue kite, drawn",
            }],
        })
        .to_string()
    }

    /// What the test server saw: the request count and each request
    /// read, head and body.
    #[derive(Clone)]
    struct Seen {
        hits: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl Seen {
        fn count(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        fn contains(&self, bytes: &[u8]) -> bool {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.windows(bytes.len()).any(|w| w == bytes))
        }
    }

    /// Answers every request on a fresh port with one image, counting
    /// the requests and keeping each one read.
    fn edit_server() -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = Seen {
            hits: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let known = Arc::clone(&seen.hits);
        let requests = Arc::clone(&seen.requests);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                known.fetch_add(1, Ordering::SeqCst);
                let request = read_request(&mut conn);
                requests.lock().unwrap().push(request);
                let body = png_answer();
                let answer = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(answer.as_bytes());
            }
        });
        (url, seen)
    }

    /// Reads one request, headers and body. Closing a socket with
    /// request bytes still unread resets the connection on Windows,
    /// before the client reads the answer.
    fn read_request(conn: &mut std::net::TcpStream) -> Vec<u8> {
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        let head_end = loop {
            if let Some(at) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            match conn.read(&mut buf) {
                Ok(0) | Err(_) => return seen,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        };
        let head = String::from_utf8_lossy(&seen[..head_end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0);
        while seen.len() < head_end + length {
            match conn.read(&mut buf) {
                Ok(0) | Err(_) => return seen,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
        seen
    }

    fn tool(base_url: &str) -> EditImageTool {
        EditImageTool {
            endpoint: MediaEndpoint {
                base_url: base_url.to_owned(),
                ..MediaEndpoint::default()
            },
            model: "image".to_owned(),
            timeout: Duration::from_secs(10),
            ask_base64: AtomicBool::new(true),
        }
    }

    fn config(cfg: MediaToolConfig) -> Config {
        let mut config = Config::default();
        config.tools.edit_image = cfg;
        config
    }

    #[test]
    fn the_edited_image_rides_the_result_and_the_source_is_uploaded() {
        let (url, seen) = edit_server();
        let workdir = tempfile::tempdir().unwrap();
        let src = workdir.path().join("src/icon.png");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, PNG).unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let output = tool(&url)
            .execute(
                serde_json::json!({ "image": "src/icon.png", "prompt": "make it blue" }),
                &cx,
            )
            .unwrap();
        assert!(!output.is_error, "{}", output.text);
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].mime, "image/png");
        assert!(
            output
                .text
                .starts_with("Edited src/icon.png (image/png, 24 B)."),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("Server note: a blue kite, drawn"),
            "{}",
            output.text
        );
        assert_eq!(output.structured.unwrap()["source"], "src/icon.png");
        assert_eq!(seen.count(), 1);
        assert!(seen.contains(PNG), "the source bytes reach the server");
    }

    #[test]
    fn jpeg_and_webp_sources_are_uploaded_too() {
        for (name, bytes) in [("photo.jpg", JPEG), ("drawing.webp", WEBP)] {
            let (url, seen) = edit_server();
            let workdir = tempfile::tempdir().unwrap();
            std::fs::write(workdir.path().join(name), bytes).unwrap();
            let cancel = CancelFlag::new();
            let cx = ToolContext::new(workdir.path(), &cancel);
            let output = tool(&url)
                .execute(
                    serde_json::json!({ "image": name, "prompt": "sharpen" }),
                    &cx,
                )
                .unwrap();
            assert!(!output.is_error, "{}", output.text);
            assert_eq!(output.media[0].mime, "image/png");
            assert_eq!(seen.count(), 1);
            assert!(seen.contains(bytes), "{name}: its bytes reach the server");
        }
    }

    #[test]
    fn a_path_equal_to_the_source_saves_under_a_new_name() {
        let (url, seen) = edit_server();
        let workdir = tempfile::tempdir().unwrap();
        std::fs::write(workdir.path().join("icon.png"), PNG).unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let output = tool(&url)
            .execute(
                serde_json::json!({
                    "image": "icon.png",
                    "prompt": "make it blue",
                    "path": "icon.png",
                }),
                &cx,
            )
            .unwrap();
        assert!(
            output.text.contains("Saved as icon-2.png."),
            "{}",
            output.text
        );
        assert!(
            output.structured.unwrap()["path"]
                .as_str()
                .unwrap()
                .ends_with("icon-2.png")
        );
        assert_eq!(std::fs::read(workdir.path().join("icon.png")).unwrap(), PNG);
        assert_eq!(
            std::fs::read(workdir.path().join("icon-2.png")).unwrap(),
            PNG
        );
        assert_eq!(seen.count(), 1);
    }

    /// `ToolContext::new` leaves `confine_paths` off, as the user's
    /// `confine_paths = false` does, and the source stays confined.
    #[cfg(unix)]
    #[test]
    fn a_source_linked_out_of_the_workspace_is_refused_unconfined() {
        let (url, seen) = edit_server();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.png"), PNG).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.png"),
            dir.path().join("link.png"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(dir.path(), &cancel);
        for image in ["link.png", "out/secret.png"] {
            let err = tool(&url)
                .execute(serde_json::json!({ "image": image, "prompt": "x" }), &cx)
                .expect_err("a source outside the workspace is refused");
            assert!(matches!(err, ToolError::Path { .. }), "{image}: {err:?}");
        }
        assert_eq!(seen.count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_source_is_refused_without_waiting() {
        let (url, seen) = edit_server();
        let dir = tempfile::tempdir().unwrap();
        nix::unistd::mkfifo(&dir.path().join("pipe.png"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(dir.path(), &cancel);
        let err = tool(&url)
            .execute(
                serde_json::json!({ "image": "pipe.png", "prompt": "x" }),
                &cx,
            )
            .expect_err("a FIFO is not a regular file");
        assert!(
            matches!(&err, ToolError::InvalidInput(why) if why.contains("not a regular file")),
            "{err:?}"
        );
        assert_eq!(seen.count(), 0);
    }

    #[test]
    fn the_source_name_is_trimmed() {
        let (url, seen) = edit_server();
        let workdir = tempfile::tempdir().unwrap();
        std::fs::write(workdir.path().join("icon.png"), PNG).unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let output = tool(&url)
            .execute(
                serde_json::json!({ "image": "  icon.png \n", "prompt": "x" }),
                &cx,
            )
            .unwrap();
        assert_eq!(output.structured.unwrap()["source"], "icon.png");
        assert_eq!(seen.count(), 1);
        let err = tool(&url)
            .execute(serde_json::json!({ "image": "   ", "prompt": "x" }), &cx)
            .expect_err("an empty source is refused");
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
        assert_eq!(seen.count(), 1);
    }

    #[test]
    fn a_source_that_cannot_be_edited_costs_no_request() {
        let (url, seen) = edit_server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("animated.gif"), b"GIF89a......").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"just text").unwrap();
        std::fs::create_dir(dir.path().join("folder")).unwrap();
        std::fs::write(dir.path().join("icon.png"), PNG).unwrap();
        std::fs::File::create(dir.path().join("huge.png"))
            .unwrap()
            .set_len(MAX_SOURCE_BYTES + 1)
            .unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(dir.path(), &cancel);
        let refused = |input: serde_json::Value| {
            tool(&url)
                .execute(input, &cx)
                .expect_err("the call is refused before any request")
        };
        for input in [
            serde_json::json!({ "image": "../out.png", "prompt": "x" }),
            serde_json::json!({ "image": "/tmp/out.png", "prompt": "x" }),
            serde_json::json!({ "image": "missing.png", "prompt": "x" }),
            serde_json::json!({ "image": "folder", "prompt": "x" }),
            serde_json::json!({ "image": "animated.gif", "prompt": "x" }),
            serde_json::json!({ "image": "notes.txt", "prompt": "x" }),
            serde_json::json!({ "image": "huge.png", "prompt": "x" }),
            serde_json::json!({ "image": "icon.png", "prompt": "  " }),
            serde_json::json!({ "image": "icon.png", "prompt": "x", "size": "large" }),
            serde_json::json!({ "image": "icon.png", "prompt": "x", "path": "../out.png" }),
        ] {
            let err = refused(input);
            assert!(
                matches!(err, ToolError::InvalidInput(_) | ToolError::Path { .. }),
                "{err:?}"
            );
        }
        let err = refused(serde_json::json!({ "image": "animated.gif", "prompt": "x" }));
        let ToolError::InvalidInput(why) = err else {
            panic!("the GIF is refused as input: {err:?}")
        };
        assert!(
            why.contains("is a GIF; edit_image takes PNG, JPEG or WebP"),
            "{why}"
        );
        let err = refused(serde_json::json!({ "image": "notes.txt", "prompt": "x" }));
        let ToolError::InvalidInput(why) = err else {
            panic!("text bytes are refused as input: {err:?}")
        };
        assert!(why.contains("is not a PNG, JPEG or WebP image"), "{why}");
        assert_eq!(seen.count(), 0, "no request is made for a refused call");
    }

    #[test]
    fn a_table_resolves_or_says_why_not() {
        let store = AuthStore::empty();
        let from = |cfg| EditImageTool::from_config(&config(cfg), &store);

        let named = from(MediaToolConfig {
            base_url: Some("https://images.example/v1".into()),
            model: Some("image".into()),
            ..MediaToolConfig::default()
        })
        .unwrap();
        assert_eq!(named.endpoint.base_url, "https://images.example/v1");
        assert!(named.endpoint.api_key.is_empty());
        assert_eq!(named.timeout, DEFAULT_TIMEOUT);

        let cases: [(MediaToolConfig, &str); 4] = [
            (
                MediaToolConfig {
                    base_url: Some("https://images.example/v1".into()),
                    ..MediaToolConfig::default()
                },
                "names no `model`",
            ),
            (
                MediaToolConfig {
                    model: Some("image".into()),
                    ..MediaToolConfig::default()
                },
                "names neither",
            ),
            (
                MediaToolConfig {
                    provider: Some("anthropic".into()),
                    model: Some("image".into()),
                    ..MediaToolConfig::default()
                },
                "cannot serve this tool: it calls an OpenAI API",
            ),
            (
                MediaToolConfig {
                    base_url: Some("https://images.example/v1".into()),
                    model: Some("image".into()),
                    timeout_secs: Some(0),
                    ..MediaToolConfig::default()
                },
                "timeout_secs",
            ),
        ];
        for (cfg, want) in cases {
            let err = from(cfg).unwrap_err();
            assert!(err.contains(want), "{err}");
        }

        let mut saved = AuthStore::empty();
        saved.set_api_key(EDIT_IMAGE_TOOL, "sk-saved");
        let unset_env = || MediaToolConfig {
            base_url: Some("https://images.example/v1".into()),
            api_key_env: Some("KAGE_TEST_EDIT_KEY_NEVER_SET".into()),
            model: Some("image".into()),
            ..MediaToolConfig::default()
        };
        let keyed = EditImageTool::from_config(&config(unset_env()), &saved).unwrap();
        assert_eq!(keyed.endpoint.api_key, "sk-saved", "the saved key fills in");
        let err = from(unset_env()).unwrap_err();
        assert!(err.contains("kage auth login edit_image"), "{err}");

        let custom: Config = toml::from_str(
            "[providers.custom.relay]\nkind = \"anthropic\"\nbase_url = \"https://relay.example\"\n\
             [[providers.custom.relay.models]]\nid = \"m\"\nname = \"M\"\n\n\
             [tools.edit_image]\nprovider = \"relay\"\nmodel = \"image\"\n",
        )
        .unwrap();
        let err = EditImageTool::from_config(&custom, &store).unwrap_err();
        assert!(err.contains("not of kind openai"), "{err}");
    }

    #[test]
    fn only_edit_image_configured_registers_edit_image_alone() {
        let mut config = Config::default();
        config.tools.edit_image = MediaToolConfig {
            base_url: Some("https://images.example/v1".into()),
            model: Some("image".into()),
            ..MediaToolConfig::default()
        };
        let tools = kage_tools::builtin_registry().with_media_tools(&config);
        assert!(tools.get(EDIT_IMAGE_TOOL).is_some());
        assert!(tools.get(GENERATE_IMAGE_TOOL).is_none());
    }
}
