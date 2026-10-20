//! Lookup of [`Tool`] implementations by name.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use kage_core::ToolSpec;

use crate::Tool;

/// Registry of tools keyed by their stable name.
///
/// Cloning is cheap: the inner map uses `Arc<dyn Tool>` so all clones share
/// the same instances.
#[derive(Clone, Debug, Default)]
pub struct ToolRegistry {
    /// Sorted by name, so `list_for_provider` emits a stable tool
    /// order: a stable prefix is what lets the provider's prompt cache
    /// survive a restart or reload.
    tools: BTreeMap<String, Arc<dyn Tool>>,
    /// Alternate names that resolve to a registered tool, used when a
    /// model reaches for a familiar name (`bash`) that the registry
    /// hosts under another (`shell`). Aliases never appear in
    /// listings.
    aliases: BTreeMap<String, String>,
    /// Real name to advertised name for tools a host renamed: the
    /// model sees only the advertised name, while lookups under either
    /// name run the same tool.
    renamed: BTreeMap<String, String>,
    /// `[tools.<name>]` overrides: which tools stay out, and which get
    /// another description. They apply to every tool, whenever it is
    /// registered, so a tool added later (MCP, plugin, delegation)
    /// follows them too.
    overrides: ToolOverrides,
}

/// Per-tool overrides a registry applies to everything registered in
/// it, keyed by the tool's real name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolOverrides {
    /// Tools never registered: the model does not see them and a call
    /// resolves as unknown.
    pub disabled: BTreeSet<String>,
    /// Descriptions advertised in place of the tool's own.
    pub descriptions: BTreeMap<String, String>,
}

impl ToolOverrides {
    /// The overrides `[tools]` configures.
    #[must_use]
    pub fn from_config(cfg: &kage_core::config::ToolsConfig) -> Self {
        Self {
            disabled: cfg.disabled(),
            descriptions: cfg.descriptions(),
        }
    }

    /// Whether nothing is overridden.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.disabled.is_empty() && self.descriptions.is_empty()
    }
}

