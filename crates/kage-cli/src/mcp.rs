//! MCP wiring for the binary.
//!
//! - [`run_serve`] is the `kage mcp serve` server: builds a registry of
//!   the requested built-in tools and hands stdin/stdout to
//!   [`kage_mcp::serve()`], gated by the layered `[permissions]`.
//! - [`spawn_and_register`] is the client side every run path calls:
//!   it spawns the configured `[mcp.servers.*]` (merged with any a
//!   plugin declared via `kage.mcp.add_server`) and registers their
//!   tools into the loop's [`ToolRegistry`], keeping the returned
//!   [`McpManager`] alive for the session.
//! - [`deferred_manager`] is the TUI variant: no server spawns until
//!   the engine starts them after the session opens, so the UI is up
//!   before MCP connects.
//! - [`spawn_and_register_with`] also takes the servers an editor passes
//!   over ACP, which count as the user's own configuration.
//! - `kage mcp login` and `logout` live in [`crate::mcp_auth`].
//!
//! Diagnostics for `serve` go to stderr so they do not corrupt the
//! JSON-RPC stream on stdout.

use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use kage_core::config::{Config, McpConfig, McpServer, ShellConfig, ToolsConfig};
use kage_core::permissions::{PermissionAction, PermissionsConfig};
use kage_mcp::{McpError, McpManager};
use kage_plugin::PluginRuntime;
use kage_tools::ToolRegistry;

/// Serve the built-in `tools` as an MCP server until stdin closes.
/// Calls are checked against the layered `[permissions]`; an `ask`
/// verdict is refused because there is no one to ask.
pub(crate) fn run_serve(tools: &[String]) -> ExitCode {
    kage_core::reaper::start();
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = match Config::load_layered(&workdir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kage: mcp serve: {e}");
            return ExitCode::from(1);
        }
    };
    crate::trust::warn_if_untrusted(&workdir);
    let permissions = config.permissions;
    let shell = config.shell;
    if let Err(e) = permissions.validate() {
        eprintln!("kage: mcp serve: {e}");
        return ExitCode::from(1);
    }
    if let Err(e) = shell.validate() {
        eprintln!("kage: mcp serve: {e}");
        return ExitCode::from(1);
    }
    if !permissions.confine_paths {
        eprintln!(
            "kage: path confinement is OFF for this session (permissions.confine_paths = false)"
        );
    }
    let registry = match serve_registry(tools, &shell, &config.tools) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kage: mcp serve: {e}");
            return ExitCode::from(2);
        }
    };
    let gate = |name: &str, input: &serde_json::Value| serve_verdict(&permissions, name, input);
    match kage_mcp::serve(
        &registry,
        &workdir,
        permissions.confine_paths,
        &gate,
        BufReader::new(std::io::stdin()),
        std::io::stdout(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: mcp serve: {e}");
            ExitCode::from(1)
        }
    }
}

/// The built-in registry, as `[tools]` and `[shell]` configure it,
/// reduced to `tools`.
///
/// # Errors
///
/// A message naming the first unknown or disabled tool and the known
/// ones.
fn serve_registry(
    tools: &[String],
    shell: &ShellConfig,
    config: &ToolsConfig,
) -> Result<ToolRegistry, String> {
    let registry = kage_tools::configured_registry(config, shell);
    let wanted: Vec<String> = tools.iter().map(|t| t.trim().to_owned()).collect();
    if let Some(off) = wanted.iter().find(|t| registry.is_disabled(t)) {
        return Err(format!(
            "tool `{off}` in --tools is disabled ([tools.{off}] enabled = false)"
        ));
    }
    // The known set is the full registry, captured before the
    // narrowing: the message exists to tell the user what they could
    // have written.
    let mut known: Vec<&str> = registry.names().collect();
    known.sort_unstable();
    let (registry, missing) = registry.retain_named(&wanted);
    if let Some(unknown) = missing.first() {
        return Err(format!(
            "unknown tool `{unknown}` in --tools (known: {})",
            known.join(", ")
        ));
    }
    Ok(registry)
}

