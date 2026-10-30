//! The plugin tools a session registered, so a reload can swap them.

use std::collections::HashMap;
use std::sync::Arc;

use kage_plugin::PluginRuntime;
use kage_tools::{Tool, ToolRegistry};

/// Names a session's plugin runtime registered, the tools those
/// registrations displaced, and the renames it applied.
#[derive(Default)]
pub(super) struct PluginTools {
    names: Vec<String>,
    displaced: HashMap<String, Arc<dyn Tool>>,
    renames: Vec<(String, String)>,
    /// `[tools.<name>] description` values an override displaced, so a
    /// reload that stops overriding the tool puts them back.
    cleared: HashMap<String, String>,
}

/// A `kage.override_tool` that replaced nothing.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct OverrideMiss {
    /// The tool name the plugin asked to override.
    pub(super) name: String,
    /// `true` when the tool exists but `[tools.<name>] enabled = false`
    /// dropped it, `false` when no tool carries that name at all.
    pub(super) disabled: bool,
}

impl PluginTools {
    /// Remove the previous plugin tools and renames from `tools`,
    /// restore what they displaced, then register what `rt` registers
    /// now and re-apply its renames. Returns the overrides that matched
    /// no live tool, so the caller can tell a disabled tool from a name
    /// nothing ever registered.
    pub(super) fn apply(
        &mut self,
        tools: &mut ToolRegistry,
        rt: &PluginRuntime,
    ) -> Vec<OverrideMiss> {
        for name in self.names.drain(..) {
            tools.unregister(&name);
        }
        for (_, tool) in self.displaced.drain() {
            tools.register(tool);
        }
        for (name, description) in self.cleared.drain() {
            tools.restore_description(&name, description);
        }
        for (from, _) in self.renames.drain(..) {
            tools.remove_rename(&from);
        }
        for tool in rt.registered_tools() {
            self.add(tools, tool);
        }
        let mut unmatched = Vec::new();
        for tool in rt.registered_tool_overrides() {
            let name = tool.name().to_owned();
            if tools.get(&name).is_none() {
                unmatched.push(OverrideMiss {
                    disabled: tools.was_registered(&name),
                    name: name.clone(),
                });
            }
            self.add(tools, tool);
            // The plugin's description matches the code now running, so
            // it replaces a `[tools.<name>] description` written for the
            // tool this override displaced. The displaced value is kept
            // so a reload without the override puts it back.
            if let Some(description) = tools.clear_description(&name) {
                self.cleared.insert(name.clone(), description);
            }
        }
        self.renames = rt.registered_renames();
        for (from, to) in &self.renames {
            tools.rename_in_place(from, to);
        }
        unmatched
    }

    fn add(&mut self, tools: &mut ToolRegistry, tool: Arc<dyn Tool>) {
        let name = tool.name().to_owned();
        if !self.names.contains(&name)
            && let Some(existing) = tools.get(&name)
        {
            self.displaced.insert(name.clone(), Arc::clone(existing));
        }
        tools.register(tool);
        self.names.push(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(lua: &str) -> PluginRuntime {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(lua).unwrap();
        rt
    }

    fn tool(name: &str) -> String {
        format!(
            "kage.register_tool({{ name = '{name}', description = 'd', \
             schema = {{ type = 'object' }}, execute = function() return 'x' end }})"
        )
    }

    #[test]
    fn reapplying_swaps_tools_and_restores_overridden_builtins() {
        let mut tools = kage_tools::builtin_registry();
        let builtin_read = Arc::as_ptr(tools.get("read").unwrap()).cast::<()>();
        let mut plugin_tools = PluginTools::default();

        let first = runtime(&format!(
            "{}\nkage.override_tool({{ name = 'read', description = 'd', \
             schema = {{ type = 'object' }}, execute = function() return 'x' end }})",
            tool("old_tool")
        ));
        assert!(plugin_tools.apply(&mut tools, &first).is_empty());
        assert!(tools.get("old_tool").is_some());
        assert_ne!(
            Arc::as_ptr(tools.get("read").unwrap()).cast::<()>(),
            builtin_read
        );

        let second = runtime(&tool("new_tool"));
        plugin_tools.apply(&mut tools, &second);
        assert!(tools.get("old_tool").is_none());
        assert!(tools.get("new_tool").is_some());
        assert_eq!(
            Arc::as_ptr(tools.get("read").unwrap()).cast::<()>(),
            builtin_read
        );
    }

    #[test]
    fn reapplying_swaps_renames_and_reverts_dropped_ones() {
        let mut tools = kage_tools::builtin_registry();
        let mut plugin_tools = PluginTools::default();

        let first = runtime(
            "kage.rename_tool({ from = 'shell', to = 'run_command' })\n\
             kage.rename_tool({ from = 'web_fetch', to = 'browse' })",
        );
        assert!(plugin_tools.apply(&mut tools, &first).is_empty());
        let names: Vec<String> = tools
            .list_for_provider()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(names.contains(&"run_command".to_owned()), "{names:?}");
        assert!(names.contains(&"browse".to_owned()), "{names:?}");
        assert!(!names.contains(&"shell".to_owned()), "{names:?}");

        // A reload without the shell rename reverts it, keeping the rest.
        let second = runtime("kage.rename_tool({ from = 'web_fetch', to = 'browse' })");
        assert!(plugin_tools.apply(&mut tools, &second).is_empty());
        let names: Vec<String> = tools
            .list_for_provider()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(names.contains(&"shell".to_owned()), "{names:?}");
        assert!(names.contains(&"browse".to_owned()), "{names:?}");
        assert!(!names.contains(&"run_command".to_owned()), "{names:?}");
    }

    #[test]
    fn a_rename_of_an_unknown_tool_changes_nothing() {
        let mut tools = kage_tools::builtin_registry();
        let rt = runtime("kage.rename_tool({ from = 'ghost', to = 'renamed' })");
        assert!(PluginTools::default().apply(&mut tools, &rt).is_empty());
        assert!(tools.get("renamed").is_none());
    }

    /// A tool disabled by `[tools.<name>]` is reported as disabled, not
    /// as a name nothing ever registered.
    #[test]
    fn an_override_of_a_disabled_tool_is_reported_as_disabled() {
        let mut tools = kage_tools::builtin_registry().with_overrides(
            kage_tools::ToolOverrides::from_config(&kage_core::config::ToolsConfig {
                shell: kage_core::config::ShellToolConfig {
                    enabled: false,
                    ..Default::default()
                },
                ..Default::default()
            }),
        );
        let rt = runtime(
            "kage.override_tool({ name = 'shell', description = 'd', \
             schema = { type = 'object' }, execute = function() return 'x' end })",
        );
        let misses = PluginTools::default().apply(&mut tools, &rt);
        assert_eq!(
            misses,
            [OverrideMiss {
                name: "shell".to_owned(),
                disabled: true,
            }]
        );
    }

    #[test]
    fn overrides_without_a_target_are_reported() {
        let mut tools = ToolRegistry::new();
        let rt = runtime(
            "kage.override_tool({ name = 'ghost', description = 'd', \
             schema = { type = 'object' }, execute = function() return 'x' end })",
        );
        let misses = PluginTools::default().apply(&mut tools, &rt);
        assert_eq!(
            misses,
            [OverrideMiss {
                name: "ghost".to_owned(),
                disabled: false,
            }]
        );
    }
}
