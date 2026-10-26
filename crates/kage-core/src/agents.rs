//! Agent definitions: markdown files with frontmatter that describe a
//! subagent the `agent` tool can start.
//!
//! ```text
//! ~/.config/kage/agents/<name>.md
//! ./.kage/agents/<name>.md
//! ```
//!
//! The file stem is the name, with the skill name rules. Frontmatter
//! keys are `description` (required), `tools` (a comma list, absent
//! means every tool of the parent), `model` (`provider/model`),
//! `thinking` (a thinking level), `max_turns` (also `maxTurns`, a
//! positive integer), `timeout` (seconds, or a number with `s`, `m`
//! or `h`) and `isolation` (`none` or `worktree`). `model` and `thinking` also accept `inherit`, the default.
//! Unknown keys are ignored, so agent files written for other tools
//! load. The body is the agent's role text.
//!
//! The built-ins `general` and `explore` come first, then user files,
//! then project files. A later definition replaces an earlier one of
//! the same name.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::skills::{split_frontmatter_pub, validate_name_pub};
use crate::thinking::ThinkingLevel;

const MAX_DESCRIPTION_LEN: usize = 1024;

const BUILTINS: &[(&str, &str)] = &[
    ("general", include_str!("agents/general.md")),
    ("explore", include_str!("agents/explore.md")),
];

/// One agent definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDef {
    /// Canonical name. Equal to the file stem.
    pub name: String,
    /// What the agent is for. The model reads it to pick an agent.
    pub description: String,
    /// Tool names the agent may use. `None` means every tool the parent
    /// has.
    pub tools: Option<Vec<String>>,
    /// Model in `provider/model` form. `None` inherits the parent's.
    pub model: Option<String>,
    /// Thinking level. `None` inherits the parent's.
    pub thinking: Option<ThinkingLevel>,
    /// Turns one run may take. `None` uses the `agent_max_turns` option.
    pub max_turns: Option<u32>,
    /// How long one run may take. `None` uses the `agent_timeout`
    /// option.
    pub timeout: Option<Duration>,
    /// Where the agent works.
    pub isolation: Isolation,
    /// Role text: the markdown body after the frontmatter.
    pub body: String,
    /// The file the definition was loaded from. `None` for built-ins.
    pub path: Option<PathBuf>,
    /// Where the definition came from.
    pub source: AgentSource,
}

/// Where an agent works.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Isolation {
    /// In its parent's checkout.
    #[default]
    None,
    /// In a checkout of its own: a jj workspace or a git worktree.
    Worktree,
}

/// Where an [`AgentDef`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSource {
    /// Embedded in kage.
    Builtin,
    /// The user config directory.
    User,
    /// The project's agent directories (`.kage/agents`, `.agents/agents`).
    Project,
}

/// Failure mode for [`load_agent_file`].
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// I/O failure while reading the file.
    #[error("read {path:?}: {source}")]
    Io {
        /// Path that failed.
        path: PathBuf,
        /// Underlying io error.
        #[source]
        source: std::io::Error,
    },
    /// Frontmatter was missing or malformed.
    #[error("{path:?}: invalid frontmatter: {message}")]
    Frontmatter {
        /// Path that failed.
        path: PathBuf,
        /// Human-readable reason.
        message: String,
    },
    /// A schema validation rule failed.
    #[error("{path:?}: invalid agent: {message}")]
    Invalid {
        /// Path that failed.
        path: PathBuf,
        /// Human-readable reason.
        message: String,
    },
}

/// The project agent directories of `workdir`: kage's own
/// `<workdir>/.kage/agents`, then the cross-tool standard
/// `<workdir>/.agents/agents`. Load order is the array order, so a
/// definition under `.agents` replaces one of the same name under
/// `.kage`.
#[must_use]
pub fn project_dirs(workdir: &Path) -> [PathBuf; 2] {
    [
        workdir.join(".kage").join("agents"),
        workdir.join(".agents").join("agents"),
    ]
}

/// Load every markdown file directly under `dir`, one result per file.
/// The extension matches case-insensitively, in lockstep with the
/// trust scan. A missing directory is empty.
#[must_use]
pub fn load_agents_dir(dir: &Path, source: AgentSource) -> Vec<Result<AgentDef, AgentError>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
                && path.is_file()
        })
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| load_agent_file(path, source))
        .collect()
}

