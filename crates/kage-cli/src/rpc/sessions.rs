//! Listing, loading and resuming recorded sessions.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kage_acp::acp::{
    CompactionPhase, CompactionUpdate, ContentBlock, ListSessionsRequest, ListSessionsResponse,
    McpServer, PromptRef, SessionConfigOption, SessionExportResponse, SessionForkRequest,
    SessionForkResponse, SessionInfo, SessionInfoKage, SessionInfoMeta, SessionInfoUpdate,
    SessionUpdate, SubagentSessionCapabilities, SubagentState, SubagentSwarm, SubagentUpdate,
    ToolCallStatus, ToolCallUpdate, TurnPhase, TurnReason, TurnUpdate,
};
use kage_acp::agent::{PromptContext, send_update};
use kage_core::protocol::{AgentNode, AgentState, AgentTree, SwarmMember, TimelineItem};
use kage_core::sync::lock;
use kage_core::{
    Content, LoopEvent, MediaSource, Message, MessageId, Modality, Role, SessionId, ToolCallId,
    ToolOutput,
};
use kage_jsonrpc::RpcError;
use kage_loop::TokenBudget;
use kage_session::{
    EntryId, ReplayResult, SessionEntry, SessionError, SessionReader, SessionWriter,
};

use super::live::Live;

use super::bridge::to_update;
use super::bridge::user_chunk;
use super::content::image_block;
use super::mcp::editor_servers;
use super::options::config_options;
use crate::engine::{Recorder, render_session_markdown};

/// The memoized [`leading_marker`] verdicts, keyed by file and the
/// (mtime, size) pair.
type MarkerCache = Mutex<
    HashMap<
        PathBuf,
        (
            Option<std::time::SystemTime>,
            u64,
            Option<serde_json::Value>,
        ),
    >,
>;

/// Sessions per `session/list` page.
const LIST_PAGE: usize = 50;

