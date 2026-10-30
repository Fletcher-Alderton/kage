//! Workspace configuration loaded from `~/.config/kage/config.toml` with env overrides.
//!
//! Layered loading via [`figment`]: defaults are merged with the file
//! (if present), which is then overridden by environment variables prefixed
//! with `KAGE_`. Nested keys use `__` as the separator, for example
//! `KAGE_UI__THEME=catppuccin-mocha`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};

use crate::event::ModelCost;
use crate::modality::Modalities;
use crate::options::OptionValue;
use crate::permissions::PermissionsConfig;
use crate::thinking::{Efforts, Reasoning, ReasoningField};

use crate::error::Result;

/// Top-level configuration for a kage process.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// LLM provider settings.
    pub provider: ProviderConfig,
    /// User interface settings (theme, mouse, etc.).
    pub ui: UiConfig,
    /// Plugin loader settings.
    pub plugins: PluginsConfig,
    /// Keybinding overrides.
    pub keybindings: KeybindingsConfig,
    /// Agent-loop tuning (`[loop]`): compaction threshold, etc.
    #[serde(rename = "loop")]
    pub loop_settings: LoopSettings,
    /// Subagent limits (`[agents]`).
    pub agents: AgentsConfig,
    /// External ACP agents usable as `acp:<name>` (`[acp.agents.*]`).
    pub acp: AcpConfig,
    /// External MCP tool servers (`[mcp.servers.*]`).
    pub mcp: McpConfig,
    /// Custom providers and overrides for registered providers
    /// (`[providers.custom.*]` / `[providers.<provider-id>]`).
    #[serde(skip_serializing_if = "ProvidersConfig::is_default")]
    pub providers: ProvidersConfig,
    /// Tool permission rules (`[permissions]`): allow / ask / deny
    /// per tool, plus the path-confinement flag (on by default;
    /// `confine_paths = false` opts out).
    #[serde(default, skip_serializing_if = "PermissionsConfig::is_default")]
    pub permissions: crate::permissions::PermissionsConfig,
    /// Shell tool policy (`[shell]`).
    #[serde(default, skip_serializing_if = "ShellConfig::is_default")]
    pub shell: ShellConfig,
    /// Built-in tool settings (`[tools]`): renames, per-tool
    /// `enabled` / `description` overrides and limits.
    #[serde(default, skip_serializing_if = "ToolsConfig::is_default")]
    pub tools: ToolsConfig,
}

/// The longest a media tool call may take, matching the ceiling the
/// media tools enforce when they build their client.
const MAX_MEDIA_TIMEOUT_SECS: u64 = 3600;

/// The endpoint a media tool calls, using an `OpenAI`-compatible API.
///
/// The endpoint is either a provider kage already reaches, reused with
/// its base URL, key and headers (`provider`), or named outright
/// (`base_url`, with `api_key_env` and `headers`). `base_url` wins when
/// both are set. A project's `.kage/config.toml` never sets this table,
/// because it names where a key is sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaToolConfig {
    /// `false` removes the tool: the model never sees it.
    #[serde(
        default = "crate::tools_config::default_true",
        skip_serializing_if = "crate::tools_config::is_true"
    )]
    pub enabled: bool,
    /// Replaces the description the model reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The provider whose base URL, key and headers the tool reuses:
    /// `openai`, an OpenAI-compatible provider kage ships, or a
    /// `[providers.custom.<id>]` of kind `openai`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The endpoint's base URL, without the API route.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The environment variable holding the key sent to `base_url`.
    /// Unset or empty, the key saved with `kage auth login <tool>` is
    /// sent instead, and with neither no key is sent.
    /// A variable named here with no saved key to fall back on leaves the
    /// tool out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Extra HTTP headers sent to `base_url`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// The model asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Seconds one call may take, the whole request included, from
    /// 1 to 3600. Defaults to 180.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl Default for MediaToolConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            description: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            headers: BTreeMap::new(),
            model: None,
            timeout_secs: None,
        }
    }
}

impl MediaToolConfig {
    /// Whether the defaults are in effect, which leaves the tool
    /// unregistered.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Whether the table asks for the tool: set, and not
    /// `enabled = false`.
    #[must_use]
    pub fn is_wanted(&self) -> bool {
        self.enabled && !self.is_default()
    }

    /// Reject a `timeout_secs` outside the range a call accepts.
    ///
    /// # Errors
    ///
    /// A [`crate::error::Error`] naming `[tools.{name}] timeout_secs`.
    pub fn validate(&self, name: &str) -> Result<()> {
        if self
            .timeout_secs
            .is_some_and(|secs| !(1..=MAX_MEDIA_TIMEOUT_SECS).contains(&secs))
        {
            return Err(config_error(format!(
                "[tools.{name}] timeout_secs must be from 1 to {MAX_MEDIA_TIMEOUT_SECS}"
            )));
        }
        Ok(())
    }
}

/// The speech endpoint the `generate_speech` tool calls
/// (`[tools.generate_speech]`): the endpoint keys of
/// [`MediaToolConfig`] plus a default voice. Without an endpoint the
/// tool is not registered.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpeechToolConfig {
    /// The endpoint the tool calls, shared with the other media tools.
    #[serde(flatten)]
    pub endpoint: MediaToolConfig,
    /// The voice asked when a call names none, left out of the
    /// request when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
}

impl SpeechToolConfig {
    /// Whether the defaults are in effect, which leaves the tool
    /// unregistered.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Whether the table asks for the tool: set, `voice` included, and
    /// not `enabled = false`.
    #[must_use]
    pub fn is_wanted(&self) -> bool {
        self.endpoint.enabled && !self.is_default()
    }
}

pub use crate::tools_config::{
    EditToolConfig, GrepToolConfig, ListingToolConfig, ReadToolConfig, SearchEngine,
    ShellToolConfig, ToolToggle, ToolsConfig, WebFetchToolConfig, WebSearchConfig,
};

/// The directory `$env_var` names, with quotes and padding stripped
/// ([`crate::fsutil::unquote_and_trim`], so a quoted or spaced value
/// still names the real directory). `None` when the variable is
/// unset, empty, or cleans to empty.
fn env_base_dir(env_var: &str) -> Option<PathBuf> {
    let value = std::env::var(env_var).ok()?;
    let cleaned = crate::fsutil::unquote_and_trim(&value);
    (!cleaned.is_empty()).then(|| PathBuf::from(cleaned))
}

/// The platform base directory for a config-tier path, without the
/// `XDG_*` override: `~/.config` on Unix, `%APPDATA%` (the Roaming
/// profile) on Windows. `None` only when the platform has no home.
fn platform_config_base() -> Option<PathBuf> {
    #[cfg(windows)]
    return dirs::config_dir();
    #[cfg(unix)]
    return dirs::home_dir().map(|home| home.join(".config"));
    #[cfg(not(any(windows, unix)))]
    None
}

/// The platform base directory for a state-tier path, without the
/// `XDG_*` override: `~/.local/state` on Unix, `%LOCALAPPDATA%` on
/// Windows (which has no state directory of its own). `None` only
/// when the platform has no home.
fn platform_state_base() -> Option<PathBuf> {
    #[cfg(windows)]
    return dirs::data_local_dir();
    #[cfg(unix)]
    return dirs::home_dir().map(|home| home.join(".local").join("state"));
    #[cfg(not(any(windows, unix)))]
    None
}

