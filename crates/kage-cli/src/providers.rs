//! The provider registry and the default model.

use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::Arc;

use kage_provider::{ProviderRegistry, anthropic, compat, gemini, openai, openai_responses};
use serde::Serialize;

use crate::{acp_glue, auth, chatgpt, state};

/// A provider kage knows by id, and which config tables may name it.
/// `kage providers` lists these, and `[providers]` validation reads
/// the same list, so what kage reports is what it accepts.
#[derive(Debug, Serialize)]
pub(crate) struct KnownProvider {
    /// The id, the `provider` in `provider/model`.
    pub(crate) id: &'static str,
    /// The wire protocol: `openai`, `anthropic` or `gemini` as a
    /// custom provider's `kind` names them, `openai-responses`, or
    /// `acp`.
    pub(crate) api: &'static str,
    /// Whether a `[providers.<id>]` override may target it.
    #[serde(rename = "override")]
    pub(crate) overridable: bool,
    /// Whether a `[providers.custom.<id>]` may take the id, replacing
    /// the provider. Builtins refuse it.
    pub(crate) custom: bool,
}

/// Reads an environment variable, so tests need not set one.
type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// A builtin provider: the [`KnownProvider`] fields `kage providers`
/// lists for it, and how it registers.
struct Builtin {
    id: &'static str,
    api: &'static str,
    overridable: bool,
    register: fn(&kage_core::config::Config, &auth::AuthStore, EnvLookup, &mut ProviderRegistry),
}

/// Every builtin, the one list both [`known_providers`] and
/// [`register_providers`] read. `acp` cannot be overridden: its
/// configuration lives under `[acp.*]`.
const BUILTINS: &[Builtin] = &[
    Builtin {
        id: "acp",
        api: "acp",
        overridable: false,
        register: register_acp,
    },
    Builtin {
        id: "anthropic",
        api: "anthropic",
        overridable: true,
        register: register_anthropic,
    },
    Builtin {
        id: "google",
        api: "gemini",
        overridable: true,
        register: register_google,
    },
    Builtin {
        id: "openai",
        api: "openai-responses",
        overridable: true,
        register: register_openai,
    },
];

/// Every provider kage registers by id: the builtins, then each
/// catalog entry.
pub(crate) fn known_providers() -> Vec<KnownProvider> {
    let builtins = BUILTINS.iter().map(|b| KnownProvider {
        id: b.id,
        api: b.api,
        overridable: b.overridable,
        custom: false,
    });
    let catalog = compat::COMPAT_PROVIDERS.iter().map(|entry| KnownProvider {
        id: entry.id,
        api: match entry.kind {
            compat::CompatKind::OpenAi => "openai",
            compat::CompatKind::Anthropic => "anthropic",
        },
        overridable: true,
        custom: true,
    });
    builtins.chain(catalog).collect()
}

/// Implement `kage providers`: list [`known_providers`] as a table,
/// or with `json` as one JSON object for scripts. A reader that closes
/// the pipe early, such as `head`, is not an error.
pub(crate) fn run_list(json: bool) -> ExitCode {
    match write_list(&mut io::stdout().lock(), &known_providers(), json) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: providers: {e}");
            ExitCode::from(1)
        }
    }
}

/// Render the `kage providers` output, sizing the id column to the
/// longest id.
fn write_list<W: Write>(out: &mut W, providers: &[KnownProvider], json: bool) -> io::Result<()> {
    if json {
        serde_json::to_writer(&mut *out, &serde_json::json!({ "providers": providers }))?;
        return writeln!(out);
    }
    let width = providers
        .iter()
        .map(|p| p.id.len())
        .chain(["PROVIDER".len()])
        .max()
        .unwrap_or_default();
    let api_width = providers
        .iter()
        .map(|p| p.api.len())
        .chain(["API".len()])
        .max()
        .unwrap_or_default();
    writeln!(
        out,
        "{:<width$}  {:<api_width$}  OVERRIDE  CUSTOM",
        "PROVIDER", "API"
    )?;
    let yes_no = |flag: bool| if flag { "yes" } else { "no" };
    for p in providers {
        writeln!(
            out,
            "{:<width$}  {:<api_width$}  {:<8}  {}",
            p.id,
            p.api,
            yes_no(p.overridable),
            yes_no(p.custom)
        )?;
    }
    Ok(())
}

/// Checks the `[providers]` cross-references serde cannot: custom ids
/// that shadow a registered provider, overrides of unknown ones and bad
/// model prices.
///
/// # Errors
///
/// The first problem found, ready for a `kage: ` line.
pub(crate) fn validate_providers(
    providers: &kage_core::config::ProvidersConfig,
) -> Result<(), String> {
    let known = known_providers();
    let builtin: Vec<&str> = known.iter().filter(|p| !p.custom).map(|p| p.id).collect();
    let overridable: Vec<&str> = known
        .iter()
        .filter(|p| p.overridable)
        .map(|p| p.id)
        .collect();
    providers
        .validate(&builtin, &overridable)
        .map_err(|e| e.to_string())
}

