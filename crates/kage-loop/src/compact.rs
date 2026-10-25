//! History compaction.
//!
//! When the running token budget approaches the model's context window
//! (or the remaining room can no longer hold a full reply) the loop
//! summarizes the oldest turns into one synthetic user message wrapped
//! in `<summary>...</summary>` framing and drops the originals. The
//! framing plus user role keeps the post-compaction history valid for
//! providers like ZAI/GLM and Anthropic that require strict role
//! ordering.
//!
//! A summarizer failure never corrupts the conversation: the history is
//! left intact and the failure surfaces as
//! [`LoopError::Compaction`](kage_core::LoopError::Compaction). An
//! automatic compaction warns and lets the run continue; a forced or
//! overflow-recovery one propagates the error to its caller.
//!
//! The synthetic message is a handoff brief: the model's summary of the
//! work, the user's own messages from the summarized region preserved
//! verbatim under token caps (head, elision note, tail), and - for an
//! automatic mid-run compaction - a note to continue the work in
//! progress. The framing constants stay the outermost wrapper so
//! session replay and the UIs keep parsing every compaction body ever
//! stored.
//!
//! The summarization is itself a model call: the prior turns are
//! serialized into a plain-text transcript and sent as one user-role
//! prompt (so the request never violates ordering rules), with a
//! handoff-brief instruction as the system prompt. A summarizer call
//! rejected for size retries with the transcript's oldest section
//! dropped, and a turn rejected for size after a compaction is answered
//! by compacting harder (a smaller keep window each round) before the
//! run gives up with [`LoopError::ContextOverflow`].

use std::fmt::Write as _;
use std::sync::Arc;

use kage_core::{CancelFlag, Content, LoopError, LoopEvent, Message, MessageId, Role};
use kage_provider::{Provider, ProviderEvent, StreamRequest};

use crate::{AgentContext, CompactionPrep, Hooks, LoopConfig, TokenBudget};

/// Target number of recent turns kept verbatim. Older turns are
/// summarized. The boundary may extend past this to keep an assistant
/// tool-call turn together with its results.
pub(crate) const KEEP_RECENT: usize = 4;

/// How many times a provider-rejected turn may be answered by
/// compacting harder before the loop gives up with
/// [`LoopError::ContextOverflow`]. Each round halves the keep window,
/// so the last attempt summarizes the entire history.
pub(crate) const MAX_OVERFLOW_COMPACTIONS: u32 = 3;

/// Total attempts the summarizer makes before its overflow error
/// propagates. Between attempts the transcript's oldest section is
/// dropped, so a rejected summary call can still succeed smaller.
const SUMMARIZE_ATTEMPTS: u32 = 3;

/// Output tokens reserved for the model's reply: compaction also
/// triggers when the live context plus this reserve reaches the
/// window, so the output ceiling is not squeezed to a short reply.
/// Capped at what the model can emit, so a small output limit does
/// not compact early to keep room it can never use.
const OUTPUT_RESERVE: u64 = 50_000;

/// Smallest reply room a request asks for, on top of any thinking
/// budget. When the window cannot fit even this, the request goes out
/// anyway and a provider rejection triggers recovery compaction.
const MIN_REPLY_TOKENS: u32 = 8_192;

/// Tokens charged per image block, about what providers bill for a
/// full-size screenshot.
const IMAGE_TOKENS: u64 = 1_600;

/// Token cap for the newest preserved user messages from the
/// summarized region.
const PRESERVE_TAIL_TOKENS: u64 = 20_000;

/// Token cap for the oldest preserved user messages, kept so the
/// original request survives even when the middle is elided.
const PRESERVE_HEAD_TOKENS: u64 = 2_000;

pub use kage_core::message::{COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX};

