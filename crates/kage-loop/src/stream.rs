//! Translate a provider's [`ProviderEvent`] stream into a finished
//! [`Message`] plus a list of tool calls awaiting dispatch.
//!
//! Provider events are narrower than loop events: providers emit fine-grained
//! deltas and stop reasons, while the loop emits a higher-level alphabet
//! that includes executed tool outputs. This module owns the translation and
//! the message-assembly state machine.

use std::time::Instant;

use kage_core::{
    Content, LoopError, LoopEvent, MediaSource, Message, MessageId, Modality, Role,
    ThinkingSignature, TokenUsage, ToolCallId,
};
use kage_provider::{EventStream, ProviderError, ProviderEvent};

/// Output of consuming one provider stream.
#[derive(Debug)]
pub(crate) struct TurnResult {
    /// Fully assembled assistant message ready to append to history.
    pub(crate) message: Message,
    /// Tool calls the model emitted, in order. Awaiting dispatch.
    pub(crate) tool_calls: Vec<PendingToolCall>,
    /// Token usage reported for the turn.
    pub(crate) usage: TokenUsage,
}

/// Why a single turn's stream did not produce a finished message.
///
/// The split is what makes auto-retry possible: a [`Provider`] error
/// carries the original [`ProviderError`] so the run loop can ask
/// [`ProviderError::is_transient`] whether re-issuing the identical
/// request might succeed, while [`Fatal`] errors (cancellation, a
/// malformed stream the assembler rejected) are never worth retrying.
///
/// [`Provider`]: TurnFailure::Provider
/// [`Fatal`]: TurnFailure::Fatal
#[derive(Debug)]
pub(crate) enum TurnFailure {
    /// Provider/transport-origin failure (pre- or mid-stream),
    /// including a stream that ended before its terminal frame.
    Provider(ProviderError),
    /// Non-retryable: cancellation or an assembler-rejected stream.
    Fatal(LoopError),
}

/// One tool invocation requested by the model and not yet executed.
#[derive(Clone, Debug)]
pub(crate) struct PendingToolCall {
    pub(crate) id: ToolCallId,
    pub(crate) name: String,
    pub(crate) input: serde_json::Value,
}

/// Drain `stream` into a finished message + tool-call manifest, emitting
/// [`LoopEvent`]s along the way. Thinking signatures are stamped with
/// `model`, the model the request went to. Each thinking block records
/// how long it ran, from its first delta to the event that closes it.
///
/// The cancellation flag is polled between provider events. If it trips,
/// the iterator is dropped (which signals the underlying HTTP request to
/// abort via the provider implementation) and `Err(LoopError::Cancelled)`
/// is returned.
pub(crate) fn collect_turn<F: FnMut(LoopEvent)>(
    parent: Option<MessageId>,
    model: &str,
    stream: EventStream,
    cancel: &kage_core::CancelFlag,
    emit: &mut F,
) -> Result<TurnResult, TurnFailure> {
    let mut assembler = Assembler::new(parent, model);
    let mut started = false;

    for event in stream {
        if cancel.is_cancelled() {
            return Err(TurnFailure::Fatal(LoopError::Cancelled));
        }
        let event = event.map_err(TurnFailure::Provider)?;

        if let Some(result) =
            handle_event(event, &mut assembler, &mut started, emit).map_err(TurnFailure::Fatal)?
        {
            return Ok(result);
        }
    }

    // A clean EOF with no terminal frame is a stream that dropped
    // before completing - the same recoverable failure class as a
    // mid-body transport error, so route it through transient retry.
    Err(TurnFailure::Provider(ProviderError::Transport(
        "stream ended before completion".into(),
    )))
}