/// Build a registry holding every configured provider: builtins and
/// catalog entries whose API key is reachable through either an env var
/// (priority) or the saved auth store, with `[providers.<id>]`
/// overrides for base URL and extra headers, plus every custom
/// provider declared under `[providers.custom.*]`.
///
/// # Errors
///
/// The user config does not load or fails `[providers]` validation.
/// Callers stop rather than run against a subset of the declared
/// providers.
///
/// A custom provider that replaces a catalog provider is reported on
/// the first build only, so a rebuild inside the TUI does not print
/// over the screen.
pub(crate) fn build_provider_registry() -> Result<ProviderRegistry, String> {
    static WARN_REPLACED: std::sync::Once = std::sync::Once::new();
    let config = kage_core::config::Config::load_default().map_err(|e| e.to_string())?;
    validate_providers(&config.providers)?;
    let mut store = auth::AuthStore::load().unwrap_or_else(|_| auth::AuthStore::empty());
    // A ChatGPT login close to expiry renews before the registry
    // builds, so requests start with a token that lasts. A failed
    // renewal stays silent here: the 401 path surfaces it when a
    // token is actually needed.
    if let Ok(path) = auth::AuthStore::default_path() {
        let _ = chatgpt::ensure_fresh(&mut store, &path, chrono::Utc::now());
    }
    let (registry, replaced) = register_providers(&config, &store, &env_key);
    WARN_REPLACED.call_once(|| {
        for id in &replaced {
            eprintln!(
                "kage: [providers.custom.{id}] replaces the catalog provider `{id}` and its models"
            );
        }
    });
    Ok(registry)
}

/// Register every provider `config` and `store` make available, and
/// return the registry with the catalog ids a custom provider replaced.
/// `env` reads the builtins' environment variables.
fn register_providers(
    config: &kage_core::config::Config,
    store: &auth::AuthStore,
    env: EnvLookup,
) -> (ProviderRegistry, Vec<String>) {
    let mut registry = ProviderRegistry::new();
    for builtin in BUILTINS {
        (builtin.register)(config, store, env, &mut registry);
    }
    register_compat_providers(config, store, &mut registry);
    let replaced = register_custom_providers(config, store, &mut registry);
    (registry, replaced)
}

/// Register the built-in `anthropic` provider when its key is set.
fn register_anthropic(
    config: &kage_core::config::Config,
    store: &auth::AuthStore,
    env: EnvLookup,
    registry: &mut ProviderRegistry,
) {
    let ov = config.providers.overrides.get("anthropic");
    let env_name = ov
        .and_then(|o| o.api_key_env.as_deref())
        .unwrap_or_else(|| auth::env_var_for("anthropic"));
    if let Some(key) = key_from("anthropic", env_name, store, env) {
        let mut provider = match ov.and_then(|o| o.base_url.clone()) {
            Some(base) => anthropic::AnthropicProvider::with_base_url(key, base),
            None => anthropic::AnthropicProvider::new(key),
        };
        if let Some(o) = ov
            && !o.headers.is_empty()
        {
            provider = provider.with_extra_headers(o.headers.clone());
        }
        registry.register(Arc::new(provider));
    }
}

/// Register the built-in `google` provider when its key is set.
fn register_google(
    config: &kage_core::config::Config,
    store: &auth::AuthStore,
    env: EnvLookup,
    registry: &mut ProviderRegistry,
) {
    let ov = config.providers.overrides.get("google");
    let env_name = ov
        .and_then(|o| o.api_key_env.as_deref())
        .unwrap_or_else(|| auth::env_var_for("google"));
    if let Some(key) = key_from("google", env_name, store, env) {
        let mut provider = match ov.and_then(|o| o.base_url.clone()) {
            Some(base) => gemini::GeminiProvider::with_base_url(key, base),
            None => gemini::GeminiProvider::new(key),
        };
        if let Some(o) = ov
            && !o.headers.is_empty()
        {
            provider = provider.with_extra_headers(o.headers.clone());
        }
        registry.register(Arc::new(provider));
    }
}

/// Register the `acp` provider: `kage -m acp:<name>` drives an external
/// ACP agent declared in `[acp.agents.*]` or via `kage.acp.add_agent`.
/// It needs no key and always registers, since plugin-declared agents
/// resolve lazily. Its permission resolver defers to
/// `kage.on_acp_permission` and denies otherwise.
fn register_acp(
    config: &kage_core::config::Config,
    _store: &auth::AuthStore,
    _env: EnvLookup,
    registry: &mut ProviderRegistry,
) {
    registry.register(Arc::new(
        kage_acp::client::AcpProvider::from_config(&config.acp)
            .with_permission(acp_glue::permission_resolver())
            .with_agent_source(acp_glue::agent_source()),
    ));
}

/// Register the built-in `openai` provider on the Responses API. An
/// exported or saved API key serves it from the public API; otherwise a
/// `ChatGPT` login (an OAuth record under `openai`) serves it from the
/// `ChatGPT` backend. `env` reads an environment variable, so tests
/// need not set one.
fn register_openai(
    config: &kage_core::config::Config,
    store: &auth::AuthStore,
    env: EnvLookup,
    registry: &mut ProviderRegistry,
) {
    let ov = config.providers.overrides.get("openai");
    let env_name = ov
        .and_then(|o| o.api_key_env.as_deref())
        .unwrap_or_else(|| auth::env_var_for("openai"));
    // An exported key wins over a stored login, matching the
    // env-first lookup everywhere else.
    let stored_key = match store.credential("openai") {
        Some(auth::Credential::ApiKey { key }) => Some(key.clone()),
        _ => None,
    };
    if let Some(key) = env(env_name).or(stored_key) {
        let mut provider = match ov.and_then(|o| o.base_url.clone()) {
            Some(base) => openai_responses::OpenAiResponsesProvider::with_base_url(key, base),
            None => openai_responses::OpenAiResponsesProvider::new(key),
        };
        if let Some(o) = ov {
            provider = provider
                .with_image_generation(o.image_generation)
                .with_extra_headers(o.headers.clone());
        }
        registry.register(Arc::new(provider));
        return;
    }
    if let Some(auth::Credential::Oauth(oauth)) = store.credential("openai")
        && let Ok(path) = auth::AuthStore::default_path()
    {
        register_chatgpt(config, oauth, path, registry);
    }
}

/// The non-empty value of environment variable `name`, if set; `None`
/// for an empty name.
fn env_key(name: &str) -> Option<String> {
    (!name.is_empty())
        .then(|| std::env::var(name).ok())
        .flatten()
        .filter(|v| !v.is_empty())
}