impl ToolRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. Replaces any prior entry with the same name.
    ///
    /// Returns `self` for chaining.
    #[must_use]
    pub fn with(mut self, tool: Arc<dyn Tool>) -> Self {
        self.register(tool);
        self
    }

    /// Register a tool. Replaces any prior entry with the same name.
    /// A tool the overrides disable is dropped instead.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        if self.overrides.disabled.contains(tool.name()) {
            return;
        }
        self.tools.insert(tool.name().to_owned(), tool);
    }

    /// Apply `overrides` to this registry and to every tool registered
    /// in it from now on: disabled tools already present are removed.
    ///
    /// Returns `self` for chaining.
    #[must_use]
    pub fn with_overrides(mut self, overrides: ToolOverrides) -> Self {
        self.set_overrides(overrides);
        self
    }

    /// In-place variant of [`Self::with_overrides`].
    pub fn set_overrides(&mut self, overrides: ToolOverrides) {
        for name in &overrides.disabled {
            self.tools.remove(name);
        }
        self.overrides = overrides;
    }

    /// The overrides this registry applies.
    #[must_use]
    pub fn overrides(&self) -> &ToolOverrides {
        &self.overrides
    }

    /// Whether the overrides keep `name` out of this registry.
    #[must_use]
    pub fn is_disabled(&self, name: &str) -> bool {
        self.overrides.disabled.contains(name)
    }

    /// Remove a tool by name, returning it if it was registered.
    ///
    /// Used by the MCP manager to drop a server's stale adapters on a
    /// hot tool-list refresh so a tool the server no longer offers
    /// does not linger.
    pub fn unregister(&mut self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.remove(name)
    }

    /// Register `from` as an alternate name for the tool registered
    /// under `to`. Lookup through [`Self::get`] follows aliases;
    /// listings and [`Self::names`] never show them.
    ///
    /// Returns `self` for chaining.
    #[must_use]
    pub fn alias(mut self, from: &str, to: &str) -> Self {
        self.aliases.insert(from.to_owned(), to.to_owned());
        self
    }

    /// Advertise the tool registered under `from` as `to`: listings
    /// show only `to`, and lookups under either name run the same
    /// tool. Renaming a name that no tool carries is harmless: the
    /// entry sits unused until something registers `from`.
    ///
    /// Returns `self` for chaining.
    #[must_use]
    pub fn rename(mut self, from: &str, to: &str) -> Self {
        self.rename_in_place(from, to);
        self
    }

    /// In-place variant of [`Self::rename`] for hosts holding the
    /// registry behind a `&mut`.
    pub fn rename_in_place(&mut self, from: &str, to: &str) {
        if from.is_empty() || to.is_empty() || from == to {
            return;
        }
        self.aliases.insert(to.to_owned(), from.to_owned());
        self.renamed.insert(from.to_owned(), to.to_owned());
    }

    /// Undo a [`Self::rename`]: the tool is advertised under its real
    /// name again. Removing an unknown rename does nothing.
    pub fn remove_rename(&mut self, from: &str) {
        if let Some(to) = self.renamed.remove(from) {
            self.aliases.remove(&to);
        }
    }

    /// Apply `[tools.rename]` entries: advertise the key under the
    /// value.
    #[must_use]
    pub fn with_renames(mut self, renames: &BTreeMap<String, String>) -> Self {
        for (from, to) in renames {
            self = self.rename(from, to);
        }
        self
    }

    /// The real name to advertised name map the host applied.
    #[must_use]
    pub fn renames(&self) -> &BTreeMap<String, String> {
        &self.renamed
    }

    /// Look up a tool by name, following alias chains. A missing
    /// target, or an alias cycle, resolves to `None`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        let mut current = name;
        // Alias chains are bounded in practice; the cap only exists to
        // turn a hand-written cycle into `None` instead of a hang.
        for _ in 0..8 {
            match self.tools.get(current) {
                Some(tool) => return Some(tool),
                None => {
                    current = self.aliases.get(current)?;
                }
            }
        }
        None
    }

    /// The registered name `name` resolves to, following aliases.
    ///
    /// This is the name a call should be judged under: permission
    /// rules, session approvals, and the transcript are all keyed by
    /// the tool's real name, so a call that arrived as `bash` must be
    /// evaluated as `shell` or the rules for `shell` never apply.
    ///
    /// Returns `name` unchanged when it is not an alias.
    #[must_use]
    pub fn canonical_name<'a>(&'a self, name: &'a str) -> &'a str {
        let mut current = name;
        // Bounded like `get`: a hand-written cycle must not hang.
        for _ in 0..8 {
            match self.tools.get(current) {
                Some(tool) => return tool.name(),
                None => match self.aliases.get(current) {
                    Some(target) => current = target,
                    None => return name,
                },
            }
        }
        name
    }

    /// Every alias as a plain `from -> to` map, for hosts that must
    /// resolve a name without holding the registry.
    ///
    /// The permission gate keeps its own copy so a call that arrived
    /// under an alias is judged under the tool's real name; see
    /// [`Self::canonical_name`] for what that prevents.
    #[must_use]
    pub fn alias_map(&self) -> BTreeMap<String, String> {
        self.aliases.clone()
    }

    /// Narrow to the tools named in `keep`, preserving aliases.
    ///
    /// Each name is resolved through [`Self::get`], so an alias listed
    /// by its alternate name (`bash`) keeps the tool it points at. An
    /// alias survives only if its target is still present, so the result
    /// never carries a dangling alias. Names that match no tool, alias
    /// included, are returned so the caller can report them.
    ///
    /// Narrowing by rebuilding a registry by hand would silently drop
    /// every alias, which is how a sub-agent ended up unable to run the
    /// `bash` calls its model asked for.
    #[must_use]
    pub fn retain_named(&self, keep: &[String]) -> (Self, Vec<String>) {
        let mut out = Self::new().with_overrides(self.overrides.clone());
        let mut missing = Vec::new();
        for name in keep {
            match self.get(name) {
                Some(tool) => out.register(Arc::clone(tool)),
                None => missing.push(name.clone()),
            }
        }
        out.aliases = self
            .aliases
            .iter()
            .filter(|(_, target)| out.tools.contains_key(*target))
            .map(|(from, to)| (from.clone(), to.clone()))
            .collect();
        out.renamed = self
            .renamed
            .iter()
            .filter(|(real, _)| out.tools.contains_key(*real))
            .map(|(real, to)| (real.clone(), to.clone()))
            .collect();
        (out, missing)
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Iterate registered tool names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    /// The description the model reads for the tool registered as
    /// `name`: the configured override, else the tool's own.
    #[must_use]
    pub fn description_of(&self, name: &str) -> Option<&str> {
        let tool = self.get(name)?;
        Some(
            self.overrides
                .descriptions
                .get(tool.name())
                .map_or_else(|| tool.description(), String::as_str),
        )
    }

    /// Snapshot of all registered tools as [`ToolSpec`]s for the
    /// provider. A renamed tool is listed only under its advertised
    /// name, and a configured description replaces the tool's own.
    #[must_use]
    pub fn list_for_provider(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|t| {
                let name = self
                    .renamed
                    .get(t.name())
                    .map_or_else(|| t.name(), String::as_str);
                let description = self
                    .overrides
                    .descriptions
                    .get(t.name())
                    .map_or_else(|| t.description(), String::as_str);
                ToolSpec {
                    name: name.to_owned(),
                    description: description.to_owned(),
                    schema: t.schema(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use kage_core::{CancelFlag, Risk, ToolOutput};

    use super::*;
    use crate::{Tool, ToolContext, ToolError};

    #[derive(Debug)]
    struct EchoTool {
        name: &'static str,
    }

    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "echo input back"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}})
        }
        fn risk(&self) -> Risk {
            Risk::Read
        }
        fn execute(
            &self,
            input: serde_json::Value,
            _cx: &ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            let text = input
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            Ok(ToolOutput {
                is_error: false,
                text,
                structured: None,
                terminate: false,
                media: Vec::new(),
            })
        }
    }

    fn echo(name: &'static str) -> Arc<dyn Tool> {
        Arc::new(EchoTool { name })
    }

    fn overrides(disabled: &[&str], descriptions: &[(&str, &str)]) -> ToolOverrides {
        ToolOverrides {
            disabled: disabled.iter().map(|s| (*s).to_owned()).collect(),
            descriptions: descriptions
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn disabled_tools_are_dropped_now_and_later() {
        let mut r = ToolRegistry::new()
            .with(echo("greet"))
            .with(echo("farewell"))
            .alias("hi", "greet")
            .with_overrides(overrides(&["greet", "late"], &[]));
        assert!(r.get("greet").is_none());
        assert!(
            r.get("hi").is_none(),
            "an alias of a disabled tool resolves to nothing"
        );
        assert!(r.get("farewell").is_some());
        r.register(echo("late"));
        assert!(r.get("late").is_none(), "registered after the overrides");
        r.register(echo("greet"));
        assert!(
            r.get("greet").is_none(),
            "re-registering does not bring it back"
        );
        assert!(r.is_disabled("greet"));
        let names: Vec<String> = r.list_for_provider().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["farewell"]);
    }

    #[test]
    fn description_overrides_follow_renames_and_narrowing() {
        let r = ToolRegistry::new()
            .with(echo("greet"))
            .with(echo("farewell"))
            .rename("greet", "hello")
            .with_overrides(overrides(&["gone"], &[("greet", "say hi")]));
        let specs = r.list_for_provider();
        let hello = specs
            .iter()
            .find(|s| s.name == "hello")
            .expect("advertised");
        assert_eq!(hello.description, "say hi");
        let farewell = specs.iter().find(|s| s.name == "farewell").expect("listed");
        assert_eq!(farewell.description, "echo input back");
        assert_eq!(r.description_of("hello"), Some("say hi"));
        assert_eq!(r.description_of("greet"), Some("say hi"));

        let (mut narrow, _) = r.retain_named(&["greet".to_owned()]);
        assert_eq!(narrow.list_for_provider()[0].description, "say hi");
        narrow.register(echo("gone"));
        assert!(
            narrow.get("gone").is_none(),
            "narrowed registry keeps the overrides"
        );
    }

    #[test]
    fn empty_registry() {
        let r = ToolRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.get("anything").is_none());
        assert!(r.list_for_provider().is_empty());
    }

    #[test]
    fn registered_tool_resolves_by_name() {
        let r = ToolRegistry::new().with(echo("greet"));
        assert_eq!(r.len(), 1);
        let tool = r.get("greet").expect("present");
        assert_eq!(tool.name(), "greet");
    }

    #[test]
    fn register_replaces_existing_entry() {
        let mut r = ToolRegistry::new();
        r.register(echo("greet"));
        r.register(echo("greet"));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn list_for_provider_emits_all_tools() {
        let r = ToolRegistry::new()
            .with(echo("greet"))
            .with(echo("farewell"));
        let mut specs: Vec<String> = r.list_for_provider().into_iter().map(|s| s.name).collect();
        specs.sort();
        assert_eq!(specs, vec!["farewell", "greet"]);
    }

    #[test]
    fn alias_resolves_to_the_target_tool() {
        let r = ToolRegistry::new().with(echo("shell")).alias("sh", "shell");
        assert_eq!(r.get("sh").unwrap().name(), "shell");
        assert!(r.get("nope").is_none());
        // Aliases are invisible to listings.
        assert_eq!(r.names().collect::<Vec<_>>(), vec!["shell"]);
    }

    #[test]
    fn alias_chains_resolve_and_cycles_stop() {
        let r = ToolRegistry::new()
            .with(echo("c"))
            .alias("a", "b")
            .alias("b", "c");
        assert_eq!(r.get("a").unwrap().name(), "c");
        let r = ToolRegistry::new()
            .with(echo("x"))
            .alias("p", "q")
            .alias("q", "p");
        assert!(r.get("p").is_none());
    }

    #[test]
    fn alias_survives_a_renamed_target() {
        // A plugin override under the real name keeps the alias working.
        let mut r = ToolRegistry::new().with(echo("shell")).alias("sh", "shell");
        r.register(echo("shell2"));
        r.register(echo("shell"));
        assert_eq!(r.get("sh").unwrap().name(), "shell");
    }

    #[test]
    fn retain_named_keeps_aliases_whose_target_survives() {
        let parent = ToolRegistry::new()
            .with(echo("shell"))
            .with(echo("read"))
            .alias("sh", "shell");

        // Narrowing to the target keeps the alias working.
        let (narrow, missing) = parent.retain_named(&["shell".to_owned()]);
        assert!(missing.is_empty());
        assert_eq!(narrow.get("sh").unwrap().name(), "shell");
        assert!(narrow.get("read").is_none());

        // Narrowing by the alias name keeps the tool, and the alias with it.
        let (by_alias, missing) = parent.retain_named(&["sh".to_owned()]);
        assert!(missing.is_empty());
        assert_eq!(by_alias.get("shell").unwrap().name(), "shell");
        assert_eq!(by_alias.get("sh").unwrap().name(), "shell");
    }

    #[test]
    fn retain_named_drops_aliases_whose_target_is_gone() {
        // A kept tool with no alias gains none: the result is exactly
        // what was asked for.
        let parent = ToolRegistry::new()
            .with(echo("shell"))
            .with(echo("read"))
            .alias("sh", "shell");
        let (narrow, _) = parent.retain_named(&["read".to_owned()]);
        assert_eq!(narrow.names().collect::<Vec<_>>(), vec!["read"]);
        assert!(narrow.get("sh").is_none(), "dangling alias kept");
    }

    #[test]
    fn retain_named_reports_names_that_match_nothing() {
        let parent = ToolRegistry::new()
            .with(echo("read"))
            .with(echo("shell"))
            .alias("sh", "shell");
        let (narrow, missing) =
            parent.retain_named(&["read".to_owned(), "ghost".to_owned(), "sh".to_owned()]);
        assert_eq!(missing, vec!["ghost".to_owned()]);
        // `sh` resolved through its alias even though the target
        // was requested under its alternate name.
        assert_eq!(narrow.get("sh").unwrap().name(), "shell");
        assert!(narrow.get("read").is_some());
    }

    /// An alias pointing at an unregistered tool cannot resolve, so it
    /// is reported as missing rather than silently dropped.
    #[test]
    fn retain_named_reports_an_alias_with_no_registered_target() {
        let parent = ToolRegistry::new().with(echo("read")).alias("sh", "shell");
        let (narrow, missing) = parent.retain_named(&["read".to_owned(), "sh".to_owned()]);
        assert_eq!(missing, vec!["sh".to_owned()]);
        assert!(narrow.get("sh").is_none());
    }

    /// An alias resolves to the same canonical name as its target, which
    /// is what a permission rule keyed on that name must be checked
    /// against.
    #[test]
    fn canonical_name_follows_aliases() {
        let r = ToolRegistry::new()
            .with(echo("shell"))
            .with(echo("read"))
            .alias("sh", "shell");
        assert_eq!(r.canonical_name("sh"), "shell");
        assert_eq!(r.canonical_name("shell"), "shell");
        // A real name is untouched.
        assert_eq!(r.canonical_name("read"), "read");
        // An unknown name is returned as given, not resolved to nothing.
        assert_eq!(r.canonical_name("ghost"), "ghost");
    }

    /// A chain resolves all the way to the registered tool.
    #[test]
    fn canonical_name_resolves_chains_and_survives_cycles() {
        let r = ToolRegistry::new()
            .with(echo("c"))
            .alias("a", "b")
            .alias("b", "c");
        assert_eq!(r.canonical_name("a"), "c");

        // A cycle must terminate rather than hang.
        let r = ToolRegistry::new()
            .with(echo("x"))
            .alias("p", "q")
            .alias("q", "p");
        assert_eq!(r.canonical_name("p"), "p");
    }

    /// A rename advertises only the new name, while lookups under
    /// either name run the same tool and gate under the real one.
    #[test]
    fn a_rename_advertises_only_the_new_name() {
        let r = ToolRegistry::new()
            .with(echo("shell"))
            .rename("shell", "run_command");
        let names: Vec<String> = r.list_for_provider().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["run_command".to_owned()]);
        assert!(r.get("run_command").is_some(), "alias lookup broken");
        assert_eq!(r.get("run_command").unwrap().name(), "shell");
        assert!(r.get("shell").is_some(), "real lookup broken");
        // The gate keys rules on the real name.
        assert_eq!(r.canonical_name("run_command"), "shell");
        // `names` stays the registered truth.
        assert_eq!(r.names().collect::<Vec<_>>(), vec!["shell"]);
    }

    #[test]
    fn a_rename_of_an_unknown_tool_sits_unused() {
        let r = ToolRegistry::new()
            .with(echo("read"))
            .rename("ghost", "renamed");
        let names: Vec<String> = r.list_for_provider().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["read".to_owned()]);
        assert!(r.get("renamed").is_none());
    }

    #[test]
    fn empty_and_identity_renames_are_ignored() {
        let r = ToolRegistry::new()
            .with(echo("shell"))
            .rename("shell", "")
            .rename("", "run_command")
            .rename("shell", "shell");
        let names: Vec<String> = r.list_for_provider().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["shell".to_owned()]);
        assert!(r.renames().is_empty());
    }

    /// Renames travel with the tool through narrowing: kept tools stay
    /// renamed, dropped tools lose their rename.
    #[test]
    fn retain_named_keeps_a_rename_whose_tool_survives() {
        let parent = ToolRegistry::new()
            .with(echo("shell"))
            .with(echo("read"))
            .rename("shell", "run_command");
        let (narrow, _) = parent.retain_named(&["shell".to_owned()]);
        let names: Vec<String> = narrow
            .list_for_provider()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["run_command".to_owned()]);

        let (narrow, _) = parent.retain_named(&["read".to_owned()]);
        let names: Vec<String> = narrow
            .list_for_provider()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["read".to_owned()]);
    }

    #[test]
    fn remove_rename_readvertises_the_real_name() {
        let mut r = ToolRegistry::new()
            .with(echo("shell"))
            .rename("shell", "run_command");
        r.remove_rename("shell");
        let names: Vec<String> = r.list_for_provider().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["shell".to_owned()]);
        assert!(r.get("run_command").is_none());
        // Removing an unknown rename does nothing.
        r.remove_rename("ghost");
    }

    #[test]
    fn with_renames_applies_the_config_map() {
        let renames = BTreeMap::from([("shell".to_owned(), "run_command".to_owned())]);
        let r = ToolRegistry::new()
            .with(echo("shell"))
            .with_renames(&renames);
        let names: Vec<String> = r.list_for_provider().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["run_command".to_owned()]);
    }

    /// A dangling alias resolves to itself: the name is not a tool, so
    /// no rules should silently apply to a different one.
    #[test]
    fn canonical_name_leaves_a_dangling_alias_alone() {
        let r = ToolRegistry::new().with(echo("read")).alias("sh", "shell");
        assert_eq!(r.canonical_name("sh"), "sh");
    }

    #[test]
    fn names_iterates_registered_names() {
        let r = ToolRegistry::new()
            .with(echo("a"))
            .with(echo("b"))
            .with(echo("c"));
        let mut names: Vec<&str> = r.names().collect();
        names.sort_unstable();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[test]
    fn execute_runs_through_arc() {
        let r = ToolRegistry::new().with(echo("greet"));
        let workdir = PathBuf::from("/tmp");
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(&workdir, &cancel);
        let tool = r.get("greet").unwrap();
        let out = tool.execute(serde_json::json!({"text":"hi"}), &cx).unwrap();
        assert_eq!(out.text, "hi");
        assert!(!out.is_error);
    }
}
