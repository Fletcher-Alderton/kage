//! Option registry and value store.
//!
//! [`OPTIONS`] lists every option with its TOML path, type, default and
//! doc. An [`OptionStore`] holds the current values, seeded from a
//! loaded [`Config`] and then written by Lua or the TUI. Every set
//! records its source, queues an [`OptionChange`] for the host to
//! apply, and bumps a generation counter; a set that leaves the value
//! unchanged does neither. The plugin runtime fires the `option_set`
//! event for each set.

use crate::config::{Config, EditorMode};
use crate::keymap;

/// Type, bounds and default of an option.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OptionKind {
    /// A boolean.
    Bool {
        /// Default value.
        default: bool,
    },
    /// An integer in `min..=max`.
    Int {
        /// Smallest valid value.
        min: i64,
        /// Largest valid value.
        max: i64,
        /// Default value.
        default: i64,
    },
    /// A number from 0 to 1.
    Fraction {
        /// Default value.
        default: f64,
    },
    /// One string out of a fixed set.
    Choice {
        /// Valid values.
        values: &'static [&'static str],
        /// Default value.
        default: &'static str,
    },
    /// A non-empty string. The host applies any further check, such as
    /// a known theme name.
    Str {
        /// Default value.
        default: &'static str,
    },
    /// A single key in keymap notation.
    Key {
        /// Default value.
        default: &'static str,
    },
}

/// One entry of [`OPTIONS`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OptionDef {
    /// Name, as in `kage.opt.<name>`.
    pub name: &'static str,
    /// Dotted path of the matching `config.toml` key.
    pub toml: &'static str,
    /// Type, bounds and default.
    pub kind: OptionKind,
    /// One-line description.
    pub doc: &'static str,
    /// Whether a change applies while running. Other options apply at
    /// the next session start.
    pub live: bool,
}

