//! Anthropic provider.
//!
//! Implements the Messages API (`POST /v1/messages`) using `ureq`. The
//! `Provider::stream` impl reads server-sent events line by line and
//! yields [`ProviderEvent`]s as they arrive.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufReader, Read};

use kage_core::{CancelFlag, Content, Message, Reasoning, Role, TokenUsage, ToolCallId};
use serde_json::Value;

use crate::{
    EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, ProviderModel,
    StopReason, StreamRequest, ToolSpec,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
/// Smallest `budget_tokens` Anthropic accepts.
const MIN_THINKING_BUDGET: u32 = 1_024;
/// Beta that lets manual (`enabled`) thinking think between tool calls
/// on Claude 4 models up to 4.5. The API ignores it on other models.
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

/// Anthropic provider implementation.
#[derive(Debug)]
pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    metadata: ProviderMetadata,
    /// Extra headers sent on every request, after the protocol's own.
    extra_headers: BTreeMap<String, String>,
    /// Models advertised from `Provider::models` (custom providers);
    /// empty lets the catalog drive the picker.
    models: Vec<ProviderModel>,
    /// The endpoint counts cached tokens inside `input_tokens`.
    input_includes_cache: bool,
}

impl AnthropicProvider {
    /// Construct a provider from an API key, using the default base URL.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// Construct a provider against a custom base URL (for tests or proxies).
    #[must_use]
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            metadata: ProviderMetadata {
                id: "anthropic".into(),
                display_name: "Anthropic".into(),
                supports_caching: true,
                supports_thinking: true,
                supports_tool_use: true,
            },
            extra_headers: BTreeMap::new(),
            models: Vec::new(),
            input_includes_cache: false,
        }
    }

    /// Register under `metadata` instead of the built-in identity, for a
    /// user-declared provider that speaks this protocol.
    #[must_use]
    pub fn with_metadata(mut self, metadata: ProviderMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Send `headers` on every request, after the protocol's own headers.
    #[must_use]
    pub fn with_extra_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Advertise `models` from [`Provider::models`] instead of relying
    /// on the catalog.
    #[must_use]
    pub fn with_models(mut self, models: Vec<ProviderModel>) -> Self {
        self.models = models;
        self
    }

    /// Whether the endpoint counts cached tokens inside `input_tokens`,
    /// as some compatible gateways do. When set, they are taken back out
    /// so the context fill and the cost count each token once.
    #[must_use]
    pub fn with_input_including_cache(mut self, includes: bool) -> Self {
        self.input_includes_cache = includes;
        self
    }

    /// Headers every request carries: content type, the pinned API
    /// version, the `x-api-key` credential (skipped when no key is
    /// configured, e.g. local endpoints), then the configured extras in
    /// key order.
    fn request_headers(&self) -> Vec<(String, String)> {
        let credential = (!self.api_key.is_empty()).then(|| ("x-api-key", self.api_key.clone()));
        crate::headers::request_headers(
            &[
                ("content-type", "application/json".to_owned()),
                ("anthropic-version", ANTHROPIC_VERSION.to_owned()),
            ],
            credential,
            &self.extra_headers,
        )
    }
}

/// Build the JSON body for a Messages API request.
///
/// Adds Anthropic prompt-cache breakpoints (`cache_control: ephemeral`) to
/// the system prompt and to the last block of the final message. This caches
/// the system prompt + tool definitions + entire prior conversation, so the
/// next turn pays read-rate (~10% of input rate) for everything before the
/// new user message.
pub(crate) fn build_request_body(req: &StreamRequest, stream: bool) -> Value {
    let mut messages: Vec<Value> = req
        .messages
        .iter()
        .filter_map(|m| internal_message_to_anthropic(m, &req.model))
        .collect();
    if let Some(last) = messages.last_mut() {
        mark_last_block_for_caching(last);
    }

    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "max_tokens": req.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        "stream": stream,
    });

    if let Some(system) = &req.system {
        body["system"] = serde_json::json!([{
            "type": "text",
            "text": system,
            "cache_control": {"type": "ephemeral"},
        }]);
    }
    if !req.tools.is_empty() {
        body["tools"] = serde_json::to_value(
            req.tools
                .iter()
                .map(tool_spec_to_anthropic)
                .collect::<Vec<_>>(),
        )
        .expect("tool spec serializes");
    }
    if let Some(temp) = req.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    apply_thinking(&mut body, req);
    body
}

