//! `kage rpc`: a spec-conformant Agent Client Protocol agent.
//!
//! Speaks ACP (newline-delimited JSON-RPC 2.0 over stdio, protocol
//! version 1) so editors that speak ACP can drive kage. Every connection
//! is served on one [`Host`], whose engine it shares with the other
//! connections, and every ACP session is an engine session: prompts
//! become engine commands, and a per-connection bridge turns engine
//! events into `session/update` notifications and
//! `session/request_permission` requests. A session another connection
//! has open is attached to instead of reopened: the attaching client
//! gets the recorded transcript plus the turn in flight, the title and
//! the open permission asks, all under one bus lock so nothing arrives
//! twice. A session no connection holds any more is closed once idle.
//! Recorded sessions can be listed, loaded with a replay of their
//! transcript, released with `session/close`, or resumed. Each session
//! offers its model, thinking level and permission mode as config
//! options, and the prompts of its live MCP servers as slash commands
//! named `<server>:<prompt>`, which the engine expands when they come back
//! as prompt text. The MCP servers a client passes when it opens a session
//! run for that session like the user's own configured servers, and win a
//! name clash with them.
//!
//! Agent sessions started by the `agent` tool are shown as subagent
//! sessions (draft RFD PR #1992) to a client that advertises the
//! `subagents` capability: announced with `subagent_update` on their
//! parent's session, streaming on their own session, and asking there.
//! For every other client they are not ACP sessions. Their permission
//! requests and progress go to the client session at the root of their
//! tree, on that session's top-level `agent` call.

mod bridge;
pub(crate) mod config_set;
mod content;
mod directory;
mod folders;
mod fs;
pub(crate) mod host;
#[cfg(unix)]
pub(crate) mod link;
mod live;
mod mcp;
mod models;
mod options;
mod plugin_files;
mod plugin_registry;
pub(crate) mod probe;
mod registry;
mod sessions;
mod themes;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufReader;
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use kage_acp::acp::{
    AgentCapabilities, AgentMeta, AuthSetRequest, CloseSessionRequest, CloseSessionResponse,
    ConfigGetRequest, ConfigGetResult, ConfigSetRequest, ConfigTestRequest, ConfigTestResult,
    DirectoryRequest, DirectoryResult, FoldersRequest, FoldersResult, FsRequest, FsResult,
    ImageSaveRequest, Implementation, InitializeRequest, InitializeResponse, InstalledPlugin,
    KageAgentInfo, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, McpCapabilities, ModelsResponse, NewSessionRequest, NewSessionResponse,
    OptionSetRequest, OptionsResponse, PROTOCOL_VERSION, PluginInstallRequest, PluginRemoveRequest,
    PromptCapabilities, PromptDelivery, PromptRequest, PromptResponse, RegistryInstallRequest,
    RegistryInstallResult, RegistrySearchRequest, RegistrySearchResult, ResumeSessionRequest,
    ResumeSessionResponse, SessionCapabilities, SessionConfigOption, SessionExportResponse,
    SessionForkRequest, SessionForkResponse, SessionRenameRequest, SessionRequest,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, Supported,
    SwarmResumeRequest, SwarmResumeResponse, ThemesRequest, ThemesResult,
};
use kage_acp::agent::{Agent, PromptContext};
use kage_core::config::{Config, McpServer as McpSpec, PluginsConfig};
use kage_core::permissions::PermissionAction;
use kage_core::protocol::{AgentTree, Command, CommandKind, Delivery, RunOutcome};
use kage_core::sync::lock;
use kage_core::{LoopError, SessionId, StopReason as CoreStopReason};
use kage_jsonrpc::{Peer, RpcError};

use bridge::{Ask, AskSet, Bridge, HeldUpdates};
use content::prompt_content;
use host::Host;
use live::Seed;
use mcp::editor_servers;
use options::{Changed, Settings, Shown, config_options};
use sessions::list_page;

use crate::engine::{Background, Recorder, SessionSpec, SubscriptionId};

/// What a redacted config value reads as.
const REDACTED: &str = "<redacted>";

