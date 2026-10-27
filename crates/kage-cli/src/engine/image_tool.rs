//! The `generate_image` tool: one image from a prompt, from the images
//! endpoint `[tools.generate_image]` names.
//!
//! The tool is registered only when that table resolves to an endpoint
//! and a model, so a config without one advertises the same tools as
//! before. Its image is the result's media (see [`ToolMedia`]): every
//! client shows it with the call, it stays in the session, a model that
//! reads images in tool results sees it, and `/image save` writes it
//! into the project. A call may also name a `path` in the workspace to
//! save it there at once.

use std::fmt::Write as _;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use base64::Engine as _;
use kage_core::config::Config;
use kage_core::{Risk, ToolMedia, ToolOutput, ToolUpdate};
use kage_provider::ProviderError;
use kage_provider::images::{self, ImageRequest};
use kage_provider::media::{MediaEndpoint, MediaError};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};
use serde::Deserialize;

use super::media_tool::{self, failure, is_size, workspace_path};
use crate::auth::AuthStore;

/// The tool's name, as the model calls it and permission rules name it.
pub(crate) const GENERATE_IMAGE_TOOL: &str = "generate_image";

/// Input shape for the `generate_image` tool.
#[derive(Debug, Deserialize)]
struct GenerateImageInput {
    prompt: String,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

/// Generate one image and hand it back as the result's media.
#[derive(Debug)]
pub(crate) struct GenerateImageTool {
    endpoint: MediaEndpoint,
    model: String,
    timeout: Duration,
    /// Whether the answer is still asked for as base64. Cleared for the
    /// rest of the session once a server refuses the field.
    ask_base64: AtomicBool,
}

impl GenerateImageTool {
    /// The tool `config`'s `[tools.generate_image]` describes, with keys
    /// looked up in the environment and then in `store`.
    ///
    /// # Errors
    ///
    /// Why the table does not describe a usable tool, ready to follow
    /// `[tools.generate_image]` in a message.
    pub(super) fn from_config(config: &Config, store: &AuthStore) -> Result<Self, String> {
        let cfg = &config.tools.generate_image;
        let endpoint = media_tool::endpoint(GENERATE_IMAGE_TOOL, cfg, config, store)?;
        let model = media_tool::model(cfg)?;
        let timeout = media_tool::timeout(cfg)?;
        Ok(Self {
            endpoint,
            model,
            timeout,
            ask_base64: AtomicBool::new(true),
        })
    }
}

impl Tool for GenerateImageTool {
    fn name(&self) -> &'static str {
        GENERATE_IMAGE_TOOL
    }

