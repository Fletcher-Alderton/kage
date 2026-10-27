//! The desktop launch: flags, transport choice and the native window.
//!
//! One window over one transport: `kage rpc` as a child by default,
//! `kage serve` over WebSocket with `--ws` and `--token`, or the
//! recorded golden transcript with `--replay`. `--smoke` quits after
//! a delay, which is how the headless runs are automated (the smoke
//! report grows with the shell, E02). The browser build replaces this
//! module with the web entry, which returns with E08.

use std::time::Duration;

use kage_client::Frame;
#[cfg(not(target_arch = "wasm32"))]
use kage_client::unquote_and_trim;

use crate::store::Command;
#[cfg(not(target_arch = "wasm32"))]
use crate::transport::stdio::{Config as StdioConfig, StdioTransport};
#[cfg(not(target_arch = "wasm32"))]
use crate::transport::ws::WsTransport;
use crate::transport::{Event, Link, Transport};

/// Which transport the shell connects through. Native only: the
/// browser's endpoint comes from `boot.js`, not a flag.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Wire {
    /// Spawn `kage rpc`, with an optional explicit binary.
    #[default]
    Stdio,
    /// Dial `kage serve` over WebSocket.
    WebSocket {
        /// The `ws://` endpoint, path included.
        url: String,
        /// The bearer token of the endpoint.
        token: String,
    },
    /// Play the recorded golden transcript.
    Replay,
}

/// Command line switches for automated runs and transport choice.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
struct Launch {
    wire: Wire,
    /// Quit this many milliseconds after launch, for smoke runs.
    smoke_millis: Option<u64>,
    /// Run the 30 updates per second stream on open.
    stream: bool,
    /// Why the switches do not fit together, if they do not.
    error: Option<String>,
    /// Raw flags, resolved by [`Launch::finalize`].
    ws_url: Option<String>,
    token: Option<String>,
    rpc_bin: Option<String>,
    replay: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl Launch {
    /// Records one problem, keeping the earlier ones, so the exit
    /// message can list every flag that did not fit.
    fn refuse(&mut self, problem: String) {
        match &mut self.error {
            Some(error) => {
                error.push_str("; ");
                error.push_str(&problem);
            }
            None => self.error = Some(problem),
        }
    }

    fn parse(args: impl Iterator<Item = String>) -> Self {
        let mut launch = Self::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--smoke" => {
                    let millis = args.peek().and_then(|value| value.parse::<u64>().ok());
                    if millis.is_some() {
                        args.next();
                    }
                    launch.smoke_millis = Some(millis.unwrap_or(0));
                }
                "--stream" => launch.stream = true,
                "--replay" => launch.replay = true,
                "--ws" => match args.next() {
                    Some(url) => launch.ws_url = Some(url),
                    None => launch.error = Some("--ws needs a ws:// URL".to_owned()),
                },
                "--token" => match args.next() {
                    Some(token) => launch.token = Some(token),
                    None => launch.error = Some("--token needs a value".to_owned()),
                },
                "--rpc-bin" => match args.next().as_deref().map(unquote_and_trim) {
                    Some("") => {}
                    Some(program) => launch.rpc_bin = Some(program.to_owned()),
                    None => launch.error = Some("--rpc-bin needs a path".to_owned()),
                },
                other => {
                    let known = other.split_once('=').is_some_and(|(name, _)| {
                        matches!(name, "--ws" | "--token" | "--rpc-bin" | "--smoke")
                    });
                    launch.refuse(if known {
                        format!("{other} is not a flag form here; pass the value after a space")
                    } else {
                        format!("unknown flag: {other}")
                    });
                }
            }
        }
        launch.finalize()
    }

    /// Resolves the raw flags into the wire to use.
    fn finalize(mut self) -> Self {
        if self.error.is_some() {
            return self;
        }
        self.wire = if let Some(url) = self.ws_url.take() {
            match self.token.take() {
                Some(token) => Wire::WebSocket { url, token },
                None => {
                    self.error =
                        Some("--ws needs --token; the token never rides the URL".to_owned());
                    return self;
                }
            }
        } else if self.token.take().is_some() {
            self.error = Some("--token needs --ws".to_owned());
            return self;
        } else if self.replay {
            Wire::Replay
        } else {
            Wire::Stdio
        };
        self
    }

    /// The transport the launch selected. Stdio runs `--rpc-bin`, else
    /// the engine [`crate::transport::stdio::engine_program`] picks.
    /// The browser has no local engine, so this never runs there.
    #[cfg(not(target_arch = "wasm32"))]
    fn transport(&self, saved: Option<&str>) -> Box<dyn Transport> {
        match &self.wire {
            Wire::Stdio => {
                let config = match self.rpc_bin.as_deref() {
                    Some(program) => StdioConfig {
                        program: program.to_owned(),
                        args: vec!["rpc".to_owned()],
                    },
                    None => StdioConfig::engine(saved),
                };
                Box::new(StdioTransport::new(config))
            }
            Wire::WebSocket { url, token } => {
                Box::new(WsTransport::new(url.clone(), token.clone()))
            }
            Wire::Replay => Box::new(crate::transport::replay::ReplayTransport::new()),
        }
    }
}

/// The shell: the store on the UI thread and the transport behind it.
///
/// One pump thread reads the transport's events and hands them to the
/// UI thread through a `std` channel, asking egui for a repaint on
/// each. The UI thread drains the store on every frame, executes the
/// Where the transport's events land before the frame drains them.
///
/// Native transports speak from a background thread, so a pump thread
/// moves their events into a thread-safe inbox and asks egui for a
/// repaint. The browser transport calls back on the UI thread itself,
/// so there is nothing to move and no thread to start: the frame
/// drains the channel directly.
enum Inbox {
    /// The native pump's inbox.
    #[cfg(not(target_arch = "wasm32"))]
    Threaded(std::sync::mpsc::Receiver<Event>),
    /// The browser transport's own channel.
    #[cfg(target_arch = "wasm32")]
    Direct(async_channel::Receiver<Event>),
}

impl Inbox {
    /// Starts a native pump thread over `incoming` and asks `ctx` for
    /// a repaint on every event.
    ///
    /// The browser has no thread to start, so its variant below is
    /// the only one that compiles there.
    #[cfg(not(target_arch = "wasm32"))]
    fn pumped(
        incoming: async_channel::Receiver<Event>,
        ctx: &egui::Context,
    ) -> (Self, std::thread::JoinHandle<()>) {
        let (tx, rx) = std::sync::mpsc::channel::<Event>();
        let ctx = ctx.clone();
        let pump = std::thread::Builder::new()
            .name("kage-pump".to_owned())
            .spawn(move || {
                while let Ok(event) = incoming.recv_blocking() {
                    if tx.send(event).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("the pump thread starts");
        (Self::Threaded(rx), pump)
    }

    /// The browser's inbox: the transport's own channel.
    #[cfg(target_arch = "wasm32")]
    fn direct(incoming: async_channel::Receiver<Event>) -> Self {
        Self::Direct(incoming)
    }

    /// Takes one event, or `None` when the channel is empty for now.
    fn try_next(&self) -> Option<Event> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Threaded(rx) => rx.try_recv().ok(),
            #[cfg(target_arch = "wasm32")]
            Self::Direct(rx) => rx.try_recv().ok(),
        }
    }
}

/// commands the store queued, and pushes the outgoing frames back
/// through the transport. The store never crosses the thread
/// boundary, matching the old single-threaded entity model.
pub struct Shell {
    /// The store, touched only on the UI thread.
    store: crate::store::Store,
    /// The transport behind the store.
    transport: Box<dyn Transport>,
    /// Where the transport's events land. A native transport hands
    /// them over on the pump thread and this is the thread-safe
    /// inbox; a browser transport calls back on the UI thread and
    /// this is the channel itself.
    inbox: Inbox,
    /// Set when `--smoke` asked for a timed quit.
    smoke_deadline: Option<(web_time::Instant, Duration)>,
    /// Whether the engine has been asked for its themes and config.
    themes_asked: bool,
    /// The settings page and active session the config snapshot was
    /// last asked for, so an open page asks once instead of per frame.
    config_asked: Option<(crate::ui::settings::Section, Option<String>)>,
    /// Synthetic chunks appended, for the 30 Hz stream measurement.
    streamed: usize,
    /// The store changes the frame's absorbed frames produced, raised
    /// as toasts once the drain ends.
    changes: Vec<kage_client::Change>,
    /// The transcript a finished export left, for the UI thread's
    /// clipboard.
    exported: Option<String>,
    /// The notice items each session held at the last scan, so a new
    /// notice toasts once.
    notices: crate::ui::toasts::NoticeWatch,
    /// The state each session's row held at the last frame, for the
    /// away-session toasts.
    statuses: std::collections::HashMap<String, crate::ui::sidebar::RowState>,
    /// The vim two-key prefix in flight (`g`, `z`), when the press
    /// before it started a sequence.
    vim_prefix: Option<char>,
    /// The `:` line's buffer, while the line is open.
    vim_line: Option<String>,
    /// The `:` line the last frame asked to run, drained after the
    /// draw.
    vim_submit: Option<String>,
    /// Whether the `:` line's field takes the focus this frame.
    vim_line_focus: bool,
    /// The keepalive handle of the pump thread, dropped with the
    /// shell. The browser has no pump thread: its transport calls back
    /// on the UI thread, so this stays `None` there.
    _pump: Option<std::thread::JoinHandle<()>>,
    /// The three-panel layout state.
    panels: crate::ui::shell::Panels,
    /// The gate banner's dismissal.
    gate_banner: crate::ui::status::Banner,
    /// The toast queue.
    toasts: crate::ui::toasts::Toasts,
    /// The command palette.
    palette: crate::ui::palette::Palette,
    /// The boot splash.
    splash: crate::ui::splash::Splash,
    /// The transcript in the chat column.
    transcript: crate::ui::transcript::view::TranscriptView,
    /// The find bar over the chat column.
    find: crate::ui::find::FindBar,
    /// The composer under the chat column: the field, its toolbar and
    /// its suggestion menus.
    composer: crate::ui::composer::Composer,
    /// The session the composer last took the keyboard for, so a new
    /// session takes it again.
    composer_focus_for: Option<Option<String>>,
    /// The widget that held the keyboard before a modal layer opened,
    /// handed back once the last layer closes.
    focus_return: Option<egui::Id>,
    /// Whether a modal layer showed at the end of the last frame.
    modal_shown: bool,
    /// The theme catalog the last theme was applied with.
    theme_catalog: crate::themes::Catalog,
    /// The welcome pane, shown in the chat column while no session is
    /// open.
    welcome: crate::ui::welcome::Welcome,
    /// The sidebar's search query and collapsed projects.
    sidebar: crate::ui::sidebar::Sidebar,
    /// The right panel's tabs, filters and selections.
    workbench: crate::ui::workbench::Workbench,
    /// The modal dialog layer over the shell.
    dialogs: crate::ui::dialog::Dialogs,
    /// The approval card over the open permission asks.
    approval: crate::ui::approval::Approval,
    /// The settings dialog over the shell.
    settings: crate::ui::settings::SettingsView,
    /// The setup screen, shown in place of the shell while no engine
    /// could start. The browser has no local engine to point at, so
    /// the screen never shows there and the field stays unread.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    setup: crate::ui::setup::SetupView,
    /// Whether the shell replays a recording: the replay always
    /// connects, so it never shows setup.
    replay: bool,
}

impl Shell {
    /// Builds the shell: the store opens sessions in `cwd`, `replay`
    /// selects the scripted prompt of the recording, and `transport`
    /// starts on the pump thread right away. `ctx` is cloned into the
    /// pump so each transport event asks for a repaint.
    #[must_use]
    pub fn new(
        cwd: impl Into<String>,
        replay: bool,
        mut transport: Box<dyn Transport>,
        ctx: &egui::Context,
    ) -> Self {
        let (events, incoming) = async_channel::unbounded();
        transport.start(events.clone());
        let (inbox, pump) = Self::start_inbox(incoming, ctx);
        Self {
            store: crate::store::Store::new(cwd, replay),
            transport,
            inbox,
            smoke_deadline: None,
            streamed: 0,
            themes_asked: false,
            config_asked: None,
            changes: Vec::new(),
            exported: None,
            notices: crate::ui::toasts::NoticeWatch::default(),
            statuses: std::collections::HashMap::new(),
            vim_prefix: None,
            vim_line: None,
            vim_submit: None,
            vim_line_focus: false,
            _pump: pump,
            panels: crate::ui::shell::Panels::default(),
            gate_banner: crate::ui::status::Banner::default(),
            toasts: crate::ui::toasts::Toasts::default(),
            palette: crate::ui::palette::Palette::default(),
            splash: crate::ui::splash::Splash::Showing,
            transcript: crate::ui::transcript::view::TranscriptView::new(),
            find: crate::ui::find::FindBar::new(),
            composer: crate::ui::composer::Composer::new(),
            composer_focus_for: None,
            focus_return: None,
            modal_shown: false,
            theme_catalog: crate::themes::Catalog::default(),
            welcome: crate::ui::welcome::Welcome::new(),
            sidebar: crate::ui::sidebar::Sidebar::new(),
            workbench: crate::ui::workbench::Workbench::new(),
            dialogs: crate::ui::dialog::Dialogs::new(),
            approval: crate::ui::approval::Approval::new(),
            settings: crate::ui::settings::SettingsView::new(),
            setup: crate::ui::setup::SetupView::new(),
            replay,
        }
    }

