//! Engine protocol: addressed events out, commands in.
//!
//! Every frontend observes the engine through [`Envelope`]s and drives it
//! through [`Command`]s. An envelope is serialized as one flat JSON object:
//! the `session` and `seq` fields sit next to the event's own `type` tag.
//!
//! Events are either durable (they describe state a client must keep, such
//! as [`LoopEvent::MessageAppended`]) or live (deltas and progress a client
//! may drop, such as [`LoopEvent::TextDelta`]). Each variant documents
//! which class it belongs to.

mod agent_tree;
mod mcp;

pub use agent_tree::{AgentNode, AgentState, AgentTree};
pub use mcp::{
    McpPrompt, McpPromptArgument, McpResource, McpResourceTemplate, McpServerInfo, McpServerStatus,
};

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::permissions::PermissionAction;
use crate::{
    Content, LoopError, LoopEvent, Message, Modalities, ThinkingLevel, TokenUsage, ToolCallId,
};

/// Stable identifier for one session.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Ulid);

impl SessionId {
    /// Generate a fresh session id.
    #[must_use]
    pub fn new() -> Self {
        Self(Ulid::generate())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Correlates a request the engine raised (permission, dialog) with the
/// command that answers it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub u64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One event on the engine bus, addressed to the session that produced it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Session that produced the event.
    pub session: SessionId,
    /// Per-session sequence number, starting at 1 and strictly increasing.
    pub seq: u64,
    /// The event itself.
    #[serde(flatten)]
    pub event: Event,
}

/// An event from either the agent loop or the engine around it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Event {
    /// Emitted by the agent loop.
    Loop(LoopEvent),
    /// Emitted by the engine.
    Host(HostEvent),
}

impl From<LoopEvent> for Event {
    fn from(event: LoopEvent) -> Self {
        Self::Loop(event)
    }
}

impl From<HostEvent> for Event {
    fn from(event: HostEvent) -> Self {
        Self::Host(event)
    }
}

/// Rewrite advertised tool names to the real names they point at.
///
/// A host that renames tools advertises only the new names, so the
/// model calls a tool the registry hosts under another name. Hosts
/// that render tool cards (the TUI transcript, the ACP bridge) apply
/// this to incoming envelopes so a renamed tool keeps its real card,
/// kind hint, and read-only grouping. Names the map does not know pass
/// through unchanged.
#[must_use]
pub fn with_canonical_tool_names(
    mut envelope: Envelope,
    canonical: &BTreeMap<String, String>,
) -> Envelope {
    let resolve = |name: &mut String| {
        if let Some(real) = canonical.get(name.as_str()) {
            *name = real.clone();
        }
    };
    match &mut envelope.event {
        Event::Loop(
            LoopEvent::ToolCallStart { name, .. } | LoopEvent::ToolCallArgsDelta { name, .. },
        ) => resolve(name),
        Event::Host(HostEvent::PermissionRequested { tool, .. }) => resolve(tool),
        _ => {}
    }
    envelope
}