/// Inspect the agent context and, if usage is past the threshold, summarize
/// the oldest turns and replace them with one synthetic user message that
/// frames the summary so downstream providers accept the conversation.
///
/// Returns whether compaction ran.
pub(crate) fn maybe_compact<F: FnMut(LoopEvent)>(
    cx: &mut AgentContext,
    config: LoopConfig,
    provider: &dyn Provider,
    cancel: &CancelFlag,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> Result<bool, LoopError> {
    if !should_compact(cx, config) {
        return Ok(false);
    }
    match run_compaction(cx, provider, cancel, hooks, emit, KEEP_RECENT, true) {
        Ok(ran) => Ok(ran),
        // Compaction is maintenance, not the user's request: a
        // summarizer or plugin failure must not kill the run. Name the
        // failure and continue with the full history; when the context
        // truly cannot fit, the run loop's overflow recovery still
        // answers it.
        Err(LoopError::Cancelled) => Err(LoopError::Cancelled),
        Err(kind) => {
            emit(LoopEvent::Error { kind });
            Ok(false)
        }
    }
}

/// Force a compaction pass right now, ignoring the token-budget
/// threshold. Used by the `:compact` and `/compact` commands so the
/// user can shrink history on demand. Returns `false` when there is
/// not enough history to compact (history at or below `KEEP_RECENT`,
/// or nothing summarizable before the keep window).
pub fn force_compact<F: FnMut(LoopEvent)>(
    cx: &mut AgentContext,
    provider: &dyn Provider,
    cancel: &CancelFlag,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> Result<bool, LoopError> {
    run_compaction(cx, provider, cancel, hooks, emit, KEEP_RECENT, false)
}

/// Compact with a keep window of `keep` recent turns. The overflow
/// recovery path uses this to compact harder than the default: a
/// provider that rejected the post-compaction context gets a smaller
/// keep window each round until the history fits or nothing is left to
/// summarize.
pub(crate) fn compact_with_keep<F: FnMut(LoopEvent)>(
    cx: &mut AgentContext,
    provider: &dyn Provider,
    cancel: &CancelFlag,
    hooks: &mut dyn Hooks,
    emit: &mut F,
    keep: usize,
) -> Result<bool, LoopError> {
    run_compaction(cx, provider, cancel, hooks, emit, keep, false)
}

fn run_compaction<F: FnMut(LoopEvent)>(
    cx: &mut AgentContext,
    provider: &dyn Provider,
    cancel: &CancelFlag,
    hooks: &mut dyn Hooks,
    emit: &mut F,
    keep: usize,
    continuation: bool,
) -> Result<bool, LoopError> {
    if cx.history.len() <= keep {
        return Ok(false);
    }

    // Pick the boundary first. Kept history may not open with a
    // `ToolResult` whose assistant tool-call turn was summarized away:
    // strict providers reject the orphaned result. Walk the boundary
    // backwards until it sits on a non-ToolResult message, pulling the
    // parent assistant turn into the keep window. With `keep` of zero
    // there is no kept history to protect, so the whole history is
    // summarized.
    let mut split = cx.history.len() - keep;
    if keep > 0 {
        while split > 0 && cx.history[split].role == Role::ToolResult {
            split -= 1;
        }
    }
    if split == 0 && cx.history[0].role == Role::Assistant {
        // Degenerate shape: the keep window's parent is the very first
        // turn, so nothing summarizable sits before it. Summarize the
        // parent tool-call turn together with all of its results, which
        // never tears a ToolResult from its parent.
        split = (1..cx.history.len())
            .find(|&index| cx.history[index].role != Role::ToolResult)
            .unwrap_or(cx.history.len());
    }
    if split == 0 {
        return Ok(false);
    }
    let kept = cx.history.len() - split;

    // Summarize from a borrowed slice and splice only after every
    // fallible step below has succeeded, so a failed summary or hook
    // leaves the conversation intact.
    let transcript = serialize_conversation(&cx.history[..split]);
    let summarized_tokens = estimate_tokens(&transcript);
    let mut prep = CompactionPrep {
        prompt: format!("<conversation>\n{transcript}</conversation>\n\n{SUMMARIZE_INSTRUCTION}"),
        transcript,
        instruction: SUMMARIZE_INSTRUCTION.to_owned(),
        model: cx.model.clone(),
        summarized: split,
        kept,
        summary_override: None,
    };
    hooks
        .prepare_compaction(&mut prep)
        .map_err(|message| LoopError::HookFailed {
            hook: "compact_prepare".to_owned(),
            message,
        })?;
    let summary_text = if let Some(text) = prep.summary_override {
        text
    } else {
        emit(LoopEvent::CompactionStarted {
            summarized: prep.summarized,
            kept: prep.kept,
        });
        summarize_with_retry(
            provider,
            &prep.model,
            std::mem::take(&mut prep.prompt),
            &prep.instruction,
            cancel,
        )
        .map_err(wrap_compaction_error)?
    };
    // An empty summary (a provider stream that ended without text, or an
    // override of blanks) must not splice an empty frame over the
    // summarized turns, which would silently drop the conversation.
    // Return before the splice so history stays intact.
    if summary_text.trim().is_empty() {
        return Err(LoopError::Compaction {
            message: "the summarizer returned an empty summary".to_owned(),
        });
    }
    // Frame the returned text in place rather than formatting a copy:
    // the user's own words from the summarized region ride below the
    // summary, and the continuation note closes the handoff. The framing
    // constants stay the outermost wrapper so replay parsers keep
    // recognizing every compaction body ever stored.
    let mut summary_body = summary_text;
    if let Some(preserved) = preserved_user_text(&cx.history[..split]) {
        summary_body.push_str(PRESERVED_USERS_HEADER);
        summary_body.push_str(&preserved);
    }
    if continuation {
        summary_body.push_str(CONTINUATION_NOTE);
    }
    summary_body.insert_str(0, COMPACTION_SUMMARY_PREFIX);
    summary_body.push_str(COMPACTION_SUMMARY_SUFFIX);

    let summary_msg = Message {
        role: Role::User,
        content: vec![Content::Text {
            text: summary_body.clone(),
        }],
        id: MessageId::new(),
        parent: None,
        ts: chrono::Utc::now(),
    };
    let before = cx.budget.current_context;
    let estimate = if before == 0 {
        0
    } else {
        before
            .saturating_sub(summarized_tokens)
            .saturating_add(estimate_tokens(&summary_body))
    };
    cx.history
        .splice(..split, std::iter::once(Arc::new(summary_msg)));
    // Zero only the live context snapshot: the cumulative `used_*`
    // counters are session-wide cost/audit totals that must survive a
    // compaction. The next turn's `TokenBudget::add` re-snapshots
    // `current_context` from real usage.
    cx.budget = TokenBudget {
        current_context: 0,
        ..cx.budget
    };

    emit(LoopEvent::Compaction {
        kept,
        summarized: split,
        summary: summary_body,
        before,
        estimate,
    });
    Ok(true)
}

/// Name the summarizer's failure as a compaction failure so hosts can
/// say what broke. A cancellation is not a compaction failure and
/// keeps its own variant.
fn wrap_compaction_error(kind: LoopError) -> LoopError {
    match kind {
        LoopError::Cancelled => LoopError::Cancelled,
        other => LoopError::Compaction {
            message: other.to_string(),
        },
    }
}

fn should_compact(cx: &AgentContext, config: LoopConfig) -> bool {
    // `0` turns compaction off; the semantic is owned by the
    // `compaction_threshold` option doc in kage-core's options registry.
    if config.compaction_threshold <= 0.0 || cx.context_window == 0 {
        return false;
    }
    // Apply threshold via permille arithmetic to keep the math in integers.
    let frac = config.compaction_threshold.clamp(0.0, 1.0);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the fraction is clamped to 0..=1 first"
    )]
    let permille = (frac * 1000.0).round() as u64;
    let threshold = cx
        .context_window
        .saturating_mul(permille)
        .saturating_div(1000);
    // Compare the *most recent* turn's full prompt size to the
    // window. Summing per-turn prompts would count history again on
    // every turn, since each prompt carries the entire prior
    // conversation. The reserve guard trips before the
    // threshold when the remaining room can no longer hold a full
    // reply, so a long generation cannot overflow the window.
    let reply = cx.max_output_tokens.map_or(OUTPUT_RESERVE, u64::from);
    let reserve = OUTPUT_RESERVE
        .min(reply.saturating_add(estimate_margin(cx.context_window)))
        .min(cx.context_window / 2);
    let input = estimate_next_input(cx);
    input >= threshold || input.saturating_add(reserve) >= cx.context_window
}

