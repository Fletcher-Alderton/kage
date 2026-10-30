//! The `[tools]` table: built-in tool settings.
//!
//! Every tool takes `enabled` and `description` under `[tools.<name>]`:
//!
//! ```toml
//! [tools.web_search]
//! enabled = false                # never offered to the model
//!
//! [tools.shell]
//! description = "Run a command. Prefer `rg` over `grep`."
//! timeout_ms = 300000            # default timeout for a command
//! ```
//!
//! The built-in file, shell and web tools also take their limits (see
//! the per-tool structs). Any other registered tool, such as `agent`,
//! `ask_user_question`, a plugin tool or an MCP tool
//! (`<server>__<tool>`), takes `enabled` and `description` only.
//!
//! Keys are the tool's real name, never a `[tools.rename]` target.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::config::{MediaToolConfig, SpeechToolConfig, config_error};
use crate::error::Result;

/// The largest `max_bytes` a tool table may set. Any context window is
/// far below it, and it keeps a tool's `budget + 1` from overflowing a
/// `usize` on a 32-bit build.
const MAX_TOOL_BYTES: u64 = 1 << 30;

fn default_true() -> bool {
    true
}

// serde's `skip_serializing_if` hands the field over by reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(value: &bool) -> bool {
    *value
}

/// The `[tools]` table.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// Advertise a tool under another name (`[tools.rename]`,
    /// `shell = "run_command"`): the model sees only the new name,
    /// while execution and permission rules keep working under the
    /// real one. Renames of unknown tools are ignored.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub rename: BTreeMap<String, String>,
    /// `[tools.read]`.
    #[serde(skip_serializing_if = "ReadToolConfig::is_default")]
    pub read: ReadToolConfig,
    /// `[tools.edit]`.
    #[serde(skip_serializing_if = "EditToolConfig::is_default")]
    pub edit: EditToolConfig,
    /// `[tools.grep]`.
    #[serde(skip_serializing_if = "GrepToolConfig::is_default")]
    pub grep: GrepToolConfig,
    /// `[tools.find]`.
    #[serde(skip_serializing_if = "ListingToolConfig::is_default")]
    pub find: ListingToolConfig,
    /// `[tools.ls]`.
    #[serde(skip_serializing_if = "ListingToolConfig::is_default")]
    pub ls: ListingToolConfig,
    /// `[tools.shell]`. The program and the environment scrub stay in
    /// the top-level `[shell]` table.
    #[serde(skip_serializing_if = "ShellToolConfig::is_default")]
    pub shell: ShellToolConfig,
    /// `[tools.web_fetch]`.
    #[serde(skip_serializing_if = "WebFetchToolConfig::is_default")]
    pub web_fetch: WebFetchToolConfig,
    /// `[tools.web_search]`: the engine it asks, and its limits.
    #[serde(skip_serializing_if = "WebSearchConfig::is_default")]
    pub web_search: WebSearchConfig,
    /// `[tools.<name>]` for every other tool: `write`, `todo_list`,
    /// `agent`, `swarm`, `send_message`, `ask_user_question`,
    /// `exit_plan`, plugin tools and MCP tools.
    ///
    /// Held as raw TOML so a stray non-table key (`[tools] foo = 1`),
    /// ignored before this table existed, still parses instead of
    /// refusing to start. [`Self::validate`] then rejects a table whose
    /// keys no tool takes.
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub other: BTreeMap<String, toml::Value>,
    /// The images endpoint the `generate_image` tool calls
    /// (`[tools.generate_image]`). Without one the tool is not
    /// registered.
    #[serde(default, skip_serializing_if = "MediaToolConfig::is_default")]
    pub generate_image: MediaToolConfig,
    /// The images endpoint the `edit_image` tool calls
    /// (`[tools.edit_image]`). Without one the tool is not
    /// registered.
    #[serde(default, skip_serializing_if = "MediaToolConfig::is_default")]
    pub edit_image: MediaToolConfig,
    /// The speech endpoint the `generate_speech` tool calls
    /// (`[tools.generate_speech]`). Without one the tool is not
    /// registered.
    #[serde(default, skip_serializing_if = "SpeechToolConfig::is_default")]
    pub generate_speech: SpeechToolConfig,
    /// The music endpoint the `generate_music` tool calls
    /// (`[tools.generate_music]`). Without one the tool is not
    /// registered.
    #[serde(default, skip_serializing_if = "MediaToolConfig::is_default")]
    pub generate_music: MediaToolConfig,
}

