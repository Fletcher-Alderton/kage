//! Engine events applied to the conversation buffer.
//!
//! [`apply_loop_event`] folds streamed [`kage_core::LoopEvent`]s into the
//! buffer's block model and [`populate_from_history`] rebuilds a buffer
//! from a stored conversation. The renderer reads the same buffer each
//! frame, so painting is decoupled from event arrival.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kage_core::agent_report::{AgentText, split_agent_text};
use kage_core::protocol::{CompactionCounts, TimelineItem};
use kage_core::resource_block::{self, ResourceRef};
use kage_core::{Content, LoopError, LoopEvent, MediaSource, Message, MessageId, Role, StopReason};

use crate::buffer::{Block, Buffer};
use crate::view::tool_view::ToolPhase;

/// Cloneable handle to the conversation buffer shared between the App
/// and its renderer.
pub type SharedBuffer = Arc<Mutex<Buffer>>;

/// Construct an empty shared buffer.
#[must_use]
pub fn shared_buffer() -> SharedBuffer {
    Arc::new(Mutex::new(Buffer::new()))
}

/// Fold one loop event into `buf`. User prompts arrive as
/// [`LoopEvent::MessageAppended`], so a steered prompt appears when the
/// agent actually receives it.
pub fn apply_loop_event(buf: &mut Buffer, event: &LoopEvent) {
    match event {
        LoopEvent::MessageAppended { message } if message.role == Role::User => {
            push_user_message(buf, message);
        }
        LoopEvent::MessageStart { .. }
        | LoopEvent::MessageAppended { .. }
        | LoopEvent::TurnStarted { .. }
        | LoopEvent::TurnEnded { .. }
        | LoopEvent::CompactionStarted { .. } => {
            // The buffer lazily begins an Assistant block on the first
            // text/thinking delta, so MessageStart is a no-op here.
        }
        LoopEvent::TextDelta { delta, .. } => buf.append_assistant_delta(delta),
        LoopEvent::ThinkingDelta { delta, .. } => buf.append_thinking_delta(delta),
        LoopEvent::Media { mime, data, .. } => {
            let label = produced_media_label(mime, data.len() / 4 * 3);
            buf.push_custom("kage:image", label, false);
        }
        LoopEvent::ToolCallArgsDelta {
            id,
            name,
            input_partial,
        } => buf.upsert_tool_call(id.to_string(), name, input_partial.clone()),
        LoopEvent::ToolCallStart {
            id,
            name,
            input_partial,
        } => {
            let id = id.to_string();
            buf.upsert_tool_call(id.clone(), name, input_partial.clone());
            buf.set_tool_phase(&id, ToolPhase::Queued);
        }
        LoopEvent::ToolExecutionStart { id } => {
            buf.set_tool_phase(&id.to_string(), ToolPhase::Running);
        }
        LoopEvent::ToolUpdate { id, update } => {
            buf.set_tool_progress(&id.to_string(), update.content.clone());
        }
        LoopEvent::ToolCallEnd { id, output } => {
            buf.push_tool_result(id.to_string(), output.text.clone(), output.is_error);
            for media in &output.media {
                let label = tool_media_label(&media.mime, media.data.len() / 4 * 3);
                buf.push_custom("kage:image", label, false);
            }
        }
        LoopEvent::MessageEnd {
            stop_reason: StopReason::MaxTokens,
            ..
        } => {
            // Finalize the live assistant bubble first: finish_streaming
            // targets the last block, so the notice must land after it.
            buf.finish_streaming();
            buf.push_custom(
                "kage:truncated",
                "reply hit the max output token limit",
                false,
            );
        }
        LoopEvent::MessageEnd { .. } => buf.finish_streaming(),
        LoopEvent::Compaction {
            kept,
            summarized,
            summary,
            ..
        } => {
            let counts = CompactionCounts {
                summarized: *summarized,
                kept: *kept,
            };
            buf.push_custom(
                "kage:compaction",
                compaction_text(Some(counts), None, summary),
                true,
            );
        }
        LoopEvent::ProviderRetry {
            attempt,
            max_attempts,
            wait_secs,
            wait_ms,
            requested_secs,
            error,
        } => {
            let wait_text = if *wait_ms < 1000 {
                format!("{wait_ms}ms")
            } else {
                format!("{wait_secs}s")
            };
            let mut msg = format!(
                "provider error ({error}); retrying {attempt}/{max_attempts} in {wait_text}"
            );
            if let Some(req) = requested_secs
                && *req > *wait_secs
            {
                use std::fmt::Write as _;
                let _ = write!(msg, " (server asked for {req}s)");
            }
            buf.replace_or_push_custom("kage:retry", msg);
        }
        LoopEvent::Error { kind } => push_error(buf, kind),
    }
}

/// Surface a loop error in `buf`: finalize the live assistant bubble,
/// then add one notice naming the failure.
fn push_error(buf: &mut Buffer, kind: &LoopError) {
    buf.finish_streaming();
    match kind {
        LoopError::Cancelled => buf.push_custom("kage:notify", "Interrupted", false),
        LoopError::Auth { message } => buf.push_custom(
            "kage:error",
            format!(
                "authentication failed: {}. Run /login to re-authenticate.",
                message.trim_end_matches('.')
            ),
            false,
        ),
        // The block's `error` chrome already names the severity;
        // the payload adds nothing but the message.
        other => buf.push_custom("kage:error", other.to_string(), false),
    }
}

/// Paint a user prompt: its text as a user bubble, then one placeholder
/// per attached image. An image that follows the resource block naming
/// it already has its `attached` line in the bubble.
fn push_user_message(buf: &mut Buffer, message: &Message) {
    if let Some(text) = user_text(message) {
        push_user_text(buf, text);
    }
    push_user_image_rows(buf, message);
}

/// The row that stands for an image a tool produced, the same live and
/// on replay.
fn tool_media_label(mime: &str, bytes: usize) -> String {
    format!(
        "[{mime} from the tool, {}]",
        crate::image::human_bytes(bytes)
    )
}

/// The row that stands for media the model produced: its type and
/// size, the same live and on replay.
fn produced_media_label(mime: &str, bytes: usize) -> String {
    format!(
        "[{mime} from the model, {}]",
        crate::image::human_bytes(bytes)
    )
}

/// One `kage:image` row per attached image, skipping an image the
/// bubble's resource block already named. Shared by the live prompt
/// path and the resume replay so both show attached images.
fn push_user_image_rows(buf: &mut Buffer, message: &Message) {
    let mut labelled = false;
    for block in &message.content {
        match block {
            Content::Text { text } => {
                labelled = resource_block::parse(text).is_some_and(|r| is_image(&r));
            }
            Content::Image { mime, .. } => {
                if !labelled {
                    buf.push_custom("kage:image", format!("[image: {mime}]"), false);
                }
                labelled = false;
            }
            _ => labelled = false,
        }
    }
}

/// How long tool calls took in milliseconds, keyed by the message that
/// carries each result and the call id. Providers may reuse call ids
/// across turns, so the id alone does not name one call.
pub type ToolDurations = HashMap<(MessageId, String), u64>;

/// How long each tool call took, recovered from the timestamps of the
/// assistant message that made the call and the message carrying its
/// result.
#[must_use]
pub fn tool_durations(messages: &[Arc<Message>]) -> ToolDurations {
    let mut started = HashMap::new();
    let mut durations = HashMap::new();
    for message in messages {
        for block in &message.content {
            match block {
                Content::ToolCall { id, .. } => {
                    started.insert(id.to_string(), message.ts);
                }
                Content::ToolResultBlock { call_id, .. } => {
                    if let Some(start) = started.remove(&call_id.to_string()) {
                        let ms = (message.ts - start).num_milliseconds();
                        durations.insert(
                            (message.id, call_id.to_string()),
                            u64::try_from(ms).unwrap_or(0),
                        );
                    }
                }
                _ => {}
            }
        }
    }
    durations
}

