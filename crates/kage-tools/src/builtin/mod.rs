//! Built-in tools shipped with kage.

pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod shell;
pub mod todo;
pub mod web_fetch;
pub mod web_search;
pub mod write;

use std::sync::Arc;

pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use shell::ShellTool;
pub use todo::{TodoList, TodoListTool};
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use write::WriteTool;

use kage_core::config::{ShellConfig, ToolsConfig, WebSearchConfig};

use crate::ToolRegistry;
use crate::registry::ToolOverrides;

/// `bytes` the way a tool description states a cap: `2MB`, `100KB`,
/// else the exact count.
pub(crate) fn human_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000 && bytes.is_multiple_of(1_000_000) {
        format!("{}MB", bytes / 1_000_000)
    } else if bytes >= 1_000 && bytes.is_multiple_of(1_000) {
        format!("{}KB", bytes / 1_000)
    } else {
        format!("{bytes} bytes")
    }
}

/// `ms` the way a tool description states a timeout: `120 seconds`,
/// else milliseconds.
pub(crate) fn human_ms(ms: u64) -> String {
    if ms.is_multiple_of(1_000) {
        format!("{} seconds", ms / 1_000)
    } else {
        format!("{ms} milliseconds")
    }
}

/// Deserialize an optional path argument, reading an empty string or the
/// literal `"null"` (which some models send instead of JSON null) as
/// absent.
pub(crate) fn optional_path<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(value.filter(|p| !p.is_empty() && p != "null"))
}

/// Construct a [`ToolRegistry`] with all built-in tools registered,
/// at their default settings.
///
/// Includes: `read`, `write`, `edit`, `shell`, `grep`, `find`, `ls`,
/// `web_fetch`, `web_search` (on DuckDuckGo), `todo_list`. A caller that carries the todo list
/// across a session registers its own with [`ToolRegistry::with_todo_list`].
#[must_use]
pub fn builtin_registry() -> ToolRegistry {
    configured_registry(&ToolsConfig::default(), &ShellConfig::default())
}

/// The built-in tools as `[tools]` and `[shell]` configure them: each
/// tool's limits, the `web_search` engine, the shell program and
/// environment scrub, the renames, and the `enabled` / `description`
/// overrides. The overrides stay on the registry, so tools registered
/// later (delegation, plugin, MCP) follow them too.
#[must_use]
pub fn configured_registry(tools: &ToolsConfig, shell: &ShellConfig) -> ToolRegistry {
    let shell_tool = ShellTool::new(&tools.shell)
        .with_env_scrub(&shell.scrub_env)
        .with_shell(shell.program.as_deref());
    ToolRegistry::new()
        .with(Arc::new(ReadTool::new(&tools.read)))
        .with(Arc::new(WriteTool))
        .with(Arc::new(EditTool::new(&tools.edit)))
        .with(Arc::new(shell_tool))
        .with(Arc::new(GrepTool::new(&tools.grep)))
        .with(Arc::new(FindTool::new(&tools.find)))
        .with(Arc::new(LsTool::new(&tools.ls)))
        .with(Arc::new(WebFetchTool::new(&tools.web_fetch)))
        .with(Arc::new(WebSearchTool::new(tools.web_search.clone())))
        .with_todo_list(TodoList::new())
        .with_renames(&tools.rename)
        .with_overrides(ToolOverrides::from_config(tools))
}

impl ToolRegistry {
    /// Register `todo_list` over `todos`, the session's task list.
    #[must_use]
    pub fn with_todo_list(mut self, todos: TodoList) -> Self {
        self.register(Arc::new(TodoListTool::new(todos)));
        self
    }

    /// Replace the registered `web_search` tool with one asking the
    /// engine `cfg` names. The default config leaves the registry
    /// unchanged. [`configured_registry`] applies the whole `[tools]`
    /// table at once.
    #[must_use]
    pub fn with_web_search(mut self, cfg: &WebSearchConfig) -> Self {
        if !cfg.is_default() {
            self.register(Arc::new(WebSearchTool::new(cfg.clone())));
        }
        self
    }

