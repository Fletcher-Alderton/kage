//! Interactive TUI entry point: `run_tui`.

use super::*;

use std::sync::OnceLock;

use kage_core::ThinkingLevel;
use kage_core::options::{OptionStore, OptionValue};
use kage_core::protocol::{Command, CommandKind, Envelope, HostEvent, NoticeLevel};
use kage_plugin::LogLevel;
use kage_tui::TranscriptScope;
use kage_tui::hostlog::LogPublisher;

use crate::engine::WithMediaTools as _;

/// Drop into the interactive TUI, on the recorded session at `resume`
/// when given, the way the session picker resumes one. A session
/// another process holds is attached to through the `kage serve`
/// hosting it, when one does. Returns the appropriate process exit
/// code once the user quits.
#[expect(clippy::too_many_lines, reason = "one linear startup sequence")]
pub fn run_tui(model: Option<&str>, system: &str, resume: Option<PathBuf>, yolo: bool) -> ExitCode {
    #[cfg(unix)]
    if let Some(path) = resume.as_deref()
        && kage_session::is_locked(path)
        && let Some(attached) = super::remote::attach(path)
    {
        return super::remote::run(attached, model);
    }
    let mut registry = match crate::build_provider_registry() {
        Ok(registry) => registry,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let provisional_model = model.map_or_else(|| crate::default_model(&registry), str::to_owned);

    // The buffer must exist before we build the plugin runtime so we can
    // hand the runtime a sink that routes notify/log into the buffer
    // instead of stderr (which would corrupt the alt screen).
    let buffer = shared_buffer();
    let toasts = shared_toasts();
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    crate::trust::confirm_project_trust(&workdir);
    // Ask the terminal for its background before raw mode and before
    // any theme resolves, so the `default` theme picks kage shadow or
    // kage dawn.
    kage_tui::theme::detect_terminal_background();
    let Some((app_config, options)) = startup_config(&workdir, &buffer) else {
        return ExitCode::from(1);
    };
    // Build the plugin runtime against a bare prompt first; skills land
    // below once plugins have had a chance to contribute extra dirs via
    // `resources_discover`.
    let bare_prompt = crate::runtime_env::build_system_prompt(
        system,
        &workdir,
        &provisional_model,
        &[],
        app_config.shell.program.as_deref(),
    );
    // Resolve once up-front so the same paths are shared by initial load,
    // the file-system watcher, and the worker's reload handler.
    let plugins_dir_path = match crate::plugins_dir() {
        Ok(dir) => Some(dir),
        Err(e) => {
            let mut buf = lock(&buffer);
            buf.push_custom("kage:error", e, false);
            None
        }
    };
    let user_dir = crate::config_dir().ok();
    let log_publisher: Arc<OnceLock<LogPublisher>> = Arc::default();
    let plugin_runtime = match setup_tui_runtime(
        plugins_dir_path.as_deref(),
        user_dir.as_deref(),
        app_config.plugins.clone(),
        app_config.keybindings.clone(),
        Arc::clone(&options),
        &workdir,
        &provisional_model,
        &bare_prompt,
        buffer_host_log(buffer.clone(), toasts.clone(), Arc::clone(&log_publisher)),
    ) {
        Ok(rt) => Some(rt),
        Err(e) => {
            let mut buf = lock(&buffer);
            buf.push_custom("kage:error", e, false);
            None
        }
    };
    let mut reported_shadows = std::collections::HashSet::new();
    if let Some(rt) = plugin_runtime.as_ref() {
        for id in crate::plugins::merge_plugin_providers(rt, &mut registry) {
            eprintln!("kage: plugin provider `{id}` shadows the built-in registration");
            reported_shadows.insert(id);
        }
        crate::acp_glue::set_runtime(rt);
    }

    let requested_model = model.map_or_else(|| crate::default_model(&registry), str::to_owned);
    if !crate::has_usable_provider(&registry) && registry.resolve(&requested_model).is_err() {
        // First-run onboarding: with no credentials anywhere, walk the
        // new user through `auth login` (its provider picker and key
        // prompt run in the normal terminal before the TUI starts)
        // instead of exiting with instructions. Esc on the picker or
        // an empty key falls back to the env-var guidance and exits.
        eprintln!("kage: welcome! no provider credentials found, let's add one.");
        loop {
            let code = crate::auth::run_login(None, &app_config);
            if code != ExitCode::SUCCESS {
                eprintln!("{}", crate::NO_CREDENTIALS_MESSAGE);
                return ExitCode::from(1);
            }
            registry = match crate::build_provider_registry() {
                Ok(registry) => registry,
                Err(e) => {
                    eprintln!("kage: {e}");
                    return ExitCode::from(1);
                }
            };
            if let Some(rt) = plugin_runtime.as_ref() {
                for id in crate::plugins::merge_plugin_providers(rt, &mut registry) {
                    eprintln!("kage: plugin provider `{id}` shadows the built-in registration");
                    reported_shadows.insert(id);
                }
            }
            if crate::has_usable_provider(&registry) {
                break;
            }
            eprintln!("kage: credential saved, but no provider is usable yet; add another.");
        }
        eprintln!("kage: connected. starting kage...");
    }
    // Recompute the default against the merged registry so a last-used
    // plugin-provided model resolves on restart.
    let qualified_model = match model {
        Some(m) => m.to_owned(),
        None => crate::default_model(&registry),
    };
    if qualified_model.is_empty() {
        eprintln!("{}", crate::NO_MODEL_MESSAGE);
        return ExitCode::from(1);
    }
    let bare_model = match registry.resolve(&qualified_model) {
        Ok(r) => r.model.clone(),
        Err(e) => {
            eprintln!("kage: cannot resolve model {qualified_model}: {e}");
            return ExitCode::from(1);
        }
    };
    let defaulted = model.is_none().then_some(qualified_model.as_str());
    let notices = support::start_notices(&registry, defaulted, chrono::Utc::now());
    let registry = Arc::new(registry);

    let skills = crate::load_skills(&workdir, plugin_runtime.as_deref());
    let system_prompt = crate::runtime_env::build_system_prompt(
        system,
        &workdir,
        &qualified_model,
        &skills,
        app_config.shell.program.as_deref(),
    );
    let system = system_prompt.as_str();

    let tools = kage_tools::configured_registry(&app_config.tools, &app_config.shell)
        .with_media_tools(&app_config);
    let mut plugin_command_listing: Vec<kage_tui::command::PluginCommand> = Vec::new();
    if let Some(rt) = plugin_runtime.as_ref() {
        plugin_command_listing = support::snapshot_plugin_commands(rt);
        support::register_block_renderers(rt);
    }
    // No MCP server spawns here: the TUI opens first and the engine
    // starts them off-thread once the session is open, so launch never
    // waits on `npx` or a slow handshake.
    let mcp_manager = crate::mcp::deferred_manager(&workdir, plugin_runtime.as_deref());
    let mut cx = AgentContext::new(bare_model, system).with_workdir(&workdir);
    if app_config.permissions.confine_paths {
        cx = cx.with_confine_paths();
    } else {
        cx = cx.without_confine_paths();
        eprintln!(
            "kage: path confinement is OFF for this session (permissions.confine_paths = false)"
        );
    }
    if let Some(window) = crate::runtime_env::context_window_for(&registry, &qualified_model) {
        cx = cx.with_context_window(window);
    }
    if let Some(out) = crate::runtime_env::max_output_tokens_for(&registry, &qualified_model) {
        cx = cx.with_max_output_tokens(out);
    }
    let (loop_cfg, thinking_level) = startup_options(&options);
    if let Some(level) = thinking_level {
        cx = cx.with_thinking_level(level);
    }
    let (tx, rx) = mpsc::channel::<RunRequest>();
    let tx_watcher = tx.clone();
    let (dialog_tx, dialog_rx) = mpsc::channel::<PluginDialog>();
    let (plugin_refresh_tx, plugin_refresh_rx) = mpsc::channel::<PluginRefresh>();
    let aliases = tools.alias_map();
    let gate = crate::permissions::PermissionGate::new(app_config.permissions.clone())
        .with_mcp_servers(mcp_manager.server_names().map(str::to_owned).collect())
        .with_aliases(aliases.clone());
    if yolo {
        gate.set_mode(Some(kage_core::permissions::PermissionAction::Allow));
    }

    let (agent_defs, agent_errors) = crate::agents::load(&workdir);
    for err in agent_errors {
        let mut buf = lock(&buffer);
        buf.push_custom("kage:error", err, false);
    }
    let agents = crate::engine::AgentSetup::from_options(
        agent_defs,
        &lock(&options),
        crate::engine::Background::Wake,
    );
    let model_choices = available_model_items(&registry, &qualified_model);
    if let Err(err) = crate::state::record_last_model(&qualified_model) {
        let mut buf = lock(&buffer);
        buf.push_custom("kage:error", format!("state: {err}"), false);
    }

    let planned = match crate::plan_session(&qualified_model, system) {
        Ok(planned) => Some(planned),
        Err(e) => {
            let mut buf = lock(&buffer);
            buf.push_custom("kage:error", format!("session: {e}"), false);
            None
        }
    };
    let session_id = planned
        .as_ref()
        .map_or_else(kage_core::SessionId::new, |(_, header)| header.session);
    if let Some(rt) = plugin_runtime.as_ref() {
        let ui = rt.slots().ui_state();
        let mut ui = lock(&ui);
        ui.session_id = session_id.to_string();
        ui.cwd = workdir.display().to_string();
    }
    let mirror = Arc::new(Mutex::new(host::Mirror::new(
        planned.as_ref().map(|(path, _)| path.clone()),
    )));
    let engine = crate::engine::Engine::start(Arc::clone(&registry));
    let (events_tx, events_rx) = mpsc::channel();
    engine.subscribe(Box::new(move |envelope| {
        let _ = events_tx.send(kage_core::protocol::with_canonical_tool_names(
            envelope.clone(),
            &aliases,
        ));
    }));
    engine.subscribe(Box::new(host::mirror(
        Arc::clone(&mirror),
        plugin_runtime.clone(),
    )));
    engine.open(crate::engine::SessionSpec {
        id: session_id,
        model: qualified_model.clone(),
        cx,
        recorder: planned.map(|(path, header)| {
            crate::engine::Recorder::planned(path, header, plugin_runtime.clone())
        }),
        tools,
        plugins: plugin_runtime.clone(),
        gate,
        loop_cfg,
        mcp: Some(mcp_manager),
        interactive: true,
        title: true,
        agents: Some(agents),
        shell: app_config.shell.program.clone(),
    });
    if let Some(path) = resume {
        engine.send(Command::active(CommandKind::LoadSession { path }));
        if let Some(model) = model {
            engine.send(Command::active(CommandKind::SetModel {
                model: model.to_owned(),
            }));
        }
    }
    let log_commander = engine.commander();
    let _ = log_publisher.set(Box::new(move |level, message| {
        let level = match level {
            LogLevel::Error => NoticeLevel::Error,
            _ => NoticeLevel::Info,
        };
        log_commander.publish(HostEvent::Notice {
            level,
            text: message.to_owned(),
            transient: false,
        });
    }));
    host::Host {
        link: Link::Local(engine.commander()),
        registry: Arc::clone(&registry),
        plugins: plugin_runtime.clone(),
        plugins_dir: plugins_dir_path.clone(),
        dialog_tx,
        plugin_refresh_tx,
        mirror: Arc::clone(&mirror),
        reported_shadows,
    }
    .spawn(rx);

    // Hot-reload watcher: polls the plugins dir, `init.lua` and `lua/`
    // every 150ms and asks for a reload when a Lua file changes. It ends
    // with the process.
    if plugin_runtime.is_some() {
        let (dir, user) = (plugins_dir_path.clone(), user_dir.clone());
        let buf = buffer.clone();
        thread::spawn(move || {
            let watcher = match kage_plugin::PluginWatcher::for_config(dir, user) {
                Ok(w) => w,
                Err(err) => {
                    let mut b = lock(&buf);
                    b.push_custom("kage:error", format!("plugin watcher: {err}"), false);
                    return;
                }
            };
            loop {
                thread::sleep(std::time::Duration::from_millis(150));
                if watcher.poll() && tx_watcher.send(RunRequest::ReloadPlugins).is_err() {
                    return;
                }
            }
        });
    }

    Frontend {
        buffer,
        toasts,
        options,
        config: app_config,
        workdir,
        plugins: plugin_runtime,
        mirror,
        requests: tx,
        events: events_rx,
        dialogs: dialog_rx,
        refresh: plugin_refresh_rx,
        model: qualified_model,
        model_choices,
        plugin_commands: plugin_command_listing,
        skills,
        notices,
        session: session_id,
    }
    .run(|| engine.shutdown())
}

/// Load the layered config for `workdir` and seed the options from it
/// before any Lua runs, so `init.lua` overrides them. Option values
/// that do not apply are shown in `buffer` and keep their defaults.
/// `None` once a config error that must stop kage is printed.
pub(super) fn startup_config(
    workdir: &std::path::Path,
    buffer: &kage_tui::SharedBuffer,
) -> Option<(kage_core::config::Config, kage_plugin::SharedOptions)> {
    let config = match kage_core::config::Config::load_layered(workdir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kage: {e}");
            return None;
        }
    };
    // Structurally broken permission rules are a hard error: kage
    // would silently misapply them otherwise. Mirrors the providers
    // validation in `build_provider_registry`.
    if let Err(e) = config.permissions.validate() {
        eprintln!("kage: {e}");
        return None;
    }
    if let Err(e) = config.shell.validate() {
        eprintln!("kage: {e}");
        return None;
    }
    let (store, option_errors) = OptionStore::from_config(&config);
    for err in option_errors {
        let mut buf = lock(buffer);
        buf.push_custom("kage:error", format!("config: {err}"), false);
    }
    Some((config, Arc::new(Mutex::new(store))))
}