/// Estimated prompt size of the next request built from `cx`. The last
/// turn's reported fill covers history through the newest assistant
/// message; what follows it (tool results, a new user message) is
/// estimated. With no reported fill, as in a fresh or just-compacted
/// context, the system prompt and whole history are estimated.
pub(crate) fn estimate_next_input(cx: &AgentContext) -> u64 {
    let (base, tail) = if cx.budget.current_context == 0 {
        (estimate_tokens(&cx.system_prompt), &cx.history[..])
    } else {
        let start = cx
            .history
            .iter()
            .rposition(|m| m.role == Role::Assistant)
            .map_or(0, |i| i + 1);
        (cx.budget.current_context, &cx.history[start..])
    };
    let images = tail
        .iter()
        .flat_map(|m| &m.content)
        .filter(|c| matches!(c, Content::Image { .. }))
        .count() as u64;
    base.saturating_add(estimate_tokens(&serialize_conversation(tail)))
        .saturating_add(images.saturating_mul(IMAGE_TOKENS))
}

/// Slack for [`estimate_next_input`] error: 2% of the window.
fn estimate_margin(window: u64) -> u64 {
    window / 50
}

/// `ceiling` lowered so the estimated input plus the requested output
/// fits the window. Providers reject `input + max_tokens > window` up
/// front, so asking for a model's full output limit on a well-filled
/// context fails even when the reply would be short. The
/// [`estimate_margin`] absorbs estimate error. The result never drops below room for a
/// short reply plus the thinking budget, nor rises above `ceiling`.
pub(crate) fn fit_output_ceiling(cx: &AgentContext, ceiling: u32) -> u32 {
    if cx.context_window == 0 {
        return ceiling;
    }
    let thinking = cx
        .reasoning
        .resolve(cx.thinking_level)
        .and_then(|level| cx.reasoning.budget(level))
        .unwrap_or(0);
    let floor = MIN_REPLY_TOKENS.saturating_add(thinking).min(ceiling);
    let room = cx
        .context_window
        .saturating_sub(estimate_next_input(cx))
        .saturating_sub(estimate_margin(cx.context_window));
    u32::try_from(room)
        .unwrap_or(u32::MAX)
        .clamp(floor, ceiling)
}

/// Instruction sent as the summarizer request's system prompt. The
/// summary is a handoff brief, not a narrative: the next context has no
/// other memory, so it must carry the work's state, not the chat's
/// history. Markdown is fine; hosts render the summary through their
/// markdown views.
const SUMMARIZE_INSTRUCTION: &str = "\
You are summarizing the conversation above so work can continue in a fresh context that has no \
other memory of it. Write a handoff brief for the next instance of yourself. Let the structure \
serve the task; keep it concise but never drop load-bearing detail. Cover, where applicable:\n\
- The user's request and any constraints they stated, in your own words.\n\
- What has actually been done so far, at high fidelity: the exact commands run, the exact file \
paths touched, and the concrete results or errors each step produced.\n\
- Current state: what is in progress, what changed and what did not, anything uncommitted or \
half-finished.\n\
- Open questions, unknowns, and anything you promised the user.\n\
- The committed forward plan: the exact next steps, so work resumes without re-deciding it.\n\n\
Preserve identifiers verbatim: file paths, commands, commit hashes, error codes, names. Write \
the state of the work, not a turn-by-turn retelling of the chat. The user's own messages may \
also be preserved verbatim below the summary; do not re-transcribe them.";

/// Opened below the model's summary when user messages were preserved.
const PRESERVED_USERS_HEADER: &str =
    "\n\n--- the user's own messages from before the compaction, preserved verbatim ---\n\n";

/// Closes the synthetic message after an automatic mid-run compaction:
/// the run resumes right after the splice, and the note tells the model
/// where to pick the work back up.
const CONTINUATION_NOTE: &str = "\n\nContinue the work that was in progress.";

/// Rough token estimate for `text`: one token per four ASCII bytes,
/// one token per non-ASCII char. Cheap and biased high, which is the
/// safe direction for a cap.
fn estimate_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut wide = 0u64;
    for ch in text.chars() {
        if (ch as u32) < 128 {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    ascii / 4 + wide
}

/// The user's own text from the summarized region, selected newest-first
/// under a token cap with the oldest messages kept as a small head so
/// the original request survives; anything between head and tail is
/// elided behind a named note. Returns `None` when there is nothing to
/// preserve, so the synthetic message keeps its plain summary shape.
fn preserved_user_text(messages: &[Arc<Message>]) -> Option<String> {
    let texts: Vec<&str> = messages
        .iter()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| &m.content)
        .filter_map(|block| match block {
            Content::Text { text }
                if !text.trim().is_empty() && !text.starts_with(COMPACTION_SUMMARY_PREFIX) =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    let last = texts.len().checked_sub(1)?;

    // Newest-first under the tail cap; the newest message is always
    // included, however large, since dropping the latest instruction
    // is the worst possible elision.
    let mut tail_start = last;
    let mut tail_tokens = estimate_tokens(texts[last]);
    while tail_start > 0 {
        let tokens = estimate_tokens(texts[tail_start - 1]);
        if tail_tokens + tokens > PRESERVE_TAIL_TOKENS {
            break;
        }
        tail_tokens += tokens;
        tail_start -= 1;
    }

    // Oldest-first under the head cap, but only while a middle was
    // actually elided; otherwise head and tail are the same run.
    let mut head_end = 0;
    if tail_start > 0 {
        let mut head_tokens = 0;
        while head_end < tail_start {
            let tokens = estimate_tokens(texts[head_end]);
            if head_tokens + tokens > PRESERVE_HEAD_TOKENS {
                break;
            }
            head_tokens += tokens;
            head_end += 1;
        }
    }

    let mut out = String::new();
    for text in &texts[..head_end] {
        out.push_str("--- user message ---\n");
        out.push_str(text);
        out.push('\n');
    }
    if tail_start > head_end {
        let elided_tokens: u64 = texts[head_end..tail_start]
            .iter()
            .map(|t| estimate_tokens(t))
            .sum();
        let _ = writeln!(
            out,
            "[{} earlier user message(s) elided, ~{} tokens]",
            tail_start - head_end,
            elided_tokens,
        );
    }
    for text in &texts[tail_start..] {
        out.push_str("--- user message ---\n");
        out.push_str(text);
        out.push('\n');
    }
    Some(out)
}

/// Serialize the to-be-summarized history into a plain-text transcript.
///
/// Sending the original messages verbatim risks two consecutive same-role
/// turns (e.g. ending on a User turn and then appending the User-role
/// summarize instruction), which providers like ZAI/GLM reject with
/// `"messages parameter is illegal"`. Folding everything into a single
/// User message sidesteps the ordering rules entirely.
fn serialize_conversation(messages: &[Arc<Message>]) -> String {
    let mut out = String::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::ToolResult => "tool_result",
            Role::System => "system",
        };
        out.push_str("=== ");
        out.push_str(role);
        out.push_str(" ===\n");
        for block in &msg.content {
            match block {
                Content::Text { text } | Content::Thinking { text, .. } => {
                    out.push_str(text);
                    out.push('\n');
                }
                Content::ToolCall { name, input, .. } => {
                    let _ = writeln!(out, "[tool_call {name}] {input}");
                }
                Content::ToolResultBlock {
                    output, is_error, ..
                } => {
                    let tag = if *is_error {
                        "tool_error"
                    } else {
                        "tool_result"
                    };
                    let _ = writeln!(out, "[{tag}] {output}");
                }
                Content::Image { mime, .. } => {
                    let _ = writeln!(out, "[image {mime}]");
                }
                Content::Custom { kind, data } => {
                    let _ = writeln!(out, "[custom {kind}] {data}");
                }
            }
        }
        out.push('\n');
    }
    out
}