fn handle_event<F: FnMut(LoopEvent)>(
    event: ProviderEvent,
    assembler: &mut Assembler,
    started: &mut bool,
    emit: &mut F,
) -> Result<Option<TurnResult>, LoopError> {
    let id = assembler.message_id;
    let mut ensure_started = |emit: &mut F| {
        if !*started {
            *started = true;
            emit(LoopEvent::MessageStart { id });
        }
    };

    match event {
        ProviderEvent::MessageStart => {
            ensure_started(emit);
        }
        ProviderEvent::TextDelta { delta } => {
            ensure_started(emit);
            assembler.push_text(&delta);
            emit(LoopEvent::TextDelta { id, delta });
        }
        ProviderEvent::ThinkingDelta { delta } => {
            ensure_started(emit);
            assembler.push_thinking(&delta);
            emit(LoopEvent::ThinkingDelta { id, delta });
        }
        ProviderEvent::Media { kind, mime, data } => {
            ensure_started(emit);
            assembler.push_media(kind, &mime, &data);
            emit(LoopEvent::Media {
                id,
                kind,
                mime,
                data,
            });
        }
        ProviderEvent::ThinkingSignature { data } => assembler.sign_thinking(data),
        ProviderEvent::RedactedThinking { data } => assembler.push_redacted(data),
        ProviderEvent::ToolCallStart { id: call_id, name } => {
            ensure_started(emit);
            emit(LoopEvent::ToolCallArgsDelta {
                id: call_id.clone(),
                name: name.clone(),
                input_partial: serde_json::json!({}),
            });
            assembler.begin_tool(call_id, name);
        }
        ProviderEvent::ToolCallArgsDelta {
            id: call_id,
            partial,
        } => {
            let acc = assembler.partial_args.entry(call_id.clone()).or_default();
            acc.push_str(&partial);
            // Tool-call arguments are a JSON object/array; the
            // accumulated buffer can only parse once a closing
            // delimiter has arrived. Gating on that avoids re-parsing
            // the whole growing buffer on every delta (quadratic over
            // the stream). Worst case for a value containing a literal
            // `}`/`]` is an occasional extra parse, never a missed
            // final one.
            let maybe_complete = matches!(acc.trim_end().as_bytes().last(), Some(b'}' | b']'));
            let parsed = maybe_complete
                .then(|| serde_json::from_str::<serde_json::Value>(acc.as_str()).ok())
                .flatten();
            if let Some(value) = parsed
                && let Some(name) = assembler.pending_tools.get(&call_id)
            {
                let ev = LoopEvent::ToolCallArgsDelta {
                    id: call_id.clone(),
                    name: name.clone(),
                    input_partial: value,
                };
                emit(ev);
            }
        }
        ProviderEvent::ToolCallEnd { id: call_id, input } => {
            assembler.partial_args.remove(&call_id);
            let (call_id, name, input) = assembler.complete_tool(call_id, input)?;
            emit(LoopEvent::ToolCallStart {
                id: call_id.clone(),
                name: name.clone(),
                input_partial: input.clone(),
            });
            assembler.tool_calls.push(PendingToolCall {
                id: call_id,
                name,
                input,
            });
        }
        ProviderEvent::MessageEnd { usage, stop_reason } => {
            ensure_started(emit);
            emit(LoopEvent::MessageEnd {
                id,
                usage,
                stop_reason,
            });
            return Ok(Some(assembler.finish(usage)));
        }
    }
    Ok(None)
}

/// Mutable message-assembly state for one turn.
struct Assembler {
    message_id: MessageId,
    parent: Option<MessageId>,
    model: String,
    blocks: Vec<Content>,
    pending_tools: std::collections::HashMap<ToolCallId, String>,
    partial_args: std::collections::HashMap<ToolCallId, String>,
    tool_calls: Vec<PendingToolCall>,
    thinking_since: Option<Instant>,
}

impl Assembler {
    fn new(parent: Option<MessageId>, model: &str) -> Self {
        Self {
            message_id: MessageId::new(),
            parent,
            model: model.to_owned(),
            blocks: Vec::new(),
            pending_tools: std::collections::HashMap::new(),
            partial_args: std::collections::HashMap::new(),
            tool_calls: Vec::new(),
            thinking_since: None,
        }
    }