impl Config {
    /// Path to the user config file: `$XDG_CONFIG_HOME/kage/config.toml`
    /// when `XDG_CONFIG_HOME` is set, else the platform directory
    /// (`~/.config/kage/config.toml` on Unix, `%APPDATA%\kage\config.toml`
    /// on Windows). `None` only when there is no home directory.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        let base = match env_base_dir("XDG_CONFIG_HOME") {
            Some(base) => base,
            None => platform_config_base()?,
        };
        Some(base.join("kage").join("config.toml"))
    }

    /// Check the security-sensitive tables a load must refuse:
    /// permission rules and shell policy. [`Self::load`],
    /// [`Self::load_default`] and [`Self::load_layered`] run this
    /// already, so broken rules stop the process at startup instead
    /// of failing open at call time. Repair-capable callers load with
    /// the `*_raw` loaders.
    ///
    /// # Errors
    ///
    /// A [`crate::error::Error`] describing the first broken rule.
    pub fn validate(&self) -> Result<()> {
        self.permissions.validate()?;
        self.shell.validate()?;
        self.tools.validate()
    }

    /// Load configuration from `path`, merging with defaults and env overrides.
    ///
    /// A missing file is not an error: defaults plus env are returned.
    /// The permission and shell tables are validated (see
    /// [`Self::validate`]); repair callers use [`Self::load_raw`].
    pub fn load(path: &Path) -> Result<Self> {
        let cfg = Self::load_raw(path)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load configuration from `path` like [`Self::load`], but without
    /// validating the permission and shell tables. Repair-capable
    /// callers (doctor, config editing) use this so a broken config
    /// stays loadable and fixable.
    pub fn load_raw(path: &Path) -> Result<Self> {
        Ok(Figment::new()
            .merge(Serialized::defaults(Self::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("KAGE_").split("__"))
            .extract()?)
    }

    /// Load configuration from [`Self::default_path`], or pure defaults plus
    /// env if no home directory is available. Validates like [`Self::load`].
    pub fn load_default() -> Result<Self> {
        let cfg = Self::load_default_raw()?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// [`Self::load_default`] without validation, for repair callers.
    pub fn load_default_raw() -> Result<Self> {
        match Self::default_path() {
            Some(p) => Self::load_raw(&p),
            None => Ok(Figment::new()
                .merge(Serialized::defaults(Self::default()))
                .merge(Env::prefixed("KAGE_").split("__"))
                .extract()?),
        }
    }

    /// Path to the project-local config inside `workdir`:
    /// `<workdir>/.kage/config.toml`.
    #[must_use]
    pub fn project_path(workdir: &Path) -> PathBuf {
        workdir.join(".kage").join("config.toml")
    }

    /// Directory for kage's mutable state: `$XDG_STATE_HOME/kage`
    /// when `XDG_STATE_HOME` is set, else the platform directory
    /// (`~/.local/state/kage` on Unix, `%LOCALAPPDATA%\kage` on
    /// Windows). `None` only when there is no home directory.
    #[must_use]
    pub fn state_dir() -> Option<PathBuf> {
        let base = match env_base_dir("XDG_STATE_HOME") {
            Some(base) => base,
            None => platform_state_base()?,
        };
        Some(base.join("kage"))
    }

    /// Load layered configuration: defaults < user file < project file < env.
    ///
    /// The project file is `<workdir>/.kage/config.toml`. Either file may be
    /// absent; only the layers that exist contribute. Env overrides win
    /// over both files just like in [`Self::load`], and the result is
    /// validated like [`Self::load`]; repair callers use
    /// [`Self::load_layered_raw`].
    ///
    /// The project file's `mcp`, `permissions` and
    /// `plugins.capabilities` tables are dropped unless the project is
    /// trusted (see [`crate::trust`]). The project file's `providers`,
    /// `acp`, `plugins.registry` and the media tables
    /// `tools.generate_image`, `tools.edit_image`,
    /// `tools.generate_speech` and `tools.generate_music` never apply:
    /// those are only read from the user config.
    /// `plugins.disabled` is the union of the user's list and the
    /// winning layer's, so a project can disable a plugin but never
    /// enable one the user disabled.
    pub fn load_layered(workdir: &Path) -> Result<Self> {
        let cfg = Self::load_layered_raw(workdir)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// [`Self::load_layered`] without validation, for repair callers.
    pub fn load_layered_raw(workdir: &Path) -> Result<Self> {
        let mut figment = Figment::new().merge(Serialized::defaults(Self::default()));
        if let Some(user) = Self::default_path() {
            figment = figment.merge(Toml::file(user));
        }
        figment = match crate::trust::filtered_project(workdir) {
            Some(filtered) => figment.merge(Toml::string(&filtered)),
            None => figment.merge(Toml::file(Self::project_path(workdir))),
        };
        figment = figment.merge(Env::prefixed("KAGE_").split("__"));
        let mut cfg: Self = figment.extract()?;
        // A later layer replaces an array, so a project's `disabled`
        // would otherwise re-enable what the user turned off.
        if let Some(user) = Self::default_path() {
            let user_disabled: Vec<String> = Figment::from(Toml::file(user))
                .extract_inner("plugins.disabled")
                .unwrap_or_default();
            for name in user_disabled {
                if !cfg.plugins.disabled.contains(&name) {
                    cfg.plugins.disabled.push(name);
                }
            }
        }
        Ok(cfg)
    }

    /// Set each `(key path, value)` pair in the TOML file at `path`,
    /// in place. A key path is the list of table names ending in the
    /// key, such as `["ui", "theme"]`.
    ///
    /// Only the named keys change: comments, formatting, key order and
    /// every other key and table stay as written. Missing tables are
    /// created, and a missing file (and its parent directory) is
    /// created holding just these keys.
    ///
    /// The write is atomic: the TOML is rendered to a sibling temp
    /// file and renamed over `path`, so an interrupted save never
    /// truncates an existing config. An empty key path is an error.
    ///
    /// # Errors
    ///
    /// An edit carries an empty key path, the file cannot be read,
    /// parsed or written, or a table on a key path is already set to
    /// a value that is not a table.
    pub fn save_keys(path: &Path, edits: &[(Vec<&str>, OptionValue)]) -> Result<()> {
        use toml_edit::DocumentMut;

        let mut doc = match std::fs::read_to_string(path) {
            Ok(existing) => existing
                .parse::<DocumentMut>()
                .map_err(|e| crate::error::Error::ConfigWrite(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
            Err(e) => return Err(e.into()),
        };
        for (keys, value) in edits {
            set_key(doc.as_table_mut(), keys, value)?;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::fsutil::atomic_write(path, doc.to_string().as_bytes())?;
        Ok(())
    }
}

/// Set `keys` to `value` under `root`, creating missing tables. An
/// existing value keeps its surrounding whitespace and trailing
/// comment.
fn set_key(root: &mut toml_edit::Table, keys: &[&str], value: &OptionValue) -> Result<()> {
    use toml_edit::{InlineTable, Item, Table, TableLike, Value};

    let Some((last, parents)) = keys.split_last() else {
        return Err(crate::error::Error::ConfigWrite(
            "empty key path".to_owned(),
        ));
    };
    let mut table: &mut dyn TableLike = root;
    let mut inline = false;
    for (depth, key) in parents.iter().enumerate() {
        let dotted = table.is_dotted();
        let item = table.entry(key).or_insert_with(|| {
            if inline {
                Item::Value(Value::InlineTable(InlineTable::new()))
            } else {
                let mut new = Table::new();
                new.set_implicit(true);
                new.set_dotted(dotted);
                Item::Table(new)
            }
        });
        inline = item.is_inline_table();
        table = item.as_table_like_mut().ok_or_else(|| {
            crate::error::Error::ConfigWrite(format!(
                "`{}` is not a table",
                parents[..=depth].join(".")
            ))
        })?;
    }
    let mut new = match value {
        OptionValue::Bool(b) => Value::from(*b),
        OptionValue::Int(n) => Value::from(*n),
        OptionValue::Float(x) => Value::from(*x),
        OptionValue::Str(s) => Value::from(s.as_str()),
    };
    match table.get_mut(last) {
        Some(Item::Value(old)) => {
            *new.decor_mut() = old.decor().clone();
            *old = new;
        }
        _ => {
            table.insert(last, Item::Value(new));
        }
    }
    Ok(())
}

/// Agent-loop tuning persisted under `[loop]`. Mirrors the subset of
/// `kage-loop`'s `LoopConfig` the user is allowed to set from config /
/// the settings dialog; the host maps it onto the real loop config at
/// startup.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopSettings {
    /// Trigger compaction once estimated token usage exceeds this
    /// fraction of the model's context window. Must be in `[0.0, 1.0]`:
    /// `0` disables compaction, and `1` compacts as soon as usage
    /// reaches the context window.
    pub compaction_threshold: f32,
}

impl Default for LoopSettings {
    fn default() -> Self {
        Self {
            compaction_threshold: 0.8,
        }
    }
}

/// Subagent limits persisted under `[agents]`. The option registry
/// validates the ranges (`agent_max_depth`, `agent_max_running`,
/// `swarm_max_items`, `swarm_timeout_ms`, `agent_model`,
/// `agent_thinking`, `agent_max_turns`, `agent_timeout`, `agent_budget`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentsConfig {
    /// How deep agents may nest. 0 turns the `agent` tool off, and 1
    /// lets only the main session start agents.
    pub max_depth: u8,
    /// How many agents run at once across the engine. Further agents
    /// wait until one finishes.
    pub max_running: u32,
    /// Most items one `swarm` call may run, one child agent per item.
    pub swarm_max_items: u32,
    /// Milliseconds one swarm child may run, from its run start.
    pub swarm_timeout_ms: u64,
    /// Model agents run when neither the call nor the definition names
    /// one, as `provider/model`. `inherit` uses the parent's model.
    pub model: String,
    /// Thinking level agents run with when neither the call nor the
    /// definition names one. `inherit` uses the parent's level.
    pub thinking: String,
    /// Turns one agent run may take. 0 means no limit.
    pub max_turns: u32,
    /// Seconds one agent run may take. 0 means no limit.
    pub timeout: u32,
    /// Tokens all agents of a session may use between two user
    /// prompts. 0 means no limit.
    pub budget: u64,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            max_depth: 1,
            max_running: 4,
            swarm_max_items: 32,
            swarm_timeout_ms: 7_200_000,
            model: "inherit".to_owned(),
            thinking: "inherit".to_owned(),
            max_turns: 0,
            timeout: 0,
            budget: 0,
        }
    }
}

/// LLM provider configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Model identifier in `provider/model` form.
    pub default_model: String,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            default_model: "anthropic/claude-sonnet-4-6".into(),
        }
    }
}

/// Custom providers and overrides for registered providers, loaded from
/// the `[providers]` section.
///
/// Two shapes share the section:
///
/// * `[providers.custom.<id>]` defines a brand-new provider with its own
///   id, base URL, and model list.
/// * `[providers.<registered-id>]` overrides settings of a provider kage
///   registers itself (base URL, key variable, extra headers).
///
/// The named `custom` field holds the first shape; every other key is
/// flattened into `overrides`, keyed by provider id.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    /// Providers defined in this config file, keyed by their id.
    pub custom: BTreeMap<String, CustomProviderConfig>,
    /// Overrides for providers kage registers itself, keyed by provider
    /// id (`[providers.deepseek]`).
    #[serde(flatten)]
    pub overrides: BTreeMap<String, BuiltinProviderOverride>,
}

