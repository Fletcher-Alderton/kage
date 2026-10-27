//! Translates the App's requests into engine commands.
//!
//! The engine runs the agent. This thread handles what stays on the TUI
//! side: plugin commands and keybindings (which may open dialogs in the
//! App), provider refresh, plugin reload, and the plugin events and
//! toasts that accompany model, thinking, and permission changes.

use super::*;

use kage_core::ThinkingLevel;
use kage_core::permissions::PermissionAction;
use kage_core::protocol::{
    Command, CommandKind, Delivery, Envelope, Event, HostEvent, NoticeLevel, RunOutcome,
    SessionState,
};
use kage_provider::ProviderError;

use crate::engine::{AUTO_THINKING, Commander};

/// Where the TUI's commands go: the engine in this process, or the
/// `kage serve` the TUI attached to.
#[derive(Clone)]
pub(crate) enum Link {
    /// The engine in this process.
    Local(Commander),
    /// A session a `kage serve` hosts.
    #[cfg(unix)]
    Remote(Arc<super::remote::Remote>),
}

impl Link {
    pub(crate) fn send(&self, command: Command) {
        match self {
            Self::Local(commander) => commander.send(command),
            #[cfg(unix)]
            Self::Remote(remote) => remote.send(&command),
        }
    }

    /// Publish a host event, such as a notice, on the active session.
    pub(crate) fn publish(&self, event: HostEvent) {
        match self {
            Self::Local(commander) => commander.publish(event),
            #[cfg(unix)]
            Self::Remote(remote) => remote.publish(event),
        }
    }

    /// Resolve models against `registry` from the next run on. Serve
    /// keeps its own registry.
    fn set_registry(&self, registry: Arc<ProviderRegistry>) {
        match self {
            Self::Local(commander) => commander.set_registry(registry),
            #[cfg(unix)]
            Self::Remote(_) => {}
        }
    }

    /// Replace every session's plugin tools after a plugin reload.
    /// Serve's plugins are its own.
    fn reload_plugin_tools(&self) {
        match self {
            Self::Local(commander) => commander.reload_plugin_tools(),
            #[cfg(unix)]
            Self::Remote(_) => {}
        }
    }

    /// The process id of the serve the TUI is attached to.
    fn serve_pid(&self) -> Option<u32> {
        match self {
            Self::Local(_) => None,
            #[cfg(unix)]
            Self::Remote(remote) => Some(remote.pid),
        }
    }
}

/// What the TUI host knows about the active session, kept current by
/// [`mirror`].
pub(crate) struct Mirror {
    state: SessionState,
    path: Option<PathBuf>,
}

impl Mirror {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self {
            state: SessionState::default(),
            path,
        }
    }

    /// The active session's file.
    pub(crate) fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }
}

/// A bus subscriber that keeps `mirror` and the UI state slot
/// components read current, records the last used model after a
/// completed run, tracks per-provider auth failures for the start
/// screen, and refreshes the plugin view of the session file. Sessions
/// announced by `AgentSpawned` are agents and never reach any of it.
pub(crate) fn mirror(
    mirror: Arc<Mutex<Mirror>>,
    plugins: Option<Arc<PluginRuntime>>,
) -> impl FnMut(&Envelope) + Send {
    let ui = plugins.as_ref().map(|rt| rt.slots().ui_state());
    let update_ui = move |f: &dyn Fn(&mut kage_plugin::UiState)| {
        if let Some(ui) = &ui {
            f(&mut lock(ui));
        }
    };
    let mut agents = std::collections::HashSet::new();
    move |envelope| {
        if let Event::Host(HostEvent::AgentSpawned { .. }) = &envelope.event {
            agents.insert(envelope.session);
        }
        if agents.contains(&envelope.session) {
            return;
        }
        match &envelope.event {
            Event::Host(HostEvent::StateChanged { state }) => {
                lock(&mirror).state = state.clone();
                update_ui(&|ui| ui.state = state.clone());
            }
            Event::Host(HostEvent::UsageUpdated { usage }) => {
                update_ui(&|ui| ui.usage = Some(*usage));
            }
            Event::Host(HostEvent::TitleChanged { title }) => {
                update_ui(&|ui| ui.session_title = Some(title.clone()));
            }
            Event::Host(HostEvent::SessionChanged { path, title, .. }) => {
                agents.clear();
                lock(&mirror).path = Some(path.clone());
                update_ui(&|ui| {
                    ui.session_id = envelope.session.to_string();
                    ui.session_title.clone_from(title);
                });
                refresh_session_entries(plugins.as_ref(), Some(path));
            }
            Event::Host(HostEvent::RunEnded { outcome }) => {
                let mirror = lock(&mirror);
                if *outcome == RunOutcome::Completed {
                    let _ = crate::state::record_last_model(&mirror.state.model);
                }
                let _ = crate::state::record_run_outcome(&mirror.state.model, outcome);
                refresh_session_entries(plugins.as_ref(), mirror.path.as_deref());
            }
            _ => {}
        }
    }
}