    /// Whether the setup screen shows instead of the shell: the link
    /// was refused and nothing else carries it. A replay never lands
    /// here: it connects on its own.
    #[must_use]
    pub fn needs_setup(&self) -> bool {
        !self.replay && matches!(self.store.connect(), crate::transport::State::Refused(_))
    }

    /// Starts `program` as the engine and lets it connect again.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn use_engine(&mut self, program: String, ctx: &egui::Context) {
        let config = StdioConfig {
            program,
            args: vec!["rpc".to_owned()],
        };
        self.replace_transport(Box::new(StdioTransport::new(config)), ctx);
    }

    /// Looks for `kage` again, the way the launch did.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn retry_engine(&mut self, saved: Option<&str>, ctx: &egui::Context) {
        let config = StdioConfig::engine(saved);
        self.replace_transport(
            Box::new(crate::transport::stdio::StdioTransport::new(config)),
            ctx,
        );
    }

    /// Swaps in `transport` and restarts the pump thread behind it.
    #[cfg(not(target_arch = "wasm32"))]
    fn replace_transport(&mut self, mut transport: Box<dyn Transport>, ctx: &egui::Context) {
        self._pump = None;
        let (events, incoming) = async_channel::unbounded();
        transport.start(events.clone());
        let (inbox, pump) = Self::start_inbox(incoming, ctx);
        self.transport = transport;
        self.inbox = inbox;
        self._pump = pump;
        self.store.set_connect(crate::transport::State::Connecting);
    }

    /// The inbox a fresh transport reports into, and the pump handle
    /// to keep on native. The browser has no pump thread, so its
    /// handle is `None`.
    fn start_inbox(
        incoming: async_channel::Receiver<Event>,
        ctx: &egui::Context,
    ) -> (Inbox, Option<std::thread::JoinHandle<()>>) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let (inbox, pump) = Inbox::pumped(incoming, ctx);
            (inbox, Some(pump))
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = ctx;
            (Inbox::direct(incoming), None)
        }
    }

    /// The connection's link, for the status bar.
    #[must_use]
    pub fn link(&self) -> Link {
        self.transport.link()
    }

    /// The store, for the views.
    pub const fn store(&self) -> &crate::store::Store {
        &self.store
    }

    /// The transcript, for the probes that watch the vim cursor move.
    #[must_use]
    pub fn transcript(&self) -> &crate::ui::transcript::view::TranscriptView {
        &self.transcript
    }

    /// Whether the workbench panel takes its column, for the probes
    /// that watch `:q` land.
    #[must_use]
    pub fn workbench_visible(&self) -> bool {
        self.panels.workbench_visible
    }

    /// Whether the `:` command line is open, for the probes.
    #[must_use]
    pub fn vim_line_open(&self) -> bool {
        self.vim_line.is_some()
    }

    /// The `:` command line's buffer, for the probes.
    #[must_use]
    pub fn vim_line_text(&self) -> Option<&str> {
        self.vim_line.as_deref()
    }

    /// Turns vim mode on or off, as the `:vim` command and the
    /// settings row do.
    pub fn set_vim(&mut self, on: bool) {
        self.store.update_prefs(|prefs| prefs.vim = on);
    }

    /// Drains the pump's events into the store and carries out the
    /// commands it produced.
    fn on_events(&mut self) {
        let mut reconnecting = false;
        self.changes.clear();
        while let Some(event) = self.inbox.try_next() {
            match event {
                Event::Frame(frame) => {
                    self.changes.extend(self.store.absorb(frame));
                }
                Event::State(state) => {
                    if matches!(state, crate::transport::State::Reconnecting { .. }) {
                        reconnecting = true;
                    }
                    self.store.set_connect(state);
                }
            }
        }
        // A reconnect asks for the themes and the config again, and
        // brings a still-short gate report back.
        if reconnecting {
            self.gate_banner.reset();
            self.themes_asked = false;
        }
        let answered =
            self.store.state().agent.is_some() || self.store.state().capabilities.is_some();
        self.splash.follow(self.store.connect(), answered);
        let now = web_time::Instant::now();
        for note in self.store.take_notes() {
            self.toasts.push_note(&note, now);
        }
        self.raise_change_toasts(now);
        self.watch_away(now);
        let commands = self.store.take_commands();
        for command in commands {
            match command {
                Command::Handshake { replay_sessions } => {
                    self.store.handshake(replay_sessions);
                }
                Command::NewSession => {
                    // The boot command races the recording's own
                    // session: when the answer already settled, the
                    // shell follows it and no second session asks.
                    if self.store.active_id().is_none() {
                        self.store.new_session();
                    }
                }
                Command::ReplayPrompt => {
                    self.store.prompt(REPLAY_PROMPT);
                }
            }
        }
    }

    /// Raises the toasts the frame's changes ask for: an ask answered
    /// elsewhere, a failed request, a finished export, and the notice
    /// items the opened sessions gained. The export's markdown waits
    /// in [`Shell::exported`] for the UI thread's clipboard.
    fn raise_change_toasts(&mut self, now: web_time::Instant) {
        let mut drafts = crate::ui::toasts::drafts_for_changes(&self.changes);
        drafts.extend(self.notices.scan(self.store.state()));
        for change in &self.changes {
            if let kage_client::Change::Exported { markdown, .. } = change {
                self.exported = Some(markdown.clone());
                drafts.push(crate::ui::toasts::ToastDraft {
                    tone: kage_client::wire::NoticeTone::Success,
                    text: "Copied the session as Markdown".to_owned(),
                    action: crate::ui::toasts::ToastAction::None,
                });
            }
        }
        for draft in drafts {
            self.toasts.push(draft, now);
        }
    }

    /// Toasts the background sessions whose row state moved under the
    /// last frame: an ask, a plan, a failure or a finish away from the
    /// active one. A click follows the session.
    fn watch_away(&mut self, now: web_time::Instant) {
        let active = self.store.active_id().map(str::to_owned);
        let mut drafts = Vec::new();
        for (id, session) in &self.store.state().sessions {
            if !session.opened || session.parent.is_some() {
                continue;
            }
            let state = crate::ui::sidebar::row_state(session);
            let was = self.statuses.insert(id.clone(), state);
            if active.as_deref() == Some(id.as_str()) {
                continue;
            }
            let Some((tone, what)) = away_toast(was, state) else {
                continue;
            };
            let title = self.store.display_title(id);
            drafts.push(crate::ui::toasts::ToastDraft {
                tone,
                text: format!("{title}: {what}"),
                action: crate::ui::toasts::ToastAction::ActivateSession(id.clone()),
            });
        }
        for draft in drafts {
            self.toasts.push(draft, now);
        }
    }

    /// Hands the store out for the frame's drawing and takes it
    /// back afterwards. The workbench's panes write to the store, so
    /// its closure needs it in a cell, and the cell outlives the
    /// closure only inside this frame.
    fn store_take(&mut self) -> crate::store::Store {
        std::mem::replace(&mut self.store, crate::store::Store::empty())
    }

    /// Drains the store's outgoing frames into the transport.
    fn flush_outgoing(&mut self) {
        if !self.themes_asked && self.store.live() {
            self.themes_asked = true;
            self.store.ask_themes();
            self.store.ask_config();
        }
        for frame in self.store.take_outgoing() {
            self.transport.send(frame);
        }
    }

    /// The line automated runs print before quitting.
    #[must_use]
    pub fn smoke_line(&self) -> String {
        let state = self.store.state();
        let agent = state
            .agent
            .as_ref()
            .map(|agent| format!("{} {}", agent.name, agent.version.as_deref().unwrap_or("?")))
            .unwrap_or_else(|| "none".to_owned());
        let (items, used, size) = self
            .store
            .active_session()
            .map(|session| (session.items.len(), session.usage.used, session.usage.size))
            .unwrap_or((0, 0, 0));
        format!(
            "smoke: connect={} agent=({agent}) sessions={sessions} items={items} used={used}/{size} streamed={streamed} connection={connection}",
            self.store.connect().label(),
            sessions = state.sessions.len(),
            streamed = self.streamed,
            connection = self
                .transport
                .connection_id()
                .unwrap_or_else(|| "-".to_owned()),
        )
    }

    /// Appends one synthetic agent chunk, for the 30 updates per
    /// second measurement against the real client path.
    fn stream_chunk(&mut self) {
        let Some(id) = self.store.active_id().map(str::to_owned) else {
            return;
        };
        self.streamed += 1;
        let text = format!("stream chunk {}\n", self.streamed);
        let frame = Frame::Notification {
            method: "session/update".to_owned(),
            params: serde_json::json!({
                "sessionId": id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text},
                },
            }),
        };
        self.store.absorb(frame);
    }
}

/// Hands the keys back once the find bar closed: to the composer,
/// unless vim's normal mode or an open ask holds them.
fn find_closed(ctx: &egui::Context, store: &crate::store::Store) {
    if store.prefs().vim || !store.active_asks().is_empty() {
        return;
    }
    ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("composer-input")));
}

/// The window's body: the shell plus the frame clock.
pub struct ShellApp {
    /// The shell under the window.
    pub shell: Shell,
    /// The 30 Hz stream runs while this is set. Native only: it is
    /// `--stream`, a measurement path the browser never takes.
    pub stream: bool,
    /// The theme the window opened with. The settings dialog reports
    /// a change with the choice it stored; the shell applies it.
    pub prefs_theme: crate::theme::ThemeChoice,
    /// The 30 Hz ticker the `--stream` run drives, for the NFR-01
    /// measurement.
    stream_tick: crate::frame_times::Ticker,
    /// When the daily release check last ran, unix seconds.
    last_update_check: i64,
    /// The finished release check's inbox, while one is in flight.
    update_rx: Option<std::sync::mpsc::Receiver<Option<crate::update::CheckOutcome>>>,
    /// Whether the engine runs on another machine, where the local
    /// folder picker would name paths the engine cannot open.
    pub remote: bool,
    /// The preferences revision last written to disk, once they came
    /// from disk. A shell built without them, as the tests build it,
    /// never writes the file.
    saved_prefs: Option<u64>,
    /// The system folder picker's answer, while it is open. Native
    /// only: the browser has no local folders.
    #[cfg(not(target_arch = "wasm32"))]
    folder_rx: Option<std::sync::mpsc::Receiver<crate::picker::Picked>>,
    /// Whether the check in flight is the daily one, whose answer
    /// raises the update toast. The settings page's own check reports
    /// in place instead.
    announce_update: bool,
    /// The finished install's inbox, while one is in flight. The
    /// browser never installs anything.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    install_rx: Option<
        std::sync::mpsc::Receiver<(
            crate::ui::settings::Install,
            Result<std::path::PathBuf, String>,
        )>,
    >,
    /// Which install the in-flight download is for.
    installing: Option<crate::ui::settings::Install>,
    /// What the last install ended with, as the Updates row reads it.
    install_note: Option<(bool, String)>,
    /// The actions the transcript's rows raised this frame, for the
    /// shell to carry out.
    row_actions: Vec<crate::ui::transcript::tool_rows::RowAction>,
    /// The window's context, for the modal key contexts.
    ctx: egui::Context,
    /// When this window was built, the clock the dialogs' fields and
    /// the constellation twinkle on.
    born: web_time::Instant,
    /// The frame-time recorder, off unless `KAGE_FRAME_TIMES` is set.
    frame_times: crate::frame_times::FrameTimes,
}

impl eframe::App for ShellApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // The frame clock starts here and stops around the draw. The
        // time between frames is idle time: egui schedules no repaint
        // while the app waits, so charging that gap to a frame is how
        // an idle window reports one-second frames.
        self.frame_times.begin_frame(web_time::Instant::now());
        self.shell.on_events();
        if let Some(markdown) = self.shell.exported.take() {
            ctx.copy_text(markdown);
        }
        self.shell.flush_outgoing();
        self.save_prefs();
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.pump_update_check();
            self.apply_update();
            self.apply_install();
            self.apply_folder();
        }
        self.route_keys(&ctx);
        self.vim_keys(&ctx);
        if self.stream {
            // The ticker advances its clock by one period per tick
            // rather than to the current time: resetting to `now`
            // pushed every tick out by a frame and the 30 Hz rate
            // drifted below its NFR-01 budget however fast the frames
            // were (284 of 300 at 64 fps).
            let now = web_time::Instant::now();
            if self.stream_tick.tick(now) {
                self.shell.stream_chunk();
            }
            ctx.request_repaint_after(self.stream_tick.due_in(now));
        }
        if let Some((started, delay)) = self.shell.smoke_deadline.take() {
            if started.elapsed() >= delay {
                println!("{}", self.shell.smoke_line());
                // The frame began above and drew nothing: it found the
                // deadline and asked the window to close. Closing it
                // here would measure the whole idle wait as a draw.
                self.frame_times.report();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            self.shell.smoke_deadline = Some((started, delay));
            // The smoke window runs without input, so keep the frame
            // clock alive until the deadline.
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        self.shell.toasts.expire(web_time::Instant::now());
        #[cfg(not(target_arch = "wasm32"))]
        if self.shell.needs_setup() {
            let draw_started = web_time::Instant::now();
            self.draw_setup(ui);
            let now = web_time::Instant::now();
            self.frame_times
                .end_frame(now, now.saturating_duration_since(draw_started));
            return;
        }
        let draw_started = web_time::Instant::now();
        self.draw(ui);
        if let Some(line) = self.shell.vim_submit.take() {
            self.run_vim(crate::vim::parse(&line), &ctx);
        }
        let now = web_time::Instant::now();
        self.frame_times
            .end_frame(now, now.saturating_duration_since(draw_started));
    }

    /// Prints the frame-time summary when the window closes, so a run
    /// that quits from the title bar or a signal still reports. It
    /// records no frame: every draw path already closed its own, and
    /// the time between the last frame and here is teardown.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.frame_times.report();
    }
}