/// The two keys every `[tools.<name>]` table takes.
///
/// `deny_unknown_fields` so a key no tool reads (`[tools.write]
/// max_bytes = 1000`) fails loudly instead of parsing and doing
/// nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolToggle {
    /// `false` removes the tool: the model never sees it and a call to
    /// it fails as an unknown tool.
    #[serde(skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Replaces the description the model reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Default for ToolToggle {
    fn default() -> Self {
        Self {
            enabled: true,
            description: None,
        }
    }
}

/// Declares a per-tool settings struct: `enabled` and `description`
/// plus the tool's own limits, each with its default.
macro_rules! tool_config {
    (
        $(#[$meta:meta])*
        $name:ident {
            $(
                $(#[$fmeta:meta])*
                $field:ident : $ty:ty = $default:expr
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct $name {
            /// `false` removes the tool: the model never sees it.
            #[serde(default = "default_true", skip_serializing_if = "is_true")]
            pub enabled: bool,
            /// Replaces the description the model reads.
            #[serde(skip_serializing_if = "Option::is_none")]
            pub description: Option<String>,
            $(
                $(#[$fmeta])*
                pub $field: $ty,
            )*
        }

        impl Default for $name {
            fn default() -> Self {
                Self {
                    enabled: true,
                    description: None,
                    $($field: $default,)*
                }
            }
        }

        impl $name {
            /// Whether the defaults are in effect.
            #[must_use]
            pub fn is_default(&self) -> bool {
                *self == Self::default()
            }

            /// The `enabled` and `description` keys.
            #[must_use]
            pub fn toggle(&self) -> ToolToggle {
                ToolToggle {
                    enabled: self.enabled,
                    description: self.description.clone(),
                }
            }
        }
    };
}

tool_config! {
    /// `[tools.read]`.
    ReadToolConfig {
        /// Bytes one read returns before it is cut, with a footer
        /// saying so. Default 2000000.
        max_bytes: u64 = 2_000_000,
    }
}

tool_config! {
    /// `[tools.edit]`.
    EditToolConfig {
        /// Files larger than this are refused; loading one for a single
        /// edit would otherwise pull the whole file into memory.
        /// Default 2000000.
        max_bytes: u64 = 2_000_000,
    }
}

tool_config! {
    /// `[tools.grep]`.
    GrepToolConfig {
        /// Matches returned when the call sets no `max_matches`.
        /// Default 1000.
        max_matches: u64 = 1_000,
        /// Characters kept of one matching line; a longer line is cut
        /// around the match. Minified bundles put a whole file on one
        /// line, so a single hit could otherwise carry megabytes.
        /// Default 300.
        max_line_chars: usize = 300,
    }
}

tool_config! {
    /// `[tools.find]` and `[tools.ls]`.
    ListingToolConfig {
        /// Entries listed before the output is cut; keeps a huge tree
        /// from flooding the model's context. Default 5000.
        max_entries: usize = 5_000,
    }
}

tool_config! {
    /// `[tools.shell]`.
    ShellToolConfig {
        /// Timeout of a command whose call sets no `timeout_ms`, in
        /// milliseconds. Default 120000.
        timeout_ms: u64 = 120_000,
        /// Bytes kept of each of stdout and stderr. Default 100000.
        max_output_bytes: usize = 100_000,
    }
}

tool_config! {
    /// `[tools.web_fetch]`.
    WebFetchToolConfig {
        /// Body bytes read when the call sets no `max_bytes`.
        /// Default 2000000.
        max_bytes: u64 = 2_000_000,
        /// The most a call may ask for with `max_bytes`. The body lands
        /// in the conversation, so this bounds what a model can pull in.
        /// Default 5000000.
        max_bytes_limit: u64 = 5_000_000,
        /// Whole-request timeout in milliseconds: resolving,
        /// connecting, redirects and the body. Default 30000.
        timeout_ms: u64 = 30_000,
        /// Redirects followed before the fetch fails. Default 5.
        max_redirects: u32 = 5,
    }
}

tool_config! {
    /// `[tools.web_search]`: the engine `web_search` asks, and its limits.
    WebSearchConfig {
        /// `duckduckgo` (the default, no key), `searxng` or `brave`.
        engine: SearchEngine = SearchEngine::default(),
        /// The SearXNG instance's base URL, for `engine = "searxng"`. The
        /// instance must allow the `json` format.
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String> = None,
        /// The environment variable holding the Brave Search API key, for
        /// `engine = "brave"`. Defaults to `BRAVE_API_KEY`.
        #[serde(skip_serializing_if = "Option::is_none")]
        api_key_env: Option<String> = None,
        /// Whole-request timeout in milliseconds. Default 20000.
        timeout_ms: u64 = 20_000,
        /// Results returned when the call sets no `count`. Default 8.
        count: usize = 8,
        /// The most results a call may ask for. Default 20.
        max_count: usize = 20,
    }
}

/// A search engine `web_search` can ask.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchEngine {
    /// DuckDuckGo's HTML results page.
    #[default]
    Duckduckgo,
    /// A SearXNG instance's JSON API.
    Searxng,
    /// The Brave Search API, with a key.
    Brave,
}

impl ToolsConfig {
    /// Whether nothing is configured.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.rename.is_empty()
            && self.read.is_default()
            && self.edit.is_default()
            && self.grep.is_default()
            && self.find.is_default()
            && self.ls.is_default()
            && self.shell.is_default()
            && self.web_fetch.is_default()
            && self.web_search.is_default()
            && self.other.is_empty()
            && self.generate_image.is_default()
            && self.edit_image.is_default()
            && self.generate_speech.is_default()
            && self.generate_music.is_default()
    }

    /// Every configured `enabled` / `description` pair, keyed by tool
    /// name, the typed tables included.
    ///
    /// An `other` entry that is not a toggle table (a stray `[tools] foo
    /// = 1`, or a table with a key no tool takes) is left out here;
    /// [`Self::validate`] is what reports it.
    #[must_use]
    pub fn toggles(&self) -> BTreeMap<String, ToolToggle> {
        let mut out: BTreeMap<String, ToolToggle> = self
            .other
            .iter()
            .filter_map(|(name, value)| {
                ToolToggle::deserialize(value.clone())
                    .ok()
                    .map(|toggle| (name.clone(), toggle))
            })
            .collect();
        for (name, toggle) in [
            ("read", self.read.toggle()),
            ("edit", self.edit.toggle()),
            ("grep", self.grep.toggle()),
            ("find", self.find.toggle()),
            ("ls", self.ls.toggle()),
            ("shell", self.shell.toggle()),
            ("web_fetch", self.web_fetch.toggle()),
            ("web_search", self.web_search.toggle()),
        ] {
            if toggle != ToolToggle::default() {
                out.insert(name.to_owned(), toggle);
            }
        }
        out
    }

    /// Names of the tools set to `enabled = false`.
    #[must_use]
    pub fn disabled(&self) -> BTreeSet<String> {
        self.toggles()
            .into_iter()
            .filter(|(_, t)| !t.enabled)
            .map(|(name, _)| name)
            .collect()
    }

    /// Description overrides, keyed by tool name.
    #[must_use]
    pub fn descriptions(&self) -> BTreeMap<String, String> {
        self.toggles()
            .into_iter()
            .filter_map(|(name, t)| Some((name, t.description?)))
            .collect()
    }

    /// `[tools.<name>]` tables whose name matches no tool in `known`.
    ///
    /// A typo or the wrong case (`[tools.web-search]`, `[tools.Shell]`)
    /// would otherwise sit in the config doing nothing, so a host warns
    /// once the registry is built.
    #[must_use]
    pub fn unmatched(&self, known: &BTreeSet<String>) -> BTreeSet<String> {
        self.toggles()
            .into_keys()
            .filter(|name| !known.contains(name))
            .collect()
    }

    /// Reject values no tool could work with: an empty description, a
    /// zero limit, a default above its own ceiling, or a `[tools.<name>]`
    /// table carrying a key no tool reads.
    pub fn validate(&self) -> Result<()> {
        for (name, value) in &self.other {
            // A non-table value (`[tools] foo = 1`) was ignored before
            // this table existed, so keep ignoring it rather than
            // refusing to start.
            if !value.is_table() {
                continue;
            }
            if let Err(err) = ToolToggle::deserialize(value.clone()) {
                return Err(config_error(format!("[tools.{name}] {err}")));
            }
        }
        for (name, toggle) in self.toggles() {
            if toggle
                .description
                .as_ref()
                .is_some_and(|d| d.trim().is_empty())
            {
                return Err(crate::config::config_error(format!(
                    "[tools.{name}] description must not be empty; remove the key to keep the built-in one"
                )));
            }
        }
        let positive = [
            ("read", "max_bytes", self.read.max_bytes),
            ("edit", "max_bytes", self.edit.max_bytes),
            ("grep", "max_matches", self.grep.max_matches),
            ("grep", "max_line_chars", self.grep.max_line_chars as u64),
            ("find", "max_entries", self.find.max_entries as u64),
            ("ls", "max_entries", self.ls.max_entries as u64),
            ("shell", "timeout_ms", self.shell.timeout_ms),
            (
                "shell",
                "max_output_bytes",
                self.shell.max_output_bytes as u64,
            ),
            ("web_fetch", "max_bytes", self.web_fetch.max_bytes),
            (
                "web_fetch",
                "max_bytes_limit",
                self.web_fetch.max_bytes_limit,
            ),
            ("web_fetch", "timeout_ms", self.web_fetch.timeout_ms),
            ("web_search", "timeout_ms", self.web_search.timeout_ms),
            ("web_search", "count", self.web_search.count as u64),
            ("web_search", "max_count", self.web_search.max_count as u64),
        ];
        for (tool, key, value) in positive {
            if value == 0 {
                return Err(crate::config::config_error(format!(
                    "[tools.{tool}] {key} must be at least 1"
                )));
            }
        }
        if self.web_fetch.max_bytes > self.web_fetch.max_bytes_limit {
            return Err(crate::config::config_error(format!(
                "[tools.web_fetch] max_bytes ({}) is above max_bytes_limit ({})",
                self.web_fetch.max_bytes, self.web_fetch.max_bytes_limit
            )));
        }
        if self.web_search.count > self.web_search.max_count {
            return Err(config_error(format!(
                "[tools.web_search] count ({}) is above max_count ({})",
                self.web_search.count, self.web_search.max_count
            )));
        }
        // A cap this large is already far past any context window, and
        // keeping it under a ceiling leaves room for the `+ 1` a tool
        // adds to its budget without overflowing a `usize`.
        for (tool, key, value) in [
            ("read", "max_bytes", self.read.max_bytes),
            ("edit", "max_bytes", self.edit.max_bytes),
            ("web_fetch", "max_bytes", self.web_fetch.max_bytes),
            (
                "web_fetch",
                "max_bytes_limit",
                self.web_fetch.max_bytes_limit,
            ),
        ] {
            if value > MAX_TOOL_BYTES {
                return Err(config_error(format!(
                    "[tools.{tool}] {key} ({value}) is above the {MAX_TOOL_BYTES} byte ceiling"
                )));
            }
        }
        self.generate_image.validate("generate_image")?;
        self.edit_image.validate("edit_image")?;
        self.generate_speech.endpoint.validate("generate_speech")?;
        self.generate_music.validate("generate_music")?;
        Ok(())
    }
}

#[cfg(test)]
#[expect(
    clippy::result_large_err,
    reason = "figment::Jail closures must return figment::Error"
)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn try_load(toml: &str) -> std::result::Result<Config, figment::Error> {
        use figment::Figment;
        use figment::providers::{Format, Serialized, Toml};
        Figment::new()
            .merge(Serialized::defaults(Config::default()))
            .merge(Toml::string(toml))
            .extract()
    }

    fn load(toml: &str) -> Config {
        try_load(toml).expect("config parses")
    }

    #[test]
    fn defaults_are_default_and_valid() {
        let tools = ToolsConfig::default();
        assert!(tools.is_default());
        assert!(tools.disabled().is_empty());
        assert!(tools.descriptions().is_empty());
        tools.validate().unwrap();
        // Serializing the defaults writes nothing, so a saved config
        // never grows a `[tools]` table it did not have.
        assert_eq!(toml::to_string(&tools).unwrap(), "");
        assert!(Config::default().tools.is_default());
    }

    #[test]
    fn typed_and_other_tables_parse() {
        let cfg = load(
            r#"
            [tools.rename]
            shell = "run_command"

            [tools.web_search]
            enabled = false
            engine = "searxng"
            url = "https://searx.example"
            count = 5

            [tools.shell]
            description = "Run it."
            timeout_ms = 300000

            [tools.read]
            max_bytes = 500

            [tools.ask_user_question]
            enabled = false

            [tools.github__create_issue]
            description = "Open an issue."
            "#,
        );
        let t = &cfg.tools;
        assert_eq!(t.rename["shell"], "run_command");
        assert_eq!(t.web_search.engine, SearchEngine::Searxng);
        assert_eq!(t.web_search.count, 5);
        assert_eq!(t.web_search.max_count, 20);
        assert_eq!(t.shell.timeout_ms, 300_000);
        assert_eq!(t.shell.max_output_bytes, 100_000);
        assert_eq!(t.read.max_bytes, 500);
        assert!(!t.other.contains_key("rename"));
        assert!(!t.other.contains_key("shell"));
        assert_eq!(
            t.disabled().into_iter().collect::<Vec<_>>(),
            vec!["ask_user_question", "web_search"]
        );
        let descriptions = t.descriptions();
        assert_eq!(descriptions["shell"], "Run it.");
        assert_eq!(descriptions["github__create_issue"], "Open an issue.");
        t.validate().unwrap();
    }

    #[test]
    fn round_trips_through_toml() {
        let cfg = load(
            "[tools.write]\nenabled = false\n[tools.grep]\nmax_matches = 50\ndescription = \"g\"\n",
        );
        let text = toml::to_string(&cfg.tools).unwrap();
        let back: ToolsConfig = toml::from_str(&text).unwrap();
        assert_eq!(back, cfg.tools);
        assert!(text.contains("enabled = false"), "{text}");
    }

    #[test]
    fn rejects_unusable_values() {
        for (toml, needle) in [
            ("[tools.read]\nmax_bytes = 0\n", "[tools.read] max_bytes"),
            (
                "[tools.shell]\ndescription = \"  \"\n",
                "[tools.shell] description",
            ),
            (
                "[tools.todo_list]\ndescription = \"\"\n",
                "[tools.todo_list] description",
            ),
            (
                "[tools.web_fetch]\nmax_bytes = 10\nmax_bytes_limit = 5\n",
                "max_bytes_limit",
            ),
            ("[tools.web_search]\ncount = 30\n", "max_count"),
            ("[tools.read]\nmax_bytes = 1099511627776\n", "byte ceiling"),
        ] {
            let err = load(toml).tools.validate().unwrap_err().to_string();
            assert!(err.contains(needle), "{toml}: {err}");
        }
    }

    /// A key no tool reads used to parse and do nothing, so a disable
    /// meant for safety could silently not apply. A typed table refuses
    /// it while parsing, an `other` table when the config validates.
    #[test]
    fn a_key_no_tool_reads_is_refused() {
        for (toml, needle) in [
            ("[tools.shell]\nmax_byte = 1000\n", "max_byte"),
            ("[tools.read]\nmax_byte = 1000\n", "max_byte"),
        ] {
            let err = try_load(toml).unwrap_err().to_string();
            assert!(err.contains(needle), "{toml}: {err}");
        }
        for (toml, needle) in [
            ("[tools.write]\nmax_bytes = 1000\n", "max_bytes"),
            (
                "[tools.github]\ndelete_repository = { enabled = false }\n",
                "delete_repository",
            ),
        ] {
            let err = load(toml).tools.validate().unwrap_err().to_string();
            assert!(err.contains(needle), "{toml}: {err}");
        }
    }

    /// A stray non-table key under `[tools]` was ignored before the
    /// per-tool tables existed, so it must not refuse to start.
    #[test]
    fn a_stray_non_table_key_is_ignored() {
        let cfg = load("[tools]\nfoo = 1\n[tools.write]\nenabled = false\n");
        cfg.tools.validate().unwrap();
        assert!(cfg.tools.disabled().contains("write"));
        assert!(!cfg.tools.toggles().contains_key("foo"));
    }

    /// A name that matches no tool is reported rather than silently
    /// doing nothing, which is what a `[tools.web-search]` typo or the
    /// wrong case would otherwise do.
    #[test]
    fn a_name_matching_no_tool_is_reported() {
        let cfg = load(
            "[tools.web-search]\nenabled = false\n[tools.Shell]\nenabled = false\n\
             [tools.shell]\nenabled = false\n",
        );
        cfg.tools.validate().unwrap();
        let known: BTreeSet<String> = ["shell", "read", "write"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            cfg.tools.unmatched(&known).into_iter().collect::<Vec<_>>(),
            vec!["Shell", "web-search"]
        );
    }

    #[test]
    fn env_overrides_reach_tool_limits() {
        use figment::Figment;
        use figment::providers::{Env, Serialized};
        let _globals = crate::test_support::process_globals();
        figment::Jail::expect_with(|jail| {
            jail.set_env("KAGE_TOOLS__SHELL__TIMEOUT_MS", "5000");
            jail.set_env("KAGE_TOOLS__WEB_FETCH__ENABLED", "false");
            let cfg: Config = Figment::new()
                .merge(Serialized::defaults(Config::default()))
                .merge(Env::prefixed("KAGE_").split("__"))
                .extract()?;
            assert_eq!(cfg.tools.shell.timeout_ms, 5000);
            assert!(cfg.tools.disabled().contains("web_fetch"));
            Ok(())
        });
    }
}
