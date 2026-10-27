//! The sans-IO client.
//!
//! A host feeds every frame the transport receives into
//! [`Client::handle`] and drains [`Client::take_outgoing`] to send
//! what the client produced. The client owns the request ids and the
//! pending requests, so answers land on the session that asked, and
//! it keeps [`State`] exactly as the frames delivered it. There is no
//! thread, no clock and no IO here, which makes the client testable
//! from recorded transcripts and usable on wasm.

use std::collections::BTreeMap;

use serde_json::Value;

use kage_acp_wire::{
    CancelNotification, ClientCapabilities, CloseSessionRequest, CompactionPhase, ConfigGetRequest,
    ContentBlock, FsOp, FsRequest, Implementation, InitializeRequest, KageMeta,
    ListSessionsRequest, LoadSessionRequest, McpServer, ModelsResponse, NewSessionRequest,
    OptionSetRequest, OptionsResponse, PROTOCOL_VERSION, PermissionOptionKind, PermissionOutcome,
    PlanReview, PromptDelivery, PromptRef, PromptRequest, PromptResponse, QuestionMeta,
    RequestMeta, RequestPermissionRequest, RequestPermissionResult, ResumeSessionRequest,
    SelectedOption, SessionExportResponse, SessionForkRequest, SessionForkResponse,
    SessionNotification, SessionRenameRequest, SessionRequest, SessionUpdate,
    SetSessionConfigOptionRequest, SwarmResumeRequest,
};

use crate::change::Change;
use crate::frame::{Frame, RequestId, RpcError};
use crate::state::{
    PermissionAsk, QueuedPrompt, Session, State, ToolCallItem, TranscriptItem, Usage,
};

/// The method an agent calls to ask for a tool call's verdict.
const ASK_METHOD: &str = "session/request_permission";

/// Why a steer did not go out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerError {
    /// No run is in flight on the session, so there is nothing to
    /// steer. Send the prompt plain instead.
    NotRunning,
    /// The agent did not advertise the steering capability, so the
    /// wire cannot join a run in flight. The prompt queues instead.
    NotAdvertised,
}

/// What [`Client::prompt`] did with a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptOutcome {
    /// Sent plain: the session was idle, so the run starts now.
    Sent {
        /// The id the stop reason comes back on.
        request_id: u64,
    },
    /// Sent with the steer marker to join the run already in flight
    /// at its next turn boundary. Only when the agent advertised the
    /// steering capability.
    Steered {
        /// The id the stop reason comes back on.
        request_id: u64,
    },
    /// Held in the session's queue because a run was in flight and
    /// the agent cannot steer. The client sends it itself, plain,
    /// when the run ends.
    Queued,
}

/// The verdict a user makes on a permission ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Pick the ask's `allow_once` option.
    Allow,
    /// Pick the ask's `allow_always` option.
    AllowAlways,
    /// Pick the ask's `reject_once` option.
    Reject,
    /// Pick the option with this id and carry `feedback` as the
    /// user's own words through the `_meta.kage.planReview` channel:
    /// the revise and reject-with-feedback answers of a plan review
    /// ride it.
    Feedback {
        /// The offered option the verdict picks, verbatim.
        option_id: String,
        /// The text the user typed.
        feedback: String,
    },
    /// Answer with the cancelled outcome, as when the dialog is
    /// dismissed without a choice.
    Cancel,
    /// Pick the option with this id verbatim, for asks whose options
    /// a host shows by id.
    Option(String),
    /// Answer a question: pick the option with this id and carry the
    /// whole answer, several choices or the user's own words, through
    /// `_meta.kage.question`.
    Answer {
        /// The offered option the answer picks, verbatim.
        option_id: String,
        /// The labels picked, or the user's own words.
        answer: Vec<String>,
    },
}

/// What the client is still waiting to hear back about a request.
#[derive(Debug, Clone, PartialEq)]
enum Pending {
    Initialize,
    NewSession {
        cwd: String,
    },
    Open {
        session_id: String,
        /// The session the open replaced, put back when it fails.
        snapshot: Box<Session>,
    },
    List {
        /// The directory filter of the first page, kept for the next.
        cwd: Option<String>,
    },
    Prompt {
        session_id: String,
        /// Whether the answer ends the run: false for a prompt that
        /// only steered the run another request owns.
        owns_run: bool,
    },
    SetConfigOption {
        session_id: String,
        config_id: String,
        /// The value shown before the change, restored if the agent
        /// refuses it.
        previous: Option<String>,
    },
    Close {
        session_id: String,
    },
    ConfigGet,
    Fs {
        session_id: String,
    },
    SwarmResume {
        session_id: String,
    },
    Fork {
        session_id: String,
    },
    Export {
        session_id: String,
    },
    Compact {
        session_id: String,
    },
    Rename {
        session_id: String,
    },
    Models,
    Options,
    ConfigTest,
    AuthSet {
        provider: String,
    },
    Directory,
    Folders,
    Themes,
    Plugins,
    Registry,
    RegistryInstall,
    ImageSave,
}

/// The client side of an ACP connection.
#[derive(Debug, Default)]
pub struct Client {
    state: State,
    next_id: u64,
    pending: BTreeMap<RequestId, Pending>,
    outgoing: Vec<Frame>,
}

impl Client {
    /// A client that has sent nothing and heard nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The state as the handled frames left it.
    #[must_use]
    pub fn state(&self) -> &State {
        &self.state
    }

    /// The frames to send, in the order they were produced, drained
    /// by the call.
    pub fn take_outgoing(&mut self) -> Vec<Frame> {
        std::mem::take(&mut self.outgoing)
    }

    /// Stores the composer draft of `session_id`. See
    /// [`State::set_draft`].
    pub fn set_draft(&mut self, session_id: &str, text: &str) -> bool {
        self.state.set_draft(session_id, text)
    }

    /// Applies one incoming message and reports what moved. Server
    /// requests are answered here too: an unknown method is refused
    /// with a method-not-found error, and a malformed permission ask
    /// with an invalid-params error, both into the outgoing queue.
    pub fn handle(&mut self, frame: Frame) -> Vec<Change> {
        match frame {
            Frame::Request { id, method, params } => {
                self.handle_server_request(id, &method, params)
            }
            Frame::Notification { method, params } => self.handle_notification(&method, params),
            Frame::Success { id, result } => self.handle_success(&id, result),
            Frame::Failure { id, error } => self.handle_failure(&id, error),
        }
    }

    /// Starts the handshake. The answer carries the protocol version,
    /// the agent identity and the capabilities every later command
    /// checks, steering among them.
    pub fn initialize(
        &mut self,
        client_capabilities: ClientCapabilities,
        client_info: Option<Implementation>,
    ) -> u64 {
        self.request(
            "initialize",
            params(&InitializeRequest {
                protocol_version: PROTOCOL_VERSION,
                client_capabilities,
                client_info,
            }),
            Pending::Initialize,
        )
    }

    /// Opens a new session in `cwd`.
    pub fn new_session(&mut self, cwd: &str, mcp_servers: &[McpServer]) -> u64 {
        self.request(
            "session/new",
            params(&NewSessionRequest {
                cwd: cwd.to_owned(),
                mcp_servers: mcp_servers.to_vec(),
            }),
            Pending::NewSession {
                cwd: cwd.to_owned(),
            },
        )
    }