/// The plugins in `dir`, scripts and packages, by name, each marked
/// with whether `plugins` lets it load, its version and whether it came
/// from a registry. A missing or unreadable directory has none, and an
/// entry that cannot load is left out (the load reports it).
fn installed_plugins(dir: &Path, plugins: &PluginsConfig) -> Vec<InstalledPlugin> {
    let mut installed: Vec<InstalledPlugin> = kage_kura::status(dir, None)
        .unwrap_or_default()
        .into_iter()
        .map(|status| InstalledPlugin {
            enabled: plugins.loads(&status.name),
            version: status.version.map(|v| v.to_string()),
            from_registry: status.locked.is_some(),
            name: status.name,
        })
        .collect();
    installed.sort_by(|a, b| a.name.cmp(&b.name));
    installed
}

/// Blanks the config values that may carry credentials before they leave
/// the process: provider and MCP header values, MCP environment values
/// and plugin settings. The names stay, so a client can still say what
/// is configured.
fn redact_secrets(config: &mut Config) {
    let blank = |map: &mut BTreeMap<String, String>| {
        for value in map.values_mut() {
            REDACTED.clone_into(value);
        }
    };
    for provider in config.providers.custom.values_mut() {
        blank(&mut provider.headers);
    }
    for provider in config.providers.overrides.values_mut() {
        blank(&mut provider.headers);
    }
    for server in config.mcp.servers.values_mut() {
        blank(&mut server.headers);
        blank(&mut server.env);
    }
    for agent in config.acp.agents.values_mut() {
        blank(&mut agent.env);
    }
    for settings in config.plugins.config.values_mut() {
        *settings = serde_json::Value::String(REDACTED.to_owned());
    }
}

/// Entry point for the `Rpc` subcommand.
pub(crate) fn run(
    model_override: Option<&str>,
    system_role: &str,
    system_file: Option<&Path>,
    yolo: bool,
) -> ExitCode {
    let system_role = match resolve_system(system_role, system_file) {
        Ok(system_role) => system_role,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    kage_core::reaper::start();
    let served = Host::start(model_override, &system_role, yolo)
        .map_err(|e| format!("rpc: {e}"))
        .and_then(|host| {
            host.serve(BufReader::new(std::io::stdin()), std::io::stdout())
                .map_err(|e| format!("rpc: {e}"))
        });
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: {e}");
            ExitCode::from(1)
        }
    }
}

/// The system prompt a run starts with: the inline text, with the
/// contents of `file` appended when one was given.
///
/// The file exists so a caller can keep a long or private prompt out of
/// its own command line, which `/proc/<pid>/cmdline` publishes to every
/// user on the host. Reading it once here rather than per session gives
/// the failure one path: a file that cannot be read stops the run,
/// instead of starting sessions that quietly lack their instructions.
fn resolve_system(inline: &str, file: Option<&Path>) -> Result<String, String> {
    let Some(file) = file else {
        return Ok(inline.to_owned());
    };
    let appended = std::fs::read_to_string(file)
        .map_err(|e| format!("cannot read the system prompt at {}: {e}", file.display()))?;
    let appended = appended.trim_end();
    match (inline.trim().is_empty(), appended.is_empty()) {
        (true, _) => Ok(appended.to_owned()),
        (false, true) => Ok(inline.to_owned()),
        (false, false) => Ok(format!("{inline}\n\n{appended}")),
    }
}

/// Maps between the session ids a client uses and engine session ids. A
/// client may load a session by an id prefix, so the two can differ.
#[derive(Default)]
struct Ids {
    by_client: HashMap<String, SessionId>,
    by_engine: HashMap<SessionId, String>,
    /// Running subagents, which the client may only cancel.
    subagents: HashMap<String, SessionId>,
    /// Agent sessions the client loaded to read. The engine never
    /// hosts them for the client, so they take no prompts.
    read_only: HashSet<String>,
}

impl Ids {
    fn insert(&mut self, client: String, engine: SessionId) {
        self.by_engine.insert(engine, client.clone());
        self.by_client.insert(client, engine);
    }

    /// Forgets a client session, after the connection released it.
    fn remove_client(&mut self, client: &str) {
        if let Some(engine) = self.by_client.remove(client) {
            self.by_engine.remove(&engine);
        }
    }
}

