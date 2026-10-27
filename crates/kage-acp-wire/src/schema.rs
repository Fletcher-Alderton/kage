//! The ACP v1 wire schema: every request, response, notification and
//! update type exchanged on the `initialize` and `session/*` methods,
//! plus the `_kage/*` extensions kage adds (turn, notice, compaction,
//! `mcp_status`, `fs`, config get, steer).
//!
//! Field names are `camelCase` and update/content tags are
//! `snake_case`, matching the published ACP schema. Internally-tagged
//! enums wrap a per-variant `camelCase` struct so the discriminant
//! (`sessionUpdate`, `type`, `outcome`) sits beside the struct's
//! fields exactly as the spec shows.
//!
//! Only the surface kage needs is modelled; unknown optional fields
//! are tolerated on the way in and omitted on the way out, and an
//! unknown `sessionUpdate` kind parses as [`SessionUpdate::Unknown`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// ACP protocol version kage implements.
pub const PROTOCOL_VERSION: i64 = 1;

/// Name/version pair exchanged in `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Implementation {
    /// Program name.
    pub name: String,
    /// Optional display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Program version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Client-side filesystem capability flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapability {
    /// Client can serve `fs/read_text_file`.
    #[serde(default)]
    pub read_text_file: bool,
    /// Client can serve `fs/write_text_file`.
    #[serde(default)]
    pub write_text_file: bool,
}

/// What the client can do for the agent.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    /// Filesystem capabilities.
    #[serde(default)]
    pub fs: FsCapability,
    /// Client can serve `terminal/*`.
    #[serde(default)]
    pub terminal: bool,
    /// The draft subagents capability (RFD PR #1992), kept raw because
    /// its shape may still change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<serde_json::Value>,
    /// Extension facts; kage's own live under `kage`.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<ClientMeta>,
}

/// The `_meta` of [`ClientCapabilities`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClientMeta {
    /// What a kage client asks of the sessions it opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kage: Option<KageClientCapabilities>,
}

/// What a kage client asks of the sessions it opens, under
/// `clientCapabilities._meta.kage`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KageClientCapabilities {
    /// How tools without a `[permissions]` rule are judged. Unset means
    /// [`UnconfiguredTools::Ask`], the editor default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unconfigured_tools: Option<UnconfiguredTools>,
}

/// How a session judges a tool with no `[permissions]` rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnconfiguredTools {
    /// Every such call asks the client first.
    Ask,
    /// Such calls run, as they do in the TUI. Tools of an MCP server
    /// with no rule still ask.
    Allow,
}

impl ClientCapabilities {
    /// Whether the client asked for tools without a rule to run, as in
    /// the TUI, instead of asking first.
    #[must_use]
    pub fn unconfigured_tools_run(&self) -> bool {
        self.meta
            .as_ref()
            .and_then(|meta| meta.kage.as_ref())
            .and_then(|kage| kage.unconfigured_tools)
            == Some(UnconfiguredTools::Allow)
    }

    /// Whether the client understands `subagent_update` and child
    /// sessions: any advertised value other than `false`.
    #[must_use]
    pub fn supports_subagents(&self) -> bool {
        self.subagents
            .as_ref()
            .is_some_and(|v| *v != serde_json::Value::Bool(false))
    }
}

/// Prompt content kinds the agent accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    /// Accepts image content blocks.
    #[serde(default)]
    pub image: bool,
    /// Accepts audio content blocks.
    #[serde(default)]
    pub audio: bool,
    /// Accepts embedded `resource` context.
    #[serde(default)]
    pub embedded_context: bool,
}

/// What the agent can do.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    /// Agent implements `session/load`.
    #[serde(default)]
    pub load_session: bool,
    /// Agent steers a running prompt at its next turn boundary when a
    /// `session/prompt` arrives marked [`PromptDelivery::Steer`].
    #[serde(default)]
    pub steer: bool,
    /// Prompt content the agent accepts.
    #[serde(default)]
    pub prompt_capabilities: PromptCapabilities,
    /// MCP transports the agent can connect to from `mcpServers`.
    #[serde(default)]
    pub mcp_capabilities: McpCapabilities,
    /// Optional session methods the agent implements.
    #[serde(default)]
    pub session_capabilities: SessionCapabilities,
}

/// MCP transports the agent accepts in `mcpServers` (stdio is always
/// supported).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct McpCapabilities {
    /// Accepts `http` servers.
    #[serde(default)]
    pub http: bool,
    /// Accepts `sse` servers.
    #[serde(default)]
    pub sse: bool,
}

/// Optional session methods. Each is an empty object when supported
/// and omitted otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionCapabilities {
    /// Agent implements `session/list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<Supported>,
    /// Agent implements `session/resume`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Supported>,
    /// Agent implements `session/close`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close: Option<Supported>,
}

/// An empty capability object whose presence means supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Supported {}

/// `initialize` request params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    /// Highest protocol version the client speaks.
    pub protocol_version: i64,
    /// Client capabilities.
    #[serde(default)]
    pub client_capabilities: ClientCapabilities,
    /// Optional client identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<Implementation>,
}

/// `initialize` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    /// Negotiated protocol version.
    pub protocol_version: i64,
    /// Agent capabilities.
    pub agent_capabilities: AgentCapabilities,
    /// Optional agent identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<Implementation>,
    /// Supported auth methods (empty: no auth).
    #[serde(default)]
    pub auth_methods: Vec<serde_json::Value>,
    /// Extension facts; kage's own live under `kage`.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<AgentMeta>,
}

/// The `_meta` of [`InitializeResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentMeta {
    /// What a kage agent tells its clients about itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kage: Option<KageAgentInfo>,
}

/// What a kage agent tells its clients about itself, under
/// `_meta.kage` of the `initialize` result.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KageAgentInfo {
    /// The directory a session opened with an empty `cwd` runs in: the
    /// server's working directory. A client that knows no directory of
    /// its own, such as a browser, names its sessions' project by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The config options a new session opens with, so a client can
    /// show the default model, thinking level and mode before it opens
    /// one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<SessionConfigOption>,
}

/// `session/new` request params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    /// Working directory for the session (absolute path).
    pub cwd: String,
    /// MCP servers the client wants the session connected to.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
}

/// `session/new` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    /// Opaque session id the client uses on later calls.
    pub session_id: String,
    /// The session's config options, if the agent offers any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<SessionConfigOption>,
}

/// `session/load` request params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionRequest {
    /// Session to replay.
    pub session_id: String,
    /// Working directory.
    pub cwd: String,
    /// MCP servers the client wants the session connected to.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
}

/// `session/load` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionResponse {
    /// The session's config options, if the agent offers any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<SessionConfigOption>,
}

/// `session/resume` request params: reopen a session without replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionRequest {
    /// Session to reopen.
    pub session_id: String,
    /// Working directory.
    pub cwd: String,
    /// MCP servers the client wants the session connected to.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
}

/// `session/resume` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionResponse {
    /// The session's config options, if the agent offers any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<SessionConfigOption>,
}

/// `session/close` request params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseSessionRequest {
    /// Session the connection releases.
    pub session_id: String,
}

/// `session/close` result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseSessionResponse {}

/// `session/list` request params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsRequest {
    /// Only sessions recorded in this working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Opaque cursor from a previous page's `nextCursor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// `session/list` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsResponse {
    /// One page of sessions.
    pub sessions: Vec<SessionInfo>,
    /// Cursor for the next page, absent on the last one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// One entry of a `session/list` page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// Session id, usable with `session/load` and `session/resume`.
    pub session_id: String,
    /// Working directory the session was recorded in.
    pub cwd: String,
    /// Display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// ISO 8601 time of the last activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// kage's facts about the session, under `_meta.kage`.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<SessionInfoMeta>,
}

/// The `_meta` of a listed session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfoMeta {
    /// kage's facts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kage: Option<SessionInfoKage>,
}

/// kage's facts about a listed session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoKage {
    /// The session this one was forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
}

/// `session/set_config_option` request params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionRequest {
    /// Target session.
    pub session_id: String,
    /// The [`SessionConfigOption::id`] to change.
    pub config_id: String,
    /// The chosen [`SessionConfigSelectOption::value`].
    pub value: String,
}

/// `session/set_config_option` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionResponse {
    /// Every config option of the session, after the change.
    pub config_options: Vec<SessionConfigOption>,
}

/// `_kage/config/get` and `_kage/options/list` request params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigGetRequest {
    /// The session whose workdir the configuration is read for. Without
    /// one, or for an unknown session, the server's own directory is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Whether an MCP server is usable, as reported by the
/// `_kage/mcp_status` update.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpServerStatus {
    /// The server is live.
    Connected,
    /// The server is configured but has not been spawned yet. A host
    /// that opens its UI first resolves this to `connected` or
    /// `failed` once it starts MCP.
    Starting,
    /// The server failed to start or its transport died.
    Failed {
        /// The spawn, handshake or crash error.
        error: String,
    },
    /// The server needs an OAuth login before it can connect.
    NeedsAuth,
}

/// One MCP server's reachability, carried by the `_kage/mcp_status`
/// update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatusUpdate {
    /// Configured server name.
    pub name: String,
    /// Whether the server is usable.
    #[serde(flatten)]
    pub status: McpServerStatus,
}

