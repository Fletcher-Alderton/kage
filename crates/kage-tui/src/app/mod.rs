//! Interactive TUI event loop.
//!
//! [`App::run`] owns the [`Tui`] and a [`SharedBuffer`], polls crossterm
//! key events, drives [`InputState`], applies [`InputAction`]s to the
//! buffer, and redraws the screen ~30 times a second. Submitting a
//! prompt fires a `RunRequest` through the provided sink; the host turns
//! requests into engine commands and feeds the engine's
//! [`kage_core::protocol::Envelope`]s back through
//! [`App::set_engine_events`].

pub(crate) use std::io::Write;
pub(crate) use std::sync::mpsc::{Sender, TrySendError};
pub(crate) use std::sync::{Arc, Mutex};
pub(crate) use std::time::{Duration, Instant};

pub(crate) use kage_core::options::{OptionSource, OptionValue};
pub(crate) use kage_core::sync::lock;
pub(crate) use ratatui::crossterm::event::{Event, KeyEventKind, MouseEventKind};

pub(crate) use crate::toast::{self, SharedToasts, Toast, ToastKind};

pub(crate) use kage_core::keymap::{Lookup, Rhs};

pub(crate) use crate::cmdline::{CommandLine, CommandLineEvent};
pub(crate) use crate::cmdparse::{EmptyResolver, Resolver};
pub(crate) use crate::command::{
    ArgSource, ArgSpec, BUILTIN_COMMANDS, CommandCategory, CommandSpec, OwnedArgSpec, PluginCommand,
};
pub(crate) use crate::error::TuiError;
pub(crate) use crate::events::SharedBuffer;
pub(crate) use crate::input::{Draft, InputAction, InputState, Mode, Pane};
pub(crate) use crate::keymap::{
    self, EditState, Sequencer, Step, event_from_key, help_groups, key_from_event,
};
pub(crate) use crate::layout::split;
pub(crate) use crate::overlay::{
    ApprovalOutcome, CompletionAction, ContextAction, ContextMenu, ContextMenuOutcome,
    InputCompletion, OverlayAction, OverlayPicker, SessionTreeOverlay, SessionTreeSource,
    SettingsOverlay, file_completions, prefix_before_cursor,
};
pub(crate) use crate::picker::PickItem;
pub(crate) use crate::terminal::Tui;
pub(crate) use crate::view;

/// Lines scrolled per mouse wheel notch.
const MOUSE_SCROLL_LINES: i32 = 3;

/// Outcome of validating a command before execution.
///
/// [`CommandResult::Done`] means the command was dispatched (or the
/// command name was empty). [`CommandResult::ValidationError`] means
/// the argument schema rejected the input; the caller should keep the
/// cmdline open and display the error inline.
#[derive(Debug)]
pub(crate) enum CommandResult {
    Done(Option<AppExit>),
    ValidationError(String),
}

/// What a key resolved to once the keymap and the editor grammar saw
/// it, in the order to carry it out.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Routed {
    /// An input action from a mapping or the grammar.
    Input(InputAction),
    /// A mapping's command line, run like the `:` cmdline.
    Command(String),
    /// A mapping's Lua handler, run by the worker.
    Lua(u64),
}

/// When `KAGE_DEBUG_KEYS` is set to a non-empty value, every press is
/// appended to the file at that path (or the state directory's
/// `keys.log`, [`kage_core::config::Config::state_dir`], when the
/// value is `1`). Lets us diagnose terminal-specific quirks
/// like "Shift+Enter doesn't transmit" without instrumenting the host.
fn log_key_event(key: &ratatui::crossterm::event::KeyEvent) {
    let Ok(value) = std::env::var("KAGE_DEBUG_KEYS") else {
        return;
    };
    if value.is_empty() {
        return;
    }
    let path = if value == "1" {
        let Some(state) = kage_core::config::Config::state_dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&state);
        state.join("keys.log")
    } else {
        std::path::PathBuf::from(value)
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(
            f,
            "{:?}  modifiers={:?}  kind={:?}",
            key.code, key.modifiers, key.kind
        );
    }
}