    /// Replace the registered `shell` tool with one running commands via
    /// `cfg.shell` (default `bash`, see [`ShellTool::with_shell`]) and
    /// stripping environment variables matched by `cfg.scrub_env` (see
    /// [`ShellTool::with_env_scrub`]). A fully default `cfg` leaves the
    /// registry unchanged.
    #[must_use]
    pub fn with_shell_config(mut self, cfg: &ShellConfig) -> Self {
        if cfg.program.is_some() || !cfg.scrub_env.is_empty() {
            let tool = ShellTool::default()
                .with_env_scrub(&cfg.scrub_env)
                .with_shell(cfg.program.as_deref());
            self.register(Arc::new(tool));
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_all_ten_tools() {
        let r = builtin_registry();
        assert_eq!(r.len(), 10);
        let mut names: Vec<&str> = r.names().collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "edit",
                "find",
                "grep",
                "ls",
                "read",
                "shell",
                "todo_list",
                "web_fetch",
                "web_search",
                "write"
            ],
        );
    }

    /// A write has to be visible to the next read, since one tool does
    /// both. Two separate lists would read as the model losing its plan.
    #[test]
    fn a_todo_write_is_visible_to_the_next_read() {
        let r = builtin_registry();
        let cancel = kage_core::CancelFlag::new();
        let cx = crate::ToolContext::new(std::path::Path::new("."), &cancel);
        r.get("todo_list")
            .expect("todo_list")
            .execute(
                serde_json::json!({"todos": [{"title": "shared", "status": "done"}]}),
                &cx,
            )
            .unwrap();
        let out = r
            .get("todo_list")
            .expect("todo_list")
            .execute(serde_json::json!({}), &cx)
            .unwrap();
        assert!(out.text.contains("[x] shared"), "{}", out.text);
    }

    fn configured(toml: &str) -> ToolRegistry {
        let tools: ToolsConfig = toml::from_str(toml).expect("tools table parses");
        configured_registry(&tools, &ShellConfig::default())
    }