impl super::CliAcpAgent {
    /// Opens the recorded session `client_id` names the way the TUI resumes
    /// one: on its recorded model when that resolves, with its thinking
    /// level and token totals, and with the client's MCP `servers`. With
    /// `ctx`, streams the turn in flight, the title, the live subagents
    /// and the open asks of a session the host already runs, and with
    /// `replay_file` also its recorded history. Returns the session's
    /// config options.
    pub(super) fn open_recorded(
        &self,
        client_id: &str,
        cwd: &str,
        servers: &[McpServer],
        ctx: Option<&PromptContext>,
        replay_file: bool,
    ) -> Result<Vec<SessionConfigOption>, RpcError> {
        let servers = editor_servers(servers)?;
        let path = self.recorded_path(client_id)?;
        if kage_session::is_agent_session(&path) {
            return self.open_agent(client_id, &path, ctx, replay_file);
        }
        let id = crate::engine::session_id_of(&path).ok_or_else(|| {
            RpcError::internal(format!("bad session file name {}", path.display()))
        })?;
        if lock(&self.ids).by_engine.contains_key(&id) {
            if replay_file && let Some(ctx) = ctx {
                let replay =
                    kage_session::replay(&path).map_err(|e| RpcError::internal(e.to_string()))?;
                for update in replay_updates(&replay.timeline) {
                    ctx.update(update);
                }
                self.announce_restored(id, &replay, ctx);
                if let Some(title) = replay.title {
                    ctx.update(SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                        title: Some(title),
                        updated_at: None,
                    }));
                }
            }
            lock(&self.ids).insert(client_id.to_owned(), id);
            let shown = lock(&self.shown);
            return Ok(shown
                .get(&id)
                .map(|shown| config_options(&self.host.models(), &shown.settings))
                .unwrap_or_default());
        }
        if let Some(settings) = self.host.open_settings(id) {
            return self.attach_live(id, client_id, &path, &settings, replay_file, ctx);
        }
        let replay = kage_session::replay(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        if replay_file && let Some(ctx) = ctx {
            // Nothing runs this session, so a call the file never closed
            // ended with the engine that made it.
            for update in replay_updates(&replay.timeline)
                .into_iter()
                .chain(interrupted_calls(&replay.history))
            {
                ctx.update(update);
            }
            self.announce_restored(id, &replay, ctx);
            for update in orphaned_agents(&self.host.sessions, id, &replay.history) {
                ctx.update(SessionUpdate::SubagentUpdate(update));
            }
            if let Some(title) = replay.title {
                ctx.update(SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                    title: Some(title),
                    updated_at: None,
                }));
            }
        }
        let writer = Self::open_hosted(&path)?;
        let model = if self.host.registry().resolve(&replay.model).is_ok() {
            replay.model
        } else {
            eprintln!(
                "kage: rpc: session model {} unavailable; using {} instead",
                replay.model, self.host.default_model
            );
            self.host.default_model.clone()
        };
        let mut spec = self.session_spec(id, cwd, &model, servers)?;
        spec.cx.history = replay.history.into_iter().map(Arc::new).collect();
        spec.cx.budget = TokenBudget {
            used_input: replay.usage_total.input,
            used_output: replay.usage_total.output,
            used_cache_read: replay.usage_total.cache_read,
            used_cache_write: replay.usage_total.cache_write,
            current_context: replay.usage_total.last_context,
        };
        spec.cx.thinking_level = replay
            .thinking_level
            .as_deref()
            .and_then(kage_core::ThinkingLevel::parse);
        spec.recorder = Some(Recorder::new(writer, spec.plugins.clone()));
        Ok(self.open(client_id.to_owned(), spec))
    }

    /// Opens a recorded session for hosting, waiting out the window
    /// between the engine taking the close command and dropping the
    /// writer: a load that lands there finds the file still locked by
    /// this very process. A session locked by a real second kage
    /// stays locked and the error surfaces after the wait.
    fn open_hosted(path: &Path) -> Result<SessionWriter, RpcError> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match SessionWriter::open(path) {
                Ok(writer) => return Ok(writer),
                Err(SessionError::Locked { .. }) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(err) => return Err(RpcError::internal(err.to_string())),
            }
        }
    }

    /// Shows the recorded agent session at `path` without hosting it:
    /// an agent belongs to the call that started it, so a client reads
    /// its transcript but never prompts it. With `replay_file`, streams
    /// its history and title.
    fn open_agent(
        &self,
        client_id: &str,
        path: &Path,
        ctx: Option<&PromptContext>,
        replay_file: bool,
    ) -> Result<Vec<SessionConfigOption>, RpcError> {
        if replay_file && let Some(ctx) = ctx {
            let replay =
                kage_session::replay(path).map_err(|e| RpcError::internal(e.to_string()))?;
            for update in replay_updates(&replay.timeline) {
                ctx.update(update);
            }
            self.announce_restored(replay.header.session, &replay, ctx);
            if let Some(title) = replay.title {
                ctx.update(SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                    title: Some(title),
                    updated_at: None,
                }));
            }
        }
        lock(&self.ids).read_only.insert(client_id.to_owned());
        Ok(Vec::new())
    }

    /// Announces the finished agents the recorded session `root`
    /// started, at every depth: `root`'s own through `ctx`, deeper ones
    /// on the session of the agent that started them. Agents the live
    /// tree still tracks are left to the live attach, which knows their
    /// true state.
    pub(super) fn announce_restored(
        &self,
        root: SessionId,
        replay: &ReplayResult,
        ctx: &PromptContext,
    ) {
        let live = lock(&self.host.live);
        for (parent, update) in restored_subagents(&self.host.sessions, root, replay, &live) {
            let update = SessionUpdate::SubagentUpdate(update);
            if parent == root {
                ctx.update(update);
            } else {
                send_update(&self.peer, &parent.to_string(), update);
            }
        }
    }

    /// The file of the recorded session `client_id` names.
    fn recorded_path(&self, client_id: &str) -> Result<PathBuf, RpcError> {
        kage_session::find_by_prefix(&self.host.sessions, client_id)
            .map_err(|e| RpcError::internal(e.to_string()))?
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {client_id}")))
    }

    /// Copies the recorded session `req.session_id` names into a new
    /// session, whole or up to the prompt `req.before` names.
    pub(super) fn fork_recorded(
        &self,
        req: &SessionForkRequest,
    ) -> Result<SessionForkResponse, RpcError> {
        let path = self.recorded_path(&req.session_id)?;
        let at = fork_point(&path, req.before.as_ref())?;
        let id = SessionId::new();
        let dst = self.host.sessions.join(format!("{id}.jsonl"));
        let truncated = kage_session::fork(&path, &dst, id, at)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(SessionForkResponse {
            session_id: id.to_string(),
            truncated,
        })
    }

    /// The recorded session `client_id` names as a Markdown transcript.
    pub(super) fn export_recorded(
        &self,
        client_id: &str,
    ) -> Result<SessionExportResponse, RpcError> {
        let path = self.recorded_path(client_id)?;
        let replay = kage_session::replay(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(SessionExportResponse {
            markdown: render_session_markdown(&replay),
        })
    }
}