impl ShellApp {
    /// Hands the shell the preferences saved on disk. They count as
    /// saved, so only a change after this writes the file.
    #[must_use]
    pub fn with_prefs(mut self, prefs: crate::prefs::Prefs) -> Self {
        self.prefs_theme = prefs.theme.clone();
        let store = self.shell.store_take().with_prefs(prefs);
        self.shell.store = store;
        self.saved_prefs = Some(self.shell.store.prefs_rev());
        self
    }

    /// Writes the preferences when they moved since the last write.
    fn save_prefs(&mut self) {
        let rev = self.shell.store.prefs_rev();
        if self.saved_prefs.is_some_and(|saved| saved != rev) {
            self.saved_prefs = Some(rev);
            crate::prefs::save(self.shell.store.prefs());
        }
    }

    /// Builds the window body over a transport: the native flags and
    /// the browser handoff both land here.
    #[must_use]
    pub fn new(
        cwd: String,
        replay: bool,
        transport: Box<dyn Transport>,
        ctx: &egui::Context,
    ) -> Self {
        crate::theme::install_fonts(ctx);
        crate::icons::install(ctx);
        crate::shapes::install(ctx);
        Self {
            shell: Shell::new(cwd, replay, transport, ctx),
            stream: false,
            prefs_theme: crate::theme::ThemeChoice::System,
            stream_tick: crate::frame_times::Ticker::hz30(web_time::Instant::now()),
            last_update_check: crate::clock::unix_seconds(),
            update_rx: None,
            remote: false,
            #[cfg(not(target_arch = "wasm32"))]
            folder_rx: None,
            announce_update: false,
            install_rx: None,
            saved_prefs: None,
            installing: None,
            install_note: None,
            row_actions: Vec::new(),
            ctx: ctx.clone(),
            born: web_time::Instant::now(),
            frame_times: crate::frame_times::FrameTimes::from_env(),
        }
    }

    /// Draws the setup screen in place of the shell, and carries what
    /// it asked for: a probed binary becomes the engine, a retry looks
    /// on the PATH again, and a chosen path is remembered so the next
    /// launch starts without the screen. The browser has no local
    /// engine to point at, so it never gets here.
    #[cfg(not(target_arch = "wasm32"))]
    fn draw_setup(&mut self, ui: &mut egui::Ui) {
        egui::Area::new(egui::Id::new("kage-setup"))
            .anchor(egui::Align2::LEFT_TOP, egui::Vec2::ZERO)
            .order(egui::Order::Foreground)
            .interactable(true)
            .movable(false)
            .show(ui.ctx(), |ui| {
                let palette = crate::theme::Palette::active_of(ui.ctx());
                ui.set_min_size(ui.ctx().viewport_rect().size());
                egui::Frame::NONE.fill(palette.bg).show(ui, |ui| {
                    ui.centered_and_justified(|ui| {
                        let installing =
                            self.installing == Some(crate::ui::settings::Install::Engine);
                        self.shell.setup.show(ui, installing);
                    });
                });
            });
        let saved = self.shell.store.prefs().kage_path.clone();
        match self.shell.setup.take_asked() {
            Some(crate::ui::setup::SetupEvent::Use(program)) => {
                self.shell
                    .store
                    .update_prefs(|prefs| prefs.kage_path = Some(program.clone()));
                self.shell.use_engine(program, ui.ctx());
            }
            Some(crate::ui::setup::SetupEvent::Retry) => {
                self.shell.retry_engine(saved.as_deref(), ui.ctx());
            }
            #[cfg(not(target_arch = "wasm32"))]
            Some(crate::ui::setup::SetupEvent::Download) => {
                self.start_install(crate::ui::settings::Install::Engine);
            }
            #[cfg(target_arch = "wasm32")]
            Some(crate::ui::setup::SetupEvent::Download) => {}
            None => {}
        }
    }