/// Values [`OptionKind::Choice`] accepts for `thinking_level`: the
/// thinking ladder, or empty for the automatic level.
const THINKING_LEVELS: &[&str] = &[
    "", "off", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// Values [`OptionKind::Choice`] accepts for `agent_thinking`: the
/// thinking ladder, or `inherit` for the parent's level.
const AGENT_THINKING_LEVELS: &[&str] = &[
    "inherit", "off", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// Every option.
pub const OPTIONS: &[OptionDef] = &[
    OptionDef {
        name: "theme",
        toml: "ui.theme",
        kind: OptionKind::Str { default: "default" },
        doc: "Color theme, bundled or from the themes directory; `default` follows the terminal background.",
        live: true,
    },
    OptionDef {
        name: "theme_dark",
        toml: "ui.theme_dark",
        kind: OptionKind::Str {
            default: "kage-shadow",
        },
        doc: "The theme `default` picks on a dark terminal background.",
        live: true,
    },
    OptionDef {
        name: "theme_light",
        toml: "ui.theme_light",
        kind: OptionKind::Str {
            default: "kage-dawn",
        },
        doc: "The theme `default` picks on a light terminal background.",
        live: true,
    },
    OptionDef {
        name: "mouse",
        toml: "ui.mouse",
        kind: OptionKind::Bool { default: true },
        doc: "Capture mouse events.",
        live: true,
    },
    OptionDef {
        name: "editor",
        toml: "ui.editor",
        kind: OptionKind::Choice {
            values: &["vim", "modeless"],
            default: "modeless",
        },
        doc: "Prompt editing style.",
        live: true,
    },
    OptionDef {
        name: "input_min_lines",
        toml: "ui.input_min_lines",
        kind: OptionKind::Int {
            min: 1,
            max: 64,
            default: 1,
        },
        doc: "Minimum content rows of the input box.",
        live: true,
    },
    OptionDef {
        name: "input_max_lines",
        toml: "ui.input_max_lines",
        kind: OptionKind::Int {
            min: 1,
            max: 64,
            default: 8,
        },
        doc: "Content rows the input box grows to before it scrolls.",
        live: true,
    },
    OptionDef {
        name: "transcript_on_exit",
        toml: "ui.transcript_on_exit",
        kind: OptionKind::Choice {
            values: &["full", "last", "none"],
            default: "full",
        },
        doc: "What prints after exit: the whole transcript, from the last prompt on, or only the session path and a resume hint.",
        live: true,
    },
    OptionDef {
        name: "thinking_level",
        toml: "ui.thinking_level",
        kind: OptionKind::Choice {
            values: THINKING_LEVELS,
            default: "",
        },
        doc: "Thinking level for new sessions, or empty for auto (high, or the nearest level the model accepts).",
        live: false,
    },
    OptionDef {
        name: "compaction_threshold",
        toml: "loop.compaction_threshold",
        kind: OptionKind::Fraction { default: 0.8 },
        doc: "Fraction of the context window that triggers compaction. 0 turns compaction off.",
        live: false,
    },
    OptionDef {
        name: "leader",
        toml: "keybindings.leader",
        kind: OptionKind::Key { default: "\\" },
        doc: "The key `<leader>` expands to when a mapping is set.",
        live: true,
    },
    OptionDef {
        name: "timeoutlen",
        toml: "keybindings.timeoutlen",
        kind: OptionKind::Int {
            min: 0,
            max: 5000,
            default: 1000,
        },
        doc: "Milliseconds a mapping that is also a prefix waits for more keys.",
        live: true,
    },
    OptionDef {
        name: "agent_max_depth",
        toml: "agents.max_depth",
        kind: OptionKind::Int {
            min: 0,
            max: 3,
            default: 1,
        },
        doc: "How deep agents may nest. 0 turns the agent tool off, and 1 lets only the main session start agents.",
        live: false,
    },
    OptionDef {
        name: "agent_max_running",
        toml: "agents.max_running",
        kind: OptionKind::Int {
            min: 1,
            max: 16,
            default: 4,
        },
        doc: "How many agents run at once. Further agents wait until one finishes.",
        live: false,
    },
    OptionDef {
        name: "swarm_max_items",
        toml: "agents.swarm_max_items",
        kind: OptionKind::Int {
            min: 2,
            max: 128,
            default: 32,
        },
        doc: "Most items one swarm call may run, one child agent per item.",
        live: false,
    },
    OptionDef {
        name: "swarm_timeout_ms",
        toml: "agents.swarm_timeout_ms",
        kind: OptionKind::Int {
            min: 1000,
            max: 86_400_000,
            default: 7_200_000,
        },
        doc: "Milliseconds one swarm child may run, measured from its run start (not while queued). On deadline the child is cancelled and renders as cancelled in the aggregate.",
        live: false,
    },
    OptionDef {
        name: "agent_model",
        toml: "agents.model",
        kind: OptionKind::Str { default: "inherit" },
        doc: "Model agents run, as provider/model, when neither the call nor the agent's definition names one. `inherit` uses the parent's current model.",
        live: false,
    },
    OptionDef {
        name: "agent_thinking",
        toml: "agents.thinking",
        kind: OptionKind::Choice {
            values: AGENT_THINKING_LEVELS,
            default: "inherit",
        },
        doc: "Thinking level agents run with when neither the call nor the agent's definition names one. `inherit` uses the parent's current level.",
        live: false,
    },
    OptionDef {
        name: "agent_max_turns",
        toml: "agents.max_turns",
        kind: OptionKind::Int {
            min: 0,
            max: 10_000,
            default: 0,
        },
        doc: "Turns one agent run may take. The agent gets one warning turn to reply, then stops. 0 means no limit.",
        live: false,
    },
    OptionDef {
        name: "agent_timeout",
        toml: "agents.timeout",
        kind: OptionKind::Int {
            min: 0,
            max: 86_400,
            default: 0,
        },
        doc: "Seconds one agent run may take before it is stopped. 0 means no limit.",
        live: false,
    },
    OptionDef {
        name: "agent_budget",
        toml: "agents.budget",
        kind: OptionKind::Int {
            min: 0,
            max: 1_000_000_000,
            default: 0,
        },
        doc: "Tokens (input plus output) all agents of a session may use between two of your prompts. Past it, they stop. 0 means no limit.",
        live: false,
    },
];

/// Look up an option by name.
#[must_use]
pub fn find(name: &str) -> Option<&'static OptionDef> {
    OPTIONS.iter().find(|def| def.name == name)
}

/// Look up an option by name, or fail with [`OptionError::Unknown`]
/// listing every valid name.
pub fn lookup(name: &str) -> Result<&'static OptionDef, OptionError> {
    find(name).ok_or_else(|| unknown(name))
}