/// Everything the request thread needs.
pub(crate) struct Host {
    pub link: Link,
    pub registry: Arc<ProviderRegistry>,
    pub plugins: Option<Arc<PluginRuntime>>,
    pub plugins_dir: Option<PathBuf>,
    pub dialog_tx: mpsc::Sender<PluginDialog>,
    pub plugin_refresh_tx: mpsc::Sender<PluginRefresh>,
    pub mirror: Arc<Mutex<Mirror>>,
    /// Shadow ids already surfaced this session. Rebuilds run on every
    /// refresh and reload, so without this the same override would toast
    /// again each time.
    pub(crate) reported_shadows: std::collections::HashSet<String>,
}

impl Host {
    /// Serve App requests until the App goes away.
    pub(crate) fn spawn(mut self, requests: mpsc::Receiver<RunRequest>) {
        thread::spawn(move || {
            for request in requests {
                self.handle(request);
            }
        });
    }

    fn send(&self, kind: CommandKind) {
        self.link.send(Command::active(kind));
    }

    fn notify(&self, text: String) {
        self.link.publish(HostEvent::Notice {
            level: NoticeLevel::Info,
            text,
            transient: true,
        });
    }

    fn error(&self, text: String) {
        self.link.publish(HostEvent::Notice {
            level: NoticeLevel::Error,
            text,
            transient: false,
        });
    }

    /// Apply a change to the UI state slot components read, before the
    /// event announcing it dispatches, so a component recomputed on
    /// that event sees it. The engine's `StateChanged` confirms it.
    fn update_ui(&self, f: impl FnOnce(&mut kage_plugin::UiState)) {
        if let Some(rt) = &self.plugins {
            f(&mut lock(&rt.slots().ui_state()));
        }
    }

    fn plugin_event(&self, name: &str, payload: &serde_json::Value) {
        if let Some(rt) = &self.plugins {
            let _ = rt.dispatch_event(name, payload);
        }
    }

    /// Refuses `request` with a notice when it would replace the session
    /// a `kage serve` shares with other clients.
    fn refused_while_attached(&self, request: &RunRequest) -> bool {
        let Some(pid) = self.link.serve_pid() else {
            return false;
        };
        let op = match request {
            RunRequest::NewSession => "a new session",
            RunRequest::CloneSession => "cloning",
            RunRequest::ResumeSession(_) | RunRequest::SwitchSession(SwitchTarget::Session(_)) => {
                "switching sessions"
            }
            RunRequest::SwitchSession(SwitchTarget::PendingFork(_)) => "switching to a fork",
            RunRequest::UndoSession => "undoing",
            RunRequest::Rewind { .. } => "rewinding",
            _ => return false,
        };
        self.error(format!(
            "{op} is not available while attached to kage serve (pid {pid}); \
             quit and start kage again for it"
        ));
        true
    }