/// How a prompt's run ended, handed from the bridge to every waiting
/// prompt. Several prompts can wait on one run: one running and the
/// prompts steering or queuing behind it.
#[derive(Clone)]
struct PromptEnd {
    outcome: RunOutcome,
    stop: Option<CoreStopReason>,
}

type Waiters = Arc<Mutex<HashMap<SessionId, Vec<mpsc::Sender<PromptEnd>>>>>;

type ShownBySession = Arc<Mutex<HashMap<SessionId, Shown>>>;

/// Updates for client sessions whose opening response is not written
/// yet, capped per session at [`bridge::HELD_CAP`]; later updates are
/// dropped and counted. Sending them earlier would reach the client
/// before it knows the session.
type Held = bridge::HeldMap;

/// The ACP agent one connection on the host is served through. Holds the
/// per-connection maps; the engine and the session setup live in the
/// host every connection shares.
struct CliAcpAgent {
    host: Arc<Host>,
    /// This connection's id on the host.
    connection: u64,
    ids: Arc<Mutex<Ids>>,
    waiters: Waiters,
    shown: ShownBySession,
    subagents: Arc<AtomicBool>,
    /// Whether the client asked for tools without a `[permissions]`
    /// rule to run, as in the TUI, instead of asking first.
    unconfigured_run: AtomicBool,
    /// Whether the client is a kage client, which shows runs it did not
    /// prompt, so an idle session wakes for a background agent's result.
    kage_client: AtomicBool,
    peer: Peer,
    held: Held,
    asks: AskSet,
    subscription: SubscriptionId,
    /// Attaches waiting to be applied by the bridge.
    seeds: Arc<Mutex<Vec<Seed>>>,
}

impl CliAcpAgent {
    /// The agent for one connection on `host`.
    fn new(host: Arc<Host>, peer: Peer) -> Self {
        let connection = host.next_connection();
        let ids = Arc::new(Mutex::new(Ids::default()));
        let waiters = Waiters::default();
        let shown = ShownBySession::default();
        let subagents = Arc::new(AtomicBool::new(false));
        let held = Held::default();
        let asks = AskSet::default();
        let seeds = Arc::default();
        let mut bridge = Bridge {
            peer: peer.clone(),
            commander: host.engine.commander(),
            connection,
            live: Arc::clone(&host.live),
            ids: Arc::clone(&ids),
            waiters: Arc::clone(&waiters),
            models: host.models(),
            shown: Arc::clone(&shown),
            aliases: host.aliases.clone(),
            seen: HashMap::new(),
            stops: HashMap::new(),
            asks: Arc::clone(&asks),
            tree: AgentTree::default(),
            subagents: Arc::clone(&subagents),
            streaming: HashSet::new(),
            ended: HashMap::new(),
            commands: HashMap::new(),
            modes: HashMap::new(),
            names: HashMap::new(),
            statuses: HashMap::new(),
            fills: HashMap::new(),
            pruned_cost: HashMap::new(),
            compacting: HashMap::new(),
            compacting_started: HashSet::new(),
            paused: HashSet::new(),
            held: Arc::clone(&held),
            approving: HashMap::new(),
            seeds: Arc::clone(&seeds),
        };
        let subscription = host
            .engine
            .subscribe(Box::new(move |envelope| bridge.handle(envelope)));
        Self {
            host,
            connection,
            ids,
            waiters,
            shown,
            subagents,
            unconfigured_run: AtomicBool::new(false),
            kage_client: AtomicBool::new(false),
            peer,
            held,
            asks,
            subscription,
            seeds,
        }
    }

    /// The config options a new session opens with: the default model,
    /// its automatic thinking level and no permission mode.
    fn default_options(&self) -> Vec<SessionConfigOption> {
        let settings = Settings::fresh(&self.host.default_model, &self.host.registry());
        config_options(&self.host.models(), &settings)
    }

    /// What a session of this connection runs with: the host's spec,
    /// with the permission fallback the client asked for at
    /// `initialize`, and background results waking an idle session for
    /// kage clients. Agents the session starts inherit its gate.
    fn session_spec(
        &self,
        id: SessionId,
        cwd: &str,
        model: &str,
        servers: BTreeMap<String, McpSpec>,
    ) -> Result<SessionSpec, RpcError> {
        let mut spec = (self.host.spec)(&self.host.registry(), id, cwd, model, servers)?;
        if self.unconfigured_run.load(Ordering::SeqCst) {
            spec.gate = spec.gate.with_fallback(PermissionAction::Allow);
        }
        if self.kage_client.load(Ordering::SeqCst)
            && let Some(agents) = spec.agents.as_mut()
        {
            agents.background = Background::Wake;
        }
        Ok(spec)
    }

