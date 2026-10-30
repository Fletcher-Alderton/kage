//! Replay a session file back into an in-memory history.
//!
//! [`replay`] walks the active chain of a session, as
//! [`crate::tree::SessionTree`] defines it, and reconstructs the
//! [`Vec<Message>`] that the loop's `cx.history` would hold
//! immediately after the last recorded entry. Compactions are applied
//! exactly as the loop applied them at write time: the named number of
//! leading messages is dropped and replaced by a synthetic user
//! message carrying the recorded summary text, matching the role and
//! framing the live loop inserts so a resumed history is
//! byte-identical to a fresh one. Entries on abandoned branches are
//! skipped, so model, title, usage and compaction are all
//! chain-local.
//!
//! [`find_by_prefix`] and [`find_last`] resolve a session file path from a
//! directory either by id prefix or by most-recent header timestamp.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use kage_core::protocol::{CompactionRecord, TimelineItem};
use kage_core::{Content, Message, MessageId, Role};

use crate::entry::{Compaction, Header, MessageEntry, SessionEntry};
use crate::error::SessionError;
use crate::tree::SessionTree;

/// Result of replaying a session: enough state to seed an `AgentContext`
/// and resume from the last recorded turn.
#[derive(Debug)]
pub struct ReplayResult {
    /// The session's header, carrying provenance and the original model.
    pub header: Header,
    /// Reconstructed conversation history, post-compaction.
    pub history: Vec<Message>,
    /// The messages compaction summarized out of `history` that hold an
    /// `agent` or `swarm` call or an agent's result, in file order. The
    /// agents they record still ran, so a resume lists them.
    pub compacted_agents: Vec<Message>,
    /// Active model id at the end of the session, taking any
    /// [`SessionEntry::ModelChange`] entries into account.
    pub model: String,
    /// Tool-call durations recovered from the entry timestamps. Key is
    /// the `ToolCallId` as a string; value is milliseconds from the
    /// call's `MessageEntry.ts` to the matching result's `ts`.
    pub tool_durations: HashMap<String, u64>,
    /// Sum of every persisted
    /// [`MessageEntry::usage`](crate::MessageEntry::usage) across the
    /// session, post-compaction. Returned as four scalars instead of
    /// a `TokenUsage` to keep this crate's public surface free of
    /// `kage-core` types in result-only positions; the host folds it
    /// into `AgentContext::budget` on resume.
    pub usage_total: ReplayUsage,
    /// Wire identifier of the most recent
    /// [`SessionEntry::ThinkingLevelChange`], or `None` if the session
    /// never recorded one. The host parses it back into a
    /// `ThinkingLevel` and seeds `AgentContext::thinking_level` so a
    /// resumed session keeps the level the user last selected.
    pub thinking_level: Option<String>,
    /// Text of the most recent [`SessionEntry::Title`], or `None` if the
    /// session never recorded one.
    pub title: Option<String>,
    /// Latest swarm mode recorded by a
    /// `kage:swarm_mode` custom entry, or `None` if the session never
    /// toggled it. The host seeds its swarm mode flag from this so a
    /// resumed session keeps the mode without injecting the block
    /// again.
    pub swarm_mode: Option<bool>,
    /// Latest plan mode recorded by a `kage:plan_mode` custom entry, or
    /// `None` if the session never toggled it.
    pub plan_mode: Option<bool>,
    /// Whether the file carries a `kage:agent` marker: it records an
    /// agent an `agent` or `swarm` call started, not a user's session.
    pub agent: bool,
    /// The conversation as the user saw it, oldest first: every
    /// message in order with each compaction as a card at the point
    /// it ran, carrying the counts and tokens the file records. A
    /// compaction shrinks the model's `history`, never this view.
    pub timeline: Vec<TimelineItem>,
}

impl ReplayResult {
    /// Every message that records an agent the session started: the
    /// ones compaction summarized away, then the history, in order.
    pub fn agent_messages(&self) -> impl Iterator<Item = &Message> {
        self.compacted_agents.iter().chain(&self.history)
    }
}

/// Cumulative token totals replayed from a session file, plus the
/// most recent turn's full prompt size for context-fill display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayUsage {
    /// Sum of `usage.input` across all assistant turns.
    pub input: u64,
    /// Sum of `usage.output` across all assistant turns.
    pub output: u64,
    /// Sum of `usage.cache_read` across all assistant turns.
    pub cache_read: u64,
    /// Sum of `usage.cache_write` across all assistant turns.
    pub cache_write: u64,
    /// Sum of `input + output + cache_read + cache_write` of the
    /// last assistant turn that recorded usage. Compared to the
    /// model's context window for the modeline percentage on
    /// resume; `0` until a turn with usage is found.
    pub last_context: u64,
}

/// `message` with each tool result cut to the size a live run keeps.
/// Sessions recorded before tool results were capped can hold results
/// of many megabytes. Agent and swarm results stay whole, since a
/// resume reads every child back out of them.
fn capped(mut message: Message) -> Message {
    for block in &mut message.content {
        if let Content::ToolResultBlock { output, .. } = block
            && !kage_core::is_agent_result(output)
        {
            *output = kage_core::cap_tool_result(std::mem::take(output));
        }
    }
    message
}

