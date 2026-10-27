//! Loop entry point.
//!
//! Top-level control flow:
//!
//! ```text
//! outer loop {                  // follow-ups
//!   inner loop {                // tool-call rounds
//!     stream from provider
//!     translate events -> LoopEvent
//!     if message had tool calls -> dispatch, append results, continue
//!     else                       -> break inner
//!   }
//!   if hooks.get_followup() -> push as user message, continue
//!   else                    -> break outer
//! }
//! ```

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use kage_core::{CancelFlag, Content, LoopError, LoopEvent, Message, MessageId, Role, ToolCallId};
use kage_provider::{Provider, ProviderError, StreamRequest};
use kage_tools::ToolRegistry;

use crate::compact::{
    KEEP_RECENT, MAX_OVERFLOW_COMPACTIONS, compact_with_keep, fit_output_ceiling, maybe_compact,
};
use crate::dispatch::{dispatch_tool_calls, dispatch_tool_calls_parallel, unrun_results};
use crate::doom::DoomTracker;
use crate::stream::{TurnFailure, TurnResult, collect_turn};
use crate::{AgentContext, Hooks, LoopConfig, SteeringMode};

/// Maximum messages drained from a steering or follow-up hook per call.
/// A hook that never runs dry would otherwise hang the loop on an
/// ever-growing join; correct hosts are unaffected because their queues
/// are far smaller.
const MAX_DRAINED_MESSAGES: usize = 64;

/// Appended to a drained message when the [`MAX_DRAINED_MESSAGES`] cap
/// truncated the queue. There is no dedicated notice event, so the note
/// travels inside the joined text, where history and the provider
/// request both see it.
const DRAIN_TRUNCATED_NOTE: &str =
    "[kage: stopped draining after 64 queued messages; the rest were dropped]";