fn summarize_with_retry(
    provider: &dyn Provider,
    model: &str,
    prompt: String,
    instruction: &str,
    cancel: &CancelFlag,
) -> Result<String, LoopError> {
    let mut prompt = prompt;
    for attempt in 0..SUMMARIZE_ATTEMPTS {
        match summarize(provider, model, prompt.clone(), instruction, cancel) {
            Ok(text) => return Ok(text),
            Err(err) => {
                if attempt + 1 == SUMMARIZE_ATTEMPTS {
                    return Err(err);
                }
                let overflow = matches!(&err, LoopError::Provider { message }
                    if kage_provider::error::is_context_overflow_text(message));
                if !overflow {
                    return Err(err);
                }
                let Some(shrunk) = shrink_transcript_prompt(&prompt) else {
                    return Err(err);
                };
                prompt = shrunk;
            }
        }
    }
    unreachable!("the loop returns inside every branch")
}

/// `prompt` with the oldest quarter of its transcript section dropped,
/// cut at the next role marker so a message never splits. Returns
/// `None` when the prompt has no conversation frame or nothing left to
/// drop. A note names the drop so the model can say what it never saw.
fn shrink_transcript_prompt(prompt: &str) -> Option<String> {
    const OPEN: &str = "<conversation>\n";
    const CLOSE: &str = "</conversation>";
    let start = prompt.find(OPEN)? + OPEN.len();
    let end = prompt[start..].find(CLOSE)? + start;
    let transcript = &prompt[start..end];
    let mut quarter = transcript.len() / 4;
    if quarter == 0 {
        return None;
    }
    // The quarter may land inside a multi-byte character; back it up
    // before slicing, or the search below would panic.
    while !transcript.is_char_boundary(quarter) {
        quarter -= 1;
    }
    let cut = transcript[quarter..]
        .find("\n=== ")
        .map_or(quarter, |at| quarter + at + 1);
    let mut cut = cut;
    while !transcript.is_char_boundary(cut) {
        cut -= 1;
    }
    let rest = transcript[cut..].trim_start_matches('\n');
    if rest.is_empty() {
        return None;
    }
    Some(format!(
        "<conversation>\n[the oldest part of the transcript was dropped to fit the request]\n\n{rest}</conversation>{}",
        &prompt[end..],
    ))
}

/// One summarizer model call: the framed transcript as the single user
/// message, the instruction as the system prompt.
fn summarize(
    provider: &dyn Provider,
    model: &str,
    prompt: String,
    instruction: &str,
    cancel: &CancelFlag,
) -> Result<String, LoopError> {
    let payload = vec![Arc::new(Message::new(
        Role::User,
        vec![Content::Text { text: prompt }],
        None,
    ))];
    let mut req = StreamRequest::new(model, payload);
    req.system = Some(instruction.to_owned());
    let stream = provider
        .stream(req, cancel)
        .map_err(|e| LoopError::Provider {
            message: e.to_string(),
        })?;

    let mut text = String::new();
    for event in stream {
        if cancel.is_cancelled() {
            return Err(LoopError::Cancelled);
        }
        let event = event.map_err(|e| LoopError::Provider {
            message: e.to_string(),
        })?;
        match event {
            ProviderEvent::TextDelta { delta } => text.push_str(&delta),
            ProviderEvent::MessageEnd { .. } => return Ok(text),
            _ => {}
        }
    }
    Err(LoopError::Provider {
        message: "summary stream ended without MessageEnd".into(),
    })
}

#[cfg(test)]
mod tests {
    use kage_core::{Content, Role, TokenUsage};
    use kage_provider::testing::{MockProvider, user_msg};
    use kage_provider::{ProviderEvent, StopReason};

    use super::*;
    use crate::NoopHooks;

    fn assistant_msg(text: &str) -> Message {
        Message::new(
            Role::Assistant,
            vec![Content::Text {
                text: text.to_owned(),
            }],
            None,
        )
    }

