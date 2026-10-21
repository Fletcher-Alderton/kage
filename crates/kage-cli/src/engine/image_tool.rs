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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use kage_core::config::{Config, CustomProviderKind, GenerateImageConfig};
use kage_core::{Risk, ToolMedia, ToolOutput, ToolUpdate};
use kage_provider::ProviderError;
use kage_provider::compat::{COMPAT_PROVIDERS, CompatKind};
use kage_provider::images::{self, ImageEndpoint, ImageError, ImageRequest};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError, ToolRegistry};
use serde::Deserialize;

use crate::auth::{self, AuthStore};

/// The tool's name, as the model calls it and permission rules name it.
pub(crate) const GENERATE_IMAGE_TOOL: &str = "generate_image";

/// How long one generation may take when the config says nothing.
/// Image servers commonly answer in 10 to 40 seconds, and some give up
/// on their own deadline (150 seconds on one) before answering 503, so
/// the client waits past that to hear the server's answer.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

/// The longest `timeout_secs` accepted: an hour, past any image
/// server's own deadline.
const MAX_TIMEOUT_SECS: u64 = 3600;

/// Registers `generate_image` when `[tools.generate_image]` resolves.
pub(crate) trait WithImageGeneration {
    /// `self` with `generate_image` registered when `config`'s
    /// `[tools.generate_image]` resolves to an endpoint and a model.
    /// An unset table changes nothing. One that does not resolve is
    /// reported on stderr and leaves the tool out, so a broken table
    /// costs one tool rather than the session.
    #[must_use]
    fn with_image_generation(self, config: &Config) -> Self;
}

impl WithImageGeneration for ToolRegistry {
    fn with_image_generation(mut self, config: &Config) -> Self {
        if config.tools.generate_image.is_default() {
            return self;
        }
        let store = AuthStore::load().unwrap_or_else(|_| AuthStore::empty());
        match GenerateImageTool::from_config(config, &store) {
            Ok(tool) => self.register(Arc::new(tool)),
            Err(reason) => {
                eprintln!("kage: [tools.generate_image] {reason}; {GENERATE_IMAGE_TOOL} is off");
            }
        }
        self
    }
}

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
    endpoint: ImageEndpoint,
    model: String,
    timeout: Duration,
}

impl GenerateImageTool {
    /// The tool `config`'s `[tools.generate_image]` describes, with keys
    /// looked up in the environment and then in `store`.
    ///
    /// # Errors
    ///
    /// Why the table does not describe a usable tool, ready to follow
    /// `[tools.generate_image]` in a message.
    fn from_config(config: &Config, store: &AuthStore) -> Result<Self, String> {
        let cfg = &config.tools.generate_image;
        let endpoint = endpoint(cfg, config, store)?;
        let model = cfg
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .ok_or("names no `model`")?
            .to_owned();
        let timeout = match cfg.timeout_secs {
            None => DEFAULT_TIMEOUT,
            Some(secs @ 1..=MAX_TIMEOUT_SECS) => Duration::from_secs(secs),
            Some(_) => {
                return Err(format!("timeout_secs must be from 1 to {MAX_TIMEOUT_SECS}"));
            }
        };
        Ok(Self {
            endpoint,
            model,
            timeout,
        })
    }
}

/// The endpoint `cfg` names: its own `base_url`, with `headers` and the
/// key in `api_key_env` or saved under the tool's name, or else the
/// base URL, key and headers kage reaches `provider` with.
fn endpoint(
    cfg: &GenerateImageConfig,
    config: &Config,
    store: &AuthStore,
) -> Result<ImageEndpoint, String> {
    if let Some(base_url) = cfg.base_url.as_deref().filter(|url| !url.trim().is_empty()) {
        let api_key = match cfg.api_key_env.as_deref().filter(|env| !env.is_empty()) {
            None => store
                .access_token(GENERATE_IMAGE_TOOL)
                .unwrap_or_default()
                .to_owned(),
            Some(env) => crate::providers::lookup_key_with_env(GENERATE_IMAGE_TOOL, env, store)
                .ok_or_else(|| {
                    format!(
                        "names api_key_env = \"{env}\", which is not set, and no key is saved \
                         with `kage auth login {GENERATE_IMAGE_TOOL}`"
                    )
                })?,
        };
        return Ok(ImageEndpoint {
            base_url: base_url.to_owned(),
            api_key,
            headers: cfg.headers.clone(),
        });
    }
    match cfg.provider.as_deref().filter(|id| !id.is_empty()) {
        Some(id) => provider_endpoint(id, config, store),
        None => Err("names neither `base_url` nor `provider`".to_owned()),
    }
}