fn unknown(name: &str) -> OptionError {
    OptionError::Unknown {
        name: name.to_owned(),
        valid: OPTIONS
            .iter()
            .map(|def| def.name)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

impl OptionDef {
    /// The default value.
    #[must_use]
    pub fn default_value(&self) -> OptionValue {
        match self.kind {
            OptionKind::Bool { default } => OptionValue::Bool(default),
            OptionKind::Int { default, .. } => OptionValue::Int(default),
            OptionKind::Fraction { default } => OptionValue::Float(default),
            OptionKind::Choice { default, .. }
            | OptionKind::Str { default }
            | OptionKind::Key { default } => OptionValue::Str(default.to_owned()),
        }
    }

    /// Check `value` against the option's type and bounds. An integer
    /// is accepted where a fraction is expected.
    pub fn validate(&self, value: OptionValue) -> Result<OptionValue, OptionError> {
        let valid = match (&self.kind, value) {
            (OptionKind::Bool { .. }, v @ OptionValue::Bool(_)) => Some(v),
            (OptionKind::Int { min, max, .. }, OptionValue::Int(n))
                if (*min..=*max).contains(&n) =>
            {
                Some(OptionValue::Int(n))
            }
            (OptionKind::Fraction { .. }, OptionValue::Float(x)) => fraction(x),
            (OptionKind::Fraction { .. }, OptionValue::Int(n)) => {
                i32::try_from(n).ok().map(f64::from).and_then(fraction)
            }
            (OptionKind::Choice { values, .. }, OptionValue::Str(s))
                if values.contains(&s.as_str()) =>
            {
                Some(OptionValue::Str(s))
            }
            (OptionKind::Str { .. }, OptionValue::Str(s)) if !s.is_empty() => {
                Some(OptionValue::Str(s))
            }
            (OptionKind::Key { .. }, OptionValue::Str(s)) if keymap::parse_key(&s).is_ok() => {
                Some(OptionValue::Str(s))
            }
            _ => None,
        };
        valid.ok_or_else(|| OptionError::Invalid {
            name: self.name,
            expected: self.expected(),
        })
    }

    /// What the option accepts, as used in error messages.
    #[must_use]
    pub fn expected(&self) -> String {
        match self.kind {
            OptionKind::Bool { .. } => "a boolean".to_owned(),
            OptionKind::Int { min, max, .. } => format!("an integer from {min} to {max}"),
            OptionKind::Fraction { .. } => "a number from 0 to 1".to_owned(),
            OptionKind::Choice { values, .. } => {
                let quoted: Vec<String> = values.iter().map(|v| format!("\"{v}\"")).collect();
                format!("one of {}", quoted.join(", "))
            }
            OptionKind::Str { .. } => "a non-empty string".to_owned(),
            OptionKind::Key { .. } => "a single key, such as `,` or `<Space>`".to_owned(),
        }
    }
}

fn fraction(x: f64) -> Option<OptionValue> {
    (0.0..=1.0).contains(&x).then_some(OptionValue::Float(x))
}

/// An option value.
#[derive(Clone, Debug, PartialEq)]
pub enum OptionValue {
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A number.
    Float(f64),
    /// A string.
    Str(String),
}

impl OptionValue {
    /// The boolean, if this is one.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The integer, if this is one.
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// The number, if this is one.
    #[must_use]
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(x) => Some(*x),
            _ => None,
        }
    }

    /// The string, if this is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Convert an option value into its JSON form.
#[must_use]
pub fn option_to_json(value: &OptionValue) -> serde_json::Value {
    match value {
        OptionValue::Bool(b) => serde_json::Value::from(*b),
        OptionValue::Int(n) => serde_json::Value::from(*n),
        OptionValue::Float(x) => serde_json::Value::from(*x),
        OptionValue::Str(s) => serde_json::Value::from(s.as_str()),
    }
}

/// Parse an option value out of its JSON form, or `None` when the
/// JSON is not a boolean, number or string.
#[must_use]
pub fn option_from_json(value: &serde_json::Value) -> Option<OptionValue> {
    Some(match value {
        serde_json::Value::Bool(b) => OptionValue::Bool(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(n) => OptionValue::Int(n),
            None => OptionValue::Float(n.as_f64()?),
        },
        serde_json::Value::String(s) => OptionValue::Str(s.clone()),
        _ => return None,
    })
}

/// Where the current value of an option came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionSource {
    /// The registry default.
    Default,
    /// `config.toml` or a `KAGE_` environment variable.
    Toml,
    /// `init.lua` or a plugin.
    Lua,
    /// The TUI: a command or the settings dialog.
    Runtime,
}