impl ProvidersConfig {
    /// True when nothing is configured, so serializing skips the section.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.custom.is_empty() && self.overrides.is_empty()
    }

    /// Check the cross-references serde cannot express. `builtin_ids`
    /// lists the provider ids kage always registers (including `acp`);
    /// a custom provider must not shadow one. `overridable_ids` lists
    /// the ids a `[providers.<id>]` override may target: the built-in
    /// HTTP providers and the OpenAI-compatible ones, but not `acp`.
    ///
    /// Structural problems (missing `base_url`) are already load
    /// errors; an omitted model list loads as empty and fails here
    /// instead, so `kage models fetch` can still probe the endpoint.
    /// This catches the semantic ones so `kage` refuses to start on a
    /// config it would silently ignore.
    pub fn validate(&self, builtin_ids: &[&str], overridable_ids: &[&str]) -> Result<()> {
        for (id, cfg) in &self.custom {
            if id.is_empty()
                || !id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                return Err(config_error(format!(
                    "[providers.custom.{id}] id must be non-empty lowercase letters, digits, or dashes"
                )));
            }
            if builtin_ids.contains(&id.as_str()) {
                return Err(config_error(format!(
                    "[providers.custom.{id}] id conflicts with registered provider `{id}`; use [providers.{id}] to override it instead"
                )));
            }
            if cfg.models.is_empty() {
                return Err(config_error(format!(
                    "[providers.custom.{id}] must declare at least one [[providers.custom.{id}.models]] entry"
                )));
            }
            for model in &cfg.models {
                let Some(cost) = model.cost else { continue };
                let prices = [
                    ("input", Some(cost.input)),
                    ("output", Some(cost.output)),
                    ("cache_read", cost.cache_read),
                    ("cache_write", cost.cache_write),
                ];
                for (key, price) in prices {
                    if let Some(price) = price
                        && !(price.is_finite() && price >= 0.0)
                    {
                        return Err(config_error(format!(
                            "[providers.custom.{id}] model `{}` has cost.{key} = {price}; prices must be finite and not negative",
                            model.id
                        )));
                    }
                }
            }
        }
        for id in self.overrides.keys() {
            if !overridable_ids.contains(&id.as_str()) {
                return Err(config_error(format!(
                    "[providers.{id}] does not name a provider that can be overridden"
                )));
            }
        }
        Ok(())
    }
}

pub(crate) fn config_error(message: String) -> crate::error::Error {
    crate::error::Error::Config(Box::new(figment::Error::from(message)))
}

/// Which wire protocol a custom provider speaks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CustomProviderKind {
    /// OpenAI-compatible `POST {base_url}/chat/completions`.
    #[default]
    OpenAi,
    /// Anthropic Messages `POST {base_url}/v1/messages`.
    Anthropic,
    /// Gemini `POST {base_url}/v1beta/models/<model>:streamGenerateContent`.
    Gemini,
}

/// One custom provider definition (`[providers.custom.<id>]`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors independent flags of a config table"
)]
pub struct CustomProviderConfig {
    /// Which wire protocol the endpoint speaks. Defaults to `openai`.
    #[serde(default)]
    pub kind: CustomProviderKind,
    /// Endpoint base URL, without the protocol-specific path.
    pub base_url: String,
    /// Name shown in the model picker. Defaults to the provider id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Environment variable holding the API key. Defaults to
    /// `<ID>_API_KEY` uppercased; set it to `""` for endpoints that
    /// need no auth. As everywhere else, a set environment variable
    /// wins over a key stored via `kage auth login`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Where `kage models fetch <id>` reads the model list from, when
    /// the endpoint's own list will not do - a models.dev-style
    /// directory document, for instance. A key and headers are only
    /// sent when it shares the base URL's host and port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_url: Option<String>,
    /// Extra HTTP headers sent on every request, e.g. `Authorization`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Models this provider serves. Optional: a stanza without models
    /// declares the provider and lists nothing yet, ready for
    /// `kage models fetch <id>` to show what the endpoint serves.
    /// Selecting a model on a provider that lists none fails.
    #[serde(default)]
    pub models: Vec<CustomProviderModel>,
    /// Whether the endpoint accepts tool definitions. Defaults to true.
    #[serde(default = "default_true")]
    pub tool_use: bool,
    /// Whether the endpoint preserves thinking blocks across turns.
    /// Defaults to false.
    #[serde(default)]
    pub thinking: bool,
    /// Whether the endpoint supports prompt caching. Defaults to false.
    #[serde(default)]
    pub caching: bool,
    /// Whether an `anthropic` endpoint counts cached tokens inside
    /// `input_tokens`, as some gateways do. Set it when the context
    /// gauge reads about double. Defaults to false.
    #[serde(default)]
    pub input_includes_cache: bool,
}

impl CustomProviderConfig {
    /// The environment variable holding this provider's API key:
    /// `api_key_env` when set, else `<ID>_API_KEY` uppercased. Empty
    /// means the endpoint needs no key.
    #[must_use]
    pub fn key_env(&self, id: &str) -> String {
        self.api_key_env
            .clone()
            .unwrap_or_else(|| format!("{}_API_KEY", id.to_uppercase()))
    }
}

fn default_true() -> bool {
    true
}

/// One model a custom provider serves
/// (`[[providers.custom.<id>.models]]`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomProviderModel {
    /// Model id sent to the endpoint; addressable from kage as
    /// `<provider-id>:<this id>`.
    pub id: String,
    /// Display name shown in the picker.
    pub name: String,
    /// Context window in tokens, when known. Surfaces to the modeline
    /// like a catalog entry's context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// Maximum output tokens per turn, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u32>,
    /// Whether the model thinks. Left unset, thinking is unknown:
    /// explicit levels are sent as chosen and the automatic default
    /// sends nothing. `true` offers every level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// Effort values the model accepts (`none`, `minimal`, `low`,
    /// `medium`, `high`, `xhigh`, `max`, `ultra`). Implies `reasoning = true`
    /// and limits the levels to these.
    #[serde(default, skip_serializing_if = "Efforts::is_empty")]
    pub efforts: Efforts,
    /// Modalities the model accepts (`text`, `image`, `pdf`, `audio`,
    /// `video`). Empty when unknown.
    #[serde(default, skip_serializing_if = "Modalities::is_empty")]
    pub input: Modalities,
    /// Modalities the model produces (`text`, `image`, `audio`,
    /// `video`). Empty when unknown, which is read as text only.
    #[serde(default, skip_serializing_if = "Modalities::is_empty")]
    pub produces: Modalities,
    /// Assistant message field (`reasoning_content` or
    /// `reasoning_details`) an `openai` kind model reads its reasoning
    /// back from during a tool loop. Unset sends reasoning back as
    /// `<thinking>` text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interleaved: Option<ReasoningField>,
    /// Per-million-token prices in USD (`input`, `output`, and the
    /// optional `cache_read` and `cache_write`). Left unset, the cost of
    /// this model is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
}

impl CustomProviderModel {
    /// The thinking settings `reasoning` and `efforts` declare.
    #[must_use]
    pub fn reasoning(&self) -> Reasoning {
        match (self.reasoning, self.efforts.is_empty()) {
            (Some(false), _) => Reasoning::None,
            (_, false) => Reasoning::Effort {
                efforts: self.efforts,
                toggle: false,
            },
            (Some(true), true) => Reasoning::Budget {
                min: 0,
                max: None,
                toggle: true,
            },
            (None, true) => Reasoning::Unknown,
        }
    }
}

/// Settings overriding a provider kage registers itself
/// (`[providers.<id>]`). Fields left out keep the built-in value.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuiltinProviderOverride {
    /// Replace the provider's default base URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Replace the environment variable the API key is read from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Extra HTTP headers sent on every request to this provider.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// `openai` only: offer `OpenAI`'s hosted image tool to
    /// every model, so a model can answer with images. The tool runs at
    /// `OpenAI` and is billed on its own, so it is off unless set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub image_generation: bool,
}

/// User interface configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Theme name (loaded from bundled themes or
    /// `~/.config/kage/themes/<name>.toml`).
    pub theme: String,
    /// The theme `default` (and a client's System choice) picks on a
    /// dark background. Unset means `kage-shadow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_dark: Option<String>,
    /// The theme `default` (and a client's System choice) picks on a
    /// light background. Unset means `kage-dawn`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_light: Option<String>,
    /// Whether mouse events are captured by the TUI.
    pub mouse: bool,
    /// Prompt-input editing model.
    pub editor: EditorMode,
    /// Minimum content rows the input card holds (before chrome).
    /// Clamped to a sane floor of 1.
    pub input_min_lines: u16,
    /// Maximum content rows the input card grows to before it stops
    /// growing and scrolls internally. Lets a user enlarge the
    /// composing area; clamped to a sane ceiling.
    pub input_max_lines: u16,
    /// Default thinking level for new sessions: one of the
    /// `ThinkingLevel` ladder strings (`off`, `minimal`, `low`,
    /// `medium`, `high`, `xhigh`, `max`, `ultra`). Unset means automatic: high, or the
    /// nearest level the model accepts. Parsed and seeded at session
    /// start; the Shift+Tab cycle still overrides it per session. Kept
    /// as a raw string so the option registry can validate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    /// What prints to the terminal after exit: `full` for the whole
    /// transcript, `last` for the last prompt and what followed, or
    /// `none` for only the session path and a resume hint. Validated by
    /// the option registry, which keeps the default for any other value.
    pub transcript_on_exit: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "default".into(),
            theme_dark: None,
            theme_light: None,
            mouse: true,
            editor: EditorMode::default(),
            input_min_lines: 1,
            input_max_lines: 8,
            thinking_level: None,
            transcript_on_exit: "full".into(),
        }
    }
}