    /// Opens `spec` as the client session `client_id` and returns its
    /// config options. Updates for it wait for
    /// [`Agent::session_announced`].
    fn open(&self, client_id: String, spec: SessionSpec) -> Vec<SessionConfigOption> {
        let settings = Settings::of(&spec, &self.host.registry());
        let options = config_options(&self.host.models(), &settings);
        let shown = Shown {
            settings: settings.clone(),
            catching_up: false,
            changed: Changed::default(),
            swallowed: 0,
        };
        lock(&self.shown).insert(spec.id, shown);
        lock(&self.held).insert(spec.id, HeldUpdates::default());
        lock(&self.ids).insert(client_id, spec.id);
        self.host.launch(spec, settings);
        options
    }

    /// The layered config for the request's session workdir, or for the
    /// server's directory when the request names no known session.
    /// Read without validation: this serves diagnostics and the
    /// settings pages, so a broken config stays readable and fixable
    /// through the RPC surface.
    fn load_config(&self, req: &ConfigGetRequest) -> Result<Config, RpcError> {
        let workdir = req
            .session_id
            .as_deref()
            .and_then(|client_id| self.engine_id(client_id).ok())
            .and_then(|id| self.host.workdir(id));
        let dir = workdir.unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        });
        Config::load_layered_raw(&dir).map_err(|e| RpcError::internal(e.to_string()))
    }

    fn engine_id(&self, client_id: &str) -> Result<SessionId, RpcError> {
        lock(&self.ids)
            .by_client
            .get(client_id)
            .copied()
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {client_id}")))
    }

    /// The connection ended: stop delivering engine events to it, free
    /// what its prompts wait on, withdraw its open asks, and release
    /// every session it held. An ask whose session no other connection
    /// holds is denied, so the run goes on instead of waiting for a
    /// client that may never come back. The engine and its runs keep
    /// going for the connections that stay.
    fn detach(&self) {
        self.host.engine.unsubscribe(self.subscription);
        self.host.release_prompts_of(self.connection);
        lock(&self.waiters).clear();
        let asks: Vec<(SessionId, Ask)> = lock(&self.asks)
            .drain()
            .flat_map(|(session, asks)| asks.into_iter().map(move |ask| (session, ask)))
            .collect();
        let withdrawn: Vec<(SessionId, kage_core::protocol::RequestId, bool)> = asks
            .into_iter()
            .map(|(session, ask)| {
                let question = ask.is_question();
                (session, ask.stop(), question)
            })
            .collect();
        let sessions: Vec<SessionId> = lock(&self.ids).by_engine.keys().copied().collect();
        for id in sessions {
            self.host.release(id);
        }
        for (session, request_id, question) in withdrawn {
            if self.host.held(session) {
                continue;
            }
            let kind = if question {
                CommandKind::AnswerQuestion {
                    request_id,
                    answers: None,
                }
            } else {
                CommandKind::ResolvePermission {
                    request_id,
                    decision: kage_core::protocol::PermissionDecision::Deny,
                }
            };
            self.host.engine.send(Command::to(session, kind));
        }
    }
}