impl OptionSource {
    /// Lowercase name, as reported to Lua.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Toml => "toml",
            Self::Lua => "lua",
            Self::Runtime => "runtime",
        }
    }
}

/// One set, queued for the host to apply.
#[derive(Clone, Debug, PartialEq)]
pub struct OptionChange {
    /// Option name.
    pub name: &'static str,
    /// Value before the set.
    pub old: OptionValue,
    /// Value after the set.
    pub new: OptionValue,
    /// Who set it.
    pub source: OptionSource,
}

/// Current option values, their sources and the pending change queue.
#[derive(Clone, Debug, PartialEq)]
pub struct OptionStore {
    values: Vec<(OptionValue, OptionSource)>,
    changes: Vec<OptionChange>,
    generation: u64,
}

impl Default for OptionStore {
    fn default() -> Self {
        Self {
            values: OPTIONS
                .iter()
                .map(|def| (def.default_value(), OptionSource::Default))
                .collect(),
            changes: Vec::new(),
            generation: 0,
        }
    }
}

impl OptionStore {
    /// Seed a store from a loaded config. Values that differ from the
    /// default get source [`OptionSource::Toml`]. An invalid value
    /// keeps the default and is returned as an error. Nothing is
    /// queued.
    #[must_use]
    pub fn from_config(config: &Config) -> (Self, Vec<OptionError>) {
        let mut store = Self::default();
        let mut errors = Vec::new();
        for (def, slot) in OPTIONS.iter().zip(&mut store.values) {
            let Some(value) = config_value(def.name, config) else {
                continue;
            };
            if value == slot.0 {
                continue;
            }
            match def.validate(value) {
                Ok(value) => *slot = (value, OptionSource::Toml),
                Err(err) => errors.push(err),
            }
        }
        (store, errors)
    }

    /// Current value of `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&OptionValue> {
        index(name).map(|i| &self.values[i].0)
    }

    /// Source of the current value of `name`.
    #[must_use]
    pub fn source(&self, name: &str) -> Option<OptionSource> {
        index(name).map(|i| self.values[i].1)
    }

    /// Validate and set `name`, queue the change and bump the
    /// generation. Setting the value `name` already holds queues
    /// nothing and bumps nothing, so plugins are not notified, but
    /// still returns the change. Returns the queued change.
    pub fn set(
        &mut self,
        name: &str,
        value: OptionValue,
        source: OptionSource,
    ) -> Result<OptionChange, OptionError> {
        let i = index(name).ok_or_else(|| unknown(name))?;
        let def = &OPTIONS[i];
        let new = def.validate(value)?;
        if self.values[i].0 == new {
            return Ok(OptionChange {
                name: def.name,
                old: new.clone(),
                new,
                source,
            });
        }
        let (old, _) = std::mem::replace(&mut self.values[i], (new.clone(), source));
        let change = OptionChange {
            name: def.name,
            old,
            new,
            source,
        };
        self.changes.push(change.clone());
        self.generation += 1;
        Ok(change)
    }

    /// Drain the queued changes, oldest first.
    pub fn take_changes(&mut self) -> Vec<OptionChange> {
        std::mem::take(&mut self.changes)
    }

    /// Counter bumped on every set.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Write `edits` into the config file at `path`, each at its option's
/// TOML path, leaving the rest of the file as written.
///
/// # Errors
///
/// See [`Config::save_keys`].
pub fn save_options(
    path: &std::path::Path,
    edits: &[(&OptionDef, OptionValue)],
) -> crate::Result<()> {
    let edits: Vec<(Vec<&str>, OptionValue)> = edits
        .iter()
        .map(|(def, value)| (def.toml.split('.').collect(), value.clone()))
        .collect();
    Config::save_keys(path, &edits)
}

fn index(name: &str) -> Option<usize> {
    OPTIONS.iter().position(|def| def.name == name)
}