    /// Draws the frame: the gate banner over the three panels, the
    /// status bar under them, and the chrome (splash, palette,
    /// toasts) over the top.
    fn draw(&mut self, ui: &mut egui::Ui) {
        let catalog = self.shell.store.theme_catalog();
        if catalog != self.shell.theme_catalog {
            self.shell.theme_catalog = catalog;
            let choice = self.shell.store.prefs().theme.clone();
            crate::theme::apply_choice_with(&choice, ui.ctx(), &self.shell.theme_catalog);
        }
        let link = self.shell.link();
        self.shell.store.set_link(link);
        let active = self.shell.store.active_id().map(str::to_owned);
        if self.shell.composer_focus_for.as_ref() != Some(&active) {
            // Each session keeps its own draft: the text typed for the
            // one left behind is banked, and the new one's comes back.
            if let Some(previous) = &self.shell.composer_focus_for {
                let held = self.shell.composer.draft.text.clone();
                self.shell.store.set_draft(previous.as_deref(), &held);
                let draft = active
                    .as_deref()
                    .and_then(|id| self.shell.store.draft(id))
                    .unwrap_or_default()
                    .to_owned();
                if draft != held {
                    self.shell.composer.set_draft(draft);
                }
            }
            if !self.shell.store.prefs().vim {
                ui.ctx().memory_mut(|memory| {
                    memory.request_focus(egui::Id::new("composer-input"));
                });
            }
            self.shell.composer_focus_for = Some(active);
        }
        // The key context: a modal sits above the shell.
        let context = self.context();
        let gate = self.shell.store.gate().clone();
        let mut banner = self.shell.gate_banner.clone();
        self.shell.panels.follow(self.shell.store.active_id());
        let mut panels = self.shell.panels.clone();
        let mut palette = self.shell.palette.clone();
        let mut toasts = std::mem::take(&mut self.shell.toasts);
        let splash = self.shell.splash;

        banner.show(ui, &gate);

        // The panel draws the sidebar twice while narrow, so its state
        // rides in a cell the closure borrows once. The rows the
        // sidebar lists are read once, before the chat column takes
        // `self` for its mutable state, and handed to the closure as
        // data; the closure's own events are carried out afterwards.
        let sidebar = std::cell::RefCell::new(std::mem::take(&mut self.shell.sidebar));
        let sidebar_rows = crate::ui::sidebar::sidebar_rows(&self.shell.store);
        let sidebar_grouped = self.shell.store.prefs().group_by_project;
        let (link_name, link_status) = crate::ui::sidebar::connect_lines(&self.shell.store);
        let sidebar_palette = crate::theme::Palette::active_of(ui.ctx());
        let (dot, halo) = crate::ui::sidebar::connect_dot(sidebar_palette, &self.shell.store);
        let sidebar_link = (link_name, link_status, dot, halo);
        let sidebar_version = self
            .shell
            .store
            .state()
            .agent
            .as_ref()
            .and_then(|agent| agent.version.clone());
        let sidebar_events: std::cell::RefCell<Vec<crate::ui::sidebar::SidebarEvent>> =
            std::cell::RefCell::new(Vec::new());
        // The workbench and the store ride in cells the way the
        // sidebar does: its closure must stay `Fn`, because the shell
        // draws the panel twice while narrow, and its events are
        // carried out once the panels are drawn.
        let workbench_state = std::cell::RefCell::new(std::mem::take(&mut self.shell.workbench));
        // The workbench's panes read and write the store, so it rides
        // in a cell the closure borrows once, the way the sidebar's
        // state does. The shell takes it back when the panels are
        // drawn.
        let store = std::cell::RefCell::new(self.shell.store_take());
        let workbench_events: std::cell::RefCell<Vec<crate::ui::workbench::WorkbenchEvent>> =
            std::cell::RefCell::new(Vec::new());
        let sidebar_draw = |ui: &mut egui::Ui| {
            let events = crate::ui::sidebar::show_rows(
                ui,
                &mut sidebar.borrow_mut(),
                &sidebar_rows,
                sidebar_grouped,
                (
                    sidebar_link.0.as_str(),
                    sidebar_link.1.as_str(),
                    sidebar_link.2,
                    sidebar_link.3,
                ),
                sidebar_version.as_deref(),
            );
            sidebar_events.borrow_mut().extend(events);
        };
        let workbench = |ui: &mut egui::Ui| {
            let events = crate::ui::workbench::show(
                ui,
                &mut workbench_state.borrow_mut(),
                &mut store.borrow_mut(),
            );
            workbench_events.borrow_mut().extend(events);
        };
        let topbar_events: std::cell::RefCell<Vec<crate::ui::topbar::TopbarEvent>> =
            std::cell::RefCell::new(Vec::new());
        // Under the narrow breakpoint the sidebar takes no column, so
        // the bar keeps its show button up, as the reference does.
        let sidebar_hidden =
            !self.shell.panels.sidebar_visible || self.shell.panels.sidebar_floats();
        let workbench_visible = self.shell.panels.workbench_visible;
        let chat = |ui: &mut egui::Ui| {
            // The column is the transcript above and the composer
            // below: the transcript takes what is left, and the
            // composer and its dock keep their own height. With no
            // session the welcome column shows instead, and the
            // composer still mounts so a draft never resets.
            //
            // The store comes from the cell the panels share, not
            // from the shell's field: `store_take` has moved the real
            // store out for the draw, so a read through the field
            // would see the empty placeholder and draw the welcome
            // over a live transcript forever.
            let store = &mut store.borrow_mut();
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                crate::ui::topbar::show(
                    ui,
                    store,
                    sidebar_hidden,
                    workbench_visible,
                    &mut topbar_events.borrow_mut(),
                );
                if store.active_id().is_some() {
                    if self.shell.find.is_open() {
                        let rows = self.shell.transcript.searchable_rows(store);
                        if let Some(crate::ui::find::RowAction::Reveal(key)) =
                            self.shell.find.show(ui, &rows)
                        {
                            self.shell.transcript.scroll_to_row(key);
                        }
                        if !self.shell.find.is_open() {
                            find_closed(ui.ctx(), store);
                        }
                    }
                    self.shell.transcript.set_find(self.shell.find.marks());
                    egui::Panel::bottom("chat-band")
                        .resizable(false)
                        .show_separator_line(false)
                        .frame(egui::Frame::NONE.inner_margin(egui::Margin {
                            left: crate::theme::SP_8 as i8,
                            right: crate::theme::SP_8 as i8,
                            top: 0,
                            bottom: crate::theme::SP_6 as i8,
                        }))
                        .show(ui, |ui| {
                            centered_column(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                self.show_dock(ui, store);
                                self.show_approval(ui, store);
                                self.show_composer(ui, store);
                                self.show_vim_bar(ui, store);
                            });
                        });
                    let mut top = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(ui.available_rect_before_wrap())
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                    );
                    let mut events = Vec::new();
                    self.shell.transcript.show(&mut top, store, &mut events);
                    self.row_actions.extend(events);
                } else {
                    // The welcome pane: the whole column, wordmark over
                    // glow, and the composer hosted inside the stack
                    // between the pill and the cards, as the reference
                    // pane mounts it. There is no dock and no vim bar
                    // here; a draft survives the move because the same
                    // composer renders in both columns.
                    let running = store
                        .active_session()
                        .is_some_and(|session| session.running);
                    let enter_sends = store.prefs().enter_sends;
                    let commands = store.composer_commands().to_vec();
                    let listing = store
                        .active_id()
                        .and_then(|id| store.fs_listing(id))
                        .cloned();
                    self.shell.composer.set_listing(listing);
                    let crate::launch::Shell {
                        welcome, composer, ..
                    } = &mut self.shell;
                    let (events, composer_event) = welcome.show(
                        &mut ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(ui.available_rect_before_wrap())
                                .layout(egui::Layout::top_down(egui::Align::Min)),
                        ),
                        store,
                        composer,
                        &commands,
                        running,
                        enter_sends,
                    );
                    for event in events {
                        match event {
                            crate::ui::welcome::WelcomeEvent::Fill { text, option } => {
                                if let Some((id, value)) = option {
                                    store.hold_option(id, value);
                                }
                                self.shell.composer.set_draft(text.to_owned());
                                ui.ctx().memory_mut(|memory| {
                                    memory.request_focus(egui::Id::new("composer-input"));
                                });
                            }
                            crate::ui::welcome::WelcomeEvent::OpenFolder => {
                                // The system picker when the engine is
                                // local, as the reference opens it; the
                                // engine's own listing otherwise.
                                if !self.open_system_folder_picker() {
                                    self.shell.dialogs.show_dialog(
                                        crate::ui::dialog::DialogKind::OpenFolder,
                                        store,
                                    );
                                }
                            }
                        }
                    }
                    self.handle_composer_event(ui, store, composer_event, running);
                }
            });
        };
        crate::ui::shell::show(ui, &mut panels, sidebar_draw, chat, workbench);
        // The real store comes back before the bar's and the rows'
        // asks run: carried out earlier, they would land on the empty
        // placeholder `store_take` left and vanish with it.
        self.shell.sidebar = sidebar.into_inner();
        self.shell.store = store.into_inner();
        self.shell.workbench = workbench_state.into_inner();
        for event in topbar_events.into_inner() {
            match event {
                crate::ui::topbar::TopbarEvent::ShowSidebar => panels.toggle_sidebar(),
                crate::ui::topbar::TopbarEvent::ToggleWorkbench => panels.toggle_workbench(),
                crate::ui::topbar::TopbarEvent::Session(action) => {
                    self.carry_session(ui.ctx(), action);
                }
                crate::ui::topbar::TopbarEvent::Rename(title) => {
                    self.shell.store.rename(&title);
                }
            }
        }
        let sidebar_events = sidebar_events.into_inner();
        // The palette draws from this frame's clone, so an open from
        // the sidebar has to land in the clone: opening the field
        // here would be overwritten by the write-back at the end of
        // the frame, and the palette would flash for one frame.
        if sidebar_events
            .iter()
            .any(|event| matches!(event, crate::ui::sidebar::SidebarEvent::OpenPalette))
        {
            palette.open();
        }
        // The same holds for the collapse: the frame writes `panels`
        // back over the shell's own at its end.
        if sidebar_events
            .iter()
            .any(|event| matches!(event, crate::ui::sidebar::SidebarEvent::Collapse))
        {
            panels.toggle_sidebar();
        }
        self.carry_sidebar(sidebar_events);
        let workbench_events = workbench_events.into_inner();
        if workbench_events
            .iter()
            .any(|event| matches!(event, crate::ui::workbench::WorkbenchEvent::Close))
        {
            panels.toggle_workbench();
        }
        self.carry_workbench(workbench_events);
        // The rows' asks land on the store and the workbench, so they
        // run once both are back from the frame's cells.
        for action in std::mem::take(&mut self.row_actions) {
            self.carry_row(ui.ctx(), &mut panels, action);
        }

        let mut chosen = None;
        if self.shell.palette.is_open() {
            let registry = crate::ui::palette::RegistryOwned::of(self.shell.store());
            let rows = registry.view().rows(self.shell.palette.query());
            chosen = palette.show(ui, &rows);
        }
        // The dialog layer sits above every panel, over the palette
        // too: a dialog the palette opens must not answer beneath it.
        if let Some(id) = self.shell.dialogs.browsing()
            && let Some(read) = self.shell.store.folders(id).cloned()
        {
            self.shell.dialogs.answer_folders(id, read);
        }
        if self.shell.dialogs.is_open() {
            let events = self.shell.dialogs.show(ui, &self.shell.store, self.born);
            self.carry_dialog(events);
        }
        // The settings dialog sits above the dialog layer: a dialog it
        // opens from the palette must not answer beneath it.
        if self.shell.settings.is_open() {
            let updates = crate::ui::settings::UpdateState {
                installing: self.installing,
                checking: self.update_rx.is_some(),
                install_note: self.install_note.clone(),
            };
            let events = self
                .shell
                .settings
                .show(ui, &mut self.shell.store, &updates);
            self.carry_settings(events, ui.ctx());
        }
        toasts.show(ui);
        splash.show(ui);
        let _ = context;

        self.shell.gate_banner = banner;
        self.shell.panels = panels;
        self.shell.palette = palette;
        if let Some(entry) = chosen {
            self.run_palette_entry(entry, ui.ctx());
        }
        self.shell.toasts = toasts;
        self.carry_toast_actions();
        self.follow_modal_focus(ui.ctx());
    }

    /// Keeps the keyboard with the top layer, as the reference's
    /// overlays do: a modal opening over the shell takes the focus from
    /// the field under it, so typing never lands in a hidden composer,
    /// and the last one closing hands the focus back.
    fn follow_modal_focus(&mut self, ctx: &egui::Context) {
        let modal = self.shell.settings.is_open()
            || self.shell.dialogs.is_open()
            || self.shell.palette.is_open();
        let focused = ctx.memory(|memory| memory.focused());
        if !modal {
            if !self.shell.modal_shown {
                self.shell.focus_return = focused;
            } else if let Some(id) = self.shell.focus_return.take() {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        } else if !self.shell.modal_shown
            && let Some(id) = focused
            && self.shell.focus_return == Some(id)
        {
            // The layer's own field did not take the keys this frame,
            // so the field under it still holds them.
            ctx.memory_mut(|memory| memory.surrender_focus(id));
        }
        self.shell.modal_shown = modal;
    }

    /// Carries out what clicks on toasts asked for. A click already
    /// dismissed its toast; only the action is left.
    fn carry_toast_actions(&mut self) {
        for action in self.shell.toasts.take_actions() {
            match action {
                crate::ui::toasts::ToastAction::ActivateSession(id) => {
                    self.shell.store.set_active(id);
                }
                crate::ui::toasts::ToastAction::Restore(id) => {
                    self.shell.store.restore(&id);
                }
                crate::ui::toasts::ToastAction::OpenUpdates => {
                    crate::update::open_releases();
                    self.shell
                        .store
                        .update_prefs(|prefs| prefs.update_badge = false);
                }
                crate::ui::toasts::ToastAction::None => {}
            }
        }
    }

    /// Carries out what the sidebar's controls asked for.
    fn carry_sidebar(&mut self, events: Vec<crate::ui::sidebar::SidebarEvent>) {
        if !events.is_empty() {
            self.ctx.request_repaint();
        }
        for event in events {
            match event {
                crate::ui::sidebar::SidebarEvent::NewSession(dir) => {
                    if let Some(dir) = dir {
                        self.shell.store.set_project(Some(dir));
                    }
                    self.shell.store.show_welcome();
                }
                crate::ui::sidebar::SidebarEvent::Activate(id) => {
                    self.shell.store.set_active(id);
                }
                // An open goes to the frame's palette clone before the
                // show, not to the field this frame overwrites.
                crate::ui::sidebar::SidebarEvent::OpenPalette => {}
                crate::ui::sidebar::SidebarEvent::OpenSettings => self
                    .shell
                    .settings
                    .open(crate::ui::settings::Section::General),
                // A collapse goes to the frame's panels, as the open
                // does to its palette.
                crate::ui::sidebar::SidebarEvent::Collapse => {}
                crate::ui::sidebar::SidebarEvent::ToggleGrouping => {
                    self.shell.store.update_prefs(|prefs| {
                        prefs.group_by_project = !prefs.group_by_project;
                    });
                }
            }
        }
    }

    /// Carries out what the workbench's panes asked for.
    /// Carries out what the dialogs asked for.
    fn carry_dialog(&mut self, events: Vec<crate::ui::dialog::DialogEvent>) {
        for event in events {
            match event {
                crate::ui::dialog::DialogEvent::SaveGoal(text) => {
                    let _ = self.shell.store.set_option("goal", &text);
                }
                crate::ui::dialog::DialogEvent::EnableSwarm(permission) => {
                    if let Some(permission) = &permission {
                        let _ = self.shell.store.set_permission(permission);
                    }
                    let _ = self.shell.store.set_option("swarm", "on");
                }
                crate::ui::dialog::DialogEvent::Rewind(prompt) => {
                    // The rewind rewrites history a running turn is
                    // still appending to, so it waits.
                    let running = self
                        .shell
                        .store
                        .active_session()
                        .is_some_and(|session| session.running || session.in_turn);
                    if running {
                        self.shell.toasts.push(
                            crate::ui::toasts::ToastDraft {
                                tone: kage_client::wire::NoticeTone::Warn,
                                text: "Interrupt the running turn before rewinding".to_owned(),
                                action: crate::ui::toasts::ToastAction::None,
                            },
                            web_time::Instant::now(),
                        );
                    } else {
                        let _ = self.shell.store.rewind(prompt);
                    }
                }
                crate::ui::dialog::DialogEvent::SetProject(path) => {
                    self.shell.store.set_project(Some(path));
                }
                crate::ui::dialog::DialogEvent::Browse(path) => {
                    let id = self.shell.store.ask_folders(path.as_deref());
                    self.shell.dialogs.browse(id);
                }
            }
        }
    }

    /// Carries out what the settings dialog asked for. A page the
    /// engine's configuration backs asks for a fresh snapshot, so an
    /// edit to config.toml shows on the next visit.
    fn carry_settings(
        &mut self,
        events: Vec<crate::ui::settings::SettingsEvent>,
        ctx: &egui::Context,
    ) {
        for event in events {
            match event {
                crate::ui::settings::SettingsEvent::Theme(choice) => {
                    self.prefs_theme = choice.clone();
                    self.shell
                        .store
                        .update_prefs(|prefs| prefs.theme = choice.clone());
                    crate::theme::apply_choice_with(&choice, ctx, &self.shell.theme_catalog);
                }
                crate::ui::settings::SettingsEvent::SystemTheme { dark, name } => {
                    self.shell.store.set_system_theme(dark, Some(&name));
                }
                crate::ui::settings::SettingsEvent::Flip(field) => {
                    self.shell.store.update_prefs(|prefs| match field {
                        "enter_sends" => prefs.enter_sends = !prefs.enter_sends,
                        "group_by_project" => prefs.group_by_project = !prefs.group_by_project,
                        "confirm_swarm" => prefs.confirm_swarm = !prefs.confirm_swarm,
                        "vim" => prefs.vim = !prefs.vim,
                        "smooth_stream" => prefs.smooth_stream = !prefs.smooth_stream,
                        "rail" => prefs.rail = !prefs.rail,
                        "constellation" => prefs.constellation = !prefs.constellation,
                        "fuel" => prefs.fuel = !prefs.fuel,
                        _ => {}
                    });
                }
                crate::ui::settings::SettingsEvent::EngineOption(name, value) => {
                    self.shell.store.set_engine_option(&name, value);
                }
                crate::ui::settings::SettingsEvent::Mode(value) => {
                    let _ = self.shell.store.set_permission(&value);
                }
                crate::ui::settings::SettingsEvent::Restore(id) => {
                    self.shell.store.restore(&id);
                }
                crate::ui::settings::SettingsEvent::Channel(channel) => {
                    self.shell
                        .store
                        .update_prefs(|prefs| prefs.channel = channel);
                    self.check_releases(true);
                }
                crate::ui::settings::SettingsEvent::CheckNow => self.check_releases(true),
                #[cfg(target_arch = "wasm32")]
                crate::ui::settings::SettingsEvent::Install(_)
                | crate::ui::settings::SettingsEvent::OpenReleases => {}
                #[cfg(not(target_arch = "wasm32"))]
                crate::ui::settings::SettingsEvent::Install(which) => {
                    self.start_install(which);
                }
                #[cfg(not(target_arch = "wasm32"))]
                crate::ui::settings::SettingsEvent::OpenReleases => {
                    crate::update::open_releases();
                }
                crate::ui::settings::SettingsEvent::ProviderForm(_)
                | crate::ui::settings::SettingsEvent::McpForm(_)
                | crate::ui::settings::SettingsEvent::AcpForm(_)
                | crate::ui::settings::SettingsEvent::Directory(_) => {}
            }
        }
        let page = (self.shell.settings.is_open() && self.shell.settings.section().reads_config())
            .then(|| {
                (
                    self.shell.settings.section(),
                    self.shell.store.active_id().map(str::to_owned),
                )
            });
        if newly_wanted(&mut self.shell.config_asked, page) {
            self.shell.store.ask_config();
        }
    }

    fn carry_workbench(&mut self, events: Vec<crate::ui::workbench::WorkbenchEvent>) {
        for event in events {
            match event {
                crate::ui::workbench::WorkbenchEvent::Close => {}
                crate::ui::workbench::WorkbenchEvent::Mention(path) => {
                    self.shell.composer.mention(&path);
                }
                crate::ui::workbench::WorkbenchEvent::OpenAgent(id) => {
                    self.shell.store.load_child(&id);
                    self.shell.workbench.open_agent(id);
                }
                crate::ui::workbench::WorkbenchEvent::Row(action) => {
                    self.row_actions.push(action);
                    self.ctx.request_repaint();
                }
                crate::ui::workbench::WorkbenchEvent::StopAgent(id) => {
                    self.shell.store.cancel_session(&id);
                }
                crate::ui::workbench::WorkbenchEvent::AskFiles => {
                    if let Some(id) = self.shell.store.active_id().map(str::to_owned) {
                        self.shell.store.fs_list("");
                        self.shell.workbench.mark_listing_asked(&id);
                    }
                }
                crate::ui::workbench::WorkbenchEvent::ReadFile(path) => {
                    self.shell.store.fs_read(&path);
                }
            }
        }
    }

    /// Draws the dock row: the pills the session's state names, over
    /// the composer. The rows carry the shell's follow-up actions.
    /// Draws the approval card over the dock row: the oldest open
    /// permission ask of the active session and its subagents, with
    /// the digit keys while it holds them.
    fn show_approval(&mut self, ui: &mut egui::Ui, store: &mut crate::store::Store) {
        let open = store.active_asks().len();
        let events = self.shell.approval.show(ui, store);
        self.carry_approval(store, events);
        // A first ask takes the keys from whatever field held them, so
        // its digits answer; the last one closing while nothing holds
        // the keys hands them back to the composer.
        let focus = ui.ctx().memory(|memory| memory.focused());
        match self.shell.approval.sync_open(open) {
            Some(true) => {
                if let Some(id) = focus {
                    ui.ctx().memory_mut(|memory| memory.surrender_focus(id));
                }
            }
            Some(false) if focus.is_none() => {
                ui.ctx().memory_mut(|memory| {
                    memory.request_focus(egui::Id::new("composer-input"));
                });
            }
            _ => {}
        }
        if open > 0 {
            if let Some(ask) = store.active_asks().first() {
                let request = ask.1.request_id.clone();
                self.shell.approval.focus(&request);
            }
        } else {
            self.shell.approval.blur();
        }
    }

    /// Steps the active session's permission mode to the next one the
    /// agent offers, wrapping at the end. Plan is not one of the steps.
    fn cycle_permission(&mut self) {
        let Some(session) = self.shell.store.active_session() else {
            return;
        };
        let values: Vec<String> = crate::modes::select_option(session, "mode")
            .map(|option| {
                option
                    .options
                    .iter()
                    .map(|mode| mode.value.clone())
                    .filter(|value| value != crate::modes::PLAN_MODE)
                    .collect()
            })
            .unwrap_or_default();
        let current = self.shell.store.permission_mode().unwrap_or_default();
        let index = values
            .iter()
            .position(|value| *value == current)
            .map_or(0, |index| (index + 1) % values.len().max(1));
        if let Some(next) = values.get(index) {
            self.shell.store.set_permission(next);
        }
    }

    /// Carries out what a transcript row asked for.
    fn carry_row(
        &mut self,
        ctx: &egui::Context,
        panels: &mut crate::ui::shell::Panels,
        action: crate::ui::transcript::tool_rows::RowAction,
    ) {
        use crate::ui::transcript::media_rows::MediaPress;
        use crate::ui::transcript::tool_rows::RowAction;
        use crate::ui::workbench::Tab;
        match action {
            RowAction::OpenFile(path) => self.show_workbench(panels, Tab::Files, Some(path)),
            RowAction::OpenChange(path) => self.show_workbench(panels, Tab::Changes, Some(path)),
            RowAction::OpenBrowser => self.show_workbench(panels, Tab::Browser, None),
            RowAction::OpenAgent(id) => {
                self.shell.store.load_child(&id);
                self.show_workbench(panels, Tab::Agents, None);
                self.shell.workbench.open_agent(id);
            }
            RowAction::OpenAgents => self.show_workbench(panels, Tab::Agents, None),
            RowAction::Review(choice, revision) => {
                self.shell.store.review_plan(choice, revision.as_deref());
            }
            RowAction::CancelAgent(id) => self.shell.store.cancel_session(&id),
            RowAction::ResumeSwarm(members) => {
                self.shell.store.resume_swarm(&members);
            }
            RowAction::Copy(text) => ctx.copy_text(text),
            RowAction::Edit(text) => {
                self.shell.composer.set_draft(text);
                ctx.memory_mut(|memory| {
                    memory.request_focus(egui::Id::new("composer-input"));
                });
            }
            RowAction::Retry(prompt) => {
                self.shell.store.prompt(&prompt);
            }
            RowAction::Fork(through) => {
                self.shell.store.fork(through);
            }
            RowAction::Media(ix, nth, MediaPress::Save) => self.shell.store.save_media(ix, nth),
            RowAction::Media(ix, nth, MediaPress::Play) => self.shell.store.play_media(ix, nth),
            RowAction::Rewind(prompt) => {
                // A rewind drops history, so it asks first, as the
                // reference does; the dialog carries it out.
                let running = self
                    .shell
                    .store
                    .active_session()
                    .is_some_and(|session| session.running || session.in_turn);
                if running {
                    self.shell.toasts.push(
                        crate::ui::toasts::ToastDraft {
                            tone: kage_client::wire::NoticeTone::Warn,
                            text: "Interrupt the running turn before rewinding".to_owned(),
                            action: crate::ui::toasts::ToastAction::None,
                        },
                        web_time::Instant::now(),
                    );
                } else {
                    let events = self.shell.dialogs.show_dialog(
                        crate::ui::dialog::DialogKind::Rewind(prompt),
                        &mut self.shell.store,
                    );
                    self.carry_dialog(events);
                }
            }
        }
    }

    /// Shows the workbench on `tab`, previewing `file` when the tab
    /// shows files.
    fn show_workbench(
        &mut self,
        panels: &mut crate::ui::shell::Panels,
        tab: crate::ui::workbench::Tab,
        file: Option<String>,
    ) {
        panels.workbench_visible = true;
        self.shell.workbench.set_tab(tab);
        if file.is_some() {
            self.shell.workbench.set_file(file);
        }
    }

    /// Carries out a session action of the top bar's menu on the active
    /// session.
    fn carry_session(&mut self, ctx: &egui::Context, action: crate::ui::topbar::SessionAction) {
        use crate::ui::topbar::SessionAction;
        let Some(id) = self.shell.store.active_id().map(str::to_owned) else {
            return;
        };
        match action {
            SessionAction::Pin => self.shell.store.toggle_pin(&id),
            SessionAction::Fork => {
                self.shell.store.fork(None);
            }
            SessionAction::Compact => {
                self.shell.store.compact();
            }
            SessionAction::Export => {
                self.shell.store.export();
            }
            SessionAction::CopyId => ctx.copy_text(id),
            SessionAction::Close => self.shell.store.close_active(),
            SessionAction::Archive => self.shell.store.archive(&id),
        }
    }

    /// Carries out what the approval card asked for.
    fn carry_approval(
        &mut self,
        store: &mut crate::store::Store,
        events: Vec<crate::ui::approval::ApprovalEvent>,
    ) {
        for event in events {
            match event {
                crate::ui::approval::ApprovalEvent::Reply(session, request, decision) => {
                    let _ = store.reply_permission(&session, request, &decision);
                }
            }
        }
    }

    fn show_dock(&mut self, ui: &mut egui::Ui, store: &mut crate::store::Store) {
        let actions = crate::ui::dock::show(ui, store);
        for action in actions {
            match action {
                crate::ui::dock::DockAction::Steer(index) => {
                    let _ = store.steer_queued(index);
                }
                crate::ui::dock::DockAction::Edit(index) => {
                    if let Some(text) = store
                        .active_session()
                        .and_then(|session| session.queue.get(index))
                        .map(|queued| {
                            queued
                                .prompt
                                .iter()
                                .filter_map(|block| block.as_text())
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                    {
                        self.shell.composer.set_draft(text);
                        store.withdraw_queued(index);
                    }
                }
                crate::ui::dock::DockAction::Remove(index) => {
                    store.withdraw_queued(index);
                }
                crate::ui::dock::DockAction::ScrollToPlan => {
                    self.scroll_to_plan(store);
                }
                crate::ui::dock::DockAction::OpenBackgroundAgents => {
                    // The workbench's agents list, which E06 mounts.
                }
                crate::ui::dock::DockAction::OpenGoal => {
                    // The goal editor is the plus menu's dialog, which
                    // E07 mounts; the pill reports the ask for now.
                }
            }
        }
    }

    /// Brings the plan card under review into view, or the transcript's
    /// last plan item when no review waits.
    fn scroll_to_plan(&mut self, store: &crate::store::Store) {
        let Some(session) = store.active_session() else {
            return;
        };
        let plan_row = session
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| matches!(item, kage_client::TranscriptItem::Plan { .. }))
            .map(|(ix, _)| ix)
            .next_back();
        if let Some(ix) = plan_row {
            self.shell
                .transcript
                .scroll_to_row(crate::ui::transcript::model::RowKey::Item(ix));
        }
    }

    /// Draws the composer: the field, its toolbar and the dock row's
    /// goal popover. A send takes the draft out to the store.
    /// The `:` line and the modeline, while vim mode is on. The
    /// command line runs one `crate::vim` command on Enter and closes
    /// on Escape, on a blur, or on a backspace over an empty line; the
    /// modeline names the mode, the model, the marks and the cursor
    /// the old shell's bar showed. Normal mode's keys answer while the
    /// bar holds no field, so no motion loses a letter to it.
    fn show_vim_bar(&mut self, ui: &mut egui::Ui, store: &mut crate::store::Store) {
        if !store.prefs().vim {
            return;
        }
        let palette = crate::theme::Palette::active_of(ui.ctx());
        let ctx = ui.ctx().clone();
        if self.shell.vim_line.is_some() {
            let mut buffer = self.shell.vim_line.take().unwrap_or_default();
            let mut submitted = None;
            let mut closed = false;
            ui.horizontal(|ui| {
                ui.set_height(VIM_LINE_H);
                ui.separator();
                ui.label(
                    egui::RichText::new(":")
                        .family(crate::theme::family(crate::theme::FAMILY_MONO_REGULAR))
                        .color(palette.muted),
                );
                let field = egui::TextEdit::singleline(&mut buffer)
                    .id(egui::Id::new("vim-line"))
                    .frame(egui::Frame::NONE)
                    .desired_width(ui.available_width())
                    .font(egui::FontId::new(
                        12.5,
                        crate::theme::family(crate::theme::FAMILY_MONO_REGULAR),
                    ));
                let response = ui.add(field);
                if std::mem::take(&mut self.shell.vim_line_focus) {
                    response.request_focus();
                }
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let backspace_out = buffer.is_empty()
                    && response.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Backspace));
                if response.lost_focus() && enter {
                    submitted = Some(std::mem::take(&mut buffer));
                } else if response.lost_focus() || backspace_out {
                    closed = true;
                }
            });
            if let Some(line) = submitted {
                self.shell.vim_submit = Some(line);
            } else if !closed {
                self.shell.vim_line = Some(buffer);
            }
            if closed {
                // No widget owns normal mode's focus; the line's field
                // just lets it go.
                ctx.memory_mut(|memory| {
                    memory.surrender_focus(egui::Id::new("vim-line"));
                });
            }
        }
        let mode = if !store.active_asks().is_empty() {
            crate::vim::Mode::Approval
        } else {
            let composer_focused =
                ctx.memory(|memory| memory.focused()) == Some(egui::Id::new("composer-input"));
            if composer_focused {
                crate::vim::Mode::Insert
            } else {
                crate::vim::Mode::Normal
            }
        };
        let option = |id: &str| store.composer_option(id).map(|o| o.current_value);
        let model = option("model").map(|model| match option("thinking") {
            Some(level) => format!("{model}@{level}"),
            None => model,
        });
        let session = store.active_session();
        let todos = session.and_then(crate::ui::dock::todos_state);
        let usage = session.filter(|s| s.usage.size > 0).map(|session| {
            let usage = &session.usage;
            format!(
                "{}% ({}/{})",
                (usage.fill() * 100.0).round() as i64,
                crate::ui::pickers::tokens(usage.used),
                crate::ui::pickers::tokens(usage.size)
            )
        });
        let cost = store
            .active_id()
            .and_then(|id| store.tree_cost(id))
            .map(|cost| format!("{} {:.2}", cost.currency, cost.amount));
        let connect = self.shell.store.connect();
        let link = if connect.is_connected() {
            self.shell.transport.link().name.to_owned()
        } else {
            connect.label()
        };
        let cursor = self
            .shell
            .transcript
            .cursor_row()
            .map(|row| (row + 1, self.shell.transcript.row_count()));
        let line = crate::vim::Modeline {
            mode: Some(mode),
            model,
            swarm: option("swarm").as_deref() == Some("on"),
            plan: store.plan_on(),
            todos: todos.map(|todos| (todos.done, todos.total)),
            path: session.and_then(|session| session.cwd.clone()),
            cursor,
            usage,
            cost,
            link,
        };
        ui.horizontal(|ui| {
            ui.set_height(VIM_MODEL_H);
            ui.separator();
            let ink = |text: String| {
                egui::RichText::new(text)
                    .family(crate::theme::family(crate::theme::FAMILY_MONO_REGULAR))
                    .size(crate::theme::FS_XS)
                    .color(palette.muted)
            };
            if let Some(mode) = line.mode {
                ui.label(
                    egui::RichText::new(crate::vim::mode_label(mode))
                        .family(crate::theme::family(crate::theme::FAMILY_MONO_REGULAR))
                        .size(crate::theme::FS_XS)
                        .color(palette.accent),
                );
            }
            if let Some(model) = line.model {
                ui.label(ink(model));
            }
            if line.swarm {
                ui.label(ink("swarm".to_owned()));
            }
            if line.plan {
                ui.label(ink("plan".to_owned()));
            }
            if let Some((done, total)) = line.todos {
                ui.label(ink(format!("todos {done}/{total}")));
            }
            if let Some(cursor) = line.cursor {
                ui.label(ink(format!("{}/{}", cursor.0, cursor.1)));
            }
            if let Some(usage) = line.usage {
                ui.label(ink(usage));
            }
            if let Some(cost) = line.cost {
                ui.label(ink(cost));
            }
            if let Some(path) = line.path {
                ui.label(ink(path));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(ink(line.link));
            });
        });
    }

    fn show_composer(&mut self, ui: &mut egui::Ui, store: &mut crate::store::Store) {
        // A welcome prompt whose session failed to open comes back
        // here, into an empty field, so the text is not lost with the
        // session it never rode.
        if let Some(text) = store.take_returned_prompt()
            && self.shell.composer.prompt().is_none()
        {
            self.shell.composer.set_draft(text);
        }
        let running = store
            .active_session()
            .is_some_and(|session| session.running);
        let enter_sends = store.prefs().enter_sends;
        let commands = store.composer_commands().to_vec();
        let listing = store
            .active_id()
            .and_then(|id| store.fs_listing(id))
            .cloned();
        self.shell.composer.set_listing(listing);
        let event = self
            .shell
            .composer
            .show(ui, store, &commands, running, enter_sends);
        self.handle_composer_event(ui, store, event, running);
    }

    /// Carries out what the composer's frame asked: dialogs, focus,
    /// and the send path, shared by the docked composer and the
    /// welcome pane's hosted one.
    fn handle_composer_event(
        &mut self,
        ui: &mut egui::Ui,
        store: &mut crate::store::Store,
        event: crate::ui::composer::ComposerEvent,
        running: bool,
    ) {
        let steer_available = store.state().steer_available();
        match event {
            crate::ui::composer::ComposerEvent::OpenGoal => {
                self.shell
                    .dialogs
                    .show_dialog(crate::ui::dialog::DialogKind::Goal, store);
            }
            crate::ui::composer::ComposerEvent::ConfirmSwarm => self.ask_swarm(store),
            crate::ui::composer::ComposerEvent::LeaveInsert => {
                ui.ctx().memory_mut(|memory| {
                    memory.surrender_focus(egui::Id::new("composer-input"));
                });
            }
            crate::ui::composer::ComposerEvent::OpenProviders => {
                self.shell
                    .settings
                    .open(crate::ui::settings::Section::Providers);
            }
            crate::ui::composer::ComposerEvent::Stop => {
                store.cancel();
            }
            _ => {}
        }
        if let crate::ui::composer::ComposerEvent::Send { steer } = event
            && let Some(text) = self.shell.composer.take_prompt()
        {
            let builtin = crate::ui::composer::builtin(text.trim());
            let name = builtin.as_ref().map(|command| command.name);
            let ran = match builtin {
                Some(command) => self.run_builtin(store, command.name, command.args),
                None => BuiltinRun::NotMine,
            };
            match ran {
                BuiltinRun::Ran => {}
                BuiltinRun::NeedsSession => {
                    // A built-in that needs a session keeps its text,
                    // and the field says why it stayed.
                    self.shell.composer.set_draft(text.clone());
                    store.note(
                        kage_client::wire::NoticeTone::Info,
                        format!("/{} needs a session; open one first", name.unwrap_or("")),
                    );
                }
                BuiltinRun::NotMine => {
                    // With no session, the send opens the one the
                    // text rides on: the prompt goes out the moment
                    // the session is live. Ctrl+Enter steers a
                    // running turn; the engine advertises the
                    // capability, so a steer that cannot go out falls
                    // back to a plain submit.
                    if store.active_id().is_none() && !store.pending_prompt() {
                        store.open_with_prompt(&text);
                    } else if steer && running && steer_available {
                        let _ = store.steer(&text);
                    } else {
                        let _ = store.submit(&text);
                    }
                }
            }
        }
    }

    /// Turns swarm mode on, through the confirmation when the
    /// preference asks for one.
    fn ask_swarm(&mut self, store: &mut crate::store::Store) {
        if store.prefs().confirm_swarm {
            self.shell
                .dialogs
                .show_dialog(crate::ui::dialog::DialogKind::ConfirmSwarm, store);
        } else {
            let _ = store.set_option("swarm", "on");
        }
    }

    /// What became of a built-in composer command: the client ran
    /// it, it needs a session the shell does not have, or it is not
    /// one of the client's own and the text goes to the agent as
    /// plain text.
    fn run_builtin(
        &mut self,
        store: &mut crate::store::Store,
        name: &str,
        args: &str,
    ) -> BuiltinRun {
        match (name, args) {
            ("new", _) => {
                store.show_welcome();
                BuiltinRun::Ran
            }
            (_, _) if store.active_session().is_none() => BuiltinRun::NeedsSession,
            ("plan", "on") => {
                store.enter_plan();
                BuiltinRun::Ran
            }
            ("plan", "off") => {
                store.exit_plan();
                BuiltinRun::Ran
            }
            ("plan", _) => {
                if store.plan_on() {
                    store.exit_plan();
                } else {
                    store.enter_plan();
                }
                BuiltinRun::Ran
            }
            ("goal", "") => {
                self.shell
                    .dialogs
                    .show_dialog(crate::ui::dialog::DialogKind::Goal, store);
                BuiltinRun::Ran
            }
            ("goal", "clear") => {
                crate::ui::dock::save_goal(store, "");
                BuiltinRun::Ran
            }
            ("goal", text) => {
                crate::ui::dock::save_goal(store, text);
                BuiltinRun::Ran
            }
            ("swarm", "off") => {
                store.set_option("swarm", "off");
                BuiltinRun::Ran
            }
            ("swarm", "on" | "") => {
                self.ask_swarm(store);
                BuiltinRun::Ran
            }
            ("swarm", task) => {
                if store.set_option("swarm", "on") && store.submit(task).is_some() {
                    BuiltinRun::Ran
                } else {
                    BuiltinRun::NotMine
                }
            }
            _ => BuiltinRun::NotMine,
        }
    }

    /// The active key context: a modal above the shell.
    fn context(&self) -> crate::ui::keys::Context {
        if self.shell.settings.is_open() {
            return crate::ui::keys::Context::Settings;
        }
        if self.shell.dialogs.is_open() {
            return crate::ui::keys::Context::Dialog;
        }
        if self.shell.palette.is_open() {
            return crate::ui::keys::Context::Palette;
        }
        if self.shell.find.is_open() {
            return crate::ui::keys::Context::Find;
        }
        if self.shell.store.prefs().vim {
            // Vim normal mode owns the keys only while no field does:
            // the composer focused is insert mode, and the `:` line
            // holds its own keys.
            let focus = self.ctx.memory(|memory| memory.focused());
            let field = focus.is_some_and(|id| {
                id == egui::Id::new("composer-input") || id == egui::Id::new("vim-line")
            });
            if !field {
                return crate::ui::keys::Context::VimNormal;
            }
        }
        crate::ui::keys::Context::Shell
    }

    /// Reads the frame's key presses and carries out the actions the
    /// router dispatches. A modal context answers only its own keys.
    fn route_keys(&mut self, ctx: &egui::Context) {
        use crate::ui::keys::Press;
        let events: Vec<egui::Event> = ctx.input(|i| i.events.clone());
        for event in events {
            let egui::Event::Key {
                key,
                modifiers,
                pressed: true,
                ..
            } = event
            else {
                continue;
            };
            let press = Press {
                key: key_name(&key),
                mods: crate::ui::keys::Mods {
                    ctrl: modifiers.ctrl || modifiers.command,
                    shift: modifiers.shift,
                    alt: modifiers.alt,
                },
                repeat: false,
            };
            // The find bar's keys answer while its field holds the
            // keyboard, and around them the shell's own bindings keep
            // working, as the reference's do.
            let find_focused = ctx.memory(|memory| memory.has_focus(egui::Id::new("find-query")));
            let context = match self.context() {
                crate::ui::keys::Context::Find if !find_focused => crate::ui::keys::Context::Shell,
                context => context,
            };
            let action = crate::ui::keys::dispatch(context, &press).or_else(|| {
                (context == crate::ui::keys::Context::Find)
                    .then(|| crate::ui::keys::dispatch(crate::ui::keys::Context::Shell, &press))
                    .flatten()
            });
            let Some(action) = action else {
                // The approval card's digits are not the shell's
                // bindings: they answer the oldest open ask while the
                // card holds the keys and no field has them, so a digit
                // typed into the composer stays text.
                if press.mods == crate::ui::keys::Mods::default()
                    && ctx.memory(|memory| memory.focused()).is_none()
                    && let Some(digit) = press.key.parse::<usize>().ok()
                    && self.approve_digit(digit)
                {
                    continue;
                }
                continue;
            };
            self.carry(action);
        }
    }

    /// Reads the frame's key presses in vim normal mode and resolves
    /// them through the vim bindings, carrying the two-key prefixes
    /// across presses. The characters ride the text events, so a
    /// frame that carries any does not also read the key events for
    /// the same letters.
    fn vim_keys(&mut self, ctx: &egui::Context) {
        if self.context() != crate::ui::keys::Context::VimNormal {
            return;
        }
        let events: Vec<egui::Event> = ctx.input(|i| i.events.clone());
        let typed = events
            .iter()
            .any(|event| matches!(event, egui::Event::Text(_)));
        for event in events {
            let step = match event {
                egui::Event::Text(text) => {
                    // A multi-character text event is an IME commit,
                    // not a motion.
                    if text.chars().count() != 1 {
                        continue;
                    }
                    let c = text.chars().next().unwrap_or(' ');
                    if c == ':' {
                        self.shell.vim_prefix = None;
                        crate::ui::keys::VimStep::Action(crate::vim::Action::Command)
                    } else {
                        let key: &'static str = match c.to_ascii_lowercase() {
                            'j' => "j",
                            'k' => "k",
                            'g' => "g",
                            'z' => "z",
                            'i' => "i",
                            'a' => "a",
                            'n' => "n",
                            '/' => "/",
                            _ => {
                                self.shell.vim_prefix = None;
                                continue;
                            }
                        };
                        crate::ui::keys::dispatch_vim(
                            self.shell.vim_prefix,
                            &crate::ui::keys::Press {
                                key,
                                mods: crate::ui::keys::Mods {
                                    ctrl: false,
                                    shift: c.is_uppercase(),
                                    alt: false,
                                },
                                repeat: false,
                            },
                        )
                    }
                }
                egui::Event::Key {
                    key,
                    modifiers,
                    pressed: true,
                    ..
                } => {
                    let named = matches!(
                        key,
                        egui::Key::Escape
                            | egui::Key::Enter
                            | egui::Key::ArrowUp
                            | egui::Key::ArrowDown
                    );
                    if typed && !named && !modifiers.ctrl {
                        // The text events already carried the letter.
                        continue;
                    }
                    let key: &'static str = match key {
                        egui::Key::ArrowDown => "down",
                        egui::Key::ArrowUp => "up",
                        other => key_name(&other),
                    };
                    crate::ui::keys::dispatch_vim(
                        self.shell.vim_prefix,
                        &crate::ui::keys::Press {
                            key,
                            mods: crate::ui::keys::Mods {
                                ctrl: modifiers.ctrl || modifiers.command,
                                shift: modifiers.shift,
                                alt: modifiers.alt,
                            },
                            repeat: false,
                        },
                    )
                }
                _ => continue,
            };
            match step {
                crate::ui::keys::VimStep::Action(action) => {
                    self.shell.vim_prefix = None;
                    self.carry_vim(action, ctx);
                }
                crate::ui::keys::VimStep::Pending(prefix) => {
                    self.shell.vim_prefix = Some(prefix);
                }
                crate::ui::keys::VimStep::Nothing => {
                    self.shell.vim_prefix = None;
                }
            }
        }
    }

    /// Carries out one vim motion or mode key.
    fn carry_vim(&mut self, action: crate::vim::Action, ctx: &egui::Context) {
        use crate::vim::Action;
        match action {
            Action::Down => self.shell.transcript.move_cursor(1),
            Action::Up => self.shell.transcript.move_cursor(-1),
            Action::PageDown => self.shell.transcript.move_cursor(crate::vim::PAGE as isize),
            Action::PageUp => self
                .shell
                .transcript
                .move_cursor(-(crate::vim::PAGE as isize)),
            Action::Top => self.shell.transcript.jump_cursor(false),
            Action::Bottom => self.shell.transcript.jump_cursor(true),
            Action::Toggle => self.shell.transcript.toggle_vim_row(),
            Action::OpenAll => self.shell.transcript.fold_all(true),
            Action::CloseAll => self.shell.transcript.fold_all(false),
            Action::Insert => ctx.memory_mut(|memory| {
                memory.request_focus(egui::Id::new("composer-input"));
            }),
            Action::Find => self.shell.find.open(),
            Action::Next => {
                if let Some(key) = self.shell.find.step(false) {
                    self.shell.transcript.scroll_to_row(key);
                }
            }
            Action::Prev => {
                if let Some(key) = self.shell.find.step(true) {
                    self.shell.transcript.scroll_to_row(key);
                }
            }
            Action::Command => {
                self.shell.vim_line = Some(String::new());
                self.shell.vim_line_focus = true;
            }
            Action::Escape => {
                let running = self
                    .shell
                    .store
                    .active_session()
                    .is_some_and(|session| session.running || session.in_turn);
                if running {
                    let _ = self.shell.store.cancel();
                }
            }
            Action::LeaveInsert => ctx.memory_mut(|memory| {
                memory.surrender_focus(egui::Id::new("composer-input"));
            }),
        }
    }

    /// Runs one `:` command through the action its palette entry or
    /// control uses.
    fn run_vim(&mut self, command: crate::vim::Command, ctx: &egui::Context) {
        use crate::vim::Command;
        let warn = |shell: &mut Self, text: String| {
            shell.shell.toasts.push(
                crate::ui::toasts::ToastDraft {
                    tone: kage_client::wire::NoticeTone::Warn,
                    text,
                    action: crate::ui::toasts::ToastAction::None,
                },
                web_time::Instant::now(),
            );
        };
        match command {
            Command::Theme(Some(choice)) => {
                let known = match &choice {
                    crate::theme::ThemeChoice::System => true,
                    crate::theme::ThemeChoice::Named(name) => {
                        crate::themes::BUILTIN.contains(&name.as_str())
                            || self
                                .shell
                                .store
                                .themes()
                                .iter()
                                .any(|theme| &theme.name == name)
                    }
                };
                if !known {
                    warn(self, format!("Unknown theme: {}", choice.label()));
                    return;
                }
                self.prefs_theme = choice.clone();
                self.shell
                    .store
                    .update_prefs(|prefs| prefs.theme = choice.clone());
                crate::theme::apply_choice_with(&choice, ctx, &self.shell.theme_catalog);
            }
            Command::Theme(None) => {
                warn(
                    self,
                    "Name a theme: system, or one from Settings".to_owned(),
                );
            }
            Command::Model(name) => {
                let wanted = name.to_lowercase();
                let choice = self
                    .shell
                    .store
                    .composer_option("model")
                    .and_then(|option| {
                        option.options.into_iter().find(|choice| {
                            choice.value.to_lowercase() == wanted
                                || choice.name.to_lowercase().contains(&wanted)
                        })
                    })
                    .filter(|_| !wanted.is_empty());
                match choice {
                    Some(choice) => {
                        let _ = self.shell.store.set_option("model", &choice.value);
                    }
                    None => warn(self, format!("No model matches \"{name}\"")),
                }
            }
            Command::Swarm(on) => {
                let current = self
                    .shell
                    .store
                    .composer_option("swarm")
                    .is_some_and(|option| option.current_value == "on");
                if on == current {
                    return;
                }
                if on {
                    if self.shell.store.prefs().confirm_swarm {
                        self.shell.dialogs.show_dialog(
                            crate::ui::dialog::DialogKind::ConfirmSwarm,
                            &mut self.shell.store,
                        );
                    } else {
                        let _ = self.shell.store.set_option("swarm", "on");
                    }
                } else {
                    let _ = self.shell.store.set_option("swarm", "off");
                }
            }
            Command::Plan(on) => {
                let _ = if on {
                    self.shell.store.enter_plan()
                } else {
                    self.shell.store.exit_plan()
                };
            }
            Command::Goal(goal) => {
                let _ = self
                    .shell
                    .store
                    .set_option("goal", &goal.unwrap_or_default());
            }
            Command::New => self.shell.store.show_welcome(),
            Command::Settings => self
                .shell
                .settings
                .open(crate::ui::settings::Section::General),
            Command::Help => self
                .shell
                .settings
                .open(crate::ui::settings::Section::Keyboard),
            Command::Compact => {
                let _ = self.shell.store.compact();
            }
            Command::NoHighlight => self.shell.find.close(),
            Command::Quit => self.shell.panels.workbench_visible = false,
            Command::Vim(on) => {
                self.shell.store.update_prefs(|prefs| prefs.vim = on);
            }
            Command::Nothing => {}
            Command::Unknown(word) => warn(self, format!("Not an editor command: {word}")),
        }
    }

    /// Answers the oldest open permission ask with the option the
    /// 1-based `digit` names. Reports whether the card took it, so a
    /// digit aimed at a field or at the shell is left alone.
    fn approve_digit(&mut self, digit: usize) -> bool {
        let Some((session_id, ask)) = self
            .shell
            .store
            .active_asks()
            .first()
            .map(|(session_id, ask)| ((*session_id).to_owned(), (*ask).clone()))
        else {
            return false;
        };
        let Some(action) = self.shell.approval.digit(digit, &ask) else {
            return false;
        };
        let decision = match action {
            crate::ui::approval::DigitAction::Answer(index) => ask
                .options
                .get(index)
                .map(|option| kage_client::PermissionDecision::Option(option.option_id.clone())),
            crate::ui::approval::DigitAction::Toggle(index) => {
                self.shell.approval.toggle(ask.request_id.clone(), index);
                return true;
            }
        };
        if let Some(decision) = decision {
            let _ =
                self.shell
                    .store
                    .reply_permission(&session_id, ask.request_id.clone(), &decision);
            self.shell.approval.forget(&ask.request_id);
        }
        true
    }

    /// Carries out the row the command palette ran.
    fn run_palette_entry(&mut self, entry: crate::ui::palette::PaletteEntry, ctx: &egui::Context) {
        use crate::ui::palette::{Action, PaletteEntry};
        use crate::ui::workbench::Tab;
        ctx.request_repaint();
        match entry {
            PaletteEntry::Action { action, .. } => match action {
                Action::NewSession => self.carry(crate::ui::keys::Action::NewSession),
                Action::Workbench => self.carry(crate::ui::keys::Action::ToggleWorkbench),
                Action::Sidebar => self.carry(crate::ui::keys::Action::ToggleSidebar),
                Action::Settings | Action::Theme => {
                    self.carry(crate::ui::keys::Action::OpenSettings);
                }
                Action::Agents => self.show_workbench_tab(Tab::Agents),
                Action::Changes => self.show_workbench_tab(Tab::Changes),
                Action::Goal => {
                    self.shell
                        .dialogs
                        .show_dialog(crate::ui::dialog::DialogKind::Goal, &mut self.shell.store);
                }
                Action::Plan => {
                    if self.shell.store.plan_on() {
                        self.shell.store.exit_plan();
                    } else {
                        self.shell.store.enter_plan();
                    }
                }
                Action::Swarm => {
                    let on = self
                        .shell
                        .store
                        .composer_option("swarm")
                        .is_some_and(|option| option.current_value == "on");
                    if on {
                        self.shell.store.set_option("swarm", "off");
                    } else if self.shell.store.prefs().confirm_swarm {
                        self.shell.dialogs.show_dialog(
                            crate::ui::dialog::DialogKind::ConfirmSwarm,
                            &mut self.shell.store,
                        );
                    } else {
                        self.shell.store.set_option("swarm", "on");
                    }
                }
            },
            PaletteEntry::Session { id, .. } => self.shell.store.set_active(id),
            PaletteEntry::Command { name, .. } => {
                self.shell.composer.set_draft(format!("/{name} "));
                ctx.memory_mut(|memory| {
                    memory.request_focus(egui::Id::new("composer-input"));
                });
            }
        }
    }

    /// Shows the workbench on `tab`.
    fn show_workbench_tab(&mut self, tab: crate::ui::workbench::Tab) {
        self.shell.panels.workbench_visible = true;
        self.shell.workbench.set_tab(tab);
    }

    /// Carries out one routed action.
    fn carry(&mut self, action: crate::ui::keys::Action) {
        use crate::ui::keys::Action;
        match action {
            Action::Quit => {
                self.shell.smoke_deadline =
                    Some((web_time::Instant::now(), Duration::from_millis(0)));
            }
            Action::NewSession => self.shell.store.show_welcome(),
            Action::ToggleSidebar => self.shell.panels.toggle_sidebar(),
            Action::ToggleWorkbench => self.shell.panels.toggle_workbench(),
            Action::OpenPalette => self.shell.palette.open(),
            Action::OpenFind => self.shell.find.open(),
            Action::FindNext | Action::FindPrev => {
                if let Some(key) = self.shell.find.step(action == Action::FindPrev) {
                    self.shell.transcript.scroll_to_row(key);
                }
            }
            Action::FindClose => {
                self.shell.find.close();
                find_closed(&self.ctx, &self.shell.store);
            }
            Action::OpenSettings => self
                .shell
                .settings
                .open(crate::ui::settings::Section::General),
            Action::OpenProviders => self
                .shell
                .settings
                .open(crate::ui::settings::Section::Providers),
            Action::CloseDialog => {
                if self.shell.settings.is_open() {
                    // A form, menu or one-edit field over the page
                    // answers the key in its own draw, so the dialog
                    // waits for it to be gone before it closes.
                    if !self.shell.settings.holds_escape() {
                        self.shell.settings.close();
                    }
                } else {
                    self.shell.dialogs.close();
                }
            }
            Action::CycleMode => self.cycle_permission(),
            Action::ShowAgents => self.show_workbench_tab(crate::ui::workbench::Tab::Agents),
            Action::ReviewChanges => self.show_workbench_tab(crate::ui::workbench::Tab::Changes),
            Action::SetGoal => {
                self.shell
                    .dialogs
                    .show_dialog(crate::ui::dialog::DialogKind::Goal, &mut self.shell.store);
            }
            Action::SendPrompt | Action::ToggleSwarm | Action::TogglePlan => {}
        }
    }

    /// Runs the daily release check on its own thread and applies the
    /// answer on the UI thread: the check blocks on the network, so it
    /// never sits in the frame. The browser build never checks.
    #[cfg(not(target_arch = "wasm32"))]
    fn pump_update_check(&mut self) {
        let now = crate::clock::unix_seconds();
        if now < self.last_update_check + 24 * 60 * 60 {
            return;
        }
        self.last_update_check = now;
        let prefs = self.shell.store.prefs().clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("kage-update-check".to_owned())
            .spawn(move || {
                let outcome = crate::update::run_check(&prefs, false);
                let _ = tx.send(outcome);
            })
            .expect("the check thread starts");
        self.update_rx = Some(rx);
        self.announce_update = true;
    }

    /// Opens the system folder picker on its own thread, when the engine
    /// is local and no picker is open yet. Answers whether it did; the
    /// caller shows the engine's folder listing otherwise.
    #[cfg(not(target_arch = "wasm32"))]
    fn open_system_folder_picker(&mut self) -> bool {
        if self.remote {
            return false;
        }
        if self.folder_rx.is_some() {
            return true;
        }
        let start = self.shell.store.session_dir().map(str::to_owned);
        let ctx = self.ctx.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("kage-folder-picker".to_owned())
            .spawn(move || {
                let _ = tx.send(crate::picker::pick_folder(start));
                ctx.request_repaint();
            })
            .expect("the folder picker thread starts");
        self.folder_rx = Some(rx);
        true
    }

    /// The browser has no local folders to pick from.
    #[cfg(target_arch = "wasm32")]
    fn open_system_folder_picker(&mut self) -> bool {
        false
    }

    /// Takes the folder the system picker answered with, if it has, as
    /// the project the next session opens in. A cancelled picker
    /// leaves the project as it was, and one that failed opens the
    /// engine's folder listing instead.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_folder(&mut self) {
        let Some(rx) = &self.folder_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(picked) => {
                self.folder_rx = None;
                match picked {
                    crate::picker::Picked::Folder(path) => {
                        self.shell.store.set_project(Some(path));
                    }
                    crate::picker::Picked::Cancelled => {}
                    crate::picker::Picked::Failed => {
                        self.shell.dialogs.show_dialog(
                            crate::ui::dialog::DialogKind::OpenFolder,
                            &mut self.shell.store,
                        );
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.folder_rx = None,
        }
    }

    /// Runs a release check on its own thread when the daily throttle
    /// allows it, or past it when `force` is set, and applies the
    /// answer on the UI thread.
    fn check_releases(&mut self, force: bool) {
        if self.update_rx.is_some() {
            return;
        }
        let now = crate::clock::unix_seconds();
        if !force && now < self.last_update_check + 24 * 60 * 60 {
            return;
        }
        self.last_update_check = now;
        let prefs = self.shell.store.prefs().clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("kage-update-check".to_owned())
            .spawn(move || {
                let outcome = crate::update::run_check(&prefs, force);
                let _ = tx.send(outcome);
            })
            .expect("the check thread starts");
        self.update_rx = Some(rx);
        self.announce_update = false;
    }

    /// Downloads the newest release of `which` on its own thread: the
    /// download blocks on the network, so it never sits in the frame.
    /// One at a time; a second click while one runs is ignored. The
    /// browser never installs anything, so it never gets here.
    #[cfg(not(target_arch = "wasm32"))]
    fn start_install(&mut self, which: crate::ui::settings::Install) {
        if self.installing.is_some() {
            return;
        }
        self.installing = Some(which);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("kage-install".to_owned())
            .spawn(move || {
                let result = match which {
                    crate::ui::settings::Install::Engine => crate::update::install_latest(),
                    crate::ui::settings::Install::Desktop => {
                        crate::update::install_desktop_latest()
                    }
                };
                let _ = tx.send((which, result));
            })
            .expect("the install thread starts");
        self.install_rx = Some(rx);
    }

    /// Applies a finished install: the engine is restarted on the new
    /// binary, the desktop waits for the next launch, and a refusal
    /// rides a toast.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_install(&mut self) {
        let Some(rx) = self.install_rx.take() else {
            return;
        };
        let Ok((which, result)) = rx.try_recv() else {
            self.install_rx = Some(rx);
            return;
        };
        self.installing = None;
        let ctx = self.ctx.clone();
        let (tone, text) = match result {
            Ok(path) if which == crate::ui::settings::Install::Engine => {
                let program = path.display().to_string();
                self.shell
                    .store
                    .update_prefs(|prefs| prefs.kage_path = Some(program.clone()));
                self.shell.use_engine(program, &ctx);
                (
                    kage_client::wire::NoticeTone::Success,
                    "engine installed; restarting it".to_owned(),
                )
            }
            Ok(path) => (
                kage_client::wire::NoticeTone::Success,
                format!(
                    "desktop installed at {}; restart kage to apply",
                    path.display()
                ),
            ),
            Err(why) => (kage_client::wire::NoticeTone::Error, why),
        };
        self.install_note = Some((
            matches!(tone, kage_client::wire::NoticeTone::Success),
            text.clone(),
        ));
        self.shell.store.note(tone, text);
    }

    /// Applies a finished release check to the preferences, once. The
    /// browser never checks, so this is native-only too.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_update(&mut self) {
        let Some(rx) = self.update_rx.take() else {
            return;
        };
        let Ok(outcome) = rx.try_recv() else {
            self.update_rx = Some(rx);
            return;
        };
        let Some(outcome) = outcome else {
            return;
        };
        let engine = self
            .shell
            .store
            .state()
            .agent
            .as_ref()
            .and_then(|agent| agent.version.clone());
        let prefs = self.shell.store.prefs().clone();
        let mut prefs = prefs;
        crate::update::apply_outcome(&mut prefs, outcome, engine.as_deref());
        self.shell
            .store
            .update_prefs(|current| *current = prefs.clone());
        if !std::mem::take(&mut self.announce_update) {
            return;
        }
        let Some(text) = crate::update::pending_update(&prefs, engine.as_deref()) else {
            return;
        };
        self.shell.toasts.push(
            crate::ui::toasts::ToastDraft {
                tone: kage_client::wire::NoticeTone::Info,
                text,
                action: crate::ui::toasts::ToastAction::OpenUpdates,
            },
            web_time::Instant::now(),
        );
    }
}