/// Set the thinking fields for `req`. An explicit
/// [`crate::ThinkingConfig`] budget wins. Otherwise the level goes out
/// as adaptive thinking with an `output_config.effort` on effort
/// models, and as a budget elsewhere. Budget thinking stays off a
/// request that continues an unsigned assistant turn, see
/// [`continues_unsigned_turn`].
fn apply_thinking(body: &mut Value, req: &StreamRequest) {
    let open = !continues_unsigned_turn(req);
    if let Some(thinking) = &req.thinking {
        if open {
            enable_budget(body, req, thinking.budget_tokens);
        }
        return;
    }
    let Some(level) = req.level else {
        return;
    };
    match req.reasoning {
        Reasoning::None | Reasoning::Fixed => {}
        Reasoning::Effort { .. } => {
            if let Some(effort) = req.reasoning.effort(level) {
                body["thinking"] = serde_json::json!({"type": "adaptive"});
                body["output_config"] = serde_json::json!({"effort": effort.as_str()});
            } else if level.is_off() && req.reasoning.has_toggle() {
                body["thinking"] = serde_json::json!({"type": "disabled"});
            }
        }
        Reasoning::Unknown | Reasoning::Toggle | Reasoning::Budget { .. } => {
            if let Some(budget) = req.reasoning.budget(level)
                && open
            {
                enable_budget(body, req, budget);
            }
        }
    }
}

/// Turn on budget thinking. Anthropic rejects a `budget_tokens` that is
/// not below `max_tokens`. With no requested ceiling the default
/// `max_tokens` is raised to leave reply room past the budget. A
/// requested ceiling is kept, since it may be the model's limit or
/// fitted to the window, and an oversized budget halves the ceiling
/// instead. Thinking stays off when that half is under
/// [`MIN_THINKING_BUDGET`].
fn enable_budget(body: &mut Value, req: &StreamRequest, budget: u32) {
    let budget = match req.max_output_tokens {
        None => {
            body["max_tokens"] =
                serde_json::json!(budget.saturating_add(DEFAULT_MAX_OUTPUT_TOKENS));
            budget
        }
        Some(ceiling) if budget >= ceiling => {
            if ceiling / 2 < MIN_THINKING_BUDGET {
                return;
            }
            ceiling / 2
        }
        Some(_) => budget,
    };
    body["thinking"] = enabled(budget);
}

fn enabled(budget_tokens: u32) -> Value {
    serde_json::json!({"type": "enabled", "budget_tokens": budget_tokens})
}

/// Whether this request continues an assistant turn that does not
/// lead with a thinking block this model signed: a tool result follows
/// the last assistant message, so that turn is still open. With budget
/// thinking the API requires the open turn to lead with its signed
/// thinking block, so such requests drop thinking instead of sending a
/// body the API rejects. Adaptive thinking has no such rule.
fn continues_unsigned_turn(req: &StreamRequest) -> bool {
    let Some(idx) = req.messages.iter().rposition(|m| m.role == Role::Assistant) else {
        return false;
    };
    let open = req.messages[idx + 1..]
        .iter()
        .any(|m| m.role == Role::ToolResult);
    let signed = matches!(
        req.messages[idx].content.first(),
        Some(Content::Thinking { signature: Some(sig), .. }) if sig.model == req.model
    );
    open && !signed
}

/// Add `beta` to the `anthropic-beta` header, joining a value the
/// configured extras already set.
fn add_beta(headers: &mut Vec<(String, String)>, beta: &str) {
    match headers
        .iter_mut()
        .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
    {
        Some((_, value)) => {
            value.push(',');
            value.push_str(beta);
        }
        None => headers.push(("anthropic-beta".to_owned(), beta.to_owned())),
    }
}

fn mark_last_block_for_caching(message: &mut Value) {
    let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    let Some(last) = content.last_mut() else {
        return;
    };
    let Some(obj) = last.as_object_mut() else {
        return;
    };
    obj.insert(
        "cache_control".into(),
        serde_json::json!({"type": "ephemeral"}),
    );
}

fn tool_spec_to_anthropic(spec: &ToolSpec) -> Value {
    serde_json::json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": spec.schema,
    })
}