    /// Opens a recorded session and replays its transcript, which
    /// streams as updates before this answer arrives. Returns the
    /// request id and the change the reset of the local session made.
    pub fn load_session(
        &mut self,
        session_id: &str,
        cwd: &str,
        mcp_servers: &[McpServer],
    ) -> (u64, Vec<Change>) {
        self.open("session/load", session_id, cwd, mcp_servers)
    }

    /// Opens a recorded session without replaying its history. Returns
    /// the request id and the change the reset of the local session
    /// made.
    pub fn resume_session(
        &mut self,
        session_id: &str,
        cwd: &str,
        mcp_servers: &[McpServer],
    ) -> (u64, Vec<Change>) {
        self.open("session/resume", session_id, cwd, mcp_servers)
    }

    /// Asks for one page of recorded sessions. Pages land in
    /// [`State::directory`], entries a page repeats are refreshed.
    /// A page that names a next cursor asks for that page itself, so
    /// the directory ends up whole.
    pub fn list_sessions(&mut self, cwd: Option<&str>, cursor: Option<&str>) -> u64 {
        self.request(
            "session/list",
            params(&ListSessionsRequest {
                cwd: cwd.map(str::to_owned),
                cursor: cursor.map(str::to_owned),
            }),
            Pending::List {
                cwd: cwd.map(str::to_owned),
            },
        )
    }

    /// Runs one prompt on `session_id`. A prompt that arrives while a
    /// run is in flight is held in the session's queue and sent plain
    /// when the run ends; joining a run in flight on purpose goes
    /// through [`Client::steer`].
    pub fn prompt(&mut self, session_id: &str, prompt: Vec<ContentBlock>) -> PromptOutcome {
        let running = self
            .state
            .sessions
            .get(session_id)
            .is_some_and(|session| session.running);
        if running {
            self.session_mut(session_id)
                .queue
                .push(QueuedPrompt { prompt });
            return PromptOutcome::Queued;
        }
        let request_id = self.send_prompt(session_id, prompt, None, true);
        PromptOutcome::Sent { request_id }
    }

    /// Steers the run in flight on `session_id` with `prompt`, joining
    /// it at its next turn boundary. Only when a run is in flight and
    /// the agent advertised the steering capability.
    pub fn steer(
        &mut self,
        session_id: &str,
        prompt: Vec<ContentBlock>,
    ) -> Result<u64, SteerError> {
        let running = self
            .state
            .sessions
            .get(session_id)
            .is_some_and(|session| session.running);
        if !running {
            return Err(SteerError::NotRunning);
        }
        if !self.state.steer_available() {
            return Err(SteerError::NotAdvertised);
        }
        let request_id = self.send_prompt(session_id, prompt, Some(PromptDelivery::Steer), false);
        Ok(request_id)
    }

    /// Removes the queued prompt at `index` of `session_id` before it
    /// ever went on the wire, reporting the change the emptied queue
    /// made. An empty report names no such prompt.
    ///
    /// The ACP surface has no withdraw method: the engine-side prompt
    /// queue is only reachable through `session/prompt` deliveries, so
    /// a client that holds the queue, as this one does, withdraws by
    /// dropping the held prompt.
    pub fn withdraw_queued(&mut self, session_id: &str, index: usize) -> Vec<Change> {
        let queue = &mut self.session_mut(session_id).queue;
        if index >= queue.len() {
            return Vec::new();
        }
        queue.remove(index);
        vec![Change::Session {
            id: session_id.to_owned(),
        }]
    }

    /// Sends the queued prompt at `index` of `session_id` as a steer
    /// on the run in flight, removing it from the queue. The same wire
    /// limits as [`Client::steer`] apply, and a rejected steer leaves
    /// the queue untouched.
    pub fn steer_queued(&mut self, session_id: &str, index: usize) -> Result<u64, SteerError> {
        let Some(prompt) = self
            .state
            .session(session_id)
            .and_then(|session| session.queue.get(index).map(|queued| queued.prompt.clone()))
        else {
            return Err(SteerError::NotRunning);
        };
        let sent = self.steer(session_id, prompt);
        if sent.is_ok() {
            self.withdraw_queued(session_id, index);
        }
        sent
    }

    /// Asks the agent to stop the run of `session_id`.
    pub fn cancel(&mut self, session_id: &str) {
        self.outgoing.push(Frame::Notification {
            method: "session/cancel".into(),
            params: params(&CancelNotification {
                session_id: session_id.to_owned(),
            }),
        });
    }