/// Events the engine emits around agent runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostEvent {
    /// A run began. Durable.
    RunStarted,
    /// A run finished. Durable.
    RunEnded {
        /// How the run ended.
        outcome: RunOutcome,
    },
    /// The session's model, thinking level, permission mode, or working
    /// flag changed. Carries the full snapshot. Durable.
    StateChanged {
        /// Current session state.
        state: SessionState,
    },
    /// Token and cost totals changed. Durable.
    UsageUpdated {
        /// Current usage totals.
        usage: Usage,
    },
    /// A tool call needs an interactive decision. Answer with
    /// [`CommandKind::ResolvePermission`]. Durable.
    PermissionRequested {
        /// Id to answer with.
        request_id: RequestId,
        /// Tool call being gated, when the request comes from the loop.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<ToolCallId>,
        /// Tool name.
        tool: String,
        /// What the permission rules matched against: a command line or
        /// compact JSON input.
        subject: String,
        /// The full tool input.
        input: serde_json::Value,
    },
    /// A permission request was answered or abandoned. Durable.
    PermissionResolved {
        /// Id of the answered request.
        request_id: RequestId,
    },
    /// The model asks the user questions through
    /// [`ASK_USER_QUESTION_TOOL`], and its run waits for the answer.
    /// Durable.
    QuestionAsked {
        /// Id to answer with.
        request_id: RequestId,
        /// The call that asks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<ToolCallId>,
        /// What it asks, in order.
        questions: Vec<Question>,
    },
    /// A question was answered, declined or abandoned. Durable.
    QuestionClosed {
        /// Id of the closed request.
        request_id: RequestId,
    },
    /// A message for the user that is not part of the conversation.
    /// Never recorded. Live.
    Notice {
        /// Severity.
        level: NoticeLevel,
        /// Message text.
        text: String,
        /// `true` for a passing toast, `false` for a transcript line.
        transient: bool,
    },
    /// The session now points at a different file, for example after a
    /// resume, fork, or new session. Carries the full transcript so a
    /// client can rebuild its view. Durable.
    SessionChanged {
        /// Session file path.
        path: PathBuf,
        /// Stored title, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// The conversation as the user saw it, oldest first: every
        /// message and, where a compaction ran, its card with the
        /// counts and tokens the file recorded. The model's context is
        /// the compacted form of this and is deliberately not what a
        /// client renders.
        timeline: Vec<TimelineItem>,
        /// The messages compaction summarized out of the model's
        /// history that start agents or carry their results, oldest
        /// first, so a client lists the agents the session ran before
        /// it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        compacted_agents: Vec<Arc<Message>>,
    },
    /// The session title was set or generated. Durable.
    TitleChanged {
        /// New title.
        title: String,
    },
    /// A user shell command started or printed more. Carries the last
    /// lines of its output so far, empty at the start. Never recorded.
    /// Live.
    ShellOutput {
        /// Command line that runs.
        command: String,
        /// The last lines of stdout and stderr together.
        tail: String,
    },
    /// A user shell command finished or was cancelled. Its output joins
    /// the conversation as a user message the model sees on the next
    /// turn.
    ShellFinished {
        /// Command line that ran.
        command: String,
        /// Combined, truncated stdout and stderr.
        output: String,
        /// Exit code, or `None` when a signal ended the command.
        exit_code: Option<i32>,
    },
    /// Another session's `agent` call started this session. Published on
    /// the new session as its first envelope. Durable.
    AgentSpawned {
        /// Session whose `agent` call started this one.
        parent: SessionId,
        /// The parent's `agent` call.
        tool_call_id: ToolCallId,
        /// Name of the agent definition.
        agent: String,
        /// Short task description the model wrote for the user.
        description: String,
        /// Swarm batch membership, when a `swarm` call started this
        /// agent. `None` for a plain `agent` call.
        #[serde(default)]
        swarm: Option<SwarmMember>,
        /// The agent runs in the background: the `agent` call returned
        /// at once and the result reaches the parent as a message.
        #[serde(default)]
        background: bool,
    },
    /// A swarm child hit a provider rate limit and the engine requeued
    /// it: its result is still pending, so it is paused rather than
    /// finished, and its next run follows the backoff. Never recorded.
    /// Live.
    AgentPaused {
        /// Why the child paused, for the client to show.
        reason: String,
    },
    /// The session's MCP servers and what they offer. Published when a
    /// session opens, after a restart, and when a server's catalog
    /// changes. The latest snapshot wins. Never recorded. Live.
    McpServers {
        /// Every configured server, in registration order.
        servers: Vec<McpServerInfo>,
    },
    /// A [`CommandKind::WithdrawPrompt`] was answered: `content` is the
    /// prompt taken back out of the `delivery` queue, or `None` when
    /// that queue was empty. Never recorded. Live.
    PromptWithdrawn {
        /// Which queue the prompt came from.
        delivery: Delivery,
        /// The withdrawn prompt, or `None` when nothing was pending.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Vec<Content>>,
    },
    /// A [`CommandKind::Undo`] or [`CommandKind::Rewind`] was answered:
    /// the session branched in place in the same file. For Undo,
    /// `prompt` is the dropped prompt, for the client to put back into
    /// its editor; for Rewind the picked turn stays in the
    /// conversation and `prompt` is `None`. Never recorded. Live.
    Undone {
        /// The prompt to restore into the editor, or `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<Vec<Content>>,
    },
    /// A [`CommandKind::ListPrompts`] was answered: every user prompt
    /// the session's branch tree holds, oldest first, each one a
    /// target for [`CommandKind::Rewind`]. Never recorded. Live.
    PromptList {
        /// The prompts, depth-first over the branch tree.
        prompts: Vec<PastPrompt>,
    },
}

/// One past user prompt a client may rewind to, as reported by
/// [`HostEvent::PromptList`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PastPrompt {
    /// Entry id of the prompt's message, the `at` a
    /// [`CommandKind::Rewind`] names.
    pub at: String,
    /// The prompt, as it should go back into the editor.
    pub prompt: Vec<Content>,
    /// How many branch edges sit above the prompt: `0` for every
    /// prompt on the original thread, `1` for a branch re-prompted
    /// off it, and so on.
    pub depth: u32,
    /// Whether the prompt is the head, the one the conversation would
    /// continue from. Exactly one row carries it.
    pub active: bool,
}

/// One member of a [`HostEvent::AgentSpawned`] swarm batch: what its
/// `swarm` call asked it to do and where it sits in the batch, so a
/// client can show batch progress instead of one card per call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SwarmMember {
    /// Id of the batch, shared by every member of one call. `None` for
    /// a member restored from a transcript, which records no batch id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<ToolCallId>,
    /// The item this child was spawned for.
    pub item: String,
    /// Position of this child in the batch, 0-based.
    pub index: u32,
    /// How many children the batch has.
    pub total: u32,
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunOutcome {
    /// The model finished without error.
    Completed,
    /// The user or a client cancelled the run.
    Cancelled,
    /// The run stopped on an error.
    Failed {
        /// What went wrong.
        error: LoopError,
    },
}