/// Read and validate one `<name>.md` agent file. The frontmatter
/// `name`, when present, must equal the file stem.
pub fn load_agent_file(path: &Path, source: AgentSource) -> Result<AgentDef, AgentError> {
    let invalid = |message: String| AgentError::Invalid {
        path: path.to_path_buf(),
        message,
    };
    let raw = fs::read_to_string(path).map_err(|source| AgentError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let (front, body) = split_frontmatter_pub(&raw).map_err(|message| AgentError::Frontmatter {
        path: path.to_path_buf(),
        message,
    })?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("agent filename has no stem".to_owned()))?;
    validate_name_pub(stem).map_err(invalid)?;
    let mut def = from_frontmatter(stem, &front, body, source).map_err(invalid)?;
    def.path = Some(path.to_path_buf());
    Ok(def)
}

fn from_frontmatter(
    name: &str,
    front: &HashMap<String, String>,
    body: &str,
    source: AgentSource,
) -> Result<AgentDef, String> {
    if let Some(declared) = front.get("name")
        && declared != name
    {
        return Err(format!(
            "frontmatter name {declared:?} disagrees with filename stem {name:?}"
        ));
    }
    let description = front.get("description").map_or("", |d| d.trim());
    if description.is_empty() {
        return Err("description is required".to_owned());
    }
    if description.len() > MAX_DESCRIPTION_LEN {
        return Err(format!(
            "description is {}/{MAX_DESCRIPTION_LEN} chars",
            description.len()
        ));
    }
    let tools = front.get("tools").map(|list| {
        list.split(',')
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
            .map(str::to_owned)
            .collect()
    });
    let model = match inherited(front, "model") {
        None => None,
        Some(model) => match crate::split_model(model) {
            Some((provider, id)) => Some(crate::qualify_model(provider, id)),
            None => {
                return Err(format!(
                    "model {model:?} is not `provider/model` or `inherit`"
                ));
            }
        },
    };
    let thinking = match inherited(front, "thinking") {
        None => None,
        Some(level) => Some(ThinkingLevel::parse(level).ok_or_else(|| {
            format!(
                "thinking {level:?} is not one of off, minimal, low, medium, high, xhigh, max, ultra or inherit"
            )
        })?),
    };
    let max_turns = match front.get("max_turns").or_else(|| front.get("maxTurns")) {
        None => None,
        Some(value) => Some(
            value
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|turns| *turns > 0)
                .ok_or_else(|| format!("max_turns {value:?} is not a positive integer"))?,
        ),
    };
    let timeout = match front.get("timeout") {
        None => None,
        Some(value) => Some(parse_timeout(value).ok_or_else(|| {
            format!("timeout {value:?} is not seconds or a number with s, m or h")
        })?),
    };
    let isolation = match front.get("isolation").map(|value| value.trim()) {
        None | Some("none") => Isolation::None,
        Some("worktree") => Isolation::Worktree,
        Some(other) => return Err(format!("isolation {other:?} is not none or worktree")),
    };
    Ok(AgentDef {
        name: name.to_owned(),
        description: description.to_owned(),
        tools,
        model,
        thinking,
        max_turns,
        timeout,
        isolation,
        body: body.trim().to_owned(),
        path: None,
        source,
    })
}

/// A positive duration written as seconds (`90`) or a number with a
/// unit (`90s`, `10m`, `1h`).
fn parse_timeout(value: &str) -> Option<Duration> {
    let value = value.trim();
    let (number, unit) = match value.char_indices().last()? {
        (at, 's') => (&value[..at], 1),
        (at, 'm') => (&value[..at], 60),
        (at, 'h') => (&value[..at], 3600),
        _ => (value, 1),
    };
    let seconds = number.trim().parse::<u64>().ok()?.checked_mul(unit)?;
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

/// The value of `key`, or `None` when it is absent or `inherit`.
fn inherited<'a>(front: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    front
        .get(key)
        .map(|value| value.trim())
        .filter(|value| *value != "inherit")
}