/// What the App runs with, whichever engine it drives.
pub(super) struct Frontend {
    pub(super) buffer: kage_tui::SharedBuffer,
    pub(super) toasts: kage_tui::SharedToasts,
    pub(super) options: kage_plugin::SharedOptions,
    pub(super) config: kage_core::config::Config,
    pub(super) workdir: PathBuf,
    pub(super) plugins: Option<Arc<PluginRuntime>>,
    pub(super) mirror: Arc<Mutex<host::Mirror>>,
    pub(super) requests: mpsc::Sender<RunRequest>,
    pub(super) events: mpsc::Receiver<Envelope>,
    pub(super) dialogs: mpsc::Receiver<PluginDialog>,
    pub(super) refresh: mpsc::Receiver<PluginRefresh>,
    /// The model the status line shows until the engine reports one.
    pub(super) model: String,
    pub(super) model_choices: Vec<PickItem>,
    pub(super) plugin_commands: Vec<kage_tui::command::PluginCommand>,
    pub(super) skills: Vec<kage_core::Skill>,
    /// Lines the start card shows.
    pub(super) notices: Vec<(NoticeLevel, String)>,
    /// The session the TUI starts on.
    pub(super) session: kage_core::SessionId,
}

impl Frontend {
    /// Run the App until the user quits, call `stop` once the screen is
    /// restored, and print the exit summary.
    pub(super) fn run(self, stop: impl FnOnce()) -> ExitCode {
        let mut tui = match Tui::enter() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("kage: failed to enter raw mode: {e}");
                return ExitCode::from(1);
            }
        };
        let buffer = self.buffer.clone();
        let options = Arc::clone(&self.options);
        let mirror = Arc::clone(&self.mirror);
        let mut app = self.into_app();
        let result = app.run(&mut tui);
        let width = tui.terminal().size().map_or(80, |size| size.width);
        drop(tui);
        drop(app);
        stop();
        match result {
            Ok(_) => {
                print_exit_summary(&buffer, width, &options, lock(&mirror).path());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("kage: tui error: {e}");
                ExitCode::from(1)
            }
        }
    }

    fn into_app(self) -> App {
        let mut app = App::new(self.buffer.clone(), self.requests);
        app.set_model_choices(self.model_choices);
        app.set_history(crate::history::load());
        app.set_status_model(Arc::new(Mutex::new(self.model)));
        app.set_engine_events(self.events);
        app.set_plugin_commands(self.plugin_commands);
        app.set_skills(self.skills);
        // `:login` runs the interactive credential flow in the real
        // terminal (the App suspends itself around the call) and then
        // refreshes providers through the worker.
        {
            let config_for_login = self.config.clone();
            app.set_login_runner(std::sync::Arc::new(move |provider| {
                let ok = crate::auth::run_login(provider, &config_for_login) == ExitCode::SUCCESS;
                if ok && let Some(provider) = provider {
                    let _ = crate::state::clear_auth_failure(provider);
                }
                ok
            }));
        }
        // `/mcp login` runs the flow of `kage mcp login` the same way, and
        // the App restarts the server once it succeeds.
        {
            let mut servers = self.config.mcp.servers.clone();
            if let Some(rt) = self.plugins.as_ref() {
                servers.extend(rt.registered_mcp_servers());
            }
            app.set_mcp_login_runner(std::sync::Arc::new(move |server| {
                crate::mcp_auth::tui_login(server, &servers)
            }));
        }
        app.set_workdir(self.workdir.clone());
        if let Ok(dir) = crate::themes_dir() {
            app.set_themes_dir(dir);
        }
        let setter = self.plugins.clone().map(|rt| {
            Box::new(move |name: &str, value: OptionValue| {
                rt.set_option(name, value).map_err(|e| e.to_string())
            }) as kage_tui::OptionSetter
        });
        app.set_options(Arc::clone(&self.options), setter);
        if let Some(rt) = self.plugins.as_ref() {
            app.attach_plugins(rt);
        }
        app.set_plugin_dialog(self.dialogs);
        app.set_plugin_refresh(self.refresh);
        app.set_toasts(self.toasts.clone());
        app.set_session_usage(shared_session_usage());
        app.set_status_session_id(self.session.to_string().chars().take(8).collect());
        let permissions = if self.config.permissions.is_default() {
            "built-in tools run without asking"
        } else {
            "configured rules"
        };
        let start = kage_tui::StartInfo {
            sessions: Vec::new(),
            notices: self.notices,
            permissions: permissions.to_owned(),
        };
        wire_sessions(&mut app, &self.workdir, &self.mirror);
        app.set_start_info(start);
        app
    }
}