impl Agent for CliAcpAgent {
    fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
        self.subagents.store(
            req.client_capabilities.supports_subagents(),
            Ordering::SeqCst,
        );
        self.unconfigured_run.store(
            req.client_capabilities.unconfigured_tools_run(),
            Ordering::SeqCst,
        );
        self.kage_client.store(
            req.client_capabilities
                .meta
                .as_ref()
                .is_some_and(|meta| meta.kage.is_some()),
            Ordering::SeqCst,
        );
        InitializeResponse {
            protocol_version: PROTOCOL_VERSION,
            agent_capabilities: AgentCapabilities {
                load_session: true,
                steer: true,
                prompt_capabilities: PromptCapabilities {
                    image: true,
                    audio: true,
                    embedded_context: true,
                },
                mcp_capabilities: McpCapabilities {
                    http: true,
                    sse: false,
                },
                session_capabilities: SessionCapabilities {
                    list: Some(Supported {}),
                    resume: Some(Supported {}),
                    close: Some(Supported {}),
                },
            },
            agent_info: Some(Implementation {
                name: "kage".to_owned(),
                title: None,
                version: Some(kage_core::build::version().to_owned()),
            }),
            auth_methods: vec![],
            meta: Some(AgentMeta {
                kage: Some(KageAgentInfo {
                    cwd: std::env::current_dir()
                        .ok()
                        .map(|dir| dir.display().to_string()),
                    config_options: self.default_options(),
                }),
            }),
        }
    }

    fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
        let servers = editor_servers(&req.mcp_servers)?;
        let (path, mut header) =
            crate::plan_session_in(&self.host.sessions, &self.host.default_model, "");
        let id = header.session;
        let mut spec = self.session_spec(id, &req.cwd, &self.host.default_model, servers)?;
        header.cwd.clone_from(&spec.cx.workdir);
        header.system_prompt.clone_from(&spec.cx.system_prompt);
        spec.recorder = Some(Recorder::planned(path, header, spec.plugins.clone()));
        let config_options = self.open(id.to_string(), spec);
        Ok(NewSessionResponse {
            session_id: id.to_string(),
            config_options,
        })
    }

    fn load_session(
        &self,
        req: LoadSessionRequest,
        ctx: &PromptContext,
    ) -> Result<LoadSessionResponse, RpcError> {
        let config_options =
            self.open_recorded(&req.session_id, &req.cwd, &req.mcp_servers, Some(ctx), true)?;
        Ok(LoadSessionResponse { config_options })
    }

    fn list_sessions(&self, req: ListSessionsRequest) -> Result<ListSessionsResponse, RpcError> {
        list_page(&self.host.sessions, &req)
    }

    fn resume_session(
        &self,
        req: ResumeSessionRequest,
        ctx: &PromptContext,
    ) -> Result<ResumeSessionResponse, RpcError> {
        let config_options = self.open_recorded(
            &req.session_id,
            &req.cwd,
            &req.mcp_servers,
            Some(ctx),
            false,
        )?;
        Ok(ResumeSessionResponse { config_options })
    }

    /// Releases the caller's attachment to `req.session_id`, closing
    /// the session when it was the last one and the session is idle.
    /// A subagent session is not attachable, so closing one answers
    /// invalid params.
    fn close_session(&self, req: CloseSessionRequest) -> Result<CloseSessionResponse, RpcError> {
        let id = {
            let mut ids = lock(&self.ids);
            if ids.read_only.remove(&req.session_id) {
                return Ok(CloseSessionResponse {});
            }
            if ids.subagents.contains_key(&req.session_id) {
                return Err(RpcError::new(
                    -32602,
                    format!("{} is a subagent session", req.session_id),
                ));
            }
            ids.by_client.get(&req.session_id).copied()
        };
        let Some(id) = id else {
            return Err(RpcError::new(
                -32602,
                format!("unknown session {}", req.session_id),
            ));
        };
        let asks = lock(&self.asks).remove(&id).unwrap_or_default();
        for ask in asks {
            ask.stop();
        }
        lock(&self.ids).remove_client(&req.session_id);
        lock(&self.shown).remove(&id);
        lock(&self.held).remove(&id);
        self.host.release(id);
        if !self.host.held(id) {
            // No connection is left to answer the asks parked under the
            // session, so they are declined and the session closes once
            // idle.
            self.host.decline_asks_under(id);
        }
        Ok(CloseSessionResponse {})
    }

    fn set_config_option(
        &self,
        req: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let (commands, config_options) = {
            let mut shown = lock(&self.shown);
            let shown = shown.get_mut(&id).ok_or_else(|| {
                RpcError::new(-32602, format!("unknown session {}", req.session_id))
            })?;
            let commands = shown
                .settings
                .apply(&self.host.models(), &req.config_id, &req.value)?;
            shown.catching_up = true;
            shown.changed.absorb(&commands);
            shown.swallowed = 0;
            (
                commands,
                config_options(&self.host.models(), &shown.settings),
            )
        };
        for command in commands {
            self.host.engine.send(Command::to(id, command));
        }
        Ok(SetSessionConfigOptionResponse { config_options })
    }

    fn config_get(&self, req: ConfigGetRequest) -> Result<ConfigGetResult, RpcError> {
        let mut config = self.load_config(&req)?;
        redact_secrets(&mut config);
        let store =
            crate::auth::AuthStore::load().unwrap_or_else(|_| crate::auth::AuthStore::empty());
        let provider_keys = probe::provider_keys(&config, &store);
        let installed_plugins = crate::plugins_dir()
            .map(|dir| installed_plugins(&dir, &config.plugins))
            .unwrap_or_default();
        Ok(ConfigGetResult {
            providers: config.providers,
            mcp: config.mcp,
            permissions: config.permissions,
            plugins: config.plugins,
            ui: config.ui,
            acp: config.acp,
            installed_plugins,
            provider_keys,
        })
    }

    /// Writes the entry into the user config and answers with the
    /// snapshot after it. An edited `[providers]` section is reloaded
    /// at once; MCP servers, permissions and plugins apply to sessions
    /// opened after the write.
    fn config_set(&self, req: ConfigSetRequest) -> Result<ConfigGetResult, RpcError> {
        let path =
            Config::default_path().ok_or_else(|| RpcError::internal("no user config directory"))?;
        config_set::set(&path, &req.path, req.value.as_ref())?;
        if req
            .path
            .first()
            .is_some_and(|section| section == "providers" || section == "acp")
        {
            self.host.reload_providers().map_err(RpcError::internal)?;
        }
        self.config_get(ConfigGetRequest {
            session_id: req.session_id,
        })
    }

    /// Lists the provider's models with the user config and saved keys
    /// filling what the probe leaves out, or connects to the saved MCP
    /// server or ACP agent the request names. Only specs already saved
    /// in the user config are probed: a request's own command line and
    /// environment never run, so a leaked token cannot spawn processes.
    fn config_test(&self, req: ConfigTestRequest) -> Result<ConfigTestResult, RpcError> {
        let config = Config::load_default_raw().map_err(|e| RpcError::internal(e.to_string()))?;
        if let Some(provider) = &req.provider {
            let store =
                crate::auth::AuthStore::load().unwrap_or_else(|_| crate::auth::AuthStore::empty());
            return Ok(probe::probe(provider, &config, &store));
        }
        let path =
            Config::default_path().ok_or_else(|| RpcError::internal("no user config directory"))?;
        if let Some(mcp) = &req.mcp {
            return test_saved_mcp(&path, &mcp.name);
        }
        if let Some(acp) = &req.acp {
            return test_saved_acp(&path, &acp.name);
        }
        Err(RpcError::new(
            -32602,
            "name a provider, MCP server or ACP agent to test",
        ))
    }

    /// Saves or removes the key, then reloads the providers so one that
    /// had no key before registers.
    fn auth_set(&self, req: AuthSetRequest) -> Result<serde_json::Value, RpcError> {
        if req.provider.is_empty() {
            return Err(RpcError::new(-32602, "name the provider the key is for"));
        }
        let mut store = crate::auth::AuthStore::load().map_err(RpcError::internal)?;
        match req.key.filter(|key| !key.trim().is_empty()) {
            Some(key) => {
                store.set_api_key(&req.provider, key.trim());
            }
            None => {
                store.remove(&req.provider);
            }
        }
        store.save().map_err(RpcError::internal)?;
        self.host.reload_providers().map_err(RpcError::internal)?;
        Ok(serde_json::json!({}))
    }

    fn providers_directory(&self, req: DirectoryRequest) -> Result<DirectoryResult, RpcError> {
        directory::directory(&req)
    }

    fn folders(&self, req: FoldersRequest) -> Result<FoldersResult, RpcError> {
        folders::folders(&req)
    }

    fn themes(&self, _req: ThemesRequest) -> Result<ThemesResult, RpcError> {
        let dir = crate::themes_dir().map_err(RpcError::internal)?;
        Ok(themes::themes(&dir))
    }

    fn plugin_install(&self, req: PluginInstallRequest) -> Result<serde_json::Value, RpcError> {
        let dir = crate::plugins_dir().map_err(RpcError::internal)?;
        let name = plugin_files::install(&dir, &req)?;
        Ok(serde_json::json!({ "name": name }))
    }

    fn plugin_remove(&self, req: PluginRemoveRequest) -> Result<serde_json::Value, RpcError> {
        let dir = crate::plugins_dir().map_err(RpcError::internal)?;
        plugin_files::remove(&dir, &req)?;
        Ok(serde_json::json!({}))
    }

    fn registry_search(
        &self,
        req: RegistrySearchRequest,
    ) -> Result<RegistrySearchResult, RpcError> {
        plugin_registry::search(&req)
    }

    fn registry_install(
        &self,
        req: RegistryInstallRequest,
    ) -> Result<RegistryInstallResult, RpcError> {
        plugin_registry::install(&req)
    }

    fn session_fork(&self, req: SessionForkRequest) -> Result<SessionForkResponse, RpcError> {
        self.fork_recorded(&req)
    }

    fn session_export(&self, req: SessionRequest) -> Result<SessionExportResponse, RpcError> {
        self.export_recorded(&req.session_id)
    }

    /// Asks the engine to compact the session now. The result reaches
    /// the client as the compaction's updates.
    fn session_compact(&self, req: SessionRequest) -> Result<serde_json::Value, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        self.host.engine.send(Command::to(id, CommandKind::Compact));
        Ok(serde_json::json!({}))
    }

    fn models_list(&self) -> Result<ModelsResponse, RpcError> {
        Ok(models::catalog(&self.host.registry()))
    }

    fn options_list(&self, req: ConfigGetRequest) -> Result<OptionsResponse, RpcError> {
        Ok(registry::entries(&self.load_config(&req)?))
    }

    /// Writes the option into the user config. A project config that
    /// sets the same key still wins, which the answer shows.
    fn option_set(&self, req: OptionSetRequest) -> Result<OptionsResponse, RpcError> {
        let path =
            Config::default_path().ok_or_else(|| RpcError::internal("no user config directory"))?;
        registry::set(&path, &req)?;
        self.options_list(ConfigGetRequest::default())
    }

    /// Saves an image or audio clip the model or a tool produced. The
    /// engine reports where it went, or why it could not, as a notice
    /// on the session.
    fn image_save(&self, req: ImageSaveRequest) -> Result<serde_json::Value, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        self.host.engine.send(Command::to(
            id,
            CommandKind::Media {
                action: kage_core::protocol::ImageAction::Save,
                sha256: Some(req.sha256),
                path: req.path.map(std::path::PathBuf::from),
            },
        ));
        Ok(serde_json::json!({}))
    }

    fn session_rename(&self, req: SessionRenameRequest) -> Result<serde_json::Value, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let title = req.title.trim();
        if title.is_empty() {
            return Err(RpcError::new(-32602, "a title needs text"));
        }
        self.host.engine.send(Command::to(
            id,
            CommandKind::SetTitle {
                title: title.to_owned(),
            },
        ));
        Ok(serde_json::json!({}))
    }

    /// Continues swarm children of the session. Answers once the engine
    /// checked and attached every member: one that is not a swarm child
    /// of the session, or is still working, refuses the whole request.
    /// The members' results reach the session once all have reported.
    fn swarm_resume(&self, req: SwarmResumeRequest) -> Result<SwarmResumeResponse, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let mut members = BTreeMap::new();
        for (key, prompt) in req.members {
            let child = key
                .parse::<ulid::Ulid>()
                .map(SessionId)
                .map_err(|_| RpcError::new(-32602, format!("{key} is not a session id")))?;
            members.insert(child, prompt);
        }
        let resumed = self
            .host
            .engine
            .resume_swarm(id, members)
            .map_err(|text| RpcError::new(-32602, text))?;
        Ok(SwarmResumeResponse {
            resumed: resumed.iter().map(ToString::to_string).collect(),
        })
    }

    fn fs(&self, req: FsRequest) -> Result<FsResult, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let workdir = self
            .host
            .workdir(id)
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {}", req.session_id)))?;
        let permissions = Config::load_layered(&workdir)
            .map(|config| config.permissions)
            .unwrap_or_default();
        fs::handle(&workdir, &permissions, &req)
    }

    fn prompt(&self, req: PromptRequest, _ctx: &PromptContext) -> Result<PromptResponse, RpcError> {
        if lock(&self.ids).read_only.contains(&req.session_id) {
            return Err(RpcError::new(
                -32602,
                format!(
                    "{} is an agent's session; agent sessions are read-only",
                    req.session_id
                ),
            ));
        }
        let id = self.engine_id(&req.session_id)?;
        if !self.host.claim_prompt(id, self.connection) {
            return Err(RpcError::new(
                -32603,
                "session is busy; wait for the running prompt to finish",
            ));
        }
        let content = req.prompt.into_iter().map(prompt_content).collect();
        let delivery = match req.delivery {
            Some(PromptDelivery::Steer) => Delivery::Steer,
            _ => Delivery::Queue,
        };
        let (done, end) = mpsc::channel();
        lock(&self.waiters).entry(id).or_default().push(done);
        self.host
            .engine
            .send(Command::to(id, CommandKind::Prompt { content, delivery }));
        let end = end
            .recv()
            .map_err(|_| RpcError::internal("engine stopped"))?;
        match end.outcome {
            RunOutcome::Completed => Ok(PromptResponse {
                stop_reason: stop_reason(end.stop),
            }),
            RunOutcome::Cancelled => Ok(PromptResponse {
                stop_reason: StopReason::Cancelled,
            }),
            RunOutcome::Failed {
                error: LoopError::InvalidPrompt { message },
            } => Err(RpcError::new(-32602, message)),
            RunOutcome::Failed { error } => Err(RpcError::internal(error.to_string())),
        }
    }

    fn cancel(&self, session_id: &str) {
        let id = {
            let ids = lock(&self.ids);
            ids.by_client
                .get(session_id)
                .or_else(|| ids.subagents.get(session_id))
                .copied()
        };
        if let Some(id) = id {
            self.host.engine.send(Command::to(id, CommandKind::Cancel));
        }
    }

    fn session_announced(&self, session_id: &str) {
        let Some(id) = lock(&self.ids).by_client.get(session_id).copied() else {
            return;
        };
        let held = lock(&self.held).remove(&id).unwrap_or_default();
        bridge::flush_held(&self.peer, session_id, held);
    }

    fn detached(&self) {
        self.detach();
    }
}