/// Convert an internal [`Message`] into the Anthropic wire shape.
///
/// Returns `None` for messages that should not be sent (system messages live
/// in the top-level `system` field; custom plugin content has no wire form).
fn internal_message_to_anthropic(msg: &Message, model: &str) -> Option<Value> {
    let (role, blocks) = match msg.role {
        Role::User => ("user", convert_user_blocks(&msg.content)),
        Role::Assistant => ("assistant", convert_assistant_blocks(&msg.content, model)),
        Role::ToolResult => ("user", convert_tool_result_blocks(&msg.content)),
        Role::System => return None,
    };
    if blocks.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "role": role,
        "content": blocks,
    }))
}

fn convert_user_blocks(blocks: &[Content]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"type":"text","text":text})),
            Content::Image { source, mime } => Some(image_to_anthropic(source, mime)),
            Content::Audio { mime, .. } => Some(serde_json::json!({
                "type": "text",
                "text": Content::audio_note(mime),
            })),
            _ => None,
        })
        .collect()
}

/// Assistant blocks on the wire. Thinking `model` signed goes back
/// unchanged, as the API asks. Unsigned thinking goes back as a native
/// block for Anthropic-compatible models that require it on a tool-call
/// turn, and is dropped for Claude models, which reject unsigned blocks.
fn convert_assistant_blocks(blocks: &[Content], model: &str) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"type":"text","text":text})),
            Content::Image { mime, .. } | Content::Audio { mime, .. } => Some(serde_json::json!({
                "type": "text",
                "text": Content::produced_media_note(mime),
            })),
            Content::Thinking {
                signature: Some(sig),
                ..
            } if sig.model == model && sig.redacted => Some(serde_json::json!({
                "type": "redacted_thinking",
                "data": sig.data,
            })),
            Content::Thinking {
                text,
                signature: Some(sig),
                ..
            } if sig.model == model => Some(serde_json::json!({
                "type": "thinking",
                "thinking": text,
                "signature": sig.data,
            })),
            Content::Thinking { text, .. } => (!is_claude_model(model))
                .then(|| serde_json::json!({"type": "thinking", "thinking": text})),
            Content::ToolCall { id, name, input } => Some(serde_json::json!({
                "type":"tool_use",
                "id": id.0,
                "name": name,
                "input": input,
            })),
            _ => None,
        })
        .collect()
}

/// Whether `model` names a Claude model. Claude rejects unsigned
/// thinking blocks. Anthropic-compatible gateways serving other models
/// stream thinking without a signature and reject a tool-call turn whose
/// thinking is gone, so those blocks must be preserved.
fn is_claude_model(model: &str) -> bool {
    const FAMILIES: [&str; 5] = ["opus", "sonnet", "haiku", "fable", "mythos"];
    let model = model.to_ascii_lowercase();
    model.contains("claude")
        || model
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|token| FAMILIES.contains(&token))
}

/// Tool results on the wire. Images a tool produced go inside its
/// `tool_result`, which Anthropic accepts, so the model sees them;
/// clips stand as their note text.
fn convert_tool_result_blocks(blocks: &[Content]) -> Vec<Value> {
    kage_core::tool_results(blocks)
        .iter()
        .map(|result| {
            let content = if result.media.is_empty() && result.audio.is_empty() {
                Value::String(result.output.to_owned())
            } else {
                let mut parts = Vec::new();
                if !result.output.is_empty() {
                    parts.push(serde_json::json!({"type": "text", "text": result.output}));
                }
                for (source, mime) in &result.media {
                    parts.push(image_to_anthropic(source, mime));
                }
                for mime in &result.audio {
                    parts.push(serde_json::json!({
                        "type": "text",
                        "text": Content::audio_note(mime),
                    }));
                }
                Value::Array(parts)
            };
            serde_json::json!({
                "type": "tool_result",
                "tool_use_id": result.call_id.0,
                "content": content,
                "is_error": result.is_error,
            })
        })
        .collect()
}

fn image_to_anthropic(source: &kage_core::MediaSource, mime: &str) -> Value {
    match source {
        kage_core::MediaSource::Url { url } => serde_json::json!({
            "type":"image",
            "source": {"type":"url", "url": url},
        }),
        kage_core::MediaSource::Base64 { data } => serde_json::json!({
            "type":"image",
            "source": {"type":"base64", "media_type": mime, "data": data},
        }),
        kage_core::MediaSource::Blob { .. } => serde_json::json!({
            "type": "text",
            "text": source.missing_note(mime),
        }),
    }
}

impl Provider for AnthropicProvider {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn models(&self) -> Vec<ProviderModel> {
        self.models.clone()
    }

    /// Thinking goes back as native blocks: signed as the API asks, and
    /// unsigned for non-Claude models that need it. See
    /// `build_request_body`.
    fn preserves_thinking(&self) -> bool {
        true
    }

