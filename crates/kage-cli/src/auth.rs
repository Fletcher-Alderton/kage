//! On-disk credential store for provider API keys and OAuth tokens.
//!
//! [`AuthStore`] persists per-provider credentials at
//! `$XDG_DATA_HOME/kage/auth.json` so users don't need to re-export
//! env vars every shell. The file is created with mode `0600` on Unix
//! so other users cannot read it.
//!
//! Each provider's entry is a [`Credential`] which is either an API
//! key or an OAuth record. The deserializer accepts the legacy
//! bare-string form so v1 files keep loading; the writer always emits
//! the tagged form.
//!
//! Environment variables still take precedence over the store: the
//! provider registry checks `ANTHROPIC_API_KEY` etc. first, then falls
//! back to whatever the store has, then registers the provider only if
//! one of the two yielded a key.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{DateTime, Duration, Utc};
use serde::de::{Error as DeError, Unexpected};
use serde::{Deserialize, Deserializer, Serialize};

/// On-disk shape of the auth file. Bumped to `2` when [`Credential`]
/// became a tagged enum; the loader still accepts v1 strings so older
/// files keep working without an explicit migration step.
const FORMAT_VERSION: u32 = 2;

/// One stored credential. Either a long-lived API key or an OAuth
/// access/refresh token pair the host periodically renews.
///
/// The deserializer accepts three shapes:
/// * a bare JSON string (the v1 form) - parsed as [`Credential::ApiKey`];
/// * an object `{ "type": "api_key", "key": "..." }`;
/// * an object `{ "type": "oauth", "access_token": "...", ... }`.
///
/// The serializer always emits the tagged-object form so files
/// rewritten by the current binary advance to the v2 layout. `Debug`
/// prints `***` in place of every secret.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    /// Long-lived API key the provider exchanges for one request at a time.
    ApiKey {
        /// Raw key as supplied by the provider's dashboard.
        key: String,
    },
    /// OAuth access token (with optional refresh) the host renews.
    Oauth(OAuthCredential),
}

impl Credential {
    /// Construct an [`Credential::ApiKey`] from a key string.
    #[must_use]
    pub fn api_key(key: impl Into<String>) -> Self {
        Self::ApiKey { key: key.into() }
    }

    /// `true` when this credential is an OAuth record (regardless of
    /// expiry). Useful for `auth list` to label rows.
    #[must_use]
    pub fn is_oauth(&self) -> bool {
        matches!(self, Self::Oauth(_))
    }

    /// Return the API key string, or the OAuth access token, without
    /// any expiry check. Callers that need refresh-on-expiry semantics
    /// should go through [`AuthStore::access_token`] instead.
    #[must_use]
    pub fn raw_token(&self) -> &str {
        match self {
            Self::ApiKey { key } => key.as_str(),
            Self::Oauth(o) => o.access_token.as_str(),
        }
    }
}

/// OAuth token bundle persisted alongside the access token. Mirrors
/// the canonical RFC 6749 fields the providers we support all return.
/// `Debug` prints `***` in place of both tokens.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthCredential {
    /// Bearer token presented in the `Authorization` header.
    pub access_token: String,
    /// Refresh token used to mint a new access token. `None` for
    /// providers that only issue short-lived tokens with no refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// UTC instant the access token stops being valid. `None` when
    /// the provider did not return an expiry; the host treats absent
    /// expiry as "never refresh proactively".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Space-delimited scope string the provider granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Provider-specific account or workspace id parsed from the
    /// login's `id_token` (the `ChatGPT` `chatgpt_account_id`). Sent as
    /// a request header where the backend requires it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl OAuthCredential {
    /// `true` when the access token is either expired or due to expire
    /// within `slack`. Used by the refresh path to renew a few minutes
    /// before the deadline so the next request doesn't race the clock.
    #[must_use]
    pub fn expires_within(&self, slack: Duration, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|t| now + slack >= t)
    }
}

/// What `Debug` prints in place of a secret.
const REDACTED: &str = "***";

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey { .. } => f.debug_struct("ApiKey").field("key", &REDACTED).finish(),
            Self::Oauth(oauth) => f.debug_tuple("Oauth").field(oauth).finish(),
        }
    }
}

impl fmt::Debug for OAuthCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthCredential")
            .field("access_token", &REDACTED)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| REDACTED),
            )
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .field("account_id", &self.account_id)
            .finish()
    }
}

/// Custom [`Deserialize`] tolerant of the legacy bare-string form.
impl<'de> Deserialize<'de> for Credential {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(s) = value.as_str() {
            return Ok(Self::ApiKey { key: s.to_owned() });
        }
        let map = value.as_object().ok_or_else(|| {
            D::Error::invalid_type(
                Unexpected::Other(&value.to_string()),
                &"a credential string or object",
            )
        })?;
        let kind = map
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| D::Error::missing_field("type"))?;
        match kind {
            "api_key" => {
                let key = map
                    .get("key")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| D::Error::missing_field("key"))?;
                Ok(Self::ApiKey {
                    key: key.to_owned(),
                })
            }
            "oauth" => {
                let oauth: OAuthCredential = serde_json::from_value(value)
                    .map_err(|e| D::Error::custom(format!("oauth: {e}")))?;
                Ok(Self::Oauth(oauth))
            }
            other => Err(D::Error::unknown_variant(other, &["api_key", "oauth"])),
        }
    }
}