/// `_kage/swarm/resume` request params: continue children of an
/// earlier `swarm` call of the session, whether they failed or never
/// ran. The children re-announce under the call and batch place they
/// first had and stream on their own sessions like any other member.
/// Once all have reported, the session gets a notice with the counts
/// and its next turn reads their results. A child that is not a swarm
/// child of the session, or is still working, refuses the request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwarmResumeRequest {
    /// Session whose swarm children continue.
    pub session_id: String,
    /// Child session id to a follow-up prompt. An empty prompt
    /// continues the child's task with a short nudge.
    pub members: BTreeMap<String, String>,
}

/// `_kage/swarm/resume` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwarmResumeResponse {
    /// The children the engine checked and attached, in map order.
    pub resumed: Vec<String>,
}

/// A prompt of a session, named by its text and how many earlier
/// prompts carried the same text, so a client finds it without knowing
/// the session's entry ids. A prompt without a text block, such as an
/// image-only message, is named by the absent text: `occurrence`
/// counts the earlier textless prompts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRef {
    /// The prompt's first text block, as typed. Absent names a prompt
    /// with no text block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// How many earlier prompts of the session were the same kind with
    /// the same text.
    #[serde(default)]
    pub occurrence: u32,
}

/// `_kage/session/fork` request params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionForkRequest {
    /// The session to copy.
    pub session_id: String,
    /// Copy only what came before this prompt. Absent copies the whole
    /// session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<PromptRef>,
}

/// `_kage/session/fork` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionForkResponse {
    /// The copy, a recorded session `session/load` opens.
    pub session_id: String,
    /// The source ended in a torn trailing line that the copy leaves
    /// behind, so the copy is shorter than the source's bytes.
    #[serde(default)]
    pub truncated: bool,
}

/// `_kage/session/export` and `_kage/session/compact` request params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRequest {
    /// The session to act on.
    pub session_id: String,
}

/// One engine option, as `_kage/options/list` reports it: the settings
/// a client can change through `_kage/options/set`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionEntry {
    /// The option name.
    pub name: String,
    /// The dotted `config.toml` key it writes.
    pub toml: String,
    /// One-line description.
    pub doc: String,
    /// What it accepts: `bool`, `int`, `fraction`, `choice`, `str` or
    /// `key`.
    pub kind: String,
    /// The smallest integer an `int` accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    /// The largest integer an `int` accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    /// The values a `choice` accepts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// The default value.
    pub default: serde_json::Value,
    /// The value in effect.
    pub value: serde_json::Value,
    /// Whether the value comes from a config file rather than the
    /// default.
    #[serde(default)]
    pub configured: bool,
    /// Whether a change applies while sessions run; otherwise it
    /// applies at the next session start.
    #[serde(default)]
    pub live: bool,
}

/// One model a client can pick, as `_kage/models/list` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    /// `provider/model`, the value the `model` config option takes.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Context window in tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// USD per million input tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_cost: Option<f64>,
    /// USD per million output tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_cost: Option<f64>,
    /// The thinking levels it accepts, by the `thinking` option's
    /// values. Empty when it does not think.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking: Vec<String>,
    /// Whether it reads images.
    #[serde(default)]
    pub images: bool,
    /// Release date, `YYYY-MM-DD`, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released: Option<String>,
}

/// One provider with credentials and the models it offers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProvider {
    /// The id models are qualified with.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Its models, in the provider's order.
    pub models: Vec<ModelEntry>,
}

/// `_kage/models/list` result: every model the engine can run now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsResponse {
    /// Providers by id.
    pub providers: Vec<ModelProvider>,
}

/// `_kage/options/list` and `_kage/options/set` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionsResponse {
    /// Every option, in the engine's order.
    pub options: Vec<OptionEntry>,
}

/// `_kage/options/set` request params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionSetRequest {
    /// The option to change.
    pub name: String,
    /// The new value: a boolean, a number or a string, as the option's
    /// kind takes.
    pub value: serde_json::Value,
}

/// `_kage/config/set` request params.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSetRequest {
    /// The session whose workdir the answering snapshot is read for, as
    /// in [`ConfigGetRequest`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The table names leading to the entry, such as
    /// `["mcp", "servers", "github"]`. The first is `providers`, `mcp`,
    /// `permissions`, `plugins` or `acp`; `ui` only as the two-key
    /// `ui.theme_dark` or `ui.theme_light`.
    pub path: Vec<String>,
    /// The entry's new value in the snapshot's shape; `null` removes the
    /// entry. A string the snapshot redacted keeps the value on file.
    #[serde(default)]
    pub value: Option<serde_json::Value>,
}

/// `_kage/config/test` request params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigTestRequest {
    /// A provider to list the models of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderProbe>,
    /// An MCP server to connect to and list the tools of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpProbe>,
    /// An external ACP agent to start and run the handshake with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<AcpProbe>,
}

/// An MCP server to connect to, as the config holds it or as a form
/// holds it before it is saved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpProbe {
    /// The server's name. A redacted value in `server` keeps the one
    /// saved under this name.
    pub name: String,
    /// The `[mcp.servers.<name>]` table in the snapshot's shape.
    pub server: serde_json::Value,
}

/// An external ACP agent to start.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpProbe {
    /// The agent's name. A redacted value in `env` keeps the one saved
    /// under this name.
    pub name: String,
    /// The executable.
    pub command: String,
    /// Its arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A provider to reach, as the config holds it or as a form holds it
/// before it is saved. Omitted fields fall back to the saved entry and
/// then to the provider's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProbe {
    /// The provider id.
    pub id: String,
    /// The wire protocol: `openai`, `anthropic` or `gemini`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The endpoint base URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The environment variable holding the key; empty for none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// A key typed into the form, used for this request only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// The URL the model list is fetched from, when the endpoint's
    /// own is not the right one - a models.dev-style directory
    /// document, say. Empty or absent reads the endpoint itself. A
    /// key and headers are only sent when it shares the base URL's
    /// host and port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_url: Option<String>,
    /// Extra headers. A redacted value keeps the saved one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

/// `_kage/config/test` result: whether the provider's model list
/// answered, and what it listed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigTestResult {
    /// Whether the endpoint answered with a model list.
    pub ok: bool,
    /// The HTTP status, when the endpoint answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// What happened, for the user: the request and its outcome.
    pub message: String,
    /// How long the request took, in milliseconds.
    pub millis: u64,
    /// The models listed, with the context and output limits the
    /// endpoint or the model catalog knows.
    #[serde(default)]
    pub models: Vec<ProbeModel>,
    /// The tools an MCP server listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ProbeTool>,
    /// The name and version an ACP agent gave in its handshake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// One tool an MCP server listed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeTool {
    /// The tool's name on its server.
    pub name: String,
    /// What it does, as the server describes it.
    #[serde(default)]
    pub description: String,
}

/// `_kage/folders` request params: the folder to list, on the machine
/// kage runs on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FoldersRequest {
    /// An absolute path, or one starting with `~`. Missing or empty
    /// lists the home folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// `_kage/folders` result: the folders inside one folder, for a client
/// to browse to the folder a session opens in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FoldersResult {
    /// The listed folder, absolute and resolved.
    pub path: String,
    /// Its parent, unless it is the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The home folder of the user kage runs as, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// The names of the folders inside, hidden ones left out, sorted.
    pub folders: Vec<String>,
    /// Whether the list was cut at its cap.
    #[serde(default)]
    pub truncated: bool,
    /// How many entries were left out: hidden ones, entries that
    /// could not be read, and names that are not UTF-8.
    #[serde(default)]
    pub skipped: usize,
}

/// `_kage/themes` request params: none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemesRequest {}

/// `_kage/themes` result: the user themes in the themes folder on the
/// machine the agent runs on, for a client that draws its own palette.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemesResult {
    /// One entry per theme file, sorted by name.
    pub themes: Vec<UserTheme>,
    /// Files left out, as `"<file name>: <reason>"` with reason one of
    /// `non-utf8`, `unreadable`, `invalid`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

/// One user theme file, as a client that draws its own palette reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserTheme {
    /// The file name without its `.toml`.
    pub name: String,
    /// The bundled theme it starts from, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Its `[gui]` table: palette token names to colors.
    #[serde(default)]
    pub gui: BTreeMap<String, String>,
}

/// `_kage/providers/directory` request params: a provider directory in
/// the models.dev `api.json` shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryRequest {
    /// Where the directory is; models.dev when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// A key the directory needs, sent as a bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// `_kage/providers/directory` result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryResult {
    /// The providers the directory lists, by id.
    pub providers: Vec<DirectoryProvider>,
}

/// One provider of a directory.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryProvider {
    /// The provider id.
    pub id: String,
    /// Its display name.
    pub name: String,
    /// Its API endpoint, when the directory names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// The environment variables its key is read from.
    #[serde(default)]
    pub env: Vec<String>,
    /// The protocol kage speaks to it (`openai`, `anthropic` or
    /// `gemini`), when it speaks one kage knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Its models that can call tools.
    pub models: Vec<DirectoryModel>,
}

/// One model of a directory provider.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryModel {
    /// The model id.
    pub id: String,
    /// Its display name.
    pub name: String,
    /// The context window in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// The most output tokens per turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    /// Whether it thinks.
    #[serde(default)]
    pub reasoning: bool,
    /// The inputs it takes (`text`, `image`, `pdf`, ...).
    #[serde(default)]
    pub input: Vec<String>,
    /// The media it produces (`text`, `image`, `audio`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub produces: Vec<String>,
    /// Prices in USD per million tokens, when listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<DirectoryCost>,
}

