//! The shared engine and session setup behind every `kage rpc` and
//! `kage serve` connection.

use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use kage_acp::acp::SessionConfigSelectOption;
use kage_acp::agent::serve_agent;
use kage_core::config::{Config, McpServer as McpSpec};
use kage_core::permissions::PermissionAction;
use kage_core::protocol::{Command, CommandKind, Event, HostEvent, PermissionDecision, SessionId};
use kage_core::sync::lock;
use kage_jsonrpc::RpcError;
use kage_loop::{AgentContext, LoopConfig};
use kage_provider::{Provider, ProviderRegistry};
use kage_tools::{ToolRegistry, builtin_registry};

use super::options::{Settings, choice};
use super::{CliAcpAgent, live::Live, mcp};
use crate::engine::{AgentSetup, Background, Engine, SessionSpec, WithMediaTools as _};
use crate::permissions::PermissionGate;

/// Builds the engine session for a client session from the providers
/// in effect, its id, working directory, model and the MCP servers the
/// client passed. The caller fills in the history and recorder.
pub(crate) type SpecBuilder = Box<
    dyn Fn(
            &ProviderRegistry,
            SessionId,
            &str,
            &str,
            BTreeMap<String, McpSpec>,
        ) -> Result<SessionSpec, RpcError>
        + Send
        + Sync,
>;

/// A session open in the engine, with what a connection attaching to it
/// needs.
pub(super) struct Open {
    /// What the session's config options last showed.
    pub(super) settings: Settings,
}

/// One engine with the shared setup of [`CliAcpAgent`], behind every
/// connection served on the host.
pub(crate) struct Host {
    pub(super) engine: Engine,
    /// The providers in effect, replaced when the config's change.
    registry: RwLock<Arc<ProviderRegistry>>,
    pub(super) default_model: String,
    pub(super) sessions: PathBuf,
    pub(super) spec: SpecBuilder,
    /// The model choices `registry` offers.
    models: RwLock<Arc<[SessionConfigSelectOption]>>,
    /// The providers the server's plugins registered at startup, kept
    /// over every rebuild of `registry`.
    plugin_providers: Mutex<Vec<Arc<dyn Provider>>>,
    /// Advertised name to real name for tools the host renamed, for the
    /// bridges' card titles and kind hints. Loaded once at startup, like
    /// the TUI does.
    pub(super) aliases: BTreeMap<String, String>,
    /// Live engine state one subscriber folds envelopes into: the turn
    /// in flight, open asks, the agent tree, prompt owners, and what a
    /// session's attachments allow closing.
    pub(super) live: Arc<Mutex<Live>>,
    /// Sessions open in the engine, so a load from another connection
    /// attaches to the live session instead of reopening its file.
    open: Arc<Mutex<HashMap<SessionId, Open>>>,
    /// The workdir of every launched session, so `_kage/fs` can confine
    /// its paths per session.
    workdirs: Arc<Mutex<HashMap<SessionId, PathBuf>>>,
    /// The id the next connection gets.
    next_connection: AtomicU64,
}