    #[expect(clippy::too_many_lines, reason = "one match arm per request kind")]
    fn handle(&mut self, request: RunRequest) {
        if self.refused_while_attached(&request) {
            return;
        }
        match request {
            RunRequest::Submit {
                text,
                images,
                queue,
                session,
            } => {
                if let Err(err) = crate::history::append(&text) {
                    self.error(format!("history: {err}"));
                }
                let mut content = Vec::with_capacity(1 + images.len());
                if !text.is_empty() || images.is_empty() {
                    content.push(Content::Text { text });
                }
                content.extend(images.into_iter().map(|img| Content::Image {
                    source: img.source,
                    mime: img.mime,
                }));
                let delivery = if queue {
                    Delivery::Queue
                } else {
                    Delivery::Steer
                };
                let kind = CommandKind::Prompt { content, delivery };
                match session {
                    Some(session) => self.link.send(Command::to(session, kind)),
                    None => self.send(kind),
                }
            }
            RunRequest::Cancel { session } => match session {
                Some(session) => self.link.send(Command::to(session, CommandKind::Cancel)),
                None => self.send(CommandKind::Cancel),
            },
            RunRequest::RecallPrompt { session, delivery } => match session {
                Some(session) => self.link.send(Command::to(
                    session,
                    CommandKind::WithdrawPrompt { delivery },
                )),
                None => self.send(CommandKind::WithdrawPrompt { delivery }),
            },
            RunRequest::ResolvePermission {
                request_id,
                decision,
            } => self.send(CommandKind::ResolvePermission {
                request_id,
                decision,
            }),
            RunRequest::AnswerQuestion {
                request_id,
                answers,
            } => self.send(CommandKind::AnswerQuestion {
                request_id,
                answers,
            }),
            RunRequest::SwitchModel(model) => self.switch_model(&model),
            RunRequest::CycleThinkingLevel => {
                let next = {
                    let state = &lock(&self.mirror).state;
                    ThinkingLevel::next_in(&state.thinking_levels, state.thinking_effective)
                };
                match next {
                    Some(level) => self.set_thinking(Some(level), "cycle"),
                    None => self.notify("this model has no thinking levels".to_owned()),
                }
            }
            RunRequest::SetThinkingLevel(value) => match value.as_str() {
                "" | AUTO_THINKING => self.set_thinking(None, "settings"),
                _ => match ThinkingLevel::parse(&value) {
                    Some(level) => self.set_thinking(Some(level), "settings"),
                    None => self.error(format!("settings: unknown thinking level: {value}")),
                },
            },
            RunRequest::SetPermissionMode(mode) => self.set_permission_mode(mode),
            RunRequest::SwarmMode { on } => self.send(CommandKind::SwarmMode { on }),
            RunRequest::SetGoal { goal } => self.send(CommandKind::SetGoal { goal }),
            RunRequest::PlanMode { on } => self.send(CommandKind::PlanMode { on }),
            RunRequest::CompactNow => self.send(CommandKind::Compact),
            RunRequest::RunShell(command) => self.send(CommandKind::Shell { command }),
            RunRequest::NewSession => self.send(CommandKind::NewSession),
            RunRequest::CloneSession => self.send(CommandKind::Clone),
            RunRequest::ExportSession(path) => self.send(CommandKind::Export {
                path: path.map(|path| std::path::absolute(&path).unwrap_or(path)),
            }),
            RunRequest::Image { action, path } => self.send(CommandKind::Image {
                action,
                sha256: None,
                path,
            }),
            RunRequest::Media { action, path } => self.send(CommandKind::Media {
                action,
                sha256: None,
                path,
            }),
            RunRequest::ForkSessionFile(path) => self.send(CommandKind::ForkFile { path }),
            RunRequest::DeleteSession(path) => self.send(CommandKind::DeleteSession { path }),
            RunRequest::ResumeSession(path) => {
                let target = path.display().to_string();
                if let Some(dest) = self.consult("session_before_switch", &target) {
                    self.send(CommandKind::LoadSession {
                        path: PathBuf::from(dest),
                    });
                }
            }
            RunRequest::ForkSession { at } => {
                if let Some(at) = self.consult("session_before_fork", &at) {
                    self.send(CommandKind::Fork {
                        at: (!at.is_empty()).then_some(at),
                        switch: false,
                    });
                }
            }
            RunRequest::UndoSession => self.send(CommandKind::Undo),
            RunRequest::ListPrompts => self.send(CommandKind::ListPrompts),
            RunRequest::Rewind { at } => self.send(CommandKind::Rewind { at }),
            RunRequest::SwitchSession(SwitchTarget::Session(target)) => {
                match resolve_switch_target(&target) {
                    Ok(path) => {
                        let target = path.display().to_string();
                        if let Some(dest) = self.consult("session_before_switch", &target) {
                            self.send(CommandKind::LoadSession {
                                path: PathBuf::from(dest),
                            });
                        }
                    }
                    Err(err) => self.error(format!("switch: {err}")),
                }
            }
            RunRequest::SwitchSession(SwitchTarget::PendingFork(at)) => {
                if let Some(at) = self.consult("session_before_switch", &at) {
                    self.send(CommandKind::Fork {
                        at: (!at.is_empty()).then_some(at),
                        switch: true,
                    });
                }
            }
            RunRequest::InvokePluginCommand { name, args } => {
                let Some(rt) = self.plugins.clone() else {
                    return;
                };
                let command = rt
                    .registered_command_overrides()
                    .into_iter()
                    .chain(rt.registered_commands())
                    .find(|c| c.name() == name);
                match command {
                    Some(cmd) => {
                        let output =
                            run_bridged_command(&rt, &cmd, &args, &self.dialog_tx, &self.link);
                        self.show_plugin_output(output);
                    }
                    None => self.error(format!("no plugin command: {name}")),
                }
            }
            RunRequest::InvokeKeymap { id } => {
                let Some(rt) = self.plugins.clone() else {
                    return;
                };
                let output = run_bridged_keymap(&rt, id, &self.dialog_tx, &self.link);
                self.show_plugin_output(output);
            }
            RunRequest::RefreshProviders => self.refresh_providers(),
            RunRequest::ReloadPlugins => self.reload_plugins(),
            RunRequest::RestartMcp(server) => self.send(CommandKind::RestartMcp { server }),
        }
    }

