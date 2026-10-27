//! Session file operations: fork, clone, export, and the engine commands
//! that branch, copy, or drop a session.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use kage_core::protocol::{HostEvent, NoticeLevel, PastPrompt};
use kage_core::{Content, Role, SessionId};
use kage_session::{BranchEntry, EntryId, SessionEntry, SessionTree, SessionWriter};

/// Read the JSONL session at `path` and return the id of its active
/// head, the entry a fork with no cut point copies through, or `None`
/// if the file holds only a header.
fn find_last_entry(path: &std::path::Path) -> Result<Option<EntryId>, kage_session::SessionError> {
    let tree = SessionTree::read(path)?;
    Ok((tree.head() != tree.root()).then_some(tree.head()))
}

/// One user prompt in a session's branch tree that a rewind can target.
struct PromptEntry {
    /// The prompt's own message entry.
    at: EntryId,
    /// The entry the conversation continues from to drop the prompt
    /// and everything after it: the prompt's tree parent.
    cut: EntryId,
    /// The last entry of the prompt's turn: the prompt, its reply and
    /// the state recorded before the next prompt. The entry a rewind
    /// keeps the conversation at.
    turn_end: EntryId,
    /// The prompt's content.
    content: Vec<Content>,
    /// How many branch edges sit above the prompt, for the picker's
    /// indentation.
    depth: u32,
    /// Whether the prompt is the head the conversation would continue
    /// from.
    active: bool,
}

/// Every user prompt in the branch tree of `path`, depth-first: a
/// branch's descendants before its next sibling, siblings in file
/// order, so the list reads oldest first with the active head last.
/// Depth counts the branch edges above a prompt, so every prompt on
/// the original thread sits at `0` and a re-prompted branch indents
/// one level under the thread it forked from. `active` marks exactly
/// one row: the head prompt the conversation would continue from.
fn prompt_tree(path: &std::path::Path) -> Result<Vec<PromptEntry>, kage_session::SessionError> {
    let tree = SessionTree::read(path)?;
    let mut head = None;
    let mut cursor = tree.head();
    loop {
        if matches!(
            tree.entry(cursor),
            Some(kage_session::SessionEntry::Message(m)) if m.message.role == Role::User
        ) {
            head = Some(cursor);
            break;
        }
        let Some(parent) = tree.parent(cursor) else {
            break;
        };
        cursor = parent;
    }
    // The natural child of a node: the entry that directly follows it
    // in the file with no branch marker between. A turn runs along
    // natural edges and ends at the next prompt.
    let mut natural: std::collections::HashMap<EntryId, EntryId> = std::collections::HashMap::new();
    for (node, _) in tree.iter() {
        if !node.branched {
            natural.insert(node.parent, node.id);
        }
    }
    let is_prompt = |id: EntryId| matches!(tree.entry(id), Some(kage_session::SessionEntry::Message(m)) if m.message.role == Role::User);
    let mut found = Vec::new();
    for step in tree.walk() {
        let kage_session::SessionEntry::Message(message) = step.entry else {
            continue;
        };
        if message.message.role != Role::User {
            continue;
        }
        let mut depth = 0u32;
        let mut cursor = step.node.id;
        while let Some(parent) = tree.parent(cursor) {
            if tree.branched(cursor) {
                depth += 1;
            }
            cursor = parent;
        }
        let mut turn_end = step.node.id;
        while let Some(&child) = natural.get(&turn_end) {
            if is_prompt(child) {
                break;
            }
            turn_end = child;
        }
        found.push(PromptEntry {
            at: step.node.id,
            cut: step.node.parent,
            turn_end,
            content: message.message.content.clone(),
            depth,
            active: Some(step.node.id) == head,
        });
    }
    Ok(found)
}

/// Copy `src` up through entry `at` (an id prefix, or the latest entry)
/// into a new session file next to it. Returns the new path and id.
pub(super) fn fork_session(src: &Path, at: Option<&str>) -> Result<(PathBuf, SessionId), String> {
    if !src.exists() {
        return Err("the session has no committed entries yet".to_owned());
    }
    let entry: EntryId = match at {
        Some(prefix) => {
            kage_session::resolve_entry_prefix(src, prefix).map_err(|e| e.to_string())?
        }
        None => find_last_entry(src)
            .map_err(|e| e.to_string())?
            .ok_or("the session has no entries to fork at")?,
    };
    let dir = src.parent().ok_or("session path has no parent directory")?;
    let id = SessionId::new();
    let dst = dir.join(format!("{id}.jsonl"));
    kage_session::fork(src, &dst, id, entry).map_err(|e| e.to_string())?;
    Ok((dst, id))
}