/// Record one message entry on both views: the model's `history` and
/// the user's `timeline` share the capped message, and the entry's
/// timestamps feed the tool-call durations.
fn record_message(
    entry: &MessageEntry,
    call_starts: &mut HashMap<String, DateTime<Utc>>,
    tool_durations: &mut HashMap<String, u64>,
    timeline: &mut Vec<TimelineItem>,
    history: &mut Vec<Message>,
) {
    let ts = entry.ts;
    for block in &entry.message.content {
        match block {
            Content::ToolCall { id, .. } => {
                call_starts.insert(id.to_string(), ts);
            }
            Content::ToolResultBlock { call_id, .. } => {
                if let Some(start) = call_starts.remove(&call_id.to_string()) {
                    let delta = ts.signed_duration_since(start).num_milliseconds();
                    if delta >= 0 {
                        tool_durations.insert(
                            call_id.to_string(),
                            u64::try_from(delta).unwrap_or(u64::MAX),
                        );
                    }
                }
            }
            _ => {}
        }
    }
    let message = Arc::new(capped(Arc::unwrap_or_clone(Arc::clone(&entry.message))));
    timeline.push(TimelineItem::Message(Arc::clone(&message)));
    history.push(Arc::unwrap_or_clone(message));
}

/// Replay the active chain of `path`, returning the final history.
pub fn replay(path: &Path) -> Result<ReplayResult, SessionError> {
    let tree = SessionTree::read(path)?;
    let header = tree.header().clone();

    let mut model = kage_core::canonical_model(&header.model);
    let mut thinking_level: Option<String> = None;
    let mut title: Option<String> = None;
    let mut swarm_mode: Option<bool> = None;
    let mut agent = false;
    let mut plan_mode: Option<bool> = None;
    let mut history: Vec<Message> = Vec::new();
    let mut compacted_agents: Vec<Message> = Vec::new();
    // `call_starts` tracks the wall-clock time each ToolCall was
    // appended; on a matching ToolResult we compute the elapsed
    // duration so resumed sessions can show real `Took Xms` instead
    // of `Took 0ms` (which the renderer would otherwise compute from
    // back-to-back replay pushes).
    let mut call_starts: HashMap<String, DateTime<Utc>> = HashMap::new();
    let mut tool_durations: HashMap<String, u64> = HashMap::new();
    let mut usage_total = ReplayUsage::default();
    let mut timeline: Vec<TimelineItem> = Vec::new();
    // Index into `timeline` of the newest compaction card still missing
    // its `after` count. The first usage recorded after the compaction
    // is the post-compaction context fill, so the next usage-carrying
    // message fills it in.
    let mut pending_after: Option<usize> = None;
    for (node, entry) in tree.iter() {
        if !tree.is_active(node.id) {
            continue;
        }
        match entry {
            SessionEntry::Header(_) | SessionEntry::Branch(_) | SessionEntry::Label(_) => {}
            SessionEntry::Message(m) => {
                if let Some(u) = m.usage {
                    usage_total.input = usage_total.input.saturating_add(u.input);
                    usage_total.output = usage_total.output.saturating_add(u.output);
                    usage_total.cache_read = usage_total.cache_read.saturating_add(u.cache_read);
                    usage_total.cache_write = usage_total.cache_write.saturating_add(u.cache_write);
                    let fill = u
                        .input
                        .saturating_add(u.output)
                        .saturating_add(u.cache_read)
                        .saturating_add(u.cache_write);
                    usage_total.last_context = fill;
                    if let Some(at) = pending_after.take()
                        && let TimelineItem::Compaction(card) = &mut timeline[at]
                    {
                        card.after = Some(fill);
                    }
                }
                record_message(
                    m,
                    &mut call_starts,
                    &mut tool_durations,
                    &mut timeline,
                    &mut history,
                );
            }
            SessionEntry::Compaction(c) => {
                compacted_agents.extend(
                    apply_compaction(&mut history, c.clone())
                        .into_iter()
                        .filter(records_agents),
                );
                // The entry sits where the compaction ran, after every
                // turn it could see, so the card goes there too and a
                // resumed transcript reads like the live one did.
                timeline.push(TimelineItem::Compaction(CompactionRecord {
                    summarized: c.summarized,
                    kept: c.kept,
                    before: c.before,
                    after: None,
                    summary: c.summary.clone(),
                }));
                let at = timeline.len() - 1;
                pending_after = Some(at);
            }
            SessionEntry::ModelChange(mc) => model = kage_core::canonical_model(&mc.model),
            SessionEntry::ThinkingLevelChange(t) => thinking_level = Some(t.level.clone()),
            SessionEntry::Title(t) => title = Some(t.title.clone()),
            SessionEntry::Custom(c) => {
                agent |= crate::list::is_agent_marker(c);
                read_mode(c, &mut swarm_mode, &mut plan_mode);
            }
        }
    }
    Ok(ReplayResult {
        header,
        history,
        compacted_agents,
        model,
        tool_durations,
        usage_total,
        thinking_level,
        title,
        swarm_mode,
        plan_mode,
        agent,
        timeline,
    })
}