fn config_value(name: &str, config: &Config) -> Option<OptionValue> {
    let ui = &config.ui;
    Some(match name {
        "theme" => OptionValue::Str(ui.theme.clone()),
        "theme_dark" => return ui.theme_dark.clone().map(OptionValue::Str),
        "theme_light" => return ui.theme_light.clone().map(OptionValue::Str),
        "mouse" => OptionValue::Bool(ui.mouse),
        "editor" => OptionValue::Str(
            match ui.editor {
                EditorMode::Vim => "vim",
                EditorMode::Modeless => "modeless",
            }
            .to_owned(),
        ),
        "input_min_lines" => OptionValue::Int(i64::from(ui.input_min_lines)),
        "input_max_lines" => OptionValue::Int(i64::from(ui.input_max_lines)),
        "transcript_on_exit" => OptionValue::Str(ui.transcript_on_exit.clone()),
        "thinking_level" => OptionValue::Str(ui.thinking_level.clone().unwrap_or_default()),
        "compaction_threshold" => {
            OptionValue::Float(widen(config.loop_settings.compaction_threshold))
        }
        "leader" => OptionValue::Str(config.keybindings.leader.clone()),
        "timeoutlen" => OptionValue::Int(i64::from(config.keybindings.timeoutlen)),
        "agent_max_depth" => OptionValue::Int(i64::from(config.agents.max_depth)),
        "agent_max_running" => OptionValue::Int(i64::from(config.agents.max_running)),
        "swarm_max_items" => OptionValue::Int(i64::from(config.agents.swarm_max_items)),
        "swarm_timeout_ms" => {
            OptionValue::Int(i64::try_from(config.agents.swarm_timeout_ms).unwrap_or(i64::MAX))
        }
        "agent_model" => OptionValue::Str(config.agents.model.clone()),
        "agent_thinking" => OptionValue::Str(config.agents.thinking.clone()),
        "agent_max_turns" => OptionValue::Int(i64::from(config.agents.max_turns)),
        "agent_timeout" => OptionValue::Int(i64::from(config.agents.timeout)),
        "agent_budget" => OptionValue::Int(i64::try_from(config.agents.budget).unwrap_or(i64::MAX)),
        _ => return None,
    })
}

/// Widen through the shortest decimal form, so `0.8f32` reads as `0.8`
/// instead of `0.800000011920929`.
fn widen(x: f32) -> f64 {
    x.to_string().parse().unwrap_or(f64::from(x))
}

/// Errors from [`OptionStore::set`] and [`OptionDef::validate`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OptionError {
    /// No option has this name.
    #[error("unknown option `{name}` (valid: {valid})")]
    Unknown {
        /// The name as given.
        name: String,
        /// Every option name, comma-separated.
        valid: String,
    },
    /// The value does not fit the option.
    #[error("option `{name}` expects {expected}")]
    Invalid {
        /// Option name.
        name: &'static str,
        /// What the option accepts.
        expected: String,
    },
}

#[cfg(test)]
#[expect(
    clippy::result_large_err,
    reason = "figment::Jail closures must return figment::Error"
)]
mod tests {
    use super::*;
    use crate::test_support::process_globals;

    fn load(toml: &str) -> Config {
        let _globals = process_globals();
        let mut out = None;
        figment::Jail::expect_with(|jail| {
            jail.create_file("config.toml", toml)?;
            out = Some(Config::load(jail.directory().join("config.toml").as_path()).unwrap());
            Ok(())
        });
        out.unwrap()
    }

    #[test]
    fn defaults_validate_and_seed_the_default_store() {
        let store = OptionStore::default();
        for def in OPTIONS {
            assert_eq!(def.validate(def.default_value()), Ok(def.default_value()));
            assert_eq!(store.get(def.name), Some(&def.default_value()));
            assert_eq!(store.source(def.name), Some(OptionSource::Default));
        }
    }

    #[test]
    fn default_config_seeds_defaults_only() {
        let (store, errors) = OptionStore::from_config(&Config::default());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(store, OptionStore::default());
    }

    #[test]
    fn toml_paths_exist_in_serialized_config() {
        let mut cfg = Config::default();
        cfg.ui.thinking_level = Some("high".to_owned());
        cfg.ui.theme_dark = Some("kimi-dark".to_owned());
        cfg.ui.theme_light = Some("kimi-light".to_owned());
        cfg.keybindings.leader = ",".to_owned();
        cfg.keybindings.timeoutlen = 300;
        let value = toml::Value::try_from(&cfg).unwrap();
        for def in OPTIONS {
            let found = def
                .toml
                .split('.')
                .try_fold(&value, |node, part| node.get(part));
            assert!(found.is_some(), "`{}` is not in Config", def.toml);
        }
    }