/// Write `src` as a Markdown transcript to `dest`, or to
/// `<short-id>.md` in the working directory. Returns the written path.
pub(super) fn export_session(src: &Path, dest: Option<PathBuf>) -> Result<PathBuf, String> {
    if !src.exists() {
        return Err("the session has no committed entries yet".to_owned());
    }
    let replay = kage_session::replay(src).map_err(|e| e.to_string())?;
    let out = dest.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(format!("{}.md", short_id(replay.header.session)))
    });
    std::fs::write(&out, render_session_markdown(&replay))
        .map_err(|e| format!("write {}: {e}", out.display()))?;
    Ok(out)
}

/// First eight characters of a session id, for notices.
pub(super) fn short_id(id: SessionId) -> String {
    id.to_string().chars().take(8).collect()
}

/// The markdown section for one compaction card: the counts, then the
/// summary body without its machine framing.
fn write_compaction_markdown(md: &mut String, record: &kage_core::protocol::CompactionRecord) {
    use std::fmt::Write as _;
    let _ = writeln!(md, "## Compacted history");
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "{}/{} turns summarized, summary follows.",
        record.summarized,
        record.summarized + record.kept,
    );
    let _ = writeln!(md);
    let body = record
        .summary
        .strip_prefix(kage_core::message::COMPACTION_SUMMARY_PREFIX)
        .unwrap_or(&record.summary);
    let body = body
        .strip_suffix(kage_core::message::COMPACTION_SUMMARY_SUFFIX)
        .unwrap_or(body);
    let _ = writeln!(md, "{}", body.trim());
    let _ = writeln!(md);
}

/// Render a replayed session as a Markdown transcript. Plain text and
/// fenced code only, no HTML, so the file stays greppable and
/// ASCII-clean.
pub(crate) fn render_session_markdown(replay: &kage_session::ReplayResult) -> String {
    use std::fmt::Write as _;
    let short: String = replay.header.session.to_string().chars().take(8).collect();
    let mut md = String::new();
    let _ = writeln!(md, "# kage session {short}");
    let _ = writeln!(md);
    let _ = writeln!(md, "- model: `{}`", replay.model);
    let _ = writeln!(md, "- created: {}", replay.header.ts.to_rfc3339());
    let _ = writeln!(md, "- cwd: `{}`", replay.header.cwd.display());
    let _ = writeln!(md);
    for item in &replay.timeline {
        let msg = match item {
            kage_core::protocol::TimelineItem::Compaction(record) => {
                write_compaction_markdown(&mut md, record);
                continue;
            }
            kage_core::protocol::TimelineItem::Message(message) => message,
        };
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::ToolResult => "Tool",
            Role::System => "System",
        };
        let _ = writeln!(md, "## {role}");
        let _ = writeln!(md);
        for block in &msg.content {
            match block {
                Content::Text { text } => {
                    let _ = writeln!(md, "{text}");
                    let _ = writeln!(md);
                }
                Content::Thinking { text, .. } => {
                    let _ = writeln!(md, "**thinking**");
                    let _ = writeln!(md);
                    for line in text.lines() {
                        let _ = writeln!(md, "> {line}");
                    }
                    let _ = writeln!(md);
                }
                Content::ToolCall { name, input, .. } => {
                    let pretty =
                        serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
                    let fence = fence_for(&pretty);
                    let _ = writeln!(md, "**tool call: `{name}`**");
                    let _ = writeln!(md);
                    let _ = writeln!(md, "{fence}json");
                    let _ = writeln!(md, "{pretty}");
                    let _ = writeln!(md, "{fence}");
                    let _ = writeln!(md);
                }
                Content::ToolResultBlock {
                    output, is_error, ..
                } => {
                    let label = if *is_error {
                        "tool result (error)"
                    } else {
                        "tool result"
                    };
                    let fence = fence_for(output);
                    let _ = writeln!(md, "**{label}**");
                    let _ = writeln!(md);
                    let _ = writeln!(md, "{fence}");
                    let _ = writeln!(md, "{output}");
                    let _ = writeln!(md, "{fence}");
                    let _ = writeln!(md);
                }
                Content::Image { mime, .. } => {
                    let _ = writeln!(md, "_[image omitted: {mime}]_");
                    let _ = writeln!(md);
                }
                Content::Audio { mime, .. } => {
                    let _ = writeln!(md, "_[audio omitted: {mime}]_");
                    let _ = writeln!(md);
                }
                Content::Custom { kind, data } => {
                    let pretty =
                        serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
                    let fence = fence_for(&pretty);
                    let _ = writeln!(md, "_[custom block: {kind}]_");
                    let _ = writeln!(md);
                    let _ = writeln!(md, "{fence}json");
                    let _ = writeln!(md, "{pretty}");
                    let _ = writeln!(md, "{fence}");
                    let _ = writeln!(md);
                }
            }
        }
    }
    md
}