/// The text of a `kage:compaction` block: a header line naming the
/// message `counts` and, when known, the tokens the summary saved,
/// over the `summary`.
fn compaction_text(
    counts: Option<CompactionCounts>,
    tokens: Option<(u64, u64)>,
    summary: &str,
) -> String {
    match counts {
        Some(CompactionCounts { summarized, kept }) => format!(
            "Compacted history ({summarized} messages summarized, {kept} kept){}\n{summary}",
            tokens_suffix(tokens),
        ),
        None => summary.to_owned(),
    }
}

/// The ` \u{b7} 84k \u{2192} 12k tokens` a compaction header carries once the
/// post-compaction usage has arrived. A block pushed before then gets
/// its header line amended with this, so the suffix must ride the
/// header's first line.
pub(crate) fn tokens_suffix(tokens: Option<(u64, u64)>) -> String {
    match tokens {
        Some((before, after)) => format!(
            " \u{b7} {} \u{2192} {} tokens",
            crate::usage::compact_tokens(before),
            crate::usage::compact_tokens(after),
        ),
        None => String::new(),
    }
}

/// Fold the post-compaction context fill into the newest compaction
/// card that still lacks a token delta. Returns whether one was
/// amended. The delta rides the header's first line, so a card whose
/// header already names one is left alone.
pub fn amend_compaction_with_usage(buf: &mut Buffer, before: u64, after: u64) -> bool {
    let found = buf.blocks().iter().rev().find_map(|b| match b.as_ref() {
        Block::Custom { kind, text, .. } if kind == "kage:compaction" => {
            let header = text.lines().next().unwrap_or_default();
            (!header.contains('\u{2192}')).then(|| (text.clone(), header.to_owned()))
        }
        _ => None,
    });
    let Some((text, header)) = found else {
        return false;
    };
    let rest = &text[header.len()..];
    let amended = format!("{header}{}{rest}", tokens_suffix(Some((before, after))));
    buf.replace_custom_where(
        "kage:compaction",
        |t| !t.lines().next().unwrap_or_default().contains('\u{2192}'),
        amended,
    );
    true
}

/// Last line of a shell block cancelled before its command finished.
const SHELL_CANCELLED: &str = "(cancelled)";

/// Last line of a shell block whose command a signal ended.
const SHELL_KILLED: &str = "(killed by a signal)";

/// The text of a finished `kage:shell` block: the `$ command` header,
/// the output and a status line. The engine ends the output of a
/// cancelled command with a `cancelled` line, which becomes the status.
pub(crate) fn shell_block(command: &str, output: &str, exit_code: Option<i32>) -> String {
    let output = output.trim_end();
    let (output, status) = match exit_code {
        Some(code) => (output, format!("(exit code {code})")),
        None => match output.strip_suffix("cancelled") {
            Some(rest) if rest.is_empty() || rest.ends_with('\n') => {
                (rest.trim_end(), SHELL_CANCELLED.to_owned())
            }
            _ => (output, SHELL_KILLED.to_owned()),
        },
    };
    if output.is_empty() {
        format!("$ {command}\n{status}")
    } else {
        format!("$ {command}\n{output}\n{status}")
    }
}

/// Whether `line` is the status line [`shell_block`] ends a finished
/// shell block with.
pub(crate) fn is_shell_status(line: &str) -> bool {
    line.starts_with("(exit code ") || line == SHELL_CANCELLED || line == SHELL_KILLED
}

/// Pour a replayed session timeline into a fresh [`Buffer`] so the
/// user sees the prior conversation rendered with the current TUI
/// styling.
///
/// The timeline is the conversation as the user saw it: every message
/// in order, and at each point a compaction ran, its card with the
/// counts and token delta the session file recorded. Nothing a
/// compaction summarized is missing here; the compacted form is what
/// the model reads, not what this renders.
///
/// Each message translates in order: user prompts become user
/// bubbles, assistant text/thinking blocks become their respective
/// streamed-and-finished blocks, tool calls and tool results become
/// the merged tool composites the renderer pairs at draw time. Pass
/// `tool_durations` to recover real durations from session entry
/// timestamps; a call missing from the map shows none. Calls left
/// without a result read as interrupted.
pub fn populate_from_history(
    buf: &mut Buffer,
    timeline: &[TimelineItem],
    tool_durations: &ToolDurations,
) {
    for item in &timeline[tail_start(timeline)..] {
        match item {
            TimelineItem::Compaction(record) => {
                let tokens = match (record.before, record.after) {
                    (Some(before), Some(after)) => Some((before, after)),
                    _ => None,
                };
                let text = compaction_text(
                    Some(CompactionCounts {
                        summarized: record.summarized,
                        kept: record.kept,
                    }),
                    tokens,
                    &record.summary,
                );
                buf.push_custom("kage:compaction", text, true);
            }
            TimelineItem::Message(msg) => push_replayed_message(buf, msg, tool_durations),
        }
    }
    buf.interrupt_running_tools();
}

/// Where a replay starts rendering. The buffer keeps at most
/// [`MAX_BLOCKS`](crate::buffer::MAX_BLOCKS) blocks, so a long session
/// renders only a generous tail of messages instead of building blocks
/// the first trim throws away. The start never lands on a tool result,
/// whose call would be cut off.
fn tail_start(timeline: &[TimelineItem]) -> usize {
    let keep = 2 * crate::buffer::MAX_BLOCKS;
    let mut messages = 0;
    let mut start = timeline.len();
    while start > 0 && messages < keep {
        start -= 1;
        if matches!(timeline[start], TimelineItem::Message(_)) {
            messages += 1;
        }
    }
    while start > 0
        && matches!(&timeline[start], TimelineItem::Message(m) if m.role == Role::ToolResult)
    {
        start -= 1;
    }
    start
}

/// Translate one replayed message into its blocks. The live event path
/// and this share the shapes; the difference is timing, which replay
/// recovers from `tool_durations` instead of live phase changes.
fn push_replayed_message(buf: &mut Buffer, msg: &Arc<Message>, tool_durations: &ToolDurations) {
    match msg.role {
        Role::User => {
            if let Some(text) = user_text(msg) {
                if let Some(run) = kage_core::message::ShellRun::parse(&text) {
                    let body = shell_block(&run.command, &run.output, run.exit_code);
                    buf.push_custom("kage:shell", body, false);
                } else {
                    push_user_text(buf, text);
                }
            }
            push_user_image_rows(buf, msg);
        }
        Role::Assistant => {
            for block in &msg.content {
                match block {
                    Content::Text { text } => {
                        buf.append_assistant_delta(text);
                        buf.finish_streaming();
                    }
                    Content::Thinking {
                        text, duration_ms, ..
                    } if !text.trim().is_empty() => {
                        buf.push_thinking(text.clone(), *duration_ms);
                    }
                    Content::ToolCall { id, name, input } => {
                        buf.push_tool_call(id.to_string(), name, input.clone());
                    }
                    Content::Image { source, mime } => {
                        let bytes = match source {
                            MediaSource::Base64 { data } => data.len() / 4 * 3,
                            MediaSource::Blob { size, .. } => usize::try_from(*size).unwrap_or(0),
                            MediaSource::Url { .. } => 0,
                        };
                        buf.push_custom("kage:image", produced_media_label(mime, bytes), false);
                    }
                    Content::Thinking { .. }
                    | Content::Audio { .. }
                    | Content::ToolResultBlock { .. }
                    | Content::Custom { .. } => {}
                }
            }
        }
        Role::ToolResult => {
            for block in &msg.content {
                if let Content::ToolResultBlock {
                    call_id,
                    output,
                    is_error,
                } = block
                {
                    // Replay: real timing is recovered from the
                    // session's per-entry `ts` deltas via
                    // `tool_durations`. A miss yields `None`,
                    // shown without a duration.
                    let duration = tool_durations.get(&(msg.id, call_id.to_string())).copied();
                    buf.push_tool_result_with_duration(
                        call_id.to_string(),
                        output.clone(),
                        *is_error,
                        duration,
                    );
                }
                if let Content::Image { source, mime } = block {
                    let bytes = match source {
                        MediaSource::Base64 { data } => data.len() / 4 * 3,
                        MediaSource::Blob { size, .. } => usize::try_from(*size).unwrap_or(0),
                        MediaSource::Url { .. } => 0,
                    };
                    buf.push_custom("kage:image", tool_media_label(mime, bytes), false);
                }
            }
        }
        Role::System => {}
    }
}