/// `openai` on the `ChatGPT` backend with a renewable
/// [`chatgpt::ChatGptBearer`] over the auth store at `path`, in the
/// backend's request shape, offering the plan's cached models. Override
/// headers merge after the backend's own, so a proxy can add to them.
/// Bodies are zstd-compressed only on the backend itself, as the Codex
/// CLI does, since a proxy in between may not decode them.
fn register_chatgpt(
    config: &kage_core::config::Config,
    oauth: &auth::OAuthCredential,
    path: std::path::PathBuf,
    registry: &mut ProviderRegistry,
) {
    let ov = config.providers.overrides.get("openai");
    let mut headers = chatgpt::backend_headers(oauth.account_id.as_deref());
    if let Some(o) = ov {
        for (name, value) in &o.headers {
            headers.insert(name.clone(), value.clone());
        }
    }
    let mut provider = match ov.and_then(|o| o.base_url.clone()) {
        Some(base) => openai_responses::OpenAiResponsesProvider::with_base_url("", base),
        None => {
            openai_responses::OpenAiResponsesProvider::with_base_url("", chatgpt::BACKEND_BASE_URL)
                .with_zstd_bodies()
        }
    }
    .with_bearer(Arc::new(chatgpt::ChatGptBearer::new(oauth, path)))
    .with_extra_headers(headers)
    .with_models(
        chatgpt::models_cache_path()
            .map(|cache| chatgpt::cached_models(&cache))
            .unwrap_or_default(),
    )
    .for_chatgpt_backend();
    if let Some(o) = ov {
        provider = provider.with_image_generation(o.image_generation);
    }
    registry.register(Arc::new(provider));
}

/// Register each OpenAI-compatible catalog entry the user has a key
/// for. Every entry is described once in `compat::COMPAT_PROVIDERS`;
/// adding a provider is a single table entry there.
fn register_compat_providers(
    config: &kage_core::config::Config,
    store: &auth::AuthStore,
    registry: &mut ProviderRegistry,
) {
    for entry in compat::COMPAT_PROVIDERS {
        let ov = config.providers.overrides.get(entry.id);
        let env = ov
            .and_then(|o| o.api_key_env.as_deref())
            .unwrap_or_else(|| auth::env_var_for(entry.id));
        if let Some(key) = lookup_key_with_env(entry.id, env, store) {
            let headers = ov.map(|o| o.headers.clone()).unwrap_or_default();
            let provider = match ov.and_then(|o| o.base_url.clone()) {
                Some(base) => entry.build_with_base_url(key, base, headers),
                None => entry.build(key, headers),
            };
            registry.register(provider);
        }
    }
}

/// Register every custom provider declared under
/// `[providers.custom.<id>]`. A provider with an explicitly empty
/// `api_key_env` needs no key at all (local gateways); any other
/// missing key skips registration. A custom provider that reuses a
/// catalog provider id replaces it. Returns the ids replaced that way.
fn register_custom_providers(
    config: &kage_core::config::Config,
    store: &auth::AuthStore,
    registry: &mut ProviderRegistry,
) -> Vec<String> {
    let mut replaced = Vec::new();
    for (id, cfg) in &config.providers.custom {
        let env = cfg
            .api_key_env
            .clone()
            .unwrap_or_else(|| format!("{}_API_KEY", id.to_uppercase()));
        let key = if env.is_empty() {
            String::new()
        } else {
            match lookup_key_with_env(id, &env, store) {
                Some(key) => key,
                None => continue,
            }
        };
        let metadata = kage_provider::ProviderMetadata {
            id: id.clone(),
            display_name: cfg.display_name.clone().unwrap_or_else(|| id.clone()),
            supports_caching: cfg.caching,
            supports_thinking: cfg.thinking,
            supports_tool_use: cfg.tool_use,
        };
        let models: Vec<kage_provider::ProviderModel> = cfg
            .models
            .iter()
            .map(|m| kage_provider::ProviderModel {
                id: m.id.clone(),
                name: m.name.clone(),
                context: m.context,
                max_output: m.max_output,
                reasoning: m.reasoning(),
                input: m.input,
                produces: m.produces,
                interleaved: m.interleaved,
                cost: m.cost,
            })
            .collect();
        let provider: Arc<dyn kage_provider::Provider> = match cfg.kind {
            kage_core::config::CustomProviderKind::OpenAi => Arc::new(
                openai::OpenAiProvider::compatible(key, cfg.base_url.clone(), metadata)
                    .with_extra_headers(cfg.headers.clone())
                    .with_models(models),
            ),
            kage_core::config::CustomProviderKind::Anthropic => Arc::new(
                anthropic::AnthropicProvider::with_base_url(key, cfg.base_url.clone())
                    .with_metadata(metadata)
                    .with_extra_headers(cfg.headers.clone())
                    .with_models(models)
                    .with_input_including_cache(cfg.input_includes_cache),
            ),
            kage_core::config::CustomProviderKind::Gemini => Arc::new(
                gemini::GeminiProvider::with_base_url(key, cfg.base_url.clone())
                    .with_metadata(metadata)
                    .with_extra_headers(cfg.headers.clone())
                    .with_models(models),
            ),
        };
        if is_catalog_provider(id) {
            replaced.push(id.clone());
        }
        registry.register(provider);
    }
    replaced
}

/// Whether `id` names a provider kage knows from its catalog or its
/// OpenAI-compatible table.
fn is_catalog_provider(id: &str) -> bool {
    kage_provider::catalog::provider(id).is_some()
        || compat::COMPAT_PROVIDERS.iter().any(|entry| entry.id == id)
}