/// Drive one agent run to completion.
///
/// `cx` carries the conversation forward: the caller is expected to push the
/// initiating user message into `cx.history` before calling this. On return,
/// `cx.history` reflects every message produced during the run, and
/// `cx.budget` is updated from provider-reported usage. Every message the
/// loop appends is announced with [`LoopEvent::MessageAppended`]; the
/// initiating message is the caller's to announce.
///
/// Streaming events are delivered to `emit` in order.
///
/// Cancellation is cooperative: the loop polls `cancel` between turns and
/// after each provider event. On cancel, the run terminates with
/// [`LoopError::Cancelled`].
///
/// # Errors
///
/// Returns the same [`LoopError`] variant that was emitted as the terminal
/// [`LoopEvent::Error`], so callers can react programmatically without
/// re-parsing events.
#[expect(
    clippy::too_many_lines,
    reason = "the turn loop reads best as one function"
)]
pub fn run<F>(
    provider: &dyn Provider,
    tools: &ToolRegistry,
    cx: &mut AgentContext,
    config: LoopConfig,
    hooks: &mut dyn Hooks,
    cancel: &CancelFlag,
    mut emit: F,
) -> Result<(), LoopError>
where
    F: FnMut(LoopEvent),
{
    let mut doom = DoomTracker::default();
    let mut turn_index: u32 = 0;

    // A relative workdir (the historical `.` fallback when `current_dir`
    // fails) would let confinement resolve against whatever the process
    // CWD becomes later. Refuse it instead of silently running there.
    if !cx.workdir.is_absolute() {
        let kind = LoopError::Other {
            message: format!(
                "agent workdir '{}' is not absolute; build the context with \
                 AgentContext::try_new or set an absolute workdir",
                cx.workdir.display()
            ),
        };
        emit(LoopEvent::Error { kind: kind.clone() });
        return Err(kind);
    }

    loop {
        if cancel.is_cancelled() {
            return finish_cancelled(&mut emit);
        }

        loop {
            if cancel.is_cancelled() {
                return finish_cancelled(&mut emit);
            }

            match drain_content(config.steering_mode, cancel, || hooks.get_steering()) {
                Ok(items) => {
                    for content in items {
                        push_user_content(cx, &mut emit, content);
                    }
                }
                Err(LoopError::Cancelled) => return finish_cancelled(&mut emit),
                Err(_) => unreachable!("drain only fails with Cancelled"),
            }

            if let Err(kind) = maybe_compact(cx, config, provider, cancel, hooks, &mut emit) {
                emit(LoopEvent::Error { kind: kind.clone() });
                return Err(kind);
            }

            emit(LoopEvent::TurnStarted { index: turn_index });

            if let Err(message) = hooks.transform_context(&mut cx.history) {
                let kind = LoopError::HookFailed {
                    hook: "transform_context".to_owned(),
                    message,
                };
                emit(LoopEvent::Error { kind: kind.clone() });
                return Err(kind);
            }

            // Auto-retry a transiently-failed turn. The conversation
            // context is unchanged across ordinary attempts - no
            // assistant message is appended on failure - so re-issuing
            // the identical request is a clean re-request, not a resume
            // (SSE has no resume token).
            //
            // A context-overflow rejection is the one failure that
            // changes context between attempts: the loop compacts
            // harder (a smaller keep window each round, at most
            // `MAX_OVERFLOW_COMPACTIONS` rounds) and rebuilds the
            // request from the shrunk history. That is why the request
            // is constructed inside the attempt loop.
            //
            // Event contract for `LoopEvent::ProviderRetry`: every
            // delta streamed since the last `MessageEnd` belonged to a
            // dropped attempt and must be discarded by the receiver.
            // Hosts surface the event as a notice-like block (the
            // TUI's `kage:retry`, the RPC notice update), and only the
            // successful attempt is persisted because a failed attempt
            // appends nothing. The retrying turn reuses no message or
            // tool-call ids, so id-keyed clients cannot concatenate
            // the attempts either. The retry streams the full turn
            // from scratch; no deltas are replayed.
            let turn = {
                let mut attempt: u32 = 0;
                let mut overflow_round: u32 = 0;
                loop {
                    if cancel.is_cancelled() {
                        return finish_cancelled(&mut emit);
                    }
                    let mut req = build_request(cx, tools, provider);
                    if let Err(message) = hooks.transform_provider_request(&mut req) {
                        let kind = LoopError::HookFailed {
                            hook: "transform_provider_request".to_owned(),
                            message,
                        };
                        emit(LoopEvent::Error { kind: kind.clone() });
                        return Err(kind);
                    }
                    let parent = cx.history.last().map(|m| m.id);
                    match stream_one_attempt(provider, req, parent, cancel, &mut emit) {
                        Ok(t) => break t,
                        Err(TurnFailure::Fatal(kind)) => {
                            emit(LoopEvent::Error { kind: kind.clone() });
                            return Err(kind);
                        }
                        Err(TurnFailure::Provider(_)) if cancel.is_cancelled() => {
                            return finish_cancelled(&mut emit);
                        }
                        Err(TurnFailure::Provider(e)) => {
                            let overflow = e.is_context_overflow();
                            // Recovery compaction honors the user's
                            // `compaction_threshold: 0` (compaction
                            // off): with it off, the overflow error
                            // surfaces instead of rewriting history.
                            if overflow
                                && config.compaction_threshold > 0.0
                                && overflow_round < MAX_OVERFLOW_COMPACTIONS
                            {
                                overflow_round += 1;
                                match compact_with_keep(
                                    cx,
                                    provider,
                                    cancel,
                                    hooks,
                                    &mut emit,
                                    KEEP_RECENT >> overflow_round,
                                ) {
                                    Ok(true) => continue,
                                    Ok(false) => {}
                                    Err(kind) => {
                                        emit(LoopEvent::Error { kind: kind.clone() });
                                        return Err(kind);
                                    }
                                }
                            }
                            let exhausted = attempt >= config.max_provider_retries;
                            if exhausted || !e.is_transient() {
                                let kind = match e {
                                    ProviderError::Auth(message) => LoopError::Auth { message },
                                    ProviderError::RateLimited { retry_after, body } => {
                                        LoopError::RateLimited {
                                            message: body
                                                .unwrap_or_else(|| "too many requests".to_owned()),
                                            retry_after_secs: retry_after.map(|d| d.as_secs()),
                                        }
                                    }
                                    ProviderError::Http { status: 429, body } => {
                                        LoopError::RateLimited {
                                            message: format!(
                                                "provider returned status 429: {body}"
                                            ),
                                            retry_after_secs: None,
                                        }
                                    }
                                    _other if overflow => LoopError::ContextOverflow,
                                    other => LoopError::Provider {
                                        message: other.to_string(),
                                    },
                                };
                                emit(LoopEvent::Error { kind: kind.clone() });
                                return Err(kind);
                            }
                            attempt += 1;
                            let requested = e.retry_after();
                            let wait = retry_backoff(attempt, &e);
                            emit(LoopEvent::ProviderRetry {
                                attempt,
                                max_attempts: config.max_provider_retries,
                                wait_secs: wait.as_secs(),
                                // A backoff past u64::MAX ms is impossible in
                                // practice; saturation preserves intent.
                                wait_ms: u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                                requested_secs: requested.map(|d| d.as_secs()),
                                error: e.to_string(),
                            });
                            if !sleep_cancelable(cancel, wait) {
                                return finish_cancelled(&mut emit);
                            }
                        }
                    }
                }
            };

            cx.budget.add(turn.usage);
            let turn_usage = turn.usage;
            let assistant_id = turn.message.id;
            let pending = turn.tool_calls.clone();
            append(cx, &mut emit, turn.message);

            let had_tool_calls = !pending.is_empty();
            emit(LoopEvent::TurnEnded {
                index: turn_index,
                had_tool_calls,
            });
            let summary = crate::hooks::TurnSummary {
                index: turn_index,
                had_tool_calls,
                usage: turn_usage,
            };
            turn_index = turn_index.saturating_add(1);
            if hooks.should_stop_after_turn(&summary) {
                let unrun = unrun_results(pending, assistant_id, &mut emit);
                append_all(cx, &mut emit, unrun);
                return Ok(());
            }
            // Opt-in failsafe (LoopConfig::max_turns). Answering pending
            // calls keeps the every-tool_use-answered invariant; the
            // error, not Ok(()), tells the host the cap and not the
            // model ended the run.
            if config.max_turns.is_some_and(|max| turn_index >= max) {
                let unrun = unrun_results(pending, assistant_id, &mut emit);
                append_all(cx, &mut emit, unrun);
                let kind = LoopError::Other {
                    message: format!(
                        "turn limit reached: stopped after {turn_index} turns (max_turns)"
                    ),
                };
                emit(LoopEvent::Error { kind: kind.clone() });
                return Err(kind);
            }

            if !had_tool_calls {
                break;
            }

            let workdir = cx.workdir.clone();
            // Parallel dispatch when the loop is configured for it and no
            // tool in the batch overrides to Sequential (e.g. `shell`), or
            // when every tool in the batch declares itself Parallel.
            // A tool that runs alone (`swarm`) is refused here rather than
            // run beside others; see `dispatch::runs_alone_in_batch`.
            let mode_of = |name: &str| tools.get(name).and_then(|t| t.execution_mode());
            let any_sequential = pending
                .iter()
                .any(|call| mode_of(&call.name) == Some(kage_tools::ExecMode::Sequential));
            let all_parallel = pending
                .iter()
                .all(|call| mode_of(&call.name) == Some(kage_tools::ExecMode::Parallel));
            let dispatch = if (config.parallel_tools && !any_sequential) || all_parallel {
                dispatch_tool_calls_parallel
            } else {
                dispatch_tool_calls
            };
            let outcome = dispatch(
                pending.clone(),
                tools,
                &workdir,
                cancel,
                cx.confine_paths,
                &cx.history,
                assistant_id,
                hooks,
                &mut emit,
            );
            if let Some(kind) = outcome.error {
                // Every tool_use in the assistant message now has an answer
                // in `outcome.results`; append them so in-memory history and
                // the persisted session never carry a dangling tool_use.
                append_all(cx, &mut emit, outcome.results);
                emit(LoopEvent::Error { kind: kind.clone() });
                return Err(kind);
            }
            let results = outcome.results;
            // If every tool in the batch signaled `terminate`, persist the
            // results and exit the run cleanly. The loop never asks the
            // model for another turn, never dequeues a follow-up.
            if outcome.all_terminate {
                append_all(cx, &mut emit, results);
                return Ok(());
            }

            let mut steering = None;
            for (call, result) in pending.iter().zip(&results) {
                let is_error = matches!(
                    result.content.first(),
                    Some(kage_core::Content::ToolResultBlock { is_error: true, .. })
                );
                if let Some(msg) = doom.observe(&call.name, &call.input, is_error)
                    && let Some(msg) = hooks.on_doom_loop(&call.name, msg)
                {
                    steering = Some(msg);
                }
            }
            append_all(cx, &mut emit, results);
            if let Some(text) = steering {
                push_user_text(cx, &mut emit, text);
            }
        }

        let followup = match drain_messages(config.followup_mode, cancel, || hooks.get_followup()) {
            Ok(text) => text,
            Err(LoopError::Cancelled) => return finish_cancelled(&mut emit),
            Err(_) => unreachable!("drain_messages only fails with Cancelled"),
        };
        let Some(text) = followup else {
            return Ok(());
        };
        push_user_text(cx, &mut emit, text);
    }
}