/// What a session's row-state move raises, when it raises: the first
/// sight of a session and an unchanged state stay quiet, an ask, a
/// plan and a failure speak from any prior state, and only a running
/// turn that finished says so.
#[must_use]
fn away_toast(
    was: Option<crate::ui::sidebar::RowState>,
    state: crate::ui::sidebar::RowState,
) -> Option<(kage_client::wire::NoticeTone, &'static str)> {
    use crate::ui::sidebar::RowState;
    if was.is_none() || was == Some(state) {
        return None;
    }
    match (was, state) {
        (_, RowState::Approve) => {
            Some((kage_client::wire::NoticeTone::Info, "needs your approval"))
        }
        (_, RowState::Review) => {
            Some((kage_client::wire::NoticeTone::Info, "has a plan to review"))
        }
        (_, RowState::Failed) => Some((kage_client::wire::NoticeTone::Error, "turn failed")),
        (Some(RowState::Running), RowState::Idle) => {
            Some((kage_client::wire::NoticeTone::Success, "turn finished"))
        }
        _ => None,
    }
}

/// The prompt the recording expects, once its session is open.
const REPLAY_PROMPT: &str = "fix the null check";

/// The composer column's height: the field's minimum, its toolbar and
/// the hint line under it.
/// What became of a built-in composer command: the client ran it, it
/// needs a session the shell does not have, or it is not one of the
/// client's own and the text goes to the agent as plain text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuiltinRun {
    /// The client carried the command out.
    Ran,
    /// The command needs a session and none is open.
    NeedsSession,
    /// Not one of the client's own commands.
    NotMine,
}