/// Probes the `[mcp.servers.<name>]` entry saved in the user config at
/// `path`: the request names the server and nothing else, so only what
/// is on file is ever spawned. A name with no saved entry is invalid
/// params.
fn test_saved_mcp(path: &Path, name: &str) -> Result<ConfigTestResult, RpcError> {
    let invalid = |message: String| RpcError::new(-32602, message);
    let saved = saved_entry(path, &["mcp", "servers", name])?
        .ok_or_else(|| invalid(format!("save the server `{name}`, then test it")))?;
    let spec = serde_json::from_value::<McpSpec>(saved).map_err(|e| invalid(e.to_string()))?;
    Ok(probe::probe_mcp(name, &spec))
}

/// Probes the `[acp.agents.<name>]` entry saved in the user config at
/// `path`, command and environment included, the same way: an unsaved
/// name is refused instead of running a request-supplied command line.
fn test_saved_acp(path: &Path, name: &str) -> Result<ConfigTestResult, RpcError> {
    let invalid = |message: String| RpcError::new(-32602, message);
    let saved = saved_entry(path, &["acp", "agents", name])?
        .ok_or_else(|| invalid(format!("save the agent `{name}`, then test it")))?;
    let agent = serde_json::from_value::<kage_core::config::AcpAgent>(saved)
        .map_err(|e| invalid(e.to_string()))?;
    Ok(probe::probe_acp(&agent))
}

/// The JSON value the user config at `path` holds at `keys`, or `None`
/// when the file or the entry is not there.
fn saved_entry(path: &Path, keys: &[&str]) -> Result<Option<serde_json::Value>, RpcError> {
    kage_core::config_edit::current(path, keys).map_err(|e| RpcError::internal(e.to_string()))
}

/// A completed turn that hit the output-token cap surfaces as `MaxTokens`
/// so editors can warn the reply was cut off; every other ending is an
/// ordinary turn.
fn stop_reason(last: Option<CoreStopReason>) -> StopReason {
    match last {
        Some(CoreStopReason::MaxTokens) => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests;