/// The entry a fork of `path` copies through: the one just before the
/// prompt `before` names, or the last entry when `before` is absent.
fn fork_point(path: &Path, before: Option<&PromptRef>) -> Result<EntryId, RpcError> {
    let internal = |e: kage_session::SessionError| RpcError::internal(e.to_string());
    let mut last = None;
    let mut seen = 0;
    for entry in SessionReader::iter(path).map_err(internal)? {
        let entry = entry.map_err(internal)?;
        if let (Some(before), SessionEntry::Message(m)) = (before, &entry)
            && m.message.role == Role::User
            && prompt_text(&m.message) == before.text.as_deref()
        {
            if seen == before.occurrence {
                return last.ok_or_else(|| RpcError::internal("the prompt has no entry before it"));
            }
            seen += 1;
        }
        last = Some(entry.id());
    }
    match before {
        Some(_) => Err(RpcError::new(-32602, "the session has no such prompt")),
        None => last.ok_or_else(|| RpcError::internal("the session has no entries")),
    }
}

/// The first text block of a user message: the prompt as typed, ahead
/// of any attachment rendered as text. `None` when the prompt has no
/// text block, such as an image-only message.
fn prompt_text(message: &Message) -> Option<&str> {
    message.content.iter().find_map(|block| match block {
        Content::Text { text } => Some(text.as_str()),
        _ => None,
    })
}

/// The subagent updates that rebuild the finished agents of the
/// recorded session `root`: the ones its `replay` started, compacted
/// away or not, then each agent's own from that agent's file in `dir`,
/// parents first. Each comes with the session whose call started it.
/// The records come from the `<agent>` and `<swarm>` results the
/// engine wrote, so they survive a restart.
pub(super) fn restored_subagents(
    dir: &Path,
    root: SessionId,
    replay: &ReplayResult,
    live: &Live,
) -> Vec<(SessionId, SubagentUpdate)> {
    let mut tree = AgentTree::default();
    tree.restore(root, replay.agent_messages());
    let mut pending: Vec<SessionId> = tree
        .under(root)
        .into_iter()
        .map(|(_, node)| node.session)
        .collect();
    let mut seen = HashSet::new();
    while let Some(session) = pending.pop() {
        if !seen.insert(session) {
            continue;
        }
        let Ok(replay) = kage_session::replay(&crate::build_session_path(dir, session)) else {
            continue;
        };
        tree.restore(session, replay.agent_messages());
        pending.extend(
            tree.under(session)
                .into_iter()
                .map(|(_, node)| node.session),
        );
    }
    tree.under(root)
        .into_iter()
        .filter(|(_, node)| !live.tracks(node.session))
        .map(|(_, node)| (node.parent, restored_update(node)))
        .collect()
}

/// The announcement of a finished agent rebuilt from a stored result.
fn restored_update(node: &AgentNode) -> SubagentUpdate {
    SubagentUpdate {
        subagent_session_id: node.session.to_string(),
        name: Some(node.agent.clone()),
        task: Some(node.description.clone()),
        capabilities: Some(SubagentSessionCapabilities { cancel: false }),
        state: Some(match node.state {
            AgentState::Done => SubagentState::Completed,
            AgentState::Failed => SubagentState::Failed,
            AgentState::Cancelled => SubagentState::Cancelled,
            AgentState::Queued | AgentState::Running => SubagentState::Running,
        }),
        swarm: node.swarm.as_ref().map(|member| SubagentSwarm {
            id: member
                .batch
                .as_ref()
                .map_or_else(|| node.tool_call_id.to_string(), ToString::to_string),
            item: member.item.clone(),
            index: member.index,
            total: member.total,
        }),
        reason: None,
        tool_call_id: Some(node.tool_call_id.to_string()),
        usage: super::bridge::agent_usage(node),
        model: super::bridge::agent_model(node),
        background: node.background,
    }
}