/// How the prompt input behaves.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditorMode {
    /// Modal editing in the vim style: a normal mode, an insert
    /// mode, and a visual mode, with motions, operators, and
    /// registers. Opt in with `editor = "vim"`.
    Vim,
    /// Always-editable, non-modal editing. Readline / Emacs keys do
    /// all editing (`Ctrl+A`/`E`/`W`/`U`/`K`/`Y`, `Ctrl+/`,
    /// `Alt+B`/`F`); `Esc` cancels the in-flight turn; the buffer
    /// scrolls with `PageUp` / `PageDown` and the mouse. No modal
    /// states. The default.
    #[default]
    Modeless,
}

/// Plugin loader configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    /// Override the default plugin directory `~/.config/kage/plugins/`.
    /// `~` expands to the home directory; relative paths resolve
    /// against the kage config directory.
    pub dir: Option<PathBuf>,
    /// Plugins that do not load, by name. A project config can add to
    /// the user's list but never shorten it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disabled: Vec<String>,
    /// Deprecated allowlist: when non-empty, only the plugins it names
    /// load. Superseded by [`Self::disabled`]; still honored so older
    /// configs keep working, and dropped by the first toggle that
    /// writes `disabled`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub enabled: Vec<String>,
    /// The index URL of the plugin registry `kage plugin` installs from.
    /// Unset means kura, kage's own. Read from the user config only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// Per-plugin elevated capability grants, keyed by plugin file
    /// stem (`[plugins.capabilities] my-plugin = ["session_write"]`).
    /// Plugins get only the sandboxed base surface by default; a
    /// capability listed here is exposed solely to the named plugin,
    /// and only after that plugin requests it via
    /// `kage.request_capabilities`. Unknown capability names are a
    /// hard error at startup, not silently ignored.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub capabilities: BTreeMap<String, Vec<String>>,
    /// Per-plugin settings, keyed by plugin file stem
    /// (`[plugins.config.my-plugin] key = "value"`). The named plugin
    /// reads its own slice via `kage.plugin_config()`; a plugin sees
    /// only its own entry, never another's. Arbitrary nested tables are
    /// allowed.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, serde_json::Value>,
}

impl PluginsConfig {
    /// Whether the plugin `name` loads under these lists.
    #[must_use]
    pub fn loads(&self, name: &str) -> bool {
        plugin_loads(&self.enabled, &self.disabled, name)
    }
}

/// Whether plugin `name` loads: it is not `disabled`, and the
/// deprecated `enabled` allowlist is empty or names it.
#[must_use]
pub fn plugin_loads(enabled: &[String], disabled: &[String], name: &str) -> bool {
    !disabled.iter().any(|entry| entry == name)
        && (enabled.is_empty() || enabled.iter().any(|entry| entry == name))
}

/// The `[shell]` table: shell tool policy.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    /// Program the shell tool runs commands with (`bash`, `fish`, an
    /// absolute path, ...). Defaults to `bash`. Known Windows shells get
    /// their own command flag; every other program is driven with `-c`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// Glob patterns of environment variable names the shell tool strips
    /// from its child's environment. Matched case-sensitively against the
    /// whole name, so `*_TOKEN` covers `GITHUB_TOKEN` and `PATH` names one
    /// variable exactly. Empty leaves the environment untouched.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scrub_env: Vec<String>,
}

impl ShellConfig {
    /// Whether nothing is configured.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.program.is_none() && self.scrub_env.is_empty()
    }

    /// Reject patterns that could never match: they are empty or do not
    /// compile as globs, which would silently disable a rule the user
    /// wrote.
    pub fn validate(&self) -> Result<()> {
        if self.program.as_ref().is_some_and(String::is_empty) {
            return Err(config_error("[shell] program must not be empty".to_owned()));
        }
        for pattern in &self.scrub_env {
            if pattern.is_empty() {
                return Err(config_error(
                    "[shell] scrub_env patterns must be non-empty".to_owned(),
                ));
            }
            if let Err(e) = globset::Glob::new(pattern) {
                return Err(config_error(format!(
                    "[shell] scrub_env pattern `{pattern}` does not compile: {e}"
                )));
            }
        }
        Ok(())
    }
}

/// The `[keybindings]` table: chord overrides plus the leader key and
/// the sequence timeout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeybindingsConfig {
    /// Map of keybinding chord (for example `<leader>cf`) to command name.
    pub bindings: BTreeMap<String, String>,
    /// The key `<leader>` expands to, as one key in keymap notation.
    #[serde(skip_serializing_if = "is_default_leader")]
    pub leader: String,
    /// How long a mapping that is also a prefix waits for more keys,
    /// in milliseconds.
    #[serde(skip_serializing_if = "is_default_timeoutlen")]
    pub timeoutlen: u32,
    /// Keys in the table other than the ones above, such as a chord
    /// written directly under `[keybindings]`. Kept so the loader can
    /// report them instead of dropping them silently.
    #[serde(flatten, skip_serializing)]
    pub unknown: BTreeMap<String, serde_json::Value>,
}

const DEFAULT_LEADER: &str = "\\";
const DEFAULT_TIMEOUTLEN: u32 = 1000;

fn is_default_leader(leader: &str) -> bool {
    leader == DEFAULT_LEADER
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if passes the field by reference"
)]
fn is_default_timeoutlen(timeoutlen: &u32) -> bool {
    *timeoutlen == DEFAULT_TIMEOUTLEN
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            bindings: BTreeMap::new(),
            leader: DEFAULT_LEADER.to_owned(),
            timeoutlen: DEFAULT_TIMEOUTLEN,
            unknown: BTreeMap::new(),
        }
    }
}

/// External ACP agents kage can drive as a provider. Each entry is
/// addressable as the model id `acp:<name>`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AcpConfig {
    /// Map of agent name to its launch spec.
    pub agents: BTreeMap<String, AcpAgent>,
}

/// How to launch one external ACP agent over stdio.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcpAgent {
    /// Executable to spawn.
    pub command: String,
    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// External MCP tool servers, keyed by name (`[mcp.servers.<name>]`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Map of server name to its launch spec.
    pub servers: BTreeMap<String, McpServer>,
    /// Allow MCP servers to drive `sampling/createMessage`: an LLM
    /// completion run against the host's default model and billed to the
    /// user. Off by default because it lets a server spend the user's
    /// token budget; opt in per project with `[mcp] allow_sampling =
    /// true`.
    #[serde(default)]
    pub allow_sampling: bool,
}

/// How to reach one external MCP server.
///
/// Two transports, selected by which field is set (exactly one of
/// `command` or `url` is required; setting both, or neither, is a
/// configuration error surfaced when the server is brought up):
///
/// * stdio: set `command` (and optionally `args` / `env`). kage spawns
///   the child and speaks JSON-RPC over its stdio.
/// * Streamable HTTP: set `url`. kage POSTs each JSON-RPC message to
///   that endpoint; `headers` are sent on every request (use it for
///   `Authorization`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpServer {
    /// Executable to spawn for a stdio transport (e.g. `npx`). Mutually
    /// exclusive with `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments passed to `command`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment variables for the child process.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Base URL for a remote Streamable HTTP transport. Mutually
    /// exclusive with `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra headers sent on every request (HTTP transport only),
    /// e.g. `Authorization`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// When `true`, the server is configured but not spawned/connected.
    #[serde(default)]
    pub disabled: bool,
    /// Tools of this server, by the server's own name, that are never
    /// registered: the model does not see them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_tools: Vec<String>,
    /// OAuth settings for an HTTP server that needs a login
    /// (`[mcp.servers.<name>.oauth]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuth>,
    /// Seconds the `initialize` handshake may wait for a silent server
    /// before kage gives up; 10 when unset. Raise it for a server whose
    /// cold start (a first `npx` download) is slow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_secs: Option<u64>,
}

/// OAuth settings for one HTTP MCP server. Every field is optional:
/// without them kage registers itself with the authorization server and
/// requests the scope the server advertises.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpOAuth {
    /// A pre-registered client id, which skips dynamic client
    /// registration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// The scope to request, overriding the one the server advertises.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[cfg(test)]
#[expect(
    clippy::result_large_err,
    reason = "figment::Jail closures must return figment::Error"
)]
mod tests {
    use super::*;

    use crate::test_support::process_globals;

    #[test]
    fn default_provider_model_is_anthropic_sonnet() {
        let cfg = Config::default();
        assert_eq!(cfg.provider.default_model, "anthropic/claude-sonnet-4-6");
    }