/// Name of the tool the agent calls in plan mode to present its plan
/// for review. Hosts render its `plan` argument as the plan document.
pub const EXIT_PLAN_TOOL: &str = "exit_plan";

/// Name of the tool the model asks the user questions with.
pub const ASK_USER_QUESTION_TOOL: &str = "ask_user_question";

/// One question the model asks the user, with the choices it offers.
/// The user may also answer in their own words.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Question {
    /// A short label for the question, such as `Auth method`.
    pub header: String,
    /// The whole question.
    pub question: String,
    /// The choices, two to four.
    pub options: Vec<QuestionOption>,
    /// Whether the user may pick several choices.
    #[serde(default)]
    pub multi_select: bool,
}

/// One choice of a [`Question`].
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    /// What the user picks, in a few words.
    pub label: String,
    /// What picking it means.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// Snapshot of the settings that shape a session's next run.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    /// Provider-qualified model id, such as `anthropic:claude-sonnet-4-6`.
    pub model: String,
    /// Thinking level the user chose. `None` means automatic: high,
    /// or the nearest level the model accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingLevel>,
    /// Level the next run sends after fitting [`Self::thinking`] to the
    /// model. `None` when the run sends no thinking setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effective: Option<ThinkingLevel>,
    /// Levels the model accepts, lowest first. Empty when the model has
    /// no thinking setting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking_levels: Vec<ThinkingLevel>,
    /// Modalities the model accepts. Empty when unknown.
    #[serde(default, skip_serializing_if = "Modalities::is_empty")]
    pub input: Modalities,
    /// Session permission override. `None` means the configured rules
    /// decide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionAction>,
    /// `true` while a run is in flight.
    pub working: bool,
    /// Whether the session's swarm mode is on. The statusline shows a
    /// `swarm` segment while it is.
    #[serde(default)]
    pub swarm: bool,
    /// Goal the session works toward. When set, every completed turn is
    /// checked against it and the user is told when it is met. `None`
    /// means no goal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// Whether the session's plan mode is on: write tools are refused,
    /// command tools always ask, and the agent ends by presenting a
    /// plan for review. The statusline shows a `plan` segment while it
    /// is.
    #[serde(default)]
    pub plan: bool,
    /// Background shell commands still running.
    #[serde(default)]
    pub shells: u32,
}

/// Message counts of one history compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompactionCounts {
    /// Older messages the summary replaced.
    pub summarized: usize,
    /// Recent messages kept verbatim.
    pub kept: usize,
}

/// One compaction a session records, with everything its card shows:
/// the message counts and the context tokens before and after.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionRecord {
    /// Older turns the summary replaced.
    pub summarized: usize,
    /// Recent turns kept verbatim.
    pub kept: usize,
    /// Context tokens in use right before the compaction spliced the
    /// summary in. `None` for sessions recorded before the count was
    /// kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    /// Context fill of the first turn recorded after the compaction,
    /// when the file holds one; the other half of the card's token
    /// delta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    /// The framed summary text the model reads in the summarized
    /// turns' place.
    pub summary: String,
}

/// One entry of a session's visual timeline: the conversation as the
/// user saw it, with each compaction as a card at the point it ran.
/// Compaction shrinks the model's context, never this view; the turns
/// a compaction summarized stay in the timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineItem {
    /// A conversation message.
    Message(Arc<Message>),
    /// A compaction card: the turns before it were summarized into
    /// `summary`, which the model reads in their place.
    Compaction(CompactionRecord),
}

/// Running token and cost totals for a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Cumulative tokens across every turn.
    pub total: TokenUsage,
    /// Prompt size of the most recent turn, compared against
    /// `context_window` for the fill percentage.
    pub context_used: u64,
    /// Context window of the active model. `0` when unknown.
    pub context_window: u64,
    /// Cumulative cost in dollars. `0.0` when the model has no pricing.
    pub cost: f64,
    /// Whether `context_used` is a compaction's estimate rather than a
    /// turn's reported usage.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub context_estimated: bool,
}

/// Severity of a [`HostEvent::Notice`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    /// Informational.
    Info,
    /// Something the user may want to act on.
    Warning,
    /// Something failed.
    Error,
    /// Something the user wanted happened.
    Success,
}

/// Answer to a [`HostEvent::PermissionRequested`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Run this one call. The next identical call asks again.
    AllowOnce,
    /// Run this call and allow the tool for the rest of the session
    /// without persisting anything.
    AllowSession,
    /// Run this call, allow the tool for the rest of the session, and
    /// persist an allow rule for it.
    AllowAlways,
    /// Refuse the call.
    Deny,
}

/// How a prompt submitted during a run is delivered.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Deliver at the next turn boundary of the running run.
    #[default]
    Steer,
    /// Deliver as a new run once the current run ends.
    Queue,
}

/// A request for the engine to act.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Command {
    /// Target session. `None` addresses the engine's active session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// What to do.
    #[serde(flatten)]
    pub kind: CommandKind,
}

impl Command {
    /// Address `kind` to the engine's active session.
    #[must_use]
    pub fn active(kind: CommandKind) -> Self {
        Self {
            session: None,
            kind,
        }
    }