/// A fenced code block fence for `body`: one backtick longer than the
/// longest backtick run inside it, minimum three, per the `CommonMark`
/// fence rule, so the wrapper can never close on the body's own
/// fences.
fn fence_for(body: &str) -> String {
    let mut longest = 2;
    let mut run = 0;
    for ch in body.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest + 1)
}

impl super::Dispatcher {
    /// `true` when `id` has no run or shell command in flight; otherwise
    /// tells the user.
    pub(super) fn ensure_idle(&self, id: SessionId, what: &str) -> bool {
        if self
            .sessions
            .get(&id)
            .is_some_and(|s| s.idle.is_some() && s.shells == 0)
        {
            return true;
        }
        super::notice(
            &self.bus,
            id,
            NoticeLevel::Warning,
            format!("{what}: wait for the current run to finish or cancel it"),
        );
        false
    }

    /// Drop the idle session `id` and its idle agent descendants, and
    /// stop its running background agents. A session with a run or
    /// shell in flight is kept and warned instead. Closing the active session leaves the engine without
    /// one, so a later command without a session id is refused.
    pub(super) fn close(&mut self, id: SessionId) {
        if !self.ensure_idle(id, "close") {
            return;
        }
        // Background agents may still run under an idle session; they
        // have nobody to report to once it is gone.
        for (agent, session) in &self.sessions {
            if session.idle.is_none() && self.descends_from(*agent, id) {
                session.cancel.cancel();
            }
        }
        let mut dropped = vec![id];
        dropped.extend(
            self.sessions
                .keys()
                .copied()
                .filter(|agent| self.descends_from(*agent, id))
                .filter(|agent| {
                    self.sessions
                        .get(agent)
                        .is_some_and(|s| s.idle.is_some() && s.shells == 0)
                }),
        );
        for dropped in &dropped {
            // A queued child still owes its call a result, so it ends
            // through the waiting path, which publishes its cancelled
            // `RunEnded` and delivers the cancelled report.
            if self.waiting.contains(dropped) {
                self.end_waiting(*dropped);
            }
            self.sessions.remove(dropped);
            self.swarm_requeues.remove(dropped);
            self.bus.forget(*dropped);
            self.report_closed(*dropped);
        }
        if self.active == Some(id) {
            self.active = None;
        }
    }

    /// `true` when `id` is an agent session, which cannot be replaced or
    /// copied; tells the user.
    fn refuse_for_agent(&self, id: SessionId, what: &str) -> bool {
        if self.sessions.get(&id).is_none_or(|s| s.link.is_none()) {
            return false;
        }
        super::notice(
            &self.bus,
            id,
            NoticeLevel::Warning,
            format!("{what}: not available in an agent session"),
        );
        true
    }

    /// `true` when `id` and its agents are all idle; otherwise tells the
    /// user.
    fn ensure_tree_idle(&self, id: SessionId, what: &str) -> bool {
        if !self.ensure_idle(id, what) {
            return false;
        }
        let busy = self.sessions.iter().any(|(agent, s)| {
            (s.idle.is_none() || self.waiting.contains(agent)) && self.descends_from(*agent, id)
        });
        if busy {
            super::notice(
                &self.bus,
                id,
                NoticeLevel::Warning,
                format!("{what}: stop or wait for the agents first"),
            );
        }
        !busy
    }