    #[test]
    fn toml_values_seed_the_store_with_source_toml() {
        for (name, literal, expected) in [
            (
                "theme",
                "\"tokyo-night\"",
                OptionValue::Str("tokyo-night".into()),
            ),
            (
                "theme_dark",
                "\"kimi-dark\"",
                OptionValue::Str("kimi-dark".into()),
            ),
            (
                "theme_light",
                "\"kimi-light\"",
                OptionValue::Str("kimi-light".into()),
            ),
            ("mouse", "false", OptionValue::Bool(false)),
            ("editor", "\"vim\"", OptionValue::Str("vim".into())),
            ("input_min_lines", "2", OptionValue::Int(2)),
            ("input_max_lines", "12", OptionValue::Int(12)),
            (
                "transcript_on_exit",
                "\"last\"",
                OptionValue::Str("last".into()),
            ),
            (
                "thinking_level",
                "\"high\"",
                OptionValue::Str("high".into()),
            ),
            ("compaction_threshold", "0.6", OptionValue::Float(0.6)),
            ("leader", "\"<Space>\"", OptionValue::Str("<Space>".into())),
            ("timeoutlen", "300", OptionValue::Int(300)),
            ("agent_max_depth", "0", OptionValue::Int(0)),
            ("agent_max_running", "8", OptionValue::Int(8)),
            ("swarm_max_items", "64", OptionValue::Int(64)),
            ("swarm_timeout_ms", "60000", OptionValue::Int(60_000)),
            (
                "agent_model",
                "\"zai/glm-5\"",
                OptionValue::Str("zai/glm-5".into()),
            ),
            ("agent_thinking", "\"low\"", OptionValue::Str("low".into())),
            ("agent_max_turns", "60", OptionValue::Int(60)),
            ("agent_timeout", "600", OptionValue::Int(600)),
            ("agent_budget", "2000000", OptionValue::Int(2_000_000)),
        ] {
            let def = find(name).unwrap();
            let (table, key) = def.toml.rsplit_once('.').unwrap();
            let cfg = load(&format!("[{table}]\n{key} = {literal}\n"));
            let (store, errors) = OptionStore::from_config(&cfg);
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(store.get(name), Some(&expected), "{name}");
            assert_eq!(store.source(name), Some(OptionSource::Toml), "{name}");
        }
    }

    #[test]
    fn unset_theme_variants_read_the_kage_palettes() {
        let (mut store, errors) = OptionStore::from_config(&load("[ui]\ntheme = \"kage-dawn\"\n"));
        assert!(errors.is_empty(), "{errors:?}");
        for (name, theme) in [("theme_dark", "kage-shadow"), ("theme_light", "kage-dawn")] {
            assert_eq!(store.get(name), Some(&OptionValue::Str(theme.into())));
            assert_eq!(store.source(name), Some(OptionSource::Default));
            assert!(
                store
                    .set(name, OptionValue::Str(String::new()), OptionSource::Lua)
                    .is_err()
            );
        }
    }

