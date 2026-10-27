//! Dispatch of tool calls produced by one assistant turn.
//!
//! Walks the [`PendingToolCall`] list from [`crate::stream::collect_turn`],
//! consults [`Hooks::before_tool_call`] for short-circuit, emits a
//! [`LoopEvent::ToolExecutionStart`] and executes the tool through the
//! registry, runs the result through [`Hooks::after_tool_call`],
//! emits a [`LoopEvent::ToolCallEnd`], and produces one tool-result message
//! per call to append to history. Calls run one at a time or, for a
//! parallel batch, on a bounded worker pool. A parallel call is finished
//! as soon as it completes, so its `ToolCallEnd` follows completion
//! order, while the result messages always keep input order.
//!
//! Dispatch is infallible: a call that never produced an output (cancel,
//! or a tool failure the loop cannot recover from) gets a synthesized
//! `is_error` result. The assistant message's `tool_use` blocks are
//! therefore always answered, in memory and in the persisted session, so a
//! resumed run never sends a provider a dangling `tool_use`.

use std::path::Path;
use std::sync::Arc;

use crossbeam_channel::Sender;
use kage_core::event::TOOL_CANCELLED_TEXT;
use kage_core::{
    CancelFlag, Content, LoopError, LoopEvent, MediaSource, Message, MessageId, Modality, Role,
    ToolCallId, ToolOutput, ToolUpdate,
};
use kage_tools::{ProgressSink, ToolContext, ToolError, ToolRegistry};

use crate::Hooks;
use crate::stream::PendingToolCall;

/// Maximum worker threads per dispatch batch. Oversized batches run in
/// waves; result placement is index-keyed, so waves never reorder
/// messages.
const MAX_TOOL_WORKERS: usize = 8;
/// Capacity of the progress channel from tool threads to the loop
/// thread. A chattier tool blocks on its own emit (backpressure)
/// instead of growing the queue without bound while a slow host sink
/// consumes the events.
const PROGRESS_CAPACITY: usize = 256;

/// How long a cancelled batch waits for in-flight tools to report their
/// real results before the unreported calls are answered with
/// `Cancelled`. Tools that honor the cancel flag finish well inside
/// this window; a tool that ignores it only delays the batch by this
/// much, never blocks it.
const CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// One recv slice inside the grace window, so the overall deadline is
/// checked often.
const GRACE_SLICE: std::time::Duration = std::time::Duration::from_millis(50);

/// Message from a tool thread to the dispatching loop thread.
enum Progress {
    Update(ToolCallId, ToolUpdate),
    Done(usize, Result<ToolOutput, LoopError>),
}

/// Progress sink handed to one tool call. Forwards each update to the loop
/// thread, which emits it as a [`LoopEvent::ToolUpdate`] while the tool is
/// still running.
struct ChannelSink {
    id: ToolCallId,
    tx: Sender<Progress>,
}

impl ProgressSink for ChannelSink {
    fn emit(&self, update: ToolUpdate) {
        let _ = self.tx.send(Progress::Update(self.id.clone(), update));
    }
}

/// Reports the result of the call at `index`. When dropped without a
/// report, as on panic, it reports an error for that call instead.
struct DoneOnDrop {
    index: usize,
    tx: Sender<Progress>,
    reported: bool,
}

impl DoneOnDrop {
    fn report(mut self, result: Result<ToolOutput, LoopError>) {
        self.reported = true;
        let _ = self.tx.send(Progress::Done(self.index, result));
    }
}

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        if !self.reported {
            let _ = self.tx.send(Progress::Done(
                self.index,
                Err(LoopError::Other {
                    message: "tool thread panicked".into(),
                }),
            ));
        }
    }
}

/// One queued tool call handed to a pool worker.
struct Job {
    index: usize,
    call: PendingToolCall,
}