/// Read a swarm or plan mode toggle from a custom entry into the
/// latest value of that mode.
fn read_mode(c: &crate::Custom, swarm: &mut Option<bool>, plan: &mut Option<bool>) {
    let on = c.data.get("on").and_then(serde_json::Value::as_bool);
    match c.kind.as_str() {
        crate::list::SWARM_MODE_ENTRY_KIND => *swarm = on,
        crate::list::PLAN_MODE_ENTRY_KIND => *plan = on,
        _ => {}
    }
}

/// Whether `message` starts an agent or carries an agent's result.
fn records_agents(message: &Message) -> bool {
    message.content.iter().any(|block| match block {
        Content::ToolCall { name, .. } => name == "agent" || name == "swarm",
        Content::ToolResultBlock { output, .. } => kage_core::is_agent_result(output),
        _ => false,
    })
}

/// Replace the messages `compaction` summarized at the front of
/// `history` with its summary, as the loop did, and give back the
/// messages it replaced.
fn apply_compaction(history: &mut Vec<Message>, compaction: Compaction) -> Vec<Message> {
    // Files from before shell output was recorded can count more
    // summarized messages than they hold, which would leave a result
    // without its call at the front.
    let mut split = compaction.summarized.min(history.len());
    while history
        .get(split)
        .is_some_and(|m| m.role == Role::ToolResult)
    {
        split += 1;
    }
    history
        .splice(
            ..split,
            std::iter::once(Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: compaction.summary,
                }],
                id: MessageId::new(),
                parent: None,
                ts: compaction.ts,
            }),
        )
        .collect()
}

/// Find the session file in `dir` whose id starts with `prefix`. Returns
/// `Ok(None)` if no match exists; an error if the prefix is ambiguous.
pub fn find_by_prefix(dir: &Path, prefix: &str) -> Result<Option<PathBuf>, SessionError> {
    // Each file is named for its session id, so the names alone answer
    // without opening a single session.
    let mut matches = session_files(dir)?
        .into_iter()
        .filter(|(id, _)| id.starts_with(prefix));
    let Some((_, first)) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(SessionError::Io {
            path: dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("session id prefix '{prefix}' is ambiguous"),
            ),
        });
    }
    Ok(Some(first))
}

/// Find the most recently created session in `dir` that was recorded
/// in working directory `cwd`, skipping agent sessions. Candidates are
/// ranked by their header timestamp, so a copied or renamed file whose
/// name sorts high cannot win over a session that is newer by its
/// header; stems break ties, newest first.
pub fn find_last(dir: &Path, cwd: &Path) -> Result<Option<PathBuf>, SessionError> {
    let mut files = session_files(dir)?;
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let mut ranked: Vec<(DateTime<Utc>, String, PathBuf)> = files
        .into_iter()
        .filter_map(|(stem, path)| user_session_header(&path, cwd).map(|ts| (ts, stem, path)))
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    Ok(ranked.into_iter().next().map(|(_, _, path)| path))
}

/// The `*.jsonl` files of `dir`, matching the extension without case,
/// so a `SESSION.JSONL` copy is a candidate too. A missing directory
/// has none.
pub(crate) fn jsonl_candidates(dir: &Path) -> Result<Vec<PathBuf>, SessionError> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(SessionError::Io {
                path: dir.to_path_buf(),
                source: err,
            });
        }
    };
    let mut candidates = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|err| SessionError::Io {
            path: dir.to_path_buf(),
            source: err,
        })?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("jsonl"))
        {
            candidates.push(path);
        }
    }
    Ok(candidates)
}

/// The candidate session files of `dir` with their file stems, which
/// are the session ids. Stems are kept lossy, so a file with a
/// non-UTF8 name still lists and only misses a prefix that is not a
/// prefix of its lossy form. A missing directory has none.
fn session_files(dir: &Path) -> Result<Vec<(String, PathBuf)>, SessionError> {
    Ok(jsonl_candidates(dir)?
        .into_iter()
        .map(|path| {
            let stem = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            (stem, path)
        })
        .collect())
}