/// Give the App the recorded sessions of `workdir` for its start card,
/// picker and tree, and a loader for agent transcripts beside the
/// session file `mirror` points at.
fn wire_sessions(app: &mut App, workdir: &std::path::Path, mirror: &Arc<Mutex<host::Mirror>>) {
    if let Ok(dir) = crate::sessions_dir() {
        let sessions_cache = Arc::new(Mutex::new(kage_session::SessionCache::default()));
        // The first paint must not wait on the scan: the start card's
        // recent sessions arrive on this channel once the thread is
        // done, and the warmed cache makes the first Ctrl+S quick.
        let (sessions_tx, sessions_rx) = mpsc::channel();
        let scan_dir = dir.clone();
        let scan_workdir = workdir.to_path_buf();
        let scan_cache = Arc::clone(&sessions_cache);
        thread::spawn(move || {
            let _ = sessions_tx.send(list_session_choices(
                &scan_dir,
                &scan_workdir,
                false,
                &mut lock(&scan_cache),
            ));
        });
        let tree_dir = dir.clone();
        let tree_mirror = Arc::clone(mirror);
        let lister_workdir = workdir.to_path_buf();
        app.set_session_lister(Box::new(move |all| {
            list_session_choices(&dir, &lister_workdir, all, &mut lock(&sessions_cache))
        }));
        app.set_session_tree_source(Box::new(move || {
            list_session_nodes(&tree_dir, lock(&tree_mirror).path())
        }));
        app.set_start_sessions(sessions_rx);
    }
    let loader_mirror = Arc::clone(mirror);
    app.set_agent_loader(Box::new(move |session| {
        let dir = lock(&loader_mirror).path()?.parent()?.to_path_buf();
        let replay = kage_session::replay(&dir.join(format!("{session}.jsonl"))).ok()?;
        Some(kage_tui::StoredAgent {
            messages: replay.history.into_iter().map(Arc::new).collect(),
            compacted_agents: replay.compacted_agents.into_iter().map(Arc::new).collect(),
        })
    }));
}