/// Execute `calls` on a bounded worker pool and emit their progress live.
///
/// Emits a [`LoopEvent::ToolExecutionStart`] per call first. The calls are
/// queued in input order to at most [`MAX_TOOL_WORKERS`] worker threads;
/// the calling thread forwards every update as a
/// [`LoopEvent::ToolUpdate`] and hands each result to `done` with the
/// call's index in `calls` as soon as that call completes, so `emit` and
/// `done` stay on the loop thread. A panicking tool yields an error for
/// its own call.
///
/// The wait selects on both the progress channel and `cancel`'s watch, so
/// a tool that ignores cancellation cannot park the loop: on cancel the
/// batch returns at once, every unfinished index gets a synthesized
/// [`LoopError::Cancelled`] through `done`, and the workers are left to
/// finish in the background (their late results land in a dropped
/// receiver and are ignored).
fn execute_live<F, D>(
    calls: &[&PendingToolCall],
    tools: &ToolRegistry,
    workdir: &Path,
    cancel: &CancelFlag,
    confine_paths: bool,
    emit: &mut F,
    mut done: D,
) where
    F: FnMut(LoopEvent),
    D: FnMut(&mut F, usize, Result<ToolOutput, LoopError>),
{
    if calls.is_empty() {
        return;
    }
    for call in calls {
        emit(LoopEvent::ToolExecutionStart {
            id: call.id.clone(),
        });
    }
    let (tx, rx) = crossbeam_channel::bounded(PROGRESS_CAPACITY);
    let (job_tx, job_rx) = crossbeam_channel::unbounded::<Job>();
    for (index, call) in calls.iter().enumerate() {
        let _ = job_tx.send(Job {
            index,
            call: (*call).clone(),
        });
    }
    drop(job_tx);

    // Workers own their state so a cancelled batch can return while a
    // straggler tool finishes in the background.
    let owned_tools = tools.clone();
    let owned_workdir = workdir.to_path_buf();
    let owned_cancel = cancel.clone();
    let worker_count = calls.len().min(MAX_TOOL_WORKERS);
    for _ in 0..worker_count {
        let job_rx = job_rx.clone();
        let tx = tx.clone();
        let tools = owned_tools.clone();
        let workdir = owned_workdir.clone();
        let cancel = owned_cancel.clone();
        std::thread::spawn(move || {
            for job in job_rx {
                // Work queued behind a cancel never starts: the batch
                // has already been answered with synthesized results.
                if cancel.is_cancelled() {
                    break;
                }
                let sink = Arc::new(ChannelSink {
                    id: job.call.id.clone(),
                    tx: tx.clone(),
                });
                let reporter = DoneOnDrop {
                    index: job.index,
                    tx: tx.clone(),
                    reported: false,
                };
                // A panicking tool must not kill the worker and strand
                // the jobs queued behind it.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    execute(
                        &tools,
                        &job.call,
                        &workdir,
                        &cancel,
                        confine_paths,
                        Some(sink),
                    )
                }));
                reporter.report(result.unwrap_or_else(|_| {
                    Err(LoopError::Other {
                        message: "tool thread panicked".into(),
                    })
                }));
            }
        });
    }
    drop(tx);

    let cancel_watch = cancel.watch();
    let mut finished = vec![false; calls.len()];
    let mut remaining = calls.len();
    let cancelled = loop {
        if cancel.is_cancelled() {
            break true;
        }
        crossbeam_channel::select! {
            recv(cancel_watch.receiver()) -> _ => break true,
            recv(rx) -> message => match message {
                Ok(Progress::Update(id, update)) => {
                    emit(LoopEvent::ToolUpdate { id, update });
                }
                Ok(Progress::Done(index, result)) => {
                    finished[index] = true;
                    remaining -= 1;
                    done(emit, index, result);
                    if remaining == 0 {
                        break false;
                    }
                }
                Err(_) => break true,
            },
        }
    };
    if cancelled {
        drain_cancel_grace(&rx, &mut finished, &mut remaining, emit, &mut done);
    }
}

/// Answer a cancelled batch: give in-flight tools a short grace window to
/// land their real results, then answer every unreported call with
/// [`LoopError::Cancelled`]. The return happens without joining:
/// stragglers finish in the background.
fn drain_cancel_grace<F, D>(
    rx: &crossbeam_channel::Receiver<Progress>,
    finished: &mut [bool],
    remaining: &mut usize,
    emit: &mut F,
    done: &mut D,
) where
    F: FnMut(LoopEvent),
    D: FnMut(&mut F, usize, Result<ToolOutput, LoopError>),
{
    let deadline = std::time::Instant::now() + CANCEL_GRACE;
    while *remaining > 0 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left.min(GRACE_SLICE)) {
            Ok(Progress::Update(id, update)) => {
                emit(LoopEvent::ToolUpdate { id, update });
            }
            Ok(Progress::Done(index, result)) => {
                if !finished[index] {
                    finished[index] = true;
                    *remaining -= 1;
                    done(emit, index, result);
                }
            }
            Err(_) => break,
        }
    }
    for (index, reported) in finished.iter().enumerate() {
        if !reported {
            done(emit, index, Err(LoopError::Cancelled));
        }
    }
}