/// The vim modeline's height, and the `:` line's above it.
const VIM_MODEL_H: f32 = 26.0;
const VIM_LINE_H: f32 = 28.0;

/// Lays `add` out in the design's content column: the parent's width
/// up to [`crate::theme::CONTENT_W`], centered.
fn centered_column<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let free = ui.available_rect_before_wrap();
    let width = free.width().min(crate::theme::CONTENT_W);
    let rect = egui::Rect::from_min_size(
        egui::pos2(
            crate::theme::snap(free.center().x - width / 2.0),
            free.top(),
        ),
        egui::vec2(width, free.height()),
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        add,
    )
    .inner
}

/// The name the router spells a key by: the key's own name, lowercased
/// for the letter keys, as egui prints it.
fn key_name(key: &egui::Key) -> &'static str {
    match key {
        egui::Key::Enter => "Enter",
        egui::Key::Escape => "Escape",
        egui::Key::Tab => "Tab",
        egui::Key::ArrowUp => "ArrowUp",
        egui::Key::ArrowDown => "ArrowDown",
        egui::Key::ArrowLeft => "ArrowLeft",
        egui::Key::ArrowRight => "ArrowRight",
        other => other.symbol_or_name(),
    }
}

/// Parses the flags and runs until quit. Native only: the browser
/// entry is [`crate::web::start`].
#[cfg(not(target_arch = "wasm32"))]
pub fn run() {
    let launch = Launch::parse(std::env::args().skip(1));
    if let Some(error) = &launch.error {
        eprintln!("kage-desktop: {error}");
        std::process::exit(2);
    }
    let smoke_millis = launch.smoke_millis;
    let stream = launch.stream;
    let replay = launch.wire == Wire::Replay;
    let remote = matches!(launch.wire, Wire::WebSocket { .. });
    let prefs = crate::prefs::load();
    let cwd = std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_owned());
    let saved = prefs.kage_path.clone();
    let transport = launch.transport(saved.as_deref());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("kage")
            // Matches the `kage.desktop` entry the release ships, so a
            // Wayland shell groups the window under its launcher.
            .with_app_id("kage")
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([960.0, 640.0])
            // The runtime icon applies on X11 only; the other
            // platforms take the icon from the packaged resources.
            .with_icon(crate::icon::window_icon().unwrap_or_default()),
        ..Default::default()
    };
    #[cfg(target_os = "linux")]
    let appearance = crate::appearance::watch();
    eframe::run_native(
        "kage",
        options,
        Box::new(move |cc| {
            let theme = prefs.theme.clone();
            let mut app = ShellApp::new(cwd, replay, transport, &cc.egui_ctx).with_prefs(prefs);
            app.stream = stream;
            app.remote = remote;
            #[cfg(target_os = "linux")]
            appearance.attach(&cc.egui_ctx);
            crate::theme::apply_choice(&theme, &cc.egui_ctx);
            if let Some(millis) = smoke_millis {
                app.shell.smoke_deadline =
                    Some((web_time::Instant::now(), Duration::from_millis(millis)));
            }
            Ok(Box::new(app))
        }),
    )
    .expect("failed to open the window");
}