/// The base URL, key and headers kage reaches provider `id` with, for a
/// provider that speaks the `OpenAI` API: a custom provider of kind
/// `openai`, `openai` itself, or an OpenAI-compatible catalog provider.
/// kage's `anthropic` and `google` providers speak other APIs, so they
/// cannot lend the tool their endpoint.
fn provider_endpoint(
    id: &str,
    config: &Config,
    store: &AuthStore,
) -> Result<ImageEndpoint, String> {
    let no_key = |env: &str| {
        if env.is_empty() {
            format!("provider `{id}` has no saved key")
        } else {
            format!("provider `{id}` has no key: {env} is not set and no key is saved")
        }
    };
    if let Some(custom) = config.providers.custom.get(id) {
        if custom.kind != CustomProviderKind::OpenAi {
            return Err(format!(
                "provider `{id}` is not of kind openai, the only kind the tool can call"
            ));
        }
        let env = custom.key_env(id);
        let api_key = if env.is_empty() {
            String::new()
        } else {
            crate::providers::lookup_key_with_env(id, &env, store).ok_or_else(|| no_key(&env))?
        };
        return Ok(ImageEndpoint {
            base_url: custom.base_url.clone(),
            api_key,
            headers: custom.headers.clone(),
        });
    }
    let default_base = if id == "openai" {
        kage_provider::openai::DEFAULT_BASE_URL
    } else {
        match COMPAT_PROVIDERS.iter().find(|entry| entry.id == id) {
            Some(entry) if entry.kind == CompatKind::OpenAi => entry.base_url,
            Some(_) => {
                return Err(format!(
                    "provider `{id}` speaks the Anthropic API, which has no images endpoint"
                ));
            }
            None => {
                return Err(format!(
                    "provider `{id}` cannot serve images: the tool calls the OpenAI Images \
                     API, which kage reaches through `openai`, an OpenAI-compatible \
                     provider, or a [providers.custom.<id>] of kind openai"
                ));
            }
        }
    };
    let ov = config.providers.overrides.get(id);
    let env = ov
        .and_then(|o| o.api_key_env.as_deref())
        .unwrap_or_else(|| auth::env_var_for(id));
    let api_key =
        crate::providers::lookup_key_with_env(id, env, store).ok_or_else(|| no_key(env))?;
    Ok(ImageEndpoint {
        base_url: ov
            .and_then(|o| o.base_url.clone())
            .unwrap_or_else(|| default_base.to_owned()),
        api_key,
        headers: ov.map(|o| o.headers.clone()).unwrap_or_default(),
    })
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
        let image = match images::generate(&self.endpoint, &request, self.timeout, cx.cancel_flag())
        {
            Ok(image) => image,
            Err(ImageError {
                error: ProviderError::Cancelled,
                ..
            }) => return Err(ToolError::Cancelled),
            Err(error) => return Ok(failure(&error)),
        };
        let saved = match path {
            Some(path) => {
                // The directory may only now exist; resolve again so a
                // link planted there is caught before writing.
                let path = matching_extension(workspace_path(cx.workdir(), path)?, image.mime);
                let saved =
                    super::images::save(cx.workdir(), Some(&path), image.mime, &image.bytes)
                        .map_err(ToolError::Other)?;
                Some(saved)
            }
            None => None,
        };
        let mut text = format!(
            "Generated an image ({}, {}). It comes with this result, and the user sees it.",
            image.mime,
            kage_core::image::human_bytes(image.bytes.len())
        );
        if let Some(saved) = &saved {
            // The save went through the resolved workspace path, which
            // may differ from the workdir as given (`/private/var` on
            // macOS, `\\?\` on Windows).
            let root = cx
                .workdir()
                .canonicalize()
                .unwrap_or_else(|_| cx.workdir().to_owned());
            let shown = saved.strip_prefix(&root).unwrap_or(saved);
            let _ = write!(text, " Saved as {}.", shown.display());
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

/// `path` as an absolute path inside `workdir`, refusing one that leaves
/// it: through `..`, a root, a drive, a `:` (a Windows stream or drive),
/// or a symlink.
fn workspace_path(workdir: &Path, path: &str) -> Result<PathBuf, ToolError> {
    let refuse = |reason: &str| ToolError::Path {
        path: PathBuf::from(path),
        reason: reason.to_owned(),
    };
    if path.contains(':') || Path::new(path).is_absolute() {
        return Err(refuse("must be a path inside the workspace"));
    }
    if Path::new(path)
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(refuse("must not leave the workspace"));
    }
    kage_tools::resolve_under(workdir, Path::new(path))
}

/// `path` with an extension naming `mime`'s type. One that already
/// does (`jpg` and `jpeg` alike, in any case) is kept, and so is any
/// path when the type has no known extension.
fn matching_extension(path: PathBuf, mime: &str) -> PathBuf {
    let Some(want) = kage_core::image::extension_for(mime) else {
        return path;
    };
    let has = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    let same = has
        .as_deref()
        .is_some_and(|ext| ext == want || want == "jpg" && ext == "jpeg");
    if same {
        path
    } else {
        path.with_extension(want)
    }
}

/// Whether `size` reads `WIDTHxHEIGHT` with whole numbers.
fn is_size(size: &str) -> bool {
    size.split_once('x').is_some_and(|(w, h)| {
        !w.is_empty()
            && !h.is_empty()
            && w.bytes().all(|b| b.is_ascii_digit())
            && h.bytes().all(|b| b.is_ascii_digit())
    })
}

/// A failed generation, said so the model knows whether to try again.
fn failure(error: &ImageError) -> ToolOutput {
    let cause = match &error.error {
        ProviderError::Http { status, body } if body.is_empty() => {
            format!("the server answered {status}")
        }
        ProviderError::Http { status, body } => format!("the server answered {status}: {body}"),
        ProviderError::RateLimited { body, .. } => format!(
            "the server is rate limiting: {}",
            body.as_deref().unwrap_or("too many requests")
        ),
        ProviderError::Auth(detail) => format!("the server refused the key: {detail}"),
        ProviderError::Transport(detail) | ProviderError::Decode(detail) => detail.clone(),
        other => other.to_string(),
    };
    // A server's message often ends in a period of its own.
    let cause = cause.trim_end();
    let cause = cause.strip_suffix('.').unwrap_or(cause);
    let advice = if error.error.is_transient() {
        match error.retry_after {
            Some(wait) => format!(
                "A retry may succeed; the server asks to wait {}s first.",
                wait.as_secs().max(1)
            ),
            None => "A retry may succeed.".to_owned(),
        }
    } else if matches!(error.error, ProviderError::Auth(_)) {
        "Retrying will fail the same way: the key needs fixing, so tell the user.".to_owned()
    } else {
        "Retrying the same request will fail the same way; change the prompt or size \
         first, or tell the user."
            .to_owned()
    };
    ToolOutput {
        is_error: true,
        text: format!("Image generation failed: {cause}.\n{advice}"),
        structured: None,
        terminate: false,
        media: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    use kage_core::CancelFlag;
    use kage_core::config::GenerateImageConfig;

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
            endpoint: ImageEndpoint {
                base_url: base_url.to_owned(),
                ..ImageEndpoint::default()
            },
            model: "image".to_owned(),
            timeout: Duration::from_secs(10),
        }
    }

    fn config(cfg: GenerateImageConfig) -> Config {
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
    fn the_extension_follows_the_image_type() {
        let ext = |path: &str, mime| matching_extension(PathBuf::from(path), mime);
        assert_eq!(ext("a/x.png", "image/jpeg"), PathBuf::from("a/x.jpg"));
        assert_eq!(ext("x.jpeg", "image/jpeg"), PathBuf::from("x.jpeg"));
        assert_eq!(ext("x.JPG", "image/jpeg"), PathBuf::from("x.JPG"));
        assert_eq!(ext("x", "image/webp"), PathBuf::from("x.webp"));
        assert_eq!(ext("x.bmp", "image/bmp"), PathBuf::from("x.bmp"));
    }

    #[test]
    fn a_path_outside_the_workspace_costs_no_generation() {
        let (url, hits) = image_server();
        let workdir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        for path in ["../out.png", "/tmp/out.png", "art/x.png:stream"] {
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

        let named = from(GenerateImageConfig {
            base_url: Some("https://images.example/v1".into()),
            model: Some("image".into()),
            ..GenerateImageConfig::default()
        })
        .unwrap();
        assert_eq!(named.endpoint.base_url, "https://images.example/v1");
        assert!(named.endpoint.api_key.is_empty());
        assert_eq!(named.timeout, DEFAULT_TIMEOUT);

        let cases: [(GenerateImageConfig, &str); 4] = [
            (
                GenerateImageConfig {
                    base_url: Some("https://images.example/v1".into()),
                    ..GenerateImageConfig::default()
                },
                "names no `model`",
            ),
            (
                GenerateImageConfig {
                    model: Some("image".into()),
                    ..GenerateImageConfig::default()
                },
                "names neither",
            ),
            (
                GenerateImageConfig {
                    provider: Some("anthropic".into()),
                    model: Some("image".into()),
                    ..GenerateImageConfig::default()
                },
                "cannot serve images",
            ),
            (
                GenerateImageConfig {
                    base_url: Some("https://images.example/v1".into()),
                    model: Some("image".into()),
                    timeout_secs: Some(0),
                    ..GenerateImageConfig::default()
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
        let unset_env = || GenerateImageConfig {
            base_url: Some("https://images.example/v1".into()),
            api_key_env: Some("KAGE_TEST_IMAGE_KEY_NEVER_SET".into()),
            model: Some("image".into()),
            ..GenerateImageConfig::default()
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

    #[test]
    fn an_unset_table_registers_nothing() {
        let tools = kage_tools::builtin_registry().with_image_generation(&Config::default());
        assert!(tools.get(GENERATE_IMAGE_TOOL).is_none());
    }
}