/// Outcome of [`Hooks::before_tool_call`] for one entry: either a
/// short-circuit output the host produced, or run the real tool.
enum Slot {
    Short(ToolOutput),
    Run,
}

/// Result of one batch of tool dispatch.
///
/// `results` is the message list to append to history, one per call, real
/// or synthesized; `all_terminate` is `true` when every tool in the batch
/// returned `ToolOutput::terminate` so the loop can stop cleanly after
/// appending the results. `error` carries the first unrecoverable failure
/// (cancel, or a tool error the loop cannot convert to an output); when it
/// is set, the trailing synthesized results explain the abort.
pub(crate) struct DispatchOutcome {
    pub results: Vec<Message>,
    pub all_terminate: bool,
    pub error: Option<LoopError>,
}

/// Tool output standing in for a call that never produced one because the
/// batch aborted (cancel or unrecoverable error). `is_error` so the model
/// sees the call did not run; the text says why.
fn synthesized_output(error: &LoopError) -> ToolOutput {
    let text = match error {
        LoopError::Cancelled => TOOL_CANCELLED_TEXT.to_owned(),
        other => format!("tool did not run: {other}"),
    };
    ToolOutput {
        is_error: true,
        text,
        structured: None,
        terminate: false,
        media: Vec::new(),
    }
}

/// Cap a finished output for events and history. Text goes through
/// [`kage_core::cap_tool_result`]; a structured payload whose serialized
/// form exceeds [`kage_core::MAX_TOOL_RESULT_BYTES`] is dropped, so a
/// multi-megabyte value can neither bloat memory nor travel in the
/// `ToolCallEnd` event. History itself only ever carries the capped text.
fn cap_output(output: ToolOutput) -> ToolOutput {
    let structured = output.structured.filter(|value| {
        serde_json::to_string(value)
            .is_ok_and(|json| json.len() <= kage_core::MAX_TOOL_RESULT_BYTES)
    });
    ToolOutput {
        text: kage_core::cap_tool_result(output.text),
        structured,
        ..output
    }
}

/// Output for a call whose tool must own its message, refused because
/// the batch holds other calls alongside it.
fn alone_output(name: &str) -> ToolOutput {
    ToolOutput {
        is_error: true,
        text: format!(
            "a {name} call must be the only tool call in the message; \
             move it to a message of its own"
        ),
        structured: None,
        terminate: false,
        media: Vec::new(),
    }
}

/// Whether `call`'s tool refuses to share its message, and the batch
/// actually holds something else.
fn runs_alone_in_batch(tools: &ToolRegistry, call: &PendingToolCall, batch: usize) -> bool {
    batch > 1 && tools.get(&call.name).is_some_and(|tool| tool.runs_alone())
}

/// Answer every call in `pending` without running it, so history never
/// carries a dangling tool use when the run stops after a turn.
pub(crate) fn unrun_results<F: FnMut(LoopEvent)>(
    pending: Vec<PendingToolCall>,
    parent: MessageId,
    emit: &mut F,
) -> Vec<Message> {
    pending
        .into_iter()
        .map(|call| {
            let output = ToolOutput {
                is_error: true,
                text: "tool did not run: the run stopped after this turn".to_owned(),
                structured: None,
                terminate: false,
                media: Vec::new(),
            };
            emit(LoopEvent::ToolCallEnd {
                id: call.id.clone(),
                output: output.clone(),
            });
            Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: call.id,
                    output: output.text,
                    is_error: true,
                }],
                Some(parent),
            )
        })
        .collect()
}

/// The history message holding `call_id`'s `output`: its result block,
/// then each image and audio clip the tool produced, so it is stored,
/// shown and sent with the result. A kind of media history cannot hold
/// yet is named in the result text instead.
fn result_message(call_id: ToolCallId, output: ToolOutput, parent: MessageId) -> Message {
    let mut text = output.text;
    let mut media = Vec::new();
    for part in output.media {
        match Modality::of_mime(&part.mime) {
            Some(Modality::Image) => media.push(Content::Image {
                source: MediaSource::Base64 { data: part.data },
                mime: part.mime,
            }),
            Some(Modality::Audio) => media.push(Content::Audio {
                source: MediaSource::Base64 { data: part.data },
                mime: part.mime,
            }),
            _ => {
                let _ = std::fmt::Write::write_fmt(
                    &mut text,
                    format_args!("\n[{} output is not kept]", part.mime),
                );
            }
        }
    }
    let mut content = vec![Content::ToolResultBlock {
        call_id,
        output: text,
        is_error: output.is_error,
    }];
    content.extend(media);
    Message::new(Role::ToolResult, content, Some(parent))
}