/// Whether `now` asks for something `asked` did not, remembering it.
/// `None` asks for nothing and forgets, so the next visit asks again.
fn newly_wanted<T: PartialEq>(asked: &mut Option<T>, now: Option<T>) -> bool {
    let wanted = now.is_some() && *asked != now;
    *asked = now;
    wanted
}

#[cfg(test)]
mod tests {
    use super::{Launch, Wire, away_toast, newly_wanted};
    use crate::ui::sidebar::RowState;

    fn parse(args: &[&str]) -> Launch {
        Launch::parse(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn an_away_session_speaks_from_any_prior_state_but_the_first() {
        use kage_client::wire::NoticeTone;
        assert_eq!(away_toast(None, RowState::Approve), None, "first sight");
        assert_eq!(
            away_toast(Some(RowState::Running), RowState::Approve),
            Some((NoticeTone::Info, "needs your approval"))
        );
        assert_eq!(
            away_toast(Some(RowState::Idle), RowState::Review),
            Some((NoticeTone::Info, "has a plan to review"))
        );
        assert_eq!(
            away_toast(Some(RowState::Review), RowState::Failed),
            Some((NoticeTone::Error, "turn failed"))
        );
        assert_eq!(
            away_toast(Some(RowState::Running), RowState::Idle),
            Some((NoticeTone::Success, "turn finished"))
        );
    }

    #[test]
    fn punctuation_keys_reach_their_bindings() {
        use crate::ui::keys::{Action, Context, Mods, Press, dispatch};
        let ctrl = |key: egui::Key| Press {
            key: super::key_name(&key),
            mods: Mods {
                ctrl: true,
                ..Mods::default()
            },
            repeat: false,
        };
        assert_eq!(
            dispatch(Context::Shell, &ctrl(egui::Key::Comma)),
            Some(Action::OpenSettings)
        );
        assert_eq!(
            dispatch(Context::Shell, &ctrl(egui::Key::Backslash)),
            Some(Action::ToggleSidebar)
        );
        assert_eq!(
            dispatch(Context::Shell, &ctrl(egui::Key::K)),
            Some(Action::OpenPalette)
        );
    }

    #[test]
    fn an_open_page_asks_for_config_once_per_visit() {
        let mut asked = None;
        let asks: Vec<bool> = [Some(1), Some(1), Some(1), Some(2), None, Some(2)]
            .into_iter()
            .map(|now| newly_wanted(&mut asked, now))
            .collect();
        assert_eq!(asks, [true, false, false, true, false, true]);
    }

    #[test]
    fn quiet_moves_raise_nothing() {
        assert_eq!(away_toast(Some(RowState::Idle), RowState::Idle), None);
        assert_eq!(away_toast(Some(RowState::Idle), RowState::Running), None);
        assert_eq!(
            away_toast(Some(RowState::Failed), RowState::Idle),
            None,
            "a finish only follows a run"
        );
        assert_eq!(away_toast(None, RowState::Idle), None);
    }

    #[test]
    fn defaults_have_no_smoke_or_stream_and_speak_stdio() {
        let launch = parse(&[]);
        assert_eq!(launch.smoke_millis, None);
        assert!(!launch.stream);
        assert_eq!(launch.wire, Wire::Stdio);
        assert!(launch.rpc_bin.is_none());
    }

    #[test]
    fn smoke_takes_an_optional_millis_value() {
        assert_eq!(parse(&["--smoke", "2500"]).smoke_millis, Some(2500));
        assert_eq!(parse(&["--smoke"]).smoke_millis, Some(0));
    }

    #[test]
    fn stream_flag_stands_alone() {
        let launch = parse(&["--stream"]);
        assert!(launch.stream);
        assert_eq!(launch.smoke_millis, None);
    }

    #[test]
    fn replay_selects_the_recording() {
        assert_eq!(parse(&["--replay"]).wire, Wire::Replay);
    }

    #[test]
    fn ws_needs_a_token_and_the_token_needs_ws() {
        let launch = parse(&["--ws", "ws://127.0.0.1:7433/acp"]);
        assert!(launch.error.is_some(), "a tokenless ws is refused");

        let launch = parse(&["--token", "t"]);
        assert!(launch.error.is_some(), "a token without ws is refused");

        let launch = parse(&["--ws", "ws://127.0.0.1:7433/acp", "--token", "t"]);
        assert_eq!(
            launch.wire,
            Wire::WebSocket {
                url: "ws://127.0.0.1:7433/acp".to_owned(),
                token: "t".to_owned(),
            }
        );
    }

    #[test]
    fn rpc_bin_cleans_quotes_spaces_and_unicode() {
        assert_eq!(
            parse(&["--rpc-bin", " \"/opt/my dir/kage\" "])
                .rpc_bin
                .as_deref(),
            Some("/opt/my dir/kage")
        );
        assert_eq!(
            parse(&["--rpc-bin", "'/opt/kage'"]).rpc_bin.as_deref(),
            Some("/opt/kage")
        );
        assert_eq!(
            parse(&["--rpc-bin", " ~/k\u{e4}g\u{e9} "])
                .rpc_bin
                .as_deref(),
            Some("~/k\u{e4}g\u{e9}")
        );
        assert_eq!(
            parse(&["--rpc-bin", "\"/opt/kage"]).rpc_bin.as_deref(),
            Some("\"/opt/kage"),
            "an unmatched quote stays"
        );
    }

    #[test]
    fn an_rpc_bin_that_cleans_to_empty_is_absent() {
        let launch = parse(&["--rpc-bin", "\"\""]);
        assert!(launch.rpc_bin.is_none());
        assert!(launch.error.is_none());
        assert_eq!(launch.wire, Wire::Stdio);
    }

    #[test]
    fn rpc_bin_overrides_the_engine_binary() {
        assert_eq!(
            parse(&["--rpc-bin", "/opt/kage"]).rpc_bin.as_deref(),
            Some("/opt/kage")
        );
        assert_eq!(parse(&["--rpc-bin", "/opt/kage"]).wire, Wire::Stdio);
    }

    #[test]
    fn unknown_flags_are_refused_and_listed() {
        let launch = parse(&["--typo"]);
        let error = launch.error.expect("the typo is refused");
        assert!(error.contains("--typo"), "{error}");

        let launch = parse(&["--first-typo", "--second-typo"]);
        let error = launch.error.expect("both typos are listed");
        assert!(
            error.contains("--first-typo") && error.contains("--second-typo"),
            "{error}"
        );
    }

    #[test]
    fn an_equals_form_value_flag_is_refused_with_the_space_form() {
        for arg in ["--ws=ws://127.0.0.1:7433/acp", "--token=t"] {
            let launch = parse(&[arg]);
            let error = launch.error.unwrap_or_else(|| panic!("{arg} is refused"));
            assert!(error.contains("space"), "{error}");
        }

        let launch = parse(&["--ws=", "--token=t"]);
        assert!(launch.error.is_some(), "an empty equals value is refused");
        assert!(launch.ws_url.is_none(), "nothing became a URL");
        assert_eq!(launch.wire, Wire::Stdio, "the launch never misroutes");
    }
}