/// Drain queued steering content from a hook poll according to `mode`.
/// In `OneAtATime`, polls once and returns at most the one entry. In
/// `All`, polls repeatedly until the hook returns `None`, then returns
/// every entry. A queue of plain text entries still lands as one user
/// message joined with blank lines (plus [`DRAIN_TRUNCATED_NOTE`] when
/// the [`MAX_DRAINED_MESSAGES`] cap drops overflow, so the truncation
/// is visible in history); mixed entries are injected one user message
/// each, with the note appended as its own message on cap.
///
/// Cancel is polled between polls; tripping it aborts the drain with
/// [`LoopError::Cancelled`].
fn drain_content<F: FnMut() -> Option<Vec<Content>>>(
    mode: SteeringMode,
    cancel: &CancelFlag,
    mut poll: F,
) -> Result<Vec<Vec<Content>>, LoopError> {
    if cancel.is_cancelled() {
        return Err(LoopError::Cancelled);
    }
    let Some(first) = poll() else {
        return Ok(Vec::new());
    };
    if mode == SteeringMode::OneAtATime {
        return Ok(vec![first]);
    }
    let mut items = vec![first];
    let mut capped = false;
    while let Some(next) = poll() {
        if cancel.is_cancelled() {
            return Err(LoopError::Cancelled);
        }
        if items.len() >= MAX_DRAINED_MESSAGES {
            capped = true;
            break;
        }
        items.push(next);
    }
    let all_text = items
        .iter()
        .all(|c| matches!(c.as_slice(), [Content::Text { .. }]));
    if all_text {
        let mut joined = String::new();
        for content in &items {
            if !joined.is_empty() {
                joined.push_str("\n\n");
            }
            let Some(Content::Text { text }) = content.first() else {
                continue;
            };
            joined.push_str(text);
        }
        if capped {
            joined.push_str("\n\n");
            joined.push_str(DRAIN_TRUNCATED_NOTE);
        }
        return Ok(vec![vec![Content::Text { text: joined }]]);
    }
    if capped {
        items.push(vec![Content::Text {
            text: DRAIN_TRUNCATED_NOTE.to_owned(),
        }]);
    }
    Ok(items)
}