/// Record the batch-level failure worth surfacing to the caller.
///
/// `Cancelled` wins because the run is aborting by user request; otherwise
/// the first error sticks.
fn record_batch_error(slot: &mut Option<LoopError>, kind: &LoopError) {
    let cancelled = matches!(kind, LoopError::Cancelled);
    if matches!(slot, Some(LoopError::Cancelled)) && !cancelled {
        return;
    }
    if slot.is_none() || cancelled {
        *slot = Some(kind.clone());
    }
}

/// Dispatch every pending tool call sequentially.
///
/// Returns one tool-result [`Message`] per call, in input order. The caller
/// appends them to history before continuing the inner loop.
///
/// Cancellation: polled before every call. On cancel, or on a tool error
/// the loop cannot recover from, the failing call and every remaining call
/// get synthesized `is_error` results, the completed results are kept, and
/// the failure is carried in [`DispatchOutcome::error`].
#[expect(
    clippy::too_many_arguments,
    reason = "the loop hands over its run state unbundled"
)]
pub(crate) fn dispatch_tool_calls<F: FnMut(LoopEvent)>(
    pending: Vec<PendingToolCall>,
    tools: &ToolRegistry,
    workdir: &Path,
    cancel: &CancelFlag,
    confine_paths: bool,
    parent: MessageId,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> DispatchOutcome {
    let batch = pending.len();
    let mut results = Vec::with_capacity(pending.len());
    let mut all_terminate = !pending.is_empty();
    let mut error: Option<LoopError> = None;
    for call in pending {
        let raw = if error.is_none() && !cancel.is_cancelled() {
            let pre = hooks.before_tool_call(&call.id, &call.name, &call.input);
            if let Some(out) = pre {
                Some(out)
            } else if runs_alone_in_batch(tools, &call, batch) {
                Some(alone_output(&call.name))
            } else {
                let mut result = None;
                execute_live(
                    &[&call],
                    tools,
                    workdir,
                    cancel,
                    confine_paths,
                    emit,
                    |_, _, done| result = Some(done),
                );
                match result.expect("one call yields one result") {
                    Ok(out) => Some(out),
                    Err(kind) => {
                        record_batch_error(&mut error, &kind);
                        None
                    }
                }
            }
        } else {
            // The call did not run because the batch already failed or
            // the run was cancelled. Record the cancel only when it is
            // the actual cause: the synthetic bookkeeping entry must not
            // overwrite a concrete error from an earlier call.
            if error.is_none() {
                record_batch_error(&mut error, &LoopError::Cancelled);
            }
            None
        };

        let output = match raw {
            Some(out) => hooks.after_tool_call(&call.name, out),
            None => hooks.after_tool_call(
                &call.name,
                synthesized_output(error.as_ref().unwrap_or(&LoopError::Cancelled)),
            ),
        };
        let output = cap_output(output);
        all_terminate &= output.terminate;

        emit(LoopEvent::ToolCallEnd {
            id: call.id.clone(),
            output: output.clone(),
        });

        results.push(result_message(call.id, output, parent));
    }
    DispatchOutcome {
        results,
        all_terminate,
        error,
    }
}

/// Dispatch tool calls in parallel on the bounded worker pool.
///
/// Hooks (`before_tool_call`, `after_tool_call`) stay on the calling thread;
/// only the tool's `execute` runs concurrently. Each call is finished on
/// the calling thread as soon as it completes: `after_tool_call` runs and
/// its [`LoopEvent::ToolCallEnd`] is emitted and its result message built in
/// completion order, so each message carries its own call's end time. Result
/// message order is preserved to match the input order, regardless of
/// completion order.
///
/// Calls that get short-circuited by `before_tool_call` skip thread
/// dispatch entirely and are finished before the others start. The
/// remaining calls queue onto at most [`MAX_TOOL_WORKERS`] worker
/// threads and run in waves; the function blocks until the last one
/// completes.
///
/// A call whose thread reports cancel or panic gets a synthesized
/// `is_error` result; every call that did produce an output keeps it. The
/// batch-level failure (cancel preferred over panic, first otherwise) is
/// carried in [`DispatchOutcome::error`]. On cancel the batch returns as
/// soon as the cancel is observed, with synthesized results for every
/// unfinished call, while straggler tools finish in the background.
#[expect(
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    reason = "matches dispatch_tool_calls"
)]
pub(crate) fn dispatch_tool_calls_parallel<F: FnMut(LoopEvent)>(
    pending: Vec<PendingToolCall>,
    tools: &ToolRegistry,
    workdir: &Path,
    cancel: &CancelFlag,
    confine_paths: bool,
    parent: MessageId,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> DispatchOutcome {
    let batch = pending.len();
    // Resolve hook short-circuits up front, single-threaded. Skipped
    // entirely when the batch is already cancelled: nothing will run.
    let entry_cancelled = cancel.is_cancelled();
    let mut slots: Vec<Slot> = Vec::with_capacity(pending.len());
    if !entry_cancelled {
        for call in &pending {
            match hooks.before_tool_call(&call.id, &call.name, &call.input) {
                Some(out) => slots.push(Slot::Short(out)),
                None if runs_alone_in_batch(tools, call, batch) => {
                    slots.push(Slot::Short(alone_output(&call.name)));
                }
                None => slots.push(Slot::Run),
            }
        }
    }

    let mut error = entry_cancelled.then_some(LoopError::Cancelled);
    let mut all_terminate = !pending.is_empty();
    let mut results: Vec<Option<Message>> = std::iter::repeat_with(|| None)
        .take(pending.len())
        .collect();
    let mut finish = |emit: &mut F, index: usize, raw: Result<ToolOutput, LoopError>| {
        let call = &pending[index];
        let output = match raw {
            Ok(out) => hooks.after_tool_call(&call.name, out),
            Err(kind) => {
                record_batch_error(&mut error, &kind);
                hooks.after_tool_call(&call.name, synthesized_output(&kind))
            }
        };
        let output = cap_output(output);
        all_terminate &= output.terminate;
        emit(LoopEvent::ToolCallEnd {
            id: call.id.clone(),
            output: output.clone(),
        });
        results[index] = Some(result_message(call.id.clone(), output, parent));
    };

    if entry_cancelled {
        for index in 0..pending.len() {
            finish(emit, index, Err(LoopError::Cancelled));
        }
    } else {
        let mut to_run = Vec::new();
        for (index, slot) in slots.into_iter().enumerate() {
            match slot {
                Slot::Short(out) => finish(emit, index, Ok(out)),
                Slot::Run => to_run.push(index),
            }
        }
        let calls: Vec<&PendingToolCall> = to_run.iter().map(|&index| &pending[index]).collect();
        execute_live(
            &calls,
            tools,
            workdir,
            cancel,
            confine_paths,
            emit,
            |emit, index, raw| finish(emit, to_run[index], raw),
        );
    }

    DispatchOutcome {
        results: results
            .into_iter()
            .map(|result| result.expect("every call is finished"))
            .collect(),
        all_terminate,
        error,
    }
}

/// Execute a single tool through the registry, mapping errors to outputs.
///
/// Cancellation surfaces as `Err(LoopError::Cancelled)`; every other tool
/// error is converted to `Ok(ToolOutput { is_error: true, ... })` so the
/// model can observe the failure and adapt rather than terminating the run.
fn execute(
    tools: &ToolRegistry,
    call: &PendingToolCall,
    workdir: &Path,
    cancel: &CancelFlag,
    confine_paths: bool,
    progress: Option<Arc<dyn ProgressSink>>,
) -> Result<ToolOutput, LoopError> {
    let Some(tool) = tools.get(&call.name) else {
        // Name the alternatives: models reach for tools from their
        // training data (`bash`, `todowrite`, `websearch`), and a bare
        // refusal leaves them retrying the same unknown name.
        let known = tools.names().collect::<Vec<_>>().join(", ");
        return Ok(ToolOutput {
            is_error: true,
            text: format!(
                "tool '{}' is not registered. Available tools: {known}",
                call.name
            ),
            structured: None,
            terminate: false,
            media: Vec::new(),
        });
    };

    let mut cx = ToolContext::new(workdir, cancel).with_call_id(&call.id);
    if confine_paths {
        cx = cx.with_confine();
    }
    if let Some(sink) = progress {
        cx = cx.with_progress(sink);
    }
    match tool.execute(call.input.clone(), &cx) {
        Ok(out) => Ok(out),
        Err(ToolError::Cancelled) => Err(LoopError::Cancelled),
        Err(err) => Ok(ToolOutput {
            is_error: true,
            text: err.to_string(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        }),
    }
}

#[cfg(test)]
mod tests;