/// The `session/update`s that show `timeline` as the live bridge showed
/// it: user chunks, then each assistant block and tool result mapped
/// through [`to_update`], and at each compaction a `_kage/compaction`
/// update carrying the counts and token delta the file recorded. The
/// timeline is the user's view, so the turns a compaction summarized
/// are all here, with the card where the compaction ran.
/// Thoughts and tool calls carry the durations the file recorded, and
/// each run that ended closes with a `_kage/turn` end saying when and
/// how long. No `plan` update: the todo list lives in memory only, so
/// a resumed session starts with an empty plan.
pub(super) fn replay_updates(timeline: &[TimelineItem]) -> Vec<SessionUpdate> {
    let mut seen = HashMap::new();
    let mut called = HashMap::new();
    let mut run: Option<(&Message, Option<&Message>)> = None;
    let mut updates = Vec::new();
    for item in timeline {
        let message = match item {
            TimelineItem::Compaction(record) => {
                // The card sits where the compaction ran, which for an
                // automatic one is between the turns of a still-open
                // run. Close the run only when its last word is a
                // reply without calls, like the trailing check below;
                // otherwise the run's remaining turns would split off
                // into a phantom second run.
                if let Some((start, Some(last))) = run
                    && last.role == Role::Assistant
                    && !last
                        .content
                        .iter()
                        .any(|block| matches!(block, Content::ToolCall { .. }))
                {
                    updates.extend(run_end(start, last));
                    run = None;
                }
                updates.push(SessionUpdate::Compaction(CompactionUpdate {
                    phase: CompactionPhase::Complete,
                    kept: Some(u64::try_from(record.kept).unwrap_or_default()),
                    summarized: Some(u64::try_from(record.summarized).unwrap_or_default()),
                    before: record.before,
                    after: record.after,
                    summary: Some(record.summary.clone()),
                }));
                continue;
            }
            TimelineItem::Message(message) => message,
        };
        let prompt = message.role == Role::User
            && message
                .content
                .iter()
                .any(|block| matches!(block, Content::Text { .. } | Content::Image { .. }));
        if prompt {
            updates.extend(run.and_then(|(start, last)| run_end(start, last?)));
            run = Some((message, None));
        } else if let Some((_, last)) = &mut run {
            *last = Some(message);
        }
        for block in &message.content {
            let update = match (message.role, block) {
                (_, Content::Text { text } | Content::Thinking { text, .. }) if text.is_empty() => {
                    None
                }
                (Role::User, Content::Text { text }) => Some(user_chunk(ContentBlock::text(text))),
                (Role::User, Content::Image { source, mime }) => {
                    Some(user_chunk(image_block(source, mime)))
                }
                // A tool's images and clips ride its result, below.
                (Role::ToolResult, Content::Image { .. } | Content::Audio { .. }) => None,
                (Role::ToolResult, Content::ToolResultBlock { call_id, .. }) => {
                    replay_event(message.id, block)
                        .map(|event| with_tool_media(event, message, call_id))
                        .and_then(|e| to_update(&mut seen, &e))
                        .map(|update| timed(update, recorded_ms(message, block, &mut called)))
                }
                (_, block) => replay_event(message.id, block)
                    .and_then(|e| to_update(&mut seen, &e))
                    .map(|update| timed(update, recorded_ms(message, block, &mut called))),
            };
            updates.extend(update);
        }
    }
    // A run whose last word is a reply without calls has ended; any
    // other may still be in flight.
    if let Some((start, Some(last))) = run
        && last.role == Role::Assistant
        && !last
            .content
            .iter()
            .any(|block| matches!(block, Content::ToolCall { .. }))
    {
        updates.extend(run_end(start, last));
    }
    updates
}

/// A failed update for each tool call in `history` with no recorded
/// result: the engine stopped before the call finished, such as a
/// desktop app that quit mid-run, so nothing will ever answer it.
pub(super) fn interrupted_calls(history: &[Message]) -> Vec<SessionUpdate> {
    let answered: HashSet<String> = history
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            Content::ToolResultBlock { call_id, .. } => Some(call_id.to_string()),
            _ => None,
        })
        .collect();
    history
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            Content::ToolCall { id, .. } if !answered.contains(&id.to_string()) => {
                Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                    tool_call_id: id.to_string(),
                    status: Some(ToolCallStatus::Failed),
                    content: vec![super::bridge::text_content(
                        "Interrupted: kage stopped before this call finished.".to_owned(),
                    )],
                    ..ToolCallUpdate::default()
                }))
            }
            _ => None,
        })
        .collect()
}

/// The agents that the `agent` and `swarm` calls `history` never got a
/// result for started, found in `dir` by the `kage:agent` marker each
/// agent's file starts with. A run cut off mid-swarm leaves no result
/// to restore them from, yet the members that finished hold their
/// replies; each reads as completed when its last word is a reply
/// without calls, and as failed otherwise.
pub(super) fn orphaned_agents(
    dir: &Path,
    root: SessionId,
    history: &[Message],
) -> Vec<SubagentUpdate> {
    let answered: HashSet<String> = history
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            Content::ToolResultBlock { call_id, .. } => Some(call_id.to_string()),
            _ => None,
        })
        .collect();
    let open: HashSet<String> = history
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            Content::ToolCall { id, name, .. }
                if (name == "agent" || name == "swarm") && !answered.contains(&id.to_string()) =>
            {
                Some(id.to_string())
            }
            _ => None,
        })
        .collect();
    if open.is_empty() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut nodes: Vec<AgentNode> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let session = crate::engine::session_id_of(&path)?;
            let marker = leading_marker_cached(&path)?;
            let text = |key: &str| {
                marker
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let call = text("tool_call_id");
            if text("parent") != root.to_string() || !open.contains(&call) {
                return None;
            }
            let number = |key: &str| {
                marker
                    .get(key)
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or_default()
            };
            let swarm = (!text("batch_id").is_empty()).then(|| SwarmMember {
                batch: None,
                item: text("item"),
                index: number("index"),
                total: number("total"),
            });
            let finished = kage_session::replay(&path).is_ok_and(|replay| replied(&replay.history));
            Some(AgentNode {
                session,
                parent: root,
                tool_call_id: ToolCallId::new(call),
                agent: text("agent"),
                description: text("description"),
                swarm,
                state: if finished {
                    AgentState::Done
                } else {
                    AgentState::Failed
                },
                model: String::new(),
                usage: kage_core::protocol::Usage::default(),
                tool_calls: 0,
                last_tool: None,
                waiting: 0,
                started: None,
                took: None,
                restored: true,
                background: false,
            })
        })
        .collect();
    nodes.sort_by_key(|node| {
        (
            node.tool_call_id.to_string(),
            node.swarm.as_ref().map(|member| member.index),
        )
    });
    nodes.iter().map(restored_update).collect()
}