/// Drain queued messages from a hook poll according to `mode`. In
/// `OneAtATime`, polls once and returns whatever the hook gave us. In
/// `All`, polls repeatedly until the hook returns `None`, then joins the
/// collected messages with blank-line separators into a single user
/// message.
///
/// The drain is bounded: at most [`MAX_DRAINED_MESSAGES`] messages are
/// consumed per call. A hook that keeps producing past the cap has its
/// overflow dropped for this turn, with a note appended to the joined
/// text so the truncation is visible in history and in the provider
/// request. Cancel is polled between polls; tripping it aborts the
/// drain with [`LoopError::Cancelled`].
///
/// Returns `None` when the hook had nothing to give on the first poll.
fn drain_messages<F: FnMut() -> Option<String>>(
    mode: SteeringMode,
    cancel: &CancelFlag,
    mut poll: F,
) -> Result<Option<String>, LoopError> {
    if cancel.is_cancelled() {
        return Err(LoopError::Cancelled);
    }
    let Some(first) = poll() else {
        return Ok(None);
    };
    if mode == SteeringMode::OneAtATime {
        return Ok(Some(first));
    }
    let mut out = first;
    let mut drained = 1usize;
    while let Some(next) = poll() {
        if cancel.is_cancelled() {
            return Err(LoopError::Cancelled);
        }
        if drained >= MAX_DRAINED_MESSAGES {
            out.push_str("\n\n");
            out.push_str(DRAIN_TRUNCATED_NOTE);
            break;
        }
        out.push_str("\n\n");
        out.push_str(&next);
        drained += 1;
    }
    Ok(Some(out))
}

/// Append `message` to history and announce it with
/// [`LoopEvent::MessageAppended`]. History and event share one
/// allocation.
fn append<F: FnMut(LoopEvent)>(cx: &mut AgentContext, emit: &mut F, message: Message) {
    let message = Arc::new(message);
    cx.history.push(Arc::clone(&message));
    emit(LoopEvent::MessageAppended { message });
}