/// Persistent credential store, keyed by provider id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStore {
    /// Schema version so future bumps can migrate.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Map of provider id (e.g. `anthropic`, `zai`) to credential.
    #[serde(default)]
    pub providers: BTreeMap<String, Credential>,
}

fn default_version() -> u32 {
    FORMAT_VERSION
}

impl AuthStore {
    /// Construct an empty store at the current format version.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            version: FORMAT_VERSION,
            providers: BTreeMap::new(),
        }
    }

    /// Path on disk: `$XDG_DATA_HOME/kage/auth.json`. Returns an error
    /// if the home directory cannot be resolved.
    pub fn default_path() -> Result<PathBuf, String> {
        Ok(crate::data_root()?.join("auth.json"))
    }

    /// Load the store from `path`. Returns an empty store when the file
    /// does not exist; surfaces every other I/O or decode error.
    pub fn load_from(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|e| format!("auth: parse {}: {e}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(err) => Err(format!("auth: read {}: {err}", path.display())),
        }
    }

    /// Convenience wrapper around [`Self::load_from`] using
    /// [`Self::default_path`].
    pub fn load() -> Result<Self, String> {
        Self::load_from(&Self::default_path()?)
    }

    /// Persist the store to `path` with mode `0600` on Unix so other
    /// users on the host cannot read it. Always rewrites at the
    /// current [`FORMAT_VERSION`] so loading and resaving an older
    /// file performs the v1 -> v2 migration in place.
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("auth: mkdir {}: {err}", parent.display()))?;
        }
        let mut bumped = self.clone();
        bumped.version = FORMAT_VERSION;
        let raw =
            serde_json::to_string_pretty(&bumped).map_err(|e| format!("auth: encode: {e}"))?;
        kage_core::fsutil::atomic_write_private(path, raw.as_bytes())
            .map_err(|err| format!("auth: write {}: {err}", path.display()))
    }

    /// Convenience wrapper around [`Self::save_to`] using
    /// [`Self::default_path`].
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::default_path()?)
    }

    /// Look up a provider's full [`Credential`]. `zhipuai-coding-plan`
    /// falls back to the credential saved for `zai-coding-plan`, since
    /// one coding plan key works on both endpoints; the same applies
    /// to `commandcode-claude` and `commandcode`, and to `kenari-claude`
    /// and `kenari`.
    #[must_use]
    pub fn credential(&self, provider: &str) -> Option<&Credential> {
        self.providers.get(provider).or_else(|| match provider {
            "zhipuai-coding-plan" => self.providers.get("zai-coding-plan"),
            "commandcode-claude" => self.providers.get("commandcode"),
            "kenari-claude" => self.providers.get("kenari"),
            _ => None,
        })
    }

    /// The stored credential `provider`'s public API accepts:
    /// [`Self::credential`], except a `ChatGPT` login (an OAuth record
    /// under `openai`), which only the `ChatGPT` backend takes.
    #[must_use]
    pub fn api_credential(&self, provider: &str) -> Option<&Credential> {
        self.credential(provider)
            .filter(|c| provider != "openai" || !c.is_oauth())
    }

    /// Look up a single bearer token for `provider`, regardless of
    /// credential kind. For API keys returns the raw key; for OAuth
    /// returns the access token *as currently stored*, which may be
    /// expired because kage does not refresh provider OAuth tokens.
    #[must_use]
    pub fn access_token(&self, provider: &str) -> Option<&str> {
        self.credential(provider).map(Credential::raw_token)
    }

    /// OAuth credentials whose access token expires within `window` of
    /// `now` (or already has), as `(provider, expires_at)` pairs in
    /// provider order.
    pub fn oauth_expiring(
        &self,
        window: Duration,
        now: DateTime<Utc>,
    ) -> impl Iterator<Item = (&str, DateTime<Utc>)> {
        self.providers
            .iter()
            .filter_map(move |(id, cred)| match cred {
                Credential::Oauth(o) if o.expires_within(window, now) => {
                    o.expires_at.map(|at| (id.as_str(), at))
                }
                _ => None,
            })
    }

    /// Insert or replace `provider`'s credential. Returns the previous
    /// value, if any.
    pub fn set(&mut self, provider: &str, credential: Credential) -> Option<Credential> {
        self.providers.insert(provider.to_owned(), credential)
    }

    /// Insert or replace `provider`'s credential as an API key.
    /// Convenience wrapper around [`Self::set`].
    pub fn set_api_key(&mut self, provider: &str, key: impl Into<String>) -> Option<Credential> {
        self.set(provider, Credential::api_key(key))
    }

    /// Insert or replace `provider`'s credential as an OAuth record.
    /// Convenience wrapper around [`Self::set`].
    pub fn set_oauth(&mut self, provider: &str, creds: OAuthCredential) -> Option<Credential> {
        self.set(provider, Credential::Oauth(creds))
    }

    /// Remove a provider's saved credential. Returns true when something
    /// was actually removed.
    pub fn remove(&mut self, provider: &str) -> bool {
        self.providers.remove(provider).is_some()
    }
}