    fn description(&self) -> &'static str {
        "Generate an image from a text prompt. The image comes back with this result: \
         the user sees it, and `path` also saves it in the workspace. Use it for \
         illustrations, icons, diagrams and mock-ups the user asks for."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "What to draw. Be specific, and put any exact text the \
                                    image must show in quotes.",
                },
                "size": {
                    "type": "string",
                    "description": "Width x height in pixels, such as 1024x1024. A hint: \
                                    the server may round or refuse it.",
                },
                "path": {
                    "type": "string",
                    "description": "Also save the image here, relative to the workspace, \
                                    such as assets/icon.png. The extension follows the \
                                    image's type, and an existing file is never replaced: \
                                    the name gets -2, -3 and so on.",
                },
            },
            "required": ["prompt"],
        })
    }

    /// A call may save a file in the workspace, so plan mode and a
    /// read-only session refuse it as they refuse `write`.
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
        let input: GenerateImageInput = serde_json::from_value(input)?;
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
        // Checked before the call, so a path outside the workspace costs
        // no generation.
        let path = input
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty());
        if let Some(path) = path {
            workspace_path(cx.workdir(), path)?;
        }
        if cx.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        cx.update(ToolUpdate {
            content: format!("generating an image with {}", self.model),
            structured: None,
        });
        let request = ImageRequest {
            model: self.model.clone(),
            prompt: prompt.to_owned(),
            size,
        };
        let image = match images::generate(
            &self.endpoint,
            &request,
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
                    "Image generation failed",
                    "the prompt or size",
                ));
            }
        };
        // A failed save keeps the image: it was made and paid for, and
        // it stays in the session for `/image save`.
        let saved = path.map(|path| {
            media_tool::save_in_workspace(cx.workdir(), path, image.mime, &image.bytes)
        });
        let mut text = format!(
            "Generated an image ({}, {}). It comes with this result, and the user sees it.",
            image.mime,
            kage_core::image::human_bytes(image.bytes.len())
        );
        match &saved {
            Some(Ok(saved)) => {
                let shown = media_tool::shown_path(cx.workdir(), saved);
                let _ = write!(text, " Saved as {}.", shown.display());
            }
            Some(Err(why)) => {
                let _ = write!(text, " It was not saved: {why}.");
            }
            None => {}
        }
        if let Some(note) = &image.revised_prompt {
            let _ = write!(text, "\nServer note: {note}");
        }
        Ok(ToolOutput {
            is_error: false,
            text,
            structured: Some(serde_json::json!({
                "model": self.model,
                "note": image.revised_prompt,
                "path": saved
                    .as_ref()
                    .and_then(|saved| saved.as_ref().ok())
                    .map(|path| path.display().to_string()),
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
    use super::super::media_tool::DEFAULT_TIMEOUT;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::path::Path;

    use kage_core::CancelFlag;
    use kage_core::config::MediaToolConfig;

    use super::*;

    /// A PNG cut after its `IHDR` size fields, enough for the sniffer.
    const PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0, 0,
        0, 1, 0, 0, 0, 1,
    ];

    /// Answers every request on a fresh port with one image, and counts
    /// the requests.
    fn image_server() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&hits);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                read_request(&mut conn);
                let body = serde_json::json!({
                    "data": [{
                        "b64_json": base64::engine::general_purpose::STANDARD.encode(PNG),
                        "revised_prompt": "a red kite, drawn",
                    }],
                })
                .to_string();
                let answer = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(answer.as_bytes());
            }
        });
        (url, hits)
    }

    /// Reads one request, headers and body. Closing a socket with
    /// request bytes still unread resets the connection on Windows,
    /// before the client reads the answer.
    fn read_request(conn: &mut std::net::TcpStream) {
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        let head_end = loop {
            if let Some(at) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            match conn.read(&mut buf) {
                Ok(0) | Err(_) => return,
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
                Ok(0) | Err(_) => return,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
    }

    fn tool(base_url: &str) -> GenerateImageTool {
        GenerateImageTool {
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
        config.tools.generate_image = cfg;
        config
    }

    #[test]
    fn the_image_rides_the_result_and_can_be_saved_in_the_workspace() {
        let (url, _) = image_server();
        let workdir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let tool = tool(&url);

        let output = tool
            .execute(serde_json::json!({ "prompt": "a red kite" }), &cx)
            .unwrap();
        assert!(!output.is_error, "{}", output.text);
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].mime, "image/png");
        assert!(
            output.text.contains("Server note: a red kite, drawn"),
            "{}",
            output.text
        );
        assert_eq!(
            std::fs::read_dir(workdir.path()).unwrap().count(),
            0,
            "nothing is written without a path"
        );

        let input = serde_json::json!({ "prompt": "a red kite", "path": "art/kite.png" });
        let first = tool.execute(input.clone(), &cx).unwrap();
        let second = tool.execute(input, &cx).unwrap();
        let saved_as = |name: &str| format!("Saved as {}.", Path::new("art").join(name).display());
        assert!(first.text.contains(&saved_as("kite.png")), "{}", first.text);
        assert!(
            second.text.contains(&saved_as("kite-2.png")),
            "{}",
            second.text
        );
        assert_eq!(
            std::fs::read(workdir.path().join("art/kite.png")).unwrap(),
            PNG
        );

        let misnamed = serde_json::json!({ "prompt": "a red kite", "path": "art/bird.jpg" });
        let output = tool.execute(misnamed, &cx).unwrap();
        assert!(
            output.text.contains(&saved_as("bird.png")),
            "{}",
            output.text
        );
        assert!(!workdir.path().join("art/bird.jpg").exists());
    }

    #[test]
    fn an_image_that_cannot_be_saved_still_rides_the_result() {
        let (url, hits) = image_server();
        let workdir = tempfile::tempdir().unwrap();
        std::fs::write(workdir.path().join("art"), b"a file, not a directory").unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let output = tool(&url)
            .execute(
                serde_json::json!({ "prompt": "a kite", "path": "art/kite.png" }),
                &cx,
            )
            .unwrap();
        assert!(!output.is_error);
        assert_eq!(output.media.len(), 1);
        assert!(
            output.text.contains("It was not saved: "),
            "{}",
            output.text
        );
        assert!(output.structured.unwrap()["path"].is_null());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_path_outside_the_workspace_costs_no_generation() {
        let (url, hits) = image_server();
        let workdir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        for path in ["../out.png", "/tmp/out.png", "art/x.png:stream", ".", "./"] {
            let err = tool(&url)
                .execute(serde_json::json!({ "prompt": "x", "path": path }), &cx)
                .unwrap_err();
            assert!(matches!(err, ToolError::Path { .. }), "{path}: {err:?}");
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn bad_input_is_refused() {
        let workdir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let tool = tool("http://127.0.0.1:9");
        for input in [
            serde_json::json!({ "prompt": "  " }),
            serde_json::json!({ "prompt": "x", "size": "large" }),
        ] {
            assert!(
                matches!(tool.execute(input, &cx), Err(ToolError::InvalidInput(_))),
                "refused before any request"
            );
        }
    }

    #[test]
    fn a_table_resolves_or_says_why_not() {
        let store = AuthStore::empty();
        let from = |cfg| GenerateImageTool::from_config(&config(cfg), &store);

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
        saved.set_api_key(GENERATE_IMAGE_TOOL, "sk-saved");
        let unset_env = || MediaToolConfig {
            base_url: Some("https://images.example/v1".into()),
            api_key_env: Some("KAGE_TEST_IMAGE_KEY_NEVER_SET".into()),
            model: Some("image".into()),
            ..MediaToolConfig::default()
        };
        let keyed = GenerateImageTool::from_config(&config(unset_env()), &saved).unwrap();
        assert_eq!(keyed.endpoint.api_key, "sk-saved", "the saved key fills in");
        let err = from(unset_env()).unwrap_err();
        assert!(err.contains("kage auth login generate_image"), "{err}");

        let custom: Config = toml::from_str(
            "[providers.custom.relay]\nkind = \"anthropic\"\nbase_url = \"https://relay.example\"\n\
             [[providers.custom.relay.models]]\nid = \"m\"\nname = \"M\"\n\n\
             [tools.generate_image]\nprovider = \"relay\"\nmodel = \"image\"\n",
        )
        .unwrap();
        let err = GenerateImageTool::from_config(&custom, &store).unwrap_err();
        assert!(err.contains("not of kind openai"), "{err}");
    }

    /// Execute one call against a server refusal, keeping its retry hint.
    fn refused(status: &str, wait: Option<u64>, timeout_secs: u64) -> ToolOutput {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let body = r#"{"error":{"message":"the server's reason."}}"#;
        let wait = wait
            .map(|secs| format!("retry-after: {secs}\r\n"))
            .unwrap_or_default();
        let answer = format!(
            "HTTP/1.1 {status}\r\n{wait}content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            read_request(&mut conn);
            conn.write_all(answer.as_bytes()).unwrap();
        });
        let workdir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let mut tool = tool(&url);
        tool.timeout = Duration::from_secs(timeout_secs);
        let output = tool
            .execute(serde_json::json!({ "prompt": "a kite" }), &cx)
            .unwrap();
        server.join().unwrap();
        assert!(output.is_error);
        assert!(output.media.is_empty());
        assert!(output.structured.is_none());
        assert!(!output.terminate);
        assert!(
            output.text.starts_with("Image generation failed:"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("the server's reason.\n"),
            "{}",
            output.text
        );
        output
    }

    #[test]
    fn a_403_tells_the_user_to_fix_the_key_or_service() {
        let output = refused("403 Forbidden", None, 180);
        assert!(
            output.text.contains("key or service needs fixing"),
            "{}",
            output.text
        );
        assert!(output.text.contains("tell the user"), "{}", output.text);
        assert!(
            !output.text.contains("change the prompt"),
            "{}",
            output.text
        );
    }

    #[test]
    fn a_410_tells_the_user_to_fix_the_key_or_service() {
        let output = refused("410 Gone", None, 180);
        assert!(
            output.text.contains("key or service needs fixing"),
            "{}",
            output.text
        );
        assert!(output.text.contains("tell the user"), "{}", output.text);
        assert!(
            !output.text.contains("change the prompt"),
            "{}",
            output.text
        );
    }

    #[test]
    fn a_413_asks_for_a_smaller_input() {
        let output = refused("413 Content Too Large", None, 180);
        assert!(
            output
                .text
                .contains("input is larger than the server takes"),
            "{}",
            output.text
        );
        assert!(
            output
                .text
                .contains("The input is larger than the server takes; make it smaller."),
            "{}",
            output.text
        );
        assert!(
            !output.text.contains("change the prompt"),
            "{}",
            output.text
        );
    }

    #[test]
    fn a_429_with_a_short_wait_can_be_retried() {
        for wait in [0, 7] {
            let output = refused("429 Too Many Requests", Some(wait), 7);
            let advice = format!(
                "A retry may succeed; the server asks to wait {}s first.",
                wait.max(1)
            );
            assert!(output.text.contains(&advice), "{}", output.text);
            assert!(!output.text.contains("do not retry"), "{}", output.text);
        }
    }

    #[test]
    fn a_503_with_a_wait_past_the_tools_timeout_stops_retries() {
        for (wait, timeout, shown) in [
            (61, 60, "The server asks to wait 2 min."),
            (41_230, 180, "The server asks to wait 11 h 28 min."),
            (86_400, 3600, "The server asks to wait 24 h."),
        ] {
            let output = refused("503 Service Unavailable", Some(wait), timeout);
            assert!(output.text.contains(shown), "{}", output.text);
            assert!(
                output
                    .text
                    .contains(". Do not retry in this turn; tell the user."),
                "{}",
                output.text
            );
            assert!(
                !output.text.contains("A retry may succeed"),
                "{}",
                output.text
            );
        }
    }

    #[test]
    fn other_refusals_keep_their_retry_advice() {
        for (status, advice) in [
            ("401 Unauthorized", "the key needs fixing, so tell the user"),
            (
                "400 Bad Request",
                "change the prompt or size first, or tell the user",
            ),
            ("503 Service Unavailable", "A retry may succeed."),
        ] {
            let output = refused(status, None, 180);
            assert!(output.text.contains(advice), "{}", output.text);
        }
    }
}