/// Push the text of a user message: the person's own words as a
/// bubble, then each agent report as a folded report block and each
/// message from another session as a one-line row, then any human
/// words the burst interleaved between them as bubbles.
fn push_user_text(buf: &mut Buffer, text: String) {
    if let Some(note) = engine_note(&text) {
        buf.push_custom("kage:mode", note, false);
        return;
    }
    let Some(burst) = split_agent_text(&text) else {
        buf.push_user(text);
        return;
    };
    if !burst.words.is_empty() {
        buf.push_user(burst.words.to_owned());
    }
    for part in burst.parts {
        match part {
            AgentText::Report(report) => buf.push_custom("kage:agent", report.to_text(), true),
            AgentText::Mail(mail) => buf.push_custom(
                "kage:mail",
                format!("< message from {}: {}", mail.from, mail.body),
                true,
            ),
        }
    }
    for words in burst.prose {
        buf.push_user(words.to_owned());
    }
}

/// What a note the engine adds to the conversation as a user message
/// says, as the quiet line shown in place of a user bubble: plan or
/// swarm mode switching, resumed swarm members reporting back, or a
/// goal message: the goal set for the session, or a goal check
/// sending the model back to work. `None` for anything else.
fn engine_note(text: &str) -> Option<String> {
    const MODES: [(&str, &str); 4] = [
        ("[plan mode on]", "plan mode on"),
        ("[plan mode off]", "plan mode off"),
        ("[swarm mode on]", "swarm mode on"),
        ("[swarm mode off]", "swarm mode off"),
    ];
    let text = text.trim_start();
    if let Some((_, label)) = MODES.iter().find(|(tag, _)| text.starts_with(tag)) {
        return Some((*label).to_owned());
    }
    if text.starts_with("[swarm resume]") {
        return Some("resumed swarm members reported back".to_owned());
    }
    let goal = text.strip_prefix("[goal]")?.trim_start();
    if let Some(set) = goal.strip_prefix("Work toward this goal: ") {
        let first = set.split_inclusive(". ").next().unwrap_or(set).trim();
        return Some(format!("goal: {first}"));
    }
    let first = goal.split_inclusive(". ").next().unwrap_or(goal).trim();
    Some(format!("goal check: {first}"))
}

/// Whether a user message carries words someone typed, rather than
/// only agent reports and messages the engine delivered. Words typed
/// between the elements count too.
pub(crate) fn typed_by_the_user(message: &Message) -> bool {
    user_text(message).is_none_or(|text| {
        engine_note(&text).is_none()
            && split_agent_text(&text)
                .is_none_or(|burst| !burst.words.is_empty() || !burst.prose.is_empty())
    })
}

/// The text of a user message as its bubble shows it: every text
/// block, with each attached resource shortened to one `attached` line.
/// The contents stay in the message the model receives.
fn user_text(message: &Message) -> Option<String> {
    let text: Vec<Cow<'_, str>> = message
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(match resource_block::parse(text) {
                Some(resource) => Cow::Owned(attached_line(&resource)),
                None => Cow::Borrowed(text.as_str()),
            }),
            _ => None,
        })
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

/// `attached <server>:<uri> (<size>)`, with the MIME type in place of
/// the size for an image.
fn attached_line(resource: &ResourceRef) -> String {
    let detail = match &resource.mime {
        Some(mime) if is_image(resource) => mime.clone(),
        _ => crate::image::human_bytes(resource.bytes),
    };
    match &resource.server {
        Some(server) => format!("attached {server}:{} ({detail})", resource.uri),
        None => format!("attached {} ({detail})", resource.uri),
    }
}

/// Whether `resource` stands for an image sent as its own content.
fn is_image(resource: &ResourceRef) -> bool {
    resource.binary
        && resource
            .mime
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"))
}

/// One-line summary of a tool's input: the
/// [`crate::view::tool_view::describe`] target, such as the path for
/// `read` or the command for `shell`.
pub(crate) fn summarize_input(name: &str, input: &serde_json::Value) -> String {
    if matches!(input, serde_json::Value::Null) {
        return String::new();
    }
    let target = crate::view::tool_view::describe(name, input).target;
    crate::view::truncate_to_width(&target, 60, "...")
}

#[cfg(test)]
mod tests {
    use kage_core::sync::lock;
    use kage_core::{LoopError, MessageId, StopReason, TokenUsage, ToolCallId, ToolOutput};
    use serde_json::json;

    use super::*;
    use crate::buffer::Block;

    #[test]
    fn a_produced_image_shows_live_and_on_replay() {
        let rows = |buf: &Buffer| -> Vec<String> {
            buf.blocks()
                .iter()
                .filter_map(|block| match block.as_ref() {
                    Block::Custom { kind, text, .. } if kind == "kage:image" => Some(text.clone()),
                    _ => None,
                })
                .collect()
        };
        let mut live = Buffer::new();
        apply_loop_event(
            &mut live,
            &LoopEvent::Media {
                id: MessageId::new(),
                kind: kage_core::Modality::Image,
                mime: "image/png".into(),
                data: "A".repeat(4000),
            },
        );
        let mut replayed = Buffer::new();
        let message = std::sync::Arc::new(Message::new(
            Role::Assistant,
            vec![Content::Image {
                source: MediaSource::Base64 {
                    data: "A".repeat(4000),
                },
                mime: "image/png".into(),
            }],
            None,
        ));
        push_replayed_message(&mut replayed, &message, &ToolDurations::default());
        assert_eq!(rows(&live), rows(&replayed));
        assert!(
            rows(&live)[0].contains("image/png from the model"),
            "{:?}",
            rows(&live)
        );
    }

    #[test]
    fn a_tool_s_image_shows_after_its_result() {
        let mut buf = Buffer::new();
        apply_loop_event(
            &mut buf,
            &LoopEvent::ToolCallEnd {
                id: ToolCallId::new("c1"),
                output: ToolOutput {
                    text: "drawn".into(),
                    media: vec![kage_core::ToolMedia {
                        mime: "image/png".into(),
                        data: "A".repeat(4000),
                    }],
                    ..ToolOutput::default()
                },
            },
        );
        let last = buf.blocks().last().cloned().unwrap();
        assert!(
            matches!(last.as_ref(), Block::Custom { kind, text, .. }
                if kind == "kage:image" && text.contains("image/png from the tool")),
            "{last:?}"
        );
    }

