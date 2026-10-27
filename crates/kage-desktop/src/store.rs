//! The store: the shell's handle on the [`kage_client::Client`].
//!
//! The store owns the client, mirrors transport events into it, runs
//! the initialize gate, and drives the boot flow: handshake first,
//! then one session, then the replay prompt when playing the
//! recording. Commands it cannot carry out itself come back to the
//! shell, which owns the transport. Views read the store and call its
//! command methods; they never see frames.
//!
//! With egui the shell drains the store once per frame, so every
//! take-style accessor ([`Store::take_commands`],
//! [`Store::take_outgoing`], [`Store::take_notes`]) drains without
//! a notify and without re-queuing: a mutation that queues frames
//! leaves them on the client until the next drain, which the shell
//! performs on every frame.

use kage_client::wire::{
    ContentBlock, FsListResult, FsOp, FsReadResult, NoticeTone, PermissionOption,
    PermissionOptionKind, SessionConfigOption,
};
use kage_client::{
    Change, Client, Frame, PermissionAsk, PromptOutcome, RequestId, Session, SteerError,
    TranscriptItem,
};

use crate::gate::{self, Report};
use crate::modes::{PLAN_MODE, active_mode};
use crate::prefs::Prefs;
use std::collections::HashMap;

use crate::timing::{SessionTimes, Timings};
use crate::transport::{Link, State};

/// A directory read's answer: the providers it lists, or why it failed.
pub type DirectoryRead = Result<Vec<kage_client::wire::DirectoryProvider>, String>;

/// The folders one folder holds, as the engine listed them, or why it
/// could not.
pub type FoldersRead = Result<kage_client::wire::FoldersResult, String>;

/// A plugin registry read's answer: the registry's packages, or why it
/// could not be read.
pub type RegistryRead = Result<kage_client::wire::RegistrySearchResult, String>;

/// A registry install's answer: what was installed, or why not.
pub type RegistryInstall = Result<kage_client::wire::RegistryInstallResult, String>;

/// What the store asks the shell to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run the handshake: `initialize`, and `session/load` for every
    /// session already open when the link came back.
    Handshake {
        /// True when this is a reconnect, so open sessions replay.
        replay_sessions: bool,
    },
    /// Open the first session of a fresh connection.
    NewSession,
    /// Send the prompt the recording expects, once its session is
    /// open.
    ReplayPrompt,
}

/// A message for the user the shell toasts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Severity.
    pub tone: NoticeTone,
    /// The message.
    pub text: String,
    /// The archived session a click on the toast restores.
    pub undo_archive: Option<String>,
}

impl Note {
    fn new(tone: NoticeTone, text: String) -> Self {
        Self {
            tone,
            text,
            undo_archive: None,
        }
    }
}

/// What a fork the user asked for does once its copy is made.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ForkPlan {
    /// Open the copy.
    Fork,
    /// Open the copy in place of the source: the copy takes the
    /// source's title, the source keeps the old state under a "(before
    /// rewind)" title, and the rewound prompt goes back to the composer.
    Rewind {
        /// The source's title, which the copy takes.
        title: Option<String>,
        /// The prompt the rewind discarded.
        prompt: String,
    },
}

/// The tool whose open ask is the plan mode review.
pub(crate) const EXIT_PLAN_TOOL: &str = "exit_plan";

/// The open plan review of a session: the exit plan ask and the
/// options it offers for each answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanReviewState {
    /// The id the decision answers.
    pub request_id: RequestId,
    /// The `exit_plan` call under review.
    pub call_id: String,
    /// The offered approve option, when the ask offers one.
    pub approve: Option<String>,
    /// The offered revise option, when the ask offers one.
    pub revise: Option<String>,
    /// The offered reject option, when the ask offers one.
    pub reject: Option<String>,
}

/// The first offered option whose id or name is `name`, case
/// insensitively. A review ask carries two reject-kind options, so
/// the kind alone cannot tell revise from reject; the option ids the
/// agent reads decisions from can.
fn option_named(options: &[PermissionOption], name: &str) -> Option<String> {
    options
        .iter()
        .find(|option| {
            option.option_id.eq_ignore_ascii_case(name) || option.name.eq_ignore_ascii_case(name)
        })
        .map(|option| option.option_id.clone())
}

/// The open plan review of a session, when one is pending.
#[must_use]
pub(crate) fn plan_review(session: &Session) -> Option<PlanReviewState> {
    let ask = session
        .permissions
        .iter()
        .find(|ask| ask.tool_call.title.as_deref() == Some(EXIT_PLAN_TOOL))?;
    let approve = match ask.option_of(PermissionOptionKind::AllowOnce) {
        Some(id) => Some(id.to_owned()),
        None => option_named(&ask.options, "approve"),
    };
    Some(PlanReviewState {
        request_id: ask.request_id.clone(),
        call_id: ask.tool_call.tool_call_id.clone(),
        approve,
        revise: option_named(&ask.options, "revise"),
        reject: option_named(&ask.options, "reject"),
    })
}

/// An answer to a plan review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanChoice {
    /// Build with the plan.
    Approve,
    /// Rework the plan with the given text.
    Revise,
    /// Drop the plan and leave plan mode.
    Reject,
}

/// The client state plus everything the shell and views read around
/// it.
#[derive(Debug)]
pub struct Store {
    client: Client,
    /// Where the connection stands, as the transport last reported.
    connect: State,
    /// What the last initialize answer fell short of.
    gate: Report,
    /// Whether the user dismissed the gate banner for this answer.
    gate_dismissed: bool,
    /// Whether a Connected was seen before, so the next one is a
    /// reconnect.
    had_link: bool,
    /// Whether the boot flow already opened its first session.
    booted: bool,
    /// Whether the connection plays the recording, which wants the
    /// scripted prompt.
    replay: bool,
    /// Whether the scripted prompt already went out.
    prompted: bool,
    /// The directory sessions open in.
    cwd: String,
    /// The project the user picked for the next new session, over
    /// [`Store::cwd`].
    project: Option<String>,
    /// What this client timed itself as frames arrived.
    timings: Timings,
    /// What the transport connects to.
    link: Link,
    /// The session the transcript view follows. `None` is the welcome
    /// state, where the next prompt opens the session it rides on.
    active: Option<String>,
    /// The prompt typed on the welcome pane, waiting for the session
    /// it opens.
    pending_prompt: Option<String>,
    /// The `session/new` request the user asked for, until it answers.
    /// Only its session becomes active; a session that opens on its own
    /// leaves the welcome pane where it is.
    opening: Option<u64>,
    /// A welcome prompt whose session failed to open, for the composer
    /// to take back.
    returned_prompt: Option<String>,
    /// The options a welcome card chose, set on the session the welcome
    /// prompt opens before the prompt goes out.
    held_options: Vec<(String, String)>,
    /// Plan mode chosen on the welcome pane. It is held apart from the
    /// permission mode, which plan mode returns to.
    held_plan: bool,
    /// The main session the welcome template last came from, whose
    /// commands the welcome slash menu offers.
    template_session: Option<String>,
    /// Sessions whose transcript moved while another one showed.
    unread: std::collections::HashSet<String>,
    /// The client's own preferences.
    prefs: Prefs,
    /// Bumped on every preferences change, so the shell stores each
    /// change once.
    prefs_rev: u64,
    /// Per session, the permission mode under plan mode. The wire shows
    /// only `plan` while plan mode is on; the engine keeps the
    /// permission mode underneath, and leaving plan mode restores it.
    permissions: HashMap<String, String>,
    /// The last `_kage/fs` listing answer, held with the session it
    /// ran against for the picker that asked. A later answer replaces
    /// it.
    fs_listing: Option<(String, FsListResult)>,
    /// The `_kage/fs` read in flight: its request id with the session
    /// and path it asked, until it answers or fails. One at a time, so
    /// an answer cannot land under another read's path.
    fs_reading: Option<(u64, String, String)>,
    /// The last file read: the session, the path and the answer.
    fs_preview: Option<(String, String, FsReadResult)>,
    /// The `session/load` requests this store issued, by request id,
    /// with the session they loaded and whether a retry was spent. A
    /// failed load is retried once, then reported.
    loads: HashMap<u64, (String, bool)>,
    /// The last `_kage/config/get` answer, raw as the wire carried it.
    config: Option<serde_json::Value>,
    /// The last `_kage/options` answer: the engine options in effect.
    engine_options: Option<Vec<kage_client::wire::OptionEntry>>,
    /// The models the engine can run, from `_kage/models/list`.
    models: Option<Vec<kage_client::wire::ModelProvider>>,
    /// Config writes by request id: `None` while in flight, then
    /// whether the engine took the write.
    writes: HashMap<u64, Option<Result<(), String>>>,
    /// Provider tests by request id, once answered.
    tests: HashMap<u64, kage_client::wire::ConfigTestResult>,
    /// Directory reads by request id: `None` while in flight, then the
    /// providers, or why the read failed.
    directories: HashMap<u64, Option<DirectoryRead>>,
    /// Folder listings asked for, by request id, until they answer.
    folders: HashMap<u64, Option<FoldersRead>>,
    /// Plugin registry reads by request id: `None` while in flight.
    registry_reads: HashMap<u64, Option<RegistryRead>>,
    /// Registry installs by request id: `None` while in flight.
    registry_installs: HashMap<u64, Option<RegistryInstall>>,
    /// The user themes on the engine's machine, once listed.
    themes: Vec<kage_client::wire::UserTheme>,
    /// Forks waiting for their copy, by source session.
    forking: HashMap<String, ForkPlan>,
    /// Copies waiting to open, with their source, by copy.
    forked: HashMap<String, (String, ForkPlan)>,
    /// Messages for the user the shell toasts.
    notes: Vec<Note>,
    /// Commands waiting for the shell.
    commands: Vec<Command>,
}