/// The memoized [`leading_marker`] verdicts, keyed by file and the
/// (mtime, size) pair, so a session listing re-reads a file's head
/// only when the file changed. The cap keeps restored sessions of a
/// long-lived serve from growing it without bound.
fn marker_cache() -> &'static MarkerCache {
    static PEEKED: std::sync::OnceLock<MarkerCache> = std::sync::OnceLock::new();
    PEEKED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The `kage:agent` marker near the top of the session file at
/// `path`, memoized by (mtime, size): the leading entries of a
/// session file are fixed once written, so an unchanged file yields
/// the same marker without the read.
fn leading_marker_cached(path: &Path) -> Option<serde_json::Value> {
    let Ok(meta) = std::fs::metadata(path) else {
        return None;
    };
    let stamp = (meta.modified().ok(), meta.len());
    let mut cache = lock(marker_cache());
    if let Some((seen_mtime, seen_size, marker)) = cache.get(path)
        && (*seen_mtime, *seen_size) == stamp
    {
        return marker.clone();
    }
    let marker = leading_marker(path);
    if cache.len() >= 1024 {
        cache.clear();
    }
    cache.insert(path.to_path_buf(), (stamp.0, stamp.1, marker.clone()));
    marker
}

/// The `kage:agent` marker near the top of the session file at `path`,
/// read without the rest of the file.
fn leading_marker(path: &Path) -> Option<serde_json::Value> {
    SessionReader::iter(path)
        .ok()?
        .take(4)
        .find_map(|entry| match entry {
            Ok(SessionEntry::Custom(custom))
                if custom.kind == kage_session::list::AGENT_ENTRY_KIND =>
            {
                Some(custom.data)
            }
            _ => None,
        })
}

/// Whether `history` ends on a reply that calls no tool: the agent
/// finished its run.
fn replied(history: &[Message]) -> bool {
    history.last().is_some_and(|message| {
        message.role == Role::Assistant
            && !message
                .content
                .iter()
                .any(|block| matches!(block, Content::ToolCall { .. }))
    })
}

/// The `_kage/turn` end of the run `start` prompted and `last` closed,
/// with when it ended and how long it took.
fn run_end(start: &Message, last: &Message) -> Option<SessionUpdate> {
    let took = (last.ts - start.ts).to_std().ok()?;
    Some(SessionUpdate::Turn(TurnUpdate {
        phase: TurnPhase::End,
        reason: Some(TurnReason::NoToolCalls),
        at: Some(last.ts.timestamp()),
        took_ms: Some(u64::try_from(took.as_millis()).unwrap_or(u64::MAX)),
    }))
}

/// How long `block` of `message` took, as recorded: a thought's own
/// duration, or for a tool result the time since its call's message.
/// `called` holds when each call was made.
fn recorded_ms(
    message: &Message,
    block: &Content,
    called: &mut HashMap<String, chrono::DateTime<chrono::Utc>>,
) -> Option<u64> {
    match block {
        Content::Thinking { duration_ms, .. } => *duration_ms,
        Content::ToolCall { id, .. } => {
            called.insert(id.to_string(), message.ts);
            None
        }
        Content::ToolResultBlock { call_id, .. } => {
            let took = (message.ts - called.remove(&call_id.to_string())?)
                .to_std()
                .ok()?;
            Some(u64::try_from(took.as_millis()).unwrap_or(u64::MAX))
        }
        _ => None,
    }
}

/// `update` with the recorded duration `ms` on its thought chunk or
/// tool call update.
fn timed(mut update: SessionUpdate, ms: Option<u64>) -> SessionUpdate {
    if ms.is_none() {
        return update;
    }
    match &mut update {
        SessionUpdate::AgentThoughtChunk(chunk) => {
            chunk.meta.get_or_insert_default().kage.duration_ms = ms;
        }
        SessionUpdate::ToolCallUpdate(call) => {
            call.meta.get_or_insert_default().kage.duration_ms = ms;
        }
        _ => {}
    }
    update
}