/// Look up `provider`'s bearer credential from `env_var` (when
/// non-empty and set), falling back to the stored credential its public
/// API accepts ([`auth::AuthStore::api_credential`], so a `ChatGPT`
/// login never stands in for an `OpenAI` API key). Returns the API key
/// string for [`auth::Credential::ApiKey`] entries and the access token
/// for [`auth::Credential::Oauth`] entries. `env_var` defaults to
/// [`auth::env_var_for`]'s name for the provider; `[providers.<id>]`
/// `api_key_env` overrides can redirect the lookup.
pub(crate) fn lookup_key_with_env(
    provider: &str,
    env_var: &str,
    store: &auth::AuthStore,
) -> Option<String> {
    key_from(provider, env_var, store, &env_key)
}

/// [`lookup_key_with_env`] with `env` reading the environment variable.
fn key_from(
    provider: &str,
    env_var: &str,
    store: &auth::AuthStore,
    env: EnvLookup,
) -> Option<String> {
    env(env_var).or_else(|| {
        store
            .api_credential(provider)
            .map(|c| c.raw_token().to_owned())
    })
}

/// Printed when no provider other than `acp` is registered and the
/// requested model does not resolve.
pub(crate) const NO_CREDENTIALS_MESSAGE: &str = "kage: no provider credentials found. \
    Run `kage auth login` to save one, or export one of ANTHROPIC_API_KEY, \
    OPENAI_API_KEY, GEMINI_API_KEY, ZAI_API_KEY, ZAI_CODING_API_KEY, DEEPSEEK_API_KEY, \
    GROQ_API_KEY, MISTRAL_API_KEY, CEREBRAS_API_KEY, XAI_API_KEY, OPENROUTER_API_KEY, \
    FIREWORKS_API_KEY, MOONSHOT_API_KEY, KIMI_API_KEY, XIAOMI_API_KEY, CMD_API_KEY, \
    KENARI_API_KEY or OPENCODE_API_KEY.";

/// Printed when no model was requested and none could be picked.
pub(crate) const NO_MODEL_MESSAGE: &str =
    "kage: no model configured. Set [provider] default_model or pass -m provider/model";

/// Whether any provider other than the always-registered `acp` provider
/// is available, meaning some credential or custom provider is wired up.
pub(crate) fn has_usable_provider(registry: &ProviderRegistry) -> bool {
    registry.ids().any(|id| id != "acp")
}

/// Pick a sensible default model. A configured `[provider] default_model`
/// that still resolves (its provider has credentials) wins; otherwise the
/// last model the user successfully ran (when it still resolves), then
/// [`fallback_model`]. Returns an empty string when nothing is wired up.
pub(crate) fn default_model(registry: &ProviderRegistry) -> String {
    if let Ok(cfg) = kage_core::config::Config::load_default()
        && registry.resolve(&cfg.provider.default_model).is_ok()
    {
        return kage_core::canonical_model(&cfg.provider.default_model);
    }
    if let Some(model) = state::State::load().last_model
        && registry.resolve(&model).is_ok()
    {
        return kage_core::canonical_model(&model);
    }
    fallback_model(registry)
}

/// The model to default `provider` to: its first declared model, else
/// the catalog's preferred model for its id. `None` when the provider
/// declares nothing and the catalog does not list it.
fn provider_default_model(provider: &dyn kage_provider::Provider) -> Option<String> {
    let id = &provider.metadata().id;
    provider
        .models()
        .into_iter()
        .next()
        .map(|m| m.id)
        .or_else(|| kage_provider::catalog::preferred_model(id).map(|m| m.id.to_owned()))
}

/// Every registered provider except `acp`, most preferred first:
/// [`SUPPORTED_PROVIDERS`] order, then any custom or plugin provider by
/// id.
fn ordered_provider_ids(registry: &ProviderRegistry) -> Vec<&str> {
    let mut ordered: Vec<&str> = kage_provider::catalog::source::SUPPORTED_PROVIDERS
        .iter()
        .map(|map| map.kage_id)
        .filter(|id| *id != "acp" && registry.get(id).is_some())
        .collect();
    let mut rest: Vec<&str> = registry
        .ids()
        .filter(|id| {
            *id != "acp"
                && !kage_provider::catalog::source::SUPPORTED_PROVIDERS
                    .iter()
                    .any(|map| map.kage_id == *id)
        })
        .collect();
    rest.sort_unstable();
    ordered.extend(rest);
    ordered
}

/// The model to fall back on when nothing is configured: the first
/// provider in [`ordered_provider_ids`] that has one. Empty when none
/// does.
fn fallback_model(registry: &ProviderRegistry) -> String {
    for id in ordered_provider_ids(registry) {
        if let Some(model) = registry
            .get(id)
            .and_then(|p| provider_default_model(p.as_ref()))
        {
            return kage_core::qualify_model(id, &model);
        }
    }
    String::new()
}

/// The `[provider] default_model` the user set explicitly, through
/// `KAGE_PROVIDER__DEFAULT_MODEL` or the user config file. `None` when
/// it was left at its default or the config does not load.
pub(crate) fn configured_default_model() -> Option<String> {
    let explicit = std::env::var_os("KAGE_PROVIDER__DEFAULT_MODEL").is_some()
        || kage_core::config::Config::default_path()
            .is_some_and(|path| config_sets_default_model(&path));
    if !explicit {
        return None;
    }
    kage_core::config::Config::load_default()
        .ok()
        .map(|config| kage_core::canonical_model(&config.provider.default_model))
}

