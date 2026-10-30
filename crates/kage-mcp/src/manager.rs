//! Own every configured MCP server and keep its tools registered.
//!
//! [`McpManager::spawn_all`] launches each enabled `[mcp.servers.*]`
//! entry, [`McpManager::register_into`] discovers and registers their
//! tools, and [`McpManager::refresh_into`] re-lists any server that
//! pushed a `notifications/tools/list_changed`, swapping its adapters
//! in place (stale tools are unregistered, not left dangling).
//!
//! The manager also caches each server's resources, resource templates
//! and prompts, listed only when the server advertises the capability
//! and reloaded when it announces a change, so [`McpManager::catalog`]
//! never touches the network.
//!
//! [`McpManager::refresh_into`] also notices a server whose transport
//! has died: it unregisters that server's tools, evicts the handle,
//! and reports a [`McpError::Crashed`] failure, while the launch spec
//! is retained so [`McpManager::restart`] can respawn it later.
//!
//! A server that fails to spawn or list tools does not abort the
//! agent: the failure is collected and returned to the caller to
//! surface, while the rest of the servers continue. A server that
//! failed to spawn is kept like an evicted one, so `restart` can bring
//! it up later and permission gates still know its name. The manager
//! owns the [`McpServerHandle`]s, so dropping it kills every child.
//!
//! An HTTP server that refuses kage's token (or has none) reports
//! [`McpError::Unauthorized`]. It is kept like a failed server but shows
//! as [`McpServerStatus::NeedsAuth`], and a live server that starts
//! refusing is taken down the same way: at the next refresh after any
//! request was refused (a tool call included), at a refresh once its
//! stored token is gone (a logout), or when a restart is refused.
//! [`McpManager::spawn_all_with`] takes the [`TokenSource`] and keeps it
//! for `restart`.
//!
//! While at least one live server advertises resources, the manager also
//! registers the [`McpResourceTool`] over the cached lists, rebuilding it
//! whenever it registers, refreshes or restarts, and unregisters it once
//! no such server is left.

use std::sync::Arc;
use std::time::Duration;

use kage_core::config::{McpConfig, McpServer};
use kage_core::protocol::{
    McpPrompt, McpResource, McpResourceTemplate, McpServerInfo, McpServerStatus,
};
use kage_tools::ToolRegistry;

use crate::oauth::TokenSource;
use crate::resource_tool::{McpResourceTool, RESOURCE_TOOL, ResourceServer};
use crate::server::{INITIALIZE_TIMEOUT, McpConnection, McpError, McpServerHandle};
use crate::tools::tools_from_connection;

/// One configured server: its launch spec (kept so it can be
/// respawned by `restart`, including after an eviction), the live
/// handle (`None` when it failed to spawn or was evicted as dead), the
/// last error of a server that is not live and whether that error asks
/// for a login, whether the live handle signs in with a stored token,
/// the tool names it currently contributes, and its cached catalog
/// lists.
struct Managed {
    spec: McpServer,
    handle: Option<McpServerHandle>,
    /// Waiting for its first spawn. Only [`McpManager::unstarted`] sets
    /// this; the host brings such a server up with a `restart`.
    starting: bool,
    error: Option<String>,
    needs_auth: bool,
    signed_in: bool,
    registered: Vec<String>,
    resources: Vec<McpResource>,
    templates: Vec<McpResourceTemplate>,
    prompts: Vec<McpPrompt>,
}

impl Managed {
    fn new(spec: McpServer, handle: Option<McpServerHandle>) -> Self {
        Self {
            spec,
            handle,
            starting: false,
            error: None,
            needs_auth: false,
            signed_in: false,
            registered: Vec::new(),
            resources: Vec::new(),
            templates: Vec::new(),
            prompts: Vec::new(),
        }
    }

    /// A server configured but not yet spawned for the first time.
    fn unstarted(spec: McpServer) -> Self {
        let mut managed = Self::new(spec, None);
        managed.starting = true;
        managed
    }

    fn connection(&self) -> Option<Arc<McpConnection>> {
        self.handle.as_ref().map(|h| Arc::clone(h.connection()))
    }

    /// Reload the cached resources and templates. A failure keeps the
    /// previous lists.
    fn load_resources(&mut self) -> Result<(), McpError> {
        let Some(conn) = self.connection() else {
            return Ok(());
        };
        let resources = conn.list_resources()?;
        self.templates = conn.list_resource_templates()?;
        self.resources = resources;
        Ok(())
    }

    /// Reload the cached prompts. A failure keeps the previous list.
    fn load_prompts(&mut self) -> Result<(), McpError> {
        let Some(conn) = self.connection() else {
            return Ok(());
        };
        self.prompts = conn.list_prompts()?;
        Ok(())
    }

    /// Reload tools, resources and prompts, returning every failure. A
    /// failing catalog list leaves the tools registered.
    fn load_all(&mut self, reg: &mut ToolRegistry) -> Vec<McpError> {
        [
            McpManager::reload(self, reg),
            self.load_resources(),
            self.load_prompts(),
        ]
        .into_iter()
        .filter_map(Result::err)
        .collect()
    }

    /// Forget the cached catalog of a server that is no longer live or
    /// is about to be replaced.
    fn clear_catalog(&mut self) {
        self.resources.clear();
        self.templates.clear();
        self.prompts.clear();
    }

    /// Record why the server is not live.
    fn failed(&mut self, error: &McpError) {
        self.error = Some(error.to_string());
        self.needs_auth = matches!(error, McpError::Unauthorized { .. });
    }

    /// Take the server down: unregister its tools, drop the handle and
    /// the catalog, and record `error`.
    fn evict(&mut self, reg: &mut ToolRegistry, error: &McpError) {
        for stale in self.registered.drain(..) {
            reg.unregister(&stale);
        }
        self.handle = None;
        self.signed_in = false;
        self.clear_catalog();
        self.failed(error);
    }

    /// The URL this server's HTTP transport asks `tokens` a bearer for:
    /// `None` for a stdio server, without a token source, or when a
    /// configured `authorization` header wins.
    fn token_url(&self, tokens: Option<&Arc<dyn TokenSource>>) -> Option<&str> {
        let configured = self
            .spec
            .headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("authorization"));
        tokens.and(self.spec.url.as_deref()).filter(|_| !configured)
    }

    /// Whether `tokens` has a token for this server's HTTP transport.
    fn has_token(&self, tokens: Option<&Arc<dyn TokenSource>>) -> bool {
        match (self.token_url(tokens), tokens) {
            (Some(url), Some(tokens)) => tokens.bearer(url).is_some(),
            _ => false,
        }
    }

    /// Take the server down when one of `failures` says its token was
    /// refused, and hand the failures back.
    fn settle(&mut self, reg: &mut ToolRegistry, failures: Vec<McpError>) -> Vec<McpError> {
        if let Some(denied) = failures
            .iter()
            .find(|e| matches!(e, McpError::Unauthorized { .. }))
        {
            self.evict(reg, denied);
        }
        failures
    }

    fn info(&self, name: &str) -> McpServerInfo {
        let status = if self.handle.is_some() {
            McpServerStatus::Connected
        } else if self.starting {
            McpServerStatus::Starting
        } else if self.needs_auth {
            McpServerStatus::NeedsAuth
        } else {
            McpServerStatus::Failed {
                error: self.error.clone().unwrap_or_default(),
            }
        };
        McpServerInfo {
            name: name.to_owned(),
            status,
            tools: u32::try_from(self.registered.len()).unwrap_or(u32::MAX),
            resources: self.resources.clone(),
            templates: self.templates.clone(),
            prompts: self.prompts.clone(),
        }
    }
}