/// `event`, a replayed tool result, with the images and clips the
/// tool produced, which follow the result in `message`.
fn with_tool_media(mut event: LoopEvent, message: &Message, call_id: &ToolCallId) -> LoopEvent {
    if let LoopEvent::ToolCallEnd { output, .. } = &mut event {
        output.media = kage_core::tool_results(&message.content)
            .into_iter()
            .filter(|result| result.call_id == call_id)
            .flat_map(|result| result.media.into_iter().chain(result.audio))
            .filter_map(|(source, mime)| match source {
                MediaSource::Base64 { data } => Some(kage_core::ToolMedia {
                    mime: mime.to_owned(),
                    data: data.clone(),
                }),
                MediaSource::Url { .. } | MediaSource::Blob { .. } => None,
            })
            .collect();
    }
    event
}

/// The loop event that streamed `block` of message `id`, if it shows.
fn replay_event(id: MessageId, block: &Content) -> Option<LoopEvent> {
    match block {
        Content::Text { text } => Some(LoopEvent::TextDelta {
            id,
            delta: text.clone(),
        }),
        Content::Thinking { text, .. } => Some(LoopEvent::ThinkingDelta {
            id,
            delta: text.clone(),
        }),
        Content::ToolCall { id, name, input } => Some(LoopEvent::ToolCallStart {
            id: id.clone(),
            name: name.clone(),
            input_partial: input.clone(),
        }),
        Content::ToolResultBlock {
            call_id,
            output,
            is_error,
        } => Some(LoopEvent::ToolCallEnd {
            id: call_id.clone(),
            output: ToolOutput {
                is_error: *is_error,
                text: output.clone(),
                ..ToolOutput::default()
            },
        }),
        Content::Image { source, mime } => Some(match source {
            MediaSource::Base64 { data } => LoopEvent::Media {
                id,
                kind: Modality::of_mime(mime).unwrap_or(Modality::Image),
                mime: mime.clone(),
                data: data.clone(),
            },
            MediaSource::Url { url } => LoopEvent::TextDelta {
                id,
                delta: format!("\n![image]({url})\n"),
            },
            MediaSource::Blob { .. } => LoopEvent::TextDelta {
                id,
                delta: source.missing_note(mime).unwrap_or_default(),
            },
        }),
        Content::Audio { source, mime } => Some(match source {
            MediaSource::Base64 { data } => LoopEvent::Media {
                id,
                kind: Modality::of_mime(mime).unwrap_or(Modality::Audio),
                mime: mime.clone(),
                data: data.clone(),
            },
            MediaSource::Url { url } => LoopEvent::TextDelta {
                id,
                delta: format!("\n[audio]({url})\n"),
            },
            MediaSource::Blob { .. } => LoopEvent::TextDelta {
                id,
                delta: source.missing_note(mime).unwrap_or_default(),
            },
        }),
        Content::Custom { .. } => None,
    }
}