/// The header timestamp when `path` starts with a header recorded in
/// `cwd` and is not an agent session, through the one
/// session-identity scan the summary and `is_agent_session` share.
/// `None` when it is not a user session for `cwd`.
fn user_session_header(path: &Path, cwd: &Path) -> Option<DateTime<Utc>> {
    use std::io::BufRead as _;
    let file = std::fs::File::open(path).ok()?;
    let header = std::io::BufReader::new(file)
        .lines()
        .next()
        .and_then(Result::ok)
        .and_then(|line| serde_json::from_str::<SessionEntry>(&line).ok());
    let ts = match header {
        Some(SessionEntry::Header(header)) if header.cwd == cwd => header.ts,
        _ => return None,
    };
    (!crate::list::is_agent_session(path)).then_some(ts)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use kage_core::{Content, Message, Role, ToolCallId};
    use tempfile::tempdir;

    use super::*;
    use crate::entry::{
        Compaction, Custom, EntryId, FORMAT_VERSION, Header, MessageEntry, ModelChange,
        SessionEntry, SessionId,
    };
    use crate::writer::SessionWriter;

    fn fresh_header() -> Header {
        Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "be helpful".into(),
            parent_session: None,
            parent_entry: None,
        }
    }

    fn message_entry(role: Role, text: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Arc::new(Message::new(
                role,
                vec![Content::Text {
                    text: text.to_owned(),
                }],
                None,
            )),
            usage: None,
        })
    }

    fn write(path: &Path, header: Header, entries: &[SessionEntry]) {
        let mut w = SessionWriter::create(path, header).unwrap();
        for e in entries {
            w.append(e).unwrap();
        }
    }

    /// A tool result that only mentions a swarm marker on a later line
    /// is ordinary output, so replay cuts it like any other oversized
    /// result instead of keeping it whole as a swarm aggregate.
    #[test]
    fn replay_caps_a_result_that_only_mentions_a_swarm_marker() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let output = format!(
            "src/a.jsonl:1:\n<swarm description=\"d\" item=\"a\">\n{}",
            "x".repeat(kage_core::MAX_TOOL_RESULT_BYTES + 1)
        );
        write(
            &path,
            fresh_header(),
            &[SessionEntry::Message(MessageEntry {
                id: EntryId::new(),
                ts: Utc::now(),
                message: Arc::new(Message::new(
                    Role::ToolResult,
                    vec![Content::ToolResultBlock {
                        call_id: ToolCallId::new("c1"),
                        output: output.clone(),
                        is_error: false,
                    }],
                    None,
                )),
                usage: None,
            })],
        );

        let history = replay(&path).unwrap().history;
        let Content::ToolResultBlock {
            output: replayed, ..
        } = &history[0].content[0]
        else {
            panic!("expected a tool result, got {:?}", history[0].content[0]);
        };
        assert!(
            replayed.len() <= kage_core::MAX_TOOL_RESULT_BYTES,
            "replayed length {}",
            replayed.len()
        );
        assert!(
            replayed.starts_with("src/a.jsonl:1:"),
            "the head is kept, the filler is cut"
        );
    }

    #[test]
    fn replay_reconstructs_basic_history() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "hello"),
                message_entry(Role::Assistant, "hi there"),
                message_entry(Role::User, "what is 2+2"),
                SessionEntry::Message(MessageEntry {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    message: Arc::new(Message::new(
                        Role::ToolResult,
                        vec![Content::ToolResultBlock {
                            call_id: ToolCallId::new("c1"),
                            output: "4".into(),
                            is_error: false,
                        }],
                        None,
                    )),
                    usage: None,
                }),
                message_entry(Role::Assistant, "4"),
            ],
        );

        let result = replay(&path).unwrap();
        assert_eq!(result.history.len(), 5);
        assert_eq!(result.history[0].role, Role::User);
        assert_eq!(result.history[1].role, Role::Assistant);
        assert_eq!(result.history[3].role, Role::ToolResult);
        assert_eq!(
            result.model, "anthropic/claude",
            "an older id reads with a slash"
        );
    }

    #[test]
    fn replay_applies_compaction() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "old 1"),
                message_entry(Role::Assistant, "old 2"),
                message_entry(Role::User, "old 3"),
                message_entry(Role::Assistant, "old 4"),
                message_entry(Role::User, "kept 1"),
                message_entry(Role::Assistant, "kept 2"),
                SessionEntry::Compaction(Compaction {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    kept: 2,
                    summarized: 4,
                    summary: "[summary of 4 earlier turns]\nthey did stuff".into(),
                    before: Some(160_000),
                }),
                message_entry(Role::User, "post-compact"),
            ],
        );

        let result = replay(&path).unwrap();
        // 1 synthetic + 2 kept + 1 post-compact
        assert_eq!(result.history.len(), 4);
        assert_eq!(result.history[0].role, Role::User);
        match &result.history[0].content[0] {
            Content::Text { text } => {
                assert_eq!(text, "[summary of 4 earlier turns]\nthey did stuff");
            }
            other => panic!("expected text, got {other:?}"),
        }
        // Second-to-last kept message comes from the original "kept 1".
        match &result.history[1].content[0] {
            Content::Text { text } => assert_eq!(text, "kept 1"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// The timeline keeps the turns a compaction summarized, and the
    /// card sits where the compaction ran: after the kept turns, which
    /// existed when it ran, so a resumed transcript matches the live
    /// one. The card carries the counts and the before tokens.
    #[test]
    fn timeline_keeps_summarized_turns_and_the_card_where_it_ran() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "old 1"),
                message_entry(Role::Assistant, "old 2"),
                message_entry(Role::User, "old 3"),
                message_entry(Role::Assistant, "old 4"),
                message_entry(Role::User, "kept 1"),
                message_entry(Role::Assistant, "kept 2"),
                SessionEntry::Compaction(Compaction {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    kept: 2,
                    summarized: 4,
                    summary: "summary".into(),
                    before: Some(160_000),
                }),
                message_entry(Role::User, "post-compact"),
            ],
        );

        let timeline = replay(&path).unwrap().timeline;
        let texts: Vec<String> = timeline
            .iter()
            .map(|item| match item {
                TimelineItem::Message(m) => match &m.content[0] {
                    Content::Text { text } => text.clone(),
                    other => panic!("expected text, got {other:?}"),
                },
                TimelineItem::Compaction(_) => "<card>".to_owned(),
            })
            .collect();
        assert_eq!(
            texts,
            [
                "old 1",
                "old 2",
                "old 3",
                "old 4",
                "kept 1",
                "kept 2",
                "<card>",
                "post-compact"
            ],
            "nothing a compaction summarized may leave the user's view"
        );
        match &timeline[6] {
            TimelineItem::Compaction(card) => {
                assert_eq!(card.summarized, 4);
                assert_eq!(card.kept, 2);
                assert_eq!(card.before, Some(160_000));
                assert_eq!(card.summary, "summary");
            }
            other @ TimelineItem::Message(_) => panic!("expected the card, got {other:?}"),
        }
    }

    /// A keep window that spans the whole history (a manual compaction
    /// on a short session) must not pull the card to the very top, and
    /// a second compaction's card follows the first in run order.
    #[test]
    fn timeline_keeps_each_card_at_its_own_point() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let compaction = |summarized: usize, kept: usize| {
            SessionEntry::Compaction(Compaction {
                id: EntryId::new(),
                ts: Utc::now(),
                kept,
                summarized,
                summary: "summary".into(),
                before: None,
            })
        };
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "one"),
                message_entry(Role::Assistant, "two"),
                compaction(2, 2),
                message_entry(Role::User, "three"),
                message_entry(Role::Assistant, "four"),
                compaction(3, 2),
                message_entry(Role::User, "five"),
            ],
        );

        let timeline = replay(&path).unwrap().timeline;
        let texts: Vec<String> = timeline
            .iter()
            .map(|item| match item {
                TimelineItem::Message(m) => match &m.content[0] {
                    Content::Text { text } => text.clone(),
                    other => panic!("expected text, got {other:?}"),
                },
                TimelineItem::Compaction(_) => "<card>".to_owned(),
            })
            .collect();
        assert_eq!(
            texts,
            ["one", "two", "<card>", "three", "four", "<card>", "five"],
            "cards land in the order the compactions ran"
        );
    }

    /// The card's `after` is the fill of the first turn recorded with
    /// usage after the compaction, the same pairing the live bridge
    /// performs.
    #[test]
    fn timeline_fills_the_card_after_from_the_next_usage() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let used = |input: u64| {
            SessionEntry::Message(MessageEntry {
                id: EntryId::new(),
                ts: Utc::now(),
                message: Arc::new(Message::new(
                    Role::Assistant,
                    vec![Content::Text {
                        text: "reply".into(),
                    }],
                    None,
                )),
                usage: Some(kage_core::TokenUsage {
                    input,
                    output: 100,
                    ..kage_core::TokenUsage::default()
                }),
            })
        };
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "old"),
                used(1_000),
                message_entry(Role::User, "kept"),
                SessionEntry::Compaction(Compaction {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    kept: 1,
                    summarized: 2,
                    summary: "summary".into(),
                    before: Some(9_000),
                }),
                message_entry(Role::User, "post-compact"),
                used(42_000),
            ],
        );

        let timeline = replay(&path).unwrap().timeline;
        let card = timeline
            .iter()
            .find_map(|item| match item {
                TimelineItem::Compaction(card) => Some(card),
                TimelineItem::Message(_) => None,
            })
            .unwrap();
        assert_eq!(card.before, Some(9_000));
        assert_eq!(card.after, Some(42_100), "the first usage after the card");
    }

    /// A compaction whose next turn never recorded usage keeps its
    /// `after` unset rather than guessing.
    #[test]
    fn timeline_leaves_the_card_after_unset_without_usage() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "old"),
                message_entry(Role::Assistant, "reply"),
                SessionEntry::Compaction(Compaction {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    kept: 1,
                    summarized: 1,
                    summary: "summary".into(),
                    before: None,
                }),
                message_entry(Role::User, "post-compact"),
            ],
        );

        let timeline = replay(&path).unwrap().timeline;
        match timeline.iter().find_map(|item| match item {
            TimelineItem::Compaction(card) => Some(card),
            TimelineItem::Message(_) => None,
        }) {
            Some(card) => {
                assert_eq!(card.before, None);
                assert_eq!(card.after, None);
            }
            None => panic!("the compaction card is missing"),
        }
    }

    #[test]
    fn replay_drops_results_a_compaction_cut_from_their_calls() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let result = SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Arc::new(Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: ToolCallId::new("c1"),
                    output: "done".into(),
                    is_error: false,
                }],
                None,
            )),
            usage: None,
        });
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "old"),
                message_entry(Role::Assistant, "call"),
                result,
                message_entry(Role::Assistant, "kept"),
                SessionEntry::Compaction(Compaction {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    kept: 2,
                    summarized: 2,
                    summary: "summary".into(),
                    before: None,
                }),
            ],
        );

        let roles: Vec<Role> = replay(&path)
            .unwrap()
            .history
            .iter()
            .map(|m| m.role)
            .collect();
        assert_eq!(roles, [Role::User, Role::Assistant]);
    }

    #[test]
    fn replay_keeps_the_agents_a_compaction_summarized() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let entry = |role, content| {
            SessionEntry::Message(MessageEntry {
                id: EntryId::new(),
                ts: Utc::now(),
                message: Arc::new(Message::new(role, vec![content], None)),
                usage: None,
            })
        };
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "look around"),
                entry(
                    Role::Assistant,
                    Content::ToolCall {
                        id: ToolCallId::new("a1"),
                        name: "agent".into(),
                        input: serde_json::json!({"description": "explore"}),
                    },
                ),
                entry(
                    Role::ToolResult,
                    Content::ToolResultBlock {
                        call_id: ToolCallId::new("a1"),
                        output: "<agent session=\"x\">found it</agent>".into(),
                        is_error: false,
                    },
                ),
                message_entry(Role::Assistant, "done"),
                message_entry(Role::User, "kept"),
                SessionEntry::Compaction(Compaction {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    kept: 1,
                    summarized: 4,
                    summary: "summary".into(),
                    before: None,
                }),
            ],
        );

        let result = replay(&path).unwrap();
        assert_eq!(result.history.len(), 2, "the summary and the kept prompt");
        let roles: Vec<Role> = result.compacted_agents.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [Role::Assistant, Role::ToolResult],
            "the agent's call and result outlive the compaction"
        );
        assert_eq!(result.agent_messages().count(), 4);
    }

    #[test]
    fn replay_applies_model_change() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "go"),
                SessionEntry::ModelChange(ModelChange {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    model: "openai:gpt-4o".into(),
                }),
                message_entry(Role::Assistant, "ok"),
            ],
        );
        let result = replay(&path).unwrap();
        assert_eq!(result.model, "openai/gpt-4o");
        assert_eq!(result.history.len(), 2);
    }

    #[test]
    fn replay_keeps_the_latest_title() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let title = |text: &str| {
            SessionEntry::Title(crate::entry::SessionTitle {
                id: EntryId::new(),
                ts: Utc::now(),
                title: text.into(),
            })
        };
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "go"),
                title("first"),
                title("second"),
            ],
        );
        assert_eq!(replay(&path).unwrap().title.as_deref(), Some("second"));
    }

    /// The file an in-place undo of everything but the first exchange
    /// produces, plus one new prompt on the new branch.
    fn branched(path: &std::path::Path) {
        let header = fresh_header();
        let kept = message_entry(Role::User, "kept");
        let reply = message_entry(Role::Assistant, "reply");
        let dropped = message_entry(Role::User, "dropped");
        let dropped_reply = message_entry(Role::Assistant, "dropped reply");
        let marker = SessionEntry::Branch(crate::entry::BranchEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            at: reply.id(),
        });
        let redo = message_entry(Role::User, "redo");
        write(
            path,
            header,
            &[kept, reply, dropped, dropped_reply, marker, redo],
        );
    }

    #[test]
    fn replay_follows_the_active_branch() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        branched(&path);
        let history = replay(&path).unwrap().history;
        let texts: Vec<&str> = history
            .iter()
            .map(|m| match &m.content[0] {
                Content::Text { text } => text.as_str(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect();
        assert_eq!(texts, ["kept", "reply", "redo"]);
    }

    #[test]
    fn replay_takes_state_entries_from_the_active_chain_only() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let header = fresh_header();
        let kept = message_entry(Role::User, "kept");
        let title = SessionEntry::Title(crate::entry::SessionTitle {
            id: EntryId::new(),
            ts: Utc::now(),
            title: "abandoned".into(),
        });
        let change = SessionEntry::ModelChange(ModelChange {
            id: EntryId::new(),
            ts: Utc::now(),
            model: "openai:gpt-4o".into(),
        });
        // The marker rewinds to the header: the title and the model
        // change above it are abandoned, and the prompt below is the
        // only thing left on the active chain.
        let marker = SessionEntry::Branch(crate::entry::BranchEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            at: header.id,
        });
        let redo = message_entry(Role::User, "redo");
        write(&path, header, &[kept, title, change, marker, redo]);

        let result = replay(&path).unwrap();
        assert_eq!(result.title, None, "the abandoned title is skipped");
        assert_eq!(
            result.model, "anthropic/claude",
            "the abandoned model change is skipped"
        );
        assert_eq!(result.history.len(), 1);
    }

    #[test]
    fn replay_reads_a_version_one_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("old.jsonl");
        let mut header = fresh_header();
        header.version = 1;
        write(&path, header, &[message_entry(Role::User, "hello")]);
        assert_eq!(replay(&path).unwrap().history.len(), 1);
    }

    #[test]
    fn replay_reports_a_terminated_corrupt_tail() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("torn.jsonl");
        write(&path, fresh_header(), &[message_entry(Role::User, "hello")]);
        // A corrupt last line that ends in `\n` is not a torn write:
        // replay must fail instead of returning a shortened history.
        let mut raw = std::fs::read(&path).unwrap();
        raw.extend_from_slice(b"{\"corrupt\": true}\n");
        std::fs::write(&path, raw).unwrap();
        match replay(&path) {
            Err(SessionError::Decode { line: 3, .. }) => {}
            other => panic!("expected a decode error at line 3, got {other:?}"),
        }
    }

    #[test]
    fn replay_rejects_unsupported_format_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let header = Header {
            version: FORMAT_VERSION + 1,
            ..fresh_header()
        };
        write(&path, header, &[message_entry(Role::User, "hi")]);
        match replay(&path) {
            Err(SessionError::UnsupportedVersion { found, .. }) => {
                assert_eq!(found, FORMAT_VERSION + 1);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    /// The structural failures used to surface as laundered serde
    /// messages (`missing field 'type' ...`); they must name the real
    /// problem instead.
    #[test]
    fn structural_failures_name_the_real_problem() {
        let dir = tempdir().unwrap();

        let empty = dir.path().join("empty.jsonl");
        std::fs::write(&empty, b"").unwrap();
        let err = replay(&empty).unwrap_err();
        assert!(matches!(err, SessionError::Empty { .. }), "{err:?}");
        assert_eq!(
            err.to_string(),
            format!("session {} is empty", empty.display())
        );

        let headerless = dir.path().join("headerless.jsonl");
        // A decodable entry first: a bad *trailing* line is treated as
        // a torn write by the reader, so it must decode to be refused
        // as headerless.
        let entry = serde_json::to_string(&message_entry(Role::User, "hi")).unwrap();
        std::fs::write(&headerless, format!("{entry}\n")).unwrap();
        let err = replay(&headerless).unwrap_err();
        assert!(matches!(err, SessionError::MissingHeader { .. }), "{err:?}");
        assert!(err.to_string().contains("first entry is not a header"));

        let path = dir.path().join("two-headers.jsonl");
        write(
            &path,
            fresh_header(),
            &[
                SessionEntry::Header(fresh_header()),
                message_entry(Role::User, "hi"),
            ],
        );
        let err = replay(&path).unwrap_err();
        assert!(matches!(err, SessionError::SecondHeader { .. }), "{err:?}");
        assert!(err.to_string().contains("second header in file"));
    }

    #[test]
    fn replay_reads_the_latest_swarm_mode_entry() {
        let dir = tempdir().unwrap();
        let custom = |on: bool| {
            SessionEntry::Custom(Custom {
                id: EntryId::new(),
                ts: Utc::now(),
                kind: crate::list::SWARM_MODE_ENTRY_KIND.to_owned(),
                data: serde_json::json!({ "on": on }),
            })
        };
        let path = dir.path().join("off.jsonl");
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "hello"),
                custom(true),
                message_entry(Role::Assistant, "hi"),
                custom(false),
            ],
        );
        assert_eq!(replay(&path).unwrap().swarm_mode, Some(false));

        let path = dir.path().join("on.jsonl");
        write(
            &path,
            fresh_header(),
            &[message_entry(Role::User, "hello"), custom(true)],
        );
        assert_eq!(replay(&path).unwrap().swarm_mode, Some(true));

        let path = dir.path().join("never.jsonl");
        write(&path, fresh_header(), &[message_entry(Role::User, "hello")]);
        assert_eq!(replay(&path).unwrap().swarm_mode, None);
    }

    #[test]
    fn replay_reads_the_latest_plan_mode_entry() {
        let dir = tempdir().unwrap();
        let custom = |kind: &str, on: bool| {
            SessionEntry::Custom(Custom {
                id: EntryId::new(),
                ts: Utc::now(),
                kind: kind.to_owned(),
                data: serde_json::json!({ "on": on }),
            })
        };
        let path = dir.path().join("plan.jsonl");
        write(
            &path,
            fresh_header(),
            &[
                custom(crate::list::PLAN_MODE_ENTRY_KIND, true),
                custom(crate::list::SWARM_MODE_ENTRY_KIND, false),
            ],
        );
        let replayed = replay(&path).unwrap();
        assert_eq!(replayed.plan_mode, Some(true));
        assert_eq!(replayed.swarm_mode, Some(false));
    }

    #[test]
    fn find_by_prefix_resolves_unique_match() {
        let dir = tempdir().unwrap();
        let path_a = dir.path().join("01AAAAAAAAAAAAAAAAAAAAAAAA.jsonl");
        let path_b = dir.path().join("01BBBBBBBBBBBBBBBBBBBBBBBB.jsonl");
        let mut header_a = fresh_header();
        let mut header_b = fresh_header();
        header_a.session =
            SessionId(ulid::Ulid::from_string("01AAAAAAAAAAAAAAAAAAAAAAAA").unwrap());
        header_b.session =
            SessionId(ulid::Ulid::from_string("01BBBBBBBBBBBBBBBBBBBBBBBB").unwrap());
        write(&path_a, header_a, &[]);
        write(&path_b, header_b, &[]);

        let found = find_by_prefix(dir.path(), "01A").unwrap().unwrap();
        assert_eq!(found, path_a);
    }

    #[test]
    fn find_by_prefix_errors_on_ambiguity() {
        let dir = tempdir().unwrap();
        let path_a = dir.path().join("01AAAAAAAAAAAAAAAAAAAAAAAA.jsonl");
        let path_b = dir.path().join("01ABBBBBBBBBBBBBBBBBBBBBBB.jsonl");
        let mut header_a = fresh_header();
        let mut header_b = fresh_header();
        header_a.session =
            SessionId(ulid::Ulid::from_string("01AAAAAAAAAAAAAAAAAAAAAAAA").unwrap());
        header_b.session =
            SessionId(ulid::Ulid::from_string("01ABBBBBBBBBBBBBBBBBBBBBBB").unwrap());
        write(&path_a, header_a, &[]);
        write(&path_b, header_b, &[]);

        let err = find_by_prefix(dir.path(), "01A").unwrap_err();
        assert!(matches!(err, SessionError::Io { .. }));
    }

    #[test]
    fn find_last_returns_newest() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("a.jsonl"), fresh_header(), &[]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let path_b = dir.path().join("b.jsonl");
        write(&path_b, fresh_header(), &[]);

        let last = find_last(dir.path(), Path::new("/work")).unwrap().unwrap();
        assert_eq!(last, path_b);
    }

    #[test]
    fn find_last_keeps_to_the_working_directory() {
        let dir = tempdir().unwrap();
        let here = dir.path().join("a.jsonl");
        write(&here, fresh_header(), &[]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut elsewhere = fresh_header();
        elsewhere.cwd = PathBuf::from("/other");
        write(&dir.path().join("b.jsonl"), elsewhere, &[]);

        assert_eq!(
            find_last(dir.path(), Path::new("/work")).unwrap(),
            Some(here)
        );
        assert_eq!(find_last(dir.path(), Path::new("/nowhere")).unwrap(), None);
    }

    #[test]
    fn find_last_skips_agent_sessions() {
        let dir = tempdir().unwrap();
        let main = dir.path().join("a.jsonl");
        write(&main, fresh_header(), &[]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let marker = SessionEntry::Custom(crate::entry::Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind: crate::list::AGENT_ENTRY_KIND.into(),
            data: serde_json::json!({ "agent": "explore" }),
        });
        write(&dir.path().join("b.jsonl"), fresh_header(), &[marker]);

        assert_eq!(
            find_last(dir.path(), Path::new("/work")).unwrap().unwrap(),
            main
        );
    }

    #[test]
    fn find_last_skips_a_fork_shaped_late_marker() {
        let dir = tempdir().unwrap();
        let main = dir.path().join("a.jsonl");
        write(&main, fresh_header(), &[]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let marker = SessionEntry::Custom(crate::entry::Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind: crate::list::AGENT_ENTRY_KIND.into(),
            data: serde_json::json!({ "agent": "explore" }),
        });
        // The shape a forked child is written in: the copied history
        // first, then the marker appended after.
        let forked = dir.path().join("b.jsonl");
        write(
            &forked,
            fresh_header(),
            &[
                message_entry(Role::User, "copied ask"),
                message_entry(Role::Assistant, "copied reply"),
                marker,
            ],
        );

        assert!(crate::list::is_agent_session(&forked));
        assert_eq!(
            find_last(dir.path(), Path::new("/work")).unwrap().unwrap(),
            main,
            "the forked child is not the last user session"
        );
    }

    #[test]
    fn find_last_ranks_by_header_timestamp_not_stem() {
        let dir = tempdir().unwrap();
        let mut older = fresh_header();
        older.ts = Utc::now() - chrono::Duration::hours(1);
        let older_path = dir.path().join("z.jsonl");
        write(&older_path, older, &[]);
        let newer_path = dir.path().join("a.jsonl");
        write(&newer_path, fresh_header(), &[]);

        let last = find_last(dir.path(), Path::new("/work")).unwrap().unwrap();
        assert_eq!(
            last, newer_path,
            "a high-sorting stem with an older header must lose"
        );
    }

    #[test]
    fn jsonl_candidates_match_the_extension_without_case() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("a.jsonl"), fresh_header(), &[]);
        write(&dir.path().join("UPPER.JSONL"), fresh_header(), &[]);
        std::fs::write(dir.path().join("notes.txt"), b"nope").unwrap();

        let mut found: Vec<String> = jsonl_candidates(dir.path())
            .unwrap()
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(found, ["UPPER.JSONL", "a.jsonl"]);
    }

    #[test]
    fn find_last_sees_an_uppercase_extension() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ONLY.JSONL");
        write(&path, fresh_header(), &[]);

        assert_eq!(
            find_last(dir.path(), Path::new("/work")).unwrap().unwrap(),
            path
        );
    }

    // APFS and NTFS reject non-UTF-8 names, so the fixture only
    // exists on Linux.
    #[cfg(target_os = "linux")]
    #[test]
    fn find_last_and_prefix_scan_carry_a_non_utf8_stem() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempdir().unwrap();
        let path = dir.path().join(OsStr::from_bytes(b"bad\xff.jsonl"));
        write(&path, fresh_header(), &[]);

        assert_eq!(
            find_last(dir.path(), Path::new("/work")).unwrap().unwrap(),
            path,
            "a non-UTF8 stem stays a find_last candidate"
        );
        let stems: Vec<String> = session_files(dir.path())
            .unwrap()
            .into_iter()
            .map(|(stem, _)| stem)
            .collect();
        assert_eq!(stems, ["bad\u{fffd}"], "stems are kept lossy");
    }
}