    #[test]
    fn engine_notes_are_quiet_lines_not_user_bubbles() {
        let mut buf = Buffer::new();
        push_user_text(&mut buf, "[swarm mode on] Split the work early.".into());
        push_user_text(
            &mut buf,
            "[goal] The goal is not met yet: tests pass. Keep working.".into(),
        );
        let lines: Vec<_> = buf
            .blocks()
            .iter()
            .map(|block| match block.as_ref() {
                Block::Custom { kind, text, .. } => (kind.clone(), text.clone()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            lines,
            [
                ("kage:mode".to_owned(), "swarm mode on".to_owned()),
                (
                    "kage:mode".to_owned(),
                    "goal check: The goal is not met yet: tests pass.".to_owned()
                ),
            ]
        );
    }

    fn agent_report_text(name: &str, session: &str, body: &str) -> String {
        format!(
            "<agent name=\"{name}\" session=\"{session}\" state=\"completed\">\n{body}\n</agent>"
        )
    }

    #[test]
    fn a_burst_with_prose_around_reports_renders_blocks_and_keeps_the_prose() {
        let mut buf = Buffer::new();
        let first = agent_report_text("alpha", "01ARZ3NDEKTSV4RRFFQ69G5FAV", "first reply");
        let second = agent_report_text("beta", "01ARZ3NDEKTSV4RRFFQ69G5FAW", "second reply");
        let burst = format!("running these\n{first}\nnotes between\n{second}\nthoughts after");
        push_user_text(&mut buf, burst);
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 5, "{blocks:?}");
        assert!(matches!(blocks[0].as_ref(), Block::User { text } if text == "running these"));
        assert!(matches!(
            blocks[1].as_ref(),
            Block::Custom { kind, text, folded: true, .. }
                if kind == "kage:agent" && text.contains("alpha")
        ));
        assert!(matches!(
            blocks[2].as_ref(),
            Block::Custom { kind, text, folded: true, .. }
                if kind == "kage:agent" && text.contains("beta")
        ));
        assert!(matches!(blocks[3].as_ref(), Block::User { text } if text == "notes between"));
        assert!(matches!(blocks[4].as_ref(), Block::User { text } if text == "thoughts after"));
    }

    #[test]
    fn a_plain_prompt_without_markers_stays_a_user_bubble() {
        let mut buf = Buffer::new();
        push_user_text(&mut buf, "just a plain question".into());
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 1);
        assert!(matches!(
            blocks[0].as_ref(),
            Block::User { text } if text == "just a plain question"
        ));
    }

    #[test]
    fn prose_between_reports_counts_as_typed_but_a_pure_burst_does_not() {
        let first = agent_report_text("alpha", "01ARZ3NDEKTSV4RRFFQ69G5FAV", "first reply");
        let second = agent_report_text("beta", "01ARZ3NDEKTSV4RRFFQ69G5FAW", "second reply");
        let typed = |text: String| {
            typed_by_the_user(&Message::new(
                Role::User,
                vec![Content::Text { text }],
                None,
            ))
        };
        assert!(typed(format!("{first}\nnotes between\n{second}")));
        assert!(!typed(format!("{first}\n\n{second}")));
        assert!(typed("plain words".into()));
    }

    #[test]
    fn a_shell_block_ends_with_a_clear_status() {
        assert_eq!(
            shell_block("ls", "a.rs\n", Some(0)),
            "$ ls\na.rs\n(exit code 0)"
        );
        assert_eq!(
            shell_block("sleep 9", "tick\ncancelled", None),
            "$ sleep 9\ntick\n(cancelled)"
        );
        assert_eq!(
            shell_block("sleep 9", "cancelled", None),
            "$ sleep 9\n(cancelled)"
        );
        assert_eq!(
            shell_block("yes", "not cancelled", None),
            "$ yes\nnot cancelled\n(killed by a signal)"
        );
        for status in ["(exit code 0)", "(cancelled)", "(killed by a signal)"] {
            assert!(is_shell_status(status));
        }
        assert!(!is_shell_status("cancelled"));
    }

    struct Apply(SharedBuffer);

    impl Apply {
        fn on_event(&mut self, event: &LoopEvent) {
            apply_loop_event(&mut lock(&self.0), event);
        }
    }

    fn fresh() -> (SharedBuffer, Apply) {
        let buf = shared_buffer();
        (buf.clone(), Apply(buf))
    }

    fn id() -> MessageId {
        MessageId::new()
    }

    #[test]
    fn summarize_read_returns_just_the_path() {
        assert_eq!(
            summarize_input("read", &json!({"path": "README.md"})),
            "README.md"
        );
    }

    #[test]
    fn summarize_edit_is_the_path() {
        assert_eq!(
            summarize_input("edit", &json!({"path":"a.rs","old_str":"x","new_str":"y"})),
            "a.rs"
        );
    }

    #[test]
    fn summarize_grep_combines_pattern_and_path() {
        assert_eq!(
            summarize_input("grep", &json!({"pattern": "foo", "path": "src"})),
            "\"foo\" in src"
        );
        assert_eq!(
            summarize_input("grep", &json!({"pattern": "foo", "path": "."})),
            "\"foo\""
        );
    }

    #[test]
    fn summarize_shell_uses_command_field() {
        assert_eq!(
            summarize_input("shell", &json!({"command": "ls -la"})),
            "ls -la"
        );
    }

    #[test]
    fn summarize_unknown_tool_is_its_name() {
        assert_eq!(
            summarize_input("custom_tool", &json!({"foo": "bar"})),
            "custom_tool"
        );
    }

    #[test]
    fn summarize_truncates_long_summaries() {
        let path = "a".repeat(80);
        let out = summarize_input("read", &json!({"path": path}));
        assert!(out.ends_with("..."));
        assert!(out.chars().count() <= 60);
    }