    fn switch_model(&self, model: &str) {
        if let Err(err) = self.registry.resolve(model) {
            self.error(switch_error(model, &err));
            return;
        }
        let prev = lock(&self.mirror).state.model.clone();
        self.send(CommandKind::SetModel {
            model: model.to_owned(),
        });
        self.notify(format!("switched to {model}"));
        self.update_ui(|ui| model.clone_into(&mut ui.state.model));
        self.plugin_event(
            "model_select",
            &serde_json::json!({ "prev": prev, "next": model, "source": "set" }),
        );
        if let Err(err) = crate::state::record_last_model(model) {
            self.error(format!("state: {err}"));
        }
        // Model-select handlers can register commands, widgets, or completions.
        self.publish_plugin_refresh(model);
    }

    /// Choose `level` for the active session, `None` for the automatic
    /// level.
    fn set_thinking(&self, level: Option<ThinkingLevel>, source: &str) {
        let name = |l: Option<ThinkingLevel>| l.map_or(AUTO_THINKING, ThinkingLevel::as_str);
        let prev = lock(&self.mirror).state.thinking;
        self.send(CommandKind::SetThinking { level });
        self.notify(format!(
            "thinking level: {}",
            level.map_or("auto", ThinkingLevel::as_str)
        ));
        self.update_ui(|ui| ui.state.thinking = level);
        self.plugin_event(
            "thinking_level_select",
            &serde_json::json!({
                "prev": name(prev),
                "next": name(level),
                "source": source,
            }),
        );
    }

    fn set_permission_mode(&self, mode: Option<PermissionAction>) {
        let label = |m: Option<PermissionAction>| match m {
            Some(PermissionAction::Ask) => "ask",
            Some(PermissionAction::Deny) => "deny",
            _ => "default",
        };
        let prev = lock(&self.mirror).state.permission_mode;
        self.send(CommandKind::SetPermissionMode { mode });
        self.notify(match mode {
            Some(_) => format!("permission mode: {}", label(mode)),
            None => "permission mode: default (configured rules)".to_owned(),
        });
        self.update_ui(|ui| ui.state.permission_mode = mode);
        self.plugin_event(
            "permission_mode_select",
            &serde_json::json!({
                "prev": label(prev),
                "next": label(mode),
                "source": "command",
            }),
        );
    }

    fn show_plugin_output(&self, output: Option<CommandOutput>) {
        let Some(output) = output.filter(|o| !o.text.is_empty()) else {
            return;
        };
        self.link.publish(HostEvent::Notice {
            level: if output.is_error {
                NoticeLevel::Error
            } else {
                NoticeLevel::Info
            },
            text: output.text,
            transient: false,
        });
    }

    /// Let plugins veto or rewrite a session operation's target.
    fn consult(&self, event: &str, target: &str) -> Option<String> {
        let Some(rt) = &self.plugins else {
            return Some(target.to_owned());
        };
        if rt.handler_count(event) == 0 {
            return Some(target.to_owned());
        }
        match rt.dispatch_session_op(event, target) {
            Ok(kage_plugin::SessionOpDecision::Proceed) => Some(target.to_owned()),
            Ok(kage_plugin::SessionOpDecision::Patch(next)) => Some(next),
            Ok(kage_plugin::SessionOpDecision::Cancel { reason }) => {
                self.error(format!("{event}: {reason}"));
                None
            }
            Err(err) => {
                self.error(format!("{event}: plugin dispatch failed: {err}"));
                Some(target.to_owned())
            }
        }
    }

    fn refresh_providers(&mut self) {
        let active_ok = match self.rebuild_registry() {
            Ok(active_ok) => active_ok,
            Err(e) => {
                self.error(format!("providers not refreshed: {e}"));
                return;
            }
        };
        let active = lock(&self.mirror).state.model.clone();
        self.publish_plugin_refresh(&active);
        if active_ok {
            self.notify("providers refreshed".to_owned());
        } else {
            self.notify("providers refreshed; pick a model (/model)".to_owned());
        }
    }