    fn spec(r: &ToolRegistry, name: &str) -> kage_core::ToolSpec {
        r.list_for_provider()
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} is listed"))
    }

    #[test]
    fn default_config_matches_the_plain_builtin_registry() {
        let plain = builtin_registry().list_for_provider();
        let configured = configured("").list_for_provider();
        assert_eq!(plain.len(), configured.len());
        for (a, b) in plain.iter().zip(&configured) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.description, b.description, "{}", a.name);
            assert_eq!(a.schema, b.schema, "{}", a.name);
        }
    }

    #[test]
    fn default_descriptions_keep_their_wording() {
        let r = builtin_registry();
        assert!(spec(&r, "read").description.contains("capped at 2MB"));
        assert!(
            spec(&r, "grep")
                .description
                .contains("Capped at 1000 matches by default.")
        );
        assert!(
            spec(&r, "find")
                .description
                .contains("capped at 5000 entries.")
        );
        assert!(
            spec(&r, "ls")
                .description
                .contains("capped at 5000 entries.")
        );
        assert!(
            spec(&r, "web_fetch")
                .description
                .contains("capped at 2MB by default, 5MB at most.")
        );
        assert!(
            spec(&r, "shell")
                .description
                .contains("Default timeout is 120 seconds; output is truncated at 100KB.")
        );
    }

    /// A configured limit rewrites the number in the derived schema
    /// doc. If a doc comment is reworded, the anchor no longer matches
    /// and the rewrite silently does nothing, so pin the anchors to the
    /// derived text.
    #[test]
    fn schema_doc_anchors_match_the_derived_docs() {
        let r = builtin_registry();
        for (tool, key, anchor) in [
            ("web_fetch", "max_bytes", web_fetch::DEFAULT_MAX_BYTES_DOC),
            ("shell", "timeout_ms", shell::DEFAULT_TIMEOUT_DOC),
            ("web_search", "count", web_search::DEFAULT_COUNT_DOC),
            ("grep", "max_matches", grep::DEFAULT_MATCHES_DOC),
        ] {
            let doc = spec(&r, tool).schema["properties"][key]["description"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            assert!(
                doc.contains(anchor),
                "{tool}.{key}: {doc:?} lacks {anchor:?}"
            );
        }
    }

    #[test]
    fn disabled_builtins_are_not_registered() {
        let r = configured(
            "[web_search]\nenabled = false\n[web_fetch]\nenabled = false\n[todo_list]\nenabled = false\n",
        );
        for name in ["web_search", "web_fetch", "todo_list"] {
            assert!(r.get(name).is_none(), "{name} is still registered");
        }
        assert_eq!(r.len(), 7);
    }

    #[test]
    fn descriptions_and_limits_reach_the_model() {
        let r = configured(
            r#"
            [rename]
            shell = "run"
            [shell]
            description = "Run one command."
            timeout_ms = 300000
            [read]
            max_bytes = 500000
            [grep]
            max_matches = 50
            [find]
            max_entries = 10
            [web_fetch]
            max_bytes = 1000000
            max_bytes_limit = 3000000
            [web_search]
            count = 3
            max_count = 5
            [write]
            description = "Write a file."
            "#,
        );
        let shell = spec(&r, "run");
        assert_eq!(shell.description, "Run one command.");
        assert!(
            shell.schema["properties"]["timeout_ms"]["description"]
                .as_str()
                .unwrap()
                .contains("300000"),
            "{}",
            shell.schema
        );
        assert_eq!(spec(&r, "write").description, "Write a file.");
        assert!(spec(&r, "read").description.contains("capped at 500KB"));
        let grep = spec(&r, "grep");
        assert!(grep.description.contains("Capped at 50 matches"));
        assert!(
            grep.schema["properties"]["max_matches"]["description"]
                .as_str()
                .unwrap()
                .contains("Defaults to 50.")
        );
        assert!(
            spec(&r, "find")
                .description
                .contains("capped at 10 entries")
        );
        let fetch = spec(&r, "web_fetch");
        assert!(
            fetch
                .description
                .contains("capped at 1MB by default, 3MB at most.")
        );
        assert!(
            fetch.schema["properties"]["max_bytes"]["description"]
                .as_str()
                .unwrap()
                .contains("Defaults to 1,000,000 and is\nhard-capped at 3,000,000.")
        );
        let search = spec(&r, "web_search");
        assert!(
            search.schema["properties"]["count"]["description"]
                .as_str()
                .unwrap()
                .contains("Defaults to 3, at most 5."),
            "{}",
            search.schema
        );
    }

    #[test]
    fn humanised_limits() {
        assert_eq!(human_bytes(2_000_000), "2MB");
        assert_eq!(human_bytes(100_000), "100KB");
        assert_eq!(human_bytes(1_500), "1500 bytes");
        assert_eq!(human_ms(120_000), "120 seconds");
        assert_eq!(human_ms(1_500), "1500 milliseconds");
    }

    #[test]
    fn each_tool_has_a_schema_and_description() {
        let r = builtin_registry();
        for spec in r.list_for_provider() {
            assert!(
                !spec.description.is_empty(),
                "{} has no description",
                spec.name
            );
            assert_eq!(
                spec.schema["type"], "object",
                "{} has non-object schema",
                spec.name
            );
        }
    }

    /// `path` takes `filePath` as a deserialization alias only. The
    /// schema the model reads must keep a single canonical key, or it
    /// learns to send both and every call is rejected as a duplicate.
    #[test]
    fn filepath_alias_is_absent_from_the_advertised_schema() {
        let r = builtin_registry();
        for spec in r.list_for_provider() {
            let props = &spec.schema["properties"];
            assert!(
                !props
                    .as_object()
                    .is_some_and(|p| p.contains_key("filePath")),
                "{} advertises the filePath alias: {props}",
                spec.name
            );
        }
    }

    /// Every tool that reads a path must still name it `path` in the
    /// schema, since that is the key the alias falls back to.
    #[test]
    fn path_taking_tools_advertise_the_canonical_path_key() {
        let r = builtin_registry();
        for name in ["read", "write", "edit", "ls", "find", "grep"] {
            let spec = r
                .list_for_provider()
                .into_iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("{name} is registered"));
            assert!(
                spec.schema["properties"]["path"].is_object(),
                "{name} has no path property: {}",
                spec.schema
            );
        }
    }
}