/// Owns the spawned MCP servers and mediates their tools into a
/// [`ToolRegistry`].
#[derive(Default)]
pub struct McpManager {
    servers: Vec<(String, Managed)>,
    /// Filesystem roots advertised to every server (the host workdir),
    /// retained so a `restart` re-advertises the same roots.
    roots: Vec<std::path::PathBuf>,
    /// Host handler for server-initiated requests (sampling, ...),
    /// retained so a `restart` re-injects it.
    handler: Option<Arc<dyn crate::ServerRequestHandler>>,
    /// Bearer tokens for HTTP servers, retained so a `restart` (for
    /// example after a login) sends them.
    tokens: Option<Arc<dyn TokenSource>>,
    /// Whether the manager registered [`McpResourceTool`], so it never
    /// unregisters a tool of the same name that it did not add.
    resource_tool: bool,
}

impl McpManager {
    /// The child process id of server `name`, for diagnostics.
    /// `None` when the server is not running or is not a spawned
    /// subprocess (an HTTP server has no child).
    #[must_use]
    pub fn server_pid(&self, name: &str) -> Option<u32> {
        let managed = &self.servers.iter().find(|(n, _)| n == name)?.1;
        managed.handle.as_ref()?.child_pid()
    }

    /// Spawn every enabled server in `cfg` (sorted by name for a
    /// deterministic registration order). Disabled entries are
    /// skipped. Spawn/handshake failures are collected as
    /// `(server_name, error)` and returned alongside the manager so
    /// the caller can surface them without losing the servers that
    /// did come up. A failed server stays in the manager without a
    /// handle, so [`Self::restart`] can retry it. `roots` are the
    /// filesystem roots advertised to every server (typically the host
    /// workdir); `handler` answers server-initiated requests such as
    /// sampling.
    #[must_use]
    pub fn spawn_all(
        cfg: &McpConfig,
        roots: Vec<std::path::PathBuf>,
        handler: Option<Arc<dyn crate::ServerRequestHandler>>,
    ) -> (Self, Vec<(String, McpError)>) {
        Self::spawn_all_with(cfg, roots, handler, None)
    }

    /// [`Self::spawn_all`] with a bearer token source for HTTP servers,
    /// kept for [`Self::restart`]. A server that refuses kage's token
    /// stays in the manager as [`McpServerStatus::NeedsAuth`].
    #[must_use]
    pub fn spawn_all_with(
        cfg: &McpConfig,
        roots: Vec<std::path::PathBuf>,
        handler: Option<Arc<dyn crate::ServerRequestHandler>>,
        tokens: Option<Arc<dyn TokenSource>>,
    ) -> (Self, Vec<(String, McpError)>) {
        let mut servers = Vec::new();
        let mut errors = Vec::new();
        for (name, spec) in &cfg.servers {
            if spec.disabled {
                continue;
            }
            let timeout = spec
                .startup_timeout_secs
                .map_or(INITIALIZE_TIMEOUT, Duration::from_secs);
            let spawned = McpServerHandle::spawn_with_startup_timeout(
                name.clone(),
                spec,
                &roots,
                handler.clone(),
                tokens.clone(),
                timeout,
            );
            let managed = match spawned {
                Ok(handle) => {
                    let mut managed = Managed::new(spec.clone(), Some(handle));
                    managed.signed_in = managed.has_token(tokens.as_ref());
                    managed
                }
                Err(e) => {
                    let mut managed = Managed::new(spec.clone(), None);
                    managed.failed(&e);
                    errors.push((name.clone(), e));
                    managed
                }
            };
            servers.push((name.clone(), managed));
        }
        (
            Self {
                servers,
                roots,
                handler,
                tokens,
                resource_tool: false,
            },
            errors,
        )
    }

    /// Every enabled server in `cfg`, configured but not spawned: no
    /// process runs and no tool registers until each server is brought
    /// up with [`Self::restart`]. For hosts that show a UI first and
    /// connect MCP afterwards: the servers report
    /// [`McpServerStatus::Starting`] and [`Self::server_names`] still
    /// lists them for the permission gate. `roots`, `handler` and
    /// `tokens` are kept for those restarts, exactly as in
    /// [`Self::spawn_all_with`].
    #[must_use]
    pub fn unstarted(
        cfg: &McpConfig,
        roots: Vec<std::path::PathBuf>,
        handler: Option<Arc<dyn crate::ServerRequestHandler>>,
        tokens: Option<Arc<dyn TokenSource>>,
    ) -> Self {
        let servers = cfg
            .servers
            .iter()
            .filter(|(_, spec)| !spec.disabled)
            .map(|(name, spec)| {
                let mut managed = Managed::unstarted(spec.clone());
                managed.signed_in = managed.has_token(tokens.as_ref());
                (name.clone(), managed)
            })
            .collect();
        Self {
            servers,
            roots,
            handler,
            tokens,
            resource_tool: false,
        }
    }

    /// Names of the servers still waiting for their first spawn.
    pub fn starting_names(&self) -> impl Iterator<Item = &str> {
        self.servers
            .iter()
            .filter(|(_, m)| m.starting)
            .map(|(n, _)| n.as_str())
    }

    /// Whether no server is live (failed and evicted ones do not count).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of live servers (failed and evicted ones do not count).
    #[must_use]
    pub fn len(&self) -> usize {
        self.servers
            .iter()
            .filter(|(_, m)| m.handle.is_some())
            .count()
    }

    /// Names of the servers whose tools must be gated: every
    /// configured, enabled server, live, failed or evicted, in
    /// registration order. A server that comes up later through
    /// [`Self::restart`] is therefore already known to a gate built
    /// from this list.
    pub fn server_names(&self) -> impl Iterator<Item = &str> {
        self.servers.iter().map(|(n, _)| n.as_str())
    }