    fn stream(
        &self,
        req: StreamRequest,
        cancel: &CancelFlag,
    ) -> Result<EventStream, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let body = build_request_body(&req, true);
        let url = format!("{}/v1/messages", self.base_url);
        let mut headers = self.request_headers();
        headers.extend(crate::headers::opencode_headers(
            &self.base_url,
            req.session.as_deref(),
            &self.extra_headers,
        ));
        if body["thinking"]["type"] == "enabled" {
            add_beta(&mut headers, INTERLEAVED_THINKING_BETA);
        }
        let (response, kill) = crate::http::send(cancel, url, move |agent, url| {
            let mut request = agent.post(url);
            for (name, value) in &headers {
                request = request.header(name.as_str(), value.as_str());
            }
            request.send_json(&body)
        })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(crate::http::read_error_body(status, response));
        }

        let reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
        let mut inner: EventStream = Box::new(AnthropicStream::new(reader, cancel.clone()));
        inner = uncached_input(inner, self.input_includes_cache);
        Ok(crate::cancelable::make_cancelable(
            inner,
            cancel.clone(),
            kill,
        ))
    }
}

/// `events` with the cached tokens taken out of the final usage's
/// input: always when `forced`, otherwise only when the counts show
/// the endpoint put them there.
fn uncached_input(events: EventStream, forced: bool) -> EventStream {
    Box::new(events.map(move |event| match event {
        Ok(ProviderEvent::MessageEnd { stop_reason, usage }) => Ok(ProviderEvent::MessageEnd {
            stop_reason,
            usage: if forced {
                crate::event::without_cached_input(usage)
            } else {
                crate::event::without_cached_input_if_included(usage)
            },
        }),
        other => other,
    }))
}

/// Iterator over a streaming Anthropic Messages response.
///
/// Drives an SSE reader and translates Anthropic's event vocabulary
/// (`message_start`, `content_block_*`, `message_delta`, `message_stop`)
/// into our generic [`ProviderEvent`] alphabet. Polls the [`CancelFlag`]
/// between each SSE event.
pub struct AnthropicStream {
    reader: BufReader<Box<dyn Read + Send>>,
    cancel: CancelFlag,
    state: StreamState,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
    done: bool,
}

/// Synthetic block slots count up from here for frames that omit the
/// `index` field, far above any index a real stream carries.
const SYNTHETIC_INDEX_BASE: usize = 1 << 62;

#[derive(Default)]
struct StreamState {
    blocks: HashMap<usize, BlockBuilder>,
    /// Synthetic slots handed out so far to frames without an index.
    synthetic: usize,
    /// The synthetic slot of the index-less block in flight, so its
    /// delta and stop frames reuse the slot their start allocated.
    open_synthetic: Option<usize>,
    usage: TokenUsage,
    stop_reason: StopReason,
    /// Set once `message_start` arrived, so an EOF before it is a dead
    /// stream (nothing synthesized) and an EOF after it is a stream
    /// whose `message_stop` never made it (turn completed at EOF).
    started: bool,
}

impl StreamState {
    /// The frame's `index`, or a fresh synthetic slot when the frame
    /// omits it, so index-less blocks cannot share a slot with an
    /// indexed one. Index-less delta and stop frames continue the
    /// index-less block in flight.
    fn block_index(&mut self, value: &Value) -> usize {
        if let Some(v) = value.get("index").and_then(Value::as_u64) {
            return usize::try_from(v).unwrap_or(0);
        }
        if let Some(slot) = self.open_synthetic
            && self.blocks.contains_key(&slot)
        {
            return slot;
        }
        let slot = SYNTHETIC_INDEX_BASE.wrapping_add(self.synthetic);
        self.synthetic += 1;
        self.open_synthetic = Some(slot);
        slot
    }
}

enum BlockBuilder {
    Text,
    Thinking,
    ToolUse {
        id: ToolCallId,
        partial_input: String,
    },
}

impl AnthropicStream {
    /// Construct a stream from any byte source carrying Anthropic SSE.
    #[must_use]
    pub fn new(reader: Box<dyn Read + Send>, cancel: CancelFlag) -> Self {
        Self {
            reader: BufReader::new(reader),
            cancel,
            state: StreamState::default(),
            pending: VecDeque::new(),
            done: false,
        }
    }