impl Host {
    /// The host every connection is served on, with the credential and
    /// model checks the subcommand ran before serving. The error is
    /// ready for the caller's `kage: ` line; the caller adds its own
    /// subcommand prefix. With `yolo`, every session opens in allow
    /// mode.
    pub(crate) fn start(
        model_override: Option<&str>,
        system_role: &str,
        yolo: bool,
    ) -> Result<Arc<Self>, String> {
        let mut registry = crate::build_provider_registry()?;
        let plugin_providers = merge_plugin_providers(&mut registry, model_override, system_role);
        let default_model =
            model_override.map_or_else(|| crate::default_model(&registry), str::to_owned);
        if crate::has_usable_provider(&registry) {
            registry
                .resolve(&default_model)
                .map_err(|e| format!("cannot resolve model {default_model}: {e}"))?;
        } else {
            // A headless entry point starts anyway: a GUI or editor
            // client connects, saves a key through `_kage/auth/set`,
            // and the reloaded providers resolve. Terminal users get
            // the hint here; runs fail with the same hint per prompt.
            eprintln!(
                "kage: no provider credentials found; add one with `kage auth login` or an API-key env var"
            );
        }
        let sessions = crate::sessions_dir()?;
        let registry = Arc::new(registry);
        let aliases = {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let renames = Config::load_layered(&cwd)
                .map(|config| config.tools.rename)
                .unwrap_or_default();
            ToolRegistry::new().with_renames(&renames).alias_map()
        };
        let spec: SpecBuilder = {
            let system_role = system_role.to_owned();
            Box::new(
                move |registry: &ProviderRegistry, id, cwd: &str, model: &str, servers| {
                    let spec = session_spec(registry, &system_role, id, cwd, model, servers)?;
                    if yolo {
                        spec.gate.set_mode(Some(PermissionAction::Allow));
                    }
                    Ok(spec)
                },
            )
        };
        let host = Self::new(registry, default_model, sessions, spec, aliases);
        *lock(&host.plugin_providers) = plugin_providers;
        Ok(host)
    }

    /// The engine and shared setup every connection works through. The
    /// model list follows `registry`.
    pub(crate) fn new(
        registry: Arc<ProviderRegistry>,
        default_model: String,
        sessions: PathBuf,
        spec: SpecBuilder,
        aliases: BTreeMap<String, String>,
    ) -> Arc<Self> {
        let models = model_choices(&registry);
        let engine = Engine::start(Arc::clone(&registry));
        let live = Arc::new(Mutex::new(Live::new(engine.commander())));
        let host = Arc::new(Self {
            engine,
            registry: RwLock::new(registry),
            default_model,
            sessions,
            spec,
            models: RwLock::new(models),
            plugin_providers: Mutex::default(),
            aliases,
            live,
            open: Arc::default(),
            workdirs: Arc::default(),
            next_connection: AtomicU64::new(0),
        });
        let open = Arc::clone(&host.open);
        host.engine.subscribe(Box::new(move |envelope| {
            if let Event::Host(HostEvent::StateChanged { state }) = &envelope.event
                && let Some(open) = lock(&open).get_mut(&envelope.session)
            {
                open.settings = Settings::from(state);
            }
        }));
        let live = Arc::clone(&host.live);
        host.engine
            .subscribe(Box::new(move |envelope| lock(&live).observe(envelope)));
        host.prune_closed_sessions();
        host
    }

    /// Drops the open-session and workdir bookkeeping of every session
    /// the engine closed, so the maps cannot grow with the sessions a
    /// long-lived host served. The stream ends when the engine stops.
    fn prune_closed_sessions(self: &Arc<Self>) {
        let closed = self.engine.closed_sessions();
        let host = Arc::clone(self);
        thread::Builder::new()
            .name("kage-host-prune".to_owned())
            .spawn(move || {
                for id in closed {
                    lock(&host.open).remove(&id);
                    lock(&host.workdirs).remove(&id);
                }
            })
            .expect("the host prune thread");
    }