/// How far ahead the OAuth expiry warnings look. Shared by the TUI
/// startup notices, `auth list` and doctor.
pub(crate) const OAUTH_EXPIRY_WARNING: Duration = Duration::days(3);

/// The expiry phrase for an OAuth credential expiring at `at`,
/// relative to `now`: `has expired`, `expires within a day`,
/// `expires in 1 day` or `expires in N days`. Shared by the TUI
/// startup notices, `auth list` and doctor.
pub(crate) fn expiry_label(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    match (at - now).num_days() {
        _ if at <= now => "has expired".to_owned(),
        0 => "expires within a day".to_owned(),
        1 => "expires in 1 day".to_owned(),
        days => format!("expires in {days} days"),
    }
}

/// Provider ids the auth subcommand can target. The list mirrors the
/// catalog (and our hardcoded `Provider` impls). `zai` and the two
/// coding plans are billed separately, so `zai` takes its own key;
/// the coding plans share `ZAI_CODING_API_KEY`, the four `xiaomi*`
/// ids all share `XIAOMI_API_KEY`, both `commandcode` ids share
/// `CMD_API_KEY`, both `kenari` ids share `KENARI_API_KEY`, and
/// `opencode-go` takes `OPENCODE_API_KEY`.
pub const KNOWN_PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "zai",
    "zai-coding-plan",
    "zhipuai-coding-plan",
    "deepseek",
    "groq",
    "mistral",
    "cerebras",
    "xai",
    "openrouter",
    "fireworks-ai",
    "moonshotai",
    "kimi-for-coding",
    "xiaomi",
    "xiaomi-token-plan-ams",
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-sgp",
    "commandcode",
    "commandcode-claude",
    "kenari",
    "kenari-claude",
    "opencode-go",
];

/// Env-var name that, when set, supersedes any saved key for `provider`.
#[must_use]
pub fn env_var_for(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "google" => "GEMINI_API_KEY",
        "zai" => "ZAI_API_KEY",
        "zai-coding-plan" | "zhipuai-coding-plan" => "ZAI_CODING_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "groq" => "GROQ_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "cerebras" => "CEREBRAS_API_KEY",
        "xai" => "XAI_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "fireworks-ai" => "FIREWORKS_API_KEY",
        "moonshotai" => "MOONSHOT_API_KEY",
        "kimi-for-coding" => "KIMI_API_KEY",
        "xiaomi" | "xiaomi-token-plan-ams" | "xiaomi-token-plan-cn" | "xiaomi-token-plan-sgp" => {
            "XIAOMI_API_KEY"
        }
        // One Command Code key works on both the Chat Completions and
        // the Claude (Messages) provider.
        "commandcode" | "commandcode-claude" => "CMD_API_KEY",
        // Likewise one Kenari key covers both of its providers.
        "kenari" | "kenari-claude" => "KENARI_API_KEY",
        "opencode-go" => "OPENCODE_API_KEY",
        _ => "",
    }
}

/// `kage auth login [provider]`: sign in and persist the credential.
/// `provider` may name a builtin or catalog id, or a custom provider
/// declared under `[providers.custom.*]` in `config`. The `OpenAI`
/// family offers the choice of a `ChatGPT` sign-in (the browser OAuth
/// flow, stored as an OAuth credential) or a plain API key; every
/// other provider prompts for a key.
pub fn run_login(provider: Option<&str>, config: &kage_core::config::Config) -> ExitCode {
    let provider = match select_provider(provider, config) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(2);
        }
    };
    if provider == "openai" {
        return run_openai_login();
    }
    run_login_api_key(&provider)
}

/// The `OpenAI` login: pick `ChatGPT` sign-in or an API key, then store
/// the result under `openai`.
fn run_openai_login() -> ExitCode {
    let items = [
        kage_tui::PickItem::simple("chatgpt")
            .with_label("Sign in with ChatGPT (uses your ChatGPT plan)"),
        kage_tui::PickItem::simple("api").with_label("Use an OpenAI API key"),
    ];
    let choice = match kage_tui::pick("How do you want to sign in to OpenAI?", &items) {
        Ok(Some(choice)) => choice,
        Ok(None) => {
            eprintln!("kage: cancelled");
            return ExitCode::from(2);
        }
        Err(_) => "api".to_owned(),
    };
    if choice != "chatgpt" {
        return run_login_api_key("openai");
    }
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let credential = match crate::chatgpt::login(&cancel) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let mut store = match AuthStore::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = crate::chatgpt::models_cache_path()
        .and_then(|cache| crate::chatgpt::refresh_models(&credential, &cache))
    {
        eprintln!("kage: {e}; `kage models refresh` retries");
    }
    store.set_oauth("openai", credential);
    if let Err(e) = store.save() {
        eprintln!("kage: {e}");
        return ExitCode::from(1);
    }
    eprintln!("kage: signed in with ChatGPT; `openai` models now use your plan");
    ExitCode::SUCCESS
}