/// Agent definitions by name.
#[derive(Clone, Debug, Default)]
pub struct AgentDefs {
    defs: BTreeMap<String, AgentDef>,
}

impl AgentDefs {
    /// The built-in definitions, `general` and `explore`.
    #[must_use]
    pub fn builtin() -> Self {
        let mut defs = Self::default();
        for (name, raw) in BUILTINS {
            let def = split_frontmatter_pub(raw)
                .and_then(|(front, body)| {
                    from_frontmatter(name, &front, body, AgentSource::Builtin)
                })
                .unwrap_or_else(|e| panic!("built-in agent {name}: {e}"));
            defs.insert(def);
        }
        defs
    }

    /// Add `def`, replacing any definition of the same name.
    pub fn insert(&mut self, def: AgentDef) {
        self.defs.insert(def.name.clone(), def);
    }

    /// The definition named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&AgentDef> {
        self.defs.get(name)
    }

    /// Every definition, by name.
    pub fn iter(&self) -> impl Iterator<Item = &AgentDef> {
        self.defs.values()
    }

    /// One `- <name>: <description>` line per definition, for the
    /// `agent` tool description.
    #[must_use]
    pub fn tool_listing(&self) -> String {
        self.iter()
            .map(|def| format!("- {}: {}", def.name, def.description))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    fn load(body: &str) -> Result<AgentDef, AgentError> {
        let dir = tempfile::tempdir().unwrap();
        load_agent_file(&write(dir.path(), "reviewer.md", body), AgentSource::User)
    }

    #[test]
    fn file_with_every_key_parses() {
        let def = load(
            "---\nname: reviewer\ndescription: Reviews a diff.\ntools: read, grep\n\
             model: anthropic:claude-sonnet-4-6\nthinking: high\n---\nYou review code.\n",
        )
        .unwrap();
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "Reviews a diff.");
        assert_eq!(def.tools, Some(vec!["read".to_owned(), "grep".to_owned()]));
        assert_eq!(
            def.model.as_deref(),
            Some("anthropic/claude-sonnet-4-6"),
            "the older form reads and comes back with a slash"
        );
        assert_eq!(def.thinking, Some(ThinkingLevel::High));
        assert_eq!(def.body, "You review code.");
        assert_eq!(def.source, AgentSource::User);
        assert!(def.path.unwrap().ends_with("reviewer.md"));
    }

    #[test]
    fn description_only_gets_the_defaults() {
        let def = load("---\ndescription: Reviews a diff.\n---\nBody\n").unwrap();
        assert_eq!(def.tools, None);
        assert_eq!(def.model, None);
        assert_eq!(def.thinking, None);
    }

    #[test]
    fn turn_limits_and_timeouts_parse() {
        for (front, turns) in [("max_turns: 60", 60), ("maxTurns: 7", 7)] {
            let def = load(&format!("---\ndescription: d\n{front}\n---\n")).unwrap();
            assert_eq!(def.max_turns, Some(turns), "{front}");
        }
        for (value, seconds) in [("90", 90), ("90s", 90), ("10m", 600), ("1h", 3600)] {
            let def = load(&format!("---\ndescription: d\ntimeout: {value}\n---\n")).unwrap();
            assert_eq!(def.timeout, Some(Duration::from_secs(seconds)), "{value}");
        }
        for bad in [
            "max_turns: 0",
            "max_turns: many",
            "timeout: 0",
            "timeout: 5d",
        ] {
            let err = load(&format!("---\ndescription: d\n{bad}\n---\n")).unwrap_err();
            assert!(err.to_string().contains("reviewer.md"), "{bad}: {err}");
        }
        let def = load("---\ndescription: d\n---\n").unwrap();
        assert_eq!((def.max_turns, def.timeout), (None, None));
    }

    #[test]
    fn isolation_is_none_or_worktree() {
        let def = load("---\ndescription: d\nisolation: worktree\n---\n").unwrap();
        assert_eq!(def.isolation, Isolation::Worktree);
        let def = load("---\ndescription: d\nisolation: none\n---\n").unwrap();
        assert_eq!(def.isolation, Isolation::None);
        let err = load("---\ndescription: d\nisolation: sandbox\n---\n").unwrap_err();
        assert!(err.to_string().contains("reviewer.md"), "{err}");
    }

    #[test]
    fn inherit_means_none() {
        let def = load("---\ndescription: d\nmodel: inherit\nthinking: inherit\n---\n").unwrap();
        assert_eq!(def.model, None);
        assert_eq!(def.thinking, None);
    }

    #[test]
    fn tools_split_and_trim() {
        let def = load("---\ndescription: d\ntools:  read ,grep,, \n---\n").unwrap();
        assert_eq!(def.tools, Some(vec!["read".to_owned(), "grep".to_owned()]));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let def = load("---\ndescription: d\ncolor: blue\nmode: subagent\n---\n").unwrap();
        assert_eq!(def.description, "d");
    }

    #[test]
    fn invalid_files_fail_with_the_path() {
        let dir = tempfile::tempdir().unwrap();
        for (file, body, reason) in [
            (
                "reviewer.md",
                "---\ntools: read\n---\n",
                "description is required",
            ),
            (
                "Bad_Name.md",
                "---\ndescription: d\n---\n",
                "illegal character",
            ),
            (
                "reviewer.md",
                "---\nname: other\ndescription: d\n---\n",
                "disagrees with filename stem",
            ),
            (
                "reviewer.md",
                "---\ndescription: d\nmodel: sonnet\n---\n",
                "provider/model",
            ),
            (
                "reviewer.md",
                "---\ndescription: d\nthinking: huge\n---\n",
                "not one of",
            ),
            (
                "reviewer.md",
                "---\ndescription: d\n",
                "invalid frontmatter",
            ),
        ] {
            let path = write(dir.path(), file, body);
            let err = load_agent_file(&path, AgentSource::User)
                .unwrap_err()
                .to_string();
            assert!(err.contains(reason), "{err}");
            assert!(err.contains(file), "{err}");
        }
    }

    #[test]
    fn dir_loads_flat_markdown_files_only() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "b.md", "---\ndescription: b\n---\n");
        write(dir.path(), "a.md", "---\ndescription: a\n---\n");
        write(dir.path(), "notes.txt", "ignored");
        fs::create_dir(dir.path().join("nested.md")).unwrap();
        let names: Vec<String> = load_agents_dir(dir.path(), AgentSource::Project)
            .into_iter()
            .map(|r| r.unwrap().name)
            .collect();
        assert_eq!(names, ["a", "b"]);
        assert!(load_agents_dir(Path::new("/definitely/not/here"), AgentSource::User).is_empty());
    }

    #[test]
    fn dir_loads_the_md_extension_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "reviewer.MD", "---\ndescription: caps\n---\n");
        write(dir.path(), "notes.mdown", "ignored");
        let names: Vec<String> = load_agents_dir(dir.path(), AgentSource::User)
            .into_iter()
            .map(|r| r.unwrap().name)
            .collect();
        assert_eq!(names, ["reviewer"]);
    }

    #[test]
    fn builtins_parse_and_list() {
        let defs = AgentDefs::builtin();
        let general = defs.get("general").unwrap();
        assert_eq!(general.tools, None);
        assert_eq!(general.source, AgentSource::Builtin);
        assert!(general.path.is_none());
        let explore = defs.get("explore").unwrap();
        assert_eq!(
            explore.tools.as_deref(),
            Some(
                ["read", "grep", "find", "ls", "web_fetch", "web_search"]
                    .map(str::to_owned)
                    .as_slice()
            )
        );
        let listing = defs.tool_listing();
        assert!(listing.starts_with("- explore: "), "{listing}");
        assert!(listing.contains("\n- general: "), "{listing}");
    }

    #[test]
    fn a_user_file_replaces_the_builtin() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "explore.md",
            "---\ndescription: mine\n---\nMine.\n",
        );
        let mut defs = AgentDefs::builtin();
        for def in load_agents_dir(dir.path(), AgentSource::User) {
            defs.insert(def.unwrap());
        }
        let explore = defs.get("explore").unwrap();
        assert_eq!(explore.description, "mine");
        assert_eq!(explore.source, AgentSource::User);
        assert_eq!(explore.tools, None);
        assert_eq!(defs.iter().count(), 2);
    }
}