/// Whether the TOML file at `path` sets `[provider] default_model`.
fn config_sets_default_model(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .is_some_and(|table| {
            table
                .get("provider")
                .and_then(toml::Value::as_table)
                .is_some_and(|provider| provider.contains_key("default_model"))
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A server that answers one streaming request and hands back its
    /// request head. The body is read in full before the answer, since
    /// Windows resets a connection closed with unread data.
    fn capture_one() -> (std::net::SocketAddr, std::thread::JoinHandle<String>) {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            let head_end = loop {
                if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
                let n = conn.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
            };
            let head = String::from_utf8_lossy(&raw[..head_end]).to_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |value| value.trim().parse().unwrap());
            while raw.len() < head_end + length {
                let n = conn.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
            }
            conn.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: [DONE]\n\n",
            )
            .unwrap();
            String::from_utf8_lossy(&raw).to_lowercase()
        });
        (addr, server)
    }

    /// Stream one turn through `model` of `registry`.
    fn stream_once(registry: &ProviderRegistry, model: &str) {
        let resolved = registry.resolve(model).unwrap();
        let req = kage_provider::StreamRequest::new(
            resolved.model.clone(),
            vec![Arc::new(kage_core::Message::new(
                kage_core::Role::User,
                vec![kage_core::Content::Text { text: "hi".into() }],
                None,
            ))],
        );
        let events: Vec<_> = resolved
            .provider
            .stream(req, &kage_core::CancelFlag::new())
            .unwrap()
            .collect();
        assert!(events.iter().all(Result::is_ok), "{events:?}");
    }

    /// Whether `[providers]` validation accepts `toml`.
    fn accepts(toml: &str) -> bool {
        let config: kage_core::config::Config = toml::from_str(toml).unwrap();
        validate_providers(&config.providers).is_ok()
    }

    /// A custom provider table for `id` that is complete apart from
    /// its id, so only the id decides whether it is accepted.
    fn custom_table(id: &str) -> String {
        format!(
            "[providers.custom.{id}]\nbase_url = \"http://127.0.0.1:1/v1\"\n\n\
             [[providers.custom.{id}.models]]\nid = \"m\"\nname = \"m\"\n"
        )
    }

    /// An id `kage providers` does not list takes a custom table and
    /// no override, which is what lets a script treat the listing as
    /// complete.
    #[test]
    fn an_unlisted_id_takes_only_a_custom_table() {
        assert!(known_providers().iter().all(|p| p.id != "unlisted-gw"));
        assert!(!accepts("[providers.unlisted-gw]\n"));
        assert!(accepts(&custom_table("unlisted-gw")));
    }

    /// The JSON form is one object whose entries carry the four
    /// fields scripts read, under these names.
    #[test]
    fn the_json_listing_names_each_field() {
        let mut out = Vec::new();
        write_list(&mut out, &known_providers(), true).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.ends_with("}\n"), "{text}");
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        let providers = parsed["providers"].as_array().unwrap();
        let openai = providers.iter().find(|p| p["id"] == "openai").unwrap();
        assert_eq!(
            openai,
            &serde_json::json!({
                "id": "openai",
                "api": "openai-responses",
                "override": true,
                "custom": false,
            })
        );
        let kenari = providers
            .iter()
            .find(|p| p["id"] == "kenari-claude")
            .unwrap();
        assert_eq!(kenari["api"], "anthropic");
    }

    /// With every key set, registration builds exactly the builtins
    /// `kage providers` lists, so a builtin registered outside
    /// [`BUILTINS`] fails here.
    #[test]
    fn the_listed_builtins_are_the_registered_ones() {
        let known = known_providers();
        let mut listed: Vec<&str> = known.iter().filter(|p| !p.custom).map(|p| p.id).collect();
        listed.sort_unstable();
        let env = |_: &str| Some("fake-key".to_owned());
        let (registry, _) = register_providers(
            &kage_core::config::Config::default(),
            &auth::AuthStore::empty(),
            &env,
        );
        let mut registered: Vec<&str> = registry
            .ids()
            .filter(|id| compat::COMPAT_PROVIDERS.iter().all(|e| e.id != *id))
            .collect();
        registered.sort_unstable();
        assert_eq!(registered, listed);
    }

    /// A store holding only a `ChatGPT` login.
    fn chatgpt_store() -> auth::AuthStore {
        let mut store = auth::AuthStore::empty();
        store.set_oauth(
            "openai",
            auth::OAuthCredential {
                access_token: "chatgpt-token".to_owned(),
                refresh_token: Some("refresh".to_owned()),
                expires_at: None,
                scope: None,
                account_id: Some("acc-9".to_owned()),
            },
        );
        store
    }

    /// A `ChatGPT` login registers `openai` pointed at the `ChatGPT`
    /// backend, with the renewable bearer and the backend headers the
    /// public client sends. A proxy base URL gets plain bodies.
    #[test]
    fn a_chatgpt_login_routes_openai_through_the_backend() {
        let (addr, server) = capture_one();
        let config: kage_core::config::Config = toml::from_str(&format!(
            r#"
            [providers.openai]
            base_url = "http://{addr}/backend-api/codex"
            "#
        ))
        .unwrap();
        let mut registry = ProviderRegistry::new();
        register_openai(&config, &chatgpt_store(), &|_| None, &mut registry);
        stream_once(&registry, "openai/gpt-5.2");
        let request = server.join().unwrap();
        assert!(
            request.starts_with("post /backend-api/codex/responses "),
            "{request}"
        );
        for header in [
            "openai-beta: responses=experimental",
            "originator: codex_cli_rs",
            "chatgpt-account-id: acc-9",
            "authorization: bearer chatgpt-token",
        ] {
            assert!(request.contains(&format!("\r\n{header}\r\n")), "{request}");
        }
        assert!(!request.contains("content-encoding"), "{request}");
        assert!(request.ends_with('}'), "the whole body was read: {request}");
    }

    /// Key lookups (the image tool's among them) never hand a `ChatGPT`
    /// token to the public `OpenAI` API.
    #[test]
    fn a_chatgpt_login_is_no_openai_api_key() {
        assert_eq!(lookup_key_with_env("openai", "", &chatgpt_store()), None);
    }

    /// An exported `OpenAI` key wins over a stored `ChatGPT` login and
    /// serves `openai` from the public Responses API.
    #[test]
    fn an_exported_key_wins_over_a_chatgpt_login() {
        let (addr, server) = capture_one();
        let config: kage_core::config::Config = toml::from_str(&format!(
            r#"
            [providers.openai]
            base_url = "http://{addr}/v1"
            "#
        ))
        .unwrap();
        let env = |name: &str| (name == "OPENAI_API_KEY").then(|| "sk-1".to_owned());
        let mut registry = ProviderRegistry::new();
        register_openai(&config, &chatgpt_store(), &env, &mut registry);
        stream_once(&registry, "openai/gpt-5.2");
        let request = server.join().unwrap();
        assert!(request.starts_with("post /v1/responses "), "{request}");
        assert!(
            request.contains("\r\nauthorization: bearer sk-1\r\n"),
            "{request}"
        );
        assert!(!request.contains("chatgpt-account-id"), "{request}");
    }

    #[test]
    fn custom_anthropic_and_gemini_providers_register_under_their_own_ids() {
        let config: kage_core::config::Config = toml::from_str(
            r#"
            [providers.custom.zhipu-anthropic]
            kind = "anthropic"
            base_url = "http://127.0.0.1:1/anthropic"
            api_key_env = ""
            [[providers.custom.zhipu-anthropic.models]]
            id = "glm-5.3"
            name = "GLM-5.3"

            [providers.custom.my-gemini]
            kind = "gemini"
            base_url = "http://127.0.0.1:1/gemini"
            api_key_env = ""
            [[providers.custom.my-gemini.models]]
            id = "g-1"
            name = "G 1"
            "#,
        )
        .unwrap();
        let mut registry = ProviderRegistry::new();
        let replaced = register_custom_providers(&config, &auth::AuthStore::empty(), &mut registry);
        assert!(replaced.is_empty());
        assert!(registry.resolve("zhipu-anthropic/glm-5.3").is_ok());
        assert!(registry.resolve("my-gemini/g-1").is_ok());
        assert!(registry.get("anthropic").is_none());
        assert!(registry.get("google").is_none());
    }

    #[test]
    fn china_coding_plan_override_sends_configured_headers_and_the_shared_key() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = conn.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
            }
            let head = String::from_utf8_lossy(&raw).to_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map_or(0, |v| v.trim().parse().unwrap());
            let body_start = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            while raw.len() < body_start + length {
                let n = conn.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
            }
            conn.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: [DONE]\n\n",
            )
            .unwrap();
            String::from_utf8(raw).unwrap()
        });
        let config: kage_core::config::Config = toml::from_str(&format!(
            r#"
            [providers.zhipuai-coding-plan]
            base_url = "http://{addr}/api/coding/paas/v4"
            api_key_env = ""
            headers = {{ X-Team = "kage-test" }}
            "#
        ))
        .unwrap();
        let mut store = auth::AuthStore::empty();
        store.set_api_key("zai-coding-plan", "fake-shared-key");
        let mut registry = ProviderRegistry::new();
        register_compat_providers(&config, &store, &mut registry);
        let resolved = registry.resolve("zhipuai-coding-plan/glm-5.3").unwrap();
        let req = kage_provider::StreamRequest::new(
            resolved.model.clone(),
            vec![Arc::new(kage_core::Message::new(
                kage_core::Role::User,
                vec![kage_core::Content::Text { text: "hi".into() }],
                None,
            ))],
        );
        let events: Vec<_> = resolved
            .provider
            .stream(req, &kage_core::CancelFlag::new())
            .unwrap()
            .collect();
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let request = server.join().unwrap();
        let lower = request.to_lowercase();
        assert!(
            lower.starts_with("post /api/coding/paas/v4/chat/completions "),
            "{request}"
        );
        assert!(lower.contains("\r\nx-team: kage-test\r\n"), "{request}");
        assert!(lower.contains("\r\nuser-agent: kage/"), "{request}");
        assert!(
            lower.contains("\r\nauthorization: bearer fake-shared-key\r\n"),
            "{request}"
        );
        assert!(request.contains("\"max_tokens\""), "{request}");
    }

    /// A tiny SSE response good enough for the Anthropic stream to
    /// reach a clean end.
    const CLAUDE_SSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\nevent: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    /// Read one request off `listener`, reply with `response`, and
    /// return the raw request bytes.
    fn serve_one(
        listener: std::net::TcpListener,
        response: &'static [u8],
    ) -> std::thread::JoinHandle<String> {
        use std::io::{Read as _, Write as _};
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = conn.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
            }
            let head = String::from_utf8_lossy(&raw).to_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map_or(0, |v| v.trim().parse().unwrap());
            let body_start = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            while raw.len() < body_start + length {
                let n = conn.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
            }
            conn.write_all(response).unwrap();
            String::from_utf8(raw).unwrap()
        })
    }

    fn stream_one_model(registry: &ProviderRegistry, addressed: &str) {
        let resolved = registry.resolve(addressed).unwrap();
        let req = kage_provider::StreamRequest::new(
            resolved.model.clone(),
            vec![Arc::new(kage_core::Message::new(
                kage_core::Role::User,
                vec![kage_core::Content::Text { text: "hi".into() }],
                None,
            ))],
        );
        let events: Vec<_> = resolved
            .provider
            .stream(req, &kage_core::CancelFlag::new())
            .unwrap()
            .collect();
        assert!(events.iter().all(Result::is_ok), "{events:?}");
    }

    #[test]
    fn commandcode_posts_chat_completions_with_the_shared_key() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_one(listener, b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: [DONE]\n\n");
        let config: kage_core::config::Config = toml::from_str(&format!(
            r#"
            [providers.commandcode]
            base_url = "http://{addr}/provider/v1"
            api_key_env = ""
            "#
        ))
        .unwrap();
        let mut store = auth::AuthStore::empty();
        store.set_api_key("commandcode", "cmd-shared-key");
        let mut registry = ProviderRegistry::new();
        register_compat_providers(&config, &store, &mut registry);
        stream_one_model(&registry, "commandcode/gpt-6-astra");
        let request = server.join().unwrap();
        let lower = request.to_lowercase();
        assert!(
            lower.starts_with("post /provider/v1/chat/completions "),
            "{request}"
        );
        assert!(
            lower.contains("\r\nauthorization: bearer cmd-shared-key\r\n"),
            "{request}"
        );
        let compact = request.replace(' ', "");
        assert!(compact.contains("\"model\":\"gpt-6-astra\""), "{request}");
    }

    #[test]
    fn commandcode_claude_posts_messages_with_the_shared_key() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_one(listener, CLAUDE_SSE);
        let config: kage_core::config::Config = toml::from_str(&format!(
            r#"
            [providers.commandcode-claude]
            base_url = "http://{addr}/provider"
            api_key_env = ""
            "#
        ))
        .unwrap();
        // Only the `commandcode` credential exists; the Claude flavor
        // must pick the shared key up through its alias.
        let mut store = auth::AuthStore::empty();
        store.set_api_key("commandcode", "cmd-shared-key");
        let mut registry = ProviderRegistry::new();
        register_compat_providers(&config, &store, &mut registry);
        assert!(registry.get("commandcode").is_some());
        assert!(registry.get("commandcode-claude").is_some());
        stream_one_model(&registry, "commandcode-claude/claude-sonnet-4-6");
        let request = server.join().unwrap();
        let lower = request.to_lowercase();
        assert!(
            lower.starts_with("post /provider/v1/messages "),
            "{request}"
        );
        assert!(
            lower.contains("\r\nx-api-key: cmd-shared-key\r\n"),
            "{request}"
        );
        assert!(lower.contains("\r\nanthropic-version: "), "{request}");
        let compact = request.replace(' ', "");
        assert!(
            compact.contains("\"model\":\"claude-sonnet-4-6\""),
            "{request}"
        );
    }

    #[test]
    fn kenari_claude_posts_messages_with_the_shared_key() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_one(listener, CLAUDE_SSE);
        let config: kage_core::config::Config = toml::from_str(&format!(
            r#"
            [providers.kenari-claude]
            base_url = "http://{addr}"
            api_key_env = ""
            "#
        ))
        .unwrap();
        let mut store = auth::AuthStore::empty();
        store.set_api_key("kenari", "kn-shared-key");
        let mut registry = ProviderRegistry::new();
        register_compat_providers(&config, &store, &mut registry);
        assert!(registry.get("kenari").is_some());
        stream_one_model(&registry, "kenari-claude/claude-sonnet-5-5");
        let request = server.join().unwrap();
        let lower = request.to_lowercase();
        assert!(lower.starts_with("post /v1/messages "), "{request}");
        assert!(
            lower.contains("\r\nx-api-key: kn-shared-key\r\n"),
            "{request}"
        );
    }

    #[derive(Debug)]
    struct StubProvider {
        meta: kage_provider::ProviderMetadata,
        models: Vec<kage_provider::ProviderModel>,
    }

    impl kage_provider::Provider for StubProvider {
        fn metadata(&self) -> &kage_provider::ProviderMetadata {
            &self.meta
        }

        fn stream(
            &self,
            _req: kage_provider::StreamRequest,
            _cancel: &kage_core::CancelFlag,
        ) -> Result<kage_provider::EventStream, kage_provider::ProviderError> {
            Ok(Box::new(std::iter::empty()))
        }

        fn models(&self) -> Vec<kage_provider::ProviderModel> {
            self.models.clone()
        }
    }

    fn stub(id: &str, models: &[&str]) -> Arc<dyn kage_provider::Provider> {
        Arc::new(StubProvider {
            meta: kage_provider::ProviderMetadata {
                id: id.to_owned(),
                display_name: id.to_owned(),
                supports_caching: false,
                supports_thinking: false,
                supports_tool_use: true,
            },
            models: models
                .iter()
                .map(|m| kage_provider::ProviderModel {
                    id: (*m).to_owned(),
                    name: (*m).to_owned(),
                    ..kage_provider::ProviderModel::default()
                })
                .collect(),
        })
    }

    #[test]
    fn custom_provider_reusing_a_catalog_id_offers_its_own_models() {
        let config: kage_core::config::Config = toml::from_str(
            r#"
            [providers.custom.deepseek]
            base_url = "http://127.0.0.1:1/v1"
            api_key_env = ""
            display_name = "My DeepSeek"
            [[providers.custom.deepseek.models]]
            id = "ds-local"
            name = "DS Local"
            context = 4096
            "#,
        )
        .unwrap();
        assert!(is_catalog_provider("deepseek"));
        assert!(!is_catalog_provider("my-gateway"));
        assert!(kage_provider::catalog::provider("deepseek").is_some_and(|p| !p.models.is_empty()));
        let mut registry = ProviderRegistry::new().with(stub("deepseek", &[]));
        let replaced = register_custom_providers(&config, &auth::AuthStore::empty(), &mut registry);
        assert_eq!(replaced, ["deepseek"]);

        let rows: Vec<(String, Option<String>)> =
            crate::tui::available_model_items(&registry, "deepseek/ds-local")
                .into_iter()
                .map(|item| (item.value, item.group))
                .collect();
        assert_eq!(
            rows,
            [(
                "deepseek/ds-local".to_owned(),
                Some("My DeepSeek".to_owned())
            )]
        );
        assert_eq!(
            crate::runtime_env::context_window_for(&registry, "deepseek/ds-local"),
            Some(4096)
        );
        assert_eq!(fallback_model(&registry), "deepseek/ds-local");
    }

    #[test]
    fn custom_provider_reusing_a_catalog_id_is_priced_from_its_own_models() {
        let priced = kage_provider::catalog::provider("deepseek")
            .and_then(|p| p.models.iter().find(|m| m.cost.is_some()))
            .expect("catalog prices a deepseek model");
        let config: kage_core::config::Config = toml::from_str(&format!(
            r#"
            [providers.custom.deepseek]
            base_url = "http://127.0.0.1:1/v1"
            api_key_env = ""
            [[providers.custom.deepseek.models]]
            id = "{id}"
            name = "Unpriced"
            [[providers.custom.deepseek.models]]
            id = "ds-priced"
            name = "Priced"
            cost = {{ input = 0.27, output = 1.10, cache_read = 0.07 }}
            "#,
            id = priced.id,
        ))
        .unwrap();
        let mut registry = ProviderRegistry::new();
        register_custom_providers(&config, &auth::AuthStore::empty(), &mut registry);
        let custom = registry.get("deepseek").unwrap().as_ref();
        assert_eq!(kage_provider::model_cost(custom, priced.id), None);
        assert_eq!(
            kage_provider::model_cost(custom, "ds-priced"),
            Some(kage_core::ModelCost {
                input: 0.27,
                output: 1.10,
                cache_read: Some(0.07),
                cache_write: None,
            })
        );
        assert_eq!(kage_provider::model_cost(custom, "not-declared"), None);

        let catalog = stub("deepseek", &[]);
        assert_eq!(
            kage_provider::model_cost(catalog.as_ref(), priced.id),
            priced.cost
        );
    }

    #[test]
    fn acp_only_registry_has_no_usable_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register(stub("acp", &[]));
        assert!(!has_usable_provider(&registry));
        registry.register(stub("custom", &[]));
        assert!(has_usable_provider(&registry));
    }

    #[test]
    fn fallback_model_uses_first_declared_custom_model() {
        let registry = ProviderRegistry::new()
            .with(stub("acp", &["agent"]))
            .with(stub("zeta", &["z-1"]))
            .with(stub("empty", &[]))
            .with(stub("local", &["llama-3", "qwen"]));
        assert_eq!(fallback_model(&registry), "local/llama-3");
    }

    #[test]
    fn fallback_model_is_empty_without_models() {
        let registry = ProviderRegistry::new().with(stub("acp", &["agent"]));
        assert_eq!(fallback_model(&registry), "");
    }

    /// A registry holding only `ids`, built the way
    /// [`register_compat_providers`] builds them, so the catalog is the
    /// only source of models.
    fn compat_registry(ids: &[&str]) -> ProviderRegistry {
        let mut registry = ProviderRegistry::new();
        for id in ids {
            let entry = compat::COMPAT_PROVIDERS
                .iter()
                .find(|e| e.id == *id)
                .unwrap_or_else(|| panic!("{id} is a compat provider"));
            registry.register(entry.build("k", BTreeMap::new()));
        }
        registry.register(stub("acp", &["agent"]));
        registry
    }

    /// Every shipped compat provider must resolve a default model from
    /// its catalog entry alone.
    #[test]
    fn every_compat_provider_resolves_a_default_model() {
        for entry in compat::COMPAT_PROVIDERS {
            let id = entry.id;
            let model = fallback_model(&compat_registry(&[id]));
            let prefix = kage_core::split_model(&model).map(|(p, _)| p.to_owned());
            assert_eq!(
                prefix.as_deref(),
                Some(id),
                "{id}: fallback_model returned {model:?}"
            );
            assert!(!model.is_empty(), "{id} resolved to an empty model");
        }
    }

    /// The builtins are not compat entries but resolve the same way.
    #[test]
    fn every_builtin_resolves_a_default_model() {
        for id in ["anthropic", "openai", "google"] {
            let registry = ProviderRegistry::new().with(stub(id, &[]));
            assert!(
                fallback_model(&registry).starts_with(&format!("{id}/")),
                "{id} resolved to {:?}",
                fallback_model(&registry)
            );
        }
    }

    /// Ranking comes from the catalog order, not the alphabet: `groq`
    /// precedes `xiaomi` there.
    #[test]
    fn providers_follow_the_catalog_order() {
        let registry = compat_registry(&["xiaomi", "groq"]);
        assert!(fallback_model(&registry).starts_with("groq/"));
    }

    /// The Z.AI coding plans outrank the pay-as-you-go `zai` endpoint.
    /// Both answer to one `ZAI_CODING_API_KEY`, and only the plans bill
    /// as a subscription.
    #[test]
    fn the_coding_plans_outrank_plain_zai() {
        let registry = compat_registry(&["zai", "zai-coding-plan"]);
        assert!(fallback_model(&registry).starts_with("zai-coding-plan/"));
    }

    /// `google` outranks the compat providers it is billed alongside.
    #[test]
    fn google_outranks_the_providers_it_used_to_be_listed_after() {
        let mut registry = compat_registry(&["deepseek", "groq", "xai"]);
        registry.register(stub("google", &[]));
        assert!(fallback_model(&registry).starts_with("google/"));
    }

    #[test]
    fn detects_explicit_default_model_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[provider]\ndefault_model = \"openai/gpt-4o\"\n").unwrap();
        assert!(config_sets_default_model(&path));
        std::fs::write(&path, "[ui]\ntheme = \"dark\"\n[provider]\n").unwrap();
        assert!(!config_sets_default_model(&path));
        assert!(!config_sets_default_model(&dir.path().join("missing.toml")));
    }
}