/// Request the host should act on. Either the user submitted a prompt
/// (the host runs the agent loop in a worker thread), the user asked
/// to cancel the in-flight turn, the user picked a different model,
/// or the user picked a prior session to resume into the current TUI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunRequest {
    /// New user prompt to submit to the agent loop, with any images
    /// the user attached (pasted/dragged path, `:attach`, or OS
    /// clipboard). The worker turns these into `Content::Image`
    /// blocks on the outgoing user message.
    Submit {
        /// Prompt text (may be empty if only images were attached).
        text: String,
        /// Queued image attachments, in attach order.
        images: Vec<crate::image::AttachedImage>,
        /// While a run is in flight: `true` delivers the prompt after
        /// the run ends, `false` steers it into the run at the next
        /// turn boundary. Ignored while idle.
        queue: bool,
        /// The agent session the prompt goes to. `None` sends it to the
        /// main session.
        session: Option<kage_core::SessionId>,
    },
    /// Trip the agent loop's cancellation flag.
    Cancel {
        /// The agent session to stop, with the agents under it. `None`
        /// stops the main session and every agent under it.
        session: Option<kage_core::SessionId>,
    },
    /// Take the newest pending prompt of `session` for `delivery` back
    /// out of the engine queue, to edit and resubmit it. The engine's
    /// [`kage_core::protocol::HostEvent::PromptWithdrawn`] names what
    /// actually came out.
    RecallPrompt {
        /// The agent session the prompt waits for, `None` for the main
        /// session.
        session: Option<kage_core::SessionId>,
        /// Which queue the prompt waits in.
        delivery: kage_core::protocol::Delivery,
    },
    /// Switch to a different `provider/model` for subsequent turns.
    SwitchModel(String),
    /// Replay the session at the given path into the conversation
    /// buffer and pre-load its history into the agent context. The
    /// next [`RunRequest::Submit`] continues from that history.
    ResumeSession(std::path::PathBuf),
    /// Invoke a plugin-registered command by name with the trailing
    /// argument string. The host runs it on the worker thread (so the
    /// main thread keeps painting) and pushes its output as a custom
    /// block.
    InvokePluginCommand {
        /// Plugin command name (without the leading `/` or `:`).
        name: String,
        /// Whatever followed the command on the cmdline; an empty
        /// string when the command takes no arguments.
        args: String,
    },
    /// Force a compaction pass right now, regardless of token budget.
    /// The worker runs `maybe_compact` with the threshold lowered so it
    /// fires unconditionally, then emits the resulting `Compaction`
    /// event through the buffer/session hooks like an automatic pass.
    CompactNow,
    /// Plugin-initiated fork. `at` is an entry-id prefix or an empty
    /// string for "latest entry". The worker copies the current
    /// session up through that entry into a fresh session file and
    /// surfaces its id as a toast. The live session is left untouched:
    /// the fork is an independent snapshot, not a reseat.
    ForkSession {
        /// Entry-id prefix the fork should stop at, or empty for the
        /// most recent entry.
        at: String,
    },
    /// Drop the last user prompt and everything after it: the worker
    /// branches the session in place, continuing from just before
    /// that prompt, so the transcript and history lose the undone
    /// exchange. The removed prompt comes back in
    /// [`kage_core::protocol::HostEvent::Undone`] for the editor.
    UndoSession,
    /// Ask for every user prompt the session still holds, answered by
    /// [`kage_core::protocol::HostEvent::PromptList`], which opens the
    /// rewind picker.
    ListPrompts,
    /// Rewind to a picked prompt: the worker branches the session in
    /// place to just before it, on any branch, and the prompt comes
    /// back in [`kage_core::protocol::HostEvent::Undone`] for the
    /// editor.
    Rewind {
        /// Entry id of the picked prompt.
        at: String,
    },
    /// Duplicate the active session to a fresh id and reseat the
    /// runtime onto the copy. Unlike [`RunRequest::ForkSession`], the
    /// original file is frozen as a snapshot and every subsequent turn
    /// appends to the clone. History, model, and usage carry over
    /// unchanged because the copy is byte-identical through the last
    /// entry.
    CloneSession,
    /// Abandon the active conversation and start a fresh, empty
    /// session. The worker plans a new session file (deferred until
    /// the first prompt, like startup), clears the agent history and
    /// token budget, wipes the rendered buffer, and reseats
    /// `session_path` onto the new file. The model and system prompt
    /// carry over; the prior session file is left intact on disk.
    NewSession,
    /// Render the active session transcript to a Markdown file. `None`
    /// writes `<short-session-id>.md` in the working directory; `Some`
    /// uses the given path. The worker replays the session file (the
    /// source of truth) rather than the rendered buffer.
    ExportSession(Option<std::path::PathBuf>),
    /// Open or save the newest image the model produced (`/image`).
    Image {
        /// What to do with it.
        action: kage_core::protocol::ImageAction,
        /// Where a save writes, when given.
        path: Option<std::path::PathBuf>,
    },
    /// Advance the active thinking level one step forward (the
    /// `Shift+Tab` cycle). The worker mutates the agent context's
    /// `thinking_level`, persists the change as a session entry, and
    /// fires the `thinking_level_select` plugin event.
    CycleThinkingLevel,
    /// Set the thinking level to an explicit ladder string (`off`,
    /// `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`) from the
    /// `:settings` dialog. The level rides as a raw string because
    /// kage-tui is provider-free; the worker parses it and applies
    /// exactly what [`RunRequest::CycleThinkingLevel`] applies,
    /// firing the plugin event with `"source": "settings"`. Unknown
    /// strings surface an inline error instead of changing anything.
    SetThinkingLevel(String),
    /// Run a `!`-prefixed shell-escape line. The engine executes the
    /// command in the session working directory, streams its output
    /// into a `kage:shell` block, stops it on a cancel, and adds a
    /// recorded `[shell]` user message to the history so the model
    /// sees the result on the next turn.
    RunShell(String),
    /// Rebuild the provider registry from the auth store, env vars,
    /// and plugin contributions after a `:login` changed credentials.
    /// The worker republishes the model list through the plugin
    /// refresh channel and keeps the active model if it still
    /// resolves.
    RefreshProviders,
    /// Set the session permission mode override from the `:permission`
    /// command. `Some(action)` forces every tool call through that
    /// action for the rest of the session; `None` (values `default`
    /// or `allow`) clears the override so the configured
    /// `[permissions]` rules decide again. The worker applies it to
    /// the gate, updates the modeline state, and fires
    /// `permission_mode_select`.
    SetPermissionMode(Option<kage_core::permissions::PermissionAction>),
    /// Run the Lua handler of a key mapping. The worker fetches it
    /// with `PluginRuntime::keymap_handler` and runs it through the
    /// coroutine bridge (so it may open `kage.ui.*` dialogs), like a
    /// plugin command.
    InvokeKeymap {
        /// Handler id from the mapping's `Rhs::Lua`.
        id: u64,
    },
    /// Fork the session file at the given path at its last entry into
    /// a fresh session, without reseating the runtime. Issued by the
    /// `:tree` browser's `f` so any session (not just the active one)
    /// can be branched.
    ForkSessionFile(std::path::PathBuf),
    /// Delete the session file at the given path. The worker refuses
    /// to delete the session that is currently active and surfaces a
    /// toast rather than orphaning the live writer. Issued by the
    /// `:tree` browser's `d`.
    DeleteSession(std::path::PathBuf),
    /// Plugin-initiated reseat from the `session_write` capability.
    /// `Session` resumes an existing session; `PendingFork` forks the
    /// live session at the carried entry then lands on the new branch
    /// (the rewind move). The worker consults the
    /// `session_before_switch` veto, then reseats the runtime onto the
    /// target so subsequent turns continue there.
    SwitchSession(kage_plugin::SwitchTarget),
    /// Answer a permission request the engine raised.
    ResolvePermission {
        /// Request being answered.
        request_id: kage_core::protocol::RequestId,
        /// The user's decision.
        decision: PermissionDecision,
    },
    /// Answer the questions an `ask_user_question` call asked.
    AnswerQuestion {
        /// Request being answered.
        request_id: kage_core::protocol::RequestId,
        /// Per question, the labels picked or the user's own words.
        /// `None` declines.
        answers: Option<Vec<Vec<String>>>,
    },
    /// A plugin file changed on disk. The worker re-evaluates every
    /// `.lua` in the plugins directory and toasts the outcome. Chrome
    /// (`set_header`/`set_footer`), status, autocomplete, terminal
    /// hooks, and block renderers reattach automatically because they
    /// live in shared slots the runtime overwrites during load, and
    /// so does the keymap. Commands arrive through a fresh
    /// [`PluginRefresh`].
    ReloadPlugins,
    /// Restart the named MCP server of the main session, from `/mcp
    /// restart` or the `/mcp` picker.
    RestartMcp(String),
    /// Turn the main session's swarm mode on or off, from `/swarm on`
    /// or `/swarm off`. While on, the engine injects a workflow block
    /// into the next run that steers the model to split the work early
    /// and delegate it through `swarm` calls.
    SwarmMode {
        /// The mode to switch to.
        on: bool,
    },
    /// Set the goal the session works toward, from `/goal`: the engine
    /// keeps the session working until a judge finds it met. `None`
    /// clears it.
    SetGoal {
        /// What done looks like.
        goal: Option<String>,
    },
    /// Turn the session's plan mode on or off, from `/plan` or the
    /// toggle key. While on, the agent investigates without changing
    /// anything and presents a plan for review.
    PlanMode {
        /// The mode to switch to.
        on: bool,
    },
}