fn append_all<F: FnMut(LoopEvent)>(cx: &mut AgentContext, emit: &mut F, messages: Vec<Message>) {
    for message in messages {
        append(cx, emit, message);
    }
}

/// Append a user message the loop injected (steering, follow-up, or a
/// doom-loop nudge).
fn push_user_content<F: FnMut(LoopEvent)>(
    cx: &mut AgentContext,
    emit: &mut F,
    content: Vec<Content>,
) {
    let message = Message::new(
        kage_core::Role::User,
        content,
        cx.history.last().map(|m| m.id),
    );
    append(cx, emit, message);
}

/// Append a user text message that the loop injected (follow-up
/// hooks and doom-loop nudges).
fn push_user_text<F: FnMut(LoopEvent)>(cx: &mut AgentContext, emit: &mut F, text: String) {
    push_user_content(cx, emit, vec![Content::Text { text }]);
}

/// Rewrite persisted `Content::Thinking` blocks into inline
/// `<thinking>...</thinking>` text before a request is built.
///
/// Thinking blocks are not portable across providers: a native block
/// needs the signature of the provider and model that produced it, and
/// providers without thinking input drop unknown content silently,
/// losing the reasoning chain outright. Flattening historical thinking
/// to plain text keeps the reasoning visible to whatever provider runs
/// the next turn, regardless of which produced it. Providers that send
/// thinking back themselves opt out via [`Provider::preserves_thinking`].
///
/// Only persisted history is touched. The in-flight assistant turn is
/// not appended to `cx.history` until after it has streamed, so live
/// thinking deltas reach the UI unmodified.
fn flatten_thinking(history: &[Arc<Message>]) -> Vec<Arc<Message>> {
    history
        .iter()
        .map(|msg| {
            if !msg
                .content
                .iter()
                .any(|c| matches!(c, Content::Thinking { .. }))
            {
                return Arc::clone(msg);
            }
            // Build the rewritten content directly instead of
            // `(**msg).clone()` then overwriting `.content`: the other
            // fields are all `Copy`, so `..**msg` copies them and the
            // original content vec is never cloned just to be dropped.
            let content = msg
                .content
                .iter()
                .filter_map(|c| match c {
                    Content::Thinking { text, .. } => {
                        Content::flattened_thinking(text).map(|text| Content::Text { text })
                    }
                    other => Some(other.clone()),
                })
                .collect();
            Arc::new(Message { content, ..**msg })
        })
        .collect()
}

/// Construct the next [`StreamRequest`] from the current agent context.
fn build_request(
    cx: &AgentContext,
    tools: &ToolRegistry,
    provider: &dyn Provider,
) -> StreamRequest {
    let history = if provider.preserves_thinking() {
        cx.history.clone()
    } else {
        flatten_thinking(&cx.history)
    };
    let mut req = StreamRequest::new(&cx.model, close_unanswered_calls(history));
    if !cx.system_prompt.is_empty() {
        req.system = Some(cx.system_prompt.clone());
    }
    req.tools = tools.list_for_provider();
    req.max_output_tokens = cx
        .max_output_tokens
        .map(|ceiling| fit_output_ceiling(cx, ceiling));
    req.level = cx.reasoning.resolve(cx.thinking_level);
    req.reasoning = cx.reasoning;
    req.produces = cx.produces;
    req.accepts = cx.accepts;
    req.session.clone_from(&cx.session);
    req
}

/// What a call that never got a result answers with in a request.
const INTERRUPTED: &str = "Interrupted: kage stopped before this call finished.";

/// `history` with a failed result after every tool call that has none.
/// A session that kage quit in the middle of a call (a swarm member
/// reopened for a retry, a resumed main session) still ends on that
/// call, and providers refuse a call without its result. The stored
/// history keeps the gap, which clients show as an interrupted call.
fn close_unanswered_calls(history: Vec<Arc<Message>>) -> Vec<Arc<Message>> {
    let answered: HashSet<&ToolCallId> = history
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResultBlock { call_id, .. } => Some(call_id),
            _ => None,
        })
        .collect();
    let unanswered = |message: &Message| -> Vec<ToolCallId> {
        message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolCall { id, .. } if !answered.contains(id) => Some(id.clone()),
                _ => None,
            })
            .collect()
    };
    if history.iter().all(|message| unanswered(message).is_empty()) {
        return history;
    }
    let mut closed = Vec::with_capacity(history.len() + 1);
    let mut open: Vec<ToolCallId> = Vec::new();
    let mut parent = None;
    let close =
        |open: &mut Vec<ToolCallId>, parent: Option<MessageId>, closed: &mut Vec<Arc<Message>>| {
            closed.extend(open.drain(..).map(|call_id| {
                Arc::new(Message::new(
                    Role::ToolResult,
                    vec![Content::ToolResultBlock {
                        call_id,
                        output: INTERRUPTED.to_owned(),
                        is_error: true,
                    }],
                    parent,
                ))
            }));
        };
    for message in &history {
        if message.role != Role::ToolResult {
            close(&mut open, parent, &mut closed);
        }
        if message.role == Role::Assistant {
            open = unanswered(message);
            parent = Some(message.id);
        }
        closed.push(Arc::clone(message));
    }
    close(&mut open, parent, &mut closed);
    closed
}