    pub(super) fn new_session(&mut self, id: SessionId) {
        if self.refuse_for_agent(id, "new session") || !self.ensure_tree_idle(id, "new session") {
            return;
        }
        let session = &self.sessions[&id];
        let Some(idle) = session.idle.as_ref() else {
            return;
        };
        // The new session goes next to the one it replaces.
        let dir = idle
            .recorder
            .as_ref()
            .and_then(|recorder| recorder.path().parent().map(Path::to_path_buf));
        let planned = match dir {
            Some(dir) => Ok(crate::plan_session_in(
                &dir,
                &session.state.model,
                &idle.cx.system_prompt,
            )),
            None => crate::plan_session(&session.state.model, &idle.cx.system_prompt),
        };
        let (path, mut header) = match planned {
            Ok(planned) => planned,
            Err(err) => {
                self.error(id, format!("new session: {err}"));
                return;
            }
        };
        header.cwd.clone_from(&session.workdir);
        session.gate.set_plan(false);
        let new_id = header.session;
        let mut cx = super::clone_shallow(&idle.cx);
        cx.budget = kage_loop::TokenBudget::default();
        let recorder = super::Recorder::planned(path, header, session.plugins.clone());
        self.reseat(
            id,
            new_id,
            cx,
            recorder,
            None,
            Vec::new(),
            Vec::new(),
            format!("new session: {}", short_id(new_id)),
        );
    }

    pub(super) fn load_session(&mut self, id: SessionId, path: &Path) {
        if self.refuse_for_agent(id, "resume") || !self.ensure_tree_idle(id, "resume") {
            return;
        }
        if self.sessions[&id].path.as_deref() == Some(path) {
            self.info(id, "that session is already open".to_owned());
            return;
        }
        let replay = match kage_session::replay(path) {
            Ok(replay) => replay,
            Err(err) => {
                self.error(id, format!("resume {}: {err}", path.display()));
                return;
            }
        };
        let writer = match SessionWriter::open(path) {
            Ok(writer) => writer,
            Err(kage_session::SessionError::Locked { .. }) => {
                let short = short_id(replay.header.session);
                self.error(
                    id,
                    format!(
                        "resume: session {short} is open in another kage process. Quit kage \
                         there and resume it again. When a `kage serve` hosts it, \
                         `kage resume {short}` attaches to it."
                    ),
                );
                return;
            }
            Err(err) => {
                self.error(id, format!("resume {}: {err}", path.display()));
                return;
            }
        };
        let session = self.sessions.get_mut(&id).expect("session checked");
        let Some(idle) = session.idle.as_ref() else {
            return;
        };
        let mut cx = super::clone_shallow(&idle.cx);
        let mut fallback = None;
        if self.registry.resolve(&replay.model).is_ok() {
            session.state.model.clone_from(&replay.model);
        } else {
            fallback = Some(format!(
                "session model {} unavailable; using {} instead",
                replay.model, session.state.model
            ));
        }
        cx.history = replay.history.into_iter().map(Arc::new).collect();
        session.swarm_mode = replay.swarm_mode.unwrap_or(false);
        session.gate.set_plan(replay.plan_mode.unwrap_or(false));
        cx.budget = kage_loop::TokenBudget {
            used_input: replay.usage_total.input,
            used_output: replay.usage_total.output,
            used_cache_read: replay.usage_total.cache_read,
            used_cache_write: replay.usage_total.cache_write,
            current_context: replay.usage_total.last_context,
        };
        cx.thinking_level = replay
            .thinking_level
            .as_deref()
            .and_then(kage_core::ThinkingLevel::parse);
        let new_id = replay.header.session;
        let message = format!(
            "resumed session {} on {}",
            short_id(new_id),
            session.state.model
        );
        let recorder = super::Recorder::new(writer, session.plugins.clone());
        self.reseat(
            id,
            new_id,
            cx,
            recorder,
            replay.title,
            replay.timeline,
            replay.compacted_agents.into_iter().map(Arc::new).collect(),
            message,
        );
        if let Some(note) = fallback {
            self.info(new_id, note);
        }
    }