    /// Rebuild the provider registry with plugin providers and hand it to
    /// the engine. Returns whether the active model still resolves, or
    /// the config error that kept the current registry.
    fn rebuild_registry(&mut self) -> Result<bool, String> {
        let mut fresh = crate::build_provider_registry()?;
        if let Some(rt) = &self.plugins {
            for id in crate::plugins::merge_plugin_providers(rt, &mut fresh) {
                if self.reported_shadows.insert(id.clone()) {
                    self.notify(format!(
                        "plugin provider `{id}` shadows the built-in registration"
                    ));
                }
            }
        }
        let active_ok = fresh.resolve(&lock(&self.mirror).state.model).is_ok();
        self.registry = Arc::new(fresh);
        self.link.set_registry(Arc::clone(&self.registry));
        Ok(active_ok)
    }

    /// Re-read plugins and `init.lua` from disk and republish everything
    /// they contribute: tools, block renderers, providers, commands,
    /// widgets, and autocomplete. Keymaps live in the table the App
    /// shares with the runtime, so the reload updates them in place.
    fn reload_plugins(&mut self) {
        let Some(rt) = self.plugins.clone() else {
            return;
        };
        let reload = rt.reload_all(self.plugins_dir.as_deref());
        self.link.reload_plugin_tools();
        super::support::register_block_renderers(&rt);
        if let Err(e) = self.rebuild_registry() {
            self.error(format!("providers not refreshed: {e}"));
        }
        let active = lock(&self.mirror).state.model.clone();
        self.publish_plugin_refresh(&active);
        let report = match reload {
            Ok(report) => report,
            Err(err) => {
                self.error(format!("plugin reload: {err}"));
                return;
            }
        };
        let init = match report.init {
            Some(Ok(())) => ", init.lua ok",
            Some(Err(_)) => ", init.lua failed",
            None => "",
        };
        for err in &report.keymap_errors {
            self.error(err.clone());
        }
        if report.failed.is_empty() {
            self.notify(format!(
                "plugins reloaded ({} loaded{init})",
                report.loaded.len()
            ));
        } else {
            self.notify(format!(
                "plugins reloaded ({} ok, {} failed{init})",
                report.loaded.len(),
                report.failed.len()
            ));
            for (path, err) in report.failed {
                self.error(format!("plugin {}: {err}", path.display()));
            }
        }
    }

    /// Hand the App fresh plugin contributions and model choices.
    fn publish_plugin_refresh(&self, active_model: &str) {
        let rt = self.plugins.as_ref();
        let _ = self.plugin_refresh_tx.send(PluginRefresh {
            commands: rt
                .map(|rt| snapshot_plugin_commands(rt))
                .unwrap_or_default(),
            widgets: rt.map(|rt| rt.registered_widgets()).unwrap_or_default(),
            autocomplete: rt
                .map(|rt| rt.registered_autocomplete_providers())
                .unwrap_or_default(),
            models: available_model_items(&self.registry, active_model),
        });
    }
}

/// Why switching to `model` failed, with the next step for an unknown
/// model.
fn switch_error(model: &str, err: &ProviderError) -> String {
    match err {
        ProviderError::UnknownModel(_) => {
            format!("unknown model {model}. Press ctrl+p to pick one, or check the provider id.")
        }
        other => format!("cannot switch to {model}: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use kage_core::SessionId;

    use super::*;

    fn state(model: &str) -> Event {
        Event::Host(HostEvent::StateChanged {
            state: SessionState {
                model: model.into(),
                ..SessionState::default()
            },
        })
    }

    #[test]
    fn agent_sessions_never_reach_the_mirror() {
        let shared = Arc::new(Mutex::new(Mirror::new(None)));
        let mut apply = mirror(Arc::clone(&shared), None);
        let (main, child) = (SessionId::new(), SessionId::new());
        let envelope = |session, seq, event| Envelope {
            session,
            seq,
            event,
        };
        apply(&envelope(main, 1, state("main:m")));
        let spawned = Event::Host(HostEvent::AgentSpawned {
            parent: main,
            tool_call_id: kage_core::ToolCallId::new("call_1"),
            agent: "explore".into(),
            description: "map".into(),
            swarm: None,
            background: false,
        });
        apply(&envelope(child, 1, spawned));
        apply(&envelope(child, 2, state("agent:m")));
        assert_eq!(lock(&shared).state.model, "main:m");
    }

    #[test]
    fn unknown_model_error_names_the_model_once_and_the_fix() {
        let err = ProviderError::UnknownModel("bogus:thing".into());
        let text = switch_error("bogus:thing", &err);
        assert_eq!(text.matches("bogus:thing").count(), 1, "{text}");
        assert!(text.contains("ctrl+p"), "{text}");
    }
}