    /// Stamp the timed thinking block, the last block while one is
    /// open, with how long it ran.
    fn end_thinking(&mut self) {
        if let Some(since) = self.thinking_since.take()
            && let Some(Content::Thinking { duration_ms, .. }) = self.blocks.last_mut()
        {
            *duration_ms = Some(u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX));
        }
    }

    /// Append `delta` to the current text block, opening one if needed.
    fn push_text(&mut self, delta: &str) {
        self.end_thinking();
        if let Some(Content::Text { text }) = self.blocks.last_mut() {
            text.push_str(delta);
        } else {
            self.blocks.push(Content::Text {
                text: delta.to_owned(),
            });
        }
    }

    /// Append `delta` to the current thinking block, opening one if
    /// needed. A signed block is closed, so its successor opens anew.
    fn push_thinking(&mut self, delta: &str) {
        if let Some(Content::Thinking {
            text,
            signature: None,
            ..
        }) = self.blocks.last_mut()
        {
            text.push_str(delta);
        } else {
            self.end_thinking();
            self.blocks.push(Content::Thinking {
                text: delta.to_owned(),
                signature: None,
                duration_ms: None,
            });
        }
        self.thinking_since.get_or_insert_with(Instant::now);
    }

    /// Append a whole media part. An image or an audio clip becomes
    /// its block; a kind kage cannot hold yet becomes a short note in
    /// its place, so the reply still says something arrived.
    fn push_media(&mut self, kind: Modality, mime: &str, data: &str) {
        self.end_thinking();
        match kind {
            Modality::Image => self.blocks.push(Content::Image {
                source: MediaSource::Base64 {
                    data: data.to_owned(),
                },
                mime: mime.to_owned(),
            }),
            Modality::Audio => self.blocks.push(Content::Audio {
                source: MediaSource::Base64 {
                    data: data.to_owned(),
                },
                mime: mime.to_owned(),
            }),
            _ => self.push_text(&format!(
                "\n[{} output ({mime}) is not kept yet]\n",
                kind.as_str()
            )),
        }
    }

    fn signature(&self, data: String, redacted: bool) -> ThinkingSignature {
        ThinkingSignature {
            model: self.model.clone(),
            data,
            redacted,
        }
    }

    /// Sign the current thinking block with `data`, or record `data`
    /// as an empty signed block when no unsigned block is in flight.
    fn sign_thinking(&mut self, data: String) {
        self.end_thinking();
        let signed = Some(self.signature(data, false));
        if let Some(Content::Thinking { signature, .. }) = self.blocks.last_mut()
            && signature.is_none()
        {
            *signature = signed;
        } else {
            self.blocks.push(Content::Thinking {
                text: String::new(),
                signature: signed,
                duration_ms: None,
            });
        }
    }

    /// Record a thinking block the provider sent encrypted only.
    fn push_redacted(&mut self, data: String) {
        self.end_thinking();
        let signature = Some(self.signature(data, true));
        self.blocks.push(Content::Thinking {
            text: String::new(),
            signature,
            duration_ms: None,
        });
    }

    fn begin_tool(&mut self, id: ToolCallId, name: String) {
        self.end_thinking();
        self.pending_tools.insert(id, name);
    }

    fn complete_tool(
        &mut self,
        id: ToolCallId,
        input: serde_json::Value,
    ) -> Result<(ToolCallId, String, serde_json::Value), LoopError> {
        self.end_thinking();
        let name = self
            .pending_tools
            .remove(&id)
            .ok_or_else(|| LoopError::Provider {
                message: format!("tool_call_end without tool_call_start: {id}"),
            })?;
        self.blocks.push(Content::ToolCall {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        });
        Ok((id, name, input))
    }

    fn finish(&mut self, usage: TokenUsage) -> TurnResult {
        self.end_thinking();
        TurnResult {
            message: Message {
                role: Role::Assistant,
                content: std::mem::take(&mut self.blocks),
                id: self.message_id,
                parent: self.parent,
                ts: chrono::Utc::now(),
            },
            tool_calls: std::mem::take(&mut self.tool_calls),
            usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use kage_core::{CancelFlag, ToolCallId};
    use kage_provider::{Provider, StopReason, StreamRequest, testing::MockProvider};

    use super::*;

    fn run_collect(events: Vec<Result<ProviderEvent, kage_provider::ProviderError>>) -> TurnResult {
        let mock = MockProvider::replaying(events);
        let cancel = CancelFlag::new();
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        let mut emitted = Vec::new();
        collect_turn(None, "m", stream, &cancel, &mut |ev| {
            emitted.push(ev);
        })
        .unwrap()
    }

    fn run_collect_with_emits(
        events: Vec<Result<ProviderEvent, kage_provider::ProviderError>>,
    ) -> (TurnResult, Vec<LoopEvent>) {
        let mock = MockProvider::replaying(events);
        let cancel = CancelFlag::new();
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        let mut emitted = Vec::new();
        let res = collect_turn(None, "m", stream, &cancel, &mut |ev| {
            emitted.push(ev);
        })
        .unwrap();
        (res, emitted)
    }

    fn end_event() -> ProviderEvent {
        ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input: 5,
                output: 10,
                cache_read: 0,
                cache_write: 0,
            },
        }
    }

    #[test]
    fn text_only_turn_assembles_one_text_block() {
        let result = run_collect(vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "hello ".into(),
            }),
            Ok(ProviderEvent::TextDelta {
                delta: "world".into(),
            }),
            Ok(end_event()),
        ]);
        assert_eq!(result.message.role, Role::Assistant);
        assert_eq!(result.message.content.len(), 1);
        assert!(matches!(
            &result.message.content[0],
            Content::Text { text } if text == "hello world"
        ));
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.usage.input, 5);
    }

    #[test]
    fn a_media_part_lands_between_the_text_around_it() {
        let (result, emitted) = run_collect_with_emits(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "here: ".into(),
            }),
            Ok(ProviderEvent::Media {
                kind: Modality::Image,
                mime: "image/png".into(),
                data: "iVBORw0K".into(),
            }),
            Ok(ProviderEvent::Media {
                kind: Modality::Audio,
                mime: "audio/wav".into(),
                data: "UklGRg".into(),
            }),
            Ok(ProviderEvent::Media {
                kind: Modality::Video,
                mime: "video/mp4".into(),
                data: "AAAAIGZ0eXBpc29t".into(),
            }),
            Ok(ProviderEvent::TextDelta {
                delta: "done".into(),
            }),
            Ok(end_event()),
        ]);
        let content = &result.message.content;
        assert_eq!(content.len(), 4, "{content:?}");
        assert_eq!(
            content[1],
            Content::Image {
                source: MediaSource::Base64 {
                    data: "iVBORw0K".into()
                },
                mime: "image/png".into(),
            }
        );
        assert_eq!(
            content[2],
            Content::Audio {
                source: MediaSource::Base64 {
                    data: "UklGRg".into()
                },
                mime: "audio/wav".into(),
            }
        );
        assert!(
            matches!(&content[3], Content::Text { text }
                if text == "\n[video output (video/mp4) is not kept yet]\ndone"),
            "{content:?}"
        );
        assert!(emitted.iter().any(|e| matches!(
            e,
            LoopEvent::Media { kind: Modality::Image, mime, .. } if mime == "image/png"
        )));
    }

    #[test]
    fn emits_message_lifecycle_events() {
        let (_, emitted) = run_collect_with_emits(vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta { delta: "hi".into() }),
            Ok(end_event()),
        ]);
        assert!(matches!(emitted[0], LoopEvent::MessageStart { .. }));
        assert!(matches!(
            &emitted[1],
            LoopEvent::TextDelta { delta, .. } if delta == "hi"
        ));
        assert!(matches!(
            emitted.last().unwrap(),
            LoopEvent::MessageEnd { .. }
        ));
    }

    #[test]
    fn message_end_forwards_stop_reason() {
        let (_, emitted) = run_collect_with_emits(vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::MaxTokens,
            usage: TokenUsage::default(),
        })]);
        assert!(matches!(
            emitted.last().unwrap(),
            LoopEvent::MessageEnd {
                stop_reason: StopReason::MaxTokens,
                ..
            }
        ));
    }

    #[test]
    fn synthesizes_message_start_when_provider_skips_it() {
        let (_, emitted) = run_collect_with_emits(vec![
            Ok(ProviderEvent::TextDelta { delta: "hi".into() }),
            Ok(end_event()),
        ]);
        assert!(matches!(emitted[0], LoopEvent::MessageStart { .. }));
    }

    #[test]
    fn thinking_block_separates_from_text() {
        let result = run_collect(vec![
            Ok(ProviderEvent::ThinkingDelta {
                delta: "ponder".into(),
            }),
            Ok(ProviderEvent::TextDelta {
                delta: "answer".into(),
            }),
            Ok(end_event()),
        ]);
        assert_eq!(result.message.content.len(), 2);
        assert!(matches!(
            &result.message.content[0],
            Content::Thinking { text, signature: None, duration_ms: Some(_) } if text == "ponder"
        ));
        assert!(matches!(
            &result.message.content[1],
            Content::Text { text } if text == "answer"
        ));
    }

    #[test]
    fn signatures_close_thinking_blocks_and_carry_the_model() {
        let result = run_collect(vec![
            Ok(ProviderEvent::ThinkingDelta { delta: "a".into() }),
            Ok(ProviderEvent::ThinkingSignature { data: "s1".into() }),
            Ok(ProviderEvent::ThinkingDelta { delta: "b".into() }),
            Ok(ProviderEvent::RedactedThinking { data: "enc".into() }),
            Ok(ProviderEvent::TextDelta { delta: "t".into() }),
            Ok(ProviderEvent::ThinkingSignature { data: "s2".into() }),
            Ok(end_event()),
        ]);
        let mut content = result.message.content;
        let timed: Vec<bool> = content
            .iter_mut()
            .map(|c| match c {
                Content::Thinking { duration_ms, .. } => duration_ms.take().is_some(),
                _ => false,
            })
            .collect();
        assert_eq!(timed, [true, true, false, false, false]);
        let sig = |data: &str, redacted| {
            Some(ThinkingSignature {
                model: "m".into(),
                data: data.into(),
                redacted,
            })
        };
        assert_eq!(
            content,
            vec![
                Content::Thinking {
                    text: "a".into(),
                    signature: sig("s1", false),
                    duration_ms: None,
                },
                Content::Thinking {
                    text: "b".into(),
                    signature: None,
                    duration_ms: None,
                },
                Content::Thinking {
                    text: String::new(),
                    signature: sig("enc", true),
                    duration_ms: None,
                },
                Content::Text { text: "t".into() },
                Content::Thinking {
                    text: String::new(),
                    signature: sig("s2", false),
                    duration_ms: None,
                },
            ]
        );
    }

    #[test]
    fn each_thinking_block_records_how_long_it_ran() {
        let id = ToolCallId::new("c1");
        let events = vec![
            (0, ProviderEvent::ThinkingDelta { delta: "a".into() }),
            (0, ProviderEvent::ThinkingDelta { delta: "b".into() }),
            (
                60,
                ProviderEvent::ToolCallStart {
                    id: id.clone(),
                    name: "ls".into(),
                },
            ),
            (
                0,
                ProviderEvent::ToolCallEnd {
                    id,
                    input: serde_json::json!({}),
                },
            ),
            (0, ProviderEvent::ThinkingDelta { delta: "c".into() }),
            (30, end_event()),
        ];
        let stream: EventStream = Box::new(events.into_iter().map(|(delay, event)| {
            std::thread::sleep(std::time::Duration::from_millis(delay));
            Ok(event)
        }));
        let result = collect_turn(None, "m", stream, &CancelFlag::new(), &mut |_| {}).unwrap();
        let durations: Vec<Option<u64>> = result
            .message
            .content
            .iter()
            .map(|c| match c {
                Content::Thinking { duration_ms, .. } => *duration_ms,
                _ => None,
            })
            .collect();
        assert!(matches!(durations[..], [Some(a), None, Some(c)] if a >= 60 && c >= 30));
    }

    #[test]
    fn tool_call_assembles_into_block_and_pending_list() {
        let id = ToolCallId::new("call_42");
        let result = run_collect(vec![
            Ok(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name: "read".into(),
            }),
            Ok(ProviderEvent::ToolCallArgsDelta {
                id: id.clone(),
                partial: r#"{"path":"#.into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: id.clone(),
                input: serde_json::json!({"path": "src/lib.rs"}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ]);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "read");
        assert_eq!(result.tool_calls[0].input["path"], "src/lib.rs");
        assert_eq!(result.message.content.len(), 1);
        assert!(matches!(
            &result.message.content[0],
            Content::ToolCall { name, .. } if name == "read"
        ));
    }

    #[test]
    fn tool_call_emits_tool_call_start_with_full_input() {
        let id = ToolCallId::new("call_1");
        let (_, emitted) = run_collect_with_emits(vec![
            Ok(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name: "ls".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: id.clone(),
                input: serde_json::json!({"path": "."}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ]);
        let start = emitted
            .iter()
            .find(|e| matches!(e, LoopEvent::ToolCallStart { .. }))
            .unwrap();
        match start {
            LoopEvent::ToolCallStart {
                name,
                input_partial,
                ..
            } => {
                assert_eq!(name, "ls");
                assert_eq!(input_partial["path"], ".");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn tool_call_streams_partial_args_before_final_start() {
        let id = ToolCallId::new("call_1");
        let (_, emitted) = run_collect_with_emits(vec![
            Ok(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name: "ls".into(),
            }),
            Ok(ProviderEvent::ToolCallArgsDelta {
                id: id.clone(),
                partial: "{\"path\":".into(),
            }),
            Ok(ProviderEvent::ToolCallArgsDelta {
                id: id.clone(),
                partial: "\".\"}".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: id.clone(),
                input: serde_json::json!({"path": "."}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ]);

        let first = emitted
            .iter()
            .find(|e| matches!(e, LoopEvent::ToolCallArgsDelta { .. }))
            .expect("an args-delta should fire at provider tool-call start");
        match first {
            LoopEvent::ToolCallArgsDelta {
                name,
                input_partial,
                ..
            } => {
                assert_eq!(name, "ls");
                assert_eq!(*input_partial, serde_json::json!({}));
            }
            _ => unreachable!(),
        }

        let populated = emitted
            .iter()
            .find_map(|e| match e {
                LoopEvent::ToolCallArgsDelta { input_partial, .. }
                    if input_partial.get("path").is_some() =>
                {
                    Some(input_partial.clone())
                }
                _ => None,
            })
            .expect("a populated args-delta should fire once fragments parse");
        assert_eq!(populated["path"], ".");

        let starts: Vec<&LoopEvent> = emitted
            .iter()
            .filter(|e| matches!(e, LoopEvent::ToolCallStart { .. }))
            .collect();
        assert_eq!(
            starts.len(),
            1,
            "the authoritative tool-call start must fire exactly once"
        );
        match starts[0] {
            LoopEvent::ToolCallStart {
                name,
                input_partial,
                ..
            } => {
                assert_eq!(name, "ls");
                assert_eq!(input_partial["path"], ".");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn tool_call_end_without_start_errors() {
        let mock = MockProvider::replaying(vec![
            Ok(ProviderEvent::ToolCallEnd {
                id: ToolCallId::new("orphan"),
                input: serde_json::json!({}),
            }),
            Ok(end_event()),
        ]);
        let cancel = CancelFlag::new();
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        let res = collect_turn(None, "m", stream, &cancel, &mut |_| {});
        assert!(matches!(
            res,
            Err(TurnFailure::Fatal(LoopError::Provider { .. }))
        ));
    }

    #[test]
    fn cancellation_aborts_mid_stream() {
        let mock = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta { delta: "a".into() }),
            Ok(end_event()),
        ]);
        let cancel = CancelFlag::new();
        cancel.cancel();
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        let res = collect_turn(None, "m", stream, &cancel, &mut |_| {});
        assert!(matches!(res, Err(TurnFailure::Fatal(LoopError::Cancelled))));
    }

    #[test]
    fn provider_error_surfaces_with_its_original_kind() {
        let mock = MockProvider::replaying(vec![Err(kage_provider::ProviderError::Auth(
            "no key".into(),
        ))]);
        let cancel = CancelFlag::new();
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        let res = collect_turn(None, "m", stream, &cancel, &mut |_| {});
        // The original ProviderError is preserved (not stringified) so
        // the run loop can classify it; Auth is not transient.
        match res {
            Err(TurnFailure::Provider(e)) => assert!(!e.is_transient()),
            other => panic!("expected Provider failure, got {other:?}"),
        }
    }

    #[test]
    fn clean_eof_without_message_end_is_a_retryable_provider_failure() {
        let mock = MockProvider::replaying(vec![Ok(ProviderEvent::TextDelta {
            delta: "partial".into(),
        })]);
        let cancel = CancelFlag::new();
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        match collect_turn(None, "m", stream, &cancel, &mut |_| {}) {
            Err(TurnFailure::Provider(e)) => assert!(e.is_transient()),
            other => panic!("expected a transient Provider failure, got {other:?}"),
        }
    }

    #[test]
    fn parent_id_is_carried_through() {
        let parent = MessageId::new();
        let mock = MockProvider::replaying(vec![Ok(end_event())]);
        let cancel = CancelFlag::new();
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        let res = collect_turn(Some(parent), "m", stream, &cancel, &mut |_| {}).unwrap();
        assert_eq!(res.message.parent, Some(parent));
    }
}