fn finish_cancelled<F: FnMut(LoopEvent)>(emit: &mut F) -> Result<(), LoopError> {
    emit(LoopEvent::Error {
        kind: LoopError::Cancelled,
    });
    Err(LoopError::Cancelled)
}

/// Issue one streaming attempt: open the provider stream and drain it
/// into a finished turn. A pre-stream failure and a mid-stream failure
/// are unified into the same [`TurnFailure`] so the caller's retry
/// logic does not care which phase broke.
fn stream_one_attempt<F: FnMut(LoopEvent)>(
    provider: &dyn Provider,
    req: StreamRequest,
    parent: Option<MessageId>,
    cancel: &CancelFlag,
    emit: &mut F,
) -> Result<TurnResult, TurnFailure> {
    let model = req.model.clone();
    let stream = provider
        .stream(req, cancel)
        .map_err(TurnFailure::Provider)?;
    collect_turn(parent, &model, stream, cancel, emit)
}

/// Backoff before retry `attempt` (1-based). A provider `retry_after`
/// hint wins (capped at 60s so a hostile header cannot park the loop);
/// otherwise exponential 1s, 2s, 4s, 8s, 16s capped at 30s.
fn retry_backoff(attempt: u32, err: &ProviderError) -> Duration {
    if let Some(hint) = err.retry_after() {
        return hint.min(Duration::from_secs(60));
    }
    let secs = 1u64 << attempt.saturating_sub(1).min(5);
    Duration::from_secs(secs.min(30))
}

/// Sleep `dur`, returning `false` if `cancel` tripped during the wait.
/// The wait wakes the moment the flag is set, so a cancel aborts the
/// backoff at once instead of after the full delay.
fn sleep_cancelable(cancel: &CancelFlag, dur: Duration) -> bool {
    if cancel.is_cancelled() {
        return false;
    }
    cancel.watch().receiver().recv_timeout(dur).is_err() && !cancel.is_cancelled()
}

#[cfg(test)]
mod retry_display {
    use std::sync::Arc;

    use kage_core::{CancelFlag, TokenUsage};
    use kage_provider::testing::{MockProvider, user_msg};
    use kage_provider::{ProviderError, ProviderEvent, StopReason};
    use kage_tools::ToolRegistry;

    use super::*;
    use crate::NoopHooks;

    fn good_turn() -> Vec<Result<ProviderEvent, ProviderError>> {
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta { delta: "hi".into() }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]
    }

    /// A 1 ms `retry_after` used to display as "in 1s"; the event must
    /// carry the raw sub-second wait instead.
    #[test]
    fn a_sub_second_hint_is_not_rounded_up_to_a_second() {
        let mock = MockProvider::sequence(vec![
            vec![Err(ProviderError::RateLimited {
                retry_after: Some(Duration::from_millis(1)),
                body: None,
            })],
            good_turn(),
        ]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(Arc::new(user_msg("hello")));
        let cancel = CancelFlag::new();
        let events = std::cell::RefCell::new(Vec::new());
        let res = run(
            &mock,
            &ToolRegistry::new(),
            &mut cx,
            LoopConfig::default(),
            &mut NoopHooks,
            &cancel,
            |e| events.borrow_mut().push(e),
        );
        assert!(res.is_ok(), "{res:?}");
        let wait = events
            .into_inner()
            .iter()
            .find_map(|e| match e {
                LoopEvent::ProviderRetry { wait_secs, .. } => Some(*wait_secs),
                _ => None,
            })
            .expect("retry event");
        assert_eq!(wait, 0, "a 1 ms hint must not display as 1s");
    }
}

#[cfg(test)]
mod tests;