    /// Address `kind` to `session`.
    #[must_use]
    pub fn to(session: SessionId, kind: CommandKind) -> Self {
        Self {
            session: Some(session),
            kind,
        }
    }
}

/// What [`CommandKind::Image`] and [`CommandKind::Media`] do with the
/// media.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageAction {
    /// Show it in the system's viewer or player.
    Open,
    /// Write it into the working directory.
    Save,
}

/// Everything a client can ask the engine to do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandKind {
    /// Submit a user prompt. Starts a run when idle; otherwise delivered
    /// according to `delivery`.
    Prompt {
        /// Prompt content: text first, then images.
        content: Vec<Content>,
        /// Delivery while a run is in flight.
        #[serde(default)]
        delivery: Delivery,
    },
    /// Take the newest pending prompt for `delivery` back out of the
    /// queue, answering with [`HostEvent::PromptWithdrawn`]. A client
    /// recalls a prompt this way to edit it before it is delivered.
    WithdrawPrompt {
        /// Which queue to pop from.
        delivery: Delivery,
    },
    /// Cancel the in-flight run.
    Cancel,
    /// Answer a [`HostEvent::PermissionRequested`].
    ResolvePermission {
        /// Request being answered.
        request_id: RequestId,
        /// The decision.
        decision: PermissionDecision,
    },
    /// Answer a [`HostEvent::QuestionAsked`].
    AnswerQuestion {
        /// Request being answered.
        request_id: RequestId,
        /// Per question, in order, the labels picked or the user's own
        /// words. `None` declines to answer.
        answers: Option<Vec<Vec<String>>>,
    },
    /// Use a different provider-qualified model from the next run on.
    SetModel {
        /// Provider-qualified model id.
        model: String,
    },
    /// Use a different thinking level from the next run on.
    SetThinking {
        /// New level. `None` returns to the automatic default.
        #[serde(default)]
        level: Option<ThinkingLevel>,
    },
    /// Override the configured permission rules for this session. `None`
    /// restores the configured rules.
    SetPermissionMode {
        /// Override to apply.
        #[serde(default)]
        mode: Option<PermissionAction>,
    },
    /// Compact the conversation now.
    Compact,
    /// Name the session. The name replaces any generated title, and no
    /// title is generated after it.
    SetTitle {
        /// The new title.
        title: String,
    },
    /// Run a shell command in the session directory and share its output
    /// with the model on the next turn.
    Shell {
        /// Command line passed to `sh -c`.
        command: String,
    },
    /// Start a fresh, empty session.
    NewSession,
    /// Resume the session stored at `path`.
    LoadSession {
        /// Session file.
        path: PathBuf,
    },
    /// Copy the active session up to an entry into a new session file.
    Fork {
        /// Entry id prefix to stop at. `None` means the latest entry.
        #[serde(default)]
        at: Option<String>,
        /// Continue on the copy instead of leaving it as a snapshot.
        #[serde(default)]
        switch: bool,
    },
    /// Drop the last user prompt and everything after it: branch the
    /// session in place, continuing from the entry before that
    /// prompt, and answer with [`HostEvent::Undone`]. Refused while a
    /// run is in flight.
    Undo,
    /// List the user prompts a rewind can target, oldest first over
    /// the branch tree, answering with [`HostEvent::PromptList`].
    /// Read-only.
    ListPrompts,
    /// Branch the session in place to the end of the turn of the user
    /// prompt named by an entry-id prefix from
    /// [`HostEvent::PromptList`]: the prompt and its reply stay and
    /// what follows the turn is abandoned, answering with
    /// [`HostEvent::Undone`]. Any prompt on any branch is a target.
    /// Refused while a run is in flight.
    Rewind {
        /// Entry id of the user prompt to drop.
        at: String,
    },
    /// Fork the session stored at `path` at its latest entry.
    ForkFile {
        /// Session file.
        path: PathBuf,
    },
    /// Duplicate the active session and continue on the copy.
    Clone,
    /// Delete the session stored at `path`. Refused for the active session.
    DeleteSession {
        /// Session file.
        path: PathBuf,
    },
    /// Render the active session as Markdown.
    Export {
        /// Destination. `None` writes next to the working directory.
        #[serde(default)]
        path: Option<PathBuf>,
    },
    /// Open or save an image the model produced in the session: the one
    /// whose bytes hash to `sha256`, else the newest.
    Image {
        /// What to do with it.
        action: ImageAction,
        /// The sha256 of the image's bytes, lowercase hex.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
        /// Where `save` writes. `None` picks a new file under
        /// `generated/` in the working directory. An existing file is
        /// never replaced.
        #[serde(default)]
        path: Option<PathBuf>,
    },
    /// Open or save media the model or a tool produced in the session,
    /// an image or an audio clip: the one whose bytes hash to
    /// `sha256`, else the newest.
    Media {
        /// What to do with it.
        action: ImageAction,
        /// The sha256 of the media's bytes, lowercase hex.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
        /// Where `save` writes. `None` picks a new file under
        /// `generated/` in the working directory. An existing file is
        /// never replaced.
        #[serde(default)]
        path: Option<PathBuf>,
    },
    /// Restart one MCP server of the session: now when idle, else at the
    /// start of the next run, since in-flight calls hold the old
    /// connection.
    RestartMcp {
        /// Configured server name.
        server: String,
    },
    /// Turn the session's swarm mode on or off. Turning it on injects
    /// the swarm workflow block into the context once; turning it off
    /// injects a short exit note. A set that changes nothing injects
    /// nothing.
    SwarmMode {
        /// Whether the session delegates repeated work through `swarm`.
        on: bool,
    },
    /// Set the goal the session works toward. Once set, every
    /// completed turn is checked against it and the user is told when
    /// it is met. `None` clears the goal and stops the checks.
    SetGoal {
        /// What done looks like. `None` clears the goal.
        #[serde(default)]
        goal: Option<String>,
    },
    /// Turn the session's plan mode on or off. Like
    /// [`CommandKind::SwarmMode`], a change injects a short note into
    /// the context once and a set that changes nothing injects nothing.
    PlanMode {
        /// Whether the session plans before it changes anything.
        on: bool,
    },
    /// Drop an idle session and its idle agent descendants from the
    /// engine. A session with a run or shell in flight is kept and
    /// warned instead. Engine-internal: hosts close sessions through
    /// their own protocol, so this never crosses the ACP wire.
    Close,
    /// Cancel every run and stop the engine.
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MessageId, Role};

    fn roundtrip(envelope: &Envelope) -> serde_json::Value {
        let value = serde_json::to_value(envelope).unwrap();
        let back: Envelope = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(&back, envelope);
        value
    }

    fn envelope(event: impl Into<Event>) -> Envelope {
        Envelope {
            session: SessionId::new(),
            seq: 7,
            event: event.into(),
        }
    }

    #[test]
    fn envelope_is_one_flat_object() {
        let env = envelope(LoopEvent::TextDelta {
            id: MessageId::new(),
            delta: "hi".into(),
        });
        let value = roundtrip(&env);
        assert_eq!(value["seq"], 7);
        assert_eq!(value["type"], "text_delta");
        assert_eq!(value["delta"], "hi");
        assert_eq!(value["session"], env.session.to_string());
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one linear pass asserting every event's roundtrip"
    )]
    fn events_roundtrip_and_tags_stay_disjoint() {
        let message = Message::new(Role::User, vec![Content::Text { text: "q".into() }], None);
        let events: Vec<Event> = vec![
            LoopEvent::TextDelta {
                id: MessageId::new(),
                delta: "d".into(),
            }
            .into(),
            LoopEvent::Error {
                kind: LoopError::Cancelled,
            }
            .into(),
            LoopEvent::MessageAppended {
                message: Arc::new(message.clone()),
            }
            .into(),
            LoopEvent::TurnStarted { index: 0 }.into(),
            LoopEvent::TurnEnded {
                index: 0,
                had_tool_calls: true,
            }
            .into(),
            HostEvent::RunStarted.into(),
            HostEvent::RunEnded {
                outcome: RunOutcome::Failed {
                    error: LoopError::Cancelled,
                },
            }
            .into(),
            HostEvent::StateChanged {
                state: SessionState {
                    model: "mock:m".into(),
                    thinking: Some(ThinkingLevel::High),
                    permission_mode: Some(PermissionAction::Ask),
                    working: true,
                    ..SessionState::default()
                },
            }
            .into(),
            HostEvent::UsageUpdated {
                usage: Usage::default(),
            }
            .into(),
            HostEvent::PermissionRequested {
                request_id: RequestId(3),
                tool_call_id: Some(ToolCallId("call_1".into())),
                tool: "shell".into(),
                subject: "ls".into(),
                input: serde_json::json!({ "command": "ls" }),
            }
            .into(),
            HostEvent::PermissionResolved {
                request_id: RequestId(3),
            }
            .into(),
            HostEvent::Notice {
                level: NoticeLevel::Error,
                text: "boom".into(),
                transient: false,
            }
            .into(),
            HostEvent::SessionChanged {
                path: PathBuf::from("/tmp/s.jsonl"),
                title: Some("t".into()),
                timeline: vec![TimelineItem::Message(Arc::new(message.clone()))],
                compacted_agents: Vec::new(),
            }
            .into(),
            HostEvent::TitleChanged { title: "t".into() }.into(),
            HostEvent::ShellFinished {
                command: "ls".into(),
                output: "a".into(),
                exit_code: Some(0),
            }
            .into(),
            HostEvent::AgentSpawned {
                parent: SessionId::new(),
                tool_call_id: ToolCallId("call_2".into()),
                agent: "explore".into(),
                description: "map the exports".into(),
                swarm: None,
                background: true,
            }
            .into(),
            HostEvent::McpServers {
                servers: vec![McpServerInfo {
                    name: "everything".into(),
                    status: McpServerStatus::Connected,
                    tools: 1,
                    resources: Vec::new(),
                    templates: Vec::new(),
                    prompts: Vec::new(),
                }],
            }
            .into(),
        ];
        for event in events {
            let value = roundtrip(&envelope(event.clone()));
            match event {
                Event::Loop(_) => assert!(serde_json::from_value::<HostEvent>(value).is_err()),
                Event::Host(_) => assert!(serde_json::from_value::<LoopEvent>(value).is_err()),
            }
        }
    }

    /// A message carrying every content block, an inline and a remote
    /// image among them.
    fn rich_message() -> Message {
        Message::new(
            Role::Assistant,
            vec![
                Content::Text { text: "q".into() },
                Content::Thinking {
                    text: "hm".into(),
                    signature: Some(crate::ThinkingSignature {
                        model: "anthropic/claude".into(),
                        data: "sig".into(),
                        redacted: false,
                    }),
                    duration_ms: Some(1_250),
                },
                Content::Image {
                    source: crate::MediaSource::Base64 {
                        data: "iVBORw0KGgo=".into(),
                    },
                    mime: "image/png".into(),
                },
                Content::Image {
                    source: crate::MediaSource::Url {
                        url: "https://example.com/a.png".into(),
                    },
                    mime: "image/png".into(),
                },
                Content::ToolCall {
                    id: ToolCallId::new("call_1"),
                    name: "read".into(),
                    input: serde_json::json!({ "path": "a", "offset": 3, "ratio": 0.5 }),
                },
                Content::ToolResultBlock {
                    call_id: ToolCallId::new("call_1"),
                    output: "ok".into(),
                    is_error: false,
                },
                Content::Custom {
                    kind: "plugin:tps".into(),
                    data: serde_json::json!({ "tps": 41.5, "n": u64::MAX }),
                },
            ],
            Some(MessageId::new()),
        )
    }

    fn loop_events() -> Vec<LoopEvent> {
        let id = MessageId::new();
        let call = ToolCallId::new("call_1");
        vec![
            LoopEvent::MessageStart { id },
            LoopEvent::TextDelta {
                id,
                delta: "d".into(),
            },
            LoopEvent::ThinkingDelta {
                id,
                delta: "t".into(),
            },
            LoopEvent::ToolCallStart {
                id: call.clone(),
                name: "shell".into(),
                input_partial: serde_json::json!({ "command": "ls", "timeout": 30 }),
            },
            LoopEvent::ToolCallArgsDelta {
                id: call.clone(),
                name: "shell".into(),
                input_partial: serde_json::json!({}),
            },
            LoopEvent::ToolExecutionStart { id: call.clone() },
            LoopEvent::ToolUpdate {
                id: call.clone(),
                update: crate::ToolUpdate {
                    content: "1/2".into(),
                    structured: Some(serde_json::json!({ "done": 1 })),
                },
            },
            LoopEvent::ToolCallEnd {
                id: call,
                output: crate::ToolOutput {
                    is_error: false,
                    text: "a".into(),
                    structured: Some(serde_json::json!({ "exit_code": 0 })),
                    terminate: true,
                    media: Vec::new(),
                },
            },
            LoopEvent::MessageEnd {
                id,
                usage: TokenUsage {
                    input: 12,
                    output: 34,
                    cache_read: u64::MAX,
                    cache_write: 0,
                },
                stop_reason: crate::StopReason::MaxTokens,
            },
            LoopEvent::MessageAppended {
                message: Arc::new(rich_message()),
            },
            LoopEvent::TurnStarted { index: 1 },
            LoopEvent::TurnEnded {
                index: 1,
                had_tool_calls: false,
            },
            LoopEvent::Compaction {
                kept: 2,
                summarized: 9,
                summary: "s".into(),
                before: 150_000,
                estimate: 40_000,
            },
            LoopEvent::ProviderRetry {
                attempt: 2,
                max_attempts: 4,
                wait_secs: 8,
                wait_ms: 8000,
                requested_secs: Some(300),
                error: "busy".into(),
            },
            LoopEvent::Error {
                kind: LoopError::RateLimited {
                    message: "slow down".into(),
                    retry_after_secs: Some(60),
                },
            },
        ]
    }

    /// The host events about runs, state and sessions.
    fn session_host_events() -> Vec<HostEvent> {
        vec![
            HostEvent::RunStarted,
            HostEvent::RunEnded {
                outcome: RunOutcome::Completed,
            },
            HostEvent::RunEnded {
                outcome: RunOutcome::Failed {
                    error: LoopError::Tool {
                        name: "shell".into(),
                        message: "boom".into(),
                    },
                },
            },
            HostEvent::StateChanged {
                state: SessionState {
                    model: "anthropic/claude-sonnet-4-6".into(),
                    thinking: None,
                    thinking_effective: Some(ThinkingLevel::Medium),
                    thinking_levels: vec![ThinkingLevel::Low, ThinkingLevel::High],
                    input: Modalities::of(&[crate::Modality::Text, crate::Modality::Image]),
                    permission_mode: Some(PermissionAction::Deny),
                    working: true,
                    swarm: true,
                    goal: Some("ship it".into()),
                    plan: true,
                    shells: 2,
                },
            },
            HostEvent::UsageUpdated {
                usage: Usage {
                    total: TokenUsage {
                        input: 1_000_000,
                        output: 2,
                        cache_read: 3,
                        cache_write: 4,
                    },
                    context_used: 900,
                    context_window: 200_000,
                    cost: 0.123_456_789,
                    context_estimated: true,
                },
            },
            HostEvent::SessionChanged {
                path: PathBuf::from("/tmp/s.jsonl"),
                title: None,
                timeline: vec![
                    TimelineItem::Message(Arc::new(rich_message())),
                    TimelineItem::Compaction(CompactionRecord {
                        summarized: 10,
                        kept: 3,
                        before: Some(160_000),
                        after: Some(42_000),
                        summary: "the handoff".into(),
                    }),
                ],
                compacted_agents: vec![Arc::new(rich_message())],
            },
            HostEvent::TitleChanged { title: "t".into() },
        ]
    }

    /// The host events about asks, notices, shells, agents and MCP.
    fn other_host_events() -> Vec<HostEvent> {
        vec![
            HostEvent::PermissionRequested {
                request_id: RequestId(u64::MAX),
                tool_call_id: None,
                tool: "write".into(),
                subject: "{}".into(),
                input: serde_json::json!({ "path": "a", "bytes": 12 }),
            },
            HostEvent::PermissionResolved {
                request_id: RequestId(1),
            },
            HostEvent::Notice {
                level: NoticeLevel::Success,
                text: "done".into(),
                transient: true,
            },
            HostEvent::ShellOutput {
                command: "ls".into(),
                tail: "a\nb".into(),
            },
            HostEvent::ShellFinished {
                command: "false".into(),
                output: String::new(),
                exit_code: None,
            },
            HostEvent::AgentSpawned {
                parent: SessionId::new(),
                tool_call_id: ToolCallId::new("call_2"),
                agent: "explore".into(),
                description: "map".into(),
                swarm: Some(SwarmMember {
                    batch: Some(ToolCallId::new("call_2")),
                    item: "src/a.rs".into(),
                    index: 1,
                    total: 3,
                }),
                background: false,
            },
            HostEvent::AgentPaused {
                reason: "rate limited".into(),
            },
            HostEvent::McpServers {
                servers: vec![McpServerInfo {
                    name: "srv".into(),
                    status: McpServerStatus::Failed {
                        error: "gone".into(),
                    },
                    tools: 0,
                    resources: Vec::new(),
                    templates: Vec::new(),
                    prompts: Vec::new(),
                }],
            },
            HostEvent::PromptWithdrawn {
                delivery: Delivery::Queue,
                content: Some(rich_message().content),
            },
            HostEvent::Undone {
                prompt: Some(rich_message().content),
            },
        ]
    }

    #[test]
    fn every_event_roundtrips_inside_an_envelope() {
        let events = loop_events()
            .into_iter()
            .map(Event::from)
            .chain(session_host_events().into_iter().map(Event::from))
            .chain(other_host_events().into_iter().map(Event::from));
        for event in events {
            let env = envelope(event);
            let line = serde_json::to_string(&env).unwrap();
            let back: Envelope = serde_json::from_str(&line).unwrap();
            assert_eq!(back, env, "{line}");
        }
    }

    #[test]
    fn an_agent_spawned_envelope_from_before_background_reads() {
        let event = HostEvent::AgentSpawned {
            parent: SessionId::new(),
            tool_call_id: ToolCallId("c".into()),
            agent: "general".into(),
            description: "d".into(),
            swarm: None,
            background: true,
        };
        let mut value = serde_json::to_value(&event).unwrap();
        value.as_object_mut().unwrap().remove("background");
        let read: HostEvent = serde_json::from_value(value).unwrap();
        assert!(matches!(
            read,
            HostEvent::AgentSpawned {
                background: false,
                ..
            }
        ));
    }

    #[test]
    fn every_command_roundtrips() {
        let kinds = vec![
            CommandKind::Prompt {
                content: rich_message().content,
                delivery: Delivery::Steer,
            },
            CommandKind::WithdrawPrompt {
                delivery: Delivery::Queue,
            },
            CommandKind::Cancel,
            CommandKind::ResolvePermission {
                request_id: RequestId(9),
                decision: PermissionDecision::AllowSession,
            },
            CommandKind::SetModel {
                model: "openai/gpt-5".into(),
            },
            CommandKind::SetThinking {
                level: Some(ThinkingLevel::Off),
            },
            CommandKind::SetPermissionMode { mode: None },
            CommandKind::Compact,
            CommandKind::SetTitle { title: "t".into() },
            CommandKind::Shell {
                command: "ls".into(),
            },
            CommandKind::NewSession,
            CommandKind::LoadSession {
                path: PathBuf::from("/tmp/s.jsonl"),
            },
            CommandKind::Fork {
                at: Some("01J".into()),
                switch: true,
            },
            CommandKind::ForkFile {
                path: PathBuf::from("/tmp/s.jsonl"),
            },
            CommandKind::Clone,
            CommandKind::DeleteSession {
                path: PathBuf::from("/tmp/s.jsonl"),
            },
            CommandKind::Export { path: None },
            CommandKind::Image {
                action: ImageAction::Save,
                sha256: Some("ab".repeat(32)),
                path: Some(PathBuf::from("art/cat.png")),
            },
            CommandKind::Media {
                action: ImageAction::Open,
                sha256: Some("cd".repeat(32)),
                path: None,
            },
            CommandKind::RestartMcp {
                server: "srv".into(),
            },
            CommandKind::SwarmMode { on: true },
            CommandKind::SetGoal {
                goal: Some("g".into()),
            },
            CommandKind::PlanMode { on: false },
            CommandKind::Close,
            CommandKind::Shutdown,
        ];
        for kind in kinds {
            for command in [
                Command::active(kind.clone()),
                Command::to(SessionId::new(), kind.clone()),
            ] {
                let line = serde_json::to_string(&command).unwrap();
                let back: Command = serde_json::from_str(&line).unwrap();
                assert_eq!(back, command, "{line}");
            }
        }
    }

    #[test]
    fn agent_spawned_has_its_own_tag() {
        let parent = SessionId::new();
        let value = roundtrip(&envelope(HostEvent::AgentSpawned {
            parent,
            tool_call_id: ToolCallId("call_1".into()),
            agent: "explore".into(),
            description: "map the exports".into(),
            swarm: None,
            background: false,
        }));
        assert_eq!(value["type"], "agent_spawned");
        assert_eq!(value["parent"], parent.to_string());
        assert_eq!(value["tool_call_id"], "call_1");
    }

    #[test]
    fn shell_output_has_its_own_tag() {
        let value = roundtrip(&envelope(HostEvent::ShellOutput {
            command: "ls".into(),
            tail: "a".into(),
        }));
        assert_eq!(value["type"], "shell_output");
        assert!(serde_json::from_value::<LoopEvent>(value).is_err());
    }

    #[test]
    fn restart_mcp_roundtrips() {
        let cmd = Command::active(CommandKind::RestartMcp {
            server: "everything".into(),
        });
        let value = serde_json::to_value(&cmd).unwrap();
        assert_eq!(value["type"], "restart_mcp");
        assert_eq!(value["server"], "everything");
        let back: Command = serde_json::from_value(value).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn close_roundtrips() {
        let cmd = Command::to(SessionId::new(), CommandKind::Close);
        let value = serde_json::to_value(&cmd).unwrap();
        assert_eq!(value["type"], "close");
        assert_eq!(value["session"], cmd.session.unwrap().to_string());
        let back: Command = serde_json::from_value(value).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn command_is_one_flat_object() {
        let cmd = Command::active(CommandKind::Prompt {
            content: vec![Content::Text { text: "go".into() }],
            delivery: Delivery::Queue,
        });
        let value = serde_json::to_value(&cmd).unwrap();
        assert_eq!(value["type"], "prompt");
        assert_eq!(value["delivery"], "queue");
        assert!(value.get("session").is_none());
        let back: Command = serde_json::from_value(value).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn canonical_names_resolve_renamed_tool_calls() {
        let canonical = BTreeMap::from([("run_command".to_owned(), "shell".to_owned())]);
        let env = with_canonical_tool_names(
            envelope(LoopEvent::ToolCallStart {
                id: ToolCallId::new("call-1"),
                name: "run_command".into(),
                input_partial: serde_json::json!({ "command": "ls" }),
            }),
            &canonical,
        );
        match env.event {
            Event::Loop(LoopEvent::ToolCallStart { name, .. }) => assert_eq!(name, "shell"),
            other => panic!("expected a tool call start, got {other:?}"),
        }
    }

    #[test]
    fn canonical_names_resolve_permission_requests() {
        let canonical = BTreeMap::from([("run_command".to_owned(), "shell".to_owned())]);
        let env = with_canonical_tool_names(
            envelope(HostEvent::PermissionRequested {
                request_id: RequestId(1),
                tool_call_id: None,
                tool: "run_command".into(),
                subject: "ls".into(),
                input: serde_json::json!({ "command": "ls" }),
            }),
            &canonical,
        );
        match env.event {
            Event::Host(HostEvent::PermissionRequested { tool, .. }) => {
                assert_eq!(tool, "shell");
            }
            other => panic!("expected a permission request, got {other:?}"),
        }
    }

    #[test]
    fn canonical_names_leave_unknown_names_alone() {
        let canonical = BTreeMap::from([("run_command".to_owned(), "shell".to_owned())]);
        let env = with_canonical_tool_names(
            envelope(LoopEvent::ToolCallStart {
                id: ToolCallId::new("call-1"),
                name: "grep".into(),
                input_partial: serde_json::json!({}),
            }),
            &canonical,
        );
        match env.event {
            Event::Loop(LoopEvent::ToolCallStart { name, .. }) => assert_eq!(name, "grep"),
            other => panic!("expected a tool call start, got {other:?}"),
        }
    }
}