/// The API-key path of [`run_login`], factored out so the `OpenAI`
/// fallback reuses it.
fn run_login_api_key(provider: &str) -> ExitCode {
    let mut store = match AuthStore::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let key = match read_secret(&format!("{provider} API key: ")) {
        Ok(k) if k.is_empty() => {
            eprintln!("kage: empty key, nothing saved");
            return ExitCode::from(2);
        }
        Ok(k) => k,
        Err(e) => {
            eprintln!("kage: prompt failed: {e}");
            return ExitCode::from(1);
        }
    };
    store.set_api_key(provider, key);
    if let Err(e) = store.save() {
        eprintln!("kage: {e}");
        return ExitCode::from(1);
    }
    eprintln!("kage: saved {provider} credentials");
    ExitCode::SUCCESS
}

/// `kage auth logout <provider>`: remove the saved key for `provider`.
pub fn run_logout(provider: &str) -> ExitCode {
    let mut store = match AuthStore::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    if !store.remove(provider) {
        eprintln!("kage: no saved key for {provider}");
        return ExitCode::SUCCESS;
    }
    if let Err(e) = store.save() {
        eprintln!("kage: {e}");
        return ExitCode::from(1);
    }
    eprintln!("kage: removed {provider} credentials");
    ExitCode::SUCCESS
}

/// Where one provider's credential can come from.
pub(crate) struct ProviderKey {
    /// Provider id.
    pub id: String,
    /// Environment variable checked before the auth store; empty for
    /// none.
    pub env: String,
    /// A custom provider whose endpoint needs no key.
    pub keyless: bool,
}

impl ProviderKey {
    /// Where the credential comes from right now, or `None` when the
    /// provider has none.
    pub(crate) fn source(&self, store: &AuthStore) -> Option<String> {
        self.source_with(
            store,
            |env| std::env::var(env).is_ok_and(|v| !v.is_empty()),
            Utc::now(),
        )
    }

    /// [`Self::source`] against an injected env check and clock, so
    /// callers can rate providers without touching the process env or
    /// the wall clock. An OAuth row carries its expiry label, and the
    /// `openai` row names a `ChatGPT` login as one.
    pub(crate) fn source_with(
        &self,
        store: &AuthStore,
        env_set: impl Fn(&str) -> bool,
        now: DateTime<Utc>,
    ) -> Option<String> {
        if self.keyless {
            return Some("no key needed".to_owned());
        }
        let env = (!self.env.is_empty() && env_set(&self.env)).then_some(self.env.as_str());
        let kind = if self.id == "openai" {
            "ChatGPT login"
        } else {
            "oauth"
        };
        let stored = store.credential(&self.id).map(|c| match c {
            Credential::Oauth(o) => match o.expires_at {
                Some(at) => format!("auth.json ({kind}, {})", expiry_label(at, now)),
                None => format!("auth.json ({kind})"),
            },
            Credential::ApiKey { .. } => "auth.json".to_owned(),
        });
        match (env, stored) {
            (Some(env), Some(label)) => Some(format!("{env} + {label}")),
            (Some(env), None) => Some(env.to_owned()),
            (None, Some(label)) => Some(label.clone()),
            (None, None) => None,
        }
    }
}

/// Every provider `config` can use, sorted by id: the known providers,
/// with any `[providers.<id>] api_key_env` override applied, and the
/// custom providers under `[providers.custom.*]`.
pub(crate) fn provider_keys(config: &kage_core::config::Config) -> Vec<ProviderKey> {
    let mut keys: Vec<ProviderKey> = KNOWN_PROVIDERS
        .iter()
        .map(|id| ProviderKey {
            id: (*id).to_owned(),
            env: config
                .providers
                .overrides
                .get(*id)
                .and_then(|o| o.api_key_env.clone())
                .unwrap_or_else(|| env_var_for(id).to_owned()),
            keyless: false,
        })
        .collect();
    keys.extend(config.providers.custom.iter().map(|(id, cfg)| {
        let env = cfg.key_env(id);
        ProviderKey {
            id: id.clone(),
            keyless: env.is_empty(),
            env,
        }
    }));
    keys.sort_by(|a, b| a.id.cmp(&b.id));
    keys
}

/// `kage auth list`: show one row per known or custom provider,
/// indicating whether a credential is available from the env, the
/// auth store (and what kind), both, or neither.
pub fn run_list() -> ExitCode {
    let store = match AuthStore::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let config = kage_core::config::Config::load_default().unwrap_or_else(|e| {
        eprintln!("kage: config: {e}; custom providers are not listed");
        kage_core::config::Config::default()
    });
    let mut keys = provider_keys(&config);
    keys.extend(media_tool_keys(&config, &store));
    let _ = write_list(&mut io::stdout().lock(), &keys, &store, Utc::now());
    ExitCode::SUCCESS
}