impl Store {
    /// A store with no client, no link and no sessions, for a slot the
    /// frame hands out and takes back. It answers nothing and sends
    /// nothing, so it is never left in place for a whole frame.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(String::new(), false).with_link(Link::serve(""))
    }

    /// A store for a connection that opens sessions in `cwd`; `replay`
    /// selects the scripted prompt of the recording.
    #[must_use]
    pub fn new(cwd: impl Into<String>, replay: bool) -> Self {
        Self {
            client: Client::new(),
            connect: State::Connecting,
            gate: Report::default(),
            gate_dismissed: false,
            had_link: false,
            booted: false,
            replay,
            prompted: false,
            cwd: cwd.into(),
            project: None,
            timings: Timings::default(),
            link: Link::serve(""),
            active: None,
            pending_prompt: None,
            opening: None,
            returned_prompt: None,
            held_options: Vec::new(),
            held_plan: false,
            template_session: None,
            permissions: HashMap::new(),
            prefs: Prefs::default(),
            prefs_rev: 0,
            unread: std::collections::HashSet::new(),
            fs_listing: None,
            fs_reading: None,
            fs_preview: None,
            loads: HashMap::new(),
            config: None,
            engine_options: None,
            writes: HashMap::new(),
            tests: HashMap::new(),
            directories: HashMap::new(),
            folders: HashMap::new(),
            registry_reads: HashMap::new(),
            registry_installs: HashMap::new(),
            themes: Vec::new(),
            models: None,
            forking: HashMap::new(),
            forked: HashMap::new(),
            notes: Vec::new(),
            commands: Vec::new(),
        }
    }

    /// Names what the transport connects to.
    #[must_use]
    pub fn with_link(mut self, link: Link) -> Self {
        self.link = link;
        self
    }

    /// Follows the transport's link, which changes when the engine does.
    pub fn set_link(&mut self, link: Link) {
        self.link = link;
    }

    /// What the transport connects to.
    #[must_use]
    pub fn link(&self) -> &Link {
        &self.link
    }

    /// The directory a new session opens in: the project the user
    /// picked, else the shell's own, else the engine's when the shell
    /// has none, as in a browser.
    #[must_use]
    pub fn session_dir(&self) -> Option<&str> {
        if let Some(project) = &self.project {
            return Some(project);
        }
        if self.cwd.is_empty() {
            self.state().agent_cwd.as_deref()
        } else {
            Some(&self.cwd)
        }
    }

    /// What session `id` and every agent under it have spent, as far
    /// as this client heard: each agent's reported usage, or its
    /// session's own usage updates, adds to the session's. One entry
    /// per currency heard; a tree that mixes currencies has several.
    #[must_use]
    pub fn tree_costs(&self, id: &str) -> Vec<kage_client::wire::Cost> {
        let state = self.state();
        let under = |session: &str| {
            let mut parent = state.session(session).and_then(|s| s.parent.as_deref());
            while let Some(at) = parent {
                if at == id {
                    return true;
                }
                parent = state.session(at).and_then(|s| s.parent.as_deref());
            }
            false
        };
        let mut totals: Vec<kage_client::wire::Cost> = Vec::new();
        for (session_id, session) in &state.sessions {
            if session_id != id && !under(session_id) {
                continue;
            }
            let reported = session
                .parent
                .as_deref()
                .and_then(|parent| {
                    state
                        .session(parent)?
                        .agents
                        .get(session_id)?
                        .usage
                        .as_ref()
                })
                .map(|usage| kage_client::wire::Cost {
                    amount: usage.cost,
                    currency: "USD".to_owned(),
                });
            let Some(cost) = reported.as_ref().or(session.usage.cost.as_ref()) else {
                continue;
            };
            match totals.iter_mut().find(|sum| sum.currency == cost.currency) {
                Some(sum) => sum.amount += cost.amount,
                None => totals.push(cost.clone()),
            }
        }
        totals
    }

    /// What session `id` and every agent under it have spent, when it
    /// is all one currency. `None` while nothing is priced or the
    /// tree mixes currencies; [`Store::tree_costs`] has the
    /// per-currency truth.
    #[must_use]
    pub fn tree_cost(&self, id: &str) -> Option<kage_client::wire::Cost> {
        match self.tree_costs(id).as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        }
    }

    /// Picks the directory the next new session opens in; `None` goes
    /// back to the default.
    pub fn set_project(&mut self, dir: Option<String>) {
        self.project = dir.filter(|dir| !dir.trim().is_empty());
    }

    /// Every directory a recorded or open session works in, most
    /// recently used first.
    #[must_use]
    pub fn projects(&self) -> Vec<String> {
        let state = self.state();
        let mut seen: Vec<(String, Option<String>)> = Vec::new();
        let dirs =
            state
                .directory
                .iter()
                .map(|info| (info.cwd.clone(), info.updated_at.clone()))
                .chain(state.sessions.values().filter_map(|session| {
                    Some((session.cwd.clone()?, session.updated_at.clone()))
                }));
        for (dir, at) in dirs.filter(|(dir, _)| !dir.is_empty() && dir != ".") {
            match seen.iter_mut().find(|(known, _)| *known == dir) {
                Some((_, latest)) => {
                    if at > *latest {
                        *latest = at;
                    }
                }
                None => seen.push((dir, at)),
            }
        }
        seen.sort_by(|a, b| b.1.cmp(&a.1));
        seen.into_iter().map(|(dir, _)| dir).collect()
    }

    /// What this client timed of session `id` as its frames arrived.
    #[must_use]
    pub fn timings(&self, id: &str) -> Option<&SessionTimes> {
        self.timings.session(id)
    }

    /// The client state as the frames left it.
    #[must_use]
    pub fn state(&self) -> &kage_client::State {
        self.client.state()
    }

    /// Where the connection stands.
    #[must_use]
    pub fn connect(&self) -> &State {
        &self.connect
    }

    /// Whether a live link has booted: connected and initialized, not
    /// playing a recording.
    #[must_use]
    pub fn live(&self) -> bool {
        self.booted && !self.replay
    }

    /// What the last initialize answer fell short of.
    #[must_use]
    pub fn gate(&self) -> &Report {
        &self.gate
    }

    /// Whether the banner for the current gate report was dismissed.
    #[must_use]
    pub fn gate_dismissed(&self) -> bool {
        self.gate_dismissed
    }

    /// The session the transcript follows.
    #[must_use]
    pub fn active_session(&self) -> Option<&Session> {
        let id = self.active.as_deref()?;
        self.state().session(id)
    }

    /// The open asks of the active session and the subagents under it,
    /// oldest session first. Asks of other sessions wait for the user
    /// to open those, and a plan review is answered on its plan card.
    #[must_use]
    pub fn active_asks(&self) -> Vec<(&str, &PermissionAsk)> {
        let Some(active) = self.active.as_deref() else {
            return Vec::new();
        };
        let state = self.state();
        state
            .open_asks()
            .into_iter()
            .filter(|(_, ask)| ask.tool_call.title.as_deref() != Some(EXIT_PLAN_TOOL))
            .filter(|(id, _)| {
                let mut at = Some(*id);
                // A parent chain is short; the bound only guards a cycle.
                for _ in 0..32 {
                    match at {
                        Some(id) if id == active => return true,
                        Some(id) => {
                            at = state
                                .session(id)
                                .and_then(|session| session.parent.as_deref());
                        }
                        None => return false,
                    }
                }
                false
            })
            .collect()
    }

    /// The id of the session the transcript follows.
    #[must_use]
    pub fn active_id(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Follows another session. A session the state only knows from
    /// the directory is loaded first, so clicking a recorded row
    /// actually opens it.
    pub fn set_active(&mut self, id: impl Into<String>) {
        let id = id.into();
        // An agent's session belongs to its parent's card: it is read
        // there, never followed on its own.
        if self
            .state()
            .session(&id)
            .is_some_and(|s| s.parent.is_some())
        {
            return;
        }
        let known = self.state().session(&id).is_some();
        if !known {
            let cwd = self
                .state()
                .directory
                .iter()
                .find(|info| info.session_id == id)
                .map(|info| info.cwd.clone())
                .unwrap_or_else(|| self.cwd.clone());
            let (request, _) = self.client.load_session(&id, &cwd, &[]);
            self.loads.insert(request, (id.clone(), false));
        }
        self.unread.remove(&id);
        self.active = Some(id);
    }

    /// Whether session `id` moved since the user last looked at it.
    #[must_use]
    pub fn is_unread(&self, id: &str) -> bool {
        self.unread.contains(id)
    }

    /// Leaves the active session: the welcome pane shows, and the
    /// next prompt opens the session it rides on. This is what the
    /// New session control does, as the web client's does.
    pub fn show_welcome(&mut self) {
        self.active = None;
        self.pending_prompt = None;
        self.opening = None;
        self.held_options.clear();
        self.held_plan = false;
    }

    /// Holds config option `id` at `value` for the session the welcome
    /// prompt opens. A later hold of the same option replaces it.
    pub fn hold_option(&mut self, id: &str, value: &str) {
        if id == "mode" && value == PLAN_MODE {
            self.held_plan = true;
            return;
        }
        self.held_options.retain(|(held, _)| held != id);
        self.held_options.push((id.to_owned(), value.to_owned()));
    }

    /// Accepts one incoming frame and reports what moved. The gate is
    /// rechecked on every initialize answer, and the boot flow reacts
    /// to the changes it has been waiting for.
    pub fn absorb(&mut self, frame: Frame) -> Vec<Change> {
        let answered = match &frame {
            Frame::Success { id, .. } | Frame::Failure { id, .. } => id.as_number(),
            _ => None,
        };
        let succeeded = matches!(frame, Frame::Success { .. });
        let changes = self.client.handle(frame);
        if let Some(id) = answered {
            self.settle_load(id, succeeded);
        }
        if answered.is_some() && answered == self.opening {
            self.opening = None;
            self.settle_opening(&changes);
        }
        if changes.contains(&Change::Connection) {
            self.gate = gate::check(self.client.state());
            self.gate_dismissed = false;
            if !self.booted {
                self.booted = true;
                if self.replay {
                    // The recording opens its session itself and its
                    // frame ids are fixed, so nothing else rides the
                    // link.
                    self.commands.push(Command::NewSession);
                } else {
                    // The recorded sessions fill the sidebar's project
                    // groups, and a live connection lands on the
                    // welcome pane, the way the web client boots.
                    // Every project the engine recorded is listed,
                    // the ones the TUI opened included.
                    self.client.list_sessions(None, None);
                }
            }
        }
        let mut touched: Vec<&str> = changes.iter().filter_map(Change::session_id).collect();
        touched.sort_unstable();
        touched.dedup();
        for change in &changes {
            if let Change::Transcript { id } = change
                && self.active.as_deref() != Some(id.as_str())
                && self
                    .state()
                    .session(id)
                    .is_some_and(|session| session.parent.is_none() && session.opened)
            {
                self.unread.insert(id.clone());
            }
        }
        for id in touched {
            match self.client.state().session(id) {
                Some(session) => {
                    self.timings.observe(session);
                    if let Some(mode) = active_mode(session).filter(|mode| mode != PLAN_MODE) {
                        self.permissions.insert(id.to_owned(), mode);
                    }
                }
                None => {
                    // The session closed, on the menu's ask or the
                    // engine's: its measured times and unread mark go
                    // with it instead of leaking.
                    self.timings.forget(id);
                    self.unread.remove(id);
                }
            }
        }
        if let Some(active) = self.active.clone()
            && changes.contains(&Change::Session { id: active.clone() })
        {
            self.keep_template(&active);
        }
        for change in &changes {
            if let Change::Session { id } = change {
                let opened = self.state().session(id).is_some_and(|s| s.opened);
                if self.replay && !self.prompted && opened && self.active.as_deref() == Some(id) {
                    self.prompted = true;
                    self.commands.push(Command::ReplayPrompt);
                }
            }
            match change {
                Change::Forked { from, to } => self.open_fork(from, to),
                Change::Config { request, config } => {
                    self.config = Some(config.clone());
                    if let Some(outcome) = self.writes.get_mut(request) {
                        *outcome = Some(Ok(()));
                    }
                }
                Change::Failed { request, error } => {
                    if let Some(outcome) = self.writes.get_mut(request) {
                        *outcome = Some(Err(error.message.clone()));
                    }
                    if let Some(read) = self.directories.get_mut(request) {
                        *read = Some(Err(error.message.clone()));
                    }
                    if let Some(read) = self.folders.get_mut(request) {
                        *read = Some(Err(error.message.clone()));
                    }
                    if let Some(read) = self.registry_reads.get_mut(request) {
                        *read = Some(Err(error.message.clone()));
                    }
                    if let Some(install) = self.registry_installs.get_mut(request) {
                        *install = Some(Err(error.message.clone()));
                    }
                    if self
                        .fs_reading
                        .as_ref()
                        .is_some_and(|(id, ..)| id == request)
                    {
                        self.fs_reading = None;
                    }
                }
                Change::Folders { request, result } => {
                    self.folders.insert(*request, Some(Ok(result.clone())));
                }
                Change::Themes { themes } => self.themes.clone_from(themes),
                Change::ProviderDirectory { request, providers } => {
                    self.directories
                        .insert(*request, Some(Ok(providers.clone())));
                }
                Change::Plugins { request } => {
                    if let Some(outcome) = self.writes.get_mut(request) {
                        *outcome = Some(Ok(()));
                    }
                    self.ask_config();
                }
                Change::Registry { request, result } => {
                    self.registry_reads
                        .insert(*request, Some(Ok(result.clone())));
                }
                Change::RegistryInstalled { request, result } => {
                    self.registry_installs
                        .insert(*request, Some(Ok(result.clone())));
                    self.ask_config();
                }
                Change::Tested { request, result } => {
                    self.tests.insert(*request, result.clone());
                }
                Change::KeySaved { .. } => self.ask_config(),
                Change::Options { options } => self.engine_options = Some(options.clone()),
                Change::Models { providers } => self.models = Some(providers.clone()),
                Change::Session { id } => self.settle_fork(id),
                Change::Fs {
                    session_id,
                    result: kage_client::wire::FsResult::List(listing),
                } => self.merge_listing(session_id, listing),
                Change::Fs {
                    result: kage_client::wire::FsResult::Read(read),
                    ..
                } => {
                    if let Some((_, session, path)) = self.fs_reading.take() {
                        self.fs_preview = Some((session, path, read.clone()));
                    }
                }
                _ => {}
            }
        }
        changes
    }

    /// Settles the store's watch on a `session/load` it issued: a
    /// success stops watching, a first failure re-issues the load
    /// once, and a second failure says the transcript could not be
    /// restored instead of leaving an empty transcript silently.
    fn settle_load(&mut self, id: u64, succeeded: bool) {
        let Some((session, retried)) = self.loads.remove(&id) else {
            return;
        };
        if succeeded {
            return;
        }
        if retried {
            let title = self.display_title(&session);
            self.notes.push(Note::new(
                NoticeTone::Warn,
                format!("Loading \"{title}\" failed; the transcript could not be restored."),
            ));
            return;
        }
        let cwd = self
            .state()
            .directory
            .iter()
            .find(|info| info.session_id == session)
            .map(|info| info.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        let (request, _) = self.client.load_session(&session, &cwd, &[]);
        self.loads.insert(request, (session, true));
    }

    /// Lands the answer to the user's `session/new`: the session it
    /// made becomes active and takes the welcome prompt, or, when the
    /// open failed, the prompt goes back to the composer.
    fn settle_opening(&mut self, changes: &[Change]) {
        let opened = changes.iter().find_map(|change| match change {
            Change::Session { id } => Some(id.clone()),
            _ => None,
        });
        match opened {
            Some(id) => {
                let held_mode = self
                    .held_options
                    .iter()
                    .find(|(option, _)| option == "mode")
                    .map(|(_, value)| value.clone());
                for (option, value) in std::mem::take(&mut self.held_options) {
                    let offered = self.state().session(&id).is_some_and(|session| {
                        session.config_options.iter().any(|offer| {
                            offer.id == option
                                && (offer.options.is_empty()
                                    || offer.options.iter().any(|choice| choice.value == value))
                        })
                    });
                    if offered {
                        self.client.set_config_option(&id, &option, &value);
                    }
                }
                if std::mem::take(&mut self.held_plan) {
                    let mode =
                        held_mode.or_else(|| self.state().session(&id).and_then(active_mode));
                    if let Some(mode) = mode {
                        self.permissions.insert(id.clone(), mode);
                    }
                    self.client.set_config_option(&id, "mode", PLAN_MODE);
                }
                if let Some(text) = self.pending_prompt.take() {
                    let _ = self.client.prompt(&id, vec![ContentBlock::text(text)]);
                }
                self.active = Some(id);
            }
            None => self.returned_prompt = self.pending_prompt.take(),
        }
    }

    /// The welcome prompt whose session failed to open, taken once.
    pub fn take_returned_prompt(&mut self) -> Option<String> {
        self.returned_prompt.take()
    }

    /// Records a connect-state move and returns what the shell must
    /// carry out for it. A link that lands somewhere final while a
    /// run was in flight notes the loss: the answer that would have
    /// ended the run died with the link.
    pub fn set_connect(&mut self, state: State) {
        if state.is_connected() && !self.connect.is_connected() {
            let replay_sessions = self.had_link;
            self.had_link = true;
            self.commands.push(Command::Handshake { replay_sessions });
        }
        let dropping = matches!(
            self.connect,
            State::Connected | State::Connecting | State::Reconnecting { .. }
        );
        if dropping
            && matches!(state, State::Closed | State::Refused(_))
            && self
                .state()
                .sessions
                .values()
                .any(|session| session.running)
        {
            self.notes.push(Note::new(
                NoticeTone::Warn,
                "The link closed; the run in flight was lost with it.".to_owned(),
            ));
        }
        self.connect = state;
    }

    /// Runs the handshake: `initialize` always, `session/load` for
    /// every open session when the link came back.
    pub fn handshake(&mut self, replay_sessions: bool) {
        self.client.initialize(
            kage_client::wire::ClientCapabilities {
                subagents: Some(serde_json::json!({})),
                // The app runs routine work like the TUI: tools without
                // a rule run, and configured asks and MCP tools ask.
                meta: Some(kage_client::wire::ClientMeta {
                    kage: Some(kage_client::wire::KageClientCapabilities {
                        unconfigured_tools: Some(kage_client::wire::UnconfiguredTools::Allow),
                    }),
                }),
                ..kage_client::wire::ClientCapabilities::default()
            },
            Some(kage_client::wire::Implementation {
                name: env!("CARGO_PKG_NAME").to_owned(),
                title: None,
                version: Some(env!("KAGE_VERSION").to_owned()),
            }),
        );
        if replay_sessions {
            let open: Vec<(String, Option<String>)> = self
                .state()
                .sessions
                .values()
                .filter(|session| session.opened)
                .map(|session| (session.id.clone(), session.cwd.clone()))
                .collect();
            for (id, cwd) in open {
                let (request, _) =
                    self.client
                        .load_session(&id, cwd.as_deref().unwrap_or(&self.cwd), &[]);
                self.loads.insert(request, (id, false));
            }
        }
    }

    /// Opens a fresh session in the store's directory.
    pub fn new_session(&mut self) {
        let dir = self.project.clone().unwrap_or_else(|| self.cwd.clone());
        self.opening = Some(self.client.new_session(&dir, &[]));
    }

    /// Opens a fresh session to carry `text`, the prompt a welcome
    /// pane took. The prompt goes out the moment the session is live;
    /// until then [`Store::pending_prompt`] tells whether one waits.
    pub fn open_with_prompt(&mut self, text: &str) {
        self.pending_prompt = Some(text.to_owned());
        self.new_session();
    }

    /// Whether a welcome-typed prompt waits for its session.
    #[must_use]
    pub fn pending_prompt(&self) -> bool {
        self.pending_prompt.is_some()
    }

    /// Sends one text prompt to the session the transcript follows.
    /// Returns false when there is no session to prompt.
    pub fn prompt(&mut self, text: &str) -> bool {
        let Some(id) = self.active.clone() else {
            return false;
        };
        let _ = self.client.prompt(&id, vec![ContentBlock::text(text)]);
        true
    }

    /// Sends or queues `text` on the active session: plain when idle,
    /// queued while a run is in flight. Reports what became of it.
    #[must_use]
    pub fn submit(&mut self, text: &str) -> Option<PromptOutcome> {
        let id = self.active.clone()?;
        let outcome = self.client.prompt(&id, vec![ContentBlock::text(text)]);
        Some(outcome)
    }

    /// Steers the run in flight on the active session with `text`.
    /// Reports why nothing went out: no session, no run, or the agent
    /// never advertised steering.
    pub fn steer(&mut self, text: &str) -> Result<u64, SteerError> {
        let Some(id) = self.active.clone() else {
            return Err(SteerError::NotRunning);
        };
        self.client.steer(&id, vec![ContentBlock::text(text)])
    }

    /// Cancels the run of the active session. Reports whether there
    /// was a session to cancel.
    pub fn cancel(&mut self) -> bool {
        let Some(id) = self.active.clone() else {
            return false;
        };
        self.client.cancel(&id);
        true
    }

    /// Loads subagent `id`'s transcript when this client holds none of
    /// it: a child that ran before this client attached replays through
    /// `session/load`, while a live one already streams here.
    pub fn load_child(&mut self, id: &str) {
        if self
            .state()
            .session(id)
            .is_some_and(|session| !session.items.is_empty())
        {
            return;
        }
        let cwd = self
            .active_session()
            .and_then(|session| session.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        let (request, _) = self.client.load_session(id, &cwd, &[]);
        self.loads.insert(request, (id.to_owned(), false));
    }

    /// Stops session `id`: a subagent's stop button.
    pub fn cancel_session(&mut self, id: &str) {
        self.client.cancel(id);
    }

    /// Continues the given swarm children of the active session with
    /// their own task. Reports whether a request went out.
    pub fn resume_swarm(&mut self, members: &[String]) -> bool {
        let Some(session) = self.active.clone() else {
            return false;
        };
        if members.is_empty() {
            return false;
        }
        let members = members
            .iter()
            .map(|id| (id.clone(), String::new()))
            .collect();
        self.client.swarm_resume(&session, members);
        true
    }

    /// Copies the active session into a new one and opens it. With
    /// `through`, the copy keeps the transcript up to the end of that
    /// item's turn; without, all of it. A `through` past the
    /// transcript's end is refused with a note: the row the click
    /// named is gone. Reports whether a request went out.
    /// Asks the engine to save the `nth` image or clip of transcript
    /// item `ix` of the active session (the model's, or one a tool
    /// produced) into the session's directory, under `generated/`. The
    /// engine says where it went as a notice on the session.
    pub fn save_media(&mut self, ix: usize, nth: usize) {
        use base64::Engine as _;
        use sha2::Digest as _;

        let Some(id) = self.active.clone() else {
            return;
        };
        let Some((_, data)) = self.media_at(&id, ix, nth) else {
            return;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
            return;
        };
        let sha256 = hex::encode(sha2::Sha256::digest(&bytes));
        self.client.image_save(&id, &sha256, None);
    }

    /// Plays the `nth` clip of transcript item `ix` of the active
    /// session, here rather than on the engine's host.
    pub fn play_media(&mut self, ix: usize, nth: usize) {
        let Some(id) = self.active.clone() else {
            return;
        };
        if let Some((mime, data)) = self.media_at(&id, ix, nth) {
            crate::external::play(&mime, &data);
        }
    }

    /// The type and base64 bytes of the `nth` image or clip of item
    /// `ix` of session `id`: a model's media item, or a tool call's.
    fn media_at(&self, id: &str, ix: usize, nth: usize) -> Option<(String, String)> {
        match self.state().session(id)?.items.get(ix)? {
            kage_client::TranscriptItem::Media { mime, data } if nth == 0 => {
                Some((mime.clone(), data.clone()))
            }
            kage_client::TranscriptItem::ToolCall(call) => {
                let (mime, data) = *crate::ui::transcript::media_rows::tool_media(call).get(nth)?;
                Some((mime.to_owned(), data.to_owned()))
            }
            _ => None,
        }
    }

    pub fn fork(&mut self, through: Option<usize>) -> bool {
        let Some(session) = self.active_session() else {
            return false;
        };
        let before = match through {
            None => None,
            Some(index) => {
                let Some(window) = index
                    .checked_add(1)
                    .and_then(|next| session.items.get(next..))
                else {
                    self.notes.push(Note::new(
                        NoticeTone::Warn,
                        "That fork point is gone; the transcript moved on.".to_owned(),
                    ));
                    return false;
                };
                window
                    .iter()
                    .position(|item| matches!(item, TranscriptItem::User { .. }))
                    .and_then(|next| session.prompt_ref(index + 1 + next))
            }
        };
        let id = session.id.clone();
        self.forking.insert(id.clone(), ForkPlan::Fork);
        self.client.fork_session(&id, before);
        true
    }

    /// Rewinds the active session to just before the prompt at item
    /// `prompt`: a copy without it and what followed opens in the
    /// session's place, the session stays as it was under a "(before
    /// rewind)" title, and the prompt text comes back to the composer.
    /// Refused while a run is in flight. Reports whether a request went
    /// out.
    pub fn rewind(&mut self, prompt: usize) -> bool {
        let Some(session) = self.active_session() else {
            return false;
        };
        if session.running || session.in_turn {
            return false;
        }
        let Some(before) = session.prompt_ref(prompt) else {
            return false;
        };
        let id = session.id.clone();
        let plan = ForkPlan::Rewind {
            title: session.title.clone(),
            prompt: before.text.clone().unwrap_or_default(),
        };
        self.forking.insert(id.clone(), plan);
        self.client.fork_session(&id, Some(before));
        true
    }

    /// Opens the copy a fork made, refreshing the directory so it nests
    /// under its source.
    fn open_fork(&mut self, from: &str, to: &str) {
        let plan = self.forking.remove(from).unwrap_or(ForkPlan::Fork);
        self.forked.insert(to.to_owned(), (from.to_owned(), plan));
        self.client.list_sessions(None, None);
        let cwd = self
            .state()
            .session(from)
            .and_then(|session| session.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        let (request, _) = self.client.load_session(to, &cwd, &[]);
        self.loads.insert(request, (to.to_owned(), false));
        self.unread.remove(to);
        self.active = Some(to.to_owned());
    }

    /// Carries out the rest of a fork's plan once its copy opened.
    fn settle_fork(&mut self, id: &str) {
        if !self
            .state()
            .session(id)
            .is_some_and(|session| session.opened)
        {
            return;
        }
        let Some((from, plan)) = self.forked.remove(id) else {
            return;
        };
        let source = self.display_title(&from);
        match plan {
            ForkPlan::Fork => self.notes.push(Note::new(
                NoticeTone::Success,
                format!("Forked \"{source}\""),
            )),
            ForkPlan::Rewind { title, prompt } => {
                let kept = format!("{} (before rewind)", title.as_deref().unwrap_or(&source));
                if let Some(title) = &title {
                    self.client.rename_session(id, title);
                }
                self.client.rename_session(&from, &kept);
                self.returned_prompt = Some(prompt);
                self.notes.push(Note::new(
                    NoticeTone::Success,
                    format!("Rewound; the old state is \"{kept}\""),
                ));
            }
        }
    }

    /// Names the active session. Blank titles are ignored. Reports
    /// whether a request went out.
    pub fn rename(&mut self, title: &str) -> bool {
        let title = title.trim();
        let Some(id) = self.active.clone().filter(|_| !title.is_empty()) else {
            return false;
        };
        self.client.rename_session(&id, title);
        true
    }

    /// Summarizes the active session's older turns now. Refused while a
    /// run is in flight. Reports whether a request went out.
    pub fn compact(&mut self) -> bool {
        let Some(session) = self.active_session() else {
            return false;
        };
        if session.running || session.in_turn {
            return false;
        }
        let id = session.id.clone();
        self.client.compact_session(&id);
        true
    }

    /// Asks for the active session as Markdown; the text arrives as
    /// [`Change::Exported`]. Reports whether a request went out.
    pub fn export(&mut self) -> bool {
        let Some(id) = self.active.clone() else {
            return false;
        };
        self.client.export_session(&id);
        true
    }

    /// Releases the active session and shows the welcome pane. The
    /// session stays recorded and listed.
    pub fn close_active(&mut self) {
        let Some(id) = self.active.clone() else {
            return;
        };
        self.client.close_session(&id);
        self.client.list_sessions(None, None);
        self.show_welcome();
    }

    /// The session `id` was forked from, as the directory lists it.
    #[must_use]
    pub fn fork_parent(&self, id: &str) -> Option<&str> {
        self.state()
            .directory
            .iter()
            .find(|info| info.session_id == id)?
            .meta
            .as_ref()?
            .kage
            .as_ref()?
            .parent_session_id
            .as_deref()
    }

    /// Asks the engine for its configuration snapshot, read for the
    /// active session's directory; the answer replaces
    /// [`Store::config`].
    pub fn ask_config(&mut self) {
        match self.active.clone() {
            Some(id) => self.client.config_get_for(&id),
            None => self.client.config_get(),
        };
    }

    /// Asks the engine for the models it can run; the answer lands in
    /// [`Store::models`].
    pub fn ask_models(&mut self) {
        self.client.models_list();
    }

    /// The models the engine can run, by provider, once asked.
    #[must_use]
    pub fn models(&self) -> Option<&[kage_client::wire::ModelProvider]> {
        self.models.as_deref()
    }

    /// Stars `model` in the model picker, or unstars it.
    pub fn toggle_star(&mut self, model: &str) {
        self.update_prefs(|prefs| {
            if !prefs.starred_models.remove(model) {
                prefs.starred_models.insert(model.to_owned());
            }
        });
    }

    /// Asks the engine for its options, read for the active session's
    /// directory; the answer replaces [`Store::engine_options`].
    pub fn ask_engine_options(&mut self) {
        match self.active.clone() {
            Some(id) => self.client.options_list_for(&id),
            None => self.client.options_list(),
        };
    }

    /// The engine options as the engine last answered.
    #[must_use]
    pub fn engine_options(&self) -> Option<&[kage_client::wire::OptionEntry]> {
        self.engine_options.as_deref()
    }

    /// Stores engine option `name` in the user config; the answer
    /// refreshes [`Store::engine_options`], a refusal toasts.
    pub fn set_engine_option(&mut self, name: &str, value: serde_json::Value) {
        self.client.set_engine_option(name, value);
    }

    /// Replaces the user config entry at `path` with `value`, or removes
    /// it when `value` is `None`. The answer refreshes
    /// [`Store::config`]; [`Store::write_outcome`] reports it under the
    /// returned id.
    pub fn config_set(&mut self, path: &[&str], value: Option<serde_json::Value>) -> u64 {
        let id = self.client.config_set(self.active.as_deref(), path, value);
        self.writes.insert(id, None);
        id
    }

    /// Whether config write `id` landed, once the engine answered: the
    /// engine's refusal on failure.
    #[must_use]
    pub fn write_outcome(&self, id: u64) -> Option<&Result<(), String>> {
        self.writes.get(&id)?.as_ref()
    }

    /// Asks the engine to list `provider`'s models; the answer lands
    /// under the returned id in [`Store::test_result`].
    pub fn config_test(&mut self, provider: kage_client::wire::ProviderProbe) -> u64 {
        self.client.config_test(provider)
    }

    /// Asks the engine to connect to an MCP server or start an ACP
    /// agent; the answer lands under the returned id in
    /// [`Store::test_result`].
    pub fn config_probe(&mut self, request: &kage_client::wire::ConfigTestRequest) -> u64 {
        self.client.test(request)
    }

    /// Asks the engine for a provider directory: models.dev without
    /// `url`. The answer lands under the returned id in
    /// [`Store::directory`].
    pub fn ask_directory(&mut self, url: Option<&str>, api_key: Option<&str>) -> u64 {
        let id = self.client.providers_directory(url, api_key);
        self.directories.insert(id, None);
        id
    }

    /// Asks the engine for the folders inside `path`, the home folder
    /// without one. The answer lands under the returned id in
    /// [`Store::folders`].
    pub fn ask_folders(&mut self, path: Option<&str>) -> u64 {
        let id = self.client.folders(path);
        self.folders.insert(id, None);
        id
    }

    /// The answer to folder listing `id`, once it arrived.
    #[must_use]
    pub fn folders(&self, id: u64) -> Option<&FoldersRead> {
        self.folders.get(&id)?.as_ref()
    }

    /// The answer to directory read `id`, once it arrived.
    #[must_use]
    pub fn directory(&self, id: u64) -> Option<&DirectoryRead> {
        self.directories.get(&id)?.as_ref()
    }

    /// Installs a plugin from `source`, an `https://` URL or a path on
    /// the engine's machine; [`Store::write_outcome`] reports it under
    /// the returned id.
    pub fn install_plugin(&mut self, source: &str, replace: bool) -> u64 {
        let id = self.client.plugin_install(source, replace);
        self.writes.insert(id, None);
        id
    }

    /// Removes the installed plugin `name`; [`Store::write_outcome`]
    /// reports it under the returned id.
    pub fn remove_plugin(&mut self, name: &str) -> u64 {
        let id = self.client.plugin_remove(name);
        self.writes.insert(id, None);
        id
    }

    /// Asks the engine for the plugin registry's packages; `refresh`
    /// fetches the index again. The answer lands under the returned id
    /// in [`Store::registry`].
    pub fn ask_registry(&mut self, refresh: bool) -> u64 {
        let id = self.client.registry_search("", refresh);
        self.registry_reads.insert(id, None);
        id
    }

    /// The answer to registry read `id`, once it arrived.
    #[must_use]
    pub fn registry(&self, id: u64) -> Option<&RegistryRead> {
        self.registry_reads.get(&id)?.as_ref()
    }

    /// Installs package `name` from the plugin registry, its newest
    /// release that fits; `force` replaces what is in the way. The
    /// answer lands under the returned id in [`Store::registry_install`],
    /// and the config snapshot refreshes.
    pub fn install_package(&mut self, name: &str, force: bool) -> u64 {
        let id = self.client.registry_install(name, None, force);
        self.registry_installs.insert(id, None);
        id
    }

    /// The answer to registry install `id`, once it arrived.
    #[must_use]
    pub fn registry_install(&self, id: u64) -> Option<&RegistryInstall> {
        self.registry_installs.get(&id)?.as_ref()
    }

    /// The answer to provider test `id`, once it arrived.
    #[must_use]
    pub fn test_result(&self, id: u64) -> Option<&kage_client::wire::ConfigTestResult> {
        self.tests.get(&id)
    }

    /// Saves `key` as `provider`'s API key in the engine's credential
    /// store, or removes it when `key` is `None`. The snapshot refreshes
    /// once the engine saved it.
    pub fn save_key(&mut self, provider: &str, key: Option<String>) {
        self.client.auth_set(provider, key);
    }

    /// The last configuration snapshot the engine answered with.
    #[must_use]
    pub fn config(&self) -> Option<&serde_json::Value> {
        self.config.as_ref()
    }

    /// The user themes on the engine's machine.
    #[must_use]
    pub fn themes(&self) -> &[kage_client::wire::UserTheme] {
        &self.themes
    }

    /// Asks the engine for its user themes again, as the appearance
    /// settings open.
    pub fn ask_themes(&mut self) {
        self.client.themes();
    }

    /// The theme the config names for a System choice on a dark or a
    /// light desktop (`[ui] theme_dark` / `theme_light`), when it names
    /// one.
    #[must_use]
    pub fn system_theme(&self, dark: bool) -> Option<&str> {
        let key = if dark { "theme_dark" } else { "theme_light" };
        self.config.as_ref()?.get("ui")?.get(key)?.as_str()
    }

    /// Names `name` as the theme a System choice draws on a dark or a
    /// light desktop, or clears the pick back to the kage theme.
    pub fn set_system_theme(&mut self, dark: bool, name: Option<&str>) -> u64 {
        let key = if dark { "theme_dark" } else { "theme_light" };
        self.config_set(&["ui", key], name.map(serde_json::Value::from))
    }

    /// The theme catalog: the engine's user themes and its System picks.
    #[must_use]
    pub fn theme_catalog(&self) -> crate::themes::Catalog {
        crate::themes::Catalog {
            user: self.themes.clone(),
            dark: self.system_theme(true).map(str::to_owned),
            light: self.system_theme(false).map(str::to_owned),
        }
    }

    /// The config options the composer shows for `id`: the active
    /// session's, or on the welcome pane the last session's, or before
    /// any the ones a new session opens with, with the choices held for
    /// the next session applied.
    #[must_use]
    pub fn composer_option(&self, id: &str) -> Option<SessionConfigOption> {
        if let Some(session) = self.active_session() {
            return session
                .config_options
                .iter()
                .find(|option| option.id == id)
                .cloned();
        }
        let mut option = self
            .prefs
            .template
            .iter()
            .chain(&self.state().agent_defaults)
            .find(|option| option.id == id)
            .cloned()?;
        if let Some((_, value)) = self.held_options.iter().find(|(held, _)| held == id) {
            option.current_value.clone_from(value);
        }
        Some(option)
    }

    /// The slash commands the composer offers: the active session's, or
    /// on the welcome pane the last main session's.
    #[must_use]
    pub fn composer_commands(&self) -> &[serde_json::Value] {
        self.active
            .as_deref()
            .or(self.template_session.as_deref())
            .and_then(|id| self.state().session(id))
            .map_or(&[], |session| &session.commands)
    }

    /// Remembers the model, thinking and mode options of session `id`,
    /// the plan mode aside, for the welcome pane to offer.
    fn keep_template(&mut self, id: &str) {
        let Some(session) = self
            .state()
            .session(id)
            .filter(|s| s.opened && s.parent.is_none())
        else {
            return;
        };
        let mut template: Vec<SessionConfigOption> = session
            .config_options
            .iter()
            .filter(|option| matches!(option.id.as_str(), "model" | "thinking" | "mode" | "swarm"))
            .cloned()
            .collect();
        for option in &mut template {
            if option.id == "swarm" {
                option.current_value = "off".to_owned();
            }
            if option.id == "mode" && option.current_value == PLAN_MODE {
                option.current_value = self
                    .permissions
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| "default".to_owned());
            }
        }
        self.template_session = Some(id.to_owned());
        if !template.is_empty() {
            self.update_prefs(|prefs| prefs.template = template);
        }
    }

    /// The messages for the user since the last call, oldest first.
    pub fn take_notes(&mut self) -> Vec<Note> {
        std::mem::take(&mut self.notes)
    }

    /// Queues a message for the user, as a toast the shell raises.
    pub fn note(&mut self, tone: NoticeTone, text: impl Into<String>) {
        self.notes.push(Note::new(tone, text.into()));
    }

    /// The title session `id` shows under, from its state or the
    /// directory.
    #[must_use]
    pub fn session_title(&self, id: &str) -> Option<&str> {
        self.state()
            .session(id)
            .and_then(|session| session.title.as_deref())
            .or_else(|| {
                self.state()
                    .directory
                    .iter()
                    .find(|info| info.session_id == id)?
                    .title
                    .as_deref()
            })
    }

    /// The name session `id` shows under: its title, else its first
    /// prompt until the engine titles it, else "untitled session".
    #[must_use]
    pub fn display_title(&self, id: &str) -> String {
        if let Some(title) = self.session_title(id) {
            return title.to_owned();
        }
        self.state()
            .session(id)
            .and_then(|session| {
                session.items.iter().find_map(|item| match item {
                    TranscriptItem::User { content, .. } => {
                        content.iter().find_map(ContentBlock::as_text)
                    }
                    _ => None,
                })
            })
            .map(|text| {
                let line = text.lines().next().unwrap_or_default().trim();
                let mut short: String = line.chars().take(60).collect();
                if line.chars().count() > 60 {
                    short.push('\u{2026}');
                }
                short
            })
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "untitled session".to_owned())
    }

    /// Pins session `id` to the top of the sidebar, or unpins it.
    pub fn toggle_pin(&mut self, id: &str) {
        self.update_prefs(|prefs| {
            if !prefs.pinned.remove(id) {
                prefs.pinned.insert(id.to_owned());
            }
        });
    }

    /// Takes session `id` out of the sidebar until it is restored; the
    /// welcome shows when it was the active one. The toast that says so
    /// restores it on click.
    pub fn archive(&mut self, id: &str) {
        let title = self.display_title(id);
        self.update_prefs(|prefs| {
            prefs.archived.insert(id.to_owned());
        });
        if self.active.as_deref() == Some(id) {
            self.show_welcome();
        }
        self.notes.push(Note {
            tone: NoticeTone::Info,
            text: format!("Archived \"{title}\"; click to undo"),
            undo_archive: Some(id.to_owned()),
        });
    }

    /// Brings archived session `id` back to the sidebar.
    pub fn restore(&mut self, id: &str) {
        self.update_prefs(|prefs| {
            prefs.archived.remove(id);
        });
    }

    /// Answers the open permission ask of `session`, by request id.
    /// Reports whether the ask was found and the answer queued.
    pub fn reply_permission(
        &mut self,
        session: &str,
        request_id: RequestId,
        decision: &kage_client::PermissionDecision,
    ) -> bool {
        !self
            .client
            .reply_permission(session, request_id, decision)
            .is_empty()
    }

    /// Stores the composer draft of `session`, when the state knows
    /// it.
    pub fn set_draft(&mut self, session: Option<&str>, text: &str) {
        if let Some(id) = session {
            self.client.set_draft(id, text);
        }
    }

    /// The stored draft of `session`.
    #[must_use]
    pub fn draft(&self, session: &str) -> Option<&str> {
        self.state().draft(session)
    }

    /// Sets one config option of the active session, by option id.
    /// Reports whether a session is open to carry it.
    pub fn set_option(&mut self, id: &str, value: &str) -> bool {
        let Some(session) = self.active.clone() else {
            self.hold_option(id, value);
            return true;
        };
        if id == "mode" && value != PLAN_MODE {
            self.permissions.insert(session.clone(), value.to_owned());
        }
        self.client.set_config_option(&session, id, value);
        true
    }

    /// Answers the active session's plan review with the offered option
    /// `choice` names, exactly the id the ask offered. A revise carries
    /// its text through the client's feedback channel. Reports whether
    /// an answer went out.
    pub fn review_plan(&mut self, choice: PlanChoice, revision: Option<&str>) -> bool {
        let Some(session_id) = self.active.clone() else {
            return false;
        };
        let Some(review) = self.active_session().and_then(plan_review) else {
            return false;
        };
        let offered = match choice {
            PlanChoice::Approve => review.approve,
            PlanChoice::Revise => review.revise,
            PlanChoice::Reject => review.reject,
        };
        let Some(option_id) = offered else {
            return false;
        };
        let decision = match revision.map(str::trim).filter(|text| !text.is_empty()) {
            Some(feedback) => kage_client::PermissionDecision::Feedback {
                option_id,
                feedback: feedback.to_owned(),
            },
            None => kage_client::PermissionDecision::Option(option_id),
        };
        !self
            .client
            .reply_permission(&session_id, review.request_id, &decision)
            .is_empty()
    }

    /// Whether swarm cards draw the constellation.
    #[must_use]
    pub fn constellation(&self) -> bool {
        self.prefs.constellation
    }

    /// Starts from stored preferences.
    #[must_use]
    pub fn with_prefs(mut self, prefs: Prefs) -> Self {
        self.prefs = prefs;
        self
    }

    /// The client's own preferences.
    #[must_use]
    pub fn prefs(&self) -> &Prefs {
        &self.prefs
    }

    /// Changes the preferences; the shell stores the result.
    pub fn update_prefs(&mut self, change: impl FnOnce(&mut Prefs)) {
        let before = self.prefs.clone();
        change(&mut self.prefs);
        if self.prefs != before {
            self.prefs_rev += 1;
        }
    }

    /// The preferences revision, bumped on every change.
    #[must_use]
    pub fn prefs_rev(&self) -> u64 {
        self.prefs_rev
    }

    /// Whether the active session is in plan mode, or on the welcome
    /// pane whether the next session opens in it.
    #[must_use]
    pub fn plan_on(&self) -> bool {
        match self.active_session() {
            Some(session) => active_mode(session).is_some_and(|mode| mode == PLAN_MODE),
            None => self.held_plan,
        }
    }

    /// Whether swarm mode is on for what the composer sends to: the
    /// active session, or the session the welcome prompt opens.
    #[must_use]
    pub fn swarm_on(&self) -> bool {
        self.composer_option("swarm")
            .is_some_and(|option| option.current_value == "on")
    }

    /// The permission mode of the active session: its mode, or under
    /// plan mode the mode plan mode will return to.
    #[must_use]
    pub fn permission_mode(&self) -> Option<String> {
        let Some(session) = self.active_session() else {
            return self
                .held_options
                .iter()
                .find(|(option, _)| option == "mode")
                .map(|(_, value)| value.clone())
                .or_else(|| {
                    self.composer_option("mode")
                        .map(|option| option.current_value)
                });
        };
        let mode = active_mode(session)?;
        if mode != PLAN_MODE {
            return Some(mode);
        }
        Some(
            self.permissions
                .get(&session.id)
                .cloned()
                .unwrap_or_else(|| "default".to_owned()),
        )
    }

    /// Sets the permission mode of the active session. Under plan mode
    /// the choice waits for plan mode to end, as the engine keeps one
    /// mode on the wire.
    pub fn set_permission(&mut self, value: &str) -> bool {
        let Some(id) = self.active.clone() else {
            self.hold_option("mode", value);
            return true;
        };
        if self.plan_on() {
            self.permissions.insert(id, value.to_owned());
            return true;
        }
        self.set_option("mode", value)
    }

    /// Turns plan mode on for the active session.
    pub fn enter_plan(&mut self) -> bool {
        if self.plan_on() {
            return false;
        }
        if self.active.is_none() {
            self.held_plan = true;
            return true;
        }
        self.set_option("mode", PLAN_MODE)
    }

    /// Turns plan mode off, back to the permission mode it held.
    pub fn exit_plan(&mut self) -> bool {
        if !self.plan_on() {
            return false;
        }
        if self.active.is_none() {
            self.held_plan = false;
            return true;
        }
        let mode = self
            .permission_mode()
            .unwrap_or_else(|| "default".to_owned());
        self.set_option("mode", &mode)
    }

    /// Lists the active session's workdir through `_kage/fs`. The
    /// answer lands in [`Store::fs_listing`]. Reports whether a
    /// session is open to ask.
    pub fn fs_list(&mut self, path: &str) -> bool {
        let Some(session) = self.active.clone() else {
            return false;
        };
        self.client.fs(&session, FsOp::List, path);
        true
    }

    /// The workdir entries `_kage/fs` listed for `session` so far, every
    /// answer merged, for the mention menu and the files pane. Truncated
    /// while a listed subtree was cut short and not yet continued.
    #[must_use]
    pub fn fs_listing(&self, session: &str) -> Option<&FsListResult> {
        let (id, listing) = self.fs_listing.as_ref()?;
        (id == session).then_some(listing)
    }

    /// Folds one listing answer into the session's merged listing.
    fn merge_listing(&mut self, session: &str, listing: &FsListResult) {
        match &mut self.fs_listing {
            Some((id, merged)) if id == session => {
                for entry in &listing.entries {
                    if !merged.entries.iter().any(|known| known.path == entry.path) {
                        merged.entries.push(entry.clone());
                    }
                }
                merged.entries.sort_by(|a, b| a.path.cmp(&b.path));
                merged.truncated = listing.truncated;
            }
            _ => self.fs_listing = Some((session.to_owned(), listing.clone())),
        }
    }

    /// Reads `path` under the active session's workdir through
    /// `_kage/fs`; the answer lands in [`Store::fs_preview`] under the
    /// session and path that asked. One read at a time: a second read
    /// while one waits is refused, so an answer can never land under
    /// another read's path. Reports whether the read was asked.
    pub fn fs_read(&mut self, path: &str) -> bool {
        if self.fs_reading.is_some() {
            return false;
        }
        let Some(session) = self.active.clone() else {
            return false;
        };
        let request = self.client.fs(&session, FsOp::Read, path);
        self.fs_reading = Some((request, session, path.to_owned()));
        true
    }

    /// The last file `_kage/fs` read for `session`: its path and the
    /// answer.
    #[must_use]
    pub fn fs_preview(&self, session: &str) -> Option<(&str, &FsReadResult)> {
        let (id, path, read) = self.fs_preview.as_ref()?;
        (id == session).then_some((path.as_str(), read))
    }

    /// Removes the queued prompt at `index` of the active session
    /// before it went on the wire. Reports whether there was one.
    pub fn withdraw_queued(&mut self, index: usize) -> bool {
        let Some(session) = self.active.clone() else {
            return false;
        };
        !self.client.withdraw_queued(&session, index).is_empty()
    }

    /// Sends the queued prompt at `index` of the active session as a
    /// steer on the run in flight.
    pub fn steer_queued(&mut self, index: usize) -> Result<u64, SteerError> {
        let Some(session) = self.active.clone() else {
            return Err(SteerError::NotRunning);
        };
        self.client.steer_queued(&session, index)
    }

    /// Dismisses the gate banner until the next initialize answer.
    pub fn dismiss_gate(&mut self) {
        self.gate_dismissed = true;
    }

    /// The commands the shell must carry out, drained.
    pub fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.commands)
    }

    /// The frames to send, in order, drained from the client.
    pub fn take_outgoing(&mut self) -> Vec<Frame> {
        self.client.take_outgoing()
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Store};
    use crate::modes::next_mode_value;
    use crate::transport::State;
    use kage_client::wire::NoticeTone;
    use kage_client::{Frame, PromptOutcome, RequestId, SteerError, TranscriptItem};

    /// An initialize answer with the given version and capabilities.
    fn init_answer(version: Option<&str>, steer: bool, close: bool) -> Frame {
        init_answer_on(1, version, steer, close)
    }

    /// The same answer with an explicit id, as a re-handshake lands on a
    /// fresh one.
    fn init_answer_on(id: u64, version: Option<&str>, steer: bool, close: bool) -> Frame {
        let agent = version.map(|version| {
            serde_json::json!({
                "name": "kage", "version": version,
            })
        });
        let mut capabilities = serde_json::json!({ "steer": steer });
        if close {
            capabilities["sessionCapabilities"] = serde_json::json!({ "close": {} });
        }
        Frame::Success {
            id: RequestId::Number(id),
            result: serde_json::json!({
                "protocolVersion": 1,
                "agentCapabilities": capabilities,
                "agentInfo": agent,
            }),
        }
    }

    /// Carries out `commands` the way the shell does, so the
    /// client's requests are actually sent.
    fn run(store: &mut Store, commands: Vec<Command>) {
        for command in commands {
            match command {
                Command::Handshake { replay_sessions } => store.handshake(replay_sessions),
                Command::NewSession => store.new_session(),
                Command::ReplayPrompt => {
                    store.prompt("fix the null check");
                }
            }
        }
    }

    /// Drains and carries out the store's pending commands the way
    /// the shell does.
    fn run_commands(store: &mut Store) {
        let commands = store.take_commands();
        run(store, commands);
    }

    #[test]
    fn the_first_connect_handshakes_and_lands_on_the_welcome() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connecting);
        assert!(store.take_commands().is_empty());
        store.set_connect(State::Connected);
        assert_eq!(
            store.take_commands(),
            vec![Command::Handshake {
                replay_sessions: false
            }]
        );
        store.handshake(false);
        let outgoing = store.take_outgoing();
        assert_eq!(outgoing.len(), 1, "only initialize goes out first");
        assert!(matches!(&outgoing[0], Frame::Request { method, .. } if method == "initialize"));

        store.absorb(init_answer(Some("0.1.0"), true, true));
        assert!(store.gate().is_clean());
        assert!(
            store.take_commands().is_empty(),
            "a live connection boots into the welcome, no session"
        );
        assert_eq!(store.active_id(), None);
        let outgoing = store.take_outgoing();
        assert_eq!(outgoing.len(), 1, "the directory page is asked");
        assert!(matches!(&outgoing[0], Frame::Request { method, .. } if method == "session/list"));
        assert_eq!(store.take_outgoing(), vec![]);
    }

    #[test]
    fn a_browser_without_a_directory_lists_every_session() {
        let mut store = Store::new("", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let outgoing = store.take_outgoing();
        let Frame::Request { method, params, .. } = &outgoing[0] else {
            panic!("expected the list, got {:?}", outgoing[0]);
        };
        assert_eq!(method, "session/list");
        assert!(params.get("cwd").is_none(), "no cwd filter: {params}");
    }

    #[test]
    fn every_project_is_listed_and_one_can_be_picked() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let outgoing = store.take_outgoing();
        let Frame::Request { id, method, params } = &outgoing[0] else {
            panic!("expected the list, got {:?}", outgoing[0]);
        };
        assert_eq!(method, "session/list");
        assert!(
            params.get("cwd").is_none(),
            "a shell with a directory lists all: {params}"
        );
        store.absorb(Frame::Success {
            id: id.clone(),
            result: serde_json::json!({ "sessions": [
                { "sessionId": "a", "cwd": "/a", "updatedAt": "2026-10-01T10:00:00Z" },
                { "sessionId": "b", "cwd": "/b", "updatedAt": "2026-10-01T11:00:00Z" },
                { "sessionId": "c", "cwd": ".", "updatedAt": "2026-10-01T12:00:00Z" },
            ]}),
        });
        let _ = store.take_outgoing();
        assert_eq!(
            store.projects(),
            ["/b", "/a"],
            "newest first, no relative dirs"
        );

        store.set_project(Some("/a".into()));
        assert_eq!(store.session_dir(), Some("/a"));
        store.new_session();
        let outgoing = store.take_outgoing();
        let Frame::Request { method, params, .. } = &outgoing[0] else {
            panic!("expected session/new, got {:?}", outgoing[0]);
        };
        assert_eq!(method, "session/new");
        assert_eq!(params["cwd"], "/a");
    }

    /// Opens a session the way the shell does, answering its
    /// `session/new`, so the tests have one to work with.
    fn open_session(store: &mut Store, reply_id: u64, id: &str) {
        store.new_session();
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: RequestId::Number(reply_id),
            result: serde_json::json!({ "sessionId": id }),
        });
        let _ = store.take_outgoing();
    }

    #[test]
    fn a_reconnect_handshakes_again_and_replays_open_sessions() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");

        store.set_connect(State::Closed);
        store.set_connect(State::Connecting);
        store.set_connect(State::Connected);
        assert_eq!(
            store.take_commands(),
            vec![Command::Handshake {
                replay_sessions: true
            }]
        );
        store.handshake(true);
        let outgoing = store.take_outgoing();
        assert_eq!(outgoing.len(), 2, "initialize then session/load");
        assert!(matches!(&outgoing[0], Frame::Request { method, .. } if method == "initialize"));
        assert!(matches!(&outgoing[1], Frame::Request { method, .. } if method == "session/load"));
        match &outgoing[1] {
            Frame::Request { params, .. } => {
                assert_eq!(params["sessionId"], "s1");
            }
            other => panic!("expected the load, got {other:?}"),
        }
    }

    #[test]
    fn the_replay_flow_prompts_once_when_the_session_opens() {
        let mut store = Store::new("/w", true);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let commands = store.take_commands();
        assert_eq!(commands, vec![Command::NewSession]);
        run(&mut store, commands);
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: RequestId::Number(2),
            result: serde_json::json!({"sessionId": "s1"}),
        });
        assert_eq!(store.take_commands(), vec![Command::ReplayPrompt]);
        assert!(store.prompt("fix the null check"));
        let outgoing = store.take_outgoing();
        assert_eq!(outgoing.len(), 1);
        match &outgoing[0] {
            Frame::Request { method, params, .. } => {
                assert_eq!(method, "session/prompt");
                assert_eq!(params["prompt"][0]["text"], "fix the null check");
            }
            other => panic!("expected the prompt, got {other:?}"),
        }
        assert_eq!(store.take_commands(), vec![], "the prompt goes out once");
    }

    #[test]
    fn a_too_old_agent_raises_the_gate_but_not_the_boot() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.0.9"), false, false));
        assert!(!store.gate().is_clean());
        assert_eq!(store.gate().too_old.as_deref(), Some("0.0.9"));
        assert_eq!(store.gate().missing.len(), 2);
        assert!(!store.gate_dismissed());
        store.dismiss_gate();
        assert!(store.gate_dismissed());
        let _ = store.take_outgoing();
        assert_eq!(
            store.take_commands(),
            vec![],
            "a dirty gate still boots into the welcome, no session"
        );

        store.set_connect(State::Closed);
        store.set_connect(State::Connecting);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let frames = store.take_outgoing();
        let init_id = frames
            .iter()
            .find_map(|frame| match frame {
                Frame::Request { id, method, .. } if method == "initialize" => id.as_number(),
                _ => None,
            })
            .expect("a re-handshake sends initialize");
        store.absorb(init_answer_on(init_id, Some("0.1.0"), true, true));
        assert!(store.gate().is_clean(), "a new answer rechecks the gate");
        assert!(!store.gate_dismissed());
    }

    #[test]
    fn a_running_enter_queues_and_ctrl_enter_steers_with_the_capability() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");

        assert_eq!(
            store.submit("hello"),
            Some(PromptOutcome::Sent { request_id: 4 })
        );
        let outgoing = store.take_outgoing();
        assert!(matches!(&outgoing[0], Frame::Request { method, params, .. }
            if method == "session/prompt" && params.get("delivery").is_none()));

        assert_eq!(
            store.submit("later"),
            Some(PromptOutcome::Queued),
            "a running turn holds plain submits in the queue"
        );
        assert!(store.take_outgoing().is_empty(), "queuing sends no frame");
        assert_eq!(store.state().session("s1").unwrap().queue.len(), 1);

        assert_eq!(store.steer("hurry"), Ok(5), "ctrl-enter steers the run");
        let outgoing = store.take_outgoing();
        assert!(matches!(&outgoing[0], Frame::Request { method, params, .. }
            if method == "session/prompt" && params["delivery"] == "steer"));
    }

    #[test]
    fn ctrl_enter_without_the_capability_and_double_esc_send_the_wire_frames() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), false, true));
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");

        assert_eq!(
            store.submit("hello"),
            Some(PromptOutcome::Sent { request_id: 4 })
        );
        let _ = store.take_outgoing();
        assert_eq!(
            store.steer("hurry"),
            Err(SteerError::NotAdvertised),
            "no capability, no steer"
        );
        assert!(store.take_outgoing().is_empty());

        assert!(store.cancel());
        let outgoing = store.take_outgoing();
        assert!(matches!(&outgoing[0], Frame::Notification { method, .. }
            if method == "session/cancel"));
    }

    #[test]
    fn the_draft_survives_a_switch_away_and_back() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");
        open_session(&mut store, 4, "s2");

        store.set_active("s1");
        let active = store.active_id().map(str::to_owned);
        store.set_draft(active.as_deref(), "typed and never sent");
        store.set_active("s2");
        let active = store.active_id().map(str::to_owned);
        store.set_draft(active.as_deref(), "another session, another draft");
        store.set_active("s1");
        assert_eq!(
            store.draft("s1"),
            Some("typed and never sent"),
            "switching back restores the draft"
        );
        assert_eq!(store.draft("s2"), Some("another session, another draft"));
    }

    #[test]
    fn mode_cycles_exactly_the_advertised_values() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");
        store.absorb(Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "config_option_update",
                    "configOptions": [{
                        "id": "mode", "name": "Mode", "category": "mode",
                        "type": "select", "currentValue": "default",
                        "options": [
                            {"value": "default", "name": "Default"},
                            {"value": "ask", "name": "Ask"},
                            {"value": "allow", "name": "Allow"},
                            {"value": "deny", "name": "Deny"},
                            {"value": "plan", "name": "Plan"}
                        ]
                    }]
                },
            }),
        });

        let sent_mode = |store: &mut Store| -> Vec<String> {
            store
                .take_outgoing()
                .into_iter()
                .filter_map(|frame| match frame {
                    Frame::Request { method, params, .. }
                        if method == "session/set_config_option"
                            && params["configId"] == "mode" =>
                    {
                        params["value"].as_str().map(str::to_owned)
                    }
                    _ => None,
                })
                .collect()
        };
        // Shift+Tab walks the permission modes; plan mode is its own chip.
        for expected in ["ask", "allow", "deny", "default", "ask"] {
            let current = store.permission_mode().unwrap();
            let next = next_mode_value(store.active_session().unwrap(), &current);
            assert_eq!(next.as_deref(), Some(expected));
            assert!(store.set_permission(expected));
            assert_eq!(sent_mode(&mut store), [expected], "one frame per step");
        }

        assert!(store.enter_plan());
        assert_eq!(sent_mode(&mut store), ["plan"]);
        assert!(store.plan_on());
        assert_eq!(
            store.permission_mode().as_deref(),
            Some("ask"),
            "kept under plan"
        );
        assert!(store.set_permission("allow"));
        assert!(
            sent_mode(&mut store).is_empty(),
            "a choice under plan waits"
        );
        assert!(store.exit_plan());
        assert_eq!(
            sent_mode(&mut store),
            ["allow"],
            "plan ends on the waiting choice"
        );
    }

    #[test]
    fn the_fs_listing_holds_for_the_mention_menu() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");

        assert!(store.fs_listing("s1").is_none(), "nothing asked yet");
        assert!(store.fs_list(""));
        store.absorb(Frame::Success {
            id: RequestId::Number(4),
            result: serde_json::json!({
                "op": "list",
                "entries": [
                    {"path": "src", "kind": "directory", "size": 0},
                    {"path": "src/main.rs", "kind": "file", "size": 12}
                ],
                "truncated": false
            }),
        });
        let listing = store.fs_listing("s1").unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert!(
            store.fs_listing("other").is_none(),
            "the listing is per session"
        );
    }

    /// A welcome prompt that opens its session queues the session/new
    /// frame on the store, where the shell's per-frame drain collects
    /// it; the drain does not notify or re-queue, so a second drain
    /// finds nothing.
    #[test]
    fn a_welcome_prompt_queues_its_frame_for_the_per_frame_drain() {
        let mut store = Store::new("/w", false);
        store.open_with_prompt("fix the flake");
        let flushed = store.take_outgoing();
        assert_eq!(flushed.len(), 1, "the session/new reached the store");
        assert!(matches!(&flushed[0], Frame::Request { method, .. } if method == "session/new"));
        assert!(
            store.take_outgoing().is_empty(),
            "the drain does not re-queue what it took"
        );
    }

    /// Every store method that queues a frame leaves exactly its
    /// frames on the store for the shell's per-frame drain: one
    /// mutation, one batch, with no unrelated redraw needed.
    #[test]
    fn every_frame_queuing_method_leaves_exactly_its_frames() {
        let mut store = Store::new("/w", false);

        // A live store with one open session, the way the shell answers
        // the boot frames.
        store.set_connect(State::Connected);
        store.handshake(false);
        let mut flushed: Vec<Frame> = store.take_outgoing();
        let init_id = flushed
            .iter()
            .find_map(|frame| match frame {
                Frame::Request { id, method, .. } if method == "initialize" => id.as_number(),
                _ => None,
            })
            .expect("the handshake went out");
        store.absorb(init_answer_on(init_id, Some("0.1.0"), true, true));
        store.new_session();
        flushed.clear();
        flushed.extend(store.take_outgoing());
        let new_id = flushed
            .iter()
            .rev()
            .find_map(|frame| match frame {
                Frame::Request { id, method, .. } if method == "session/new" => Some(id.clone()),
                _ => None,
            })
            .expect("the session/new went out");
        store.absorb(Frame::Success {
            id: new_id,
            result: serde_json::json!({ "sessionId": "s1" }),
        });
        assert_eq!(store.active_id(), Some("s1"));

        type FlushCase<'a> = (&'a str, Vec<&'a str>, Box<dyn Fn(&mut Store) + 'a>);
        let cases: Vec<FlushCase> = vec![
            (
                "handshake",
                vec!["initialize"],
                Box::new(|store| store.handshake(false)),
            ),
            (
                "new_session",
                vec!["session/new"],
                Box::new(|store| store.new_session()),
            ),
            (
                "prompt",
                vec!["session/prompt"],
                Box::new(|store| assert!(store.prompt("a plain prompt"))),
            ),
            (
                "steer",
                vec!["session/prompt"],
                Box::new(|store| assert!(store.steer("join me here").is_ok())),
            ),
            (
                "config_set",
                vec!["_kage/config/set"],
                Box::new(|store| {
                    store.config_set(&["theme"], Some(serde_json::json!("dark")));
                }),
            ),
            (
                "close_active",
                vec!["session/close", "session/list"],
                Box::new(|store| store.close_active()),
            ),
        ];
        for (name, expected, run) in cases {
            flushed.clear();
            run(&mut store);
            flushed.extend(store.take_outgoing());
            let methods: Vec<&str> = flushed
                .iter()
                .filter_map(|frame| match frame {
                    Frame::Request { method, .. } => Some(method.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(methods, expected, "{name} leaves exactly its frames");
            if name == "steer" {
                let Frame::Request { params, .. } = &flushed[0] else {
                    panic!("expected the steer request, got {:?}", flushed[0]);
                };
                assert_eq!(params["delivery"], "steer");
                assert_eq!(params["prompt"][0]["text"], "join me here");
            }
        }
    }

    #[test]
    fn a_prompt_without_a_session_sends_nothing() {
        let mut store = Store::new("/w", false);
        assert!(!store.prompt("hello"));
        assert!(store.take_outgoing().is_empty());
    }

    #[test]
    fn a_welcome_prompt_opens_its_session_and_sends_when_live() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();
        assert_eq!(store.active_id(), None, "the welcome state");

        store.open_with_prompt("fix the flake");
        assert!(store.pending_prompt(), "the prompt waits for its session");
        let outgoing = store.take_outgoing();
        assert_eq!(outgoing.len(), 1);
        let Frame::Request { id, method, .. } = &outgoing[0] else {
            panic!("expected a request, got {:?}", outgoing[0]);
        };
        assert_eq!(method, "session/new");

        store.absorb(Frame::Success {
            id: id.clone(),
            result: serde_json::json!({"sessionId": "s9"}),
        });
        assert_eq!(store.active_id(), Some("s9"));
        assert!(!store.pending_prompt(), "the held prompt went out");
        let outgoing = store.take_outgoing();
        assert!(matches!(&outgoing[0], Frame::Request { method, params, .. }
            if method == "session/prompt" && params["prompt"][0]["text"] == "fix the flake"));
    }

    #[test]
    fn the_welcome_clears_and_a_second_prompt_waits_again() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");

        store.show_welcome();
        assert_eq!(store.active_id(), None, "New session shows the welcome");

        store.open_with_prompt("again");
        let outgoing = store.take_outgoing();
        let Frame::Request { id, .. } = &outgoing[0] else {
            panic!("expected a request, got {:?}", outgoing[0]);
        };
        store.absorb(Frame::Success {
            id: id.clone(),
            result: serde_json::json!({"sessionId": "s2"}),
        });
        assert_eq!(store.active_id(), Some("s2"));
        assert!(
            matches!(&store.take_outgoing()[0], Frame::Request { method, .. } if method == "session/prompt")
        );
    }

    #[test]
    fn following_a_directory_session_loads_it() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();

        store.set_active("rec-1");
        let outgoing = store.take_outgoing();
        assert_eq!(outgoing.len(), 1, "an unknown row is loaded, not assumed");
        let Frame::Request { method, params, .. } = &outgoing[0] else {
            panic!("expected a request, got {:?}", outgoing[0]);
        };
        assert_eq!(method, "session/load");
        assert_eq!(params["sessionId"], "rec-1");
        assert_eq!(params["cwd"], "/w", "the store's directory carries it");
        assert_eq!(store.active_id(), Some("rec-1"));
    }

    /// A connected store on the welcome pane.
    fn welcome_store() -> Store {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();
        store
    }

    #[test]
    fn a_provider_test_answer_lands_under_its_id() {
        let mut store = welcome_store();
        let id = store.config_test(kage_client::wire::ProviderProbe {
            id: "lab".into(),
            ..Default::default()
        });
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: RequestId::Number(id),
            result: serde_json::json!({"ok": false, "message": "refused", "millis": 1, "models": []}),
        });
        assert_eq!(
            store.test_result(id).map(|r| r.message.as_str()),
            Some("refused")
        );
    }

    #[test]
    fn a_fresh_welcome_offers_the_options_a_new_session_opens_with() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        let Frame::Success { id, mut result } = init_answer(Some("0.1.0"), true, true) else {
            unreachable!()
        };
        result["_meta"] = serde_json::json!({ "kage": { "configOptions": [{
            "id": "model", "name": "Model", "type": "select",
            "currentValue": "demo/script", "options": [],
        }] } });
        store.absorb(Frame::Success { id, result });
        let model = store.composer_option("model").expect("the default model");
        assert_eq!(model.current_value, "demo/script");
        assert!(store.composer_option("mode").is_none());
    }

    #[test]
    fn a_session_moving_in_the_background_leaves_the_welcome_alone() {
        let mut store = welcome_store();
        open_session(&mut store, 3, "s1");
        store.show_welcome();
        // A reconnect reopens every open session behind the welcome.
        store.handshake(true);
        let load = store
            .take_outgoing()
            .into_iter()
            .find_map(|frame| match frame {
                Frame::Request { id, method, .. } if method == "session/load" => Some(id.clone()),
                _ => None,
            })
            .expect("the open session reloads");
        store.absorb(Frame::Success {
            id: load,
            result: serde_json::json!({}),
        });
        assert!(store.state().session("s1").unwrap().opened);
        assert_eq!(store.active_id(), None, "the welcome stays up");
    }

    #[test]
    fn a_failed_open_hands_the_welcome_prompt_back() {
        let mut store = welcome_store();
        store.open_with_prompt("fix the flake");
        let outgoing = store.take_outgoing();
        let Frame::Request { id, .. } = &outgoing[0] else {
            panic!("expected a request, got {:?}", outgoing[0]);
        };
        store.absorb(Frame::Failure {
            id: id.clone(),
            error: kage_client::RpcError {
                code: -32603,
                message: "no provider".into(),
                data: None,
            },
        });
        assert!(!store.pending_prompt(), "nothing waits on a failed open");
        assert_eq!(store.active_id(), None);
        assert_eq!(
            store.take_returned_prompt().as_deref(),
            Some("fix the flake")
        );
        assert!(store.take_outgoing().is_empty(), "no prompt went out");
    }

    #[test]
    fn the_cost_counts_every_agent_under_the_session() {
        let mut store = welcome_store();
        open_session(&mut store, 3, "s1");
        let update = |session: &str, update: serde_json::Value| Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({ "sessionId": session, "update": update }),
        };
        let usage = |amount: f64| {
            serde_json::json!({
                "sessionUpdate": "usage_update",
                "used": 10,
                "size": 100,
                "cost": { "amount": amount, "currency": "USD" },
            })
        };
        store.absorb(update(
            "s1",
            serde_json::json!({"sessionUpdate": "subagent_update", "subagentSessionId": "c1"}),
        ));
        store.absorb(update(
            "c1",
            serde_json::json!({"sessionUpdate": "subagent_update", "subagentSessionId": "g1"}),
        ));
        store.absorb(update("s1", usage(1.0)));
        store.absorb(update("c1", usage(0.5)));
        store.absorb(update("g1", usage(0.25)));
        let total = store.tree_cost("s1").expect("priced");
        assert!((total.amount - 1.75).abs() < 1e-9, "{}", total.amount);
        assert!((store.tree_cost("c1").unwrap().amount - 0.75).abs() < 1e-9);
        store.absorb(update(
            "c1",
            serde_json::json!({
                "sessionUpdate": "subagent_update",
                "subagentSessionId": "g1",
                "usage": { "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "cost": 2.0 },
            }),
        ));
        let total = store.tree_cost("s1").expect("priced");
        assert!((total.amount - 3.5).abs() < 1e-9, "{}", total.amount);
    }

    #[test]
    fn the_card_sees_the_asks_of_the_active_tree_only() {
        let mut store = welcome_store();
        open_session(&mut store, 3, "s1");
        open_session(&mut store, 4, "s2");
        store.set_active("s1");
        store.absorb(Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({
                "sessionId": "s1",
                "update": {"sessionUpdate": "subagent_update", "subagentSessionId": "c1", "state": "running"},
            }),
        });
        let ask = |id: u64, session: &str| Frame::Request {
            id: RequestId::Number(id),
            method: "session/request_permission".into(),
            params: serde_json::json!({
                "sessionId": session,
                "toolCall": {"toolCallId": format!("call_{id}"), "title": "shell"},
                "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}],
            }),
        };
        store.absorb(ask(70, "s2"));
        store.absorb(ask(71, "c1"));
        store.absorb(ask(72, "s1"));
        let mut asked: Vec<&str> = store.active_asks().into_iter().map(|(id, _)| id).collect();
        asked.sort_unstable();
        assert_eq!(asked, ["c1", "s1"], "a child's ask shows on its parent");
        store.show_welcome();
        assert!(store.active_asks().is_empty(), "the welcome shows no card");
    }

    #[test]
    fn a_fork_past_the_end_of_the_transcript_is_refused_not_a_panic() {
        let mut store = three_prompts();
        let count = store.active_session().unwrap().items.len();
        assert!(count < 5, "the fixture is short");

        assert!(!store.fork(Some(count + 2)), "a stale index refuses");
        assert!(!store.fork(Some(usize::MAX)), "no overflow, no panic");
        assert!(
            store.take_outgoing().is_empty(),
            "a refused fork sends nothing"
        );
        let notes = store.take_notes();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].tone, NoticeTone::Warn);

        assert!(
            store.fork(Some(count - 1)),
            "the last item still forks whole"
        );
        assert!(!store.take_outgoing().is_empty());
    }

    #[test]
    fn a_second_fs_read_waits_and_answers_land_under_their_own_path() {
        let mut store = welcome_store();
        open_session(&mut store, 3, "s1");
        open_session(&mut store, 4, "s2");
        store.set_active("s1");

        assert!(store.fs_read("a.rs"));
        assert!(
            !store.fs_read("b.rs"),
            "one read at a time, so answers cannot swap paths"
        );
        let sent = requests(store.take_outgoing());
        assert_eq!(sent.len(), 1, "only the first read went out");
        let (id, method, params) = &sent[0];
        assert_eq!(method, "_kage/fs");
        assert_eq!(params["op"], "read");
        assert_eq!(params["path"], "a.rs");

        store.absorb(Frame::Failure {
            id: id.clone(),
            error: kage_client::RpcError {
                code: -32603,
                message: "gone".into(),
                data: None,
            },
        });
        assert!(store.fs_read("b.rs"), "a failed read frees the slot");
        let sent = requests(store.take_outgoing());
        let (id, ..) = sent.first().expect("the second read went out").clone();
        store.absorb(Frame::Success {
            id,
            result: serde_json::json!({"op": "read", "content": "b", "truncated": false, "binary": false}),
        });
        let (path, read) = store.fs_preview("s1").expect("the answer landed");
        assert_eq!(path, "b.rs");
        assert_eq!(read.content, "b");
        assert!(
            store.fs_preview("s2").is_none(),
            "the answer carries the session that asked"
        );
    }

    #[test]
    fn a_failed_load_retries_once_and_then_says_so() {
        let mut store = welcome_store();
        store.set_active("rec-1");
        let (id, method, params) = requests(store.take_outgoing()).remove(0);
        assert_eq!(method, "session/load");
        assert_eq!(params["sessionId"], "rec-1");

        store.absorb(Frame::Failure {
            id,
            error: kage_client::RpcError {
                code: -32603,
                message: "engine hiccup".into(),
                data: None,
            },
        });
        let sent = requests(store.take_outgoing());
        assert_eq!(sent.len(), 1, "the load is retried once");
        let (retry, method, params) = &sent[0];
        assert_eq!(method, "session/load");
        assert_eq!(params["sessionId"], "rec-1");
        store.absorb(Frame::Success {
            id: retry.clone(),
            result: serde_json::json!({}),
        });
        assert!(store.take_notes().is_empty(), "the retry landed quietly");

        store.set_active("rec-2");
        let (first, ..) = requests(store.take_outgoing()).remove(0);
        store.absorb(Frame::Failure {
            id: first,
            error: kage_client::RpcError {
                code: -32603,
                message: "down".into(),
                data: None,
            },
        });
        let (retry, ..) = requests(store.take_outgoing()).remove(0);
        store.absorb(Frame::Failure {
            id: retry,
            error: kage_client::RpcError {
                code: -32603,
                message: "still down".into(),
                data: None,
            },
        });
        assert!(requests(store.take_outgoing()).is_empty(), "no third try");
        let notes = store.take_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].tone, NoticeTone::Warn);
        assert!(
            notes[0].text.contains("could not be restored"),
            "{}",
            notes[0].text
        );
    }

    #[test]
    fn a_final_link_loss_notes_the_run_it_lost() {
        let mut store = welcome_store();
        open_session(&mut store, 3, "s1");
        store.prompt("busy");
        let _ = store.take_outgoing();

        store.set_connect(State::Reconnecting {
            attempt: 1,
            delay: std::time::Duration::from_secs(1),
        });
        assert!(
            store.take_notes().is_empty(),
            "a reconnect may resume the run"
        );
        store.set_connect(State::Closed);
        let notes = store.take_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].tone, NoticeTone::Warn);

        store.set_connect(State::Closed);
        assert!(store.take_notes().is_empty(), "the loss is reported once");
    }

    #[test]
    fn a_closed_session_leaves_no_timings_behind() {
        let mut store = three_prompts();
        assert!(store.timings("s1").is_some(), "the run left measured times");

        store.close_active();
        let sent = requests(store.take_outgoing());
        let (close, ..) = sent
            .iter()
            .find(|(_, method, _)| method == "session/close")
            .expect("the close went out");
        store.absorb(Frame::Success {
            id: close.clone(),
            result: serde_json::json!({}),
        });
        assert!(
            store.timings("s1").is_none(),
            "the closed session's times go with it"
        );
    }

    #[test]
    fn the_cost_reports_per_currency_never_one_mixed_total() {
        let mut store = welcome_store();
        open_session(&mut store, 3, "s1");
        let update = |session: &str, update: serde_json::Value| Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({ "sessionId": session, "update": update }),
        };
        store.absorb(update(
            "s1",
            serde_json::json!({"sessionUpdate": "subagent_update", "subagentSessionId": "c1"}),
        ));
        store.absorb(update("s1", usage_cost(1.0, "USD")));
        store.absorb(update("c1", usage_cost(2.0, "EUR")));

        let costs = store.tree_costs("s1");
        assert_eq!(costs.len(), 2, "two currencies, two subtotals");
        assert!(
            costs
                .iter()
                .any(|cost| cost.currency == "USD" && (cost.amount - 1.0).abs() < 1e-9)
        );
        assert!(
            costs
                .iter()
                .any(|cost| cost.currency == "EUR" && (cost.amount - 2.0).abs() < 1e-9)
        );
        assert!(
            store.tree_cost("s1").is_none(),
            "a mixed tree is not silently one total"
        );
    }

    /// A usage update carrying a cost in `currency`.
    fn usage_cost(amount: f64, currency: &str) -> serde_json::Value {
        serde_json::json!({
            "sessionUpdate": "usage_update",
            "used": 10,
            "size": 100,
            "cost": { "amount": amount, "currency": currency },
        })
    }

    #[test]
    fn a_prompt_sent_during_a_dip_survives_the_handshake() {
        let mut store = welcome_store();
        open_session(&mut store, 3, "s1");
        store.set_connect(State::Reconnecting {
            attempt: 1,
            delay: std::time::Duration::from_secs(1),
        });

        assert!(store.prompt("mid-dip"));
        let outgoing = store.take_outgoing();
        assert!(
            matches!(&outgoing[0], Frame::Request { method, .. } if method == "session/prompt"),
            "the prompt frame is produced for the shell to buffer"
        );

        store.set_connect(State::Connected);
        assert_eq!(
            store.take_commands(),
            vec![Command::Handshake {
                replay_sessions: true
            }]
        );
        store.handshake(true);
        let mut methods: Vec<String> = requests(store.take_outgoing())
            .into_iter()
            .map(|(_, method, _)| method)
            .collect();
        methods.sort();
        assert_eq!(
            methods,
            ["initialize", "session/load"],
            "the session replays"
        );
    }

    /// The requests in `frames` as (id, method, params).
    fn requests(frames: Vec<Frame>) -> Vec<(RequestId, String, serde_json::Value)> {
        frames
            .into_iter()
            .filter_map(|frame| match frame {
                Frame::Request { id, method, params } => Some((id, method, params)),
                _ => None,
            })
            .collect()
    }

    /// A store on session `s1`, titled "Parser", that ran the prompts
    /// "again", "next" and "again".
    fn three_prompts() -> Store {
        let mut store = welcome_store();
        open_session(&mut store, 3, "s1");
        store.absorb(Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({
                "sessionId": "s1",
                "update": {"sessionUpdate": "session_info_update", "title": "Parser"},
            }),
        });
        for text in ["again", "next", "again"] {
            store.prompt(text);
            let (id, ..) = requests(store.take_outgoing()).remove(0);
            store.absorb(Frame::Success {
                id,
                result: serde_json::json!({"stopReason": "end_turn"}),
            });
        }
        let _ = store.take_outgoing();
        store
    }

    /// Answers the fork request in `frames` with copy `to`, then the
    /// copy's load, and returns what the store sent meanwhile.
    fn land_fork(
        store: &mut Store,
        frames: Vec<Frame>,
        to: &str,
    ) -> Vec<(RequestId, String, serde_json::Value)> {
        let (fork, ..) = requests(frames)
            .into_iter()
            .find(|(_, method, _)| method == "_kage/session/fork")
            .expect("a fork request");
        store.absorb(Frame::Success {
            id: fork,
            result: serde_json::json!({"sessionId": to}),
        });
        let mut sent = requests(store.take_outgoing());
        let load = sent
            .iter()
            .find(|(_, method, _)| method == "session/load")
            .map(|(id, ..)| id.clone())
            .expect("the copy loads");
        store.absorb(Frame::Success {
            id: load,
            result: serde_json::json!({}),
        });
        sent.extend(requests(store.take_outgoing()));
        sent
    }

    #[test]
    fn a_fork_keeps_the_turn_it_starts_from_and_opens_the_copy() {
        let mut store = three_prompts();
        let first = store
            .active_session()
            .unwrap()
            .items
            .iter()
            .position(|item| matches!(item, TranscriptItem::User { .. }))
            .unwrap();

        assert!(store.fork(Some(first)));
        let frames = store.take_outgoing();
        let (_, _, params) = requests(frames.clone()).remove(0);
        assert_eq!(
            params["before"],
            serde_json::json!({"text": "next", "occurrence": 0})
        );
        let sent = land_fork(&mut store, frames, "s2");
        assert!(sent.iter().any(|(_, method, _)| method == "session/list"));
        assert_eq!(store.active_id(), Some("s2"));
        let notes = store.take_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(
            (notes[0].tone, notes[0].text.as_str()),
            (NoticeTone::Success, "Forked \"Parser\"")
        );
    }

    #[test]
    fn a_rewind_opens_the_copy_under_the_old_title_and_returns_the_prompt() {
        let mut store = three_prompts();
        let last = store
            .active_session()
            .unwrap()
            .items
            .iter()
            .rposition(|item| matches!(item, TranscriptItem::User { .. }))
            .unwrap();

        assert!(store.rewind(last));
        let frames = store.take_outgoing();
        let (_, _, params) = requests(frames.clone()).remove(0);
        assert_eq!(
            params["before"],
            serde_json::json!({"text": "again", "occurrence": 1})
        );
        let sent = land_fork(&mut store, frames, "s2");
        let renames: Vec<(String, String)> = sent
            .iter()
            .filter(|(_, method, _)| method == "_kage/session/rename")
            .map(|(_, _, params)| {
                (
                    params["sessionId"].as_str().unwrap().to_owned(),
                    params["title"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            renames,
            [
                ("s2".to_owned(), "Parser".to_owned()),
                ("s1".to_owned(), "Parser (before rewind)".to_owned())
            ]
        );
        assert_eq!(store.active_id(), Some("s2"));
        assert_eq!(store.take_returned_prompt().as_deref(), Some("again"));
    }

    #[test]
    fn compact_and_rewind_wait_for_the_run_and_close_keeps_the_listing() {
        let mut store = three_prompts();
        store.prompt("busy");
        let _ = store.take_outgoing();
        assert!(!store.compact(), "compact waits for the run");
        assert!(!store.rewind(0), "a rewind waits for the run");

        store.close_active();
        let methods: Vec<String> = requests(store.take_outgoing())
            .into_iter()
            .map(|(_, method, _)| method)
            .collect();
        assert_eq!(methods, ["session/close", "session/list"]);
        assert_eq!(store.active_id(), None);
    }

    #[test]
    fn the_welcome_offers_the_last_sessions_options_and_holds_a_choice() {
        let mut store = welcome_store();
        store.new_session();
        let (id, ..) = requests(store.take_outgoing()).remove(0);
        store.absorb(Frame::Success {
            id,
            result: serde_json::json!({
                "sessionId": "s1",
                "configOptions": [
                    {"id": "model", "name": "Model", "type": "select", "currentValue": "a:one",
                     "options": [{"value": "a:one", "name": "One"}, {"value": "a:two", "name": "Two"}]},
                    {"id": "swarm", "name": "Swarm", "type": "select", "currentValue": "on",
                     "options": [{"value": "off", "name": "Off"}, {"value": "on", "name": "On"}]},
                    {"id": "goal", "name": "Goal", "type": "select", "currentValue": "",
                     "options": []},
                ],
            }),
        });
        let ids: Vec<&str> = store
            .prefs()
            .template
            .iter()
            .map(|o| o.id.as_str())
            .collect();
        assert_eq!(
            ids,
            ["model", "swarm"],
            "only the options a new session starts from"
        );

        store.show_welcome();
        assert_eq!(
            store.composer_option("model").unwrap().current_value,
            "a:one"
        );
        assert!(
            store.set_option("model", "a:two"),
            "the welcome holds the choice"
        );
        assert_eq!(
            store.composer_option("model").unwrap().current_value,
            "a:two"
        );
        assert!(!store.swarm_on(), "a new session starts with swarm off");
        assert!(
            requests(store.take_outgoing()).is_empty(),
            "nothing goes out yet"
        );
    }

    #[test]
    fn plan_on_the_welcome_keeps_the_permission_mode() {
        let mut store = welcome_store();
        store.set_permission("accept-edits");
        assert!(store.enter_plan());
        assert!(store.plan_on());
        assert_eq!(store.permission_mode().as_deref(), Some("accept-edits"));
        assert_eq!(store.held_options, [("mode".into(), "accept-edits".into())]);
        assert!(store.exit_plan());
        assert!(!store.plan_on());
        store.enter_plan();

        store.new_session();
        let (id, ..) = requests(store.take_outgoing()).remove(0);
        store.absorb(Frame::Success {
            id,
            result: serde_json::json!({
                "sessionId": "s1",
                "configOptions": [
                    {"id": "mode", "name": "Mode", "type": "select", "currentValue": "default",
                     "options": [{"value": "default", "name": "Default"},
                                 {"value": "accept-edits", "name": "Accept edits"},
                                 {"value": "plan", "name": "Plan"}]},
                ],
            }),
        });
        let sent: Vec<serde_json::Value> = requests(store.take_outgoing())
            .into_iter()
            .filter(|(_, method, _)| method == "session/set_config_option")
            .map(|(_, _, params)| params["value"].clone())
            .collect();
        assert_eq!(
            sent,
            ["accept-edits", "plan"],
            "the mode, then plan over it"
        );
        assert_eq!(
            store.permissions.get("s1").map(String::as_str),
            Some("accept-edits")
        );
    }
}