    /// The providers in effect.
    pub(super) fn registry(&self) -> Arc<ProviderRegistry> {
        Arc::clone(
            &self
                .registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// The model choices the providers in effect offer.
    pub(super) fn models(&self) -> Arc<[SessionConfigSelectOption]> {
        Arc::clone(
            &self
                .models
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Rebuilds the providers from the user config, keeping the ones
    /// plugins registered, so the next session and the engine's next
    /// run use an edited `[providers]` section.
    pub(super) fn reload_providers(&self) -> Result<(), String> {
        let mut registry = crate::build_provider_registry()?;
        for provider in lock(&self.plugin_providers).iter() {
            registry.register(Arc::clone(provider));
        }
        let registry = Arc::new(registry);
        let models = model_choices(&registry);
        *self
            .registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::clone(&registry);
        *self
            .models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = models;
        self.engine.commander().set_registry(registry);
        Ok(())
    }

    /// Serves one connection: a fresh agent on this host for `reader`
    /// and `writer`, sharing the engine with every other connection.
    pub(crate) fn serve<R, W>(self: Arc<Self>, reader: R, writer: W) -> Result<(), RpcError>
    where
        R: BufRead + Send + 'static,
        W: std::io::Write + Send + 'static,
    {
        self.serve_with(reader, writer, |_| {})
    }

    /// [`Host::serve`] with `prepare` run on the connection's agent
    /// before the first request arrives. The rpc tests open their
    /// standing session there.
    pub(super) fn serve_with<R, W, P>(
        self: Arc<Self>,
        reader: R,
        writer: W,
        prepare: P,
    ) -> Result<(), RpcError>
    where
        R: BufRead + Send + 'static,
        W: std::io::Write + Send + 'static,
        P: FnOnce(&CliAcpAgent) + Send + 'static,
    {
        serve_agent(reader, writer, |peer| {
            let agent = CliAcpAgent::new(Arc::clone(&self), peer);
            prepare(&agent);
            agent
        })
    }

    /// Hosts `spec` in the engine and records it open with `settings`,
    /// so a later load from another connection attaches to it. The
    /// opening connection counts as the session's first attachment.
    pub(super) fn launch(&self, spec: SessionSpec, settings: Settings) {
        lock(&self.live).attach(spec.id, true);
        lock(&self.open).insert(spec.id, Open { settings });
        lock(&self.workdirs).insert(spec.id, spec.cx.workdir.clone());
        self.engine.open(spec);
    }

    /// The workdir a session was launched with, the root `_kage/fs`
    /// confines its paths to.
    pub(super) fn workdir(&self, id: SessionId) -> Option<PathBuf> {
        lock(&self.workdirs).get(&id).cloned()
    }

    /// The settings of a session another connection has open, so the
    /// caller attaches to it instead of reopening its file. A session
    /// a close was just sent for reads as unopened.
    pub(super) fn open_settings(&self, id: SessionId) -> Option<Settings> {
        if lock(&self.live).is_closing(id) {
            return None;
        }
        lock(&self.open).get(&id).map(|open| open.settings.clone())
    }

    /// Counts a connection's attachment to `id`. A fresh open follows
    /// a close of the same session file, so Live drops stale state.
    pub(super) fn attach(&self, id: SessionId, fresh: bool) {
        lock(&self.live).attach(id, fresh);
    }

    /// Records that a connection released `id`, closing the session
    /// when it was the last attachment and the session is idle.
    pub(super) fn release(&self, id: SessionId) {
        lock(&self.live).release(id);
    }

    /// Whether some connection still holds `session` or the session its
    /// agent tree hangs under.
    pub(super) fn held(&self, session: SessionId) -> bool {
        lock(&self.live).held(session)
    }

    /// Declines every open ask under `root`: a permission ask is
    /// denied, a question ask is answered with no answers. Sent once no
    /// connection is left to answer, so runs parked on an ask go on
    /// instead of waiting for a client that is gone.
    pub(super) fn decline_asks_under(&self, root: SessionId) {
        for ask in lock(&self.live).asks_under(root) {
            let kind = if ask.question {
                CommandKind::AnswerQuestion {
                    request_id: ask.request_id,
                    answers: None,
                }
            } else {
                CommandKind::ResolvePermission {
                    request_id: ask.request_id,
                    decision: PermissionDecision::Deny,
                }
            };
            self.engine.send(Command::to(ask.session, kind));
        }
    }

    /// The next connection's id on this host.
    pub(super) fn next_connection(&self) -> u64 {
        self.next_connection.fetch_add(1, Ordering::Relaxed)
    }

    /// Cancels every run. The dispatcher stops once each session is
    /// idle, which closes the session files; the caller gives it a
    /// moment before the process exits.
    pub(crate) fn shutdown(&self) {
        self.engine.send(Command::active(CommandKind::Shutdown));
    }

    /// Records `connection` as the owner of `id`'s running prompt, or
    /// returns false when another connection owns it.
    pub(super) fn claim_prompt(&self, id: SessionId, connection: u64) -> bool {
        lock(&self.live).claim_prompt(id, connection)
    }

    /// Forgets every prompt `connection` owns, after it disconnected.
    pub(super) fn release_prompts_of(&self, connection: u64) {
        lock(&self.live).release_prompts_of(connection);
    }
}

/// Load the plugins of the server's directory once and add the
/// providers they register, so a model only a plugin provides passes
/// the startup checks and resolves for every session, as it does in the
/// TUI and print mode. The runtime is kept for the process, which keeps
/// the merged providers' Lua state alive.
fn merge_plugin_providers(
    registry: &mut ProviderRegistry,
    model_override: Option<&str>,
    system_role: &str,
) -> Vec<Arc<dyn Provider>> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let model = model_override.map_or_else(|| crate::default_model(registry), str::to_owned);
    let bare = crate::runtime_env::build_system_prompt(system_role, &cwd, &model, &[], None);
    let runtime = match crate::plugins_dir() {
        Ok(dir) => crate::plugins::setup_runtime(&dir, &cwd, &model, &bare).unwrap_or_else(|e| {
            eprintln!("kage: {e}");
            None
        }),
        Err(e) => {
            eprintln!("kage: {e}");
            None
        }
    };
    let Some(runtime) = runtime else {
        return Vec::new();
    };
    for id in crate::plugins::merge_plugin_providers(&runtime, registry) {
        eprintln!("kage: plugin provider `{id}` shadows the built-in registration");
    }
    crate::acp_glue::set_runtime(&runtime);
    runtime
        .registered_providers()
        .into_iter()
        .map(|provider| provider as Arc<dyn Provider>)
        .collect()
}

/// The model choices `registry` offers a client.
fn model_choices(registry: &ProviderRegistry) -> Arc<[SessionConfigSelectOption]> {
    crate::tui::available_model_items(registry, "")
        .into_iter()
        .map(|item| choice(&item.value, &item.label, item.group.as_deref()))
        .collect()
}

/// Everything an engine session for `cwd` on `model` runs with, including
/// the client's MCP `servers`. The caller fills in the history and
/// recorder.
fn session_spec(
    registry: &ProviderRegistry,
    system_role: &str,
    id: SessionId,
    cwd: &str,
    model: &str,
    servers: BTreeMap<String, McpSpec>,
) -> Result<SessionSpec, RpcError> {
    let workdir = if cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(cwd)
    };
    crate::trust::warn_if_untrusted(&workdir);
    let config = Config::load_layered(&workdir).map_err(|e| RpcError::internal(e.to_string()))?;
    let model = model.to_owned();
    let bare = crate::runtime_env::build_system_prompt(system_role, &workdir, &model, &[], None);
    let plugins = match crate::plugins_dir() {
        Ok(dir) => {
            crate::plugins::setup_runtime(&dir, &workdir, &model, &bare).unwrap_or_else(|e| {
                eprintln!("kage: {e}");
                None
            })
        }
        Err(e) => {
            eprintln!("kage: {e}");
            None
        }
    };
    let skills = crate::load_skills(&workdir, plugins.as_deref());
    let system_prompt = crate::runtime_env::build_system_prompt(
        system_role,
        &workdir,
        &model,
        &skills,
        config.shell.program.as_deref(),
    );
    let mut tools = builtin_registry()
        .with_shell_config(&config.shell)
        .with_web_search(&config.tools.web_search)
        .with_media_tools(&config)
        .with_renames(&config.tools.rename);
    let aliases = tools.alias_map();
    let editor: Vec<String> = servers.keys().cloned().collect();
    let (mcp, mcp_errors) =
        crate::mcp::spawn_and_register_with(&mut tools, &workdir, plugins.as_deref(), servers);
    for (server, err) in mcp_errors {
        eprintln!("kage: mcp `{server}`: {}", mcp::without_login(err, &editor));
    }
    let (defs, agent_errors) = crate::agents::load(&workdir);
    for err in agent_errors {
        eprintln!("kage: {err}");
    }
    let agents = AgentSetup::from_config(defs, &config, Background::Hold);
    config
        .permissions
        .validate()
        .map_err(|e| RpcError::internal(format!("permissions: {e}")))?;
    config
        .shell
        .validate()
        .map_err(|e| RpcError::internal(format!("shell: {e}")))?;
    let mut cx = AgentContext::new(model.clone(), &system_prompt).with_workdir(&workdir);
    if config.permissions.confine_paths {
        cx = cx.with_confine_paths();
    } else {
        cx = cx.without_confine_paths();
        eprintln!(
            "kage: path confinement is OFF for this session (permissions.confine_paths = false)"
        );
    }
    if let Some(window) = crate::runtime_env::context_window_for(registry, &model) {
        cx = cx.with_context_window(window);
    }
    Ok(SessionSpec {
        id,
        model,
        cx,
        recorder: None,
        tools,
        gate: PermissionGate::new(config.permissions)
            .with_fallback(PermissionAction::Ask)
            .with_aliases(aliases)
            .with_mcp_servers(mcp.server_names().map(str::to_owned).collect()),
        loop_cfg: LoopConfig {
            compaction_threshold: config.loop_settings.compaction_threshold,
            parallel_tools: false,
            ..LoopConfig::default()
        },
        plugins,
        mcp: Some(mcp),
        interactive: true,
        title: true,
        agents: Some(agents),
        shell: config.shell.program.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_core::permissions::PermissionsConfig;
    use kage_provider::testing::MockProvider;

    use super::super::options::Settings;
    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    /// A host with no plugins and a plain session spec on the mock.
    fn host_in(dir: &tempfile::TempDir) -> Arc<Host> {
        let registry =
            Arc::new(ProviderRegistry::new().with(Arc::new(MockProvider::sequence(Vec::new()))));
        let spec: SpecBuilder = Box::new(|_, id, cwd, _, _| {
            Ok(SessionSpec {
                id,
                model: "mock/m".to_owned(),
                cx: AgentContext::new("mock/m", "").with_workdir(cwd),
                recorder: None,
                tools: builtin_registry(),
                gate: PermissionGate::new(PermissionsConfig::default()),
                loop_cfg: LoopConfig::default(),
                plugins: None,
                mcp: None,
                interactive: true,
                title: true,
                agents: None,
                shell: None,
            })
        });
        Host::new(
            registry,
            "mock/m".into(),
            dir.path().to_path_buf(),
            spec,
            BTreeMap::new(),
        )
    }

    #[test]
    fn a_released_session_prunes_the_host_bookkeeping() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(&dir);
        let id = SessionId::new();
        let spec = (host.spec)(
            &host.registry(),
            id,
            dir.path().to_str().unwrap(),
            "mock/m",
            BTreeMap::new(),
        )
        .unwrap();
        let settings = Settings::of(&spec, &host.registry());
        host.launch(spec, settings);

        assert!(host.open_settings(id).is_some());
        assert!(host.workdir(id).is_some());

        host.attach(id, false);
        host.release(id);
        assert!(
            host.open_settings(id).is_some() && host.workdir(id).is_some(),
            "one attachment is left"
        );

        host.release(id);
        let deadline = std::time::Instant::now() + WAIT;
        while std::time::Instant::now() < deadline && host.workdir(id).is_some() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(host.workdir(id).is_none(), "the workdir is pruned");
        assert!(
            host.open_settings(id).is_none(),
            "the open record is pruned"
        );
        host.shutdown();
    }
}