    /// Changes one config option of a session. The value shows at
    /// once, ahead of the agent's answer, and the answer replaces the
    /// session's config options. Returns the request id and the change
    /// the shown value made; an empty report means nothing was shown
    /// because the session or the option is not known yet.
    pub fn set_config_option(
        &mut self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> (u64, Vec<Change>) {
        let previous = self.set_shown_option(session_id, config_id, value);
        let shown = previous.is_some().then(|| Change::Session {
            id: session_id.to_owned(),
        });
        let id = self.request(
            "session/set_config_option",
            params(&SetSessionConfigOptionRequest {
                session_id: session_id.to_owned(),
                config_id: config_id.to_owned(),
                value: value.to_owned(),
            }),
            Pending::SetConfigOption {
                session_id: session_id.to_owned(),
                config_id: config_id.to_owned(),
                previous,
            },
        );
        (id, shown.into_iter().collect())
    }

    /// Shows `value` for option `config_id` of `session_id` at once,
    /// ahead of the agent's answer, and returns the value it replaced.
    /// The mode option moves the current mode with it.
    fn set_shown_option(
        &mut self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Option<String> {
        let session = self.state.sessions.get_mut(session_id)?;
        let option = session
            .config_options
            .iter_mut()
            .find(|option| option.id == config_id)?;
        let previous = std::mem::replace(&mut option.current_value, value.to_owned());
        if config_id == "mode" {
            session.mode = Some(value.to_owned());
        }
        Some(previous)
    }

    /// Answers the ask `request_id` of `session_id`. The ask leaves
    /// the queue once answered, a decision record joins the session's
    /// transcript so a redraw shows what was chosen, and the answer
    /// goes out. A [`PermissionDecision::Feedback`] verdict rides the
    /// `_meta.kage.planReview` channel. Returns the changes the reply
    /// made: the ask left the queue and the record joined the
    /// transcript. An empty report means no such ask was open, in
    /// which case nothing is sent.
    pub fn reply_permission(
        &mut self,
        session_id: &str,
        request_id: RequestId,
        decision: &PermissionDecision,
    ) -> Vec<Change> {
        let (outcome, meta) = {
            let Some(session) = self.state.sessions.get_mut(session_id) else {
                return Vec::new();
            };
            let Some(index) = session
                .permissions
                .iter()
                .position(|ask| ask.request_id == request_id)
            else {
                return Vec::new();
            };
            let ask = &session.permissions[index];
            let mut meta = None;
            let option_id = match decision {
                PermissionDecision::Cancel => None,
                PermissionDecision::Option(option_id) => Some(option_id.clone()),
                PermissionDecision::Allow => ask
                    .option_of(PermissionOptionKind::AllowOnce)
                    .map(str::to_owned),
                PermissionDecision::AllowAlways => ask
                    .option_of(PermissionOptionKind::AllowAlways)
                    .map(str::to_owned),
                PermissionDecision::Reject => ask
                    .option_of(PermissionOptionKind::RejectOnce)
                    .map(str::to_owned),
                PermissionDecision::Answer { option_id, answer } => {
                    meta = Some(RequestMeta {
                        kage: KageMeta {
                            question: Some(Box::new(QuestionMeta {
                                prompt: None,
                                answer: Some(answer.clone()),
                            })),
                            ..KageMeta::default()
                        },
                    });
                    Some(option_id.clone())
                }
                PermissionDecision::Feedback {
                    option_id,
                    feedback,
                } => {
                    meta = Some(RequestMeta {
                        kage: KageMeta {
                            plan_review: Some(PlanReview {
                                revision: Some(feedback.clone()),
                                plan: None,
                            }),
                            ..KageMeta::default()
                        },
                    });
                    Some(option_id.clone())
                }
            };
            let outcome = if let Some(ref option_id) = option_id {
                PermissionOutcome::Selected(SelectedOption {
                    option_id: option_id.clone(),
                })
            } else {
                if *decision != PermissionDecision::Cancel {
                    return Vec::new();
                }
                PermissionOutcome::Cancelled
            };
            let record = decision_record(ask, decision, option_id.as_deref());
            session.permissions.remove(index);
            session.items.push(record);
            (outcome, meta)
        };
        self.outgoing.push(Frame::Success {
            id: request_id,
            result: params(&RequestPermissionResult { outcome, meta }),
        });
        vec![
            Change::Permission {
                id: session_id.to_owned(),
            },
            Change::Transcript {
                id: session_id.to_owned(),
            },
        ]
    }

    /// Releases the session: the answer removes it from the state.
    pub fn close_session(&mut self, session_id: &str) -> u64 {
        self.request(
            "session/close",
            params(&CloseSessionRequest {
                session_id: session_id.to_owned(),
            }),
            Pending::Close {
                session_id: session_id.to_owned(),
            },
        )
    }

    /// Asks for the running process's read-only configuration. The
    /// answer arrives as [`Change::Config`].
    pub fn config_get(&mut self) -> u64 {
        self.request(
            "_kage/config/get",
            params(&ConfigGetRequest::default()),
            Pending::ConfigGet,
        )
    }

    /// Asks for the read-only configuration as `session_id`'s workdir
    /// layers it. The answer arrives as [`Change::Config`].
    pub fn config_get_for(&mut self, session_id: &str) -> u64 {
        self.request(
            "_kage/config/get",
            params(&ConfigGetRequest {
                session_id: Some(session_id.to_owned()),
            }),
            Pending::ConfigGet,
        )
    }

    /// Replaces the user config entry at `path`, such as
    /// `["mcp", "servers", "github"]`, with `value`, or removes it when
    /// `value` is `None`. The snapshot after the write arrives as
    /// [`Change::Config`], read for `session_id`'s workdir when given; a
    /// refused edit arrives as [`Change::Failed`].
    pub fn config_set(
        &mut self,
        session_id: Option<&str>,
        path: &[&str],
        value: Option<serde_json::Value>,
    ) -> u64 {
        self.request(
            "_kage/config/set",
            params(&kage_acp_wire::ConfigSetRequest {
                session_id: session_id.map(str::to_owned),
                path: path.iter().map(|key| (*key).to_owned()).collect(),
                value,
            }),
            Pending::ConfigGet,
        )
    }

    /// Asks the engine to list the models of `provider`, as saved or as
    /// a form holds it. The answer arrives as [`Change::Tested`] under
    /// the returned request id.
    pub fn config_test(&mut self, provider: kage_acp_wire::ProviderProbe) -> u64 {
        self.test(&kage_acp_wire::ConfigTestRequest {
            provider: Some(provider),
            ..Default::default()
        })
    }

    /// Asks the engine to connect to an MCP server or start an ACP agent
    /// as `request` describes, or to list a provider's models. The
    /// answer arrives as [`Change::Tested`] under the returned id.
    pub fn test(&mut self, request: &kage_acp_wire::ConfigTestRequest) -> u64 {
        self.request("_kage/config/test", params(request), Pending::ConfigTest)
    }

    /// Asks the engine for the providers of a directory in the
    /// models.dev `api.json` shape: models.dev itself without `url`. The
    /// answer arrives as [`Change::ProviderDirectory`] under the returned id.
    pub fn providers_directory(&mut self, url: Option<&str>, api_key: Option<&str>) -> u64 {
        self.request(
            "_kage/providers/directory",
            params(&kage_acp_wire::DirectoryRequest {
                url: url.map(str::to_owned),
                api_key: api_key.map(str::to_owned),
            }),
            Pending::Directory,
        )
    }

    /// Asks the engine for the folders inside `path` on its machine, the
    /// home folder without one. The answer arrives as
    /// [`Change::Folders`] under the returned id.
    pub fn folders(&mut self, path: Option<&str>) -> u64 {
        self.request(
            "_kage/folders",
            params(&kage_acp_wire::FoldersRequest {
                path: path.map(str::to_owned),
            }),
            Pending::Folders,
        )
    }

    /// Asks the engine for the user themes on its machine. The answer
    /// arrives as [`Change::Themes`].
    pub fn themes(&mut self) -> u64 {
        self.request(
            "_kage/themes",
            params(&kage_acp_wire::ThemesRequest {}),
            Pending::Themes,
        )
    }

    /// Installs a plugin file from `source`, an `https://` URL or a
    /// path on the engine's machine. The answer arrives as
    /// [`Change::Plugins`].
    pub fn plugin_install(&mut self, source: &str, replace: bool) -> u64 {
        self.request(
            "_kage/plugins/install",
            params(&kage_acp_wire::PluginInstallRequest {
                source: source.to_owned(),
                name: None,
                replace,
            }),
            Pending::Plugins,
        )
    }

    /// Removes the installed plugin `name`. The answer arrives as
    /// [`Change::Plugins`].
    pub fn plugin_remove(&mut self, name: &str) -> u64 {
        self.request(
            "_kage/plugins/remove",
            params(&kage_acp_wire::PluginRemoveRequest {
                name: name.to_owned(),
            }),
            Pending::Plugins,
        )
    }

    /// Asks the engine to save the image or audio clip whose bytes hash
    /// to `sha256` into session `session_id`'s directory, at `path` or under
    /// `generated/`. The outcome arrives as a notice on the session.
    pub fn image_save(&mut self, session_id: &str, sha256: &str, path: Option<&str>) -> u64 {
        self.request(
            "_kage/image/save",
            params(&kage_acp_wire::ImageSaveRequest {
                session_id: session_id.to_owned(),
                sha256: sha256.to_owned(),
                path: path.map(str::to_owned),
            }),
            Pending::ImageSave,
        )
    }

    /// Lists the plugin registry's packages whose name, description or
    /// tags contain every word of `query`; `refresh` fetches the index
    /// again. The answer arrives as [`Change::Registry`].
    pub fn registry_search(&mut self, query: &str, refresh: bool) -> u64 {
        self.request(
            "_kage/registry/search",
            params(&kage_acp_wire::RegistrySearchRequest {
                query: query.to_owned(),
                refresh,
            }),
            Pending::Registry,
        )
    }

    /// Installs package `name` from the plugin registry: `version`, or
    /// the newest that fits the engine. `force` replaces what is in the
    /// way. The answer arrives as [`Change::RegistryInstalled`].
    pub fn registry_install(&mut self, name: &str, version: Option<&str>, force: bool) -> u64 {
        self.request(
            "_kage/registry/install",
            params(&kage_acp_wire::RegistryInstallRequest {
                name: name.to_owned(),
                version: version.map(str::to_owned),
                force,
            }),
            Pending::RegistryInstall,
        )
    }

    /// Saves `key` as `provider`'s API key in the engine's credential
    /// store, or removes the saved one when `key` is `None`. The answer
    /// arrives as [`Change::KeySaved`].
    pub fn auth_set(&mut self, provider: &str, key: Option<String>) -> u64 {
        self.request(
            "_kage/auth/set",
            params(&kage_acp_wire::AuthSetRequest {
                provider: provider.to_owned(),
                key,
            }),
            Pending::AuthSet {
                provider: provider.to_owned(),
            },
        )
    }

    /// Continues swarm children of `session_id`: each member id maps to
    /// a follow-up prompt, and an empty prompt continues the child's
    /// task. The children report back through `subagent_update`.
    pub fn swarm_resume(
        &mut self,
        session_id: &str,
        members: std::collections::BTreeMap<String, String>,
    ) -> u64 {
        self.request(
            "_kage/swarm/resume",
            params(&SwarmResumeRequest {
                session_id: session_id.to_owned(),
                members,
            }),
            Pending::SwarmResume {
                session_id: session_id.to_owned(),
            },
        )
    }

    /// Copies `session_id` into a new recorded session, whole or up to
    /// the prompt `before` names. The copy's id arrives as
    /// [`Change::Forked`]; the copy is not opened.
    pub fn fork_session(&mut self, session_id: &str, before: Option<PromptRef>) -> u64 {
        self.request(
            "_kage/session/fork",
            params(&SessionForkRequest {
                session_id: session_id.to_owned(),
                before,
            }),
            Pending::Fork {
                session_id: session_id.to_owned(),
            },
        )
    }

    /// Renders the recorded `session_id` as Markdown. The text arrives
    /// as [`Change::Exported`].
    pub fn export_session(&mut self, session_id: &str) -> u64 {
        self.request(
            "_kage/session/export",
            params(&SessionRequest {
                session_id: session_id.to_owned(),
            }),
            Pending::Export {
                session_id: session_id.to_owned(),
            },
        )
    }

    /// Asks the agent to summarize the older turns of `session_id` now.
    /// The result arrives as the session's compaction update.
    pub fn compact_session(&mut self, session_id: &str) -> u64 {
        self.request(
            "_kage/session/compact",
            params(&SessionRequest {
                session_id: session_id.to_owned(),
            }),
            Pending::Compact {
                session_id: session_id.to_owned(),
            },
        )
    }

    /// Names `session_id`. The title arrives as the session's
    /// `session_info_update`, for every client.
    pub fn rename_session(&mut self, session_id: &str, title: &str) -> u64 {
        self.request(
            "_kage/session/rename",
            params(&SessionRenameRequest {
                session_id: session_id.to_owned(),
                title: title.to_owned(),
            }),
            Pending::Rename {
                session_id: session_id.to_owned(),
            },
        )
    }

    /// Asks for every model the engine can run now. The answer arrives
    /// as [`Change::Models`].
    pub fn models_list(&mut self) -> u64 {
        self.request("_kage/models/list", serde_json::json!({}), Pending::Models)
    }

    /// Asks for the engine options a client can change. The answer
    /// arrives as [`Change::Options`].
    pub fn options_list(&mut self) -> u64 {
        self.request(
            "_kage/options/list",
            params(&ConfigGetRequest::default()),
            Pending::Options,
        )
    }

    /// Asks for the engine options as `session_id`'s workdir config
    /// sets them. The answer arrives as [`Change::Options`].
    pub fn options_list_for(&mut self, session_id: &str) -> u64 {
        self.request(
            "_kage/options/list",
            params(&ConfigGetRequest {
                session_id: Some(session_id.to_owned()),
            }),
            Pending::Options,
        )
    }

    /// Stores engine option `name` as `value` in the user config. The
    /// options after the write arrive as [`Change::Options`]; a refused
    /// value arrives as [`Change::Failed`] with the engine's reason.
    pub fn set_engine_option(&mut self, name: &str, value: Value) -> u64 {
        self.request(
            "_kage/options/set",
            params(&OptionSetRequest {
                name: name.to_owned(),
                value,
            }),
            Pending::Options,
        )
    }

    /// Lists or reads a path under the session workdir. The answer
    /// arrives as [`Change::Fs`].
    pub fn fs(&mut self, session_id: &str, op: FsOp, path: &str) -> u64 {
        self.request(
            "_kage/fs",
            params(&FsRequest {
                session_id: session_id.to_owned(),
                op,
                path: path.to_owned(),
            }),
            Pending::Fs {
                session_id: session_id.to_owned(),
            },
        )
    }

    fn request(&mut self, method: &str, params: Value, pending: Pending) -> u64 {
        let id = self.take_id();
        self.pending.insert(RequestId::Number(id), pending);
        self.outgoing.push(Frame::Request {
            id: RequestId::Number(id),
            method: method.to_owned(),
            params,
        });
        id
    }

    fn take_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn open(
        &mut self,
        method: &str,
        session_id: &str,
        cwd: &str,
        mcp_servers: &[McpServer],
    ) -> (u64, Vec<Change>) {
        let request = if method == "session/load" {
            params(&LoadSessionRequest {
                session_id: session_id.to_owned(),
                cwd: cwd.to_owned(),
                mcp_servers: mcp_servers.to_vec(),
            })
        } else {
            params(&ResumeSessionRequest {
                session_id: session_id.to_owned(),
                cwd: cwd.to_owned(),
                mcp_servers: mcp_servers.to_vec(),
            })
        };
        // The replay that follows streams the whole history again, so
        // what it carries starts over; what only this client holds (the
        // draft, the held queue, the place in the forest) stays. A run
        // that was in flight answers on the old connection, which is
        // gone, so the session reads idle until the replay says
        // otherwise. A failed open puts the replaced session back,
        // reading idle with it.
        let session = self.session_mut(session_id);
        let snapshot = Box::new(session.clone());
        let kept = Session {
            id: session_id.to_owned(),
            cwd: if cwd.is_empty() {
                session.cwd.take()
            } else {
                Some(cwd.to_owned())
            },
            title: session.title.take(),
            parent: session.parent.take(),
            draft: session.draft.take(),
            queue: std::mem::take(&mut session.queue),
            ..Session::default()
        };
        *session = kept;
        let id = self.request(
            method,
            request,
            Pending::Open {
                session_id: session_id.to_owned(),
                snapshot,
            },
        );
        (
            id,
            vec![Change::Session {
                id: session_id.to_owned(),
            }],
        )
    }

    fn send_prompt(
        &mut self,
        session_id: &str,
        prompt: Vec<ContentBlock>,
        delivery: Option<PromptDelivery>,
        owns_run: bool,
    ) -> u64 {
        let id = self.take_id();
        self.pending.insert(
            RequestId::Number(id),
            Pending::Prompt {
                session_id: session_id.to_owned(),
                owns_run,
            },
        );
        let session = self.session_mut(session_id);
        session.running = true;
        // The agent echoes a prompt only to the other clients, so the
        // transcript gets its own copy here.
        session.items.push(TranscriptItem::User {
            content: prompt.clone(),
            steered: delivery == Some(PromptDelivery::Steer),
        });
        self.outgoing.push(Frame::Request {
            id: RequestId::Number(id),
            method: "session/prompt".into(),
            params: params(&PromptRequest {
                session_id: session_id.to_owned(),
                prompt,
                delivery,
            }),
        });
        id
    }

    /// Sends the oldest queued prompt of a run that just ended, if
    /// there is one.
    fn flush_queue(&mut self, session_id: &str) {
        if self.session_mut(session_id).queue.is_empty() {
            return;
        }
        let queued = self.session_mut(session_id).queue.remove(0);
        self.send_prompt(session_id, queued.prompt, None, true);
    }

    fn session_mut(&mut self, session_id: &str) -> &mut Session {
        self.state
            .sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| Session::new(session_id))
    }

    fn handle_server_request(&mut self, id: RequestId, method: &str, params: Value) -> Vec<Change> {
        if method != ASK_METHOD {
            self.outgoing.push(Frame::Failure {
                id,
                error: RpcError::method_not_found(method),
            });
            return Vec::new();
        }
        let Ok(request) = serde_json::from_value::<RequestPermissionRequest>(params) else {
            self.outgoing.push(Frame::Failure {
                id,
                error: RpcError::invalid_params(ASK_METHOD),
            });
            return Vec::new();
        };
        let session_id = request.session_id;
        let session = self.session_mut(&session_id);
        let kage = request.meta.map(|meta| meta.kage).unwrap_or_default();
        let ask = PermissionAsk {
            request_id: id,
            tool_call: request.tool_call,
            options: request.options,
            plan: kage.plan_review.and_then(|review| review.plan),
            question: kage.question.and_then(|question| question.prompt),
        };
        // A re-attach makes the agent raise its open asks again, so a
        // frame whose id is already queued replaces its echo instead
        // of joining it: one ask per request id, in the latest offer's
        // shape, position kept.
        match session
            .permissions
            .iter()
            .position(|open| open.request_id == ask.request_id)
        {
            Some(index) => session.permissions[index] = ask,
            None => session.permissions.push(ask),
        }
        vec![Change::Permission { id: session_id }]
    }

    fn handle_notification(&mut self, method: &str, params: Value) -> Vec<Change> {
        match method {
            "session/update" => {
                let session_id = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                match serde_json::from_value::<SessionNotification>(params) {
                    Ok(note) => self.apply_update(&note.session_id, note.update),
                    Err(error) => {
                        log::debug!("an unreadable session/update arrived: {error}");
                        vec![Change::Unknown {
                            session_id,
                            method: method.to_owned(),
                        }]
                    }
                }
            }
            "$/cancel_request" => {
                let Some(request_id) = params.get("requestId").and_then(RequestId::from_value)
                else {
                    return Vec::new();
                };
                self.withdraw_ask(&request_id)
            }
            _ => Vec::new(),
        }
    }

    /// Removes the open ask `request_id`: it was answered elsewhere
    /// or the agent withdrew it through `$/cancel_request`. The ask
    /// leaves the queue with no reply of ours and no decision record;
    /// [`Change::AnsweredElsewhere`] is the host's cue to close the
    /// card.
    fn withdraw_ask(&mut self, request_id: &RequestId) -> Vec<Change> {
        let mut moved = Vec::new();
        let sessions = self.state.sessions.keys().cloned().collect::<Vec<_>>();
        for session_id in sessions {
            let Some(session) = self.state.sessions.get_mut(&session_id) else {
                continue;
            };
            let Some(index) = session
                .permissions
                .iter()
                .position(|ask| &ask.request_id == request_id)
            else {
                continue;
            };
            session.permissions.remove(index);
            moved.push(Change::Permission {
                id: session_id.clone(),
            });
            moved.push(Change::AnsweredElsewhere {
                id: session_id,
                request_id: request_id.clone(),
            });
        }
        moved
    }

    fn handle_success(&mut self, id: &RequestId, result: Value) -> Vec<Change> {
        let Some((RequestId::Number(id), pending)) = self.pending.remove_entry(id) else {
            return Vec::new();
        };
        match pending {
            Pending::Initialize => self.apply_initialize(id, result),
            Pending::NewSession { cwd } => self.apply_new_session(id, cwd, result),
            Pending::Open {
                session_id,
                snapshot: _,
            } => {
                match answer::<kage_acp_wire::LoadSessionResponse>(
                    id,
                    result,
                    "session open result",
                ) {
                    Err(failed) => failed,
                    Ok(response) => {
                        let session = self.session_mut(&session_id);
                        session.opened = true;
                        session.config_options = response.config_options;
                        vec![Change::Session { id: session_id }]
                    }
                }
            }
            Pending::List { cwd } => self.apply_list_page(id, cwd.as_deref(), result),
            Pending::Prompt {
                session_id,
                owns_run,
            } => self.apply_prompt_answer(session_id, owns_run, result),
            Pending::SetConfigOption { session_id, .. } => {
                match answer::<kage_acp_wire::SetSessionConfigOptionResponse>(
                    id,
                    result,
                    "session/set_config_option result",
                ) {
                    Err(failed) => failed,
                    Ok(response) => {
                        self.session_mut(&session_id).config_options = response.config_options;
                        vec![Change::Session { id: session_id }]
                    }
                }
            }
            Pending::Close { session_id } => {
                self.state.sessions.remove(&session_id);
                vec![Change::Session { id: session_id }]
            }
            pending @ (Pending::ConfigGet
            | Pending::ConfigTest
            | Pending::AuthSet { .. }
            | Pending::Directory
            | Pending::Folders
            | Pending::Themes
            | Pending::Plugins
            | Pending::Registry
            | Pending::RegistryInstall
            | Pending::ImageSave
            | Pending::Models
            | Pending::Options) => settings_answer(id, pending, result),
            Pending::SwarmResume { session_id }
            | Pending::Compact { session_id }
            | Pending::Rename { session_id } => {
                vec![Change::Session { id: session_id }]
            }
            Pending::Fork { session_id } => {
                match answer::<SessionForkResponse>(id, result, "_kage/session/fork result") {
                    Err(failed) => failed,
                    Ok(fork) => vec![Change::Forked {
                        from: session_id,
                        to: fork.session_id,
                    }],
                }
            }
            Pending::Export { session_id } => {
                match answer::<SessionExportResponse>(id, result, "_kage/session/export result") {
                    Err(failed) => failed,
                    Ok(export) => vec![Change::Exported {
                        session_id,
                        markdown: export.markdown,
                    }],
                }
            }
            Pending::Fs { session_id } => {
                match answer::<kage_acp_wire::FsResult>(id, result, "_kage/fs result") {
                    Err(failed) => failed,
                    Ok(fs_result) => vec![Change::Fs {
                        session_id,
                        result: fs_result,
                    }],
                }
            }
        }
    }

    /// Records what the agent said of itself in its `initialize`
    /// answer.
    fn apply_initialize(&mut self, id: u64, result: Value) -> Vec<Change> {
        match answer::<kage_acp_wire::InitializeResponse>(id, result, "initialize result") {
            Err(failed) => failed,
            Ok(response) => {
                self.state.protocol_version = Some(response.protocol_version);
                self.state.capabilities = Some(response.agent_capabilities);
                self.state.agent = response.agent_info;
                let kage = response.meta.and_then(|meta| meta.kage).unwrap_or_default();
                self.state.agent_cwd = kage.cwd;
                self.state.agent_defaults = kage.config_options;
                vec![Change::Connection]
            }
        }
    }

    /// Merges a `session/list` page into the directory and asks for
    /// the next one. An empty cursor is the end of the list, not a
    /// page to fetch.
    fn apply_list_page(&mut self, id: u64, cwd: Option<&str>, result: Value) -> Vec<Change> {
        let page = match answer::<kage_acp_wire::ListSessionsResponse>(
            id,
            result,
            "session/list result",
        ) {
            Err(failed) => return failed,
            Ok(page) => page,
        };
        for info in page.sessions {
            if let Some(listed) = self
                .state
                .directory
                .iter_mut()
                .find(|listed| listed.session_id == info.session_id)
            {
                *listed = info;
            } else {
                self.state.directory.push(info);
            }
        }
        if let Some(cursor) = page.next_cursor.filter(|cursor| !cursor.is_empty()) {
            self.list_sessions(cwd, Some(&cursor));
        }
        vec![Change::Directory]
    }

    /// Records the session a `session/new` answer created.
    fn apply_new_session(&mut self, id: u64, cwd: String, result: Value) -> Vec<Change> {
        match answer::<kage_acp_wire::NewSessionResponse>(id, result, "session/new result") {
            Err(failed) => failed,
            Ok(response) => {
                let mut session = Session::new(&response.session_id);
                session.opened = true;
                // An empty cwd ran in the agent's own directory.
                session.cwd = if cwd.is_empty() {
                    self.state.agent_cwd.clone()
                } else {
                    Some(cwd)
                };
                session.config_options = response.config_options;
                let session_id = session.id.clone();
                self.state.sessions.insert(session_id.clone(), session);
                vec![Change::Session { id: session_id }]
            }
        }
    }

    /// Records a prompt answer and, when it ended the run, sends the
    /// oldest queued prompt.
    fn apply_prompt_answer(
        &mut self,
        session_id: String,
        owns_run: bool,
        result: Value,
    ) -> Vec<Change> {
        let stop = serde_json::from_value::<PromptResponse>(result)
            .ok()
            .map(|response| response.stop_reason);
        {
            let session = self.session_mut(&session_id);
            session.last_stop = stop;
            if owns_run {
                session.running = false;
                session.compacting = false;
            }
        }
        if owns_run {
            self.flush_queue(&session_id);
        }
        vec![Change::Session { id: session_id }]
    }

    fn handle_failure(&mut self, id: &RequestId, error: RpcError) -> Vec<Change> {
        let Some((RequestId::Number(id), pending)) = self.pending.remove_entry(id) else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        match pending {
            Pending::Prompt {
                session_id,
                owns_run: true,
            } => {
                let session = self.session_mut(&session_id);
                session.running = false;
                session.compacting = false;
                self.flush_queue(&session_id);
            }
            Pending::SetConfigOption {
                session_id,
                config_id,
                previous: Some(previous),
            } => {
                let restored = self
                    .set_shown_option(&session_id, &config_id, &previous)
                    .is_some();
                if restored {
                    changes.push(Change::Session { id: session_id });
                }
            }
            Pending::Open {
                session_id,
                snapshot,
            } => {
                let session = self.session_mut(&session_id);
                *session = *snapshot;
                // The run the old state read busy with rode the
                // connection this load replaced, so it reads idle.
                session.running = false;
                session.in_turn = false;
                session.compacting = false;
                changes.push(Change::Session { id: session_id });
            }
            _ => {}
        }
        changes.push(Change::Failed { request: id, error });
        changes
    }

    /// Marks a turn of `session_id` begun, or records its end.
    fn apply_turn(&mut self, session_id: &str, turn: kage_acp_wire::TurnUpdate) -> Vec<Change> {
        let session = self.session_mut(session_id);
        session.in_turn = turn.phase == kage_acp_wire::TurnPhase::Start;
        let mut changes = vec![Change::Session {
            id: session_id.into(),
        }];
        if turn.phase == kage_acp_wire::TurnPhase::End {
            session.items.push(TranscriptItem::TurnEnd {
                reason: turn.reason,
                at: turn.at,
                took_ms: turn.took_ms,
            });
            changes.push(Change::Transcript {
                id: session_id.into(),
            });
        }
        changes
    }

    fn apply_update(&mut self, session_id: &str, update: SessionUpdate) -> Vec<Change> {
        let transcript = || Change::Transcript {
            id: session_id.into(),
        };
        match update {
            SessionUpdate::UserMessageChunk(_)
            | SessionUpdate::AgentMessageChunk(_)
            | SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::ToolCall(_)
            | SessionUpdate::ToolCallUpdate(_)
            | SessionUpdate::Plan(_)
            | SessionUpdate::Notice(_)
            | SessionUpdate::Compaction(_) => {
                let session = self.session_mut(session_id);
                apply_item(session, update)
                    .then(transcript)
                    .into_iter()
                    .collect()
            }
            SessionUpdate::AvailableCommandsUpdate(update) => {
                self.session_mut(session_id).commands = update.available_commands;
                vec![Change::Session {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::CurrentModeUpdate(update) => {
                self.session_mut(session_id).mode = Some(update.current_mode_id);
                vec![Change::Session {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::UsageUpdate(update) => {
                self.session_mut(session_id).usage = Usage {
                    used: update.used,
                    size: update.size,
                    cost: update.cost,
                    tokens: update.meta.and_then(|meta| meta.kage),
                };
                vec![Change::Session {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::SessionInfoUpdate(update) => {
                let session = self.session_mut(session_id);
                if update.title.is_some() {
                    session.title = update.title;
                }
                if update.updated_at.is_some() {
                    session.updated_at = update.updated_at;
                }
                vec![Change::Session {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                self.session_mut(session_id).config_options = update.config_options;
                vec![Change::Session {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::SubagentUpdate(update) => {
                let child = update.subagent_session_id.clone();
                let session = self.session_mut(session_id);
                let agent = session.agents.entry(child.clone()).or_default();
                agent.merge(&update);
                self.session_mut(&child).parent = Some(session_id.to_owned());
                vec![Change::Agents {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::Turn(turn) => self.apply_turn(session_id, turn),
            SessionUpdate::McpStatus(status) => {
                self.session_mut(session_id)
                    .mcp
                    .insert(status.name, status.status);
                vec![Change::Session {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::Unknown => {
                log::debug!("a session/update of an unknown kind arrived");
                vec![Change::Unknown {
                    session_id: Some(session_id.to_owned()),
                    method: "session/update".to_owned(),
                }]
            }
        }
    }
}

/// Applies the updates that land in the transcript, reporting whether
/// anything changed. An update for a tool call the transcript never
/// announced changes nothing.
fn apply_item(session: &mut Session, update: SessionUpdate) -> bool {
    match update {
        SessionUpdate::UserMessageChunk(chunk) => {
            // A prompt arrives as one chunk per block, text first, and
            // the wire names no message: a text chunk starts a prompt,
            // and an image or resource chunk belongs to the prompt
            // before it.
            match session.items.last_mut() {
                Some(TranscriptItem::User { content, .. }) if chunk.content.as_text().is_none() => {
                    content.push(chunk.content);
                }
                _ => session.items.push(TranscriptItem::User {
                    content: vec![chunk.content],
                    steered: false,
                }),
            }
            true
        }
        SessionUpdate::AgentMessageChunk(chunk) => {
            session.append_chunk(&chunk.content, true);
            true
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            session.append_chunk(&chunk.content, false);
            if let Some(ms) = chunk.meta.and_then(|meta| meta.kage.duration_ms)
                && let Some(TranscriptItem::Thinking { took_ms, .. }) = session.items.last_mut()
            {
                *took_ms = Some(ms);
            }
            true
        }
        SessionUpdate::ToolCall(call) => {
            let kage = call.meta.map(|meta| meta.kage).unwrap_or_default();
            session.items.push(TranscriptItem::ToolCall(ToolCallItem {
                tool_call_id: call.tool_call_id,
                title: call.title,
                kind: call.kind,
                status: call.status,
                input: call.raw_input,
                swarm: kage.swarm,
                content: call.content,
                raw_output: None,
                took_ms: kage.duration_ms,
            }));
            true
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let call_id = update.tool_call_id.clone();
            session
                .items
                .iter_mut()
                .rev()
                .find_map(|item| match item {
                    TranscriptItem::ToolCall(call) if call.tool_call_id == call_id => Some(call),
                    _ => None,
                })
                .is_some_and(|call| {
                    call.merge(&update);
                    true
                })
        }
        SessionUpdate::Plan(plan) => {
            session.plan = Some(plan.entries.clone());
            match session.items.last_mut() {
                Some(TranscriptItem::Plan { entries }) => *entries = plan.entries,
                _ => session.items.push(TranscriptItem::Plan {
                    entries: plan.entries,
                }),
            }
            true
        }
        SessionUpdate::Notice(notice) => {
            session.items.push(TranscriptItem::Notice {
                tone: notice.tone,
                text: notice.text,
            });
            true
        }
        SessionUpdate::Compaction(compaction) => match compaction.phase {
            CompactionPhase::Started => {
                session.compacting = true;
                true
            }
            CompactionPhase::Complete => {
                session.compacting = false;
                session.items.push(TranscriptItem::Compaction {
                    kept: compaction.kept.unwrap_or(0),
                    before: compaction.before,
                    after: compaction.after,
                    summarized: compaction.summarized,
                    summary: compaction.summary,
                });
                true
            }
            CompactionPhase::Aborted => {
                session.compacting = false;
                true
            }
        },
        _ => false,
    }
}

/// The transcript record of a decision: the subject the ask named,
/// the chosen option's label as offered, whether it lets the call
/// proceed, and any feedback text. An answer without a chosen option
/// records `cancelled`. A label for an option id the ask never
/// offered is the id itself, never a made-up name.
fn decision_record(
    ask: &PermissionAsk,
    decision: &PermissionDecision,
    option_id: Option<&str>,
) -> TranscriptItem {
    let subject = ask.subject();
    let chosen = option_id.and_then(|option_id| {
        ask.options
            .iter()
            .find(|option| option.option_id == option_id)
    });
    if let PermissionDecision::Answer { answer, .. } = decision {
        return TranscriptItem::Decision {
            subject,
            label: answer.join(", "),
            allowed: true,
            feedback: None,
        };
    }
    let label = match chosen {
        Some(option) => option.name.clone(),
        None => option_id.map_or_else(|| "cancelled".to_owned(), str::to_owned),
    };
    let allowed = chosen.is_some_and(|option| {
        matches!(
            option.kind,
            PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
        )
    });
    let feedback = match decision {
        PermissionDecision::Feedback { feedback, .. } => Some(feedback.clone()),
        _ => None,
    };
    TranscriptItem::Decision {
        subject,
        label,
        allowed,
        feedback,
    }
}

/// Decodes a request answer, or the change that reports it undecodable.
fn answer<T: serde::de::DeserializeOwned>(
    id: u64,
    result: Value,
    what: &str,
) -> Result<T, Vec<Change>> {
    serde_json::from_value(result).map_err(|_| {
        vec![Change::Failed {
            request: id,
            error: RpcError::invalid_params(what),
        }]
    })
}

/// The parameters of an outgoing frame. Every wire type serializes
/// infallibly, so a failure here is a bug in the schema, not in a
/// caller's input.
fn params<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("wire type serializes")
}

/// The change a settings answer makes: a config snapshot, a provider
/// test, a saved key, the model list or the engine options.
fn settings_answer(id: u64, pending: Pending, result: Value) -> Vec<Change> {
    match pending {
        Pending::ConfigGet => vec![Change::Config {
            request: id,
            config: result,
        }],
        Pending::ConfigTest => {
            match answer::<kage_acp_wire::ConfigTestResult>(id, result, "_kage/config/test result")
            {
                Err(failed) => failed,
                Ok(result) => vec![Change::Tested {
                    request: id,
                    result,
                }],
            }
        }
        Pending::AuthSet { provider } => vec![Change::KeySaved { provider }],
        Pending::Directory => match answer::<kage_acp_wire::DirectoryResult>(
            id,
            result,
            "_kage/providers/directory result",
        ) {
            Err(failed) => failed,
            Ok(result) => vec![Change::ProviderDirectory {
                request: id,
                providers: result.providers,
            }],
        },
        Pending::Folders => {
            match answer::<kage_acp_wire::FoldersResult>(id, result, "_kage/folders result") {
                Err(failed) => failed,
                Ok(result) => vec![Change::Folders {
                    request: id,
                    result,
                }],
            }
        }
        Pending::Themes => {
            match answer::<kage_acp_wire::ThemesResult>(id, result, "_kage/themes result") {
                Err(failed) => failed,
                Ok(result) => vec![Change::Themes {
                    themes: result.themes,
                }],
            }
        }
        Pending::Plugins => vec![Change::Plugins { request: id }],
        Pending::Registry => match answer::<kage_acp_wire::RegistrySearchResult>(
            id,
            result,
            "_kage/registry/search result",
        ) {
            Err(failed) => failed,
            Ok(result) => vec![Change::Registry {
                request: id,
                result,
            }],
        },
        Pending::RegistryInstall => match answer::<kage_acp_wire::RegistryInstallResult>(
            id,
            result,
            "_kage/registry/install result",
        ) {
            Err(failed) => failed,
            Ok(result) => vec![Change::RegistryInstalled {
                request: id,
                result,
            }],
        },
        Pending::Models => match answer::<ModelsResponse>(id, result, "_kage/models result") {
            Err(failed) => failed,
            Ok(answer) => vec![Change::Models {
                providers: answer.providers,
            }],
        },
        Pending::Options => match answer::<OptionsResponse>(id, result, "_kage/options result") {
            Err(failed) => failed,
            Ok(answer) => vec![Change::Options {
                options: answer.options,
            }],
        },
        // `_kage/image/save` answers nothing: the engine reports where the
        // image went as a session notice.
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A permission ask whose request id is a string, as agents that
    /// assign string ids send.
    fn string_id_ask() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": "abc",
            "method": "session/request_permission",
            "params": {
                "sessionId": "s1",
                "toolCall": { "toolCallId": "c1" },
                "options": [
                    { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "reject", "name": "Reject", "kind": "reject_once" }
                ]
            }
        })
    }

    #[test]
    fn string_id_asks_open_a_card_and_take_an_answer() {
        let mut client = Client::new();
        let frame = Frame::parse(&string_id_ask()).unwrap();
        assert_eq!(
            client.handle(frame),
            vec![Change::Permission { id: "s1".into() }]
        );
        assert_eq!(
            client.state().session("s1").unwrap().permissions.len(),
            1,
            "the ask is queued under its string id"
        );
        let answered = client.reply_permission(
            "s1",
            RequestId::String("abc".into()),
            &PermissionDecision::Allow,
        );
        assert!(!answered.is_empty());
        let outgoing = client.take_outgoing();
        assert_eq!(outgoing.len(), 1);
        match &outgoing[0] {
            Frame::Success { id, result } => {
                assert_eq!(*id, RequestId::String("abc".into()));
                let result = result.to_string();
                assert!(result.contains("allow"), "the allow option was picked");
            }
            other => panic!("expected a success answer, got {other:?}"),
        }
        assert_eq!(outgoing[0].to_value()["id"], json!("abc"));
    }

    #[test]
    fn a_compaction_started_flags_the_session_until_the_compaction_lands() {
        let mut client = Client::new();
        let frame = |update: Value| {
            Frame::parse(&json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": "s1", "update": update}
            }))
            .unwrap()
        };
        client.handle(frame(json!({
            "sessionUpdate": "_kage/compaction",
            "phase": "started",
            "kept": 4,
            "summarized": 12
        })));
        assert!(client.state().session("s1").unwrap().compacting);

        client.handle(frame(json!({
            "sessionUpdate": "_kage/compaction",
            "phase": "complete",
            "kept": 4,
            "before": 1200,
            "after": 300,
            "summarized": 12,
            "summary": "framed"
        })));
        let session = client.state().session("s1").unwrap();
        assert!(!session.compacting, "the compaction closes the flag");
        match &session.items[..] {
            [
                TranscriptItem::Compaction {
                    kept,
                    before,
                    after,
                    summarized,
                    summary,
                },
            ] => {
                assert_eq!(*kept, 4);
                assert_eq!((*before, *after), (Some(1200), Some(300)));
                assert_eq!(*summarized, Some(12));
                assert_eq!(summary.as_deref(), Some("framed"));
            }
            other => panic!("expected one compaction item, got {other:?}"),
        }
    }

    #[test]
    fn an_aborted_compaction_closes_the_flag_without_a_card() {
        let mut client = Client::new();
        let frame = |update: Value| {
            Frame::parse(&json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": "s1", "update": update}
            }))
            .unwrap()
        };
        client.handle(frame(json!({
            "sessionUpdate": "_kage/compaction",
            "phase": "started",
            "kept": 4,
            "summarized": 12
        })));
        client.handle(frame(json!({
            "sessionUpdate": "_kage/compaction",
            "phase": "aborted"
        })));
        let session = client.state().session("s1").unwrap();
        assert!(!session.compacting);
        assert!(session.items.is_empty(), "an abort leaves no card");
    }

    #[test]
    fn a_compaction_without_a_phase_is_the_older_complete_shape() {
        let mut client = Client::new();
        client
            .handle(
                Frame::parse(&json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "s1",
                        "update": {
                            "sessionUpdate": "_kage/compaction",
                            "kept": 4,
                            "before": 1200,
                            "after": 300
                        }
                    }
                }))
                .unwrap(),
            )
            .first()
            .unwrap();
        let session = client.state().session("s1").unwrap();
        assert!(!session.compacting);
        match &session.items[..] {
            [
                TranscriptItem::Compaction {
                    kept,
                    before,
                    after,
                    summarized,
                    summary,
                },
            ] => {
                assert_eq!(*kept, 4);
                assert_eq!((*before, *after), (Some(1200), Some(300)));
                assert_eq!(*summarized, None);
                assert_eq!(*summary, None);
            }
            other => panic!("expected one compaction item, got {other:?}"),
        }
    }

    #[test]
    fn a_string_request_id_cancel_withdraws_the_ask() {
        let mut client = Client::new();
        client.handle(Frame::parse(&string_id_ask()).unwrap());
        let cancel = json!({
            "jsonrpc": "2.0",
            "method": "$/cancel_request",
            "params": { "requestId": "abc" }
        });
        let changes = client.handle(Frame::parse(&cancel).unwrap());
        assert!(changes.iter().any(|change| matches!(
            change,
            Change::AnsweredElsewhere {
                request_id: RequestId::String(id),
                ..
            } if id == "abc"
        )));
        assert!(client.state().session("s1").unwrap().permissions.is_empty());
    }

    #[test]
    fn an_empty_cursor_ends_the_paging() {
        let mut client = Client::new();
        client.list_sessions(None, None);
        let _ = client.take_outgoing();

        let changes = client.handle(Frame::Success {
            id: RequestId::Number(1),
            result: json!({"sessions": [], "nextCursor": ""}),
        });
        assert_eq!(changes, vec![Change::Directory]);
        assert!(
            client.take_outgoing().is_empty(),
            "an empty cursor is the end of the list"
        );
    }

    #[test]
    fn a_non_empty_cursor_still_pages() {
        let mut client = Client::new();
        client.list_sessions(None, None);
        let _ = client.take_outgoing();

        client.handle(Frame::Success {
            id: RequestId::Number(1),
            result: json!({"sessions": [], "nextCursor": "50"}),
        });
        let outgoing = client.take_outgoing();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(
            outgoing[0].to_value()["params"]["cursor"],
            json!("50"),
            "the named page is fetched"
        );
    }

    #[test]
    fn a_missing_cursor_ends_the_paging() {
        let mut client = Client::new();
        client.list_sessions(None, None);
        let _ = client.take_outgoing();

        client.handle(Frame::Success {
            id: RequestId::Number(1),
            result: json!({"sessions": []}),
        });
        assert!(client.take_outgoing().is_empty());
    }

    #[test]
    fn numeric_id_flows_are_unchanged() {
        let mut client = Client::new();
        let request_id = client.initialize(ClientCapabilities::default(), None);
        let outgoing = client.take_outgoing();
        let Frame::Request { id, .. } = &outgoing[0] else {
            panic!("expected a request");
        };
        assert_eq!(*id, RequestId::Number(request_id));
        client.handle(Frame::Success {
            id: RequestId::Number(request_id),
            result: json!({
                "protocolVersion": 1,
                "agentCapabilities": {},
                "agentInfo": { "name": "agent" }
            }),
        });
        assert_eq!(client.state().protocol_version, Some(1));
    }

    #[test]
    fn a_failed_load_restores_the_transcript_it_replaced() {
        let mut client = Client::new();
        let sent = client.prompt("s1", vec![ContentBlock::text("first ask")]);
        assert!(matches!(sent, PromptOutcome::Sent { .. }));
        let held = client.prompt("s1", vec![ContentBlock::text("second ask")]);
        assert_eq!(held, PromptOutcome::Queued);

        let (load_id, _) = client.load_session("s1", "", &[]);
        assert!(
            client.state().session("s1").unwrap().items.is_empty(),
            "the open replaced the transcript"
        );

        let changes = client.handle(Frame::Failure {
            id: RequestId::Number(load_id),
            error: RpcError::new(-32000, "no such session"),
        });
        let session = client.state().session("s1").unwrap();
        assert_eq!(session.items.len(), 1, "the prior transcript came back");
        assert!(matches!(session.items[0], TranscriptItem::User { .. }));
        assert_eq!(session.queue.len(), 1, "the held prompt came back");
        assert!(!session.running, "the session reads idle");
        assert!(changes.contains(&Change::Session { id: "s1".into() }));
        assert!(
            changes
                .iter()
                .any(|change| matches!(change, Change::Failed { .. }))
        );
    }

    #[test]
    fn a_failed_load_removes_its_pending_entry() {
        let mut client = Client::new();
        let (load_id, _) = client.load_session("s1", "", &[]);
        let failure = Frame::Failure {
            id: RequestId::Number(load_id),
            error: RpcError::new(-32000, "no such session"),
        };
        assert!(!client.handle(failure.clone()).is_empty());
        assert!(
            client.handle(failure).is_empty(),
            "the pending entry is gone"
        );
    }
}