/// Print what stays in the terminal once the alt screen is gone: the
/// transcript `transcript_on_exit` asks for at `width`, then the
/// session file when one was recorded. Nothing runs any more, so live
/// text is finished and unfinished tool calls and agent cards read as
/// interrupted.
fn print_exit_summary(
    buffer: &kage_tui::SharedBuffer,
    width: u16,
    options: &kage_plugin::SharedOptions,
    session: Option<&std::path::Path>,
) {
    let scope = lock(options)
        .get("transcript_on_exit")
        .and_then(OptionValue::as_str)
        .and_then(TranscriptScope::parse)
        .unwrap_or(TranscriptScope::Full);
    let transcript = {
        let mut buffer = lock(buffer);
        buffer.finish_streaming();
        buffer.interrupt_running_tools();
        kage_tui::transcript::render(&mut buffer, width, scope)
    };
    if !transcript.is_empty() {
        println!("{transcript}\n");
    }
    if let Some(path) = session.filter(|path| path.exists()) {
        println!("session saved to {}", path.display());
        if let Some(id) = crate::engine::session_id_of(path) {
            println!("resume it with `kage resume {id}`");
        }
    }
}

/// Read the options that apply when a session starts: the loop config
/// and the thinking level. Called after the runtime loaded `init.lua`,
/// so values set there reach the first session.
pub(crate) fn startup_options(
    options: &kage_plugin::SharedOptions,
) -> (LoopConfig, Option<ThinkingLevel>) {
    let store = lock(options);
    let mut loop_cfg = LoopConfig::default();
    if let Some(threshold) = store
        .get("compaction_threshold")
        .and_then(OptionValue::as_float)
    {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the threshold is a fraction that f32 holds"
        )]
        {
            loop_cfg.compaction_threshold = threshold as f32;
        }
    }
    let thinking = store
        .get("thinking_level")
        .and_then(OptionValue::as_str)
        .and_then(ThinkingLevel::parse);
    (loop_cfg, thinking)
}