/// The media tools that call an endpoint of their own (`base_url` set),
/// whose key is read from their `api_key_env` or saved under the tool's
/// name. One with neither sends no key, which a self-hosted server may
/// not need. A tool that borrows a `provider` uses that provider's key,
/// which already has its own row.
fn media_tool_keys(config: &kage_core::config::Config, store: &AuthStore) -> Vec<ProviderKey> {
    let tools = &config.tools;
    [
        (crate::engine::GENERATE_IMAGE_TOOL, &tools.generate_image),
        (crate::engine::EDIT_IMAGE_TOOL, &tools.edit_image),
        (
            crate::engine::GENERATE_SPEECH_TOOL,
            &tools.generate_speech.endpoint,
        ),
    ]
    .into_iter()
    .filter(|(_, cfg)| {
        cfg.base_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty())
    })
    .map(|(id, cfg)| {
        let env = cfg.api_key_env.clone().unwrap_or_default();
        ProviderKey {
            id: id.to_owned(),
            keyless: env.is_empty() && store.credential(id).is_none(),
            env,
        }
    })
    .collect()
}

/// Render the `kage auth list` table, sizing the id column to the
/// longest id.
fn write_list<W: Write>(
    out: &mut W,
    providers: &[ProviderKey],
    store: &AuthStore,
    now: DateTime<Utc>,
) -> io::Result<()> {
    let width = providers
        .iter()
        .map(|p| p.id.len())
        .chain(["PROVIDER".len()])
        .max()
        .unwrap_or_default();
    writeln!(out, "{:<width$}  {:<6}  SOURCE", "PROVIDER", "STATUS")?;
    for provider in providers {
        let source = provider.source_with(
            store,
            |env| std::env::var(env).is_ok_and(|v| !v.is_empty()),
            now,
        );
        let status = if source.is_some() { "ready" } else { "-" };
        writeln!(
            out,
            "{:<width$}  {status:<6}  {}",
            provider.id,
            source.as_deref().unwrap_or("(unset)")
        )?;
    }
    Ok(())
}

/// `true` when `target` names what `kage auth login` can store a
/// credential for: a builtin or catalog id, a custom provider declared
/// under `[providers.custom.*]`, or a media tool endpoint.
fn login_target_is_known(
    target: &str,
    custom: &BTreeMap<String, kage_core::config::CustomProviderConfig>,
) -> bool {
    KNOWN_PROVIDERS.contains(&target)
        || custom.contains_key(target)
        || crate::engine::MEDIA_TOOLS.contains(&target)
}

fn select_provider(
    arg: Option<&str>,
    config: &kage_core::config::Config,
) -> Result<String, String> {
    let custom = &config.providers.custom;
    if let Some(p) = arg {
        if login_target_is_known(p, custom) {
            return Ok(p.to_owned());
        }
        let mut known: Vec<&str> = KNOWN_PROVIDERS.to_vec();
        known.extend(custom.keys().map(String::as_str));
        known.extend(crate::engine::MEDIA_TOOLS);
        return Err(format!(
            "unknown provider '{p}', expected one of: {}",
            known.join(", ")
        ));
    }
    let store = AuthStore::load().unwrap_or_else(|_| AuthStore::empty());
    let mut items: Vec<kage_tui::PickItem> = KNOWN_PROVIDERS
        .iter()
        .map(|p| {
            let badge = if store.credential(p).is_some() {
                '*'
            } else {
                '-'
            };
            let compat = kage_provider::compat::COMPAT_PROVIDERS
                .iter()
                .find(|c| c.id == *p);
            let label = match (kage_provider::catalog::provider(p), compat) {
                (Some(info), _) => format!("{} ({}, {} models)", info.name, p, info.models.len()),
                // Providers the bundled catalog does not list have no
                // model count to show.
                (None, Some(c)) => format!("{} ({p})", c.display_name),
                (None, None) => (*p).to_owned(),
            };
            kage_tui::PickItem::simple(*p)
                .with_label(label)
                .with_badge(badge)
        })
        .collect();
    items.extend(custom.iter().map(|(id, cfg)| {
        let badge = if store.credential(id).is_some() {
            '*'
        } else {
            '-'
        };
        let label = format!(
            "{} ({}, {} models)",
            cfg.display_name.as_deref().unwrap_or(id),
            id,
            cfg.models.len()
        );
        kage_tui::PickItem::simple(id.clone())
            .with_label(label)
            .with_badge(badge)
    }));
    items.extend(crate::engine::MEDIA_TOOLS.iter().map(|tool| {
        let badge = if store.credential(tool).is_some() {
            '*'
        } else {
            '-'
        };
        kage_tui::PickItem::simple(*tool)
            .with_label(format!("{tool} (media tool endpoint)"))
            .with_badge(badge)
    }));
    match kage_tui::pick("Select a provider", &items) {
        Ok(Some(p)) => Ok(p),
        Ok(None) => Err("cancelled".to_owned()),
        Err(e) => Err(format!("picker failed: {e}")),
    }
}