/// Outcome of [`App::run`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppExit {
    /// User pressed `Ctrl+Q` / `:q` to leave the TUI cleanly.
    Quit,
}

/// Which overlay picker is currently open. Determines how a pick is
/// dispatched: a model id triggers a switch, a session path triggers
/// a resume.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PickerKind {
    Model,
    Session,
    /// F3 history jump: value is the target block index.
    Jump,
    /// `/mcp`: value is the server name.
    Mcp,
    /// Rewind: value is the entry id of the picked prompt.
    Rewind,
}

/// A blocking plugin dialog the worker handed to the App to run.
///
/// A `kage.ui.*` call suspends the plugin coroutine on the worker
/// thread; the worker forwards this over a channel and parks on the
/// carried `reply`. The App hosts the matching [`crate::overlay::OverlayWidget`],
/// then sends the answer back, and the worker resumes the coroutine
/// with it. `reply` carries `Some(value)` to resume with that JSON
/// value or `None` to resume with `nil`.
pub enum PluginDialog {
    /// `kage.ui.select`: pick one of `items`.
    Select {
        /// Picker title.
        title: String,
        /// Rows to choose from, in the plugin's order.
        items: Vec<kage_plugin::SelectItem>,
        /// Channel the App answers on.
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
    /// `kage.ui.confirm`: a yes/no question. Resumes with a boolean
    /// (cancel counts as `false`).
    Confirm {
        /// Overlay title.
        title: String,
        /// Body text explaining what is being confirmed.
        message: String,
        /// Channel the App answers on.
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
    /// `kage.ui.input`: a single-line text prompt. Resumes with the
    /// entered string, or `nil` on cancel.
    Input {
        /// Prompt title.
        title: String,
        /// Optional placeholder shown while the field is empty.
        placeholder: Option<String>,
        /// Channel the App answers on.
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
    /// `kage.ui.editor`: a multi-line text editor. Resumes with the
    /// final buffer, or `nil` on cancel.
    Editor {
        /// Editor title.
        title: String,
        /// Optional initial buffer contents.
        prefill: Option<String>,
        /// Channel the App answers on.
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
}

/// A fresh plugin snapshot the worker pushes to the App after a hot
/// reload, so the `:` command palette and status-bar widgets track the
/// reloaded runtime instead of serving the pre-reload registration
/// until restart. Delivered over the channel wired by
/// [`App::set_plugin_refresh`].
pub struct PluginRefresh {
    /// Plugin commands (regulars then overrides) registered in the
    /// reloaded runtime.
    pub commands: Vec<crate::command::PluginCommand>,
    /// Status-bar widgets registered in the reloaded runtime.
    pub widgets: Vec<Arc<kage_plugin::LuaWidget>>,
    /// Autocomplete providers registered in the reloaded runtime.
    pub autocomplete: Vec<Arc<kage_plugin::LuaAutocompleteProvider>>,
    /// The full model list for the picker/autocomplete, recomputed
    /// from the current provider registry (builtin + plugin
    /// contributions). Empty only when no providers exist.
    pub models: Vec<crate::picker::PickItem>,
}

/// A login waiting for the run loop to suspend the terminal: `:login`
/// with the provider picker ([`PendingLogin::Picker`]) or a named
/// provider, or `/mcp login` for an MCP server.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum PendingLogin {
    Picker,
    Provider(String),
    Mcp(String),
}

/// Host hook that runs an interactive credential login in the real
/// terminal. Returns whether a credential was saved.
pub(crate) type LoginRunner = std::sync::Arc<dyn Fn(Option<&str>) -> bool + Send + Sync>;

/// Host hook that logs in to the named MCP server in the real terminal.
/// The error says why the login failed and never carries a secret.
pub(crate) type McpLoginRunner = std::sync::Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

pub use kage_core::protocol::PermissionDecision;

/// In-flight dialog bookkeeping: the reply channel plus how to turn an
/// [`OverlayAction`] outcome into the value the parked coroutine is
/// resumed with. One variant per `kage.ui.*` dialog kind.
enum PluginDialogState {
    /// `kage.ui.select`: the overlay resolves with a stringified item
    /// index; map it back to that item's plugin value.
    Select {
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
        items: Vec<kage_plugin::SelectItem>,
    },
    /// `kage.ui.confirm`: the overlay resolves with a JSON boolean;
    /// pass it straight through.
    Confirm {
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
    /// `kage.ui.input`: the overlay resolves with the entered string;
    /// pass it straight through.
    Input {
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
    /// `kage.ui.editor`: the overlay resolves with the final buffer;
    /// pass it straight through.
    Editor {
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
}

impl PluginDialogState {
    /// The channel the parked worker is waiting on.
    fn reply(&self) -> &std::sync::mpsc::Sender<Option<serde_json::Value>> {
        match self {
            Self::Select { reply, .. }
            | Self::Confirm { reply }
            | Self::Input { reply }
            | Self::Editor { reply } => reply,
        }
    }

    /// Value to resume the coroutine with when the overlay resolved
    /// with `value`. `None` resumes with `nil`.
    fn resolved(&self, value: &serde_json::Value) -> Option<serde_json::Value> {
        match self {
            Self::Select { items, .. } => value
                .as_str()
                .and_then(|s| s.parse::<usize>().ok())
                .and_then(|idx| items.get(idx))
                .map(|item| item.value.clone()),
            Self::Confirm { .. } => Some(serde_json::Value::Bool(value.as_bool().unwrap_or(false))),
            Self::Input { .. } | Self::Editor { .. } => Some(value.clone()),
        }
    }

    /// Value to resume the coroutine with when the user dismissed the
    /// dialog (Esc / Ctrl+C). Select resumes with `nil`; confirm
    /// resumes with `false` so the call always returns a boolean.
    fn cancelled(&self) -> Option<serde_json::Value> {
        match self {
            Self::Select { .. } | Self::Input { .. } | Self::Editor { .. } => None,
            Self::Confirm { .. } => Some(serde_json::Value::Bool(false)),
        }
    }
}

/// Closure that returns the current set of resumable sessions on
/// demand. Listing happens on the main thread when the user presses
/// `Ctrl+S`, so a fresh scan reflects any sessions written elsewhere
/// since the TUI started.
///
/// The `bool` argument is `include_all`: `false` restricts the result
/// to sessions created in the current working directory (the picker
/// default), `true` returns every session (used by `kage.session.list`
/// and `:resume` completion, and by the in-picker "all dirs" toggle).
pub type SessionLister = Box<dyn Fn(bool) -> Vec<PickItem> + Send + 'static>;

/// Reads the stored transcript of an agent session, for agents listed
/// from a resumed session's history. `None` when the file is missing
/// or unreadable.
pub type AgentLoader = Box<dyn Fn(kage_core::SessionId) -> Option<StoredAgent> + Send + 'static>;

/// An agent session's stored transcript, as an [`AgentLoader`] reads it.
#[derive(Debug, Clone, Default)]
pub struct StoredAgent {
    /// The transcript, post-compaction, as the agent's view shows it.
    pub messages: Vec<Arc<kage_core::Message>>,
    /// The messages compaction summarized out of `messages` that start
    /// agents or carry their results, oldest first.
    pub compacted_agents: Vec<Arc<kage_core::Message>>,
}

/// Sets an option with source `runtime` on behalf of a command or the
/// settings dialog. The host routes it through the plugin runtime so
/// `option_set` fires. An `Err` carries the message to show.
pub type OptionSetter = Box<dyn Fn(&str, OptionValue) -> Result<(), String> + Send + 'static>;

impl App {
    /// Unified command registry the completion engine consumes: builtin
    /// commands first, then plugin commands, then MCP prompt commands
    /// (built at registration time and stored as `&'static` refs).
    /// The byte length of the command that opens the draft, slash
    /// included, when its name or an alias is a registered command.
    fn draft_command_end(&self) -> Option<usize> {
        let rest = self.input.text().strip_prefix('/')?;
        let name = rest.split(char::is_whitespace).next()?;
        let known = !name.is_empty()
            && self
                .command_registry()
                .iter()
                .any(|spec| spec.name == name || spec.aliases.contains(&name));
        known.then_some(1 + name.len())
    }

    fn command_registry(&self) -> Vec<&'static CommandSpec> {
        let mut out: Vec<&'static CommandSpec> = BUILTIN_COMMANDS.iter().collect();
        out.extend(self.plugin_command_specs.iter().copied());
        out.extend(self.mcp_command_specs.iter().copied());
        out.extend(self.skill_specs.iter().copied());
        out
    }
}

/// Translate an [`OwnedArgSpec`] entry (declared at runtime by a
/// plugin) into a static [`ArgSpec`] by leaking the owned name, hint,
/// and choice strings. Callers feed the resulting value into a leaked
/// slice; the lifetime is permanent for the process.
fn leak_argspec(owned: &OwnedArgSpec) -> ArgSpec {
    match owned {
        OwnedArgSpec::Text {
            name,
            optional,
            hint,
        } => ArgSpec::Rest {
            name: leak_str(name),
            optional: *optional,
            hint: leak_str(hint),
        },
        OwnedArgSpec::Choice {
            name,
            values,
            optional,
        } => {
            let leaked_values: Vec<&'static str> = values.iter().map(|v| leak_str(v)).collect();
            ArgSpec::Choice {
                name: leak_str(name),
                values: Box::leak(leaked_values.into_boxed_slice()),
                optional: *optional,
            }
        }
        OwnedArgSpec::Path { name, optional } => ArgSpec::Path {
            name: leak_str(name),
            optional: *optional,
        },
        OwnedArgSpec::Session { name, optional } => ArgSpec::SessionId {
            name: leak_str(name),
            optional: *optional,
        },
        OwnedArgSpec::Flag { name } => ArgSpec::Flag {
            name: leak_str(name),
        },
    }
}

fn leak_str(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

/// [`Resolver`] backed by the live App state: model choices and
/// plugin-registered commands the user has imported, plus the bundled
/// theme list and any session lister the host provided. Paths return
/// empty.
struct AppResolver<'a> {
    models: &'a [PickItem],
    plugin_commands: &'a [(String, String)],
    sessions: Option<&'a SessionLister>,
    themes_dir: Option<&'a std::path::Path>,
}

impl Resolver for AppResolver<'_> {
    fn dynamic_choice(&self, source: &ArgSource) -> Vec<String> {
        match source {
            ArgSource::Models => self.models.iter().map(|p| p.value.clone()).collect(),
            ArgSource::Themes => crate::theme::Theme::available_names(self.themes_dir),
            ArgSource::PluginCommands => self
                .plugin_commands
                .iter()
                .map(|(n, _)| n.clone())
                .collect(),
            ArgSource::Sessions => self
                .sessions
                .map(|f| f(true))
                .unwrap_or_default()
                .into_iter()
                .map(|item| item.value)
                .collect(),
        }
    }

    fn sessions(&self) -> Vec<String> {
        self.sessions
            .map(|f| f(true))
            .unwrap_or_default()
            .into_iter()
            .map(|item| item.value)
            .collect()
    }
}

/// Runtime state for the interactive TUI loop.
pub struct App {
    /// The buffer on screen: [`Self::root_buffer`], or the buffer of the
    /// agent in [`Self::focus`]. Scroll, folds, search, selection and
    /// rendering act on it.
    buffer: SharedBuffer,
    /// The main session's transcript.
    root_buffer: SharedBuffer,
    /// The agent the view points at. `None` shows the main session.
    focus: Option<kage_core::SessionId>,
    input: InputState,
    requests: Sender<RunRequest>,
    /// Available `provider/model` ids the model picker offers. Empty
    /// when the host has not registered any models with the App.
    model_choices: Vec<PickItem>,
    /// Active modal overlay, if any. Drives both render and input
    /// routing while present.
    picker: Option<OverlayPicker>,
    /// Which picker is open, mirroring [`Self::picker`]. Used to
    /// dispatch the picked value to the right `RunRequest`.
    picker_kind: Option<PickerKind>,
    /// Scope of the open session picker: `false` (default) shows only
    /// this directory's sessions; `true` shows all. Toggled in-picker
    /// with `Ctrl+A`.
    session_scope_all: bool,
    /// Open `:settings` overlay. A modal sibling of [`Self::picker`];
    /// on resolve its edits are applied live and persisted.
    settings_overlay: Option<SettingsOverlay>,
    /// Open `:tree` session-forest browser, a modal sibling of the
    /// picker. On resolve it dispatches resume / fork / delete.
    session_tree: Option<SessionTreeOverlay>,
    /// Open `?` / `:help` keyboard reference, a modal sibling of the
    /// picker. Scroll-only; every key either scrolls or closes.
    help_overlay: Option<crate::overlay::HelpOverlay>,
    /// Open agents overlay (`Ctrl+T`, `/agents`), a modal sibling of
    /// the picker. Its rows are rebuilt from [`Self::agents`] every
    /// frame.
    agents_overlay: Option<crate::overlay::AgentsOverlay>,
    /// Open todo-list viewer (`/todo_list`), a scroll-only modal
    /// sibling of the help reference. Its rows are rebuilt from the
    /// transcript's latest `todo_list` write every frame.
    todo_list_overlay: Option<crate::overlay::TodoListOverlay>,
    /// Open right-click context menu, if any. A light modal layer:
    /// while present it owns the keyboard and intercepts mouse clicks
    /// (a click on a row runs its action, a click off it dismisses).
    context_menu: Option<ContextMenu>,
    /// Produces the session forest for `:tree`. Wired by the host;
    /// `None` disables the command.
    session_tree_source: Option<SessionTreeSource>,
    /// Provider of resumable sessions for the session picker. None
    /// disables the picker (Ctrl+S is a no-op).
    session_lister: Option<SessionLister>,
    /// The host's startup session scan, still running on its own
    /// thread while the first frames paint. One delivery fills the
    /// start card's recent sessions.
    start_sessions: Option<std::sync::mpsc::Receiver<Vec<crate::picker::PickItem>>>,
    /// Open `:` command line, if any. While present it owns key input
    /// and paints over the footer row.
    cmdline: Option<CommandLine>,
    /// Open `/` search line, if any. Reuses the [`CommandLine`]
    /// widget; painted with a `/` prefix instead of `:`.
    search_line: Option<CommandLine>,
    /// Host hook that runs an interactive credential login for the
    /// named provider (or a provider picker when [`PendingLogin::Picker`])
    /// in the real terminal. Wired by kage-cli; `None` makes
    /// `:login` a no-op. Returns whether a credential was saved.
    login_runner: Option<LoginRunner>,
    /// Host hook that logs in to an MCP server in the real terminal.
    /// Wired by kage-cli; `None` makes `/mcp login` an error.
    mcp_login_runner: Option<McpLoginRunner>,
    /// A `:login` or `/mcp login` waiting for the run loop to suspend
    /// the terminal. Set by the command handler (which has no terminal
    /// access); consumed by the loop like the external-editor chord.
    pending_login: Option<PendingLogin>,
    /// The active search pattern: the open search line's text, else
    /// the last one submitted. While set, blocks containing the
    /// pattern render with a Match emphasis and `n` / `N` walk between
    /// them.
    search_pattern: Option<String>,
    /// The pattern and view from before the open search line, which
    /// `Esc` restores.
    search_origin: Option<keys::SearchOrigin>,
    /// Cached block indices matching `search_pattern`, in buffer
    /// order. Recomputed when the pattern or buffer version changes.
    search_match_set: Vec<usize>,
    /// Buffer version snapshot used to validate `search_match_set`.
    search_match_version: u64,
    /// Pattern the cached indices were computed for.
    search_match_pattern: String,
    /// Status bar context the host populates: live model id and a
    /// short session-id pill. Held as `Arc<Mutex<...>>` so the worker
    /// thread can update them out from under the renderer (model
    /// switches mid-session).
    status_model: Option<Arc<Mutex<String>>>,
    status_session_id: Option<String>,
    /// Plugin-registered command names + descriptions for completion
    /// on the `:` line. Builtin names take precedence on collision.
    plugin_commands: Vec<(String, String)>,
    /// `(alias, canonical name)` for plugin commands. The cmdline
    /// resolves an alias to its canonical name before dispatch so the
    /// plugin runtime only ever needs to look a command up by name.
    plugin_command_aliases: Vec<(String, String)>,
    /// Canonical names of `kage.override_command` registrations. These
    /// may shadow a built-in and are dispatched ahead of it.
    plugin_command_overrides: Vec<String>,
    /// Synthetic `CommandSpec` entries built from `plugin_commands`
    /// at registration time. Stored as `&'static` via `Box::leak` so
    /// the completion engine can mix them with the static builtin
    /// registry. Cleared and re-built on every `set_plugin_commands`.
    plugin_command_specs: Vec<&'static CommandSpec>,
    /// Every `&'static CommandSpec` ever leaked for plugin and MCP
    /// prompt commands, paired with the owned [`PluginCommand`] it was
    /// built from (its description already tagged). A registration
    /// reuses a pair's spec when the incoming command is equal, so
    /// repeated hot reloads and catalog snapshots of an unchanged set
    /// do not grow the leak.
    plugin_commands_leaked: Vec<(PluginCommand, &'static CommandSpec)>,
    /// The main session's MCP servers from its latest `McpServers`
    /// snapshot. Drives `@server:` completion, prompt commands and the
    /// `/mcp` picker.
    mcp_servers: Vec<kage_core::protocol::McpServerInfo>,
    /// One `server:prompt` spec per prompt of a live server whose name
    /// no builtin or plugin command takes, rebuilt with
    /// [`Self::mcp_servers`] and on every `set_plugin_commands`.
    mcp_command_specs: Vec<&'static CommandSpec>,
    /// Loaded skills invocable as `/name args`: the body becomes the
    /// prompt. Skills flagged `disable_model_invocation` are left out.
    skills: Vec<kage_core::skills::Skill>,
    /// Synthetic `CommandSpec` entries for [`Self::skills`], leaked
    /// like the plugin ones so completion mixes them with builtins.
    skill_specs: Vec<&'static CommandSpec>,
    /// Keymap table shared with the plugin runtime, which fills it
    /// from `_defaults.lua`, plugins, `config.toml` and `init.lua`.
    /// Keys resolve against it after the modal layers and before the
    /// editor grammar.
    keymap: kage_plugin::SharedKeymap,
    /// Pending key sequence state over [`Self::keymap`].
    sequencer: Sequencer,
    /// Status-bar widgets supplied by plugins via
    /// `kage.register_widget`. Each entry's `render(width)` runs on
    /// the plugin-refresh cadence and the resulting string is painted
    /// on the right edge of the status bar.
    plugin_widgets: Vec<Arc<kage_plugin::LuaWidget>>,
    /// Cache of [`Self::plugin_widgets`] outputs. Lives on the App so
    /// [`view::StatusCtx`] can borrow it; refreshed on a coarse
    /// cadence by [`Self::refresh_plugin_widget_texts_if_due`].
    plugin_widget_texts: Vec<String>,
    /// Last time the plugin text caches were refreshed. Drives the
    /// coarse refresh cadence for widget `render` calls.
    plugin_texts_refreshed_at: Option<Instant>,
    /// Width the plugin text caches were last rendered at. A width
    /// change forces an immediate refresh so widgets do not paint at
    /// a stale width until the next tick.
    plugin_texts_width: u16,
    /// Set when plugin widgets are (re)registered; the next frame
    /// refreshes immediately instead of waiting for the tick.
    plugin_texts_dirty: bool,
    /// Set by the plugin runtime when retained plugin output changed:
    /// any output, and block renderer output.
    plugin_redraw: Option<(
        Arc<std::sync::atomic::AtomicBool>,
        Arc<std::sync::atomic::AtomicBool>,
    )>,
    /// Transient status entries populated by `kage.set_status` /
    /// `kage.clear_status`. The plugin-refresh tick snapshots the map
    /// into [`Self::plugin_status_cache`].
    plugin_status: Option<kage_plugin::SharedStatus>,
    /// Snapshot of [`Self::plugin_status`] at the last refresh tick.
    /// Owned so the view layer can borrow without holding the plugin
    /// status mutex.
    plugin_status_cache: Vec<(String, String)>,
    /// JSON view of the live session usage so `kage.context_usage()`
    /// can return up-to-date numbers. Refreshed on the same coarse
    /// tick as the plugin text caches, not per frame.
    plugin_usage: Option<kage_plugin::SharedUsage>,
    /// Pending compact request flag populated by `kage.compact()`.
    /// Drained between event polls; a non-empty `Some` dispatches a
    /// [`RunRequest::CompactNow`] to the worker.
    plugin_compact_request: Option<kage_plugin::SharedCompactRequest>,
    /// Snapshot of resumable sessions exposed to `kage.session.list`.
    /// Refreshed from [`Self::session_lister`] when
    /// [`Self::plugin_sessions_stale`] says a session file changed.
    plugin_session_list: Option<kage_plugin::SharedSessionList>,
    /// Whether a session was written, switched or started since the
    /// plugin session snapshot was taken.
    plugin_sessions_stale: bool,
    /// Pending fork-request slot populated by `kage.session.fork`.
    /// Drained between event polls; the worker performs the fork.
    plugin_fork_request: Option<kage_plugin::SharedForkRequest>,
    /// Pending reseat slot populated by the `session_write`
    /// `kage.session.switch` / `fork_to`. Drained between event polls
    /// and relayed as [`RunRequest::SwitchSession`] to the worker.
    plugin_switch_request: Option<kage_plugin::SharedSwitchRequest>,
    /// Highlight table owned by the plugin runtime. The palette is
    /// recompiled from it when its generation moves.
    highlights: Option<kage_plugin::SharedHighlights>,
    /// Generation of [`Self::highlights`] the palette was compiled at.
    highlights_generation: u64,
    /// Option store shared with the plugin runtime. Queued changes are
    /// applied by [`Self::apply_option_changes`] on every loop pass.
    options: kage_plugin::SharedOptions,
    /// Route for option sets from commands and dialogs. `None` sets
    /// [`Self::options`] directly.
    option_setter: Option<OptionSetter>,
    /// Autocomplete providers from `kage.add_autocomplete_provider`,
    /// in registration order. Consulted in reverse (last registered
    /// wins) on each prompt-input change; the first provider that
    /// returns items populates [`Self::input_completion`].
    autocomplete_providers: Vec<Arc<kage_plugin::LuaAutocompleteProvider>>,
    /// Open input autocomplete popup, if the active provider returned
    /// candidates for the current prefix. `None` when closed.
    input_completion: Option<InputCompletion>,
    /// Workdir the built-in `@file` completion lists under. `None`
    /// disables that fallback (plugin providers still work).
    completion_workdir: Option<std::path::PathBuf>,
    /// The git branch of the workdir and when it was read, so the
    /// footer rereads it every few seconds rather than every frame.
    branch: Option<(Instant, Option<String>)>,
    /// User theme directory (`~/.config/kage/themes`). Names that are
    /// not bundled are resolved to `<name>.toml` here. `None` (tests,
    /// no home) restricts theme switching to the bundled set.
    themes_dir: Option<std::path::PathBuf>,
    /// Shared raw terminal-input hooks from `kage.on_terminal_input`.
    /// Snapshotted per keystroke (so an `off` takes effect at once)
    /// and offered each key before any modal layer; a truthy return
    /// consumes the event. `None` until wired.
    terminal_hooks: Option<kage_plugin::RegisteredTerminalHooks>,
    /// Slot specs from the plugin runtime, snapshotted per frame. Each
    /// frame also reports the width and editor mode back. `None` paints
    /// the default chrome.
    slots: Option<kage_plugin::Slots>,
    /// Pending request to toggle terminal mouse capture, applied by
    /// `run` between iterations. `None` means leave the capture state
    /// as-is. The indirection exists because `run_command` can't
    /// reach `Tui` directly; only [`Self::run`] holds it.
    pending_mouse_capture: Option<bool>,
    /// In-progress mouse gesture started by a left-button press. The
    /// tuple is `(down_row, down_block_idx, dragged)`. `dragged` flips
    /// to true the first time a drag event arrives; on `Up`, a
    /// non-dragged click on the block's header row toggles its fold,
    /// while a dragged release copies the highlighted selection
    /// straight to the clipboard (same path as `y`).
    mouse_drag_anchor: Option<(u16, usize, bool)>,
    /// Active screen selection in `(virtual_row, col)` coordinates,
    /// where `virtual_row` is the index of the row across the whole
    /// rendered buffer (independent of scroll position). Painted as
    /// a bg overlay over whatever the renderer drew so it covers
    /// tool blocks and chrome equally without changing layout.
    /// Tracking in virtual-row space lets the selection survive a
    /// scroll: rows that go off-screen stay selected, and their
    /// previously-captured text remains available for yank.
    screen_selection: Option<((usize, u16), (usize, u16))>,
    /// Cell snapshots accumulated for every virtual row the user has
    /// dragged through during the current selection. Indexed by
    /// virtual row; each entry stores the row's painted chars and
    /// per-cell decoration flag as captured when it was visible.
    /// Cleared on `MouseDown` (new selection) or after a yank. Lets
    /// `y` recover the full selected text even when part of the
    /// selection has scrolled off-screen.
    captured_rows: std::collections::BTreeMap<usize, Vec<view::CapturedCell>>,
    /// The last frame's buffer snapshot, parked for the next draw.
    /// An unchanged buffer version redraws it verbatim instead of
    /// deep-cloning the live buffer again; renderer caches live in
    /// this copy and are merged back into the live buffer after
    /// every paint. `None` until the first draw.
    draw_snapshot: Option<crate::Buffer>,
    /// The live-buffer version [`Self::draw_snapshot`] was cloned
    /// at. The reuse check in [`Self::draw`] keys on it.
    draw_snapshot_version: u64,
    /// Colors the terminal shows. Every painted frame is mapped to
    /// it, so themes written in 24-bit color stay readable without
    /// truecolor.
    color_depth: crate::theme::ColorDepth,
    /// Last DECSCUSR cursor shape we emitted to the terminal, keyed
    /// by `(mode, pane_focused_on_input, modeless)`. Stored so [`Self::draw`]
    /// can skip the escape on frames where the cursor shape would be
    /// identical, avoiding a flicker on terminals that briefly hide
    /// the cursor when the style is reapplied.
    last_cursor_style: Option<(Mode, bool, bool)>,
    /// Optional shared snapshot of the current session's running
    /// token totals + context window. The footer, the input rule and
    /// the working row read it. Updated by the host worker thread
    /// after every turn.
    session_usage: Option<crate::usage::SharedSessionUsage>,
    /// Shared queue of ephemeral toast notifications painted in rows
    /// of their own below the conversation buffer. The handle is
    /// cloned to whatever sinks need to push (the App's own
    /// `notify`, the host log sink for plugin `kage.notify`, etc.).
    /// When `None`, `notify(...)` is a silent no-op: toasts are
    /// decorative and never load-bearing.
    toasts: Option<SharedToasts>,
    /// Channel the worker pushes blocking [`PluginDialog`] requests
    /// onto (`kage.ui.select`). Drained between event polls; while a
    /// dialog is open the worker thread is parked awaiting the answer.
    dialog_rx: Option<std::sync::mpsc::Receiver<PluginDialog>>,
    /// Channel the worker pushes a fresh [`PluginRefresh`] snapshot
    /// onto after a plugin hot reload. Drained between event polls;
    /// the newest snapshot re-seeds commands and status widgets.
    plugin_refresh_rx: Option<std::sync::mpsc::Receiver<PluginRefresh>>,
    /// Results of async OS-clipboard image attaches. The arboard read
    /// can block for hundreds of ms on some compositors, so it runs
    /// on a background thread and sends the decoded attachment here;
    /// the run loop drains via [`App::drain_clipboard_attach`] so the
    /// input thread never waits on the clipboard.
    attach_tx: std::sync::mpsc::Sender<Result<crate::image::AttachedImage, String>>,
    attach_rx: std::sync::mpsc::Receiver<Result<crate::image::AttachedImage, String>>,
    /// The overlay hosting the current plugin dialog, if any. A
    /// trait object so every `kage.ui.*` dialog (picker, confirm,
    /// input, editor) shares one hosting path.
    plugin_overlay: Option<Box<dyn crate::overlay::OverlayWidget>>,
    /// Bookkeeping for the dialog currently in [`Self::plugin_overlay`]:
    /// where to send the answer and how to map the overlay's outcome.
    active_dialog: Option<PluginDialogState>,
    /// Session file staged for deletion while the confirm dialog in
    /// [`Self::plugin_overlay`] asks; cleared when the answer arrives.
    pending_tree_delete: Option<std::path::PathBuf>,
    /// Engine events for the sessions this App shows. Drained between
    /// event polls.
    engine_rx: Option<std::sync::mpsc::Receiver<kage_core::protocol::Envelope>>,
    /// The session whose events the App renders. Learned from the first
    /// envelope and moved by `SessionChanged`.
    active_session: Option<kage_core::SessionId>,
    /// Every agent session started under the main session, folded from
    /// their envelopes. Cleared when the main session changes.
    agents: kage_core::protocol::AgentTree,
    /// The transcript of each agent in [`Self::agents`], fed by that
    /// agent's loop events.
    agent_buffers: std::collections::HashMap<kage_core::SessionId, SharedBuffer>,
    /// Finished agents whose buffer was dropped to free memory. Opening
    /// one reads its transcript back through [`Self::agent_loader`].
    evicted_agents: std::collections::HashSet<kage_core::SessionId>,
    /// Reads the stored transcript of an agent of a resumed session, or
    /// of a finished agent whose buffer was dropped. `None` leaves such
    /// agents listed but closed, and keeps every live agent's buffer.
    agent_loader: Option<AgentLoader>,
    /// The drafts of the views off screen, by session (`None` for the
    /// main view). The draft on screen lives in [`Self::input`].
    drafts: std::collections::HashMap<Option<kage_core::SessionId>, crate::input::Draft>,
    /// The approval panel on screen in place of the input, if any. It
    /// owns the keyboard like a modal sibling of [`Self::plugin_overlay`].
    approval_panel: Option<crate::overlay::ApprovalPanel>,
    /// The request answered by [`Self::approval_panel`].
    pending_permission: Option<engine::PendingApproval>,
    /// Permission requests waiting for the panel.
    permission_queue: std::collections::VecDeque<engine::PendingApproval>,
    /// When the run in flight started, from the working flag's
    /// transition. `None` while idle.
    run_started: Option<Instant>,
    /// The main session's compaction in flight: the turn counts its
    /// `CompactionStarted` named, until the matching `Compaction` or
    /// the run's end. `None` while no compaction runs.
    compacting: Option<(usize, usize)>,
    /// `before` tokens of the compaction card just pushed, waiting for
    /// the next usage update to name the post-compaction fill. Cleared
    /// at the run's end or a session switch, which leave the card
    /// without a delta.
    pending_compaction_before: Option<u64>,
    /// The task `/swarm <task>` submitted as a one-shot swarm: the
    /// mode turns itself off when that run ends. Cleared by an
    /// explicit `on`/`off` or a session switch.
    swarm_oneshot: Option<String>,
    /// Cached hint labels for [`Self::key_label`].
    key_labels: chrome::KeyLabels,
    /// What the start card lists. `None` until the host sets it.
    start_info: Option<view::StartInfo>,
    /// Prompts sent during a run that the engine has not delivered
    /// yet, in send order, with the agent session they went to (`None`
    /// for the main session). Only the rows of the session on screen
    /// show above the input.
    pending: Vec<(Option<kage_core::SessionId>, view::PendingPrompt)>,
    /// The screen row of each pinned agent row painted last frame, with
    /// its session, so a click on one focuses that agent.
    pinned_hits: Vec<(u16, kage_core::SessionId)>,
    /// The screen row of the pinned list's summary row, painted while
    /// no agent is live but finished ones remain, so a click on it
    /// opens the agents overlay.
    pinned_summary_hit: Option<u16>,
    /// What the last Esc or Ctrl+C left for the next press, and when
    /// it lapses.
    escalation: Option<(keys::Escalation, Instant)>,
}

mod actions;
mod chrome;
mod editor;
mod engine;
mod events;
mod keys;
mod lifecycle;
mod overlays;
mod plugin_sync;
mod wiring;

/// Translate an absolute terminal `(row, col)` mouse position to a
/// Best-effort label for the current mode, exposed for the host's
/// status-bar widget.
#[must_use]
pub fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "normal",
        Mode::Insert => "insert",
        Mode::Visual => "visual",
    }
}

/// Build the key descriptor a `kage.on_terminal_input` handler
/// receives. See `kage_plugin::terminal_input` for the schema.
fn key_event_to_json(key: ratatui::crossterm::event::KeyEvent) -> serde_json::Value {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    let mods = key.modifiers;
    let (code, ch): (String, Option<String>) = match key.code {
        KeyCode::Char(c) => ("char".to_owned(), Some(c.to_string())),
        KeyCode::F(n) => (format!("f{n}"), None),
        KeyCode::Enter => ("enter".to_owned(), None),
        KeyCode::Esc => ("esc".to_owned(), None),
        KeyCode::Tab => ("tab".to_owned(), None),
        KeyCode::BackTab => ("backtab".to_owned(), None),
        KeyCode::Backspace => ("backspace".to_owned(), None),
        KeyCode::Up => ("up".to_owned(), None),
        KeyCode::Down => ("down".to_owned(), None),
        KeyCode::Left => ("left".to_owned(), None),
        KeyCode::Right => ("right".to_owned(), None),
        KeyCode::Home => ("home".to_owned(), None),
        KeyCode::End => ("end".to_owned(), None),
        KeyCode::PageUp => ("pageup".to_owned(), None),
        KeyCode::PageDown => ("pagedown".to_owned(), None),
        KeyCode::Delete => ("delete".to_owned(), None),
        KeyCode::Insert => ("insert".to_owned(), None),
        _ => ("other".to_owned(), None),
    };
    serde_json::json!({
        "code": code,
        "char": ch,
        "ctrl": mods.contains(KeyModifiers::CONTROL),
        "alt": mods.contains(KeyModifiers::ALT),
        "shift": mods.contains(KeyModifiers::SHIFT),
    })
}

#[cfg(test)]
mod tests;