    fn process_event(&mut self, name: &str, data: &str) {
        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                self.pending
                    .push_back(Err(ProviderError::Decode(e.to_string())));
                return;
            }
        };
        match name {
            "message_start" => {
                self.state.started = true;
                self.pending.push_back(Ok(ProviderEvent::MessageStart));
                if let Some(usage) = value.pointer("/message/usage") {
                    self.absorb_usage(usage);
                }
            }
            "content_block_start" => self.on_block_start(&value),
            "content_block_delta" => self.on_block_delta(&value),
            "content_block_stop" => self.on_block_stop(&value),
            "message_delta" => {
                if let Some(stop) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.state.stop_reason = parse_stop_reason(stop);
                }
                if let Some(usage) = value.pointer("/usage") {
                    self.absorb_usage(usage);
                }
            }
            "message_stop" => {
                self.pending.push_back(Ok(ProviderEvent::MessageEnd {
                    stop_reason: self.state.stop_reason,
                    usage: self.state.usage,
                }));
                self.done = true;
            }
            "error" => {
                let kind = value
                    .pointer("/error/type")
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                let msg = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider error");
                self.pending
                    .push_back(Err(ProviderError::from_stream_error(kind, msg)));
                self.done = true;
            }
            _ => {}
        }
    }

    /// Merge the usage of `message_start` or `message_delta`. The
    /// counts are cumulative, so each field keeps its largest value:
    /// some compatible endpoints send zeros in `message_delta` for
    /// counts only `message_start` carried.
    fn absorb_usage(&mut self, usage: &Value) {
        let usage_of = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        let total = &mut self.state.usage;
        total.input = total.input.max(usage_of("input_tokens"));
        total.output = total.output.max(usage_of("output_tokens"));
        total.cache_write = total
            .cache_write
            .max(usage_of("cache_creation_input_tokens"));
        total.cache_read = total.cache_read.max(usage_of("cache_read_input_tokens"));
    }

    fn on_block_start(&mut self, value: &Value) {
        let index = self.state.block_index(value);
        let Some(block) = value.get("content_block") else {
            return;
        };
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "text" => {
                self.state.blocks.insert(index, BlockBuilder::Text);
            }
            "thinking" => {
                self.state.blocks.insert(index, BlockBuilder::Thinking);
            }
            "redacted_thinking" => {
                let data = block
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                self.pending
                    .push_back(Ok(ProviderEvent::RedactedThinking { data }));
            }
            "tool_use" => {
                let id_str = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let id = ToolCallId::new(id_str);
                self.state.blocks.insert(
                    index,
                    BlockBuilder::ToolUse {
                        id: id.clone(),
                        partial_input: String::new(),
                    },
                );
                self.pending
                    .push_back(Ok(ProviderEvent::ToolCallStart { id, name }));
            }
            _ => {}
        }
    }

    fn on_block_delta(&mut self, value: &Value) {
        let index = self.state.block_index(value);
        let Some(delta) = value.get("delta") else {
            return;
        };
        let dtype = delta.get("type").and_then(Value::as_str).unwrap_or("");
        match dtype {
            "text_delta" => {
                let text = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                self.pending
                    .push_back(Ok(ProviderEvent::TextDelta { delta: text }));
            }
            "thinking_delta" => {
                let text = delta
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                self.pending
                    .push_back(Ok(ProviderEvent::ThinkingDelta { delta: text }));
            }
            "signature_delta" => {
                let data = delta
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                self.pending
                    .push_back(Ok(ProviderEvent::ThinkingSignature { data }));
            }
            "input_json_delta" => {
                let partial = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if let Some(BlockBuilder::ToolUse { id, partial_input }) =
                    self.state.blocks.get_mut(&index)
                {
                    partial_input.push_str(&partial);
                    self.pending.push_back(Ok(ProviderEvent::ToolCallArgsDelta {
                        id: id.clone(),
                        partial,
                    }));
                }
            }
            _ => {}
        }
    }

    fn on_block_stop(&mut self, value: &Value) {
        let index = self.state.block_index(value);
        if let Some(BlockBuilder::ToolUse { id, partial_input }) = self.state.blocks.remove(&index)
        {
            let input = if partial_input.is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                match serde_json::from_str::<Value>(&partial_input) {
                    Ok(v) => v,
                    Err(e) => {
                        self.pending.push_back(Err(ProviderError::Decode(format!(
                            "tool call {} input did not parse as JSON: {} (raw: {})",
                            id.0, e, partial_input
                        ))));
                        return;
                    }
                }
            };
            self.pending
                .push_back(Ok(ProviderEvent::ToolCallEnd { id, input }));
        }
    }
}