fn read_secret(prompt: &str) -> Result<String, String> {
    rpassword::prompt_password(prompt).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::TimeZone;
    use tempfile::tempdir;

    use super::*;

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    #[test]
    fn every_compat_provider_has_an_env_var() {
        for entry in kage_provider::compat::COMPAT_PROVIDERS {
            assert!(
                !env_var_for(entry.id).is_empty(),
                "compat provider `{}` has no env var mapping in env_var_for",
                entry.id
            );
        }
    }

    #[test]
    fn china_coding_plan_shares_the_coding_plan_key_unless_it_has_its_own() {
        let mut store = AuthStore::empty();
        store.set_api_key("zai-coding-plan", "shared");
        assert_eq!(store.access_token("zhipuai-coding-plan"), Some("shared"));
        assert_eq!(env_var_for("zhipuai-coding-plan"), "ZAI_CODING_API_KEY");
        store.set_api_key("zhipuai-coding-plan", "own");
        assert_eq!(store.access_token("zhipuai-coding-plan"), Some("own"));
        assert_eq!(store.access_token("zai-coding-plan"), Some("shared"));
        assert_eq!(AuthStore::empty().access_token("zhipuai-coding-plan"), None);
    }

    #[test]
    fn round_trip_through_json() {
        let mut store = AuthStore::empty();
        store.set_api_key("anthropic", "sk-ant-xxx");
        store.set_api_key("zai", "zai-key");
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        store.save_to(&path).unwrap();
        let read_back = AuthStore::load_from(&path).unwrap();
        assert_eq!(read_back, store);
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let store = AuthStore::load_from(&path).unwrap();
        assert!(store.providers.is_empty());
    }

    #[test]
    fn remove_returns_true_only_when_present() {
        let mut store = AuthStore::empty();
        store.set_api_key("zai", "k");
        assert!(store.remove("zai"));
        assert!(!store.remove("zai"));
    }

    #[test]
    fn malformed_file_is_a_decode_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(&path, b"{not json").unwrap();
        let err = AuthStore::load_from(&path).unwrap_err();
        assert!(err.contains("parse"));
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_has_mode_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let mut store = AuthStore::empty();
        store.set_api_key("anthropic", "x");
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        store.save_to(&path).unwrap();
        let perms = fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn resaving_tightens_mode_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut store = AuthStore::empty();
        store.set_api_key("anthropic", "x");
        store.save_to(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        store.set_api_key("openai", "y");
        store.save_to(&path).unwrap();

        let perms = fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["auth.json"]);
        let loaded = AuthStore::load_from(&path).unwrap();
        assert!(loaded.credential("openai").is_some());
    }

    #[test]
    fn legacy_v1_string_entries_load_as_api_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(
            &path,
            r#"{"version":1,"providers":{"anthropic":"sk-ant-old","zai":"zai-old"}}"#,
        )
        .unwrap();
        let store = AuthStore::load_from(&path).unwrap();
        assert_eq!(store.access_token("anthropic"), Some("sk-ant-old"));
        assert_eq!(store.access_token("zai"), Some("zai-old"));
        assert!(matches!(
            store.credential("anthropic"),
            Some(Credential::ApiKey { .. })
        ));
    }

    #[test]
    fn save_after_loading_v1_rewrites_as_v2_tagged_form() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(
            &path,
            r#"{"version":1,"providers":{"anthropic":"sk-ant-old"}}"#,
        )
        .unwrap();
        let store = AuthStore::load_from(&path).unwrap();
        store.save_to(&path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"version\": 2"));
        assert!(raw.contains("\"type\": \"api_key\""));
        assert!(raw.contains("\"key\": \"sk-ant-old\""));
    }

    /// A `ChatGPT` login is no key for `OpenAI`'s public API, and
    /// `auth list` names it on the `openai` row, which counts as ready.
    #[test]
    fn a_chatgpt_login_is_no_public_api_credential() {
        let mut store = AuthStore::empty();
        store.set_oauth("openai", OAuthCredential::default());
        assert!(store.api_credential("openai").is_none());
        let row = ProviderKey {
            id: "openai".to_owned(),
            env: String::new(),
            keyless: false,
        };
        assert_eq!(
            row.source_with(&store, |_| false, fixed_now()).as_deref(),
            Some("auth.json (ChatGPT login)")
        );

        store.set_api_key("openai", "sk-1");
        assert!(store.api_credential("openai").is_some());
    }

    #[test]
    fn oauth_credential_round_trips() {
        let mut store = AuthStore::empty();
        let expires_at = fixed_now() + Duration::seconds(3600);
        let oauth = OAuthCredential {
            access_token: "access-1".into(),
            refresh_token: Some("refresh-1".into()),
            expires_at: Some(expires_at),
            scope: Some("scope-x".into()),
            account_id: Some("acc-7".into()),
        };
        store.set_oauth("anthropic", oauth.clone());
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        store.save_to(&path).unwrap();
        let read_back = AuthStore::load_from(&path).unwrap();
        assert_eq!(read_back, store);
        assert!(read_back.credential("anthropic").unwrap().is_oauth());
        assert_eq!(read_back.access_token("anthropic"), Some("access-1"));
        let Credential::Oauth(o) = read_back.credential("anthropic").unwrap() else {
            panic!("expected oauth");
        };
        assert_eq!(o.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(o.scope.as_deref(), Some("scope-x"));
        assert_eq!(o.expires_at, Some(expires_at));
    }

    #[test]
    fn expires_within_returns_true_inside_slack_and_false_outside() {
        let now = fixed_now();
        let oauth = OAuthCredential {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: Some(now + Duration::seconds(120)),
            scope: None,
            account_id: None,
        };
        assert!(!oauth.expires_within(Duration::seconds(60), now));
        assert!(oauth.expires_within(Duration::seconds(120), now));
        assert!(oauth.expires_within(Duration::seconds(180), now));
    }

    #[test]
    fn expires_within_returns_false_when_no_expiry_recorded() {
        let oauth = OAuthCredential {
            access_token: "a".into(),
            ..OAuthCredential::default()
        };
        assert!(!oauth.expires_within(Duration::days(365), fixed_now()));
    }

    #[test]
    fn debug_output_redacts_every_secret() {
        let oauth = OAuthCredential {
            access_token: "access-secret".into(),
            refresh_token: Some("refresh-secret".into()),
            expires_at: Some(fixed_now()),
            scope: Some("read".into()),
            account_id: None,
        };
        let mut store = AuthStore::empty();
        store.set_api_key("anthropic", "sk-ant-secret");
        store.set_oauth("zai", oauth.clone());
        for debug in [
            format!("{oauth:?}"),
            format!("{:?}", Credential::Oauth(oauth.clone())),
            format!("{store:?}"),
            format!("{store:#?}"),
        ] {
            assert!(debug.contains("***"), "{debug}");
            for secret in ["access-secret", "refresh-secret", "sk-ant-secret"] {
                assert!(!debug.contains(secret), "{debug}");
            }
        }
        assert!(format!("{oauth:?}").contains("read"));
    }

    #[test]
    fn deserialize_rejects_unknown_credential_type() {
        let err = serde_json::from_str::<Credential>(r#"{"type":"weird"}"#).unwrap_err();
        assert!(err.to_string().contains("weird"));
    }

    #[test]
    fn deserialize_rejects_object_without_type() {
        let err = serde_json::from_str::<Credential>(r#"{"key":"x"}"#).unwrap_err();
        assert!(err.to_string().contains("type"));
    }

    #[test]
    fn login_target_accepts_known_and_custom_ids() {
        let config = kage_core::config::Config::default();
        assert!(login_target_is_known("anthropic", &config.providers.custom));
        assert!(!login_target_is_known("acp", &config.providers.custom));
        assert!(!login_target_is_known("nope", &config.providers.custom));
        assert!(login_target_is_known(
            "generate_image",
            &config.providers.custom
        ));
        assert!(login_target_is_known(
            "edit_image",
            &config.providers.custom
        ));
        assert!(login_target_is_known(
            "generate_speech",
            &config.providers.custom
        ));

        let mut config = kage_core::config::Config::default();
        config.providers.custom.insert(
            "llama-local".to_owned(),
            kage_core::config::CustomProviderConfig {
                kind: kage_core::config::CustomProviderKind::OpenAi,
                base_url: "http://localhost:8080/v1".to_owned(),
                display_name: Some("Llama (local)".to_owned()),
                api_key_env: None,
                models_url: None,
                headers: BTreeMap::new(),
                models: vec![kage_core::config::CustomProviderModel {
                    id: "llama-3".to_owned(),
                    name: "Llama 3".to_owned(),
                    ..kage_core::config::CustomProviderModel::default()
                }],
                tool_use: true,
                thinking: false,
                caching: false,
                input_includes_cache: false,
            },
        );
        assert!(login_target_is_known(
            "llama-local",
            &config.providers.custom
        ));
        assert!(!login_target_is_known("llama", &config.providers.custom));
    }

    fn keyless_config(id: &str) -> kage_core::config::Config {
        let body = format!(
            "[providers.custom.{id}]\nbase_url = \"http://127.0.0.1:1/v1\"\napi_key_env = \"\"\n\
             [[providers.custom.{id}.models]]\nid = \"small\"\nname = \"Small\"\n"
        );
        toml::from_str(&body).unwrap()
    }

    #[test]
    fn provider_keys_include_custom_providers() {
        let keys = provider_keys(&keyless_config("fake"));
        let fake = keys.iter().find(|k| k.id == "fake").unwrap();
        assert!(fake.keyless);
        assert_eq!(
            fake.source(&AuthStore::empty()).as_deref(),
            Some("no key needed")
        );
        assert!(keys.windows(2).all(|w| w[0].id <= w[1].id));
    }

    /// The one credential list behind `auth list`, doctor and init:
    /// custom env names and synthesized env names must both resolve
    /// here.
    #[test]
    fn provider_keys_resolve_every_credential_shape() {
        let config: kage_core::config::Config = toml::from_str(
            "[providers.custom.lab]\nbase_url = \"http://lab:1/v1\"\napi_key_env = \"MY_KEY\"\n\
             [[providers.custom.lab.models]]\nid = \"m\"\nname = \"M\"\n\n\
             [providers.custom.synth]\nbase_url = \"http://s:1/v1\"\n\
             [[providers.custom.synth.models]]\nid = \"m\"\nname = \"M\"\n",
        )
        .unwrap();
        let keys = provider_keys(&config);

        let lab = keys.iter().find(|k| k.id == "lab").unwrap();
        assert_eq!(lab.env, "MY_KEY");
        assert!(!lab.keyless);

        let synth = keys.iter().find(|k| k.id == "synth").unwrap();
        assert_eq!(synth.env, "SYNTH_API_KEY", "the env name is synthesized");
        assert!(!synth.keyless);
    }

    #[test]
    fn list_shows_media_tools_that_call_their_own_endpoint() {
        let config: kage_core::config::Config = toml::from_str(
            "[tools.generate_image]\nprovider = \"openai\"\nmodel = \"m\"\n\
             [tools.generate_speech]\nbase_url = \"http://tts:1/v1\"\n\
             api_key_env = \"TTS_KEY\"\nmodel = \"tts\"\n\
             [tools.edit_image]\nbase_url = \"http://img:1/v1\"\nmodel = \"m\"\n",
        )
        .unwrap();
        let keys = media_tool_keys(&config, &AuthStore::empty());

        let ids: Vec<&str> = keys.iter().map(|k| k.id.as_str()).collect();
        assert_eq!(ids, ["edit_image", "generate_speech"]);
        assert_eq!(keys[0].env, "");
        assert!(keys[0].keyless, "no variable and no saved key sends no key");
        assert_eq!(keys[1].env, "TTS_KEY");
        assert!(!keys[1].keyless);
    }

    #[test]
    fn list_aligns_columns_to_the_longest_id() {
        let keys = provider_keys(&keyless_config("a-very-long-custom-provider"));
        let mut out = Vec::new();
        write_list(&mut out, &keys, &AuthStore::empty(), fixed_now()).unwrap();
        let out = String::from_utf8(out).unwrap();
        let mut lines = out.lines();
        let header = lines.next().unwrap();
        assert!(header.ends_with("SOURCE"), "{header}");
        let column = header.find("STATUS").unwrap();
        for line in lines {
            assert_eq!(line[..column].chars().last(), Some(' '), "{line}");
            assert_ne!(line.as_bytes()[column], b' ', "{line}");
        }
        assert!(out.contains("a-very-long-custom-provider  ready   no key needed"));
    }

    /// OAuth rows carry their expiry next to the source, using the
    /// shared label: days left, the sub-day phrasing, or `expired`.
    #[test]
    fn list_labels_oauth_rows_with_their_expiry() {
        let env_config = |id: &str, env: &str| {
            let body = format!(
                "[providers.custom.{id}]\nbase_url = \"http://127.0.0.1:1/v1\"\napi_key_env = \"{env}\"\n\
                 [[providers.custom.{id}.models]]\nid = \"small\"\nname = \"Small\"\n"
            );
            toml::from_str(&body).unwrap()
        };
        let mut store = AuthStore::empty();
        store.set_oauth(
            "zai",
            OAuthCredential {
                access_token: "t".into(),
                expires_at: Some(fixed_now() + Duration::days(3)),
                ..OAuthCredential::default()
            },
        );
        store.set_oauth(
            "lab",
            OAuthCredential {
                access_token: "t".into(),
                expires_at: Some(fixed_now() - Duration::hours(1)),
                ..OAuthCredential::default()
            },
        );
        let mut keys = provider_keys(&env_config("zai", "KAGE_TEST_UNSET_ZAI"));
        keys.extend(provider_keys(&env_config("lab", "KAGE_TEST_UNSET_LAB")));
        let mut out = Vec::new();
        write_list(&mut out, &keys, &store, fixed_now()).unwrap();
        let out = String::from_utf8(out).unwrap();
        let zai = out.lines().find(|l| l.starts_with("zai ")).unwrap();
        assert!(
            zai.contains("auth.json (oauth, expires in 3 days)"),
            "{zai}"
        );
        let lab = out.lines().find(|l| l.starts_with("lab ")).unwrap();
        assert!(lab.contains("has expired"), "{lab}");
    }

    #[test]
    fn the_no_credentials_message_names_every_provider_env_var() {
        for id in KNOWN_PROVIDERS {
            let env = env_var_for(id);
            assert!(
                crate::NO_CREDENTIALS_MESSAGE.contains(env),
                "{id}: {env} missing"
            );
        }
    }
}