    pub(super) fn clone_session(&mut self, id: SessionId) {
        if self.refuse_for_agent(id, "clone") || !self.ensure_tree_idle(id, "clone") {
            return;
        }
        let Some(src) = self.sessions[&id].path.clone() else {
            self.error(id, "clone: the session is not recorded".to_owned());
            return;
        };
        let (dst, new_id) = match fork_session(&src, None) {
            Ok(forked) => forked,
            Err(err) => {
                self.error(id, format!("clone: {err}"));
                return;
            }
        };
        let writer = match SessionWriter::open(&dst) {
            Ok(writer) => writer,
            Err(err) => {
                self.error(id, format!("clone: {err}"));
                return;
            }
        };
        let session = &self.sessions[&id];
        let Some(idle) = session.idle.as_ref() else {
            return;
        };
        let cx = idle.cx.clone();
        let recorder = super::Recorder::new(writer, session.plugins.clone());
        let stored = kage_session::replay(&dst).ok();
        self.reseat(
            id,
            new_id,
            cx,
            recorder,
            stored.as_ref().and_then(|s| s.title.clone()),
            stored
                .as_ref()
                .map(|s| s.timeline.clone())
                .unwrap_or_default(),
            stored
                .map(|s| s.compacted_agents.into_iter().map(Arc::new).collect())
                .unwrap_or_default(),
            format!("cloned session: {}", short_id(new_id)),
        );
    }

    pub(super) fn fork(&mut self, id: SessionId, at: Option<&str>, switch: bool) {
        if self.refuse_for_agent(id, "fork") {
            return;
        }
        let Some(src) = self.sessions[&id].path.clone() else {
            self.error(id, "fork: the session is not recorded".to_owned());
            return;
        };
        match fork_session(&src, at) {
            Ok((dst, _)) if switch => self.load_session(id, &dst),
            Ok((_, new_id)) => self.info(id, format!("forked session: {}", short_id(new_id))),
            Err(err) => self.error(id, format!("fork: {err}")),
        }
    }

    /// Drop the last user prompt and everything after it: branch the
    /// session in place, continuing from the entry before that
    /// prompt. The removed prompt goes back to the client in
    /// [`HostEvent::Undone`] so it can offer it for a retry.
    pub(super) fn undo(&mut self, id: SessionId) {
        if self.refuse_for_agent(id, "undo") || !self.ensure_idle(id, "undo") {
            return;
        }
        let Some(src) = self.sessions[&id].path.clone() else {
            self.error(id, "undo: the session is not recorded".to_owned());
            return;
        };
        match prompt_tree(&src).map(|prompts| prompts.into_iter().find(|prompt| prompt.active)) {
            Ok(Some(prompt)) => {
                self.rewind_to(id, "undo", prompt.cut, Some(prompt.content));
            }
            Ok(None) => {
                self.error(id, "undo: nothing to undo".to_owned());
            }
            Err(err) => {
                self.error(id, format!("undo: {err}"));
            }
        }
    }

    /// Branch the session in place to the end of the turn of the user
    /// prompt with entry id `at`, the [`CommandKind::Rewind`] half of
    /// [`Self::undo`]. Any prompt on any branch of the session is a
    /// target. The picked prompt and its reply stay; what follows the
    /// turn is abandoned.
    pub(super) fn rewind(&mut self, id: SessionId, at: &str) {
        if self.refuse_for_agent(id, "rewind") || !self.ensure_idle(id, "rewind") {
            return;
        }
        let Some(src) = self.sessions[&id].path.clone() else {
            self.error(id, "rewind: the session is not recorded".to_owned());
            return;
        };
        let prompts = match prompt_tree(&src) {
            Ok(prompts) => prompts,
            Err(err) => {
                self.error(id, format!("rewind: {err}"));
                return;
            }
        };
        let matches: Vec<&PromptEntry> = prompts
            .iter()
            .filter(|prompt| prompt.at.to_string().starts_with(at))
            .collect();
        let prompt = match matches.as_slice() {
            [one] => *one,
            [] => {
                self.error(id, format!("rewind: no user prompt matches `{at}`"));
                return;
            }
            _ => {
                self.error(id, format!("rewind: `{at}` matches more than one prompt"));
                return;
            }
        };
        self.rewind_to(id, "rewind", prompt.turn_end, None);
    }