/// A model's prices in USD per million tokens.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryCost {
    /// Input tokens.
    pub input: f64,
    /// Output tokens.
    pub output: f64,
    /// Cached input reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Cached input writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// One model a provider listed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeModel {
    /// The model id the endpoint takes.
    pub id: String,
    /// A display name, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The context window in tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// The most output tokens per turn, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u64>,
    /// Whether the model thinks, when the listing says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// The inputs it takes (`text`, `image`, ...), when listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<String>,
    /// The media it produces (`text`, `image`, ...), when listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub produces: Vec<String>,
    /// The effort levels it accepts, when listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
}

/// `_kage/plugins/install` request params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInstallRequest {
    /// Where the plugin is: an `https://` URL or a path on the agent's
    /// machine, to one `.lua` file.
    pub source: String,
    /// The name to install it as; the source file's stem when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether an installed plugin of the same name is replaced.
    #[serde(default)]
    pub replace: bool,
}

/// `_kage/plugins/remove` request params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginRemoveRequest {
    /// The plugin's name, its file stem.
    pub name: String,
}

/// `_kage/registry/search` request params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrySearchRequest {
    /// Words a package's name, description or tags must all contain;
    /// empty lists every package.
    #[serde(default)]
    pub query: String,
    /// Fetch the index again instead of using a cached one.
    #[serde(default)]
    pub refresh: bool,
}

/// `_kage/registry/search` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrySearchResult {
    /// The registry's index URL.
    pub registry: String,
    /// Set when the registry could not be reached and a cached index
    /// stood in: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
    /// The matching packages, by name.
    pub packages: Vec<RegistryPackage>,
}

/// One package in a registry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryPackage {
    /// The package name.
    pub name: String,
    /// One line on what it does.
    #[serde(default)]
    pub description: String,
    /// Keywords it is found by.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Its home page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// The release an install picks, absent when none fits this kage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Why no release fits, when none does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// What that release may ask for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<RegistryCapability>,
    /// The version installed from a registry, when one is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
}

/// One capability a package may ask for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryCapability {
    /// The capability's name.
    pub name: String,
    /// What it lets a plugin do.
    #[serde(default)]
    pub summary: String,
}

/// `_kage/registry/install` request params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryInstallRequest {
    /// The package to install.
    pub name: String,
    /// The release, the newest that fits this kage when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Replace a script or local package of the same name, or discard
    /// local changes to an installed one.
    #[serde(default)]
    pub force: bool,
}

/// `_kage/registry/install` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryInstallResult {
    /// The package installed.
    pub name: String,
    /// The version now installed.
    pub version: String,
    /// The version it replaced, when one came from a registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// Every capability it may request. Installing grants none.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// `_kage/image/save` request params. Despite the name, it saves an
/// audio clip as well as an image.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSaveRequest {
    /// The session the image or clip is in.
    pub session_id: String,
    /// The sha256 of the image's or clip's bytes, lowercase hex.
    pub sha256: String,
    /// Where to write it, relative to the session's directory. Omitted,
    /// a new file under `generated/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// `_kage/auth/set` request params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthSetRequest {
    /// The provider the key is for.
    pub provider: String,
    /// The API key; `null` removes the saved one.
    #[serde(default)]
    pub key: Option<String>,
}

/// `_kage/session/rename` request params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRenameRequest {
    /// The session to name.
    pub session_id: String,
    /// The new title.
    pub title: String,
}

/// `_kage/session/export` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionExportResponse {
    /// The session's transcript as Markdown.
    pub markdown: String,
}

/// `_kage/fs` request params. `path` is relative to the session
/// workdir; empty or `.` lists the workdir itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsRequest {
    /// Target session, whose workdir confines every path.
    pub session_id: String,
    /// Which operation to run.
    pub op: FsOp,
    /// Path relative to the session workdir.
    #[serde(default)]
    pub path: String,
}

/// What `_kage/fs` should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsOp {
    /// List a directory subtree, depth- and entry-capped.
    List,
    /// Read one file, capped at 512 KB.
    Read,
}

/// `_kage/fs` result for the op that was asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FsResult {
    /// The answer to a `list` op.
    List(FsListResult),
    /// The answer to a `read` op.
    Read(FsReadResult),
}

/// `_kage/fs` list result: a capped subtree of the session workdir.
/// When [`FsListResult::truncated`] is set the client continues by
/// listing a subdirectory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsListResult {
    /// The entries found, a directory directly before its children.
    pub entries: Vec<FsEntry>,
    /// Whether caps cut the subtree short.
    pub truncated: bool,
}

/// One `_kage/fs` list entry. `path` is relative to the session
/// workdir, with `/` separators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsEntry {
    /// Path relative to the session workdir.
    pub path: String,
    /// What the entry is.
    pub kind: FsKind,
    /// Size in bytes; directories report 0.
    pub size: u64,
}

/// What a `_kage/fs` list entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsKind {
    /// A directory.
    Directory,
    /// A regular file.
    File,
    /// Anything else, such as a symlink or a fifo.
    Other,
}

/// `_kage/fs` read result. A `binary` file carries no `content`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadResult {
    /// The file bytes as UTF-8 text, capped at 512 KB.
    pub content: String,
    /// Whether the file was longer than the cap.
    pub truncated: bool,
    /// Whether the file is not valid UTF-8.
    pub binary: bool,
}

/// A session setting the client can show and change (a select).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOption {
    /// Stable id sent back in `session/set_config_option`.
    pub id: String,
    /// Human label.
    pub name: String,
    /// Optional longer explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// What the option controls, so clients can place it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<SessionConfigCategory>,
    /// The option's control type.
    #[serde(rename = "type")]
    pub kind: SessionConfigKind,
    /// The selected [`SessionConfigSelectOption::value`].
    pub current_value: String,
    /// The values to choose from.
    pub options: Vec<SessionConfigSelectOption>,
}

/// The control type of a [`SessionConfigOption`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionConfigKind {
    /// Pick one of `options`.
    Select,
    /// A free-form string. An empty value clears the setting.
    Text,
}

/// What a [`SessionConfigOption`] controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionConfigCategory {
    /// The permission mode.
    Mode,
    /// The model.
    Model,
    /// The thinking level.
    ThoughtLevel,
}

/// One value of a select [`SessionConfigOption`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSelectOption {
    /// Value sent back in `session/set_config_option`.
    pub value: String,
    /// Human label.
    pub name: String,
    /// Optional longer explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// An MCP server the client asks the agent to connect to. Stdio
/// entries carry no `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpServer {
    /// Streamable HTTP.
    Http(McpServerHttp),
    /// Legacy HTTP+SSE, which kage cannot connect to.
    Sse(McpServerHttp),
    /// A local process over stdio.
    #[serde(untagged)]
    Stdio(McpServerStdio),
}

/// A stdio [`McpServer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerStdio {
    /// Server name, unique within the session.
    pub name: String,
    /// Program to run.
    pub command: String,
    /// Program arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: Vec<EnvVariable>,
}

/// An `http` or `sse` [`McpServer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerHttp {
    /// Server name, unique within the session.
    pub name: String,
    /// Endpoint URL.
    pub url: String,
    /// Headers sent with every request.
    #[serde(default)]
    pub headers: Vec<HttpHeader>,
}

/// One environment variable of a stdio [`McpServer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVariable {
    /// Variable name.
    pub name: String,
    /// Variable value.
    pub value: String,
}

/// One header of a remote [`McpServer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpHeader {
    /// Header name.
    pub name: String,
    /// Header value.
    pub value: String,
}

/// `session/prompt` request params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    /// Target session.
    pub session_id: String,
    /// The user's turn as content blocks.
    pub prompt: Vec<ContentBlock>,
    /// How the prompt joins a run already in flight. Absent queues it
    /// until that run ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<PromptDelivery>,
}

/// How a prompt joins a run already in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptDelivery {
    /// Deliver at the running run's next turn boundary.
    Steer,
    /// Deliver as a new run once the current run ends.
    #[default]
    Queue,
}

/// Why a prompt turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its turn.
    EndTurn,
    /// Hit the output token cap.
    MaxTokens,
    /// Hit the max-requests-per-turn cap.
    MaxTurnRequests,
    /// The model refused.
    Refusal,
    /// The turn was cancelled.
    Cancelled,
}

/// `session/prompt` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    /// Why the turn ended.
    pub stop_reason: StopReason,
}

/// `session/cancel` notification params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelNotification {
    /// Session to cancel.
    pub session_id: String,
}

/// One content block (`type`-tagged, ACP/MCP aligned).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text(TextContent),
    /// Inline image bytes.
    Image(BlobContent),
    /// Inline audio bytes.
    Audio(BlobContent),
    /// A pointer to a resource.
    ResourceLink(ResourceLink),
    /// An embedded resource.
    Resource(EmbeddedResource),
    /// A block type this build does not know. Keeping it means one
    /// unknown block cannot fail the whole update it rode in on;
    /// matches against it contribute nothing (no text to show).
    #[serde(other)]
    Unknown,
}

impl ContentBlock {
    /// Shorthand for a text block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent { text: text.into() })
    }

    /// The concatenated text this block contributes, if any.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(t) => Some(&t.text),
            _ => None,
        }
    }
}

/// `text` content payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextContent {
    /// The text.
    pub text: String,
}

/// `image` / `audio` content payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobContent {
    /// Base64 bytes.
    pub data: String,
    /// MIME type.
    pub mime_type: String,
    /// Optional source uri.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

/// `resource_link` content payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLink {
    /// Resource uri.
    pub uri: String,
    /// Resource name.
    pub name: String,
    /// Optional MIME type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// `resource` embedded payload (kept opaque; we only forward it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddedResource {
    /// The embedded resource object (uri + text|blob + mimeType).
    pub resource: serde_json::Value,
}

