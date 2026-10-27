//! Endpoint resolution, registration and output helpers for media tools.
//!
//! Each tool supplies its table and name. Keys are looked up once at
//! registration, and saved files stay inside the workspace.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kage_core::ToolOutput;
use kage_core::config::{Config, CustomProviderKind, MediaToolConfig};
use kage_provider::ProviderError;
use kage_provider::compat::{COMPAT_PROVIDERS, CompatKind};
use kage_provider::media::{MediaEndpoint, MediaError};
use kage_tools::{ToolError, ToolRegistry};

use super::image_tool::{GENERATE_IMAGE_TOOL, GenerateImageTool};
use crate::auth::{self, AuthStore};

/// How long one media call may take when the config says nothing.
/// Image servers commonly answer in 10 to 40 seconds, and some give up
/// on their own deadline (150 seconds on one) before answering 503, so
/// the client waits past that to hear the server's answer.
pub(super) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

/// The longest `timeout_secs` accepted: an hour, past any image
/// server's own deadline.
const MAX_TIMEOUT_SECS: u64 = 3600;

/// Registers media tools whose configuration tables resolve.
pub(crate) trait WithMediaTools {
    /// `self` with each configured media tool registered when its table
    /// resolves to an endpoint and a model.
    /// An unset table changes nothing. One that does not resolve is
    /// reported on stderr and leaves the tool out, so a broken table
    /// costs one tool rather than the session.
    #[must_use]
    fn with_media_tools(self, config: &Config) -> Self;
}

impl WithMediaTools for ToolRegistry {
    fn with_media_tools(mut self, config: &Config) -> Self {
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

/// The nonempty model named by a media tool's table.
pub(super) fn model(cfg: &MediaToolConfig) -> Result<String, String> {
    cfg.model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "names no `model`".to_owned())
}

/// The configured whole-call timeout, or its default.
pub(super) fn timeout(cfg: &MediaToolConfig) -> Result<Duration, String> {
    match cfg.timeout_secs {
        None => Ok(DEFAULT_TIMEOUT),
        Some(secs @ 1..=MAX_TIMEOUT_SECS) => Ok(Duration::from_secs(secs)),
        Some(_) => Err(format!("timeout_secs must be from 1 to {MAX_TIMEOUT_SECS}")),
    }
}

/// The endpoint `cfg` names: its own `base_url`, with `headers` and the
/// key in `api_key_env` or saved under the tool's name, or else the
/// base URL, key and headers kage reaches `provider` with.
pub(super) fn endpoint(
    name: &str,
    cfg: &MediaToolConfig,
    config: &Config,
    store: &AuthStore,
) -> Result<MediaEndpoint, String> {
    if let Some(base_url) = cfg.base_url.as_deref().filter(|url| !url.trim().is_empty()) {
        let api_key = match cfg.api_key_env.as_deref().filter(|env| !env.is_empty()) {
            None => store.access_token(name).unwrap_or_default().to_owned(),
            Some(env) => {
                crate::providers::lookup_key_with_env(name, env, store).ok_or_else(|| {
                    format!(
                        "names api_key_env = \"{env}\", which is not set, and no key is saved \
                         with `kage auth login {name}`"
                    )
                })?
            }
        };
        return Ok(MediaEndpoint {
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
) -> Result<MediaEndpoint, String> {
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
        return Ok(MediaEndpoint {
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
                    "provider `{id}` speaks the Anthropic API, which has no OpenAI media routes"
                ));
            }
            None => {
                return Err(format!(
                    "provider `{id}` cannot serve this tool: it calls an OpenAI API, which kage reaches \
                     through `openai`, an OpenAI-compatible \
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
    Ok(MediaEndpoint {
        base_url: ov
            .and_then(|o| o.base_url.clone())
            .unwrap_or_else(|| default_base.to_owned()),
        api_key,
        headers: ov.map(|o| o.headers.clone()).unwrap_or_default(),
    })
}

/// `path` as an absolute path inside `workdir`, refusing one that leaves
/// it: through `..`, a root, a drive, a `:` (a Windows stream or drive),
/// or a symlink.
pub(super) fn workspace_path(workdir: &Path, path: &str) -> Result<PathBuf, ToolError> {
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
pub(super) fn matching_extension(path: PathBuf, mime: &str) -> PathBuf {
    let Some(want) = extension_for(mime) else {
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
pub(super) fn is_size(size: &str) -> bool {
    size.split_once('x').is_some_and(|(w, h)| {
        !w.is_empty()
            && !h.is_empty()
            && w.bytes().all(|b| b.is_ascii_digit())
            && h.bytes().all(|b| b.is_ascii_digit())
    })
}

/// A failed generation, said so the model knows whether to try again.
pub(super) fn failure(error: &MediaError) -> ToolOutput {
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

/// The extension of a recognized media type.
pub(super) fn extension_for(mime: &str) -> Option<&'static str> {
    kage_core::image::extension_for(mime)
}

/// A saved path relative to the canonical workspace, when it is inside.
/// Resolving the root also handles platform prefixes added by canonicalization.
pub(super) fn shown_path<'a>(workdir: &Path, saved: &'a Path) -> &'a Path {
    let root = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_owned());
    saved.strip_prefix(&root).unwrap_or(saved)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn an_unset_table_registers_nothing() {
        let tools = kage_tools::builtin_registry().with_media_tools(&Config::default());
        assert!(tools.get(GENERATE_IMAGE_TOOL).is_none());
    }
}