impl crate::sse::SseStreamCore for AnthropicStream {
    fn reader(&mut self) -> &mut BufReader<Box<dyn Read + Send>> {
        &mut self.reader
    }
    fn cancel(&self) -> &CancelFlag {
        &self.cancel
    }
    fn pending(&mut self) -> &mut VecDeque<Result<ProviderEvent, ProviderError>> {
        &mut self.pending
    }
    fn is_done(&self) -> bool {
        self.done
    }
    fn set_done(&mut self) {
        self.done = true;
    }
    fn process(&mut self, name: &str, data: &str) {
        self.process_event(name, data);
    }
    fn on_eof(&mut self) {
        // The protocol ends with `message_stop`; a compatible upstream
        // that drops the connection first still gets its turn completed
        // with whatever arrived, instead of a full re-request.
        if self.state.started {
            self.pending.push_back(Ok(ProviderEvent::MessageEnd {
                stop_reason: self.state.stop_reason,
                usage: self.state.usage,
            }));
        }
    }
}

impl Iterator for AnthropicStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        crate::sse::sse_next(self)
    }
}

fn parse_stop_reason(value: &str) -> StopReason {
    match value {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        _ => StopReason::Other,
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod thinking_tests {
    use std::sync::Arc;

    use kage_core::{Effort, Efforts, ThinkingLevel};

    use super::*;

    fn request(reasoning: Reasoning, level: ThinkingLevel) -> StreamRequest {
        let user = Message::new(Role::User, vec![Content::Text { text: "hi".into() }], None);
        let mut req = StreamRequest::new("m", vec![Arc::new(user)]);
        req.reasoning = reasoning;
        req.level = Some(level);
        req
    }

    fn effort(values: &[Effort], toggle: bool) -> Reasoning {
        Reasoning::Effort {
            efforts: Efforts::of(values),
            toggle,
        }
    }

    #[test]
    fn effort_models_get_adaptive_thinking_and_their_effort() {
        let r = effort(&[Effort::Low, Effort::High, Effort::Max], false);
        let body = build_request_body(&request(r, ThinkingLevel::Max), false);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "max");
    }

    #[test]
    fn off_disables_thinking_on_toggle_effort_models() {
        let r = effort(&[Effort::Low, Effort::High], true);
        let body = build_request_body(&request(r, ThinkingLevel::Off), false);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn budget_models_get_a_bounded_budget() {
        let r = Reasoning::Budget {
            min: 20_000,
            max: None,
            toggle: true,
        };
        let body = build_request_body(&request(r, ThinkingLevel::High), false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 20_000);
        assert_eq!(body["max_tokens"], 24_096);
    }

    #[test]
    fn a_budget_past_the_requested_ceiling_halves_it() {
        let mut req = request(Reasoning::Toggle, ThinkingLevel::High);
        req.max_output_tokens = Some(8_192);
        let body = build_request_body(&req, false);
        assert_eq!(body["max_tokens"], 8_192);
        assert_eq!(body["thinking"]["budget_tokens"], 4_096);
    }

    #[test]
    fn a_budget_under_the_requested_ceiling_is_kept() {
        let mut req = request(Reasoning::Toggle, ThinkingLevel::High);
        req.max_output_tokens = Some(64_000);
        let body = build_request_body(&req, false);
        assert_eq!(body["max_tokens"], 64_000);
        assert_eq!(body["thinking"]["budget_tokens"], 16_384);
    }

    #[test]
    fn a_ceiling_too_small_for_thinking_leaves_it_off() {
        let mut req = request(Reasoning::Toggle, ThinkingLevel::High);
        req.max_output_tokens = Some(1_500);
        let body = build_request_body(&req, false);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn adaptive_thinking_stays_on_for_an_unsigned_continuation() {
        let r = effort(&[Effort::Low, Effort::High], false);
        let mut req = request(r, ThinkingLevel::High);
        req.messages.push(Arc::new(Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId::new("call_1"),
                name: "shell".into(),
                input: serde_json::json!({}),
            }],
            None,
        )));
        req.messages.push(Arc::new(Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new("call_1"),
                output: "ok".into(),
                is_error: false,
            }],
            None,
        )));
        let body = build_request_body(&req, true);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn models_without_a_setting_send_no_thinking() {
        let body = build_request_body(&request(Reasoning::Fixed, ThinkingLevel::High), false);
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
    }
}