/// A `session/update` notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    /// Session this update belongs to.
    pub session_id: String,
    /// The update payload.
    pub update: SessionUpdate,
}

/// The `sessionUpdate`-tagged update union.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    /// Echo of a user message chunk.
    UserMessageChunk(MessageChunk),
    /// Assistant text chunk.
    AgentMessageChunk(MessageChunk),
    /// Assistant reasoning chunk.
    AgentThoughtChunk(MessageChunk),
    /// A tool call started.
    ToolCall(ToolCall),
    /// A tool call's status/output changed.
    ToolCallUpdate(ToolCallUpdate),
    /// The agent's plan.
    Plan(Plan),
    /// Slash-command list changed.
    AvailableCommandsUpdate(AvailableCommandsUpdate),
    /// The active session mode changed.
    CurrentModeUpdate(CurrentModeUpdate),
    /// Context window usage changed.
    UsageUpdate(UsageUpdate),
    /// Session metadata (title, activity time) changed.
    SessionInfoUpdate(SessionInfoUpdate),
    /// The session's config options changed.
    ConfigOptionUpdate(ConfigOptionUpdate),
    /// A subagent was announced or changed (draft RFD PR #1992).
    SubagentUpdate(SubagentUpdate),
    /// A turn of the running prompt began or ended.
    #[serde(rename = "_kage/turn")]
    Turn(TurnUpdate),
    /// A message for the user outside the conversation.
    #[serde(rename = "_kage/notice")]
    Notice(NoticeUpdate),
    /// Older context turns were summarized.
    #[serde(rename = "_kage/compaction")]
    Compaction(CompactionUpdate),
    /// An MCP server's reachability changed.
    #[serde(rename = "_kage/mcp_status")]
    McpStatus(McpStatusUpdate),
    /// Any update kind kage does not know. Never sent.
    #[serde(other)]
    Unknown,
}

/// A message/thought chunk payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageChunk {
    /// The chunk content.
    pub content: ContentBlock,
    /// Kage extension fields. A replayed thought carries how long it
    /// took under `_meta.kage.durationMs`.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<ChunkMeta>,
}

/// The `_meta` extension object kage adds to a message chunk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkMeta {
    /// The kage-namespaced fields.
    #[serde(default)]
    pub kage: KageMeta,
}

/// Tool kind hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reads data.
    Read,
    /// Edits a file.
    Edit,
    /// Deletes something.
    Delete,
    /// Moves something.
    Move,
    /// Searches.
    Search,
    /// Executes a command.
    Execute,
    /// Reasoning step.
    Think,
    /// Fetches remote data.
    Fetch,
    /// Anything else, including a kind this build does not know.
    #[serde(other)]
    #[default]
    Other,
}

/// Tool-call lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Not started.
    Pending,
    /// Running.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Failed.
    Failed,
    /// A status this build does not know, kept so one new status does
    /// not drop the whole tool call.
    #[serde(other)]
    Unknown,
}

/// A `tool_call` update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    /// Correlation id.
    pub tool_call_id: String,
    /// Human title (usually the tool name).
    pub title: String,
    /// Tool kind hint.
    #[serde(default)]
    pub kind: ToolKind,
    /// Lifecycle status.
    pub status: ToolCallStatus,
    /// Rich content (output, diffs, terminals).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolCallContent>,
    /// Raw tool input echoed for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<serde_json::Value>,
    /// Kage extension fields. A `swarm` call names its members and
    /// template under `_meta.kage.swarm`.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolCallMeta>,
}

/// A `tool_call_update` (partial; only changed fields set).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    /// Correlation id.
    pub tool_call_id: String,
    /// New title, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// New kind, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    /// New status, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    /// Replacement content, if any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolCallContent>,
    /// Raw tool input, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<serde_json::Value>,
    /// Raw tool output, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<serde_json::Value>,
    /// Kage extension fields, if changed. A `swarm` call names its
    /// members and template under `_meta.kage.swarm` once its input
    /// has streamed whole.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<ToolCallMeta>,
}

/// Rich tool-call content (`type`-tagged).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    /// A content block (typically the tool's text output).
    Content(MessageChunk),
    /// A unified diff.
    Diff(DiffContent),
    /// A reference to a created terminal.
    Terminal(TerminalRef),
}

/// `diff` tool-call content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffContent {
    /// File path.
    pub path: String,
    /// Prior text, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    /// New text.
    pub new_text: String,
}

/// `terminal` tool-call content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalRef {
    /// Terminal id.
    pub terminal_id: String,
}

/// A `plan` update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// Plan entries.
    pub entries: Vec<serde_json::Value>,
}

/// An `available_commands_update`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommandsUpdate {
    /// The current slash-command list.
    pub available_commands: Vec<serde_json::Value>,
}

/// A `current_mode_update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentModeUpdate {
    /// The newly active mode id.
    pub current_mode_id: String,
}

/// A `usage_update`: context tokens in use and the window size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageUpdate {
    /// Tokens currently in context.
    pub used: u64,
    /// Context window size in tokens.
    pub size: u64,
    /// Cumulative session cost, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    /// kage's token totals, under `_meta.kage`.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<UsageMeta>,
}

/// The `_meta` of a usage update.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageMeta {
    /// kage's token totals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kage: Option<UsageTokens>,
}

/// The session's cumulative token totals, split the way providers bill
/// them. The four counts do not overlap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTokens {
    /// Prompt tokens neither read from nor written to the prompt cache.
    pub input: u64,
    /// Tokens the model emitted.
    pub output: u64,
    /// Prompt tokens served from the provider's cache.
    pub cache_read: u64,
    /// Prompt tokens written to the provider's cache.
    pub cache_write: u64,
}

/// A monetary amount.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    /// The amount.
    pub amount: f64,
    /// ISO 4217 currency code.
    pub currency: String,
}

/// A `session_info_update`. Omitted fields are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoUpdate {
    /// New display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// ISO 8601 time of the last activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// A `config_option_update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionUpdate {
    /// Every config option of the session, as it is now.
    pub config_options: Vec<SessionConfigOption>,
}

/// A `subagent_update`, sent on the parent session. The first one for
/// an id announces the child. Omitted fields are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentUpdate {
    /// The child's session id, used by its own `session/update`s.
    pub subagent_session_id: String,
    /// Short label for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Summary of the work delegated to the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Operations the client may perform on the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<SubagentSessionCapabilities>,
    /// Lifecycle state. A child never given one is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<SubagentState>,
    /// Swarm batch membership, when a `swarm` call started the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm: Option<SubagentSwarm>,
    /// Why the child paused. Set while [`SubagentState::Paused`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The parent's `agent` or `swarm` call that started the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// What the child has used so far, or in all once it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SubagentUsage>,
    /// The model the child runs, as `provider/model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Whether the child runs in the background: the parent's turn does
    /// not wait for it, and its end arrives on its own later.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
}

/// A subagent's token totals, cost and run time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentUsage {
    /// Prompt tokens, cache reads aside.
    pub input: u64,
    /// Completion tokens.
    pub output: u64,
    /// Prompt tokens read from the provider's cache.
    #[serde(default)]
    pub cache_read: u64,
    /// Prompt tokens written to the provider's cache.
    #[serde(default)]
    pub cache_write: u64,
    /// Spend in USD, when the model is priced.
    #[serde(default)]
    pub cost: f64,
    /// Time spent running, in milliseconds, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_ms: Option<u64>,
}

/// Client operations permitted on one subagent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SubagentSessionCapabilities {
    /// The client may `session/cancel` the child.
    #[serde(default)]
    pub cancel: bool,
}

/// Lifecycle state of a subagent. Every state but `running` and
/// `paused` is final.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    /// Working on its task.
    Running,
    /// Finished its task.
    Completed,
    /// Could not finish its task.
    Failed,
    /// Was cancelled.
    Cancelled,
    /// Temporarily not making progress, such as a child waiting out a
    /// provider rate limit. `SubagentUpdate::reason` says why, and a
    /// later state follows.
    Paused,
    /// A state this build does not know, kept so one new state does
    /// not drop the update that carried it.
    #[serde(other)]
    Unknown,
}

/// Swarm batch membership of a subagent: which batch it belongs to and
/// where it sits in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentSwarm {
    /// Id of the swarm batch, shared by every member of one call.
    pub id: String,
    /// The item this child was spawned for.
    pub item: String,
    /// Position of this child in the batch, 0-based.
    pub index: u32,
    /// How many children the batch has.
    pub total: u32,
}

/// Whether a `_kage/turn` update opens or closes a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    /// The turn began.
    Start,
    /// The turn ended.
    End,
}

/// Why a turn ended: whether the model asked for tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnReason {
    /// The model requested tool calls, so more turns follow.
    ToolCalls,
    /// The model replied without tool calls.
    NoToolCalls,
}

/// A `_kage/turn` update: one provider round trip of the running
/// prompt, with the tool calls it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnUpdate {
    /// Whether the turn began or ended.
    pub phase: TurnPhase,
    /// Why the turn ended. Present on `end`, absent on `start`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<TurnReason>,
    /// When the prompt's run ended, in seconds since the Unix epoch.
    /// Present on the replayed end of a run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
    /// How long the prompt's run took, in milliseconds. Present on the
    /// replayed end of a run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub took_ms: Option<u64>,
}

/// Severity of a `_kage/notice` update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeTone {
    /// Informational.
    Info,
    /// Something the user may want to act on.
    Warn,
    /// Something failed.
    Error,
    /// Something the user wanted happened.
    Success,
}