    #[test]
    fn text_delta_appends_to_assistant_block() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::MessageStart { id: id() });
        hooks.on_event(&LoopEvent::TextDelta {
            id: id(),
            delta: "hello ".into(),
        });
        hooks.on_event(&LoopEvent::TextDelta {
            id: id(),
            delta: "world".into(),
        });
        hooks.on_event(&LoopEvent::MessageEnd {
            id: id(),
            usage: TokenUsage::default(),
            stop_reason: StopReason::EndTurn,
        });
        let buf = buf.lock().unwrap();
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 1);
        match blocks[0].as_ref() {
            Block::Assistant { text, live } => {
                assert_eq!(text, "hello world");
                assert!(!*live);
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    #[test]
    fn max_tokens_message_end_pushes_truncated_notice() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::TextDelta {
            id: id(),
            delta: "partial ans".into(),
        });
        hooks.on_event(&LoopEvent::MessageEnd {
            id: id(),
            usage: TokenUsage::default(),
            stop_reason: StopReason::MaxTokens,
        });
        let buf = buf.lock().unwrap();
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 2);
        match blocks[0].as_ref() {
            Block::Assistant { text, live } => {
                assert_eq!(text, "partial ans");
                assert!(!live, "assistant block must be finished");
            }
            other => panic!("expected assistant, got {other:?}"),
        }
        match blocks[1].as_ref() {
            Block::Custom { kind, text, folded } => {
                assert_eq!(kind, "kage:truncated");
                assert_eq!(text, "reply hit the max output token limit");
                assert!(!folded);
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn thinking_delta_appends_to_separate_block() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::ThinkingDelta {
            id: id(),
            delta: "let me think".into(),
        });
        hooks.on_event(&LoopEvent::TextDelta {
            id: id(),
            delta: "ok".into(),
        });
        let buf = buf.lock().unwrap();
        assert_eq!(buf.blocks().len(), 2);
        assert!(matches!(buf.blocks()[0].as_ref(), Block::Thinking { .. }));
        assert!(matches!(buf.blocks()[1].as_ref(), Block::Assistant { .. }));
    }

    #[test]
    fn tool_call_and_result_pair_into_blocks() {
        let (buf, mut hooks) = fresh();
        let cid = ToolCallId::new("c1");
        hooks.on_event(&LoopEvent::ToolCallStart {
            id: cid.clone(),
            name: "shell".into(),
            input_partial: json!({"cmd": "ls"}),
        });
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: cid,
            output: ToolOutput {
                is_error: false,
                text: "file1\nfile2".into(),
                structured: None,
                terminate: false,
                media: Vec::new(),
            },
        });
        let buf = buf.lock().unwrap();
        assert_eq!(buf.blocks().len(), 2);
        match buf.blocks()[0].as_ref() {
            Block::ToolCall { name, .. } => assert_eq!(name, "shell"),
            other => panic!("expected ToolCall, got {other:?}"),
        }
        match buf.blocks()[1].as_ref() {
            Block::ToolResult {
                output, is_error, ..
            } => {
                assert_eq!(output, "file1\nfile2");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    fn shell_output(text: &str, is_error: bool) -> ToolOutput {
        ToolOutput {
            is_error,
            text: text.into(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        }
    }

    #[test]
    fn tool_events_walk_the_phases() {
        let (buf, mut hooks) = fresh();
        let cid = ToolCallId::new("c1");
        let phase = |buf: &SharedBuffer| match lock(buf).blocks()[0].as_ref() {
            Block::ToolCall { phase, .. } => *phase,
            other => panic!("expected ToolCall, got {other:?}"),
        };
        hooks.on_event(&LoopEvent::ToolCallArgsDelta {
            id: cid.clone(),
            name: "shell".into(),
            input_partial: json!({}),
        });
        assert_eq!(phase(&buf), ToolPhase::Streaming);
        hooks.on_event(&LoopEvent::ToolCallStart {
            id: cid.clone(),
            name: "shell".into(),
            input_partial: json!({"command": "make"}),
        });
        assert_eq!(phase(&buf), ToolPhase::Queued);
        hooks.on_event(&LoopEvent::ToolExecutionStart { id: cid.clone() });
        assert_eq!(phase(&buf), ToolPhase::Running);
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: cid,
            output: shell_output("stderr:\nno\nexit: 2", true),
        });
        assert_eq!(phase(&buf), ToolPhase::Failed);
    }

    fn bash_start(id: &str, command: &str) -> LoopEvent {
        LoopEvent::ToolCallStart {
            id: ToolCallId::new(id),
            name: "shell".into(),
            input_partial: json!({ "command": command }),
        }
    }

    fn phases(buf: &SharedBuffer) -> Vec<ToolPhase> {
        phases_of(&lock(buf))
    }

    #[test]
    fn only_the_executing_call_runs_and_its_time_excludes_the_queue() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&bash_start("c1", "echo one"));
        hooks.on_event(&bash_start("c2", "echo two"));
        assert_eq!(phases(&buf), [ToolPhase::Queued, ToolPhase::Queued]);

        std::thread::sleep(std::time::Duration::from_millis(80));
        hooks.on_event(&LoopEvent::ToolExecutionStart {
            id: ToolCallId::new("c1"),
        });
        assert_eq!(phases(&buf), [ToolPhase::Running, ToolPhase::Queued]);
        assert!(!lock(&buf).is_timed(1), "a queued call has no timer");

        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: ToolCallId::new("c1"),
            output: shell_output("stdout:\none\nexit: 0", false),
        });
        let durations: Vec<Option<u64>> = lock(&buf)
            .blocks()
            .iter()
            .filter_map(|b| match b.as_ref() {
                Block::ToolResult { duration_ms, .. } => Some(*duration_ms),
                _ => None,
            })
            .collect();
        assert!(
            matches!(durations[..], [Some(ms)] if ms < 80),
            "{durations:?}"
        );
    }

    #[test]
    fn a_cancelled_call_reads_interrupted_and_keeps_its_progress() {
        let (buf, mut hooks) = fresh();
        let cid = ToolCallId::new("c1");
        hooks.on_event(&bash_start("c1", "for i in 1 2 3"));
        hooks.on_event(&LoopEvent::ToolExecutionStart { id: cid.clone() });
        hooks.on_event(&LoopEvent::ToolUpdate {
            id: cid.clone(),
            update: kage_core::ToolUpdate {
                content: "1\n2".into(),
                structured: None,
            },
        });
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: cid,
            output: shell_output(kage_core::event::TOOL_CANCELLED_TEXT, true),
        });
        assert_eq!(phases(&buf), [ToolPhase::Interrupted]);
        let mut buf = lock(&buf);
        let topo = buf.tool_topology();
        let registry = kage_core::sync::read(crate::view::registry::global());
        let lines = crate::view::build_block_lines(
            &buf,
            0,
            60,
            &topo,
            crate::view::Emphasis::None,
            &registry,
            None,
        );
        let rows: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(rows[0].contains("\u{2298} Run for i in 1 2 3"), "{rows:?}");
        assert!(rows[0].trim_end().ends_with("interrupted"), "{rows:?}");
        assert!(rows.iter().any(|r| r.trim_end().ends_with('2')), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("cancelled")), "{rows:?}");
    }

    #[test]
    fn a_cancelled_approval_reads_interrupted_without_a_duration() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&bash_start("c1", "ls"));
        lock(&buf).set_tool_phase("c1", ToolPhase::Waiting);
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: ToolCallId::new("c1"),
            output: shell_output("`bash`: permission prompt cancelled", true),
        });
        assert_eq!(phases(&buf), [ToolPhase::Interrupted]);
        assert!(matches!(
            lock(&buf).blocks()[1].as_ref(),
            Block::ToolResult {
                duration_ms: None,
                ..
            }
        ));
    }

    #[test]
    fn a_reused_call_id_opens_a_new_block() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&bash_start("call_0", "echo first"));
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: ToolCallId::new("call_0"),
            output: shell_output("stdout:\nfirst\nexit: 0", false),
        });
        hooks.on_event(&LoopEvent::MessageAppended {
            message: Arc::new(Message::new(
                Role::User,
                vec![Content::Text {
                    text: "again".into(),
                }],
                None,
            )),
        });
        hooks.on_event(&LoopEvent::ToolCallArgsDelta {
            id: ToolCallId::new("call_0"),
            name: "shell".into(),
            input_partial: json!({}),
        });
        hooks.on_event(&bash_start("call_0", "echo second"));
        hooks.on_event(&LoopEvent::ToolExecutionStart {
            id: ToolCallId::new("call_0"),
        });
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: ToolCallId::new("call_0"),
            output: shell_output("stdout:\nsecond\nexit: 0", false),
        });
        let mut buf = lock(&buf);
        let commands: Vec<String> = buf
            .blocks()
            .iter()
            .filter_map(|b| match b.as_ref() {
                Block::ToolCall { input, .. } => Some(input["command"].to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(commands, ["\"echo first\"", "\"echo second\""]);
        assert_eq!(phases_of(&buf), [ToolPhase::Done, ToolPhase::Done]);
        let topo = buf.tool_topology();
        assert_eq!(topo.result_of_call.get(&0), Some(&1));
        assert_eq!(topo.result_of_call.get(&3), Some(&4));
    }

    fn phases_of(buf: &Buffer) -> Vec<ToolPhase> {
        buf.blocks()
            .iter()
            .filter_map(|b| match b.as_ref() {
                Block::ToolCall { phase, .. } => Some(*phase),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn tool_updates_show_while_running_and_the_result_replaces_them() {
        let (buf, mut hooks) = fresh();
        let cid = ToolCallId::new("c1");
        hooks.on_event(&LoopEvent::ToolCallStart {
            id: cid.clone(),
            name: "shell".into(),
            input_partial: json!({"command": "make"}),
        });
        for content in ["compiling a", "compiling a\ncompiling b"] {
            hooks.on_event(&LoopEvent::ToolUpdate {
                id: cid.clone(),
                update: kage_core::ToolUpdate {
                    content: content.into(),
                    structured: None,
                },
            });
        }
        let rendered = |buf: &SharedBuffer| {
            let mut buf = lock(buf);
            let backend = ratatui::backend::TestBackend::new(60, 12);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let input = crate::input::InputState::new();
            terminal
                .draw(|frame| {
                    let regions = crate::layout::split(
                        frame.area(),
                        crate::layout::Heights {
                            header: 1,
                            input: crate::layout::INPUT_MIN_LINES,
                            ..crate::layout::Heights::default()
                        },
                    );
                    crate::view::render(
                        frame,
                        regions,
                        &mut buf,
                        &input,
                        None,
                        &crate::view::StatusCtx::default(),
                        None,
                        &mut std::collections::BTreeMap::new(),
                        None,
                        &[],
                    );
                })
                .unwrap();
            let screen = terminal.backend().buffer().clone();
            (0..screen.area.height)
                .map(|y| {
                    (0..screen.area.width)
                        .map(|x| screen[(x, y)].symbol().to_owned())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let live = rendered(&buf);
        assert!(
            live.contains("compiling a") && live.contains("compiling b"),
            "{live}"
        );
        assert_eq!(
            live.matches("compiling a").count(),
            1,
            "replaced, not appended: {live}"
        );
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: cid,
            output: shell_output("stdout:\nall done\nexit: 0", false),
        });
        let done = rendered(&buf);
        assert!(
            done.contains("all done") && !done.contains("compiling"),
            "{done}"
        );
    }

    #[test]
    fn consecutive_provider_retries_replace_one_notice() {
        let (buf, mut hooks) = fresh();
        for attempt in 1..=3 {
            hooks.on_event(&LoopEvent::ProviderRetry {
                attempt,
                max_attempts: 5,
                wait_secs: 1,
                wait_ms: 1000,
                requested_secs: None,
                error: "overloaded".into(),
            });
        }
        let buf = lock(&buf);
        assert_eq!(buf.blocks().len(), 1);
        assert!(matches!(
            buf.blocks()[0].as_ref(),
            Block::Custom { text, .. } if text.contains("retrying 3/5")
        ));
    }

    #[test]
    fn text_after_thinking_finishes_the_thinking_block() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::ThinkingDelta {
            id: id(),
            delta: "hmm".into(),
        });
        hooks.on_event(&LoopEvent::TextDelta {
            id: id(),
            delta: "ok".into(),
        });
        let buf = lock(&buf);
        assert!(matches!(
            buf.blocks()[0].as_ref(),
            Block::Thinking {
                live: false,
                folded: true,
                duration_ms: Some(_),
                ..
            }
        ));
        assert!(!buf.is_timed(0));
    }

    /// Wrap plain messages into the timeline shape the replay carries.
    fn timeline_of(messages: &[Arc<Message>]) -> Vec<TimelineItem> {
        messages
            .iter()
            .map(|m| TimelineItem::Message(Arc::clone(m)))
            .collect()
    }

    fn card(summarized: usize, kept: usize, tokens: (Option<u64>, Option<u64>)) -> TimelineItem {
        TimelineItem::Compaction(kage_core::protocol::CompactionRecord {
            summarized,
            kept,
            before: tokens.0,
            after: tokens.1,
            summary: format!(
                "{}summary{}",
                kage_core::message::COMPACTION_SUMMARY_PREFIX,
                kage_core::message::COMPACTION_SUMMARY_SUFFIX
            ),
        })
    }

    #[test]
    fn replayed_thinking_keeps_its_stored_timing_and_orphan_calls_are_interrupted() {
        let mut buf = Buffer::new();
        let history = vec![Arc::new(Message::new(
            Role::Assistant,
            vec![
                Content::Thinking {
                    text: "plan".into(),
                    signature: None,
                    duration_ms: None,
                },
                Content::Thinking {
                    text: "more".into(),
                    signature: None,
                    duration_ms: Some(2_300),
                },
                Content::ToolCall {
                    id: ToolCallId::new("c1"),
                    name: "shell".into(),
                    input: json!({"command": "ls"}),
                },
            ],
            None,
        ))];
        populate_from_history(&mut buf, &timeline_of(&history), &HashMap::new());
        assert!(matches!(
            buf.blocks()[0].as_ref(),
            Block::Thinking {
                duration_ms: None,
                folded: true,
                ..
            }
        ));
        assert!(matches!(
            buf.blocks()[1].as_ref(),
            Block::Thinking {
                duration_ms: Some(2_300),
                folded: true,
                ..
            }
        ));
        assert!(matches!(
            buf.blocks()[2].as_ref(),
            Block::ToolCall {
                phase: ToolPhase::Interrupted,
                ..
            }
        ));
    }

    #[test]
    fn compaction_pushes_custom_block_with_summary() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::Compaction {
            kept: 4,
            summarized: 12,
            summary: "everyone agrees".into(),
            before: 84_000,
            estimate: 0,
        });
        let buf = buf.lock().unwrap();
        match buf.blocks()[0].as_ref() {
            Block::Custom { kind, text, .. } => {
                assert_eq!(kind, "kage:compaction");
                assert!(text.starts_with("Compacted history (12 messages summarized, 4 kept)"));
                assert!(text.contains("everyone agrees"));
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn compaction_usage_amends_the_newest_bare_card_header() {
        let mut buf = Buffer::new();
        buf.push_custom(
            "kage:compaction",
            "Compacted history (12 messages summarized, 4 kept)\n<summary>\nbody\n</summary>",
            true,
        );
        assert!(amend_compaction_with_usage(&mut buf, 84_000, 12_000));
        match buf.blocks()[0].as_ref() {
            Block::Custom { text, .. } => assert!(
                text.starts_with("Compacted history (12 messages summarized, 4 kept) \u{b7} 84k \u{2192} 12k tokens\n"),
                "{text}"
            ),
            other => panic!("expected Custom, got {other:?}"),
        }
        assert!(!amend_compaction_with_usage(&mut buf, 1, 2));
    }

    #[test]
    fn compaction_usage_amends_only_the_card_without_a_delta() {
        let mut buf = Buffer::new();
        buf.push_custom(
            "kage:compaction",
            "Compacted history (9 summarized, 4 kept) \u{b7} 50k \u{2192} 8k tokens\nold",
            true,
        );
        buf.push_custom(
            "kage:compaction",
            "Compacted history (3 summarized, 4 kept)\nnew",
            true,
        );
        assert!(amend_compaction_with_usage(&mut buf, 20_000, 5_000));
        match buf.blocks()[0].as_ref() {
            Block::Custom { text, .. } => {
                assert!(text.contains("50k \u{2192} 8k tokens"), "{text}");
                assert!(!text.contains("20k"), "{text}");
            }
            other => panic!("expected Custom, got {other:?}"),
        }
        match buf.blocks()[1].as_ref() {
            Block::Custom { text, .. } => {
                assert!(
                    text.starts_with(
                        "Compacted history (3 summarized, 4 kept) \u{b7} 20k \u{2192} 5k tokens\n"
                    ),
                    "{text}"
                );
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn error_pushes_custom_unfolded_error_block() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::Error {
            kind: LoopError::ContextOverflow,
        });
        let buf = buf.lock().unwrap();
        match buf.blocks()[0].as_ref() {
            Block::Custom { kind, folded, .. } => {
                assert_eq!(kind, "kage:error");
                assert!(!folded, "errors should be visible by default");
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn cancel_is_a_quiet_notice_and_finishes_the_live_reply() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::TextDelta {
            id: id(),
            delta: "half an ans".into(),
        });
        hooks.on_event(&LoopEvent::Error {
            kind: LoopError::Cancelled,
        });
        let buf = buf.lock().unwrap();
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 2);
        assert!(matches!(
            blocks[0].as_ref(),
            Block::Assistant { live: false, .. }
        ));
        match blocks[1].as_ref() {
            Block::Custom { kind, text, .. } => {
                assert_eq!(kind, "kage:notify");
                assert_eq!(text, "Interrupted");
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn auth_error_points_at_login() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::Error {
            kind: LoopError::Auth {
                message: "token expired.".into(),
            },
        });
        let buf = buf.lock().unwrap();
        match buf.blocks()[0].as_ref() {
            Block::Custom { kind, text, .. } => {
                assert_eq!(kind, "kage:error");
                assert_eq!(
                    text,
                    "authentication failed: token expired. Run /login to re-authenticate."
                );
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn populate_from_history_translates_user_assistant_and_tools() {
        use kage_core::ToolCallId;
        let mut buf = Buffer::new();
        let history = vec![
            Arc::new(Message::new(
                Role::User,
                vec![Content::Text {
                    text: "list files".into(),
                }],
                None,
            )),
            Arc::new(Message::new(
                Role::Assistant,
                vec![
                    Content::Thinking {
                        text: "use ls".into(),
                        signature: None,
                        duration_ms: None,
                    },
                    Content::Text {
                        text: "looking now".into(),
                    },
                    Content::ToolCall {
                        id: ToolCallId::new("c1"),
                        name: "ls".into(),
                        input: json!({"path": "."}),
                    },
                ],
                None,
            )),
            Arc::new(Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: ToolCallId::new("c1"),
                    output: "a.rs\nb.rs".into(),
                    is_error: false,
                }],
                None,
            )),
        ];
        populate_from_history(
            &mut buf,
            &timeline_of(&history),
            &std::collections::HashMap::new(),
        );
        let blocks = buf.blocks();
        assert!(matches!(blocks[0].as_ref(), Block::User { .. }));
        assert!(matches!(blocks[1].as_ref(), Block::Thinking { .. }));
        assert!(matches!(blocks[2].as_ref(), Block::Assistant { .. }));
        assert!(matches!(
            blocks[3].as_ref(),
            Block::ToolCall { name, .. } if name == "ls"
        ));
        assert!(matches!(
            blocks[4].as_ref(),
            Block::ToolResult { output, .. } if output == "a.rs\nb.rs"
        ));
    }

    /// A long replay builds only the tail the buffer keeps, starting on
    /// a call rather than on the result it answers.
    #[test]
    fn a_long_replay_renders_only_its_tail() {
        let mut history = Vec::new();
        for n in 0..3_000 {
            let id = ToolCallId::new(format!("c{n}"));
            history.push(Arc::new(Message::new(
                Role::Assistant,
                vec![Content::ToolCall {
                    id: id.clone(),
                    name: "ls".into(),
                    input: serde_json::json!({}),
                }],
                None,
            )));
            history.push(Arc::new(Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: id,
                    output: format!("out {n}"),
                    is_error: false,
                }],
                None,
            )));
        }
        let timeline = timeline_of(&history);
        let start = tail_start(&timeline);
        assert!(timeline.len() - start <= 2 * crate::buffer::MAX_BLOCKS + 1);
        assert!(timeline.len() - start >= 2 * crate::buffer::MAX_BLOCKS);
        assert!(matches!(&timeline[start], TimelineItem::Message(m) if m.role == Role::Assistant));

        let mut buf = Buffer::new();
        populate_from_history(&mut buf, &timeline, &std::collections::HashMap::new());
        assert!(matches!(
            buf.blocks().last().map(AsRef::as_ref),
            Some(Block::ToolResult { output, .. }) if output == "out 2999"
        ));
        assert!(buf.blocks().len() <= 2 * crate::buffer::MAX_BLOCKS + 1);
        assert_eq!(
            tail_start(&timeline[..10]),
            0,
            "a short replay renders whole"
        );
    }

    #[test]
    fn populate_renders_a_timeline_card_with_counts_and_tokens() {
        let mut buf = Buffer::new();
        let timeline = vec![card(4, 2, (Some(160_000), Some(42_000)))];
        populate_from_history(&mut buf, &timeline, &std::collections::HashMap::new());
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 1);
        match blocks[0].as_ref() {
            Block::Custom { kind, text, .. } => {
                assert_eq!(kind, "kage:compaction");
                assert!(
                    text.starts_with(
                        "Compacted history (4 messages summarized, 2 kept) \u{b7} 160k \u{2192} 42k tokens\n"
                    ),
                    "{text}"
                );
            }
            other => panic!("expected Custom compaction block, got {other:?}"),
        }
    }

    #[test]
    fn a_replayed_card_without_tokens_shows_counts_only() {
        let header = |tokens| {
            let mut buf = Buffer::new();
            let timeline = vec![card(4, 2, tokens)];
            populate_from_history(&mut buf, &timeline, &HashMap::new());
            match buf.blocks()[0].as_ref() {
                Block::Custom { text, .. } => text.lines().next().unwrap().to_owned(),
                other => panic!("expected a compaction block, got {other:?}"),
            }
        };
        assert_eq!(
            header((None, None)),
            "Compacted history (4 messages summarized, 2 kept)",
            "a session recorded before tokens were kept"
        );
        assert_eq!(
            header((Some(160_000), None)),
            "Compacted history (4 messages summarized, 2 kept)",
            "a one-sided delta reads as none"
        );
    }

    /// The user's view of a compacted session: the summarized turns
    /// stay visible, and the card sits where the compaction ran, after
    /// the turns that existed when it ran.
    #[test]
    fn a_replayed_timeline_keeps_every_turn_and_the_card_in_place() {
        let old_user = Arc::new(Message::new(
            Role::User,
            vec![Content::Text {
                text: "old ask".into(),
            }],
            None,
        ));
        let kept_user = Arc::new(Message::new(
            Role::User,
            vec![Content::Text {
                text: "kept ask".into(),
            }],
            None,
        ));
        let mut buf = Buffer::new();
        let timeline = vec![
            TimelineItem::Message(Arc::clone(&old_user)),
            TimelineItem::Message(Arc::clone(&kept_user)),
            card(1, 1, (Some(84_000), Some(12_000))),
        ];
        populate_from_history(&mut buf, &timeline, &HashMap::new());
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 3, "{blocks:?}");
        match blocks[0].as_ref() {
            Block::User { .. } => {}
            other => panic!("the summarized turn stays visible, got {other:?}"),
        }
        match blocks[1].as_ref() {
            Block::User { .. } => {}
            other => panic!("the kept turn stays visible, got {other:?}"),
        }
        match blocks[2].as_ref() {
            Block::Custom { kind, text, .. } => {
                assert_eq!(kind, "kage:compaction");
                assert!(text.contains("84k \u{2192} 12k tokens"), "{text}");
            }
            other => panic!("expected the card where the compaction ran, got {other:?}"),
        }
    }

    /// Cards come only from timeline entries: a message that happens
    /// to carry the framing (a typed or replayed oddity) stays a user
    /// bubble, never a fabricated card.
    #[test]
    fn framed_text_in_a_timeline_message_stays_a_user_bubble() {
        let framed = format!(
            "{}the real summary{}",
            kage_core::message::COMPACTION_SUMMARY_PREFIX,
            kage_core::message::COMPACTION_SUMMARY_SUFFIX
        );
        let history = vec![
            Arc::new(Message::new(
                Role::User,
                vec![Content::Text {
                    text: "wrap it in <summary>x</summary> please".into(),
                }],
                None,
            )),
            Arc::new(Message::new(
                Role::User,
                vec![Content::Text { text: framed }],
                None,
            )),
        ];
        let mut buf = Buffer::new();
        populate_from_history(&mut buf, &timeline_of(&history), &HashMap::new());
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[0].as_ref(), Block::User { .. }));
        assert!(matches!(blocks[1].as_ref(), Block::User { .. }));
    }

    #[test]
    fn populate_keeps_regular_user_message_as_user_block() {
        let mut buf = Buffer::new();
        let history = vec![Arc::new(Message::new(
            Role::User,
            vec![Content::Text {
                text: "just a normal message".into(),
            }],
            None,
        ))];
        populate_from_history(
            &mut buf,
            &timeline_of(&history),
            &std::collections::HashMap::new(),
        );
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 1);
        assert!(matches!(blocks[0].as_ref(), Block::User { .. }));
    }

    #[test]
    fn resource_blocks_show_as_attached_lines_live_and_on_replay() {
        let message = Message::new(
            Role::User,
            vec![
                Content::Text {
                    text: "compare @docs:file:///a and the context".into(),
                },
                Content::Text {
                    text: resource_block::render("file:///a", Some("docs"), None, "secret body"),
                },
                Content::Text {
                    text: resource_block::render("file:///b.rs", None, Some("text/x-rust"), ""),
                },
            ],
            None,
        );
        let want = "compare @docs:file:///a and the context\n\
                    attached docs:file:///a (11 B)\n\
                    attached file:///b.rs (0 B)";
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::MessageAppended {
            message: Arc::new(message.clone()),
        });
        assert!(matches!(lock(&buf).blocks()[0].as_ref(), Block::User { text } if text == want));
        let mut replayed = Buffer::new();
        populate_from_history(
            &mut replayed,
            &[TimelineItem::Message(Arc::new(message))],
            &HashMap::new(),
        );
        assert!(matches!(replayed.blocks()[0].as_ref(), Block::User { text } if text == want));
    }

    #[test]
    fn binary_and_image_resources_show_as_attached_lines_live_and_on_replay() {
        let message = Message::new(
            Role::User,
            vec![
                Content::Text {
                    text: "see @fix:test://img and @fix:test://bin".into(),
                },
                Content::Text {
                    text: resource_block::render_binary("test://img", Some("fix"), "image/png", 70),
                },
                Content::Image {
                    source: kage_core::MediaSource::Base64 { data: "AA".into() },
                    mime: "image/png".into(),
                },
                Content::Text {
                    text: resource_block::render_binary(
                        "test://bin",
                        Some("fix"),
                        "application/octet-stream",
                        2048,
                    ),
                },
            ],
            None,
        );
        let want = "see @fix:test://img and @fix:test://bin\n\
                    attached fix:test://img (image/png)\n\
                    attached fix:test://bin (2 KB)";
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::MessageAppended {
            message: Arc::new(message.clone()),
        });
        let blocks = lock(&buf).blocks().to_vec();
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        assert!(matches!(blocks[0].as_ref(), Block::User { text } if text == want));
        let mut replayed = Buffer::new();
        populate_from_history(
            &mut replayed,
            &[TimelineItem::Message(Arc::new(message))],
            &HashMap::new(),
        );
        assert!(matches!(replayed.blocks()[0].as_ref(), Block::User { text } if text == want));
    }

    #[test]
    fn appended_user_messages_paint_text_then_images() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::MessageAppended {
            message: Arc::new(Message::new(
                Role::User,
                vec![
                    Content::Text {
                        text: "hello".into(),
                    },
                    Content::Image {
                        source: kage_core::MediaSource::Base64 { data: "AA".into() },
                        mime: "image/png".into(),
                    },
                ],
                None,
            )),
        });
        hooks.on_event(&LoopEvent::MessageAppended {
            message: Arc::new(Message::new(Role::Assistant, Vec::new(), None)),
        });
        let buf = buf.lock().unwrap();
        assert_eq!(buf.blocks().len(), 2);
        assert!(matches!(buf.blocks()[0].as_ref(), Block::User { text } if text == "hello"));
    }

    #[test]
    fn replayed_user_messages_paint_text_then_images() {
        let history = vec![Arc::new(Message::new(
            Role::User,
            vec![
                Content::Text {
                    text: "hello".into(),
                },
                Content::Image {
                    source: kage_core::MediaSource::Base64 { data: "AA".into() },
                    mime: "image/png".into(),
                },
            ],
            None,
        ))];
        let mut buf = Buffer::new();
        populate_from_history(&mut buf, &timeline_of(&history), &HashMap::new());
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 2, "bubble then image row");
        assert!(matches!(blocks[0].as_ref(), Block::User { text } if text == "hello"));
        match blocks[1].as_ref() {
            Block::Custom { kind, text, .. } => {
                assert_eq!(kind, "kage:image");
                assert_eq!(text, "[image: image/png]");
            }
            other => panic!("expected an image row, got {other:?}"),
        }
    }

    #[test]
    fn tool_durations_come_from_message_timestamps() {
        let call = Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId::new("c1"),
                name: "shell".into(),
                input: json!({}),
            }],
            None,
        );
        let mut result = Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new("c1"),
                output: String::new(),
                is_error: false,
            }],
            None,
        );
        result.ts = call.ts + chrono::Duration::milliseconds(250);
        let key = (result.id, "c1".to_owned());
        assert_eq!(
            tool_durations(&[Arc::new(call), Arc::new(result)])[&key],
            250
        );
    }

    #[test]
    fn a_call_id_reused_in_a_later_turn_keeps_each_duration() {
        let turn = |ms: i64| {
            let call = Message::new(
                Role::Assistant,
                vec![Content::ToolCall {
                    id: ToolCallId::new("call_0"),
                    name: "agent".into(),
                    input: json!({}),
                }],
                None,
            );
            let mut result = Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: ToolCallId::new("call_0"),
                    output: "ok".into(),
                    is_error: false,
                }],
                None,
            );
            result.ts = call.ts + chrono::Duration::milliseconds(ms);
            [call, result]
        };
        let history: Vec<_> = turn(8_000)
            .into_iter()
            .chain(turn(2_000))
            .map(Arc::new)
            .collect();
        let mut buf = Buffer::new();
        populate_from_history(&mut buf, &timeline_of(&history), &tool_durations(&history));
        let durations: Vec<Option<u64>> = buf
            .blocks()
            .iter()
            .filter_map(|b| match b.as_ref() {
                Block::ToolResult { duration_ms, .. } => Some(*duration_ms),
                _ => None,
            })
            .collect();
        assert_eq!(durations, [Some(8_000), Some(2_000)]);
    }
}