/// The serve gate's answer for one call: `[permissions.tools.<name>]`
/// decides, and `ask` is refused since `kage mcp serve` cannot prompt.
fn serve_verdict(
    permissions: &PermissionsConfig,
    name: &str,
    input: &serde_json::Value,
) -> Option<String> {
    match permissions.check(name, &PermissionsConfig::subject_for(input)) {
        PermissionAction::Allow => None,
        PermissionAction::Ask => Some(format!(
            "`{name}`: permission is `ask` ([permissions.tools.{name}]) and \
             `kage mcp serve` cannot prompt; denied"
        )),
        PermissionAction::Deny => Some(format!(
            "`{name}`: permission denied by [permissions.tools.{name}]"
        )),
    }
}

/// The MCP servers to spawn: `[mcp.servers.*]` from layered config,
/// then any a plugin declared via `kage.mcp.add_server` (a plugin
/// entry overrides a config entry of the same name, matching the
/// "plugins configure, core spawns" model used for ACP agents). A
/// malformed config warns on stderr and degrades to just the
/// plugin-declared set rather than failing the run.
fn merged_config(workdir: &Path, runtime: Option<&PluginRuntime>) -> McpConfig {
    let mut merged = match Config::load_layered(workdir) {
        Ok(c) => c.mcp,
        Err(e) => {
            eprintln!("kage: mcp: {e}; spawning only plugin-declared servers");
            McpConfig::default()
        }
    };
    if let Some(rt) = runtime {
        for (name, server) in rt.registered_mcp_servers() {
            merged.servers.insert(name, server);
        }
    }
    merged
}

/// Spawn every enabled MCP server and register its tools into
/// `tools`. HTTP servers get their stored OAuth tokens from
/// `mcp-auth.json`. The caller must keep the returned [`McpManager`] alive
/// for the session: dropping it kills the child processes. Spawn and
/// discovery failures are returned as `(server, error)` for the
/// caller to surface (never swallowed).
pub(crate) fn spawn_and_register(
    tools: &mut ToolRegistry,
    workdir: &Path,
    runtime: Option<&PluginRuntime>,
) -> (McpManager, Vec<(String, McpError)>) {
    spawn_and_register_with(tools, workdir, runtime, BTreeMap::new())
}

/// [`spawn_and_register`] plus `extra`, the servers an editor passed
/// for this session. They are user level, so no trust prompt applies,
/// and each replaces a configured or plugin server of the same name.
pub(crate) fn spawn_and_register_with(
    tools: &mut ToolRegistry,
    workdir: &Path,
    runtime: Option<&PluginRuntime>,
    extra: BTreeMap<String, McpServer>,
) -> (McpManager, Vec<(String, McpError)>) {
    let mut cfg = merged_config(workdir, runtime);
    merge_extra(&mut cfg, extra);
    let handler = sampling_handler(&cfg);
    let (mut manager, mut errors) = McpManager::spawn_all_with(
        &cfg,
        vec![workdir.to_path_buf()],
        handler,
        crate::mcp_auth::token_source(),
    );
    errors.extend(manager.register_into(tools));
    (manager, errors)
}

/// Build the manager without spawning any server, for the TUI: it
/// opens first and the engine brings each enabled server up on a
/// worker thread once the session is open ([`McpManager::unstarted`]).
/// Server names still gate permissions, and a server that fails to
/// start surfaces as an error notice instead of a startup error.
pub(crate) fn deferred_manager(workdir: &Path, runtime: Option<&PluginRuntime>) -> McpManager {
    let cfg = merged_config(workdir, runtime);
    let handler = sampling_handler(&cfg);
    McpManager::unstarted(
        &cfg,
        vec![workdir.to_path_buf()],
        handler,
        crate::mcp_auth::token_source(),
    )
}

/// Adds `extra` to `cfg`. An entry replaces the server of the same name,
/// and stderr says so.
fn merge_extra(cfg: &mut McpConfig, extra: BTreeMap<String, McpServer>) {
    for (name, server) in extra {
        if cfg.servers.insert(name.clone(), server).is_some() {
            eprintln!("kage: mcp `{name}`: the editor's server replaces the configured one");
        }
    }
}

/// Build the server-request handler when `[mcp] allow_sampling` is set,
/// backed by the host's default model. Returns `None` when sampling is
/// off, so servers never see a `sampling` capability advertised.
fn sampling_handler(cfg: &McpConfig) -> Option<std::sync::Arc<dyn kage_mcp::ServerRequestHandler>> {
    if !cfg.allow_sampling {
        return None;
    }
    let registry = match crate::build_provider_registry() {
        Ok(registry) => registry,
        Err(e) => {
            eprintln!("kage: mcp sampling: {e}");
            return None;
        }
    };
    let model = crate::default_model(&registry);
    Some(std::sync::Arc::new(SamplingHandler { registry, model }))
}