/// A `_kage/notice` update: a message for the user that is not part
/// of the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoticeUpdate {
    /// Severity.
    pub tone: NoticeTone,
    /// Message text.
    pub text: String,
}

/// The phase of a `_kage/compaction` update.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionPhase {
    /// The agent began summarizing older turns.
    Started,
    /// The summary replaced the older turns.
    #[default]
    Complete,
    /// The compaction was given up before a summary replaced any
    /// turns, as when the summarizer failed or the run was cancelled.
    Aborted,
}

/// A `_kage/compaction` update: the agent summarized older turns to
/// fit the context window, or gave up trying. `phase` names which;
/// an absent phase (a host older than the field) reads as
/// [`CompactionPhase::Complete`], the shape such hosts always sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionUpdate {
    /// What happened.
    #[serde(default)]
    pub phase: CompactionPhase,
    /// Recent turns kept verbatim, on `started` and `complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<u64>,
    /// Older turns the summary replaced, on `started` and `complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarized: Option<u64>,
    /// Context tokens in use before the compaction, on `complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    /// Context tokens in use by the first turn after the compaction,
    /// on `complete` once that turn has reported usage. Absent when
    /// the run ended before such a turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    /// The summary text that replaced the older turns, framed for the
    /// model, on `complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// How a permission option resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    /// Allow this one call.
    AllowOnce,
    /// Allow and remember.
    AllowAlways,
    /// Reject this one call.
    RejectOnce,
    /// Reject and remember.
    RejectAlways,
    /// A kind this build does not know, kept so one new kind does not
    /// drop the whole permission request.
    #[serde(other)]
    Unknown,
}

/// One choice offered in a permission prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    /// Stable id echoed back in the response.
    pub option_id: String,
    /// Human label.
    pub name: String,
    /// What picking it means.
    pub kind: PermissionOptionKind,
}

/// `session/request_permission` request params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    /// Session the call belongs to.
    pub session_id: String,
    /// The tool call awaiting a verdict.
    pub tool_call: ToolCallUpdate,
    /// The choices offered.
    pub options: Vec<PermissionOption>,
    /// Kage extension fields. A plan-mode review carries the plan
    /// document under `_meta.kage.planReview`.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

/// The client's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    /// The turn was cancelled before the user chose.
    Cancelled,
    /// The user picked an option.
    Selected(SelectedOption),
}

/// The chosen option id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedOption {
    /// Which [`PermissionOption::option_id`] was picked.
    pub option_id: String,
}

/// `session/request_permission` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestPermissionResponse {
    /// The verdict.
    pub outcome: PermissionOutcome,
}

/// The `session/request_permission` result as the agent reads it: the
/// base response plus the kage `_meta`, which a revise answer uses to
/// carry the user's requested changes under `_meta.kage.planReview`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestPermissionResult {
    /// The verdict.
    pub outcome: PermissionOutcome,
    /// Kage extension fields.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

/// The `_meta` extension object kage adds to a permission exchange.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestMeta {
    /// The kage-namespaced fields.
    #[serde(default)]
    pub kage: KageMeta,
}

/// The `_meta` extension object kage adds to a tool call and its
/// updates.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallMeta {
    /// The kage-namespaced fields.
    #[serde(default)]
    pub kage: KageMeta,
}

/// The swarm facts of a `swarm` tool call, under
/// `_meta.kage.swarm`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwarmMeta {
    /// One entry per member: the item a new child runs, or the session
    /// id of a child the call resumes.
    pub members: Vec<String>,
    /// The prompt template the items substitute into. Absent on a
    /// resume-only call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// The `kage` extension fields of a `_meta`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KageMeta {
    /// The plan review of a plan-mode `exit_plan` ask: the plan
    /// document on the request, the user's requested changes on a
    /// revise answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_review: Option<PlanReview>,
    /// The swarm batch a `swarm` tool call announces, on the call and
    /// on the update that carries its whole input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm: Option<SwarmMeta>,
    /// How long a replayed thought or tool call took, in milliseconds,
    /// as recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// One question of an `ask_user_question` call: the question on
    /// the request, the user's answer on the response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<Box<QuestionMeta>>,
}

/// One question's payload. `prompt` rides the
/// `session/request_permission` request; `answer` may ride the
/// response, where it takes the place of the option picked: several
/// choices, or the user's own words.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionMeta {
    /// The question asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<QuestionPrompt>,
    /// The labels picked, or the user's own words as one entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<Vec<String>>,
}

/// A question the model asks, with its choices.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionPrompt {
    /// A short label, such as `Auth method`.
    pub header: String,
    /// The whole question.
    pub question: String,
    /// The choices, in order. Option `choice-<n>` of the request picks
    /// the `n`th, counting from zero.
    pub options: Vec<QuestionChoice>,
    /// Whether the user may pick several choices.
    #[serde(default)]
    pub multi_select: bool,
}

/// One choice of a [`QuestionPrompt`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionChoice {
    /// What the user picks.
    pub label: String,
    /// What picking it means.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// One plan review's payload. Exactly one field is set per message:
/// `plan` rides the `session/request_permission` request, `revision`
/// rides the answer when the user picked revise.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanReview {
    /// The plan document in Markdown, as `exit_plan` presented it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The changes the user asked for, in their own words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(value: &T, json: serde_json::Value)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let encoded = serde_json::to_value(value).unwrap();
        assert_eq!(encoded, json, "serialized shape must match the spec");
        let decoded: T = serde_json::from_value(json).unwrap();
        assert_eq!(&decoded, value, "round-trip must be lossless");
    }

    #[test]
    fn option_shapes_round_trip() {
        roundtrip(
            &OptionsResponse {
                options: vec![OptionEntry {
                    name: "agent_max_depth".into(),
                    toml: "agents.max_depth".into(),
                    doc: "How deep agents may nest.".into(),
                    kind: "int".into(),
                    min: Some(0),
                    max: Some(3),
                    values: Vec::new(),
                    default: serde_json::json!(1),
                    value: serde_json::json!(2),
                    configured: true,
                    live: false,
                }],
            },
            serde_json::json!({"options": [{
                "name": "agent_max_depth", "toml": "agents.max_depth",
                "doc": "How deep agents may nest.", "kind": "int", "min": 0, "max": 3,
                "default": 1, "value": 2, "configured": true, "live": false,
            }]}),
        );
        roundtrip(
            &OptionSetRequest {
                name: "thinking_level".into(),
                value: serde_json::json!("high"),
            },
            serde_json::json!({"name": "thinking_level", "value": "high"}),
        );
    }

    #[test]
    fn initialize_shapes_match_spec() {
        roundtrip(
            &InitializeRequest {
                protocol_version: 1,
                client_capabilities: ClientCapabilities {
                    fs: FsCapability {
                        read_text_file: true,
                        write_text_file: true,
                    },
                    terminal: true,
                    subagents: Some(serde_json::json!({})),
                    meta: None,
                },
                client_info: None,
            },
            serde_json::json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": true, "writeTextFile": true},
                    "terminal": true,
                    "subagents": {}
                }
            }),
        );
        roundtrip(
            &InitializeResponse {
                protocol_version: 1,
                agent_capabilities: AgentCapabilities {
                    load_session: true,
                    steer: true,
                    prompt_capabilities: PromptCapabilities {
                        image: false,
                        audio: false,
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
                    name: "kage".into(),
                    title: None,
                    version: Some("0.1.0".into()),
                }),
                auth_methods: vec![],
                meta: None,
            },
            serde_json::json!({
                "protocolVersion": 1,
                "agentCapabilities": {
                    "loadSession": true,
                    "steer": true,
                    "promptCapabilities": {
                        "image": false, "audio": false, "embeddedContext": true
                    },
                    "mcpCapabilities": {"http": true, "sse": false},
                    "sessionCapabilities": {"list": {}, "resume": {}, "close": {}}
                },
                "agentInfo": {"name": "kage", "version": "0.1.0"},
                "authMethods": []
            }),
        );
    }

    /// An unknown block type decodes to [`ContentBlock::Unknown`] so
    /// the update carrying it stays usable.
    #[test]
    fn unknown_content_block_decodes_without_failing() {
        let block: ContentBlock =
            serde_json::from_value(serde_json::json!({"type": "hologram"})).unwrap();
        assert_eq!(block, ContentBlock::Unknown);
        assert_eq!(block.as_text(), None);
        let known: ContentBlock =
            serde_json::from_value(serde_json::json!({"type": "text", "text": "hi"})).unwrap();
        assert_eq!(known, ContentBlock::text("hi"));
    }

    /// An unknown tool kind or status decodes to a fallback instead of
    /// failing the whole `tool_call`, and encodes back to a legal
    /// value.
    #[test]
    fn unknown_tool_kind_and_status_fall_back() {
        let call: ToolCall = serde_json::from_value(serde_json::json!({
            "toolCallId": "c1",
            "title": "shiny",
            "kind": "brand_new",
            "status": "brand_new"
        }))
        .unwrap();
        assert_eq!(call.kind, ToolKind::Other);
        assert_eq!(call.status, ToolCallStatus::Unknown);
        let encoded = serde_json::to_value(&call).unwrap();
        assert_eq!(encoded["kind"], serde_json::json!("other"));
        assert_eq!(encoded["status"], serde_json::json!("unknown"));
        let again: ToolCall = serde_json::from_value(encoded).unwrap();
        assert_eq!(again, call);
        let known: ToolCall = serde_json::from_value(serde_json::json!({
            "toolCallId": "c1",
            "title": "shiny",
            "kind": "execute",
            "status": "in_progress"
        }))
        .unwrap();
        assert_eq!(known.kind, ToolKind::Execute);
        assert_eq!(known.status, ToolCallStatus::InProgress);
    }

    /// An unknown subagent state decodes instead of dropping the
    /// `subagent_update` that carried it.
    #[test]
    fn unknown_subagent_state_decodes() {
        let update: SessionNotification = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "subagent_update",
                "subagentSessionId": "child-1",
                "state": "dreaming"
            }
        }))
        .unwrap();
        let SessionUpdate::SubagentUpdate(sub) = update.update else {
            panic!("expected a subagent update");
        };
        assert_eq!(sub.state, Some(SubagentState::Unknown));
        assert_eq!(
            serde_json::to_value(&sub).unwrap()["state"],
            serde_json::json!("unknown")
        );
    }

    /// An unknown permission option kind decodes so the request keeps
    /// its other options and the ask still reaches the user.
    #[test]
    fn unknown_permission_option_kind_falls_back() {
        let request: RequestPermissionRequest = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "toolCall": {"toolCallId": "c1", "title": "t", "status": "pending"},
            "options": [
                {"optionId": "o1", "name": "New", "kind": "brand_new"},
                {"optionId": "o2", "name": "Allow", "kind": "allow_once"}
            ]
        }))
        .unwrap();
        assert_eq!(request.options[0].kind, PermissionOptionKind::Unknown);
        assert_eq!(request.options[1].kind, PermissionOptionKind::AllowOnce);
        assert_eq!(
            serde_json::to_value(&request).unwrap()["options"][0]["kind"],
            serde_json::json!("unknown")
        );
    }

    #[test]
    fn subagents_capability_is_any_value_but_false() {
        let caps = |json| serde_json::from_value::<ClientCapabilities>(json).unwrap();
        assert!(caps(serde_json::json!({"subagents": {}})).supports_subagents());
        assert!(caps(serde_json::json!({"subagents": true})).supports_subagents());
        assert!(!caps(serde_json::json!({"subagents": false})).supports_subagents());
        assert!(!caps(serde_json::json!({"subagents": null})).supports_subagents());
        assert!(!caps(serde_json::json!({})).supports_subagents());
    }

    #[test]
    fn kage_clients_choose_how_unconfigured_tools_run() {
        let caps = ClientCapabilities {
            meta: Some(ClientMeta {
                kage: Some(KageClientCapabilities {
                    unconfigured_tools: Some(UnconfiguredTools::Allow),
                }),
            }),
            ..ClientCapabilities::default()
        };
        roundtrip(
            &caps,
            serde_json::json!({
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false,
                "_meta": {"kage": {"unconfiguredTools": "allow"}},
            }),
        );
        assert!(caps.unconfigured_tools_run());
        assert!(!ClientCapabilities::default().unconfigured_tools_run());
        let ask: ClientCapabilities = serde_json::from_value(
            serde_json::json!({"_meta": {"kage": {"unconfiguredTools": "ask"}}}),
        )
        .unwrap();
        assert!(!ask.unconfigured_tools_run());
    }

    #[test]
    fn default_agent_capabilities_advertise_nothing_new() {
        roundtrip(
            &AgentCapabilities::default(),
            serde_json::json!({
                "loadSession": false,
                "steer": false,
                "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false},
                "mcpCapabilities": {"http": false, "sse": false},
                "sessionCapabilities": {}
            }),
        );
    }

    fn model_option() -> SessionConfigOption {
        SessionConfigOption {
            id: "model".into(),
            name: "Model".into(),
            description: None,
            category: Some(SessionConfigCategory::Model),
            kind: SessionConfigKind::Select,
            current_value: "a/one".into(),
            options: vec![
                SessionConfigSelectOption {
                    value: "a/one".into(),
                    name: "One".into(),
                    description: Some("the first".into()),
                },
                SessionConfigSelectOption {
                    value: "a/two".into(),
                    name: "Two".into(),
                    description: None,
                },
            ],
        }
    }

    fn model_option_json() -> serde_json::Value {
        serde_json::json!({
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": "a/one",
            "options": [
                {"value": "a/one", "name": "One", "description": "the first"},
                {"value": "a/two", "name": "Two"}
            ]
        })
    }

    #[test]
    fn config_option_shapes() {
        roundtrip(&model_option(), model_option_json());
        let mut goal = model_option();
        goal.id = "goal".into();
        goal.name = "Goal".into();
        goal.description = Some("what done looks like".into());
        goal.category = None;
        goal.kind = SessionConfigKind::Text;
        goal.current_value = "ship it".into();
        goal.options = vec![];
        roundtrip(
            &goal,
            serde_json::json!({
                "id": "goal",
                "name": "Goal",
                "description": "what done looks like",
                "type": "text",
                "currentValue": "ship it",
                "options": []
            }),
        );
        roundtrip(
            &SessionConfigOption {
                id: "thinking".into(),
                name: "Thinking".into(),
                description: Some("reasoning effort".into()),
                category: Some(SessionConfigCategory::ThoughtLevel),
                kind: SessionConfigKind::Select,
                current_value: "high".into(),
                options: vec![],
            },
            serde_json::json!({
                "id": "thinking",
                "name": "Thinking",
                "description": "reasoning effort",
                "category": "thought_level",
                "type": "select",
                "currentValue": "high",
                "options": []
            }),
        );
        roundtrip(
            &SetSessionConfigOptionRequest {
                session_id: "s1".into(),
                config_id: "model".into(),
                value: "a/two".into(),
            },
            serde_json::json!({"sessionId": "s1", "configId": "model", "value": "a/two"}),
        );
        roundtrip(
            &SetSessionConfigOptionResponse {
                config_options: vec![model_option()],
            },
            serde_json::json!({"configOptions": [model_option_json()]}),
        );
    }

    #[test]
    fn session_method_shapes() {
        roundtrip(
            &NewSessionResponse {
                session_id: "s1".into(),
                config_options: vec![model_option()],
            },
            serde_json::json!({"sessionId": "s1", "configOptions": [model_option_json()]}),
        );
        roundtrip(
            &NewSessionResponse {
                session_id: "s1".into(),
                config_options: vec![],
            },
            serde_json::json!({"sessionId": "s1"}),
        );
        roundtrip(&LoadSessionResponse::default(), serde_json::json!({}));
        roundtrip(
            &LoadSessionResponse {
                config_options: vec![model_option()],
            },
            serde_json::json!({"configOptions": [model_option_json()]}),
        );
        roundtrip(
            &ListSessionsRequest {
                cwd: Some("/w".into()),
                cursor: Some("50".into()),
            },
            serde_json::json!({"cwd": "/w", "cursor": "50"}),
        );
        roundtrip(&ListSessionsRequest::default(), serde_json::json!({}));
        roundtrip(
            &ListSessionsResponse {
                sessions: vec![
                    SessionInfo {
                        session_id: "s1".into(),
                        cwd: "/w".into(),
                        title: Some("Fix the build".into()),
                        updated_at: Some("2026-09-25T10:00:00Z".into()),
                        meta: None,
                    },
                    SessionInfo {
                        session_id: "s2".into(),
                        cwd: "/w".into(),
                        title: None,
                        updated_at: None,
                        meta: Some(SessionInfoMeta {
                            kage: Some(SessionInfoKage {
                                parent_session_id: Some("s1".into()),
                            }),
                        }),
                    },
                ],
                next_cursor: Some("50".into()),
            },
            serde_json::json!({
                "sessions": [
                    {
                        "sessionId": "s1",
                        "cwd": "/w",
                        "title": "Fix the build",
                        "updatedAt": "2026-09-25T10:00:00Z"
                    },
                    {"sessionId": "s2", "cwd": "/w", "_meta": {"kage": {"parentSessionId": "s1"}}}
                ],
                "nextCursor": "50"
            }),
        );
        roundtrip(
            &ResumeSessionRequest {
                session_id: "s1".into(),
                cwd: "/w".into(),
                mcp_servers: vec![],
            },
            serde_json::json!({"sessionId": "s1", "cwd": "/w", "mcpServers": []}),
        );
        roundtrip(&ResumeSessionResponse::default(), serde_json::json!({}));
        roundtrip(
            &CloseSessionRequest {
                session_id: "s1".into(),
            },
            serde_json::json!({"sessionId": "s1"}),
        );
        roundtrip(&CloseSessionResponse {}, serde_json::json!({}));
    }

    #[test]
    fn mcp_server_shapes() {
        roundtrip(
            &McpServer::Stdio(McpServerStdio {
                name: "fs".into(),
                command: "/bin/mcp-fs".into(),
                args: vec!["--root".into(), "/w".into()],
                env: vec![EnvVariable {
                    name: "TOKEN".into(),
                    value: "x".into(),
                }],
            }),
            serde_json::json!({
                "name": "fs",
                "command": "/bin/mcp-fs",
                "args": ["--root", "/w"],
                "env": [{"name": "TOKEN", "value": "x"}]
            }),
        );
        let remote = McpServerHttp {
            name: "api".into(),
            url: "https://example.com/mcp".into(),
            headers: vec![HttpHeader {
                name: "Authorization".into(),
                value: "Bearer x".into(),
            }],
        };
        let remote_json = |kind| {
            serde_json::json!({
                "type": kind,
                "name": "api",
                "url": "https://example.com/mcp",
                "headers": [{"name": "Authorization", "value": "Bearer x"}]
            })
        };
        roundtrip(&McpServer::Http(remote.clone()), remote_json("http"));
        roundtrip(&McpServer::Sse(remote), remote_json("sse"));
        let load: LoadSessionRequest = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "cwd": "/w",
            "mcpServers": [
                {"name": "fs", "command": "mcp-fs", "args": [], "env": []},
                remote_json("http")
            ]
        }))
        .unwrap();
        assert!(matches!(load.mcp_servers[0], McpServer::Stdio(_)));
        assert!(matches!(load.mcp_servers[1], McpServer::Http(_)));
    }

    #[test]
    fn prompt_and_stop_reason_shapes() {
        roundtrip(
            &PromptRequest {
                session_id: "s1".into(),
                prompt: vec![ContentBlock::text("hello")],
                delivery: None,
            },
            serde_json::json!({
                "sessionId": "s1",
                "prompt": [{"type": "text", "text": "hello"}]
            }),
        );
        roundtrip(
            &PromptRequest {
                session_id: "s1".into(),
                prompt: vec![ContentBlock::text("look")],
                delivery: Some(PromptDelivery::Steer),
            },
            serde_json::json!({
                "sessionId": "s1",
                "prompt": [{"type": "text", "text": "look"}],
                "delivery": "steer"
            }),
        );
        let queued: PromptRequest = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "prompt": [],
            "delivery": "queue"
        }))
        .unwrap();
        assert_eq!(queued.delivery, Some(PromptDelivery::Queue));
        roundtrip(
            &PromptResponse {
                stop_reason: StopReason::EndTurn,
            },
            serde_json::json!({"stopReason": "end_turn"}),
        );
    }

    #[test]
    fn session_update_variants_shapes() {
        roundtrip(
            &SessionNotification {
                session_id: "s1".into(),
                update: SessionUpdate::AgentMessageChunk(MessageChunk {
                    content: ContentBlock::text("hi"),
                    meta: None,
                }),
            },
            serde_json::json!({
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "hi"}
                }
            }),
        );
        roundtrip(
            &SessionUpdate::ToolCall(ToolCall {
                tool_call_id: "t1".into(),
                title: "shell".into(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Pending,
                content: vec![],
                raw_input: Some(serde_json::json!({"cmd": "ls"})),
                meta: None,
            }),
            serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "t1",
                "title": "shell",
                "kind": "execute",
                "status": "pending",
                "rawInput": {"cmd": "ls"}
            }),
        );
    }

    /// A `swarm` call names its members and template under
    /// `_meta.kage.swarm`, on the announce and on the update that
    /// carries the whole input.
    #[test]
    fn swarm_tool_call_meta_shapes() {
        let meta = ToolCallMeta {
            kage: KageMeta {
                swarm: Some(SwarmMeta {
                    members: vec!["kage-core".into(), "kage-tui".into()],
                    template: Some("review {{item}}".into()),
                }),
                ..KageMeta::default()
            },
        };
        let json = serde_json::json!({
            "kage": {"swarm": {"members": ["kage-core", "kage-tui"], "template": "review {{item}}"}}
        });
        roundtrip(&meta, json.clone());
        roundtrip(
            &SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: "t1".into(),
                raw_input: Some(serde_json::json!({})),
                meta: Some(meta),
                ..ToolCallUpdate::default()
            }),
            serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "t1",
                "rawInput": {},
                "_meta": json
            }),
        );
    }

    /// A resume-only swarm call carries the resumed session ids as its
    /// members and no template.
    #[test]
    fn swarm_meta_omits_an_absent_template() {
        let meta = ToolCallMeta {
            kage: KageMeta {
                swarm: Some(SwarmMeta {
                    members: vec!["01J8".into()],
                    template: None,
                }),
                ..KageMeta::default()
            },
        };
        roundtrip(
            &meta,
            serde_json::json!({"kage": {"swarm": {"members": ["01J8"]}}}),
        );
    }

    #[test]
    fn config_get_request_shapes() {
        roundtrip(&ConfigGetRequest::default(), serde_json::json!({}));
        roundtrip(
            &ConfigGetRequest {
                session_id: Some("s1".into()),
            },
            serde_json::json!({"sessionId": "s1"}),
        );
    }

    #[test]
    fn mcp_status_shapes() {
        roundtrip(
            &SessionNotification {
                session_id: "s1".into(),
                update: SessionUpdate::McpStatus(McpStatusUpdate {
                    name: "fs".into(),
                    status: McpServerStatus::Connected,
                }),
            },
            serde_json::json!({
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "_kage/mcp_status",
                    "name": "fs",
                    "status": "connected"
                }
            }),
        );
        roundtrip(
            &SessionUpdate::McpStatus(McpStatusUpdate {
                name: "db".into(),
                status: McpServerStatus::Failed {
                    error: "spawn failed".into(),
                },
            }),
            serde_json::json!({
                "sessionUpdate": "_kage/mcp_status",
                "name": "db",
                "status": "failed",
                "error": "spawn failed"
            }),
        );
        roundtrip(
            &SessionUpdate::McpStatus(McpStatusUpdate {
                name: "api".into(),
                status: McpServerStatus::NeedsAuth,
            }),
            serde_json::json!({
                "sessionUpdate": "_kage/mcp_status",
                "name": "api",
                "status": "needs_auth"
            }),
        );
    }

    #[test]
    fn fs_request_and_result_shapes() {
        roundtrip(
            &FsRequest {
                session_id: "s1".into(),
                op: FsOp::List,
                path: "src".into(),
            },
            serde_json::json!({"sessionId": "s1", "op": "list", "path": "src"}),
        );
        roundtrip(
            &FsRequest {
                session_id: "s1".into(),
                op: FsOp::Read,
                path: String::new(),
            },
            serde_json::json!({"sessionId": "s1", "op": "read", "path": ""}),
        );
        roundtrip(
            &FsResult::List(FsListResult {
                entries: vec![
                    FsEntry {
                        path: "src".into(),
                        kind: FsKind::Directory,
                        size: 0,
                    },
                    FsEntry {
                        path: "src/lib.rs".into(),
                        kind: FsKind::File,
                        size: 512,
                    },
                    FsEntry {
                        path: "link".into(),
                        kind: FsKind::Other,
                        size: 0,
                    },
                ],
                truncated: true,
            }),
            serde_json::json!({
                "op": "list",
                "entries": [
                    {"path": "src", "kind": "directory", "size": 0},
                    {"path": "src/lib.rs", "kind": "file", "size": 512},
                    {"path": "link", "kind": "other", "size": 0}
                ],
                "truncated": true
            }),
        );
        roundtrip(
            &FsResult::Read(FsReadResult {
                content: "hello".into(),
                truncated: false,
                binary: false,
            }),
            serde_json::json!({"op": "read", "content": "hello", "truncated": false, "binary": false}),
        );
        roundtrip(
            &FsResult::Read(FsReadResult {
                content: String::new(),
                truncated: true,
                binary: true,
            }),
            serde_json::json!({
                "op": "read",
                "content": "",
                "truncated": true,
                "binary": true
            }),
        );
    }

    #[test]
    fn usage_info_and_config_update_shapes() {
        roundtrip(
            &SessionUpdate::UsageUpdate(UsageUpdate {
                used: 1200,
                size: 200_000,
                cost: Some(Cost {
                    amount: 0.25,
                    currency: "USD".into(),
                }),
                meta: None,
            }),
            serde_json::json!({
                "sessionUpdate": "usage_update",
                "used": 1200,
                "size": 200_000,
                "cost": {"amount": 0.25, "currency": "USD"}
            }),
        );
        roundtrip(
            &SessionUpdate::UsageUpdate(UsageUpdate {
                used: 5,
                size: 10,
                cost: None,
                meta: None,
            }),
            serde_json::json!({"sessionUpdate": "usage_update", "used": 5, "size": 10}),
        );
        roundtrip(
            &SessionUpdate::UsageUpdate(UsageUpdate {
                used: 5,
                size: 10,
                cost: None,
                meta: Some(UsageMeta {
                    kage: Some(UsageTokens {
                        input: 3,
                        output: 2,
                        cache_read: 7,
                        cache_write: 1,
                    }),
                }),
            }),
            serde_json::json!({
                "sessionUpdate": "usage_update",
                "used": 5,
                "size": 10,
                "_meta": {"kage": {"input": 3, "output": 2, "cacheRead": 7, "cacheWrite": 1}}
            }),
        );
        roundtrip(
            &SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                title: Some("Fix the build".into()),
                updated_at: Some("2026-09-25T10:00:00Z".into()),
            }),
            serde_json::json!({
                "sessionUpdate": "session_info_update",
                "title": "Fix the build",
                "updatedAt": "2026-09-25T10:00:00Z"
            }),
        );
        roundtrip(
            &SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate {
                config_options: vec![model_option()],
            }),
            serde_json::json!({
                "sessionUpdate": "config_option_update",
                "configOptions": [model_option_json()]
            }),
        );
    }

    #[test]
    fn subagent_update_shapes() {
        roundtrip(
            &SessionUpdate::SubagentUpdate(SubagentUpdate {
                subagent_session_id: "child".into(),
                name: Some("reviewer".into()),
                task: Some("review the diff".into()),
                capabilities: Some(SubagentSessionCapabilities { cancel: true }),
                state: None,
                tool_call_id: Some("call_agent".into()),
                ..SubagentUpdate::default()
            }),
            serde_json::json!({
                "sessionUpdate": "subagent_update",
                "subagentSessionId": "child",
                "name": "reviewer",
                "task": "review the diff",
                "capabilities": {"cancel": true},
                "toolCallId": "call_agent"
            }),
        );
        for (state, name) in [
            (SubagentState::Running, "running"),
            (SubagentState::Completed, "completed"),
            (SubagentState::Failed, "failed"),
            (SubagentState::Cancelled, "cancelled"),
            (SubagentState::Paused, "paused"),
        ] {
            roundtrip(
                &SessionUpdate::SubagentUpdate(SubagentUpdate {
                    subagent_session_id: "child".into(),
                    state: Some(state),
                    ..SubagentUpdate::default()
                }),
                serde_json::json!({
                    "sessionUpdate": "subagent_update",
                    "subagentSessionId": "child",
                    "state": name
                }),
            );
        }
    }

    /// A swarm child's update carries its batch membership, and a
    /// rate-limited one pauses with a reason.
    #[test]
    fn subagent_swarm_and_paused_shapes() {
        roundtrip(
            &SessionUpdate::SubagentUpdate(SubagentUpdate {
                subagent_session_id: "child".into(),
                swarm: Some(SubagentSwarm {
                    id: "swarm_01J8".into(),
                    item: "kage-core".into(),
                    index: 0,
                    total: 2,
                }),
                ..SubagentUpdate::default()
            }),
            serde_json::json!({
                "sessionUpdate": "subagent_update",
                "subagentSessionId": "child",
                "swarm": {"id": "swarm_01J8", "item": "kage-core", "index": 0, "total": 2}
            }),
        );
        roundtrip(
            &SessionUpdate::SubagentUpdate(SubagentUpdate {
                subagent_session_id: "child".into(),
                state: Some(SubagentState::Paused),
                reason: Some("rate limited; retrying in 3s".into()),
                ..SubagentUpdate::default()
            }),
            serde_json::json!({
                "sessionUpdate": "subagent_update",
                "subagentSessionId": "child",
                "state": "paused",
                "reason": "rate limited; retrying in 3s"
            }),
        );
    }

    #[test]
    fn swarm_resume_shapes() {
        roundtrip(
            &SwarmResumeRequest {
                session_id: "s1".into(),
                members: BTreeMap::from([("01J8".into(), "go on".into())]),
            },
            serde_json::json!({"sessionId": "s1", "members": {"01J8": "go on"}}),
        );
        roundtrip(
            &SwarmResumeResponse {
                resumed: vec!["01J8".into()],
            },
            serde_json::json!({"resumed": ["01J8"]}),
        );
    }

    #[test]
    fn turn_update_shapes() {
        roundtrip(
            &SessionUpdate::Turn(TurnUpdate {
                phase: TurnPhase::Start,
                reason: None,
                at: None,
                took_ms: None,
            }),
            serde_json::json!({"sessionUpdate": "_kage/turn", "phase": "start"}),
        );
        for (reason, name) in [
            (TurnReason::ToolCalls, "tool_calls"),
            (TurnReason::NoToolCalls, "no_tool_calls"),
        ] {
            roundtrip(
                &SessionUpdate::Turn(TurnUpdate {
                    phase: TurnPhase::End,
                    reason: Some(reason),
                    at: None,
                    took_ms: None,
                }),
                serde_json::json!({
                    "sessionUpdate": "_kage/turn",
                    "phase": "end",
                    "reason": name
                }),
            );
        }
    }

    #[test]
    fn notice_update_shapes() {
        for (tone, name) in [
            (NoticeTone::Info, "info"),
            (NoticeTone::Warn, "warn"),
            (NoticeTone::Error, "error"),
            (NoticeTone::Success, "success"),
        ] {
            roundtrip(
                &SessionUpdate::Notice(NoticeUpdate {
                    tone,
                    text: "heads up".into(),
                }),
                serde_json::json!({
                    "sessionUpdate": "_kage/notice",
                    "tone": name,
                    "text": "heads up"
                }),
            );
        }
    }

    #[test]
    fn compaction_update_shapes() {
        roundtrip(
            &SessionUpdate::Compaction(CompactionUpdate {
                phase: CompactionPhase::Complete,
                kept: Some(4),
                summarized: Some(12),
                before: Some(1200),
                after: Some(300),
                summary: Some("framed summary".into()),
            }),
            serde_json::json!({
                "sessionUpdate": "_kage/compaction",
                "phase": "complete",
                "kept": 4,
                "summarized": 12,
                "before": 1200,
                "after": 300,
                "summary": "framed summary"
            }),
        );
    }

    #[test]
    fn compaction_started_shapes() {
        roundtrip(
            &SessionUpdate::Compaction(CompactionUpdate {
                phase: CompactionPhase::Started,
                kept: Some(4),
                summarized: Some(12),
                before: None,
                after: None,
                summary: None,
            }),
            serde_json::json!({
                "sessionUpdate": "_kage/compaction",
                "phase": "started",
                "kept": 4,
                "summarized": 12
            }),
        );
    }

    #[test]
    fn compaction_aborted_shapes() {
        roundtrip(
            &SessionUpdate::Compaction(CompactionUpdate {
                phase: CompactionPhase::Aborted,
                kept: None,
                summarized: None,
                before: None,
                after: None,
                summary: None,
            }),
            serde_json::json!({
                "sessionUpdate": "_kage/compaction",
                "phase": "aborted"
            }),
        );
    }

    #[test]
    fn compaction_update_without_a_phase_reads_complete() {
        let note: SessionNotification = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "_kage/compaction",
                "kept": 4,
                "before": 1200,
                "after": 300
            }
        }))
        .unwrap();
        let SessionUpdate::Compaction(update) = note.update else {
            panic!("expected a compaction update");
        };
        assert_eq!(update.phase, CompactionPhase::Complete);
        assert_eq!(update.kept, Some(4));
        assert_eq!(update.before, Some(1200));
        assert_eq!(update.after, Some(300));
        assert_eq!(update.summarized, None);
        assert_eq!(update.summary, None);
    }

    #[test]
    fn unknown_session_update_parses_as_unknown() {
        let note: SessionNotification = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "update": {"sessionUpdate": "compaction_update", "phase": "started"}
        }))
        .unwrap();
        assert_eq!(note.update, SessionUpdate::Unknown);
    }

    #[test]
    fn request_permission_outcome_shapes() {
        roundtrip(
            &RequestPermissionResponse {
                outcome: PermissionOutcome::Selected(SelectedOption {
                    option_id: "allow".into(),
                }),
            },
            serde_json::json!({"outcome": {"outcome": "selected", "optionId": "allow"}}),
        );
        roundtrip(
            &RequestPermissionResponse {
                outcome: PermissionOutcome::Cancelled,
            },
            serde_json::json!({"outcome": {"outcome": "cancelled"}}),
        );
        roundtrip(
            &PermissionOption {
                option_id: "a".into(),
                name: "Allow".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
            serde_json::json!({"optionId": "a", "name": "Allow", "kind": "allow_once"}),
        );
    }

    /// A plan-mode review carries the plan document to the client under
    /// `_meta.kage.planReview`, and a revise answer brings the user's
    /// requested changes back through the same field.
    #[test]
    fn plan_review_shapes() {
        roundtrip(
            &RequestPermissionRequest {
                session_id: "s1".into(),
                tool_call: ToolCallUpdate {
                    tool_call_id: "t1".into(),
                    title: Some("exit_plan".into()),
                    ..ToolCallUpdate::default()
                },
                options: vec![
                    PermissionOption {
                        option_id: "approve".into(),
                        name: "Approve".into(),
                        kind: PermissionOptionKind::AllowOnce,
                    },
                    PermissionOption {
                        option_id: "revise".into(),
                        name: "Revise".into(),
                        kind: PermissionOptionKind::RejectOnce,
                    },
                    PermissionOption {
                        option_id: "reject".into(),
                        name: "Reject".into(),
                        kind: PermissionOptionKind::RejectOnce,
                    },
                ],
                meta: Some(RequestMeta {
                    kage: KageMeta {
                        plan_review: Some(PlanReview {
                            plan: Some("# Fix the build".into()),
                            revision: None,
                        }),
                        ..KageMeta::default()
                    },
                }),
            },
            serde_json::json!({
                "sessionId": "s1",
                "toolCall": {"toolCallId": "t1", "title": "exit_plan"},
                "options": [
                    {"optionId": "approve", "name": "Approve", "kind": "allow_once"},
                    {"optionId": "revise", "name": "Revise", "kind": "reject_once"},
                    {"optionId": "reject", "name": "Reject", "kind": "reject_once"}
                ],
                "_meta": {"kage": {"planReview": {"plan": "# Fix the build"}}}
            }),
        );
        roundtrip(
            &RequestPermissionResult {
                outcome: PermissionOutcome::Selected(SelectedOption {
                    option_id: "revise".into(),
                }),
                meta: Some(RequestMeta {
                    kage: KageMeta {
                        plan_review: Some(PlanReview {
                            plan: None,
                            revision: Some("also add tests".into()),
                        }),
                        ..KageMeta::default()
                    },
                }),
            },
            serde_json::json!({
                "outcome": {"outcome": "selected", "optionId": "revise"},
                "_meta": {"kage": {"planReview": {"revision": "also add tests"}}}
            }),
        );
        let plain: RequestPermissionResponse = serde_json::from_value(serde_json::json!({
            "outcome": {"outcome": "selected", "optionId": "approve"},
            "_meta": {"kage": {"planReview": {"revision": "x"}}}
        }))
        .unwrap();
        assert_eq!(
            plain.outcome,
            PermissionOutcome::Selected(SelectedOption {
                option_id: "approve".into()
            })
        );
    }
}