    /// The spawn or crash error of a server that is not live, or `None`
    /// for a live or unknown server.
    #[must_use]
    pub fn error(&self, name: &str) -> Option<&str> {
        self.servers
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, m)| m.error.as_deref())
    }

    /// Every configured, enabled server with its status, tool count and
    /// cached resources, templates and prompts, in registration order. A
    /// server that is not live reports its error and empty lists. Never
    /// touches the network.
    #[must_use]
    pub fn catalog(&self) -> Vec<McpServerInfo> {
        self.servers.iter().map(|(n, m)| m.info(n)).collect()
    }

    /// The live connections, by server name, for the calls that read
    /// resources and fetch prompts.
    #[must_use]
    pub fn clients(&self) -> Vec<(String, Arc<McpConnection>)> {
        self.servers
            .iter()
            .filter_map(|(n, m)| Some((n.clone(), m.connection()?)))
            .collect()
    }

    /// Manage an already initialized connection as server `name`, for
    /// hosts and tests that run a server in process. The server has no
    /// launch spec, so [`Self::restart`] reports a config error for it.
    /// Call [`Self::register_into`] afterwards to list its tools and
    /// catalog.
    pub fn adopt(&mut self, name: &str, conn: Arc<McpConnection>) {
        let spec = McpServer {
            startup_timeout_secs: None,
            command: None,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            disabled: false,
            oauth: None,
            disabled_tools: Vec::new(),
        };
        let handle = McpServerHandle::from_connection(conn);
        self.servers
            .push((name.to_owned(), Managed::new(spec, Some(handle))));
    }

    /// Discover and register every live server's tools, and list the
    /// resources, templates and prompts of servers that advertise them.
    /// Returns the per-server failures; a failing tool list simply
    /// contributes no tools, and a failing catalog list leaves the
    /// tools registered. A server that refuses kage's token is taken
    /// down and needs a login.
    pub fn register_into(&mut self, reg: &mut ToolRegistry) -> Vec<(String, McpError)> {
        let mut errors = Vec::new();
        for (name, managed) in &mut self.servers {
            if managed.handle.is_none() {
                continue;
            }
            let failures = managed.load_all(reg);
            let failures = managed.settle(reg, failures);
            errors.extend(failures.into_iter().map(|e| (name.clone(), e)));
        }
        self.sync_resource_tool(reg);
        errors
    }

    /// Re-list only the lists each server announced a change for since
    /// the last call: tools (swapping their adapters in place),
    /// resources with their templates, or prompts. A server whose
    /// transport has died is evicted instead: its tools are
    /// unregistered, its catalog is cleared and a [`McpError::Crashed`]
    /// failure is reported (the launch spec is kept for a later
    /// `restart`). A server that refused kage's token on any request, or
    /// whose stored token is gone, is taken down the same way and needs a
    /// login. Returns per-server failures.
    pub fn refresh_into(&mut self, reg: &mut ToolRegistry) -> Vec<(String, McpError)> {
        let mut errors = Vec::new();
        for (name, managed) in &mut self.servers {
            let Some(handle) = managed.handle.as_ref() else {
                continue;
            };
            let conn = Arc::clone(handle.connection());
            if conn.is_dead() {
                let detail = managed
                    .handle
                    .as_mut()
                    .and_then(McpServerHandle::exit_status)
                    .unwrap_or_else(|| "connection closed".to_owned());
                let crash = McpError::Crashed {
                    server: name.clone(),
                    detail,
                };
                managed.evict(reg, &crash);
                errors.push((name.clone(), crash));
                continue;
            }
            let tokens = self.tokens.as_ref();
            if conn.is_refused() || (managed.signed_in && !managed.has_token(tokens)) {
                let denied = McpError::Unauthorized {
                    server: name.clone(),
                    login: managed.token_url(tokens).is_some(),
                };
                managed.evict(reg, &denied);
                errors.push((name.clone(), denied));
                continue;
            }
            let mut failures = Vec::new();
            if conn.take_tools_changed()
                && let Err(e) = Self::reload(managed, reg)
            {
                conn.set_tools_changed(true);
                failures.push(e);
            }
            if conn.take_resources_changed()
                && let Err(e) = managed.load_resources()
            {
                conn.set_resources_changed(true);
                failures.push(e);
            }
            if conn.take_prompts_changed()
                && let Err(e) = managed.load_prompts()
            {
                conn.set_prompts_changed(true);
                failures.push(e);
            }
            let failures = managed.settle(reg, failures);
            errors.extend(failures.into_iter().map(|e| (name.clone(), e)));
        }
        self.sync_resource_tool(reg);
        errors
    }

    /// Restart one server by name: spawn a fresh process from its
    /// original spec, discover its tools against the fresh connection,
    /// and only on success swap it in (killing the old child, if any),
    /// register the tools and reload the catalog. A failed discovery
    /// kills the fresh child and leaves the previous server live with
    /// its tools registered, so `restart` never causes downtime on its
    /// own failure. The name is looked up across every entry, including
    /// servers that failed to spawn or were evicted as dead, so
    /// `restart` can bring them up from the retained spec. The exception
    /// is a refused token: the live server is taken down too, because
    /// its requests would be refused as well.
    ///
    /// # Errors
    ///
    /// [`McpError::Unknown`] if no server has that name, the spawn
    /// error, or the discovery error from bringing the replacement up
    /// (the previous server stays live).
    pub fn restart(&mut self, name: &str, reg: &mut ToolRegistry) -> Result<(), McpError> {
        let roots = self.roots.clone();
        let handler = self.handler.clone();
        let tokens = self.tokens.clone();
        let managed = self
            .servers
            .iter_mut()
            .find(|(n, _)| n == name)
            .map(|(_, m)| m)
            .ok_or_else(|| McpError::Unknown(name.to_owned()))?;
        managed.starting = false;
        let timeout = managed
            .spec
            .startup_timeout_secs
            .map_or(INITIALIZE_TIMEOUT, Duration::from_secs);
        let spawned = McpServerHandle::spawn_with_startup_timeout(
            name.to_owned(),
            &managed.spec,
            &roots,
            handler,
            tokens,
            timeout,
        );
        let fresh = match spawned {
            Ok(fresh) => fresh,
            Err(e) => {
                if matches!(e, McpError::Unauthorized { .. }) {
                    managed.evict(reg, &e);
                    self.sync_resource_tool(reg);
                } else if managed.handle.is_none() {
                    managed.failed(&e);
                }
                return Err(e);
            }
        };
        let fresh_conn = Arc::clone(fresh.connection());
        let tools = match tools_from_connection(&fresh_conn) {
            Ok(tools) => tools,
            Err(e) => {
                drop(fresh);
                if matches!(e, McpError::Unauthorized { .. }) {
                    managed.evict(reg, &e);
                    self.sync_resource_tool(reg);
                }
                return Err(e);
            }
        };
        for stale in managed.registered.drain(..) {
            reg.unregister(&stale);
        }
        managed.handle = Some(fresh);
        managed.signed_in = managed.has_token(self.tokens.as_ref());
        managed.error = None;
        managed.needs_auth = false;
        managed.clear_catalog();
        let disabled = managed.spec.disabled_tools.clone();
        register_tools(&tools, &fresh_conn, &disabled, &mut managed.registered, reg);
        let failures: Vec<McpError> = [managed.load_resources(), managed.load_prompts()]
            .into_iter()
            .filter_map(Result::err)
            .collect();
        let failures = managed.settle(reg, failures);
        self.sync_resource_tool(reg);
        failures.into_iter().next().map_or(Ok(()), Err)
    }

    /// Register [`McpResourceTool`] over the live servers that advertise
    /// resources, or unregister it when there are none.
    fn sync_resource_tool(&mut self, reg: &mut ToolRegistry) {
        let servers: Vec<ResourceServer> = self
            .servers
            .iter()
            .filter_map(|(name, managed)| {
                let conn = managed.connection()?;
                conn.has("resources").then(|| ResourceServer {
                    name: name.clone(),
                    conn,
                    resources: managed.resources.clone(),
                    templates: managed.templates.clone(),
                })
            })
            .collect();
        if servers.is_empty() {
            if std::mem::take(&mut self.resource_tool) {
                reg.unregister(RESOURCE_TOOL);
            }
        } else {
            reg.register(Arc::new(McpResourceTool::new(servers)));
            self.resource_tool = true;
        }
    }

    /// Drop this server's previously registered tools and register
    /// its current set, updating the tracked names. The caller must
    /// ensure the handle is live.
    fn reload(managed: &mut Managed, reg: &mut ToolRegistry) -> Result<(), McpError> {
        let conn = Arc::clone(
            managed
                .handle
                .as_ref()
                .expect("reload called on a live server")
                .connection(),
        );
        let disabled = managed.spec.disabled_tools.clone();
        reload_connection(&conn, &disabled, &mut managed.registered, reg)
    }
}