    #[test]
    fn env_override_wins_over_toml() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file("config.toml", "[ui]\ntheme = \"tokyo-night\"\n")?;
            jail.set_env("KAGE_UI__THEME", "catppuccin-mocha");
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let (store, _) = OptionStore::from_config(&cfg);
            assert_eq!(
                store.get("theme"),
                Some(&OptionValue::Str("catppuccin-mocha".into()))
            );
            assert_eq!(store.source("theme"), Some(OptionSource::Toml));
            Ok(())
        });
    }

    #[test]
    fn invalid_toml_value_keeps_default_and_reports() {
        let cfg = load("[ui]\ninput_max_lines = 100\nthinking_level = \"huge\"\n");
        let (store, errors) = OptionStore::from_config(&cfg);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert_eq!(store.get("input_max_lines"), Some(&OptionValue::Int(8)));
        assert_eq!(store.source("input_max_lines"), Some(OptionSource::Default));
    }

    #[test]
    fn set_validates_enums_and_ranges() {
        let mut store = OptionStore::default();
        let err = store
            .set(
                "editor",
                OptionValue::Str("emacs".into()),
                OptionSource::Lua,
            )
            .unwrap_err();
        assert!(err.to_string().contains("\"vim\", \"modeless\""), "{err}");
        for bad in [0, 65] {
            assert!(
                store
                    .set("input_min_lines", OptionValue::Int(bad), OptionSource::Lua)
                    .is_err()
            );
        }
        let err = store
            .set("input_min_lines", OptionValue::Int(65), OptionSource::Lua)
            .unwrap_err();
        assert!(err.to_string().contains("from 1 to 64"), "{err}");
        assert!(
            store
                .set("input_min_lines", OptionValue::Int(64), OptionSource::Lua)
                .is_ok()
        );
        assert!(
            store
                .set("mouse", OptionValue::Int(1), OptionSource::Lua)
                .is_err()
        );
        for bad in [-0.5, 1.5, f64::NAN] {
            assert!(
                store
                    .set(
                        "compaction_threshold",
                        OptionValue::Float(bad),
                        OptionSource::Lua
                    )
                    .is_err()
            );
        }
        assert_eq!(
            store
                .set(
                    "compaction_threshold",
                    OptionValue::Int(1),
                    OptionSource::Lua
                )
                .unwrap()
                .new,
            OptionValue::Float(1.0)
        );
        assert!(
            store
                .set("leader", OptionValue::Str("ab".into()), OptionSource::Lua)
                .is_err()
        );
        assert!(
            store
                .set(
                    "leader",
                    OptionValue::Str("<Space>".into()),
                    OptionSource::Lua
                )
                .is_ok()
        );
        assert!(
            store
                .set("theme", OptionValue::Str(String::new()), OptionSource::Lua)
                .is_err()
        );
        assert!(
            store
                .set(
                    "thinking_level",
                    OptionValue::Str(String::new()),
                    OptionSource::Lua
                )
                .is_ok()
        );
        let err = store
            .set("nope", OptionValue::Bool(true), OptionSource::Lua)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("theme, theme_dark, theme_light, mouse"),
            "{err}"
        );
    }

    /// `0` is a valid fraction that turns compaction off, so a
    /// validated `0` must reach the loop config unchanged.
    #[test]
    fn compaction_threshold_validates_zero() {
        let mut store = OptionStore::default();
        let change = store
            .set(
                "compaction_threshold",
                OptionValue::Float(0.0),
                OptionSource::Lua,
            )
            .unwrap();
        assert_eq!(change.new, OptionValue::Float(0.0));
        assert_eq!(
            store.get("compaction_threshold"),
            Some(&OptionValue::Float(0.0))
        );
    }

    #[test]
    fn agent_limits_validate_their_ranges() {
        let mut store = OptionStore::default();
        for (name, low, high) in [
            ("agent_max_depth", 0, 3),
            ("agent_max_running", 1, 16),
            ("swarm_max_items", 2, 128),
            ("swarm_timeout_ms", 1_000, 86_400_000),
            ("agent_max_turns", 0, 10_000),
            ("agent_timeout", 0, 86_400),
            ("agent_budget", 0, 1_000_000_000),
        ] {
            for ok in [low, high] {
                assert!(
                    store
                        .set(name, OptionValue::Int(ok), OptionSource::Lua)
                        .is_ok(),
                    "{name} = {ok}"
                );
            }
            for bad in [low - 1, high + 1] {
                let err = store
                    .set(name, OptionValue::Int(bad), OptionSource::Lua)
                    .unwrap_err();
                assert!(
                    err.to_string().contains(&format!("from {low} to {high}")),
                    "{err}"
                );
            }
        }
        let cfg = load("[agents]\nmax_depth = 5\nmax_running = 0\n");
        let (store, errors) = OptionStore::from_config(&cfg);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert_eq!(store.get("agent_max_depth"), Some(&OptionValue::Int(1)));
        assert_eq!(store.get("agent_max_running"), Some(&OptionValue::Int(4)));
    }

    #[test]
    fn transcript_on_exit_takes_its_three_values() {
        let mut store = OptionStore::default();
        assert_eq!(
            store.get("transcript_on_exit"),
            Some(&OptionValue::Str("full".into()))
        );
        for value in ["full", "last", "none"] {
            assert!(
                store
                    .set(
                        "transcript_on_exit",
                        OptionValue::Str(value.into()),
                        OptionSource::Lua
                    )
                    .is_ok(),
                "{value}"
            );
        }
        let err = store
            .set(
                "transcript_on_exit",
                OptionValue::Str("all".into()),
                OptionSource::Lua,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("\"full\", \"last\", \"none\""),
            "{err}"
        );
    }

    #[test]
    fn set_records_source_and_queues_changes() {
        let mut store = OptionStore::default();
        let change = store
            .set("mouse", OptionValue::Bool(false), OptionSource::Lua)
            .unwrap();
        assert_eq!(
            change,
            OptionChange {
                name: "mouse",
                old: OptionValue::Bool(true),
                new: OptionValue::Bool(false),
                source: OptionSource::Lua,
            }
        );
        store
            .set(
                "editor",
                OptionValue::Str("vim".into()),
                OptionSource::Runtime,
            )
            .unwrap();
        assert_eq!(store.source("mouse"), Some(OptionSource::Lua));
        assert_eq!(store.source("editor"), Some(OptionSource::Runtime));
        assert_eq!(store.generation(), 2);
        let names: Vec<&str> = store.take_changes().iter().map(|c| c.name).collect();
        assert_eq!(names, ["mouse", "editor"]);
        assert!(store.take_changes().is_empty());
        assert!(
            store
                .set("mouse", OptionValue::Int(0), OptionSource::Lua)
                .is_err()
        );
        assert_eq!(store.generation(), 2);
    }

    #[test]
    fn thinking_levels_match_the_ladder() {
        for level in &THINKING_LEVELS[1..] {
            assert!(
                crate::ThinkingLevel::parse(level).is_some(),
                "{level} is not a thinking level"
            );
        }
    }

    #[test]
    fn option_to_json_maps_each_variant() {
        assert_eq!(
            option_to_json(&OptionValue::Bool(true)),
            serde_json::json!(true)
        );
        assert_eq!(option_to_json(&OptionValue::Int(-3)), serde_json::json!(-3));
        assert_eq!(
            option_to_json(&OptionValue::Float(0.5)),
            serde_json::json!(0.5)
        );
        assert_eq!(
            option_to_json(&OptionValue::Str("vim".into())),
            serde_json::json!("vim")
        );
    }

    #[test]
    fn option_from_json_accepts_scalars_and_rejects_the_rest() {
        assert_eq!(
            option_from_json(&serde_json::json!(true)),
            Some(OptionValue::Bool(true))
        );
        assert_eq!(
            option_from_json(&serde_json::json!(-3)),
            Some(OptionValue::Int(-3))
        );
        assert_eq!(
            option_from_json(&serde_json::json!(1.5)),
            Some(OptionValue::Float(1.5))
        );
        assert_eq!(
            option_from_json(&serde_json::json!("vim")),
            Some(OptionValue::Str("vim".into()))
        );
        assert_eq!(option_from_json(&serde_json::json!(null)), None);
        assert_eq!(option_from_json(&serde_json::json!([1])), None);
        assert_eq!(option_from_json(&serde_json::json!({"a": 1})), None);
    }

    #[test]
    fn save_options_writes_each_option_at_its_toml_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\n[ui]\nmouse = true\n").unwrap();
        save_options(
            &path,
            &[
                (find("mouse").unwrap(), OptionValue::Bool(false)),
                (find("agent_max_depth").unwrap(), OptionValue::Int(2)),
            ],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\n[ui]\nmouse = false\n\n[agents]\nmax_depth = 2\n"
        );
    }

    #[test]
    fn an_idempotent_set_returns_the_change_without_queueing() {
        let mut store = OptionStore::default();
        let first = store
            .set("mouse", OptionValue::Bool(false), OptionSource::Lua)
            .unwrap();
        assert_eq!(first.old, OptionValue::Bool(true));
        assert_eq!(first.new, OptionValue::Bool(false));
        let second = store
            .set("mouse", OptionValue::Bool(false), OptionSource::Lua)
            .unwrap();
        assert_eq!(second.old, OptionValue::Bool(false));
        assert_eq!(second.new, OptionValue::Bool(false));
        let names: Vec<&str> = store.take_changes().iter().map(|c| c.name).collect();
        assert_eq!(names, ["mouse"]);
        assert_eq!(store.generation(), 1);
        let third = store
            .set("mouse", OptionValue::Bool(true), OptionSource::Lua)
            .unwrap();
        assert_eq!(third.old, OptionValue::Bool(false));
        assert_eq!(third.new, OptionValue::Bool(true));
        assert_eq!(store.take_changes().len(), 1);
        assert_eq!(store.generation(), 2);
    }
}