/// Answers `sampling/createMessage` by running the request through the
/// host's default model. Other server requests are declined (`None`).
struct SamplingHandler {
    registry: kage_provider::ProviderRegistry,
    model: String,
}

impl kage_mcp::ServerRequestHandler for SamplingHandler {
    fn capabilities(&self) -> serde_json::Value {
        serde_json::json!({ "sampling": {} })
    }

    fn handle(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Option<Result<serde_json::Value, kage_jsonrpc::RpcError>> {
        if method != "sampling/createMessage" {
            return None;
        }
        Some(self.create_message(params))
    }
}

impl SamplingHandler {
    fn create_message(
        &self,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, kage_jsonrpc::RpcError> {
        use kage_core::{CancelFlag, Content, Message, Role};
        use kage_provider::{ProviderEvent, StreamRequest};

        let mut history = Vec::new();
        if let Some(messages) = params.get("messages").and_then(|m| m.as_array()) {
            for msg in messages {
                let role = match msg.get("role").and_then(serde_json::Value::as_str) {
                    Some("assistant") => Role::Assistant,
                    _ => Role::User,
                };
                let text = msg
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                history.push(Arc::new(Message::new(
                    role,
                    vec![Content::Text { text }],
                    None,
                )));
            }
        }

        let resolved = self
            .registry
            .resolve(&self.model)
            .map_err(|e| kage_jsonrpc::RpcError::internal(format!("sampling: {e}")))?;
        let mut req = StreamRequest::new(resolved.model.clone(), history);
        req.system = params
            .get("systemPrompt")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        req.max_output_tokens = params
            .get("maxTokens")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok());

        let cancel = CancelFlag::new();
        let stream = resolved
            .provider
            .stream(req, &cancel)
            .map_err(|e| kage_jsonrpc::RpcError::internal(format!("sampling stream: {e}")))?;
        let mut text = String::new();
        for event in stream {
            match event {
                Ok(ProviderEvent::TextDelta { delta }) => text.push_str(&delta),
                Ok(ProviderEvent::MessageEnd { .. }) => break,
                Ok(_) => {}
                Err(e) => {
                    return Err(kage_jsonrpc::RpcError::internal(format!("sampling: {e}")));
                }
            }
        }

        Ok(serde_json::json!({
            "role": "assistant",
            "content": { "type": "text", "text": text },
            "model": self.model,
            "stopReason": "endTurn",
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use kage_mcp::ServerRequestHandler;
    use kage_provider::testing::MockProvider;
    use kage_provider::{ProviderEvent, ProviderRegistry, StopReason};

    use kage_core::permissions::{PermissionAction, PermissionsConfig, ToolPermissionRules};

    use kage_core::config::{ShellConfig, ToolsConfig};

    use super::{SamplingHandler, merge_extra, serve_registry, serve_verdict};

    fn names(tools: &[&str]) -> Vec<String> {
        tools.iter().map(|t| (*t).to_owned()).collect()
    }

    fn stdio(command: &str) -> kage_core::config::McpServer {
        kage_core::config::McpServer {
            startup_timeout_secs: None,
            command: Some(command.to_owned()),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            disabled: false,
            oauth: None,
            disabled_tools: Vec::new(),
        }
    }

    #[test]
    fn extra_servers_join_and_win_a_name_clash() {
        let mut cfg = kage_core::config::McpConfig::default();
        cfg.servers.insert("both".into(), stdio("configured"));
        cfg.servers.insert("kept".into(), stdio("kept"));
        let extra = [("both", "editor"), ("new", "new")]
            .map(|(name, command)| (name.to_owned(), stdio(command)))
            .into();
        merge_extra(&mut cfg, extra);
        let commands: Vec<_> = cfg
            .servers
            .iter()
            .map(|(name, s)| (name.as_str(), s.command.as_deref().unwrap()))
            .collect();
        assert_eq!(
            commands,
            [("both", "editor"), ("kept", "kept"), ("new", "new")]
        );
    }

    fn renamed(rename: BTreeMap<String, String>) -> ToolsConfig {
        ToolsConfig {
            rename,
            ..ToolsConfig::default()
        }
    }

    #[test]
    fn serve_registry_refuses_a_disabled_tool_and_applies_limits() {
        let config: ToolsConfig =
            toml::from_str("[grep]\nenabled = false\n[read]\ndescription = \"Read it.\"\n")
                .unwrap();
        let err = serve_registry(&names(&["read", "grep"]), &ShellConfig::default(), &config)
            .unwrap_err();
        assert!(
            err.contains("`grep`") && err.contains("enabled = false"),
            "{err}"
        );
        let reg = serve_registry(&names(&["read"]), &ShellConfig::default(), &config).unwrap();
        assert_eq!(reg.list_for_provider()[0].description, "Read it.");
    }

    #[test]
    fn serve_registry_keeps_only_listed_tools() {
        let reg = serve_registry(
            &names(&["read", "ls"]),
            &ShellConfig::default(),
            &ToolsConfig::default(),
        )
        .unwrap();
        let mut listed: Vec<&str> = reg.names().collect();
        listed.sort_unstable();
        assert_eq!(listed, vec!["ls", "read"]);
        assert!(reg.get("shell").is_none());
    }

    #[test]
    fn serve_registry_applies_renames_to_the_advertised_names() {
        let renames = BTreeMap::from([("shell".to_owned(), "run_command".to_owned())]);
        let reg = serve_registry(
            &names(&["shell"]),
            &ShellConfig::default(),
            &renamed(renames),
        )
        .unwrap();
        let listed: Vec<String> = reg
            .list_for_provider()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(listed, vec!["run_command".to_owned()]);
    }

    #[test]
    fn serve_registry_keeps_a_tool_named_by_its_advertised_name() {
        let renames = BTreeMap::from([("shell".to_owned(), "run_command".to_owned())]);
        let reg = serve_registry(
            &names(&["run_command"]),
            &ShellConfig::default(),
            &renamed(renames),
        )
        .unwrap();
        let listed: Vec<String> = reg
            .list_for_provider()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(listed, vec!["run_command".to_owned()]);
    }

    #[test]
    fn serve_registry_rejects_unknown_tools() {
        let err = serve_registry(
            &names(&["read", "nope"]),
            &ShellConfig::default(),
            &ToolsConfig::default(),
        )
        .unwrap_err();
        assert!(err.contains("`nope`"), "{err}");
        // The known list names the whole registry, not just what the
        // caller asked for and kept.
        assert!(err.contains("grep"), "{err}");
        assert!(err.contains("write"), "{err}");
    }

    #[test]
    fn serve_verdict_refuses_ask_and_deny() {
        let mut permissions = PermissionsConfig::default();
        for (tool, default) in [
            ("shell", PermissionAction::Ask),
            ("write", PermissionAction::Deny),
        ] {
            permissions.tools.insert(
                tool.to_owned(),
                ToolPermissionRules {
                    default,
                    ..ToolPermissionRules::default()
                },
            );
        }
        let input = serde_json::json!({});
        assert!(serve_verdict(&permissions, "read", &input).is_none());
        let ask = serve_verdict(&permissions, "shell", &input).unwrap();
        assert!(ask.contains("cannot prompt"), "{ask}");
        let deny = serve_verdict(&permissions, "write", &input).unwrap();
        assert!(
            deny.contains("denied by [permissions.tools.write]"),
            "{deny}"
        );
    }

    #[test]
    fn sampling_runs_the_prompt_through_the_model() {
        let mock = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "pong".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: kage_core::TokenUsage::default(),
            }),
        ]);
        let registry = ProviderRegistry::new().with(Arc::new(mock));
        let handler = SamplingHandler {
            registry,
            model: "mock:m".to_owned(),
        };

        let params = serde_json::json!({
            "messages": [
                { "role": "user", "content": { "type": "text", "text": "ping" } }
            ],
            "systemPrompt": "be brief",
            "maxTokens": 100,
        });
        let result = handler
            .handle("sampling/createMessage", &params)
            .expect("sampling is handled")
            .expect("sampling succeeds");
        assert_eq!(result["role"], "assistant");
        assert_eq!(result["content"]["text"], "pong");
        assert_eq!(result["model"], "mock:m");
    }

    #[test]
    fn non_sampling_request_is_declined() {
        let registry = ProviderRegistry::new();
        let handler = SamplingHandler {
            registry,
            model: "mock:m".to_owned(),
        };
        assert!(
            handler
                .handle("elicitation/create", &serde_json::Value::Null)
                .is_none()
        );
        assert!(handler.capabilities()["sampling"].is_object());
    }
}