/// Connection-level reload: list `conn`'s tools, unregister the names
/// in `registered`, register the current set but the `disabled` ones,
/// and update `registered` to match. Factored out of
/// [`McpManager::reload`] so it can be exercised without spawning a
/// process.
fn reload_connection(
    conn: &Arc<McpConnection>,
    disabled: &[String],
    registered: &mut Vec<String>,
    reg: &mut ToolRegistry,
) -> Result<(), McpError> {
    let tools = tools_from_connection(conn)?;
    for stale in registered.drain(..) {
        reg.unregister(&stale);
    }
    register_tools(&tools, conn, disabled, registered, reg);
    Ok(())
}

/// Register discovered tools, skipping the `disabled` ones, and track
/// the names the registry actually took.
///
/// `[tools.<server>__<tool>] enabled = false` makes `register` drop a
/// tool, so a name is only tracked once the registry holds it. Tracking
/// a dropped one would make the next reload unregister a tool that was
/// never there and leave the manager believing a disabled tool is live.
fn register_tools(
    tools: &[Arc<dyn kage_tools::Tool>],
    conn: &Arc<McpConnection>,
    disabled: &[String],
    registered: &mut Vec<String>,
    reg: &mut ToolRegistry,
) {
    for tool in tools {
        let name = tool.name();
        if disabled
            .iter()
            .any(|dropped| name == format!("{}__{dropped}", conn.name()))
        {
            continue;
        }
        // `register` records the name even when the overrides drop it,
        // so a `[tools.<server>__<tool>]` table is not later reported as
        // naming no tool. Only a name the registry holds is tracked.
        reg.register(Arc::clone(tool));
        if reg.get(name).is_some() {
            registered.push(name.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

    use super::*;
    use crate::server::PROTOCOL_VERSION;
    use crate::test_support::{FakeServer, Reply, Seen, StaticTokens, scripted, serve, wait_until};
    use kage_core::config::McpServer;
    use kage_jsonrpc::Inbound;

    /// A server whose `tools/list` returns `old` on the first call
    /// and `new` on every call after, so a reload must swap them.
    fn flipping_server() -> Arc<McpConnection> {
        let calls = AtomicUsize::new(0);
        let (conn, _srv, _seen) =
            scripted("x", serde_json::json!({}), move |method, _| match method {
                "tools/list" => {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    let tool = if n == 0 { "old" } else { "new" };
                    Ok(serde_json::json!({
                        "tools": [{ "name": tool, "inputSchema": {} }]
                    }))
                }
                other => Err(kage_jsonrpc::RpcError::method_not_found(other)),
            });
        conn
    }

    #[test]
    fn reload_swaps_stale_tools_for_current_ones() {
        let conn = flipping_server();
        let mut reg = ToolRegistry::new();
        let mut registered = Vec::new();

        reload_connection(&conn, &[], &mut registered, &mut reg).unwrap();
        assert_eq!(registered, ["x__old"]);
        assert!(reg.get("x__old").is_some());

        reload_connection(&conn, &[], &mut registered, &mut reg).unwrap();
        assert_eq!(registered, ["x__new"]);
        assert!(reg.get("x__new").is_some());
        assert!(reg.get("x__old").is_none(), "stale tool was unregistered");
    }

    #[test]
    fn a_disabled_tool_is_never_registered() {
        let conn = flipping_server();
        let mut reg = ToolRegistry::new();
        let mut registered = Vec::new();
        reload_connection(&conn, &["old".to_owned()], &mut registered, &mut reg).unwrap();
        assert!(registered.is_empty());
        assert!(reg.get("x__old").is_none());
    }

    /// `[tools.<server>__<tool>] enabled = false` drops the tool inside
    /// `register`, so the manager must not track it as live: the next
    /// reload would unregister a tool that was never there.
    #[test]
    fn a_tool_the_tool_table_disables_is_not_tracked() {
        let conn = flipping_server();
        let mut overrides = kage_tools::ToolOverrides::default();
        overrides.disabled.insert("x__old".to_owned());
        let mut reg = ToolRegistry::new().with_overrides(overrides);
        let mut registered = Vec::new();
        // The first listing offers only `old`, which the table disables.
        reload_connection(&conn, &[], &mut registered, &mut reg).unwrap();
        assert!(registered.is_empty(), "a dropped tool is not tracked");
        assert!(reg.get("x__old").is_none());
        // The name was still seen, so the table is not reported as a typo.
        assert!(reg.was_registered("x__old"));
        // The second listing offers `new`, which is tracked.
        reload_connection(&conn, &[], &mut registered, &mut reg).unwrap();
        assert_eq!(registered, vec!["x__new".to_owned()]);
        assert!(reg.get("x__new").is_some());
    }

    #[test]
    fn spawn_all_skips_disabled_and_reports_spawn_failures() {
        use kage_core::config::McpServer;

        let mut cfg = McpConfig::default();
        cfg.servers.insert(
            "off".to_owned(),
            McpServer {
                startup_timeout_secs: None,
                command: Some("definitely-not-a-real-binary-xyz".to_owned()),
                args: vec![],
                env: std::collections::BTreeMap::new(),
                url: None,
                headers: std::collections::BTreeMap::new(),
                disabled: true,
                oauth: None,
                disabled_tools: Vec::new(),
            },
        );
        cfg.servers.insert(
            "broken".to_owned(),
            McpServer {
                startup_timeout_secs: None,
                command: Some("definitely-not-a-real-binary-xyz".to_owned()),
                args: vec![],
                env: std::collections::BTreeMap::new(),
                url: None,
                headers: std::collections::BTreeMap::new(),
                disabled: false,
                oauth: None,
                disabled_tools: Vec::new(),
            },
        );
        let (mgr, errors) = McpManager::spawn_all(&cfg, vec![], None);
        assert!(mgr.is_empty(), "disabled skipped, broken failed to spawn");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "broken");
    }

    #[test]
    fn a_failed_spawn_is_kept_for_gates_and_restart() {
        let mut cfg = McpConfig::default();
        let spec = McpServer {
            startup_timeout_secs: None,
            command: Some("definitely-not-a-real-binary-xyz".to_owned()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            disabled: false,
            oauth: None,
            disabled_tools: Vec::new(),
        };
        cfg.servers.insert("broken".to_owned(), spec.clone());
        cfg.servers.insert(
            "off".to_owned(),
            McpServer {
                startup_timeout_secs: None,
                disabled: true,
                ..spec
            },
        );
        let (mut mgr, errors) = McpManager::spawn_all(&cfg, vec![], None);
        assert_eq!(errors.len(), 1);
        assert_eq!(mgr.server_names().collect::<Vec<_>>(), ["broken"]);
        assert_eq!(mgr.len(), 0);
        assert!(mgr.is_empty());
        let spawn_error = mgr.error("broken").expect("the spawn error is kept");
        assert!(spawn_error.contains("definitely-not-a-real-binary-xyz"));

        let mut reg = ToolRegistry::new();
        let err = mgr.restart("broken", &mut reg).unwrap_err();
        assert!(
            matches!(&err, McpError::Spawn { command, .. } if command == "definitely-not-a-real-binary-xyz"),
            "restart must retry the kept spec: {err}"
        );
        assert!(mgr.error("off").is_none());
    }

    #[test]
    fn restart_unknown_server_errors() {
        let (mut mgr, _e) = McpManager::spawn_all(&McpConfig::default(), vec![], None);
        let mut reg = ToolRegistry::new();
        let err = mgr.restart("ghost", &mut reg).unwrap_err();
        assert!(
            matches!(&err, crate::server::McpError::Unknown(n) if n == "ghost"),
            "{err}"
        );
    }

    /// An HTTP server that answers `initialize` and, unless it is the
    /// refused method, nothing else. `tools_status` picks the reply to
    /// `tools/list`: `None` for a JSON-RPC error, `Some(status)` for a
    /// raw HTTP status.
    fn discovery_server(tools_status: Option<u16>) -> FakeServer {
        serve(move |request, _| {
            if request.method == "GET" {
                return Reply::status(405);
            }
            let body: serde_json::Value = serde_json::from_str(&request.body).unwrap_or_default();
            match body["method"].as_str() {
                Some("initialize") => Reply::json(
                    200,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": body["id"],
                        "result": { "protocolVersion": PROTOCOL_VERSION, "capabilities": {} },
                    }),
                ),
                Some("tools/list") => match tools_status {
                    Some(status) => Reply::status(status),
                    None => Reply::json(
                        200,
                        &serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": body["id"],
                            "error": { "code": -32000, "message": "boom" },
                        }),
                    ),
                },
                _ => Reply::status(202),
            }
        })
    }

    fn live_server_manager() -> (McpManager, ToolRegistry, Arc<McpConnection>) {
        let (conn, _srv, _seen) = scripted(
            "srv",
            serde_json::json!({ "tools": {} }),
            |method, _| match method {
                "tools/list" => {
                    Ok(serde_json::json!({ "tools": [{ "name": "t", "inputSchema": {} }] }))
                }
                other => Err(kage_jsonrpc::RpcError::method_not_found(other)),
            },
        );
        let mut mgr = McpManager::default();
        mgr.adopt("srv", Arc::clone(&conn));
        let mut reg = ToolRegistry::new();
        assert!(mgr.register_into(&mut reg).is_empty());
        assert!(reg.get("srv__t").is_some());
        (mgr, reg, conn)
    }

    #[test]
    fn a_failed_restart_discovery_leaves_the_old_server_live() {
        let (mut mgr, mut reg, conn) = live_server_manager();
        let broken = discovery_server(None);
        mgr.servers[0].1.spec.url = Some(format!("{}/mcp", broken.base));

        let err = mgr.restart("srv", &mut reg).unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        assert!(reg.get("srv__t").is_some(), "old tools stay registered");
        assert!(!conn.is_dead(), "the old connection stays live");
        assert_eq!(mgr.len(), 1);
        assert_eq!(mgr.catalog()[0].status, McpServerStatus::Connected);
        assert_eq!(mgr.catalog()[0].tools, 1);
        assert!(mgr.error("srv").is_none());
    }

    #[test]
    fn a_restart_discovery_refusal_takes_the_server_down() {
        let (mut mgr, mut reg, _conn) = live_server_manager();
        let refused = discovery_server(Some(401));
        mgr.servers[0].1.spec.url = Some(format!("{}/mcp", refused.base));

        let err = mgr.restart("srv", &mut reg).unwrap_err();
        assert!(matches!(err, McpError::Unauthorized { .. }), "{err:?}");
        assert!(reg.get("srv__t").is_none(), "evicted as refused");
        assert_eq!(mgr.catalog()[0].status, McpServerStatus::NeedsAuth);
        assert!(mgr.is_empty());
    }

    #[test]
    fn a_failed_reload_keeps_the_change_notice_pending() {
        let fail = Arc::new(AtomicBool::new(false));
        let fail_in_script = Arc::clone(&fail);
        let calls = Arc::new(AtomicUsize::new(0));
        let (conn, srv, _seen) = scripted(
            "x",
            serde_json::json!({ "tools": {} }),
            move |method, _| match method {
                "tools/list" if fail_in_script.load(Ordering::SeqCst) => {
                    Err(kage_jsonrpc::RpcError::internal("boom"))
                }
                "tools/list" => {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    Ok(serde_json::json!({
                        "tools": [{
                            "name": if n == 0 { "old" } else { "new" },
                            "inputSchema": {},
                        }]
                    }))
                }
                other => Err(kage_jsonrpc::RpcError::method_not_found(other)),
            },
        );
        let mut mgr = McpManager::default();
        mgr.adopt("x", conn);
        let mut reg = ToolRegistry::new();
        assert!(mgr.register_into(&mut reg).is_empty());
        assert!(reg.get("x__old").is_some());

        fail.store(true, Ordering::SeqCst);
        srv.notify("notifications/tools/list_changed", serde_json::json!({}))
            .unwrap();
        srv.request("ping", serde_json::json!({}))
            .expect("the drain thread handled the notice before the ping");
        let errors = mgr.refresh_into(&mut reg);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(reg.get("x__old").is_some(), "reload failed, old tools stay");

        fail.store(false, Ordering::SeqCst);
        assert!(mgr.refresh_into(&mut reg).is_empty());
        assert!(reg.get("x__new").is_some(), "the notice was retried");
        assert!(reg.get("x__old").is_none());
    }

    #[test]
    fn unstarted_servers_report_starting_and_start_on_their_first_restart() {
        let ready = Arc::new(AtomicBool::new(true));
        let server = guarded_server(Arc::clone(&ready), None);
        let mut cfg = remote_config(format!("{}/mcp", server.base));
        cfg.servers.insert(
            "broken".to_owned(),
            McpServer {
                startup_timeout_secs: None,
                command: Some("definitely-not-a-real-binary-xyz".to_owned()),
                args: vec![],
                env: std::collections::BTreeMap::new(),
                url: None,
                headers: std::collections::BTreeMap::new(),
                disabled: false,
                oauth: None,
                disabled_tools: Vec::new(),
            },
        );
        cfg.servers.insert(
            "off".to_owned(),
            McpServer {
                startup_timeout_secs: None,
                disabled: true,
                ..cfg.servers["broken"].clone()
            },
        );
        let tokens = StaticTokens::new("good", None);
        let mut mgr = McpManager::unstarted(
            &cfg,
            vec![],
            None,
            Some(Arc::clone(&tokens) as Arc<dyn TokenSource>),
        );
        assert_eq!(mgr.len(), 0);
        assert_eq!(mgr.server_names().collect::<Vec<_>>(), ["broken", "remote"]);
        assert_eq!(
            mgr.starting_names().collect::<Vec<_>>(),
            ["broken", "remote"]
        );
        assert!(
            mgr.catalog()
                .iter()
                .all(|s| s.status == McpServerStatus::Starting),
            "{:?}",
            mgr.catalog()
        );

        let mut reg = ToolRegistry::new();
        mgr.restart("remote", &mut reg).unwrap();
        let mut catalog = mgr.catalog();
        let remote = catalog.iter_mut().find(|s| s.name == "remote").unwrap();
        assert_eq!(remote.status, McpServerStatus::Connected);
        assert!(reg.get("remote__t").is_some());

        let err = mgr.restart("broken", &mut reg).unwrap_err();
        assert!(
            matches!(&err, McpError::Spawn { command, .. } if command == "definitely-not-a-real-binary-xyz"),
            "{err}"
        );
        let mut catalog = mgr.catalog();
        let broken = catalog.iter_mut().find(|s| s.name == "broken").unwrap();
        assert!(matches!(&broken.status, McpServerStatus::Failed { .. }));
        assert_eq!(mgr.starting_names().collect::<Vec<_>>(), Vec::<&str>::new());
    }

    /// A server that advertises `capabilities`, answers `initialize`,
    /// `tools/list` (one tool named `t`) and the resource lists (one
    /// resource), and exits once `kill` is set, dropping the transport so
    /// the client sees EOF.
    fn killable_server(
        kill: Arc<AtomicBool>,
        capabilities: serde_json::Value,
    ) -> Arc<McpConnection> {
        let ((cli_peer, cli_in), (responder, srv_in)) = kage_jsonrpc::testing::pair();
        thread::spawn(move || {
            loop {
                if kill.load(Ordering::SeqCst) {
                    return;
                }
                match srv_in.recv_timeout(std::time::Duration::from_millis(5)) {
                    Ok(Inbound::Request { id, method, .. }) => {
                        let outcome = match method.as_str() {
                            "initialize" => Ok(serde_json::json!({
                                "protocolVersion": PROTOCOL_VERSION,
                                "capabilities": capabilities,
                            })),
                            "tools/list" => Ok(serde_json::json!({
                                "tools": [{ "name": "t", "inputSchema": {} }],
                            })),
                            "resources/list" => Ok(serde_json::json!({
                                "resources": [{ "uri": "test://k", "name": "K" }],
                            })),
                            "resources/templates/list" => {
                                Ok(serde_json::json!({ "resourceTemplates": [] }))
                            }
                            other => Err(kage_jsonrpc::RpcError::method_not_found(other)),
                        };
                        let _ = responder.respond(&id, outcome);
                    }
                    Ok(Inbound::Notification { .. })
                    | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        Arc::new(McpConnection::initialize("x", cli_peer, cli_in, &[], None).unwrap())
    }

    #[test]
    fn refresh_evicts_a_dead_server_and_restart_uses_its_spec() {
        let kill = Arc::new(AtomicBool::new(false));
        let conn = killable_server(Arc::clone(&kill), serde_json::json!({}));
        let spec = McpServer {
            startup_timeout_secs: None,
            command: Some("definitely-not-a-real-binary-xyz".to_owned()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            disabled: false,
            oauth: None,
            disabled_tools: Vec::new(),
        };
        let mut mgr = McpManager::default();
        mgr.adopt("x", Arc::clone(&conn));
        mgr.servers[0].1.spec = spec;
        let mut reg = ToolRegistry::new();
        assert!(
            mgr.register_into(&mut reg).is_empty(),
            "injected server registers cleanly"
        );
        assert!(reg.get("x__t").is_some());
        assert_eq!(mgr.len(), 1);

        kill.store(true, Ordering::SeqCst);
        assert!(
            wait_until(|| conn.is_dead()),
            "kill switch must close the transport"
        );

        let errors = mgr.refresh_into(&mut reg);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].0, "x");
        assert!(
            matches!(&errors[0].1, McpError::Crashed { server, .. } if server == "x"),
            "{:?}",
            errors[0].1
        );
        assert!(reg.get("x__t").is_none(), "dead server's tools evicted");
        assert!(mgr.error("x").is_some_and(|e| e.contains("crashed")));
        assert_eq!(mgr.server_names().collect::<Vec<_>>(), ["x"]);
        assert!(mgr.is_empty(), "evicted server no longer counts as live");
        assert!(
            mgr.refresh_into(&mut reg).is_empty(),
            "an evicted server is skipped, not re-erroring"
        );

        let err = mgr.restart("x", &mut reg).unwrap_err();
        assert!(
            matches!(&err, McpError::Spawn { command, .. } if command == "definitely-not-a-real-binary-xyz"),
            "spec must survive eviction: {err}"
        );
    }

    /// A server with one tool, one resource, one template and one
    /// prompt, advertising `resources` and `prompts`. `resources/list`
    /// fails while `fail_resources` is set.
    fn catalog_server(
        fail_resources: Arc<AtomicBool>,
    ) -> (Arc<McpConnection>, kage_jsonrpc::Peer, Seen) {
        let caps = serde_json::json!({ "tools": {}, "resources": {}, "prompts": {} });
        scripted("srv", caps, move |method, _| {
            Ok(match method {
                "tools/list" => {
                    serde_json::json!({ "tools": [{ "name": "t", "inputSchema": {} }] })
                }
                "resources/list" if fail_resources.load(Ordering::SeqCst) => {
                    return Err(kage_jsonrpc::RpcError::internal("boom"));
                }
                "resources/list" => {
                    serde_json::json!({ "resources": [{ "uri": "test://r", "name": "R" }] })
                }
                "resources/templates/list" => serde_json::json!({
                    "resourceTemplates": [{ "uriTemplate": "test://r/{id}", "name": "T" }]
                }),
                "prompts/list" => serde_json::json!({ "prompts": [{ "name": "p" }] }),
                other => return Err(kage_jsonrpc::RpcError::method_not_found(other)),
            })
        })
    }

    fn methods(seen: &Seen) -> Vec<String> {
        seen.lock().unwrap().drain(..).map(|(m, _)| m).collect()
    }

    #[test]
    fn catalog_reports_live_counts_and_failed_errors() {
        let mut cfg = McpConfig::default();
        cfg.servers.insert(
            "broken".to_owned(),
            McpServer {
                startup_timeout_secs: None,
                command: Some("definitely-not-a-real-binary-xyz".to_owned()),
                args: vec![],
                env: std::collections::BTreeMap::new(),
                url: None,
                headers: std::collections::BTreeMap::new(),
                disabled: false,
                oauth: None,
                disabled_tools: Vec::new(),
            },
        );
        let (mut mgr, _errors) = McpManager::spawn_all(&cfg, vec![], None);
        let (conn, _srv, _seen) = catalog_server(Arc::new(AtomicBool::new(false)));
        mgr.adopt("srv", conn);
        let mut reg = ToolRegistry::new();
        assert!(mgr.register_into(&mut reg).is_empty());

        let catalog = mgr.catalog();
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0].name, "broken");
        let McpServerStatus::Failed { error } = &catalog[0].status else {
            panic!("broken must be failed: {:?}", catalog[0].status);
        };
        assert!(
            error.contains("definitely-not-a-real-binary-xyz"),
            "{error}"
        );
        assert_eq!(catalog[0].tools, 0);
        assert!(catalog[0].resources.is_empty());

        let live = &catalog[1];
        assert_eq!(live.name, "srv");
        assert_eq!(live.status, McpServerStatus::Connected);
        assert_eq!(live.tools, 1);
        assert_eq!(live.resources[0].uri, "test://r");
        assert_eq!(live.templates[0].uri_template, "test://r/{id}");
        assert_eq!(live.prompts[0].name, "p");

        let clients = mgr.clients();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].0, "srv");
        assert_eq!(clients[0].1.name(), "srv");
    }

    #[test]
    fn a_failing_catalog_list_leaves_tools_registered() {
        let fail = Arc::new(AtomicBool::new(true));
        let (conn, _srv, _seen) = catalog_server(Arc::clone(&fail));
        let mut mgr = McpManager::default();
        mgr.adopt("srv", conn);
        let mut reg = ToolRegistry::new();
        let errors = mgr.register_into(&mut reg);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].0, "srv");
        assert!(errors[0].1.to_string().contains("boom"), "{}", errors[0].1);
        assert!(reg.get("srv__t").is_some());
        let info = &mgr.catalog()[0];
        assert_eq!(info.tools, 1);
        assert!(info.resources.is_empty());
        assert_eq!(info.prompts.len(), 1, "prompts still load");
    }

    /// An HTTP MCP server on 127.0.0.1 that answers `initialize` (and
    /// `tools/list` with one tool) to `Bearer good` while `ready` is set,
    /// and 401 otherwise and to the `refused` method.
    fn guarded_server(ready: Arc<AtomicBool>, refused: Option<&'static str>) -> FakeServer {
        serve(move |request, _| {
            if request.method == "GET" {
                return Reply::status(405);
            }
            let body: serde_json::Value = serde_json::from_str(&request.body).unwrap_or_default();
            let allowed = ready.load(Ordering::SeqCst)
                && request.header("authorization") == Some("Bearer good")
                && refused.is_none_or(|method| body["method"] != method);
            if !allowed {
                return Reply::status(401);
            }
            match body["method"].as_str() {
                Some("initialize") => Reply::json(
                    200,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": body["id"],
                        "result": { "protocolVersion": PROTOCOL_VERSION, "capabilities": {} },
                    }),
                ),
                Some("tools/list") => Reply::json(
                    200,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": body["id"],
                        "result": { "tools": [{ "name": "t", "inputSchema": {} }] },
                    }),
                ),
                _ => Reply::status(202),
            }
        })
    }

    fn remote_config(url: String) -> McpConfig {
        let mut cfg = McpConfig::default();
        cfg.servers.insert(
            "remote".to_owned(),
            McpServer {
                startup_timeout_secs: None,
                command: None,
                args: vec![],
                env: std::collections::BTreeMap::new(),
                url: Some(url),
                headers: std::collections::BTreeMap::new(),
                disabled: false,
                oauth: None,
                disabled_tools: Vec::new(),
            },
        );
        cfg
    }

    #[test]
    fn a_refused_token_needs_auth_and_restart_sends_the_token() {
        let ready = Arc::new(AtomicBool::new(false));
        let server = guarded_server(Arc::clone(&ready), None);
        let tokens = StaticTokens::new("good", None);
        let (mut mgr, errors) = McpManager::spawn_all_with(
            &remote_config(format!("{}/mcp", server.base)),
            vec![],
            None,
            Some(Arc::clone(&tokens) as Arc<dyn TokenSource>),
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            matches!(&errors[0].1, McpError::Unauthorized { server, login: true } if server == "remote"),
            "{:?}",
            errors[0].1
        );
        assert_eq!(tokens.refreshes(), 1);
        assert_eq!(mgr.catalog()[0].status, McpServerStatus::NeedsAuth);
        assert!(
            mgr.error("remote")
                .is_some_and(|e| e.contains("kage mcp login remote"))
        );
        assert_eq!(mgr.server_names().collect::<Vec<_>>(), ["remote"]);

        ready.store(true, Ordering::SeqCst);
        let mut reg = ToolRegistry::new();
        mgr.restart("remote", &mut reg).unwrap();
        assert_eq!(mgr.catalog()[0].status, McpServerStatus::Connected);
        assert!(mgr.error("remote").is_none());
        assert!(reg.get("remote__t").is_some());
    }

    #[test]
    fn a_live_server_that_refuses_its_token_needs_auth() {
        let server = guarded_server(Arc::new(AtomicBool::new(true)), Some("tools/list"));
        let tokens = StaticTokens::new("good", None);
        let (mut mgr, errors) = McpManager::spawn_all_with(
            &remote_config(format!("{}/mcp", server.base)),
            vec![],
            None,
            Some(tokens),
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(mgr.len(), 1);
        let mut reg = ToolRegistry::new();
        let errors = mgr.register_into(&mut reg);
        assert!(
            matches!(&errors[..], [(name, McpError::Unauthorized { .. })] if name == "remote"),
            "{errors:?}"
        );
        assert!(mgr.is_empty());
        assert_eq!(mgr.catalog()[0].status, McpServerStatus::NeedsAuth);
        assert!(reg.get("remote__t").is_none());
    }

    /// A manager with the guarded `remote` server spawned and its tools
    /// registered.
    fn signed_in(server: &FakeServer, tokens: &Arc<StaticTokens>) -> (McpManager, ToolRegistry) {
        let (mut mgr, errors) = McpManager::spawn_all_with(
            &remote_config(format!("{}/mcp", server.base)),
            vec![],
            None,
            Some(Arc::clone(tokens) as Arc<dyn TokenSource>),
        );
        assert!(errors.is_empty(), "{errors:?}");
        let mut reg = ToolRegistry::new();
        assert!(mgr.register_into(&mut reg).is_empty());
        assert!(reg.get("remote__t").is_some());
        (mgr, reg)
    }

    fn assert_needs_auth(errors: &[(String, McpError)], mgr: &McpManager, reg: &ToolRegistry) {
        assert!(
            matches!(errors, [(name, McpError::Unauthorized { .. })] if name == "remote"),
            "{errors:?}"
        );
        assert_eq!(mgr.catalog()[0].status, McpServerStatus::NeedsAuth);
        assert!(mgr.is_empty());
        assert!(reg.get("remote__t").is_none());
    }

    #[test]
    fn a_refused_tool_call_needs_auth_at_the_next_refresh() {
        let server = guarded_server(Arc::new(AtomicBool::new(true)), Some("tools/call"));
        let tokens = StaticTokens::new("good", None);
        let (mut mgr, mut reg) = signed_in(&server, &tokens);

        let cancel = kage_core::CancelFlag::default();
        let cx = kage_tools::tool::ToolContext::new(std::path::Path::new("."), &cancel);
        let tool = reg.get("remote__t").unwrap();
        let out = tool.execute(serde_json::json!({}), &cx).unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("kage mcp login remote"), "{}", out.text);

        let errors = mgr.refresh_into(&mut reg);
        assert_needs_auth(&errors, &mgr, &reg);
    }

    #[test]
    fn a_refused_restart_takes_the_live_server_down() {
        let ready = Arc::new(AtomicBool::new(true));
        let server = guarded_server(Arc::clone(&ready), None);
        let tokens = StaticTokens::new("good", None);
        let (mut mgr, mut reg) = signed_in(&server, &tokens);

        ready.store(false, Ordering::SeqCst);
        let err = mgr.restart("remote", &mut reg).unwrap_err();
        assert_needs_auth(&[("remote".to_owned(), err)], &mgr, &reg);
    }

    #[test]
    fn a_logout_needs_auth_at_the_next_refresh() {
        let server = guarded_server(Arc::new(AtomicBool::new(true)), None);
        let tokens = StaticTokens::new("good", None);
        let (mut mgr, mut reg) = signed_in(&server, &tokens);
        assert!(mgr.refresh_into(&mut reg).is_empty());

        tokens.forget();
        let errors = mgr.refresh_into(&mut reg);
        assert_needs_auth(&errors, &mgr, &reg);
    }

    #[test]
    fn a_resources_list_changed_notice_reloads_resources_only() {
        let (conn, srv, seen) = catalog_server(Arc::new(AtomicBool::new(false)));
        let mut mgr = McpManager::default();
        mgr.adopt("srv", conn);
        let mut reg = ToolRegistry::new();
        assert!(mgr.register_into(&mut reg).is_empty());
        methods(&seen);

        assert!(mgr.refresh_into(&mut reg).is_empty());
        assert!(methods(&seen).is_empty(), "no notice, no request");

        srv.notify(
            "notifications/resources/list_changed",
            serde_json::json!({}),
        )
        .unwrap();
        srv.request("ping", serde_json::json!({}))
            .expect("the drain thread handled the notice before the ping");
        assert!(mgr.refresh_into(&mut reg).is_empty());
        assert_eq!(
            methods(&seen),
            ["resources/list", "resources/templates/list"]
        );
        assert_eq!(mgr.catalog()[0].resources.len(), 1);
    }

    #[test]
    fn the_resource_tool_exists_only_while_a_live_server_has_resources() {
        let (plain, _srv, _seen) = scripted(
            "plain",
            serde_json::json!({ "tools": {} }),
            |method, _| match method {
                "tools/list" => Ok(serde_json::json!({ "tools": [] })),
                other => Err(kage_jsonrpc::RpcError::method_not_found(other)),
            },
        );
        let mut mgr = McpManager::default();
        mgr.adopt("plain", plain);
        let mut reg = ToolRegistry::new();
        assert!(mgr.register_into(&mut reg).is_empty());
        assert!(reg.get(RESOURCE_TOOL).is_none(), "no server has resources");

        let kill = Arc::new(AtomicBool::new(false));
        let conn = killable_server(
            Arc::clone(&kill),
            serde_json::json!({ "tools": {}, "resources": {} }),
        );
        mgr.adopt("x", Arc::clone(&conn));
        assert!(mgr.register_into(&mut reg).is_empty());
        let tool = reg.get(RESOURCE_TOOL).expect("x advertises resources");
        assert!(
            !tool.description().contains('x'),
            "server names stay out of the static description: {}",
            tool.description()
        );
        assert_eq!(tool.risk(), kage_core::Risk::Read);

        kill.store(true, Ordering::SeqCst);
        assert!(
            wait_until(|| conn.is_dead()),
            "kill switch must close the transport"
        );
        let errors = mgr.refresh_into(&mut reg);
        assert!(
            matches!(&errors[..], [(name, McpError::Crashed { .. })] if name == "x"),
            "{errors:?}"
        );
        assert!(reg.get(RESOURCE_TOOL).is_none(), "x is gone");
        assert!(reg.get("x__t").is_none());
    }

    #[test]
    fn the_resource_tool_lists_the_cached_catalog() {
        let (conn, _srv, seen) = catalog_server(Arc::new(AtomicBool::new(false)));
        let mut mgr = McpManager::default();
        mgr.adopt("srv", conn);
        let mut reg = ToolRegistry::new();
        assert!(mgr.register_into(&mut reg).is_empty());
        methods(&seen);

        let tool = reg.get(RESOURCE_TOOL).expect("srv advertises resources");
        let cancel = kage_core::CancelFlag::default();
        let cx = kage_tools::tool::ToolContext::new(std::path::Path::new("."), &cancel);
        let out = tool
            .execute(serde_json::json!({ "server": "srv" }), &cx)
            .unwrap();
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("- test://r (R)"), "{}", out.text);
        assert!(out.text.contains("- test://r/{id} (T)"), "{}", out.text);
        assert!(methods(&seen).is_empty(), "listing sent a request");
    }
}