/// One `session/list` page of the client sessions recorded in `dir`,
/// newest activity first. The cursor is the offset of the page.
pub(super) fn list_page(
    dir: &Path,
    req: &ListSessionsRequest,
) -> Result<ListSessionsResponse, RpcError> {
    let start = match &req.cursor {
        Some(cursor) => cursor
            .parse::<usize>()
            .map_err(|_| RpcError::new(-32602, format!("invalid cursor {cursor}")))?,
        None => 0,
    };
    let cwd = req.cwd.as_deref().map(Path::new);
    let mut summaries: Vec<_> = kage_session::list(dir)
        .map_err(|e| RpcError::internal(e.to_string()))?
        .into_iter()
        .filter(|s| s.agent.is_none() && cwd.is_none_or(|cwd| s.cwd == cwd))
        .collect();
    summaries.sort_by_key(|s| Reverse(s.updated_at));
    let end = start.saturating_add(LIST_PAGE);
    let next_cursor = (end < summaries.len()).then(|| end.to_string());
    let sessions = summaries
        .into_iter()
        .skip(start)
        .take(LIST_PAGE)
        .map(|s| SessionInfo {
            session_id: s.id.to_string(),
            cwd: s.cwd.display().to_string(),
            title: s.title,
            updated_at: Some(
                s.updated_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
            meta: s.parent_session.map(|parent| SessionInfoMeta {
                kage: Some(SessionInfoKage {
                    parent_session_id: Some(parent.to_string()),
                }),
            }),
        })
        .collect();
    Ok(ListSessionsResponse {
        sessions,
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kage_acp::acp::TurnPhase;
    use kage_core::protocol::{CompactionRecord, TimelineItem};
    use kage_core::{Content, Message, Role, ToolCallId};

    use super::{SessionUpdate, interrupted_calls, replay_updates};

    fn call(id: &str) -> Content {
        Content::ToolCall {
            id: ToolCallId::new(id),
            name: "shell".to_owned(),
            input: serde_json::json!({}),
        }
    }

    #[test]
    fn only_calls_without_a_result_read_as_interrupted() {
        let history = vec![
            Message::new(Role::Assistant, vec![call("done"), call("swarm")], None),
            Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: ToolCallId::new("done"),
                    output: "ok".to_owned(),
                    is_error: false,
                }],
                None,
            ),
        ];
        let updates = interrupted_calls(&history);
        assert_eq!(updates.len(), 1);
        let SessionUpdate::ToolCallUpdate(update) = &updates[0] else {
            panic!("{updates:?}");
        };
        assert_eq!(update.tool_call_id, "swarm");
        assert_eq!(update.status, Some(kage_acp::acp::ToolCallStatus::Failed));
    }

    #[test]
    fn a_produced_image_replays_as_an_image_chunk() {
        let image = Content::Image {
            source: kage_core::MediaSource::Base64 {
                data: "iVBO".to_owned(),
            },
            mime: "image/png".to_owned(),
        };
        let gone = Content::Image {
            source: kage_core::MediaSource::Blob {
                sha256: "0".repeat(64),
                size: 10,
            },
            mime: "image/png".to_owned(),
        };
        let timeline = vec![TimelineItem::Message(Arc::new(Message::new(
            Role::Assistant,
            vec![image, gone],
            None,
        )))];
        let chunks: Vec<_> = replay_updates(&timeline)
            .into_iter()
            .filter_map(|update| match update {
                SessionUpdate::AgentMessageChunk(chunk) => Some(chunk.content),
                _ => None,
            })
            .collect();
        assert!(
            matches!(&chunks[0], kage_acp::acp::ContentBlock::Image(blob)
                if blob.data == "iVBO" && blob.mime_type == "image/png"),
            "{chunks:?}"
        );
        assert!(
            matches!(&chunks[1], kage_acp::acp::ContentBlock::Text(text)
                if text.text.contains("no longer stored")),
            "{chunks:?}"
        );
    }

    #[test]
    fn a_tool_s_image_replays_with_its_call() {
        let timeline = vec![
            TimelineItem::Message(Arc::new(Message::new(
                Role::Assistant,
                vec![call("draw")],
                None,
            ))),
            TimelineItem::Message(Arc::new(Message::new(
                Role::ToolResult,
                vec![
                    Content::ToolResultBlock {
                        call_id: ToolCallId::new("draw"),
                        output: "drawn".to_owned(),
                        is_error: false,
                    },
                    Content::Image {
                        source: kage_core::MediaSource::Base64 {
                            data: "iVBO".to_owned(),
                        },
                        mime: "image/png".to_owned(),
                    },
                ],
                None,
            ))),
        ];
        let updates = replay_updates(&timeline);
        assert!(
            !updates
                .iter()
                .any(|u| matches!(u, SessionUpdate::AgentMessageChunk(_))),
            "the image is not the agent's reply: {updates:?}"
        );
        let done = updates
            .iter()
            .find_map(|u| match u {
                SessionUpdate::ToolCallUpdate(call) if call.status.is_some() => Some(call),
                _ => None,
            })
            .expect("the call ends");
        let content = &done.content;
        assert!(
            content.iter().any(|c| matches!(c,
                kage_acp::acp::ToolCallContent::Content(chunk)
                    if matches!(&chunk.content, kage_acp::acp::ContentBlock::Image(blob)
                        if blob.data == "iVBO"))),
            "{content:?}"
        );
    }

    #[test]
    fn a_produced_audio_replays_as_an_audio_chunk() {
        let timeline = vec![TimelineItem::Message(Arc::new(Message::new(
            Role::Assistant,
            vec![Content::Audio {
                source: kage_core::MediaSource::Base64 {
                    data: "SUQz".to_owned(),
                },
                mime: "audio/mpeg".to_owned(),
            }],
            None,
        )))];
        let chunks: Vec<_> = replay_updates(&timeline)
            .into_iter()
            .filter_map(|update| match update {
                SessionUpdate::AgentMessageChunk(chunk) => Some(chunk.content),
                _ => None,
            })
            .collect();
        assert!(
            matches!(&chunks[0], kage_acp::acp::ContentBlock::Audio(blob)
                if blob.data == "SUQz" && blob.mime_type == "audio/mpeg"),
            "{chunks:?}"
        );
    }

    #[test]
    fn a_url_audio_replays_as_a_link() {
        let timeline = vec![TimelineItem::Message(Arc::new(Message::new(
            Role::Assistant,
            vec![Content::Audio {
                source: kage_core::MediaSource::Url {
                    url: "https://example.com/clip.mp3".to_owned(),
                },
                mime: "audio/mpeg".to_owned(),
            }],
            None,
        )))];
        let chunks: Vec<_> = replay_updates(&timeline)
            .into_iter()
            .filter_map(|update| match update {
                SessionUpdate::AgentMessageChunk(chunk) => Some(chunk.content),
                _ => None,
            })
            .collect();
        assert!(
            matches!(&chunks[0], kage_acp::acp::ContentBlock::Text(text)
                if text.text == "\n[audio](https://example.com/clip.mp3)\n"),
            "{chunks:?}"
        );
    }

    #[test]
    fn a_tool_s_audio_replays_with_its_call() {
        let timeline = vec![
            TimelineItem::Message(Arc::new(Message::new(
                Role::Assistant,
                vec![call("speak")],
                None,
            ))),
            TimelineItem::Message(Arc::new(Message::new(
                Role::ToolResult,
                vec![
                    Content::ToolResultBlock {
                        call_id: ToolCallId::new("speak"),
                        output: "spoken".to_owned(),
                        is_error: false,
                    },
                    Content::Audio {
                        source: kage_core::MediaSource::Base64 {
                            data: "SUQz".to_owned(),
                        },
                        mime: "audio/mpeg".to_owned(),
                    },
                ],
                None,
            ))),
        ];
        let updates = replay_updates(&timeline);
        assert!(
            !updates
                .iter()
                .any(|u| matches!(u, SessionUpdate::AgentMessageChunk(_))),
            "the clip is not the agent's reply: {updates:?}"
        );
        let done = updates
            .iter()
            .find_map(|u| match u {
                SessionUpdate::ToolCallUpdate(call) if call.status.is_some() => Some(call),
                _ => None,
            })
            .expect("the call ends");
        let content = &done.content;
        assert!(
            content.iter().any(|c| matches!(c,
                kage_acp::acp::ToolCallContent::Content(chunk)
                    if matches!(&chunk.content, kage_acp::acp::ContentBlock::Audio(blob)
                        if blob.data == "SUQz" && blob.mime_type == "audio/mpeg"))),
            "{content:?}"
        );
    }

    fn card() -> TimelineItem {
        TimelineItem::Compaction(CompactionRecord {
            summarized: 2,
            kept: 2,
            before: None,
            after: None,
            summary: "summary".to_owned(),
        })
    }

    fn text(role: Role, text: &str) -> TimelineItem {
        TimelineItem::Message(Arc::new(Message::new(
            role,
            vec![Content::Text {
                text: text.to_owned(),
            }],
            None,
        )))
    }

    /// A compaction that ran between the turns of one run must not
    /// split it: the card leaves the run open, and the run's final
    /// reply closes it once, after the card.
    #[test]
    fn a_mid_run_card_keeps_its_run_whole() {
        let timeline = vec![
            text(Role::User, "go"),
            TimelineItem::Message(Arc::new(Message::new(
                Role::Assistant,
                vec![call("c1")],
                None,
            ))),
            TimelineItem::Message(Arc::new(Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: ToolCallId::new("c1"),
                    output: "ok".to_owned(),
                    is_error: false,
                }],
                None,
            ))),
            card(),
            text(Role::Assistant, "done"),
        ];
        let updates = replay_updates(&timeline);
        let ends = updates
            .iter()
            .filter(|update| matches!(update, SessionUpdate::Turn(t) if t.phase == TurnPhase::End))
            .count();
        assert_eq!(ends, 1, "one run, one end: {updates:?}");
        let card_at = updates
            .iter()
            .position(|update| matches!(update, SessionUpdate::Compaction(_)))
            .unwrap();
        let end_at = updates
            .iter()
            .position(
                |update| matches!(update, SessionUpdate::Turn(t) if t.phase == TurnPhase::End),
            )
            .unwrap();
        assert!(card_at < end_at, "the run ends after the card");
    }

    /// A card that follows a completed run still closes that run
    /// before itself, as it always did.
    #[test]
    fn a_card_after_a_finished_run_closes_it_first() {
        let timeline = vec![
            text(Role::User, "go"),
            text(Role::Assistant, "done"),
            card(),
            text(Role::User, "next"),
        ];
        let updates = replay_updates(&timeline);
        let card_at = updates
            .iter()
            .position(|update| matches!(update, SessionUpdate::Compaction(_)))
            .unwrap();
        let end_at = updates
            .iter()
            .position(
                |update| matches!(update, SessionUpdate::Turn(t) if t.phase == TurnPhase::End),
            )
            .unwrap();
        assert!(end_at < card_at, "the finished run closes before the card");
    }
}