    fn assistant_tool_call(id: &str) -> Message {
        Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: kage_core::ToolCallId::new(id),
                name: "shell".into(),
                input: serde_json::json!({}),
            }],
            None,
        )
    }

    fn tool_result(id: &str) -> Message {
        Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: kage_core::ToolCallId::new(id),
                output: "out".into(),
                is_error: false,
            }],
            None,
        )
    }

    fn loaded_context(used_input: u64, history_len: usize) -> AgentContext {
        let mut cx = AgentContext::new("mock:m", "");
        cx.budget = TokenBudget {
            used_input,
            // The compaction check now compares the *most recent*
            // turn's context fill to the threshold; mirror
            // `used_input` here so existing tests (which were
            // written before the split) keep their original intent.
            current_context: used_input,
            ..Default::default()
        };
        for i in 0..history_len {
            let msg = if i % 2 == 0 {
                user_msg(&format!("turn {i}"))
            } else {
                assistant_msg(&format!("reply {i}"))
            };
            cx.history.push(Arc::new(msg));
        }
        cx
    }

    #[test]
    fn the_reserve_shrinks_to_a_small_output_limit() {
        let mut cx = loaded_context(150_000, 4);
        let config = LoopConfig::default();
        assert!(should_compact(&cx, config));
        cx.max_output_tokens = Some(8_192);
        assert!(!should_compact(&cx, config));
    }

    #[test]
    fn results_after_the_last_turn_count_toward_the_threshold() {
        let mut cx = loaded_context(120_000, 4);
        let config = LoopConfig::default();
        assert!(!should_compact(&cx, config));
        cx.history.push(Arc::new(user_msg(&"x".repeat(40_000 * 4))));
        assert!(should_compact(&cx, config));
    }

    #[test]
    fn skipped_when_under_threshold() {
        let provider = MockProvider::replaying(vec![]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig::default();
        let mut cx = loaded_context(1_000, 20);
        cx.context_window = 200_000;

        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(!ran);
        assert_eq!(cx.history.len(), 20);
    }

    #[test]
    fn skipped_when_history_too_short() {
        let provider = MockProvider::replaying(vec![]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig::default();
        let mut cx = loaded_context(u64::MAX / 2, 3);
        cx.context_window = 200_000;

        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(!ran);
        assert_eq!(cx.history.len(), 3);
    }

    /// A validated threshold of `0` is compaction off, so the loop must
    /// read it unchanged and never compact, however full the context is.
    #[test]
    fn zero_threshold_reaches_the_loop_and_compacts_nothing() {
        let provider = MockProvider::replaying(vec![]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.0,
            ..LoopConfig::default()
        };
        let mut cx = loaded_context(150_000, 10);
        cx.context_window = 200_000;

        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(!ran);
        assert_eq!(cx.history.len(), 10);
    }

    #[test]
    fn compacts_and_emits_event_when_over_threshold() {
        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "you discussed X".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = loaded_context(150_000, 10);
        cx.context_window = 200_000;

        let mut events = Vec::new();
        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |ev| {
            events.push(ev);
        })
        .unwrap();
        assert!(ran);
        assert_eq!(cx.history.len(), 1 + KEEP_RECENT);
        assert_eq!(
            cx.history[0].role,
            Role::User,
            "synthetic summary must be User role so providers like ZAI/Anthropic accept the post-compaction history"
        );
        match &cx.history[0].content[0] {
            Content::Text { text } => {
                assert!(text.contains("you discussed X"));
                assert!(text.contains("<summary>"));
                assert!(text.contains("compacted"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
        assert!(events.iter().any(
            |e| matches!(e, LoopEvent::Compaction { kept, summarized, .. }
                if *kept == KEEP_RECENT && *summarized == 10 - KEEP_RECENT)
        ));
        assert_eq!(
            cx.budget.used_input, 150_000,
            "compaction must keep the cumulative cost totals"
        );
        assert_eq!(cx.budget.current_context, 0);
    }

    /// The started signal precedes the completion event and names the
    /// split the summarizer will read, so UIs can show "compacting"
    /// while the summarizer model call runs.
    #[test]
    fn compaction_started_precedes_the_compaction_event() {
        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "summary".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = loaded_context(150_000, 10);
        cx.context_window = 200_000;

        let mut events = Vec::new();
        maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |ev| {
            events.push(ev);
        })
        .unwrap();
        let started = events
            .iter()
            .position(|e| matches!(e, LoopEvent::CompactionStarted { .. }))
            .expect("compaction must announce itself");
        let done = events
            .iter()
            .position(|e| matches!(e, LoopEvent::Compaction { .. }))
            .expect("compaction must complete");
        assert!(started < done, "started must precede the completion");
        assert_eq!(
            events[started],
            LoopEvent::CompactionStarted {
                summarized: 10 - KEEP_RECENT,
                kept: KEEP_RECENT,
            }
        );
    }

    /// A plugin that hands over the summary skips the summarizer model,
    /// so there is no wait to announce: no started event may fire.
    #[test]
    fn summary_override_emits_no_compaction_started() {
        struct OverrideHooks;
        impl Hooks for OverrideHooks {
            fn prepare_compaction(&mut self, prep: &mut CompactionPrep) -> Result<(), String> {
                prep.summary_override = Some("handwritten".to_owned());
                Ok(())
            }
        }
        let provider = MockProvider::replaying(vec![]);
        let cancel = CancelFlag::new();
        let mut hooks = OverrideHooks;
        let mut cx = loaded_context(150_000, 10);
        cx.context_window = 200_000;

        let mut events = Vec::new();
        let ran = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |ev| {
            events.push(ev);
        })
        .unwrap();
        assert!(ran);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, LoopEvent::CompactionStarted { .. })),
            "override path must not announce a start: {events:?}"
        );
    }

    /// Compaction zeroes only the live context snapshot; the cumulative
    /// `used_*` counters survive so modeline cost totals do not reset.
    #[test]
    fn compaction_preserves_cumulative_totals_and_zeroes_current_context() {
        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "summary".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = loaded_context(1_000, 10);
        cx.budget.used_output = 500;
        cx.budget.used_cache_read = 40;
        cx.budget.current_context = 150_000;
        cx.context_window = 200_000;

        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(ran);
        assert_eq!(cx.budget.used_input, 1_000);
        assert_eq!(cx.budget.used_output, 500);
        assert_eq!(cx.budget.used_cache_read, 40);
        assert_eq!(cx.budget.current_context, 0);
    }

    /// The host's estimate trades the summarized turns for the summary,
    /// while the budget keeps `0` so the estimate never trips another
    /// compaction before real usage arrives.
    #[test]
    fn compaction_estimates_the_fill_it_leaves() {
        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "summary".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let mut cx = loaded_context(1_000, 10);
        cx.budget.current_context = 150_000;
        cx.context_window = 200_000;
        let split = cx.history.len() - KEEP_RECENT;
        let summarized = estimate_tokens(&serialize_conversation(&cx.history[..split]));
        let mut events = Vec::new();

        let ran = force_compact(
            &mut cx,
            &provider,
            &CancelFlag::new(),
            &mut NoopHooks,
            &mut |e| events.push(e),
        )
        .unwrap();
        assert!(ran);
        let (estimate, summary) = events
            .iter()
            .find_map(|e| match e {
                LoopEvent::Compaction {
                    estimate, summary, ..
                } => Some((*estimate, summary.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(estimate, 150_000 - summarized + estimate_tokens(&summary));
        assert_eq!(cx.budget.current_context, 0);
    }

    #[test]
    fn boundary_extends_backwards_to_keep_tool_group_intact() {
        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "summary".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = AgentContext::new("mock:m", "");
        cx.budget = TokenBudget {
            used_input: 150_000,
            current_context: 150_000,
            ..Default::default()
        };
        cx.context_window = 200_000;
        cx.history.push(Arc::new(user_msg("turn 0")));
        cx.history.push(Arc::new(assistant_msg("reply 1")));
        cx.history.push(Arc::new(user_msg("turn 2")));
        cx.history.push(Arc::new(assistant_tool_call("call_1")));
        cx.history.push(Arc::new(tool_result("call_1")));
        cx.history.push(Arc::new(user_msg("turn 5")));
        cx.history.push(Arc::new(assistant_msg("reply 6")));
        cx.history.push(Arc::new(user_msg("turn 7")));

        let mut events = Vec::new();
        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |ev| {
            events.push(ev);
        })
        .unwrap();
        assert!(ran);
        // The raw boundary (len 8 - KEEP_RECENT) lands on the tool
        // result at index 4; it must walk back to the assistant
        // tool-call turn at index 3, keeping the pair together.
        assert_eq!(cx.history.len(), 1 + 5);
        assert!(
            matches!(&cx.history[1].content[0], Content::ToolCall { .. }),
            "kept history must open with the parent tool-call turn, not a result"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            LoopEvent::Compaction {
                kept: 5,
                summarized: 3,
                ..
            }
        )));
    }

    /// A history that is nothing but one tool-call group longer than the
    /// keep window used to refuse compaction forever. The fallback split
    /// summarizes the parent turn together with all of its results in one
    /// pass, so the group is never torn.
    #[test]
    fn nothing_summarizable_before_keep_window_compacts_nothing() {
        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "summary".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = AgentContext::new("mock:m", "");
        cx.budget = TokenBudget {
            used_input: 150_000,
            current_context: 150_000,
            ..Default::default()
        };
        cx.context_window = 200_000;
        cx.history.push(Arc::new(assistant_tool_call("call_1")));
        for _ in 0..8 {
            cx.history.push(Arc::new(tool_result("call_1")));
        }

        let mut events = Vec::new();
        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |ev| {
            events.push(ev);
        })
        .unwrap();
        assert!(ran, "the stuck shape must compact via the fallback split");
        assert_eq!(cx.history.len(), 1, "the whole group is summarized");
        assert!(
            matches!(&cx.history[0].content[0], Content::Text { text } if text.contains("summary"))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            LoopEvent::Compaction {
                kept: 0,
                summarized: 9,
                ..
            }
        )));
    }

    /// Orphaned tool results with no assistant parent in history still
    /// refuse to compact: there is no group boundary to fall back to.
    #[test]
    fn orphaned_results_without_an_assistant_still_refuse_compaction() {
        let provider = MockProvider::replaying(vec![]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = AgentContext::new("mock:m", "");
        cx.budget = TokenBudget {
            used_input: 150_000,
            current_context: 150_000,
            ..Default::default()
        };
        cx.context_window = 200_000;
        for _ in 0..8 {
            cx.history.push(Arc::new(tool_result("call_1")));
        }

        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(!ran);
        assert_eq!(cx.history.len(), 8, "history must be untouched");
    }

    /// A summarizer stream that ends with zero text must abort before the
    /// splice so the summarized conversation is never replaced by an
    /// empty frame, and the automatic path reports the failure instead
    /// of failing the run.
    #[test]
    fn empty_summary_warns_and_leaves_history_intact() {
        let provider = MockProvider::replaying(vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = loaded_context(150_000, 10);
        cx.context_window = 200_000;

        let mut events = Vec::new();
        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |ev| {
            events.push(ev);
        })
        .unwrap();
        assert!(!ran, "an empty summary must not compact");
        assert_eq!(cx.history.len(), 10, "history must stay intact");
        assert!(
            matches!(&cx.history[0].content[0], Content::Text { text } if text == "turn 0"),
            "no turn may be replaced by the empty summary"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, LoopEvent::Compaction { .. }))
        );
        // The failure is named after the started signal, so UIs can
        // close the compacting indicator with a reason.
        let started = events
            .iter()
            .position(|e| matches!(e, LoopEvent::CompactionStarted { .. }))
            .expect("the compaction announced itself");
        let failed = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    LoopEvent::Error {
                        kind: LoopError::Compaction { .. }
                    }
                )
            })
            .expect("the failure must be surfaced");
        assert!(started < failed);
    }

    /// A whitespace-only `summary_override` hits the same guard as an
    /// empty provider summary.
    #[test]
    fn empty_summary_override_aborts_before_the_splice() {
        struct BlankOverride;
        impl Hooks for BlankOverride {
            fn prepare_compaction(&mut self, prep: &mut CompactionPrep) -> Result<(), String> {
                prep.summary_override = Some("   \n\t".to_owned());
                Ok(())
            }
        }
        let provider = MockProvider::replaying(vec![]);
        let cancel = CancelFlag::new();
        let mut hooks = BlankOverride;
        let mut cx = loaded_context(150_000, 10);
        cx.context_window = 200_000;

        let res = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |_| {});
        assert!(matches!(res, Err(LoopError::Compaction { .. })));
        assert_eq!(cx.history.len(), 10, "history must stay intact");
    }

    #[test]
    fn prepare_compaction_hook_can_redirect_the_summarizer_model() {
        struct RedirectModel;
        impl crate::Hooks for RedirectModel {
            fn prepare_compaction(&mut self, prep: &mut CompactionPrep) -> Result<(), String> {
                assert_eq!(prep.model, "mock:m");
                prep.model = "mock:cheap".to_owned();
                Ok(())
            }
        }

        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "summary".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let cancel = CancelFlag::new();
        let mut hooks = RedirectModel;
        let mut cx = loaded_context(0, 10);

        let ran = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(ran);
        assert_eq!(
            provider.last_request().expect("a summarize request").model,
            "mock:cheap",
            "summarize must use the model the hook set on prep.model"
        );
    }

    /// An automatic compaction whose summarizer fails warns and leaves
    /// the run standing; a forced one, which the user asked for,
    /// returns the compaction failure to its caller. Both leave the
    /// history intact.
    #[test]
    fn a_failed_auto_compaction_warns_and_a_forced_one_fails() {
        let script = || {
            MockProvider::replaying(vec![Err(kage_provider::ProviderError::Auth(
                "no key".into(),
            ))])
        };
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = loaded_context(150_000, 10);
        cx.context_window = 200_000;

        let mut events = Vec::new();
        let ran = maybe_compact(&mut cx, cfg, &script(), &cancel, &mut hooks, &mut |ev| {
            events.push(ev);
        })
        .unwrap();
        assert!(!ran, "the run continues with the full history");
        assert_eq!(cx.history.len(), 10, "history must stay intact");
        assert!(matches!(&cx.history[0].content[0], Content::Text { text } if text == "turn 0"));
        assert!(events.iter().any(|e| matches!(
            e,
            LoopEvent::Error { kind: LoopError::Compaction { message } }
                if message.contains("authentication failed")
        )));

        let res = force_compact(&mut cx, &script(), &cancel, &mut hooks, &mut |_| {});
        match res {
            Err(LoopError::Compaction { message }) => {
                assert!(message.contains("authentication failed"), "{message}");
            }
            other => panic!("expected Compaction, got {other:?}"),
        }
        assert_eq!(cx.history.len(), 10, "history must stay intact");
    }

    /// Cancelling during the summarizer ends the compaction with
    /// `Cancelled`, which the automatic path propagates so the run
    /// still ends cancelled.
    #[test]
    fn a_cancelled_summarizer_propagates_cancelled() {
        let provider = MockProvider::replaying(summary_script());
        let cancel = CancelFlag::new();
        cancel.cancel();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = loaded_context(150_000, 10);
        cx.context_window = 200_000;

        let res = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |_| {});
        assert!(matches!(res, Err(LoopError::Cancelled)));
        assert_eq!(cx.history.len(), 10, "history must stay intact");
    }

    #[test]
    fn prepare_compaction_override_skips_model_call() {
        struct OverrideHook;
        impl Hooks for OverrideHook {
            fn prepare_compaction(&mut self, prep: &mut CompactionPrep) -> Result<(), String> {
                assert_eq!(prep.kept, KEEP_RECENT);
                assert_eq!(prep.summarized, 10 - KEEP_RECENT);
                assert!(prep.prompt.contains("<conversation>"));
                prep.summary_override = Some("PLUGIN WROTE THIS".to_owned());
                Ok(())
            }
        }
        // Empty replay: if summarize() were called it would error, so
        // a clean run proves the model call was skipped.
        let provider = MockProvider::replaying(vec![]);
        let cancel = CancelFlag::new();
        let mut hooks = OverrideHook;
        let mut cx = loaded_context(1_000, 10);
        cx.context_window = 200_000;

        let ran = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(ran);
        match &cx.history[0].content[0] {
            Content::Text { text } => {
                assert!(text.contains("PLUGIN WROTE THIS"));
                assert!(text.contains("<summary>"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn prepare_compaction_error_aborts_with_hook_failed() {
        struct FailHook;
        impl Hooks for FailHook {
            fn prepare_compaction(&mut self, _prep: &mut CompactionPrep) -> Result<(), String> {
                Err("nope".to_owned())
            }
        }
        let provider = MockProvider::replaying(vec![]);
        let cancel = CancelFlag::new();
        let mut hooks = FailHook;
        let mut cx = loaded_context(1_000, 10);
        cx.context_window = 200_000;

        let res = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |_| {});
        match res {
            Err(LoopError::HookFailed { hook, message }) => {
                assert_eq!(hook, "compact_prepare");
                assert_eq!(message, "nope");
            }
            other => panic!("expected HookFailed, got {other:?}"),
        }
        assert_eq!(
            cx.history.len(),
            10,
            "a failed hook must leave the history intact"
        );
    }

    #[test]
    fn prepare_compaction_prompt_rewrite_reaches_model() {
        struct RewriteHook;
        impl Hooks for RewriteHook {
            fn prepare_compaction(&mut self, prep: &mut CompactionPrep) -> Result<(), String> {
                prep.prompt = "REWRITTEN".to_owned();
                prep.instruction = "BE TERSE".to_owned();
                Ok(())
            }
        }
        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "summarized".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let cancel = CancelFlag::new();
        let mut hooks = RewriteHook;
        let mut cx = loaded_context(1_000, 10);
        cx.context_window = 200_000;

        let ran = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(ran);
        match &cx.history[0].content[0] {
            Content::Text { text } => assert!(text.contains("summarized")),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn serialize_conversation_keeps_role_markers_and_text() {
        let msgs: Vec<_> = vec![user_msg("hello"), assistant_msg("hi back")]
            .into_iter()
            .map(Arc::new)
            .collect();
        let out = serialize_conversation(&msgs);
        assert!(out.contains("=== user ==="));
        assert!(out.contains("=== assistant ==="));
        assert!(out.contains("hello"));
        assert!(out.contains("hi back"));
    }

    fn overflow_rejection() -> kage_provider::ProviderError {
        kage_provider::ProviderError::Http {
            status: 400,
            body: "prompt is too long: 300000 tokens > 200000 maximum".into(),
        }
    }

    fn summary_script() -> Vec<Result<ProviderEvent, kage_provider::ProviderError>> {
        vec![
            Ok(ProviderEvent::TextDelta {
                delta: "the handoff".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]
    }

    /// The reserve guard: with a threshold the fill never reaches, a
    /// remaining room too small for a full reply still compacts.
    #[test]
    fn reserve_headroom_triggers_compaction_below_the_threshold() {
        let provider = MockProvider::replaying(summary_script());
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 1.0,
            ..LoopConfig::default()
        };
        let mut cx = loaded_context(60_000, 10);
        cx.context_window = 100_000;

        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(
            ran,
            "60k of a 100k window must compact: no room for a reply"
        );
    }

    #[test]
    fn auto_compaction_preserves_user_messages_and_appends_the_continuation_note() {
        let provider = MockProvider::replaying(summary_script());
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = loaded_context(150_000, 10);
        cx.context_window = 200_000;

        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(ran);
        match &cx.history[0].content[0] {
            Content::Text { text } => {
                assert!(text.contains(PRESERVED_USERS_HEADER), "{text}");
                assert!(text.contains("--- user message ---\nturn 0"), "{text}");
                assert!(text.contains("--- user message ---\nturn 4"), "{text}");
                assert!(
                    !text.contains("turn 6"),
                    "kept-window users are not duplicated into the card: {text}"
                );
                assert!(text.ends_with("Continue the work that was in progress.\n</summary>"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn forced_compaction_appends_no_continuation_note() {
        let provider = MockProvider::replaying(summary_script());
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let mut cx = loaded_context(150_000, 10);
        cx.context_window = 200_000;

        let ran = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(ran);
        match &cx.history[0].content[0] {
            Content::Text { text } => {
                assert!(!text.contains("Continue the work"), "{text}");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn a_region_without_user_text_gains_no_preserved_section() {
        let provider = MockProvider::replaying(summary_script());
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = AgentContext::new("mock:m", "");
        cx.budget = TokenBudget {
            used_input: 150_000,
            current_context: 150_000,
            ..Default::default()
        };
        cx.context_window = 200_000;
        // Summarized region is assistant-only; users sit in the keep
        // window and must not be duplicated.
        for i in 0..6 {
            cx.history
                .push(Arc::new(assistant_msg(&format!("reply {i}"))));
        }
        cx.history.push(Arc::new(user_msg("turn 6")));
        cx.history.push(Arc::new(assistant_msg("reply 7")));
        cx.history.push(Arc::new(user_msg("turn 8")));
        cx.history.push(Arc::new(assistant_msg("reply 9")));

        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(ran);
        match &cx.history[0].content[0] {
            Content::Text { text } => {
                assert!(!text.contains(PRESERVED_USERS_HEADER), "{text}");
                assert!(text.ends_with("</summary>"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn preserved_user_selection_elides_a_large_middle() {
        let mut msgs: Vec<Arc<Message>> = vec![Arc::new(user_msg("first request"))];
        for i in 1..=24 {
            msgs.push(Arc::new(user_msg(&format!(
                "BULK{i}Z {}",
                "x".repeat(4_000)
            ))));
        }
        msgs.push(Arc::new(user_msg("final ask")));

        let preserved = preserved_user_text(&msgs).expect("users exist");
        assert!(preserved.starts_with("--- user message ---\nfirst request\n"));
        assert!(
            preserved.contains("--- user message ---\nBULK1Z "),
            "{preserved}"
        );
        assert!(
            !preserved.contains("BULK2Z"),
            "an elided middle message must be absent: {preserved}"
        );
        assert!(preserved.contains("earlier user message(s) elided"));
        assert!(preserved.contains("final ask"));
    }

    #[test]
    fn preserved_user_selection_skips_prior_summaries_and_blanks() {
        let msgs: Vec<Arc<Message>> = vec![
            Arc::new(Message::new(
                Role::User,
                vec![Content::Text {
                    text: format!("{COMPACTION_SUMMARY_PREFIX}old</summary>"),
                }],
                None,
            )),
            Arc::new(user_msg("   ")),
            Arc::new(user_msg("the real one")),
        ];
        let preserved = preserved_user_text(&msgs).expect("one text survives");
        assert_eq!(preserved, "--- user message ---\nthe real one\n");
    }

    /// A summarizer call the provider rejects for size shrinks the
    /// transcript at a role boundary and retries, so a history whose
    /// summarized region alone overflows can still be compacted.
    #[test]
    fn summarizer_overflow_shrinks_the_transcript_and_retries() {
        let provider =
            MockProvider::sequence(vec![vec![Err(overflow_rejection())], summary_script()]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let mut cx = loaded_context(150_000, 40);
        cx.context_window = 200_000;

        let ran = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(ran);
        let reqs = provider.requests();
        assert_eq!(reqs.len(), 2, "one overflow, one retry");
        let prompt_of = |req: &kage_provider::StreamRequest| match &req.messages[0].content[0] {
            Content::Text { text } => text.clone(),
            other => panic!("expected Text, got {other:?}"),
        };
        let (first, second) = (prompt_of(&reqs[0]), prompt_of(&reqs[1]));
        assert!(
            second.len() < first.len(),
            "the retry must send less: {} vs {}",
            second.len(),
            first.len()
        );
        assert!(
            second.contains("[the oldest part of the transcript was dropped to fit the request]")
        );
        assert!(second.contains("</conversation>"), "the frame must survive");
    }

    /// A quarter that lands inside a multi-byte character must back up
    /// to a boundary before slicing, or the shrink would panic on a
    /// non-ASCII transcript.
    #[test]
    fn shrink_cuts_at_a_character_boundary_for_non_ascii_transcripts() {
        let body = "\u{65e5}".repeat(2_000);
        let prompt = format!(
            "<conversation>\n=== user ===\n{body}\n=== user ===\ntail\n</conversation>INSTRUCTION"
        );
        let shrunk = shrink_transcript_prompt(&prompt).expect("something to drop");
        assert!(
            shrunk.contains("[the oldest part of the transcript was dropped"),
            "{shrunk}"
        );
        assert!(shrunk.contains("tail"), "{shrunk}");
        assert!(
            !shrunk.contains('\u{65e5}'),
            "the dropped section must be gone: {shrunk}"
        );
        assert!(shrunk.ends_with("INSTRUCTION"), "{shrunk}");
    }

    #[test]
    fn summarizer_gives_up_after_three_overflow_attempts() {
        let provider = MockProvider::sequence(vec![vec![Err(overflow_rejection())]; 3]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let mut cx = loaded_context(150_000, 40);
        cx.context_window = 200_000;

        let res = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |_| {});
        assert!(matches!(res, Err(LoopError::Compaction { .. })));
        assert_eq!(cx.history.len(), 40, "history must stay intact");
        assert_eq!(provider.call_count(), 3, "bounded attempts");
    }

    /// A non-overflow provider failure must not trigger a shrink.
    #[test]
    fn summarizer_auth_failure_propagates_without_a_retry() {
        let provider = MockProvider::replaying(vec![Err(kage_provider::ProviderError::Auth(
            "no key".into(),
        ))]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let mut cx = loaded_context(150_000, 40);
        cx.context_window = 200_000;

        let res = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |_| {});
        assert!(matches!(res, Err(LoopError::Compaction { .. })));
        assert_eq!(provider.call_count(), 1);
    }
}