    /// Shared tail of [`Self::undo`] and [`Self::rewind`]: append a
    /// branch marker that continues the session from `at`, reload the
    /// session in place from its own file, and for [`Self::undo`]
    /// hand the dropped `prompt` back to the client for the editor.
    /// The session keeps its id and its file; the abandoned entries
    /// stay in the file as an abandoned branch.
    #[expect(
        clippy::too_many_lines,
        reason = "one linear reload: marker append, replay, then the same state reseat applies"
    )]
    fn rewind_to(&mut self, id: SessionId, what: &str, at: EntryId, prompt: Option<Vec<Content>>) {
        let Some(path) = self.sessions[&id].path.clone() else {
            self.error(id, format!("{what}: the session is not recorded"));
            return;
        };
        let (state, usage, title, timeline, compacted_agents, fallback) = {
            let Some(session) = self.sessions.get_mut(&id) else {
                return;
            };
            let Some(mut idle) = session.idle.take() else {
                return;
            };
            let marker = SessionEntry::Branch(BranchEntry {
                id: EntryId::new(),
                ts: Utc::now(),
                at,
            });
            let appended = match idle.recorder.as_mut() {
                Some(recorder) => recorder.append(&marker).map_err(|err| err.to_string()),
                None => Err("the session is not recorded".to_owned()),
            };
            if let Err(err) = appended {
                session.idle = Some(idle);
                self.error(id, format!("{what}: {err}"));
                return;
            }
            let replay = match kage_session::replay(&path) {
                Ok(replay) => replay,
                Err(err) => {
                    session.idle = Some(idle);
                    self.error(id, format!("{what}: {err}"));
                    return;
                }
            };
            let mut fallback = None;
            if self.registry.resolve(&replay.model).is_ok() {
                session.state.model.clone_from(&replay.model);
            } else {
                fallback = Some(format!(
                    "session model {} unavailable; using {} instead",
                    replay.model, session.state.model
                ));
            }
            let mut cx = super::clone_shallow(&idle.cx);
            cx.history = replay.history.into_iter().map(Arc::new).collect();
            cx.budget = kage_loop::TokenBudget {
                used_input: replay.usage_total.input,
                used_output: replay.usage_total.output,
                used_cache_read: replay.usage_total.cache_read,
                used_cache_write: replay.usage_total.cache_write,
                current_context: replay.usage_total.last_context,
            };
            cx.thinking_level = replay
                .thinking_level
                .as_deref()
                .and_then(kage_core::ThinkingLevel::parse);
            session.swarm_mode = replay.swarm_mode.unwrap_or(false);
            session.gate.set_plan(replay.plan_mode.unwrap_or(false));
            super::fit_context(&mut cx, &self.registry, &session.state.model);
            session.usage = super::usage_of(&cx);
            session.state.thinking = cx.thinking_level;
            super::fit_to_model(&mut session.state, &self.registry);
            session.thinking_changed = false;
            session.model_changed = false;
            session.title_pending = session.title && !super::has_reply(&cx);
            session.pending_history.clear();
            session.queued.clear();
            kage_core::sync::lock(&session.inbox).clear();
            session.gate.reset_session();
            session.state.permission_mode = None;
            session.idle = Some(super::Idle {
                cx,
                recorder: idle.recorder,
            });
            session.sync_state();
            let state = session.state.clone();
            let usage = session.usage;
            let title = replay.title;
            let timeline = replay.timeline;
            let compacted_agents = replay
                .compacted_agents
                .into_iter()
                .map(Arc::new)
                .collect::<Vec<_>>();
            (state, usage, title, timeline, compacted_agents, fallback)
        };
        // The agents of this session forked from a history the rewind
        // rewrote.
        let agents: Vec<SessionId> = self
            .sessions
            .keys()
            .copied()
            .filter(|agent| self.descends_from(*agent, id))
            .collect();
        for agent in agents {
            self.sessions.remove(&agent);
            self.swarm_requeues.remove(&agent);
            self.bus.forget(agent);
            self.report_closed(agent);
        }
        let servers = self.sessions[&id]
            .mcp
            .as_ref()
            .map(kage_mcp::McpManager::catalog)
            .unwrap_or_default();
        self.bus.publish(
            id,
            HostEvent::SessionChanged {
                path,
                title,
                timeline,
                compacted_agents,
            },
        );
        self.bus.publish(id, HostEvent::StateChanged { state });
        self.bus.publish(id, HostEvent::UsageUpdated { usage });
        self.bus.publish(id, HostEvent::McpServers { servers });
        if let Some(note) = fallback {
            self.info(id, note);
        }
        self.bus.publish(id, HostEvent::Undone { prompt });
    }

    /// Answer [`CommandKind::ListPrompts`] with every user prompt the
    /// session's branch tree holds, oldest first.
    pub(super) fn list_prompts(&mut self, id: SessionId) {
        if self.refuse_for_agent(id, "rewind") {
            return;
        }
        let Some(src) = self.sessions[&id].path.clone() else {
            self.error(id, "rewind: the session is not recorded".to_owned());
            return;
        };
        match prompt_tree(&src) {
            Ok(prompts) => {
                let prompts = prompts
                    .into_iter()
                    .map(|prompt| PastPrompt {
                        at: prompt.at.to_string(),
                        prompt: prompt.content,
                        depth: prompt.depth,
                        active: prompt.active,
                    })
                    .collect();
                self.bus.publish(id, HostEvent::PromptList { prompts });
            }
            Err(err) => {
                self.error(id, format!("rewind: {err}"));
            }
        }
    }

    pub(super) fn fork_file(&self, id: SessionId, path: &Path) {
        if self.refuse_for_agent(id, "fork") {
            return;
        }
        match fork_session(path, None) {
            Ok((_, new_id)) => self.info(id, format!("forked session: {}", short_id(new_id))),
            Err(err) => self.error(id, format!("fork: {err}")),
        }
    }

    pub(super) fn delete_session(&self, id: SessionId, path: &Path) {
        if self
            .sessions
            .values()
            .any(|s| s.path.as_deref() == Some(path))
        {
            self.info(id, "cannot delete an open session".to_owned());
            return;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {
                let short: String = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.chars().take(8).collect())
                    .unwrap_or_default();
                self.info(id, format!("deleted session: {short}"));
            }
            Err(err) => self.error(id, format!("delete failed: {err}")),
        }
    }

    pub(super) fn export(&self, id: SessionId, dest: Option<PathBuf>) {
        let Some(src) = self.sessions[&id].path.clone() else {
            self.error(id, "export: the session is not recorded".to_owned());
            return;
        };
        match export_session(&src, dest) {
            Ok(out) => self.info(id, format!("exported to {}", out.display())),
            Err(err) => self.error(id, format!("export: {err}")),
        }
    }

    /// Replace session `old` with a session `new` recorded by
    /// `recorder` under `title`, keeping its tools, plugins, MCP servers and
    /// settings but not its permission mode or approvals, drop the
    /// agents of `old`, and tell clients, including the MCP catalog for
    /// `new` and the `compaction` counts the file records.
    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is separately loaded session state"
    )]
    fn reseat(
        &mut self,
        old: SessionId,
        new: SessionId,
        mut cx: kage_loop::AgentContext,
        recorder: super::Recorder,
        title: Option<String>,
        timeline: Vec<kage_core::protocol::TimelineItem>,
        compacted_agents: Vec<Arc<kage_core::Message>>,
        message: String,
    ) {
        let path = recorder.path().to_path_buf();
        let mut session = self.sessions.remove(&old).expect("session checked");
        self.swarm_requeues.remove(&old);
        self.bus.forget(old);
        self.report_closed(old);
        super::fit_context(&mut cx, &self.registry, &session.state.model);
        session.usage = super::usage_of(&cx);
        session.state.thinking = cx.thinking_level;
        super::fit_to_model(&mut session.state, &self.registry);
        session.thinking_changed = false;
        session.model_changed = false;
        session.title_pending = session.title && !super::has_reply(&cx);
        session.pending_history.clear();
        session.queued.clear();
        kage_core::sync::lock(&session.inbox).clear();
        session.gate.reset_session();
        session.state.permission_mode = None;
        session.path = Some(path.clone());
        session.idle = Some(super::Idle {
            cx,
            recorder: Some(recorder),
        });
        session.sync_state();
        let state = session.state.clone();
        let usage = session.usage;
        let servers = session
            .mcp
            .as_ref()
            .map(kage_mcp::McpManager::catalog)
            .unwrap_or_default();
        let agents: Vec<SessionId> = self
            .sessions
            .keys()
            .copied()
            .filter(|agent| self.descends_from(*agent, old))
            .collect();
        for agent in agents {
            self.sessions.remove(&agent);
            self.swarm_requeues.remove(&agent);
            self.bus.forget(agent);
            self.report_closed(agent);
        }
        self.sessions.insert(new, session);
        if self.active == Some(old) {
            self.active = Some(new);
        }
        self.bus.publish(
            new,
            HostEvent::SessionChanged {
                path,
                title,
                timeline,
                compacted_agents,
            },
        );
        self.bus.publish(new, HostEvent::StateChanged { state });
        self.bus.publish(new, HostEvent::UsageUpdated { usage });
        self.bus.publish(new, HostEvent::McpServers { servers });
        self.info(new, message);
    }

    pub(super) fn info(&self, id: SessionId, text: String) {
        self.bus.publish(
            id,
            HostEvent::Notice {
                level: NoticeLevel::Info,
                text,
                transient: true,
            },
        );
    }

    pub(super) fn error(&self, id: SessionId, text: String) {
        super::notice(&self.bus, id, NoticeLevel::Error, text);
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use kage_core::Message;
    use kage_session::{FORMAT_VERSION, Header, MessageEntry, SessionEntry, SessionReader};

    use super::*;

    fn write_session(dir: &Path) -> (PathBuf, SessionId) {
        let id = SessionId::new();
        let path = dir.join(format!("{id}.jsonl"));
        let header = Header {
            version: FORMAT_VERSION,
            session: id,
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: String::new(),
            parent_session: None,
            parent_entry: None,
        };
        let mut writer = SessionWriter::create(&path, header).unwrap();
        writer
            .append(&SessionEntry::Message(MessageEntry {
                id: EntryId::new(),
                ts: Utc::now(),
                message: Arc::new(Message::new(
                    Role::User,
                    vec![Content::Text {
                        text: "hello".to_owned(),
                    }],
                    None,
                )),
                usage: None,
            }))
            .unwrap();
        (path, id)
    }

    #[test]
    fn export_writes_markdown_to_the_given_path() {
        let dir = tempfile::tempdir().unwrap();
        let (src, _) = write_session(dir.path());
        let out = dir.path().join("out.md");
        assert_eq!(export_session(&src, Some(out.clone())).unwrap(), out);
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(body.contains("# kage session "));
        assert!(body.contains("## User"));
        assert!(body.contains("hello"));
    }

    #[test]
    fn fork_creates_a_parent_linked_session() {
        let dir = tempfile::tempdir().unwrap();
        let (src, src_id) = write_session(dir.path());
        let (forked, _) = fork_session(&src, None).unwrap();
        let mut reader = SessionReader::iter(&forked).unwrap();
        match reader.next().unwrap().unwrap() {
            SessionEntry::Header(h) => assert_eq!(h.parent_session, Some(src_id)),
            other => panic!("expected header, got {other:?}"),
        }
    }

    #[test]
    fn fence_for_is_one_longer_than_the_longest_inner_run() {
        assert_eq!(fence_for("plain"), "```");
        assert_eq!(fence_for("a ``` b"), "````");
        assert_eq!(fence_for("``\ntext\n````"), "`````");
        assert_eq!(fence_for(""), "```");
    }

    #[test]
    fn exported_fences_exceed_every_backtick_run_in_tool_output() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let header = Header {
            version: FORMAT_VERSION,
            session: id,
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: String::new(),
            parent_session: None,
            parent_entry: None,
        };
        let output = "read notes.md:\n```rust\nfn main() {}\n```\n````txt\nfour\n````";
        let mut writer =
            SessionWriter::create(dir.path().join(format!("{id}.jsonl")), header).unwrap();
        writer
            .append(&SessionEntry::Message(MessageEntry {
                id: EntryId::new(),
                ts: Utc::now(),
                message: Arc::new(Message::new(
                    Role::ToolResult,
                    vec![Content::ToolResultBlock {
                        call_id: kage_core::ToolCallId::new("c1"),
                        output: output.to_owned(),
                        is_error: false,
                    }],
                    None,
                )),
                usage: None,
            }))
            .unwrap();

        let replay = kage_session::replay(&dir.path().join(format!("{id}.jsonl"))).unwrap();
        let md = render_session_markdown(&replay);
        assert!(md.contains(output), "the body round-trips: {md}");
        let fences: Vec<usize> = md
            .lines()
            .filter(|line| !line.is_empty() && line.chars().all(|c| c == '`'))
            .map(str::len)
            .collect();
        assert_eq!(fences.first(), Some(&5), "fence beats the ```` run: {md}");
        assert!(
            fences.iter().all(|len| *len >= 3),
            "every wrapper is a valid fence: {fences:?}"
        );
    }
}