    #[test]
    fn missing_file_yields_defaults() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let cfg = Config::load(jail.directory().join("nope.toml").as_path()).unwrap();
            assert_eq!(cfg, Config::default());
            Ok(())
        });
    }

    #[test]
    fn quoted_and_spaced_config_env_yields_a_clean_default_path() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let home = jail.directory().to_path_buf();
            jail.set_env("HOME", home.to_string_lossy().as_ref());
            let quoted = format!("\"{}\"", home.join("cfg").display());
            jail.set_env("XDG_CONFIG_HOME", quoted.as_str());
            assert_eq!(
                Config::default_path(),
                Some(home.join("cfg").join("kage").join("config.toml"))
            );
            let spaced = format!(" {} ", home.join("cfg").display());
            jail.set_env("XDG_CONFIG_HOME", spaced.as_str());
            assert_eq!(
                Config::default_path(),
                Some(home.join("cfg").join("kage").join("config.toml"))
            );
            Ok(())
        });
    }

    #[test]
    fn an_empty_config_env_value_falls_back_to_home() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let home = jail.directory().to_path_buf();
            jail.set_env("HOME", home.to_string_lossy().as_ref());
            jail.set_env("XDG_CONFIG_HOME", "");
            // `dirs::home_dir()` follows `HOME` on Unix but reads the
            // Windows Known Folder API on Windows, where the jail's
            // `HOME` is inert; expect the platform's own directory.
            let base = if cfg!(windows) {
                dirs::config_dir().expect("test needs a config directory")
            } else {
                dirs::home_dir()
                    .expect("test needs a home directory")
                    .join(".config")
            };
            assert_eq!(
                Config::default_path(),
                Some(base.join("kage").join("config.toml"))
            );
            Ok(())
        });
    }

    #[test]
    fn quoted_and_spaced_state_env_yields_a_clean_state_dir() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let home = jail.directory().to_path_buf();
            jail.set_env("HOME", home.to_string_lossy().as_ref());
            let quoted = format!("\"{}\"", home.join("state").display());
            jail.set_env("XDG_STATE_HOME", quoted.as_str());
            assert_eq!(Config::state_dir(), Some(home.join("state").join("kage")));
            let spaced = format!(" {} ", home.join("state").display());
            jail.set_env("XDG_STATE_HOME", spaced.as_str());
            assert_eq!(Config::state_dir(), Some(home.join("state").join("kage")));
            jail.set_env("XDG_STATE_HOME", "");
            let base = if cfg!(windows) {
                dirs::data_local_dir().expect("test needs a local data directory")
            } else {
                dirs::home_dir()
                    .expect("test needs a home directory")
                    .join(".local")
                    .join("state")
            };
            assert_eq!(Config::state_dir(), Some(base.join("kage")));
            Ok(())
        });
    }

    #[test]
    fn acp_agents_default_empty_and_parse_from_table() {
        assert!(Config::default().acp.agents.is_empty());
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [acp.agents.claude-code]
                command = "npx"
                args = ["-y", "@zed-industries/claude-code-acp"]

                [acp.agents.claude-code.env]
                ANTHROPIC_API_KEY = "xxx"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let agent = cfg.acp.agents.get("claude-code").expect("agent parsed");
            assert_eq!(agent.command, "npx");
            assert_eq!(agent.args, ["-y", "@zed-industries/claude-code-acp"]);
            assert_eq!(
                agent.env.get("ANTHROPIC_API_KEY").map(String::as_str),
                Some("xxx")
            );
            Ok(())
        });
    }

    #[test]
    fn mcp_server_parses_stdio_and_http_transports() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [mcp.servers.local]
                command = "npx"
                args = ["-y", "server"]

                [mcp.servers.remote]
                url = "https://mcp.example.com/sse"
                [mcp.servers.remote.headers]
                Authorization = "Bearer x"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let local = cfg.mcp.servers.get("local").expect("stdio server");
            assert_eq!(local.command.as_deref(), Some("npx"));
            assert!(local.url.is_none());
            let remote = cfg.mcp.servers.get("remote").expect("http server");
            assert!(remote.command.is_none());
            assert_eq!(remote.url.as_deref(), Some("https://mcp.example.com/sse"));
            assert_eq!(
                remote.headers.get("Authorization").map(String::as_str),
                Some("Bearer x")
            );
            assert!(remote.oauth.is_none());
            Ok(())
        });
    }

    #[test]
    fn mcp_server_parses_the_oauth_table() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [mcp.servers.linear]
                url = "https://mcp.linear.app/mcp"

                [mcp.servers.linear.oauth]
                client_id = "kage-4f2c"
                scope = "read write"

                [mcp.servers.bare]
                url = "https://mcp.example.com/mcp"
                [mcp.servers.bare.oauth]
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let linear = cfg.mcp.servers.get("linear").expect("linear parsed");
            assert_eq!(
                linear.oauth,
                Some(McpOAuth {
                    client_id: Some("kage-4f2c".to_owned()),
                    scope: Some("read write".to_owned()),
                })
            );
            let bare = cfg.mcp.servers.get("bare").expect("bare parsed");
            assert_eq!(bare.oauth, Some(McpOAuth::default()));
            Ok(())
        });
    }

    #[test]
    fn plugins_config_parses_nested_tables_per_stem() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [plugins.config.my-plugin]
                api_url = "https://example.com"
                retries = 3

                [plugins.config.my-plugin.nested]
                flag = true
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let slice = cfg.plugins.config.get("my-plugin").expect("slice parsed");
            assert_eq!(slice["api_url"], serde_json::json!("https://example.com"));
            assert_eq!(slice["retries"], serde_json::json!(3));
            assert_eq!(slice["nested"]["flag"], serde_json::json!(true));
            Ok(())
        });
    }

    #[test]
    fn file_overrides_defaults() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [ui]
                theme = "tokyo-night"
                mouse = false

                [provider]
                default_model = "openai:gpt-4o"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert_eq!(cfg.ui.theme, "tokyo-night");
            assert!(!cfg.ui.mouse);
            assert_eq!(cfg.provider.default_model, "openai:gpt-4o");
            Ok(())
        });
    }

    #[test]
    fn env_overrides_file() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [ui]
                theme = "tokyo-night"
                "#,
            )?;
            jail.set_env("KAGE_UI__THEME", "catppuccin-mocha");
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert_eq!(cfg.ui.theme, "catppuccin-mocha");
            Ok(())
        });
    }

    #[test]
    fn a_project_adds_to_the_disabled_plugins_but_never_removes_one() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let home = jail.directory().to_path_buf();
            jail.set_env("HOME", home.to_string_lossy().as_ref());
            let xdg_config = home.join(".config");
            jail.set_env("XDG_CONFIG_HOME", xdg_config.to_string_lossy().as_ref());
            let io = |e: std::io::Error| figment::Error::from(e.to_string());
            std::fs::create_dir_all(xdg_config.join("kage")).map_err(io)?;
            std::fs::write(
                xdg_config.join("kage/config.toml"),
                "[plugins]\ndisabled = [\"spy\"]\n",
            )
            .map_err(io)?;
            let project = home.join("project");
            std::fs::create_dir_all(project.join(".kage")).map_err(io)?;
            std::fs::write(
                project.join(".kage/config.toml"),
                "[plugins]\ndisabled = [\"noisy\"]\n",
            )
            .map_err(io)?;
            let cfg =
                Config::load_layered(&project).map_err(|e| figment::Error::from(e.to_string()))?;
            assert_eq!(cfg.plugins.disabled, ["noisy", "spy"]);
            assert!(!cfg.plugins.loads("spy"));
            assert!(cfg.plugins.loads("other"));
            Ok(())
        });
    }

    #[test]
    fn project_file_overrides_user_file() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            // Pin XDG_CONFIG_HOME at the jail so default_path resolves
            // the user config beneath it deterministically, regardless
            // of any XDG_CONFIG_HOME inherited by the test runner.
            let home = jail.directory().to_path_buf();
            jail.set_env("HOME", home.to_string_lossy().as_ref());
            let xdg_config = home.join(".config");
            jail.set_env("XDG_CONFIG_HOME", xdg_config.to_string_lossy().as_ref());
            let user_cfg = xdg_config.join("kage");
            std::fs::create_dir_all(&user_cfg).map_err(|e| figment::Error::from(e.to_string()))?;
            std::fs::write(
                user_cfg.join("config.toml"),
                r#"
                [ui]
                theme = "user-theme"
                mouse = true
                "#,
            )
            .map_err(|e| figment::Error::from(e.to_string()))?;
            let project = home.join("project");
            std::fs::create_dir_all(project.join(".kage"))
                .map_err(|e| figment::Error::from(e.to_string()))?;
            std::fs::write(
                project.join(".kage").join("config.toml"),
                r#"
                [ui]
                theme = "project-theme"
                "#,
            )
            .map_err(|e| figment::Error::from(e.to_string()))?;

            let cfg =
                Config::load_layered(&project).map_err(|e| figment::Error::from(e.to_string()))?;
            assert_eq!(cfg.ui.theme, "project-theme");
            // mouse not set in project file -> inherited from user file.
            assert!(cfg.ui.mouse);
            Ok(())
        });
    }

    #[test]
    fn editor_mode_defaults_modeless_and_parses_vim() {
        assert_eq!(Config::default().ui.editor, EditorMode::Modeless);
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [ui]
                editor = "vim"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert_eq!(cfg.ui.editor, EditorMode::Vim);
            Ok(())
        });
    }

    #[test]
    fn editor_mode_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&EditorMode::Modeless).unwrap(),
            "\"modeless\""
        );
        assert_eq!(serde_json::to_string(&EditorMode::Vim).unwrap(), "\"vim\"");
    }

    #[test]
    fn loop_section_defaults_and_parses() {
        assert!((Config::default().loop_settings.compaction_threshold - 0.8).abs() < f32::EPSILON);
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r"
                [loop]
                compaction_threshold = 0.6
                ",
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert!((cfg.loop_settings.compaction_threshold - 0.6).abs() < f32::EPSILON);
            Ok(())
        });
    }

    #[test]
    fn leftover_sandbox_table_still_parses() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                "[sandbox]\nbackend = \"local\"\nnetwork_allowlist = [\"example.com\"]\n",
            )?;
            Config::load(jail.directory().join("config.toml").as_path())
                .expect("removed [sandbox] keys must stay tolerated in existing configs");
            Ok(())
        });
    }

    #[test]
    fn shell_scrub_env_parses_and_validates() {
        let cfg: Config = toml::from_str("[shell]\nscrub_env = [\"*_TOKEN\", \"PATH\"]\n").unwrap();
        assert_eq!(
            cfg.shell.scrub_env,
            vec!["*_TOKEN".to_owned(), "PATH".to_owned()]
        );
        cfg.shell.validate().unwrap();
    }

    #[test]
    fn shell_scrub_env_rejects_bad_globs() {
        let cfg: Config = toml::from_str("[shell]\nscrub_env = [\"[\"]\n").unwrap();
        let err = cfg.shell.validate().unwrap_err().to_string();
        assert!(err.contains("[shell] scrub_env"), "{err}");
    }

    #[test]
    fn load_rejects_bad_security_patterns_and_load_raw_keeps_them_loadable() {
        let _globals = process_globals();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[permissions.tools.shell]\ndeny = [\"[\"]\n").unwrap();
        let err = Config::load(&path).expect_err("bad deny pattern must fail the load");
        assert!(err.to_string().contains("does not compile"), "{err}");
        let cfg = Config::load_raw(&path).unwrap();
        assert_eq!(cfg.permissions.tools["shell"].deny, ["[".to_owned()]);

        std::fs::write(&path, "[shell]\nscrub_env = [\"[\"]\n").unwrap();
        let err = Config::load(&path).expect_err("bad scrub pattern must fail the load");
        assert!(err.to_string().contains("does not compile"), "{err}");
        let cfg = Config::load_raw(&path).unwrap();
        assert_eq!(cfg.shell.scrub_env, ["[".to_owned()]);
    }

    #[test]
    fn load_default_rejects_bad_security_patterns_in_the_user_file() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let home = jail.directory().to_path_buf();
            jail.set_env("HOME", home.to_string_lossy().as_ref());
            let xdg_config = home.join(".config");
            jail.set_env("XDG_CONFIG_HOME", xdg_config.to_string_lossy().as_ref());
            let user_cfg = xdg_config.join("kage");
            std::fs::create_dir_all(&user_cfg).map_err(|e| figment::Error::from(e.to_string()))?;

            std::fs::write(
                user_cfg.join("config.toml"),
                "[permissions.tools.shell]\ndeny = [\"[\"]\n",
            )
            .map_err(|e| figment::Error::from(e.to_string()))?;
            let err = Config::load_default().expect_err("bad deny pattern must fail the load");
            assert!(err.to_string().contains("does not compile"), "{err}");

            std::fs::write(
                user_cfg.join("config.toml"),
                "[shell]\nscrub_env = [\"[\"]\n",
            )
            .map_err(|e| figment::Error::from(e.to_string()))?;
            let err = Config::load_default().expect_err("bad scrub pattern must fail the load");
            assert!(err.to_string().contains("does not compile"), "{err}");
            assert!(
                Config::load_default_raw().is_ok(),
                "the raw loader must keep a broken config repairable"
            );
            Ok(())
        });
    }

    #[test]
    fn load_layered_rejects_bad_security_patterns_in_the_user_file() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let home = jail.directory().to_path_buf();
            jail.set_env("HOME", home.to_string_lossy().as_ref());
            let xdg_config = home.join(".config");
            jail.set_env("XDG_CONFIG_HOME", xdg_config.to_string_lossy().as_ref());
            let user_cfg = xdg_config.join("kage");
            std::fs::create_dir_all(&user_cfg).map_err(|e| figment::Error::from(e.to_string()))?;
            let project = home.join("project");
            std::fs::create_dir_all(&project).map_err(|e| figment::Error::from(e.to_string()))?;

            std::fs::write(
                user_cfg.join("config.toml"),
                "[permissions.tools.shell]\ndeny = [\"[\"]\n",
            )
            .map_err(|e| figment::Error::from(e.to_string()))?;
            let err =
                Config::load_layered(&project).expect_err("bad deny pattern must fail the load");
            assert!(err.to_string().contains("does not compile"), "{err}");

            std::fs::write(
                user_cfg.join("config.toml"),
                "[shell]\nscrub_env = [\"[\"]\n",
            )
            .map_err(|e| figment::Error::from(e.to_string()))?;
            let err =
                Config::load_layered(&project).expect_err("bad scrub pattern must fail the load");
            assert!(err.to_string().contains("does not compile"), "{err}");
            assert!(
                Config::load_layered_raw(&project).is_ok(),
                "the raw loader must keep a broken config repairable"
            );
            Ok(())
        });
    }

    #[test]
    fn shell_program_parses() {
        let cfg: Config = toml::from_str("[shell]\nprogram = \"fish\"\n").unwrap();
        assert_eq!(cfg.shell.program.as_deref(), Some("fish"));
        assert!(cfg.shell.validate().is_ok());
    }

    #[test]
    fn shell_program_rejects_empty() {
        let cfg: Config = toml::from_str("[shell]\nprogram = \"\"\n").unwrap();
        let err = cfg.shell.validate().unwrap_err().to_string();
        assert!(err.contains("[shell] program"), "{err}");
    }

    const HAND_WRITTEN: &str = r#"# my kage config

[provider]
default_model = "fake:small"  # keyless local model

[ui]
# picked by hand
theme = "default"

[providers.custom.fake]
base_url = "http://127.0.0.1:8080/v1"
api_key_env = ""

[[providers.custom.fake.models]]
id = "small"
name = "Small"

[[providers.custom.fake.models]]
id = "large"
name = "Large"
context = 32768

[mcp.servers.files]
command = "mcp-files"
args = ["--root", "."]

[permissions.tools.write]
default = "ask"   # keep asking
"#;

    fn str_value(s: &str) -> OptionValue {
        OptionValue::Str(s.to_owned())
    }

    #[test]
    fn save_keys_changes_only_the_named_keys() {
        let _globals = process_globals();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, HAND_WRITTEN).unwrap();
        Config::save_keys(
            &path,
            &[
                (vec!["ui", "theme"], str_value("tokyo-night")),
                (
                    vec!["permissions", "tools", "write", "default"],
                    str_value("allow"),
                ),
            ],
        )
        .unwrap();
        let expected = HAND_WRITTEN
            .replace("theme = \"default\"", "theme = \"tokyo-night\"")
            .replace("default = \"ask\"", "default = \"allow\"");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    }

    #[test]
    fn save_keys_appends_missing_tables_after_the_hand_written_ones() {
        let _globals = process_globals();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, HAND_WRITTEN).unwrap();
        Config::save_keys(
            &path,
            &[
                (
                    vec!["permissions", "tools", "shell", "default"],
                    str_value("allow"),
                ),
                (
                    vec!["loop", "compaction_threshold"],
                    OptionValue::Float(0.6),
                ),
            ],
        )
        .unwrap();
        let expected = format!(
            "{HAND_WRITTEN}\n[permissions.tools.shell]\ndefault = \"allow\"\n\n[loop]\ncompaction_threshold = 0.6\n"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.providers.custom["fake"].models.len(), 2);
        assert!((cfg.loop_settings.compaction_threshold - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn create_dir_all_of_an_empty_parent_is_a_noop() {
        // `save_keys` on a bare filename gets `Path::parent() == Some("")`;
        // std special-cases the empty path on every platform, so the save
        // proceeds into the current directory. Pins the contract the
        // `parent()` call relies on (plans/verify/38 F9 settler).
        std::fs::create_dir_all(Path::new("")).unwrap();
    }

    #[test]
    fn save_keys_creates_a_missing_file_with_only_those_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        Config::save_keys(
            &path,
            &[
                (vec!["ui", "mouse"], OptionValue::Bool(false)),
                (vec!["agents", "max_running"], OptionValue::Int(2)),
            ],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ui]\nmouse = false\n\n[agents]\nmax_running = 2\n"
        );
        assert!(!path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn save_keys_edits_inline_and_dotted_tables_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "ui.theme = \"default\"\n\n[permissions]\ntools = { grep = { default = \"ask\" } }\n",
        )
        .unwrap();
        Config::save_keys(
            &path,
            &[
                (vec!["ui", "theme"], str_value("ayu")),
                (
                    vec!["permissions", "tools", "shell", "default"],
                    str_value("allow"),
                ),
                (
                    vec!["permissions", "tools", "edit", "default"],
                    str_value("allow"),
                ),
            ],
        )
        .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.starts_with(
                "ui.theme = \"ayu\"\n\n[permissions]\ntools = { grep = { default = \"ask\" }"
            ),
            "{body}"
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(
            cfg.permissions.check("edit", ""),
            crate::permissions::PermissionAction::Allow
        );
    }

    #[test]
    fn save_keys_refuses_to_replace_a_value_with_a_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "ui = 3\n").unwrap();
        let err = Config::save_keys(&path, &[(vec!["ui", "theme"], str_value("ayu"))])
            .expect_err("ui is not a table");
        assert!(err.to_string().contains("`ui` is not a table"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ui = 3\n");
    }

    #[test]
    fn save_keys_refuses_an_empty_key_path_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let err =
            Config::save_keys(&path, &[(vec![], str_value("ayu"))]).expect_err("empty key path");
        assert!(err.to_string().contains("empty key path"), "{err}");
        assert!(!path.exists(), "no file is created for a rejected edit");
    }

    fn sample_custom_provider() -> CustomProviderConfig {
        CustomProviderConfig {
            kind: CustomProviderKind::OpenAi,
            base_url: "https://api.together.xyz/v1".into(),
            display_name: None,
            api_key_env: None,
            models_url: None,
            headers: BTreeMap::new(),
            models: vec![CustomProviderModel {
                id: "meta-llama/Llama-3.3-70B-Instruct-Turbo".into(),
                name: "Llama 3.3 70B".into(),
                context: Some(131_072),
                ..CustomProviderModel::default()
            }],
            tool_use: true,
            thinking: false,
            caching: false,
            input_includes_cache: false,
        }
    }

    #[test]
    fn keybindings_leader_and_timeoutlen_skip_defaults() {
        let body = toml::to_string(&Config::default()).unwrap();
        assert!(!body.contains("leader"), "{body}");
        assert!(!body.contains("timeoutlen"), "{body}");

        let mut cfg = Config::default();
        cfg.keybindings.leader = ",".to_owned();
        cfg.keybindings.timeoutlen = 300;
        let body = toml::to_string(&cfg).unwrap();
        assert!(body.contains("leader = \",\""), "{body}");
        assert!(body.contains("timeoutlen = 300"), "{body}");
    }

    #[test]
    fn keybindings_leader_and_timeoutlen_parse() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [keybindings]
                leader = "<Space>"
                timeoutlen = 250
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert_eq!(cfg.keybindings.leader, "<Space>");
            assert_eq!(cfg.keybindings.timeoutlen, 250);
            assert!(cfg.keybindings.bindings.is_empty());
            assert!(cfg.keybindings.unknown.is_empty());
            Ok(())
        });
    }

    #[test]
    fn keybindings_keep_unknown_keys_for_reporting() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [keybindings]
                "ctrl+t" = "theme set tokyo-night"
                bindings = { "ctrl+k" = "compact" }
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert_eq!(cfg.keybindings.bindings.len(), 1);
            assert_eq!(
                cfg.keybindings.unknown.keys().collect::<Vec<_>>(),
                ["ctrl+t"]
            );
            let body = toml::to_string(&cfg).unwrap();
            assert!(!body.contains("ctrl+t"), "{body}");
            Ok(())
        });
    }

    #[test]
    fn providers_section_defaults_empty() {
        let cfg = Config::default();
        assert!(cfg.providers.is_default());
        assert!(cfg.providers.custom.is_empty());
        assert!(cfg.providers.overrides.is_empty());
        let body = toml::to_string(&cfg).unwrap();
        assert!(!body.contains("[providers"), "{body}");
    }

    #[test]
    fn permissions_section_defaults_empty_and_hidden() {
        let cfg = Config::default();
        assert!(cfg.permissions.is_default());
        let body = toml::to_string(&cfg).unwrap();
        assert!(!body.contains("[permissions"), "{body}");
    }

    #[test]
    fn permissions_table_parses_under_config() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [permissions]
                confine_paths = true

                [permissions.tools.shell]
                default = "ask"
                allow = ["git *"]
                deny = ["rm -rf *"]
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert!(cfg.permissions.confine_paths);
            assert_eq!(
                cfg.permissions.check("shell", "git status"),
                crate::permissions::PermissionAction::Allow
            );
            assert_eq!(
                cfg.permissions.check("shell", "rm -rf /"),
                crate::permissions::PermissionAction::Deny
            );
            Ok(())
        });
    }

    #[test]
    fn thinking_level_parses_and_roundtrips() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [ui]
                thinking_level = "medium"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert_eq!(cfg.ui.thinking_level.as_deref(), Some("medium"));
            let body = toml::to_string(&cfg).unwrap();
            assert!(body.contains("thinking_level = \"medium\""), "{body}");
            Ok(())
        });
    }

    #[test]
    fn providers_parses_custom_and_override_tables_side_by_side() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [providers.custom.together]
                base_url = "https://api.together.xyz/v1"

                [[providers.custom.together.models]]
                id = "meta-llama/Llama-3.3-70B-Instruct-Turbo"
                name = "Llama 3.3 70B"
                context = 131072

                [providers.deepseek]
                base_url = "https://relay.example.com/v1"
                [providers.deepseek.headers]
                X-Team = "infra"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let together = cfg.providers.custom.get("together").expect("custom parsed");
            assert_eq!(
                together.kind,
                CustomProviderKind::OpenAi,
                "kind defaults to openai"
            );
            assert_eq!(together.base_url, "https://api.together.xyz/v1");
            assert!(together.tool_use, "tool_use defaults to true");
            assert!(!together.thinking);
            assert!(!together.caching);
            assert_eq!(together.models.len(), 1);
            assert_eq!(
                together.models[0].id,
                "meta-llama/Llama-3.3-70B-Instruct-Turbo"
            );
            assert_eq!(together.models[0].name, "Llama 3.3 70B");
            assert_eq!(together.models[0].context, Some(131_072));
            assert_eq!(together.models[0].max_output, None);
            let deepseek = cfg
                .providers
                .overrides
                .get("deepseek")
                .expect("override parsed via flatten");
            assert_eq!(
                deepseek.base_url.as_deref(),
                Some("https://relay.example.com/v1")
            );
            assert_eq!(deepseek.api_key_env, None);
            assert_eq!(
                deepseek.headers.get("X-Team").map(String::as_str),
                Some("infra")
            );
            assert!(!cfg.providers.custom.contains_key("deepseek"));
            Ok(())
        });
    }

    #[test]
    fn custom_models_declare_reasoning_efforts_and_input() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [providers.custom.local]
                base_url = "http://localhost:8080/v1"

                [[providers.custom.local.models]]
                id = "thinker"
                name = "Thinker"
                efforts = ["low", "high"]
                input = ["text", "image"]
                produces = ["text", "image"]
                interleaved = "reasoning_content"

                [[providers.custom.local.models]]
                id = "plain"
                name = "Plain"
                reasoning = false

                [[providers.custom.local.models]]
                id = "mystery"
                name = "Mystery"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let models = &cfg.providers.custom["local"].models;
            assert_eq!(
                models[0].reasoning(),
                Reasoning::Effort {
                    efforts: Efforts::of(&[
                        crate::thinking::Effort::Low,
                        crate::thinking::Effort::High
                    ]),
                    toggle: false,
                }
            );
            assert!(models[0].input.contains(crate::modality::Modality::Image));
            assert!(
                models[0]
                    .produces
                    .contains(crate::modality::Modality::Image)
            );
            assert!(models[1].produces.is_empty());
            assert_eq!(
                models[0].interleaved,
                Some(ReasoningField::ReasoningContent)
            );
            assert_eq!(models[1].interleaved, None);
            assert_eq!(models[1].reasoning(), Reasoning::None);
            assert_eq!(models[2].reasoning(), Reasoning::Unknown);
            assert!(models[2].input.is_empty());
            Ok(())
        });
    }

    #[test]
    fn custom_model_cost_parses_and_bad_prices_fail_validation() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [providers.custom.local]
                base_url = "http://localhost:8080/v1"

                [[providers.custom.local.models]]
                id = "priced"
                name = "Priced"
                cost = { input = 0.27, output = 1.10, cache_read = 0.07, cache_write = 0.0 }

                [[providers.custom.local.models]]
                id = "partial"
                name = "Partial"
                cost = { input = 1, output = 2 }
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let models = &cfg.providers.custom["local"].models;
            assert_eq!(
                models[0].cost,
                Some(ModelCost {
                    input: 0.27,
                    output: 1.10,
                    cache_read: Some(0.07),
                    cache_write: Some(0.0),
                })
            );
            assert_eq!(
                models[1].cost,
                Some(ModelCost {
                    input: 1.0,
                    output: 2.0,
                    cache_read: None,
                    cache_write: None,
                })
            );
            cfg.providers.validate(&[], &[]).expect("valid prices pass");

            for (cost, key) in [
                ("{ input = -0.5, output = 1 }", "cost.input"),
                ("{ input = 1, output = nan }", "cost.output"),
                (
                    "{ input = 1, output = 1, cache_read = inf }",
                    "cost.cache_read",
                ),
                (
                    "{ input = 1, output = 1, cache_write = -1 }",
                    "cost.cache_write",
                ),
            ] {
                jail.create_file(
                    "bad.toml",
                    &format!(
                        r#"
                        [providers.custom.local]
                        base_url = "http://localhost:8080/v1"

                        [[providers.custom.local.models]]
                        id = "bad"
                        name = "Bad"
                        cost = {cost}
                        "#
                    ),
                )?;
                let cfg = Config::load(jail.directory().join("bad.toml").as_path()).unwrap();
                let err = cfg
                    .providers
                    .validate(&[], &[])
                    .expect_err("invalid price must be rejected");
                let message = err.to_string();
                assert!(message.contains("[providers.custom.local]"), "{message}");
                assert!(message.contains("`bad`"), "{message}");
                assert!(message.contains(key), "{message}");
                assert!(message.contains("finite and not negative"), "{message}");
            }

            jail.create_file(
                "missing.toml",
                r#"
                [providers.custom.local]
                base_url = "http://localhost:8080/v1"

                [[providers.custom.local.models]]
                id = "bad"
                name = "Bad"
                cost = { input = 1 }
                "#,
            )?;
            let err = Config::load(jail.directory().join("missing.toml").as_path())
                .expect_err("cost without output must fail the load");
            assert!(err.to_string().contains("output"), "{err}");
            Ok(())
        });
    }

    #[test]
    fn providers_custom_requires_base_url_and_defaults_models() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "no-base-url.toml",
                r#"
                [providers.custom.broken]
                display_name = "No base URL"
                "#,
            )?;
            let err = Config::load(jail.directory().join("no-base-url.toml").as_path())
                .expect_err("missing base_url must fail the load");
            assert!(err.to_string().contains("base_url"), "{err}");

            jail.create_file(
                "no-models.toml",
                r#"
                [providers.custom.broken]
                base_url = "https://api.example.com/v1"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("no-models.toml").as_path())
                .expect("models are optional");
            let broken = cfg.providers.custom.get("broken").expect("provider parsed");
            assert!(broken.models.is_empty(), "{broken:?}");
            Ok(())
        });
    }

    #[test]
    fn providers_env_overrides_file_values() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [providers.deepseek]
                base_url = "https://api.deepseek.com/v1"
                "#,
            )?;
            jail.set_env(
                "KAGE_PROVIDERS__DEEPSEEK__BASE_URL",
                "https://relay.example.com/v1",
            );
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let deepseek = cfg
                .providers
                .overrides
                .get("deepseek")
                .expect("override parsed");
            assert_eq!(
                deepseek.base_url.as_deref(),
                Some("https://relay.example.com/v1")
            );
            Ok(())
        });
    }

    #[test]
    fn providers_validate_accepts_well_formed_config() {
        let providers = ProvidersConfig {
            custom: BTreeMap::from([("together".to_owned(), sample_custom_provider())]),
            overrides: BTreeMap::from([(
                "deepseek".to_owned(),
                BuiltinProviderOverride {
                    base_url: Some("https://relay.example.com/v1".into()),
                    api_key_env: None,
                    headers: BTreeMap::from([("X-Team".to_owned(), "infra".to_owned())]),
                    image_generation: false,
                },
            )]),
        };
        providers
            .validate(
                &["acp", "anthropic", "openai", "google"],
                &["anthropic", "openai", "google", "deepseek"],
            )
            .expect("well-formed providers validate");
    }

    #[test]
    fn providers_validate_rejects_invalid_custom_id() {
        for id in ["", "DeepSeek", "has_underscore", "has space", "has.dot"] {
            let providers = ProvidersConfig {
                custom: BTreeMap::from([(id.to_owned(), sample_custom_provider())]),
                overrides: BTreeMap::new(),
            };
            let err = providers
                .validate(&["anthropic"], &["anthropic"])
                .expect_err("invalid custom id must be rejected");
            assert!(err.to_string().contains("id must be"), "{err}");
        }
    }

    #[test]
    fn providers_validate_rejects_custom_id_shadowing_registered() {
        let providers = ProvidersConfig {
            custom: BTreeMap::from([("openai".to_owned(), sample_custom_provider())]),
            overrides: BTreeMap::new(),
        };
        let err = providers
            .validate(&["openai", "acp"], &["openai"])
            .expect_err("custom id must not shadow a registered provider");
        assert!(err.to_string().contains("conflicts"), "{err}");
    }

    #[test]
    fn providers_validate_requires_models() {
        let mut provider = sample_custom_provider();
        provider.models.clear();
        let providers = ProvidersConfig {
            custom: BTreeMap::from([("together".to_owned(), provider)]),
            overrides: BTreeMap::new(),
        };
        let err = providers
            .validate(&[], &[])
            .expect_err("custom provider without models must be rejected");
        assert!(err.to_string().contains("models"), "{err}");
    }

    #[test]
    fn providers_validate_rejects_unknown_override_id() {
        let providers = ProvidersConfig {
            custom: BTreeMap::new(),
            overrides: BTreeMap::from([(
                "not-a-provider".to_owned(),
                BuiltinProviderOverride::default(),
            )]),
        };
        let err = providers
            .validate(&["openai"], &["openai"])
            .expect_err("unknown override id must be rejected");
        assert!(err.to_string().contains("not-a-provider"), "{err}");

        let providers = ProvidersConfig {
            custom: BTreeMap::new(),
            overrides: BTreeMap::from([("acp".to_owned(), BuiltinProviderOverride::default())]),
        };
        let err = providers
            .validate(&["acp"], &["openai"])
            .expect_err("acp must not be overridable");
        assert!(err.to_string().contains("acp"), "{err}");
    }

    #[test]
    fn providers_serialize_then_load_roundtrips() {
        let _globals = process_globals();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.providers
            .custom
            .insert("together".to_owned(), sample_custom_provider());
        cfg.providers.overrides.insert(
            "deepseek".to_owned(),
            BuiltinProviderOverride {
                base_url: Some("https://relay.example.com/v1".into()),
                api_key_env: Some("DEEPSEEK_API_KEY".into()),
                headers: BTreeMap::from([("X-Team".to_owned(), "infra".to_owned())]),
                image_generation: false,
            },
        );
        let body = toml::to_string(&cfg).unwrap();
        assert!(body.contains("[providers.custom.together]"), "{body}");
        assert!(
            body.contains("[[providers.custom.together.models]]"),
            "{body}"
        );
        assert!(body.contains("[providers.deepseek]"), "{body}");
        std::fs::write(&path, body).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn tools_rename_parses_from_the_toml_table() {
        let _globals = process_globals();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[tools.rename]\nshell = \"run_command\"\nweb_fetch = \"browse\"\n",
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(
            cfg.tools.rename.get("shell").map(String::as_str),
            Some("run_command")
        );
        assert_eq!(
            cfg.tools.rename.get("web_fetch").map(String::as_str),
            Some("browse")
        );
        // The default config carries no renames.
        assert!(Config::default().tools.is_default());
    }

    #[test]
    fn tools_edit_image_roundtrips_and_hides_by_default() {
        let _globals = process_globals();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.tools.edit_image = MediaToolConfig {
            base_url: Some("https://api.example.com/v1".into()),
            api_key_env: Some("EDIT_IMAGE_API_KEY".into()),
            headers: BTreeMap::from([("X-Team".to_owned(), "infra".to_owned())]),
            model: Some("image-large".into()),
            timeout_secs: Some(60),
            ..MediaToolConfig::default()
        };
        assert!(!cfg.tools.is_default());
        std::fs::write(&path, toml::to_string(&cfg).unwrap()).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, cfg);

        let default_body = toml::to_string(&Config::default()).unwrap();
        assert!(!default_body.contains("edit_image"), "{default_body}");

        let mut only_model = Config::default();
        only_model.tools.edit_image.model = Some("image-large".into());
        assert!(!only_model.tools.is_default());
    }

    #[test]
    fn tools_generate_speech_roundtrips_and_hides_by_default() {
        let _globals = process_globals();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.tools.generate_speech = SpeechToolConfig {
            endpoint: MediaToolConfig {
                base_url: Some("https://speech.example/v1".into()),
                api_key_env: Some("GENERATE_SPEECH_API_KEY".into()),
                headers: BTreeMap::from([("X-Team".to_owned(), "infra".to_owned())]),
                model: Some("speech-small".into()),
                timeout_secs: Some(90),
                ..MediaToolConfig::default()
            },
            voice: Some("alloy".into()),
        };
        assert!(!cfg.tools.is_default());
        std::fs::write(&path, toml::to_string(&cfg).unwrap()).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, cfg);

        let default_body = toml::to_string(&Config::default()).unwrap();
        assert!(!default_body.contains("generate_speech"), "{default_body}");

        let mut only_voice = Config::default();
        only_voice.tools.generate_speech.voice = Some("alloy".into());
        assert!(!only_voice.tools.is_default());
    }

    #[test]
    fn tools_generate_speech_flat_keys_parse_beside_voice() {
        let cfg: Config = toml::from_str(
            "[tools.generate_speech]\n\
             base_url = \"https://speech.example/v1\"\n\
             model = \"speech-small\"\n\
             timeout_secs = 90\n\
             voice = \"alloy\"\n",
        )
        .unwrap();
        let speech = &cfg.tools.generate_speech;
        assert_eq!(
            speech.endpoint.base_url.as_deref(),
            Some("https://speech.example/v1")
        );
        assert_eq!(speech.endpoint.model.as_deref(), Some("speech-small"));
        assert_eq!(speech.endpoint.timeout_secs, Some(90));
        assert_eq!(speech.voice.as_deref(), Some("alloy"));
    }

    /// A voice-only table still asks for the tool, so its missing
    /// endpoint is reported; `enabled = false` is what turns it off.
    #[test]
    fn a_speech_table_is_wanted_until_disabled() {
        let speech = |toml: &str| {
            toml::from_str::<Config>(toml)
                .unwrap()
                .tools
                .generate_speech
        };
        assert!(!speech("").is_wanted());
        assert!(speech("[tools.generate_speech]\nvoice = \"alloy\"\n").is_wanted());
        assert!(
            !speech("[tools.generate_speech]\nvoice = \"alloy\"\nenabled = false\n").is_wanted()
        );
    }
}
