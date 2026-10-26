//! `OpenAI` Responses API provider.
//!
//! Distinct from Chat Completions: the Responses API
//! (`POST /v1/responses`) takes an `input` array of typed items
//! (`message`, `function_call`, `function_call_output`, ...) instead
//! of a `messages` array, surfaces native reasoning items, and emits
//! a richer SSE event vocabulary. The Codex (`gpt-5-codex`,
//! `gpt-5.1-codex-*`) models are first-class here.
//!
//! Provider id: `openai`, kage's built-in `OpenAI` provider. Chat
//! Completions stays in [`crate::openai`] for the providers that speak
//! that protocol.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io::{BufReader, Read};

use kage_core::{CancelFlag, Content, Message, Reasoning, Role, ToolCallId};
use serde_json::Value;

use crate::{
    EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, ProviderModel,
    StopReason, StreamRequest, ToolSpec,
};

use crate::bearer::SharedBearer;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;

/// The zstd level of compressed bodies, the one the Codex CLI uses.
const ZSTD_LEVEL: i32 = 3;

/// `OpenAI` Responses API provider.
pub struct OpenAiResponsesProvider {
    api_key: String,
    base_url: String,
    metadata: ProviderMetadata,
    /// Extra headers sent on every request, after the protocol's own.
    extra_headers: BTreeMap<String, String>,
    /// Models advertised from `Provider::models` (custom providers);
    /// empty lets the catalog drive the picker.
    models: Vec<ProviderModel>,
    /// Whether every model is offered `OpenAI`'s hosted image tool.
    image_generation: bool,
    /// Renewable credential (an OAuth login). When set, `api_key` is
    /// ignored and a 401 refreshes once and retries.
    bearer: Option<SharedBearer>,
    /// Whether requests follow the `ChatGPT` backend's rules: no
    /// `max_output_tokens` or `temperature`, and the session as the
    /// prompt cache key.
    chatgpt: bool,
    /// Whether request bodies go out zstd-compressed.
    zstd: bool,
}

impl fmt::Debug for OpenAiResponsesProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiResponsesProvider")
            .field("api_key", &(!self.api_key.is_empty()).then_some("<set>"))
            .field("base_url", &self.base_url)
            .field("metadata", &self.metadata)
            .field("extra_headers", &self.extra_headers)
            .field("models", &self.models.len())
            .field("image_generation", &self.image_generation)
            .field("bearer", &self.bearer.as_ref().map(|_| "<set>"))
            .field("chatgpt", &self.chatgpt)
            .field("zstd", &self.zstd)
            .finish()
    }
}

impl OpenAiResponsesProvider {
    /// Construct a provider from an API key, using the default base URL.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// Construct a provider against a custom base URL.
    #[must_use]
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            metadata: ProviderMetadata {
                id: "openai".into(),
                display_name: "OpenAI".into(),
                supports_caching: false,
                supports_thinking: true,
                supports_tool_use: true,
            },
            extra_headers: BTreeMap::new(),
            models: Vec::new(),
            image_generation: false,
            bearer: None,
            chatgpt: false,
            zstd: false,
        }
    }

    /// The streaming body for `req`, with the hosted image tool offered
    /// when the provider switch is on, in the `ChatGPT` backend's shape
    /// when that is the endpoint.
    fn request_body(&self, req: &StreamRequest) -> Value {
        let mut body = if self.image_generation {
            let mut req = req.clone();
            req.produces = req
                .produces
                .iter()
                .chain([kage_core::Modality::Text, kage_core::Modality::Image])
                .collect();
            build_request_body(&req, true)
        } else {
            build_request_body(req, true)
        };
        if self.chatgpt
            && let Some(fields) = body.as_object_mut()
        {
            fields.remove("max_output_tokens");
            fields.remove("temperature");
            if let Some(Value::Object(reasoning)) = fields.get_mut("reasoning") {
                reasoning.insert("summary".to_owned(), Value::String("auto".to_owned()));
            }
            if let Some(session) = &req.session {
                fields.insert(
                    "prompt_cache_key".to_owned(),
                    Value::String(session.clone()),
                );
            }
        }
        body
    }

    /// Offer `OpenAI`'s hosted image tool to every model, as if each said
    /// it produces images. It runs at `OpenAI` and is billed on its own,
    /// so it is opt-in.
    #[must_use]
    pub fn with_image_generation(mut self, on: bool) -> Self {
        self.image_generation = on;
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

    /// Draw the credential from `source` instead of the fixed API key:
    /// the token of the moment rides every request, and a 401 refreshes
    /// once and retries. For OAuth-backed logins the host renews.
    #[must_use]
    pub fn with_bearer(mut self, source: SharedBearer) -> Self {
        self.bearer = Some(source);
        self
    }

    /// Shape requests for the `ChatGPT` backend: it answers
    /// `max_output_tokens` with a 400 and serves only reasoning models,
    /// which refuse `temperature`, and it routes its prompt cache on the
    /// session, sent as `prompt_cache_key` and a `session-id` header.
    /// Reasoning asks for summaries, as the Codex CLI does, so thinking
    /// shows; the public API refuses them for unverified organizations,
    /// so they stay off there.
    #[must_use]
    pub fn for_chatgpt_backend(mut self) -> Self {
        self.chatgpt = true;
        self
    }

    /// Send request bodies zstd-compressed. The `ChatGPT` backend
    /// decodes them and refuses plain bodies past about 1 MiB; a proxy
    /// in front of it may not, so this stays a separate switch.
    #[must_use]
    pub fn with_zstd_bodies(mut self) -> Self {
        self.zstd = true;
        self
    }

    /// Headers every request carries: content type, the bearer
    /// credential (`token` from a [`crate::bearer::BearerSource`], else
    /// the API key, skipped when neither is set), then the configured
    /// extras in key order.
    fn request_headers(&self, token: Option<&str>) -> Vec<(String, String)> {
        let credential = token
            .map(str::to_owned)
            .or_else(|| (!self.api_key.is_empty()).then(|| self.api_key.clone()))
            .map(|key| ("authorization", format!("Bearer {key}")));
        crate::headers::request_headers(
            &[("content-type", "application/json".to_owned())],
            credential,
            &self.extra_headers,
        )
    }
}

impl Provider for OpenAiResponsesProvider {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    /// Reasoning goes back as `reasoning` items: the summary text, plus
    /// the encrypted reasoning when this model produced it. Required for
    /// reasoning models, whose `function_call` items must be preceded by
    /// their `reasoning` item on replay.
    fn preserves_thinking(&self) -> bool {
        true
    }

    fn models(&self) -> Vec<ProviderModel> {
        self.models.clone()
    }

    fn stream(
        &self,
        req: StreamRequest,
        cancel: &CancelFlag,
    ) -> Result<EventStream, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let body = self.request_body(&req);
        let url = format!("{}/responses", self.base_url);
        let session = req.session.as_deref();
        let Some(source) = &self.bearer else {
            return self.dispatch(&body, &url, None, session, cancel);
        };
        // A rejected renewable credential gets exactly one
        // refresh-and-retry; a still-rejected token then fails as the
        // auth error it is.
        let sent = source.token();
        match self.dispatch(&body, &url, Some(&sent), session, cancel) {
            Err(ProviderError::Auth(_)) => {
                let fresh = source.refresh(&sent)?;
                self.dispatch(&body, &url, Some(&fresh), session, cancel)
            }
            other => other,
        }
    }
}

impl OpenAiResponsesProvider {
    /// Send one request with bearer `token` (else the API key) and turn
    /// a non-2xx status into its error.
    fn dispatch(
        &self,
        body: &Value,
        url: &str,
        token: Option<&str>,
        session: Option<&str>,
        cancel: &CancelFlag,
    ) -> Result<EventStream, ProviderError> {
        let mut headers = self.request_headers(token);
        headers.extend(crate::headers::opencode_headers(
            &self.base_url,
            session,
            &self.extra_headers,
        ));
        if self.chatgpt
            && let Some(session) = session
            && !self
                .extra_headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("session-id"))
        {
            headers.push(("session-id".to_owned(), session.to_owned()));
        }
        let json = serde_json::to_vec(body).map_err(|e| ProviderError::Decode(e.to_string()))?;
        let payload = if self.zstd {
            let compressed = zstd::stream::encode_all(json.as_slice(), ZSTD_LEVEL)
                .map_err(|e| ProviderError::Transport(format!("compress the request: {e}")))?;
            headers.push(("content-encoding".to_owned(), "zstd".to_owned()));
            compressed
        } else {
            json
        };
        let url = url.to_owned();
        let (response, kill) = crate::http::send(cancel, url, move |agent, url| {
            let mut request = agent.post(url);
            for (name, value) in &headers {
                request = request.header(name.as_str(), value.as_str());
            }
            request.send(payload.as_slice())
        })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(crate::http::read_error_body(status, response));
        }

        let reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
        let inner: EventStream = Box::new(ResponsesStream::new(reader, cancel.clone()));
        Ok(crate::cancelable::make_cancelable(
            inner,
            cancel.clone(),
            kill,
        ))
    }
}

/// Build the JSON body for a Responses API request.
pub(crate) fn build_request_body(req: &StreamRequest, stream: bool) -> Value {
    let input: Vec<Value> = req
        .messages
        .iter()
        .flat_map(|m| internal_message_to_responses(m, &req.model))
        .collect();

    let mut body = serde_json::json!({
        "model": req.model,
        "input": input,
        "max_output_tokens": req.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        "stream": stream,
        // The API default is `store: true`, which retains the whole
        // request (system prompt, history, tool definitions) at OpenAI
        // for 30 days. kage replays history itself and encrypted
        // reasoning on reasoning models, so nothing needs storing.
        "store": false,
    });
    if let Some(system) = &req.system {
        body["instructions"] = Value::String(system.clone());
    }
    if let Some(temp) = req.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    if let Some(effort) = reasoning_effort(req) {
        body["reasoning"] = serde_json::json!({ "effort": effort });
        body["include"] = serde_json::json!(["reasoning.encrypted_content"]);
    }
    let mut tools: Vec<Value> = req.tools.iter().map(tool_spec_to_responses).collect();
    // The Responses API has no image output switch: a model draws by
    // calling OpenAI's hosted image tool, which runs at OpenAI and
    // answers with the picture. kage only offers it to a model said to
    // produce images, since the hosted tool is billed on its own.
    if req.produces.contains(kage_core::Modality::Image) {
        tools.push(serde_json::json!({ "type": "image_generation" }));
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    body
}

/// Convert one [`ToolSpec`] into the Responses API tool shape.
///
/// Unlike Chat Completions, the Responses API uses a flat `function`
/// type (no nested `function` wrapper): `{ type, name, description,
/// parameters }`.
/// The `reasoning.effort` for `req`: the model's own effort value on
/// effort models (`none` for off where the model has it), nothing on
/// models without a setting, and the generic mapping otherwise.
fn reasoning_effort(req: &StreamRequest) -> Option<&'static str> {
    let level = req.level?;
    match req.reasoning {
        Reasoning::Effort { .. } => req.reasoning.effort(level).map(kage_core::Effort::as_str),
        Reasoning::None | Reasoning::Fixed | Reasoning::Toggle => None,
        Reasoning::Unknown | Reasoning::Budget { .. } => level.openai_reasoning_effort(),
    }
}

fn tool_spec_to_responses(spec: &ToolSpec) -> Value {
    serde_json::json!({
        "type": "function",
        "name": spec.name,
        "description": spec.description,
        "parameters": spec.schema,
    })
}

/// Convert one internal [`Message`] into zero-or-more Responses input
/// items. Assistant messages expand to a `message` item plus one
/// `function_call` item per [`Content::ToolCall`]; tool results become
/// `function_call_output` items.
fn internal_message_to_responses(msg: &Message, model: &str) -> Vec<Value> {
    match msg.role {
        Role::User => {
            let parts = convert_user_parts(&msg.content);
            if parts.is_empty() {
                Vec::new()
            } else {
                vec![serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": parts,
                })]
            }
        }
        Role::Assistant => convert_assistant_items(&msg.content, model),
        Role::ToolResult => convert_tool_result_items(&msg.content),
        Role::System => Vec::new(),
    }
}

fn convert_user_parts(blocks: &[Content]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"type":"input_text","text":text})),
            Content::Image { source, mime } => Some(image_part(source, mime)),
            _ => None,
        })
        .collect()
}

/// Assistant items on the wire. Encrypted reasoning goes back only to
/// the `model` that produced it.
fn convert_assistant_items(blocks: &[Content], model: &str) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    let mut text_parts: Vec<Value> = Vec::new();
    for block in blocks {
        match block {
            Content::Text { text } => {
                text_parts.push(serde_json::json!({"type":"output_text","text":text}));
            }
            Content::Image { mime, .. } => {
                let text = Content::produced_media_note(mime);
                text_parts.push(serde_json::json!({"type":"output_text","text":text}));
            }
            Content::Thinking {
                text, signature, ..
            } => {
                let encrypted = signature
                    .as_ref()
                    .filter(|s| s.model == model && !s.redacted);
                if text.trim().is_empty() && encrypted.is_none() {
                    continue;
                }
                flush_assistant_text(&mut items, &mut text_parts);
                let summary: Vec<Value> = if text.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![serde_json::json!({"type": "summary_text", "text": text})]
                };
                let mut item = serde_json::json!({"type": "reasoning", "summary": summary});
                if let Some(sig) = encrypted {
                    item["encrypted_content"] = Value::String(sig.data.clone());
                }
                items.push(item);
            }
            Content::ToolCall { id, name, input } => {
                flush_assistant_text(&mut items, &mut text_parts);
                items.push(serde_json::json!({
                    "type": "function_call",
                    "call_id": id.0,
                    "name": name,
                    "arguments": input.to_string(),
                }));
            }
            _ => {}
        }
    }
    flush_assistant_text(&mut items, &mut text_parts);
    items
}

/// Emit accumulated assistant text as one `message` item, in stream
/// order. Called at each non-text block so `reasoning` items keep their
/// position ahead of the `function_call` items they belong to.
fn flush_assistant_text(items: &mut Vec<Value>, text_parts: &mut Vec<Value>) {
    if text_parts.is_empty() {
        return;
    }
    items.push(serde_json::json!({
        "type": "message",
        "role": "assistant",
        "content": std::mem::take(text_parts),
    }));
}

fn convert_tool_result_items(blocks: &[Content]) -> Vec<Value> {
    kage_core::tool_results(blocks)
        .iter()
        .map(|result| {
            serde_json::json!({
                "type": "function_call_output",
                "call_id": result.call_id.0,
                "output": result.text_with_media_notes(),
            })
        })
        .collect()
}

fn image_part(source: &kage_core::MediaSource, mime: &str) -> Value {
    match source {
        kage_core::MediaSource::Url { url } => serde_json::json!({
            "type": "input_image",
            "image_url": url,
        }),
        kage_core::MediaSource::Base64 { data } => {
            let mime = if mime.is_empty() { "image/png" } else { mime };
            serde_json::json!({
                "type": "input_image",
                "image_url": format!("data:{mime};base64,{data}"),
            })
        }
        kage_core::MediaSource::Blob { .. } => serde_json::json!({
            "type": "input_text",
            "text": source.missing_note(mime),
        }),
    }
}

/// Iterator over a Responses API streaming response.
pub struct ResponsesStream {
    reader: BufReader<Box<dyn Read + Send>>,
    cancel: CancelFlag,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
    done: bool,
    started: bool,
    /// Function-call assembly state, keyed by `output_index`. The
    /// `ToolCallStart` event fires immediately when `output_item.added`
    /// arrives (the upstream always sends the tool name in that
    /// event); subsequent argument deltas append to `args` and emit
    /// `ToolCallArgsDelta` against the same id.
    tool_calls: BTreeMap<usize, ToolCallBuilder>,
    /// Argument deltas that arrived before their `output_item.added`,
    /// buffered per `output_index` and drained into the builder when
    /// the item finally starts.
    pre_add: BTreeMap<usize, String>,
    finish_reason: StopReason,
    usage: kage_core::TokenUsage,
}

struct ToolCallBuilder {
    id: ToolCallId,
    args: String,
}

impl ResponsesStream {
    /// Construct a stream from any byte source carrying Responses SSE.
    #[must_use]
    pub fn new(reader: Box<dyn Read + Send>, cancel: CancelFlag) -> Self {
        Self {
            reader: BufReader::new(reader),
            cancel,
            pending: VecDeque::new(),
            done: false,
            started: false,
            tool_calls: BTreeMap::new(),
            pre_add: BTreeMap::new(),
            finish_reason: StopReason::Other,
            usage: kage_core::TokenUsage::default(),
        }
    }

    fn process_chunk(&mut self, data: &str) {
        if data.trim() == "[DONE]" {
            // A DONE-only stream invents no turn, matching the Chat
            // Completions provider.
            if self.started {
                self.emit_message_end();
            }
            self.done = true;
            return;
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                self.pending
                    .push_back(Err(ProviderError::Decode(e.to_string())));
                return;
            }
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        match kind.as_str() {
            "response.created" | "response.in_progress" if !self.started => {
                self.pending.push_back(Ok(ProviderEvent::MessageStart));
                self.started = true;
            }
            "response.output_item.added" => self.on_output_item_added(&value),
            "response.output_text.delta" => self.on_text_delta(&value),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.on_thinking_delta(&value);
            }
            "response.function_call_arguments.delta" => self.on_function_call_args_delta(&value),
            "response.output_item.done" => self.on_output_item_done(&value),
            "response.completed" => {
                self.absorb_completed(&value);
                self.emit_message_end();
            }
            "response.incomplete" => {
                self.absorb_completed(&value);
                self.finish_reason = StopReason::MaxTokens;
                self.emit_message_end();
            }
            "response.failed" | "error" => {
                let kind = value
                    .pointer("/error/code")
                    .or_else(|| value.pointer("/response/error/code"))
                    .or_else(|| value.get("code"))
                    .map_or_else(
                        || "error".to_owned(),
                        |code| {
                            if let Value::String(s) = code {
                                s.clone()
                            } else {
                                code.to_string()
                            }
                        },
                    );
                let msg = value
                    .pointer("/error/message")
                    .or_else(|| value.pointer("/response/error/message"))
                    .or_else(|| value.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("openai responses stream failed");
                self.pending
                    .push_back(Err(ProviderError::from_stream_error(&kind, msg)));
                self.done = true;
            }
            _ => {}
        }
    }

    fn on_output_item_added(&mut self, value: &Value) {
        let Some(item) = value.get("item") else {
            return;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        if item_type != "function_call" {
            return;
        }
        let index = value
            .get("output_index")
            .and_then(Value::as_u64)
            .map_or(0, |v| usize::try_from(v).unwrap_or(0));
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let id = ToolCallId::new(call_id);
        self.tool_calls.insert(
            index,
            ToolCallBuilder {
                id: id.clone(),
                args: String::new(),
            },
        );
        self.pending.push_back(Ok(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name,
        }));
        if let Some(buffered) = self.pre_add.remove(&index)
            && !buffered.is_empty()
        {
            let entry = self.tool_calls.get_mut(&index).expect("just inserted");
            entry.args.push_str(&buffered);
            self.pending.push_back(Ok(ProviderEvent::ToolCallArgsDelta {
                id,
                partial: buffered,
            }));
        }
    }

    fn on_text_delta(&mut self, value: &Value) {
        let Some(delta) = value.get("delta").and_then(Value::as_str) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        self.pending.push_back(Ok(ProviderEvent::TextDelta {
            delta: delta.to_owned(),
        }));
    }

    fn on_thinking_delta(&mut self, value: &Value) {
        let Some(delta) = value.get("delta").and_then(Value::as_str) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        self.pending.push_back(Ok(ProviderEvent::ThinkingDelta {
            delta: delta.to_owned(),
        }));
    }

    fn on_function_call_args_delta(&mut self, value: &Value) {
        let index = value
            .get("output_index")
            .and_then(Value::as_u64)
            .map_or(0, |v| usize::try_from(v).unwrap_or(0));
        let Some(delta) = value.get("delta").and_then(Value::as_str) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        let Some(entry) = self.tool_calls.get_mut(&index) else {
            self.pre_add.entry(index).or_default().push_str(delta);
            return;
        };
        entry.args.push_str(delta);
        self.pending.push_back(Ok(ProviderEvent::ToolCallArgsDelta {
            id: entry.id.clone(),
            partial: delta.to_owned(),
        }));
    }

    /// A finished `image_generation_call`: the hosted image tool's
    /// picture, base64 in `result`, in `output_format` (PNG unless it
    /// says otherwise).
    fn on_image(&mut self, value: &Value) {
        let Some(data) = value
            .pointer("/item/result")
            .and_then(Value::as_str)
            .filter(|data| !data.is_empty())
        else {
            return;
        };
        let format = value
            .pointer("/item/output_format")
            .and_then(Value::as_str)
            .unwrap_or("png");
        let mime = if format == "jpg" {
            "image/jpeg".to_owned()
        } else {
            format!("image/{format}")
        };
        self.pending.push_back(Ok(ProviderEvent::Media {
            kind: kage_core::Modality::Image,
            mime,
            data: data.to_owned(),
        }));
    }

    fn on_output_item_done(&mut self, value: &Value) {
        let item_type = value
            .pointer("/item/type")
            .and_then(Value::as_str)
            .unwrap_or("");
        if item_type == "reasoning" {
            if let Some(data) = value
                .pointer("/item/encrypted_content")
                .and_then(Value::as_str)
            {
                self.pending.push_back(Ok(ProviderEvent::ThinkingSignature {
                    data: data.to_owned(),
                }));
            }
            return;
        }
        if item_type == "image_generation_call" {
            self.on_image(value);
            return;
        }
        if item_type != "function_call" {
            return;
        }
        let index = value
            .get("output_index")
            .and_then(Value::as_u64)
            .map_or(0, |v| usize::try_from(v).unwrap_or(0));
        let Some(builder) = self.tool_calls.remove(&index) else {
            return;
        };
        // Prefer the assembled-from-deltas args; fall back to the full
        // `arguments` string echoed on the `done` event if no deltas
        // arrived (e.g. servers that batch function-call output).
        let raw = if builder.args.is_empty() {
            value
                .pointer("/item/arguments")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        } else {
            builder.args
        };
        let input = if raw.is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            match serde_json::from_str::<Value>(&raw) {
                Ok(v) => v,
                Err(e) => {
                    self.pending.push_back(Err(ProviderError::Decode(format!(
                        "tool call {} arguments did not parse as JSON: {} (raw: {})",
                        builder.id.0, e, raw
                    ))));
                    self.finish_reason = StopReason::ToolUse;
                    return;
                }
            }
        };
        self.finish_reason = StopReason::ToolUse;
        self.pending.push_back(Ok(ProviderEvent::ToolCallEnd {
            id: builder.id,
            input,
        }));
    }

    fn absorb_completed(&mut self, value: &Value) {
        if let Some(usage) = value.pointer("/response/usage") {
            if let Some(v) = usage.get("input_tokens").and_then(Value::as_u64) {
                self.usage.input = v;
            }
            if let Some(v) = usage.get("output_tokens").and_then(Value::as_u64) {
                self.usage.output = v;
            }
            if let Some(v) = usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
            {
                self.usage.cache_read = v;
            }
        }
        // Promote `tool_calls` finish from any in-flight call; otherwise
        // a completed response with no tool calls is an end_turn.
        if matches!(self.finish_reason, StopReason::Other) {
            self.finish_reason = StopReason::EndTurn;
        }
    }

    fn emit_message_end(&mut self) {
        if !self.started {
            self.pending.push_back(Ok(ProviderEvent::MessageStart));
            self.started = true;
        }
        self.pending.push_back(Ok(ProviderEvent::MessageEnd {
            stop_reason: self.finish_reason,
            usage: crate::event::without_cached_input(self.usage),
        }));
        self.done = true;
    }
}

impl crate::sse::SseStreamCore for ResponsesStream {
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
    fn process(&mut self, _name: &str, data: &str) {
        // The Responses wire form is `event: <name>\ndata: <json>`,
        // but the JSON body carries its own `type` field, so the SSE
        // event name is intentionally ignored here.
        self.process_chunk(data);
    }
}

impl Iterator for ResponsesStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        crate::sse::sse_next(self)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::testing::{collect_ok, user_msg};

    #[test]
    fn body_uses_input_array_and_instructions_field() {
        let mut req = StreamRequest::new("gpt-5", vec![Arc::new(user_msg("hi"))]);
        req.system = Some("you are kage".into());
        let body = build_request_body(&req, true);
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["instructions"], "you are kage");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "hi");
    }

    /// The API default is `store: true`; kage must always opt out so
    /// requests are not retained at `OpenAI`.
    #[test]
    fn requests_opt_out_of_server_side_storage() {
        let req = StreamRequest::new("gpt-5", vec![Arc::new(user_msg("hi"))]);
        let body = build_request_body(&req, true);
        assert_eq!(body["store"], false);
    }

    #[test]
    fn body_uses_flat_function_tool_shape() {
        let mut req = StreamRequest::new("gpt-5", vec![Arc::new(user_msg("hi"))]);
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            schema: serde_json::json!({"type":"object"}),
        }];
        let body = build_request_body(&req, true);
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "read");
        assert_eq!(tool["description"], "read a file");
        assert_eq!(tool["parameters"]["type"], "object");
        // No nested {"function": {...}} envelope (that's Chat Completions).
        assert!(tool.get("function").is_none());
    }

    #[test]
    fn a_model_that_draws_is_offered_the_hosted_image_tool() {
        let mut req = StreamRequest::new("gpt-5", vec![Arc::new(user_msg("a cat"))]);
        assert!(build_request_body(&req, true).get("tools").is_none());
        req.produces =
            kage_core::Modalities::of(&[kage_core::Modality::Text, kage_core::Modality::Image]);
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            schema: serde_json::json!({"type":"object"}),
        }];
        let body = build_request_body(&req, true);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "read");
        assert_eq!(tools[1], serde_json::json!({"type": "image_generation"}));
    }

    #[test]
    fn the_provider_switch_offers_the_image_tool_to_every_model() {
        let req = || StreamRequest::new("gpt-5", vec![Arc::new(user_msg("a cat"))]);
        let plain = OpenAiResponsesProvider::new("k").request_body(&req());
        assert!(plain.get("tools").is_none());
        let drawing = OpenAiResponsesProvider::new("k")
            .with_image_generation(true)
            .request_body(&req());
        assert_eq!(
            drawing["tools"],
            serde_json::json!([{"type": "image_generation"}])
        );
    }

    #[test]
    fn body_includes_reasoning_block_when_level_set() {
        let mut req = StreamRequest::new("gpt-5", vec![Arc::new(user_msg("hi"))]);
        req.level = Some(crate::ThinkingLevel::High);
        let body = build_request_body(&req, true);
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn body_omits_reasoning_when_level_off() {
        let mut req = StreamRequest::new("gpt-5", vec![Arc::new(user_msg("hi"))]);
        req.level = Some(crate::ThinkingLevel::Off);
        let body = build_request_body(&req, true);
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn assistant_with_tool_call_emits_function_call_item() {
        let assistant = Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId::new("call_1"),
                name: "read".into(),
                input: serde_json::json!({"path":"/x"}),
            }],
            None,
        );
        let req = StreamRequest::new(
            "gpt-5",
            vec![Arc::new(user_msg("read")), Arc::new(assistant)],
        );
        let body = build_request_body(&req, true);
        let input = body["input"].as_array().unwrap();
        let last = &input[input.len() - 1];
        assert_eq!(last["type"], "function_call");
        assert_eq!(last["call_id"], "call_1");
        assert_eq!(last["name"], "read");
        let args_str = last["arguments"].as_str().unwrap();
        let args: Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(args["path"], "/x");
    }

    #[test]
    fn assistant_with_text_and_tool_call_emits_both_items_in_order() {
        let assistant = Message::new(
            Role::Assistant,
            vec![
                Content::Text {
                    text: "let me check".into(),
                },
                Content::ToolCall {
                    id: ToolCallId::new("call_1"),
                    name: "read".into(),
                    input: serde_json::json!({"path":"/x"}),
                },
            ],
            None,
        );
        let req = StreamRequest::new(
            "gpt-5",
            vec![Arc::new(user_msg("read")), Arc::new(assistant)],
        );
        let body = build_request_body(&req, true);
        let input = body["input"].as_array().unwrap();
        // user, then assistant message, then function_call.
        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["type"], "function_call");
    }

    #[test]
    fn tool_result_emits_function_call_output_item() {
        let result = Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new("call_1"),
                output: "127.0.0.1".into(),
                is_error: false,
            }],
            None,
        );
        let req = StreamRequest::new("gpt-5", vec![Arc::new(result)]);
        let body = build_request_body(&req, true);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[0]["output"], "127.0.0.1");
    }

    #[test]
    fn reasoning_precedes_function_call_on_replay() {
        let assistant = Message::new(
            Role::Assistant,
            vec![
                Content::Thinking {
                    text: "need the file first".into(),
                    signature: None,
                    duration_ms: None,
                },
                Content::ToolCall {
                    id: ToolCallId::new("call_1"),
                    name: "read".into(),
                    input: serde_json::json!({"path":"/x"}),
                },
            ],
            None,
        );
        let req = StreamRequest::new(
            "gpt-5",
            vec![Arc::new(user_msg("read")), Arc::new(assistant)],
        );
        let body = build_request_body(&req, true);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["summary"][0]["type"], "summary_text");
        assert_eq!(input[1]["summary"][0]["text"], "need the file first");
        assert_eq!(input[2]["type"], "function_call");
    }

    #[test]
    fn encrypted_reasoning_goes_back_to_its_own_model() {
        let assistant = Message::new(
            Role::Assistant,
            vec![
                Content::Thinking {
                    text: String::new(),
                    signature: Some(kage_core::ThinkingSignature {
                        model: "gpt-5".into(),
                        data: "enc".into(),
                        redacted: false,
                    }),
                    duration_ms: None,
                },
                Content::ToolCall {
                    id: ToolCallId::new("call_1"),
                    name: "read".into(),
                    input: serde_json::json!({}),
                },
            ],
            None,
        );
        let history = vec![Arc::new(user_msg("read")), Arc::new(assistant)];
        let body = build_request_body(&StreamRequest::new("gpt-5", history.clone()), true);
        assert_eq!(
            body["input"][1],
            serde_json::json!({"type": "reasoning", "summary": [], "encrypted_content": "enc"})
        );
        let body = build_request_body(&StreamRequest::new("gpt-5.1", history), true);
        assert_eq!(body["input"][1]["type"], "function_call");
    }

    #[test]
    fn reasoning_requests_ask_for_encrypted_reasoning() {
        let mut req = StreamRequest::new("gpt-5", vec![Arc::new(user_msg("hi"))]);
        assert!(build_request_body(&req, true).get("include").is_none());
        req.level = Some(crate::ThinkingLevel::High);
        let body = build_request_body(&req, true);
        assert_eq!(
            body["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
    }

    #[test]
    fn stream_captures_encrypted_reasoning() {
        let bytes: &[u8] = b"data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[],\"encrypted_content\":\"enc\"}}\n\ndata: [DONE]\n\n";
        let events = collect_ok(ResponsesStream::new(
            Box::new(std::io::Cursor::new(bytes)),
            CancelFlag::new(),
        ));
        assert!(matches!(
            &events[0],
            ProviderEvent::ThinkingSignature { data } if data == "enc"
        ));
    }

    #[test]
    fn blank_thinking_blocks_do_not_emit_reasoning_items() {
        let assistant = Message::new(
            Role::Assistant,
            vec![
                Content::Thinking {
                    text: "   ".into(),
                    signature: None,
                    duration_ms: None,
                },
                Content::Text {
                    text: "done".into(),
                },
            ],
            None,
        );
        let req = StreamRequest::new("gpt-5", vec![Arc::new(user_msg("hi")), Arc::new(assistant)]);
        let body = build_request_body(&req, true);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["content"][0]["text"], "done");
    }

    #[test]
    fn thinking_text_and_tool_call_keep_stream_order() {
        let assistant = Message::new(
            Role::Assistant,
            vec![
                Content::Thinking {
                    text: "why".into(),
                    signature: None,
                    duration_ms: None,
                },
                Content::Text {
                    text: "checking".into(),
                },
                Content::ToolCall {
                    id: ToolCallId::new("call_1"),
                    name: "read".into(),
                    input: serde_json::json!({"path":"/x"}),
                },
            ],
            None,
        );
        let req = StreamRequest::new(
            "gpt-5",
            vec![Arc::new(user_msg("read")), Arc::new(assistant)],
        );
        let body = build_request_body(&req, true);
        let input = body["input"].as_array().unwrap();
        let kinds: Vec<&str> = input[1..]
            .iter()
            .filter_map(|i| i["type"].as_str())
            .collect();
        assert_eq!(kinds, vec!["reasoning", "message", "function_call"]);
    }

    #[test]
    fn provider_preserves_thinking_for_reasoning_replay() {
        assert!(OpenAiResponsesProvider::new("k").preserves_thinking());
    }

    fn stream_from_bytes(bytes: &'static [u8]) -> ResponsesStream {
        ResponsesStream::new(Box::new(std::io::Cursor::new(bytes)), CancelFlag::new())
    }

    #[test]
    fn the_hosted_tool_s_image_arrives_as_media() {
        let bytes: &[u8] = b"event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"image_generation_call\",\"id\":\"ig_1\",\"status\":\"completed\",\"output_format\":\"webp\",\"result\":\"UklGR\"}}\n\nevent: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"image_generation_call\",\"id\":\"ig_2\",\"result\":\"iVBO\"}}\n\n";
        let media: Vec<_> = stream_from_bytes(bytes)
            .filter_map(Result::ok)
            .filter_map(|e| match e {
                ProviderEvent::Media { mime, data, .. } => Some((mime, data)),
                _ => None,
            })
            .collect();
        assert_eq!(
            media,
            [
                ("image/webp".to_owned(), "UklGR".to_owned()),
                ("image/png".to_owned(), "iVBO".to_owned()),
            ]
        );
    }

    #[test]
    fn stream_response_failed_rate_limit_classifies_as_rate_limited() {
        let bytes: &[u8] = b"event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":8,\"response\":{\"id\":\"resp_1\",\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit reached for responses\"}}}\n\n";
        let mut events = stream_from_bytes(bytes);
        let first = events.next().unwrap();
        assert!(
            matches!(
                first,
                Err(ProviderError::RateLimited {
                    retry_after: None,
                    ..
                })
            ),
            "got {first:?}"
        );
        assert!(events.next().is_none(), "stream ends after response.failed");
    }

    #[test]
    fn stream_error_event_server_error_is_transient() {
        let bytes: &[u8] = b"event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"The server had an error\"}\n\n";
        let mut events = stream_from_bytes(bytes);
        let first = events.next().unwrap();
        match first {
            Err(err) => assert!(err.is_transient(), "server_error should retry: {err:?}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn stream_emits_text_deltas_and_message_end() {
        let bytes: &[u8] = b"event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\" world\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        assert!(matches!(events[0], ProviderEvent::MessageStart));
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::TextDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hello", " world"]);
        if let Some(ProviderEvent::MessageEnd { stop_reason, usage }) = events.last() {
            assert_eq!(*stop_reason, StopReason::EndTurn);
            assert_eq!(usage.input, 10);
            assert_eq!(usage.output, 2);
        } else {
            panic!("expected MessageEnd");
        }
    }

    #[test]
    fn cached_input_tokens_leave_the_prompt_count() {
        let bytes: &[u8] = b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":6}}}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        if let Some(ProviderEvent::MessageEnd { usage, .. }) = events.last() {
            assert_eq!(usage.input, 4);
            assert_eq!(usage.cache_read, 6);
        } else {
            panic!("expected MessageEnd");
        }
    }

    #[test]
    fn stream_emits_thinking_deltas_for_reasoning_summary() {
        let bytes: &[u8] = b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"considering\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let thinking: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ThinkingDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, vec!["considering"]);
    }

    #[test]
    fn stream_assembles_tool_call_from_arg_deltas() {
        let bytes: &[u8] = b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"path\\\":\"}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"\\\"/tmp\\\"}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":3}}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let start = events
            .iter()
            .find(|e| matches!(e, ProviderEvent::ToolCallStart { .. }))
            .expect("ToolCallStart present");
        if let ProviderEvent::ToolCallStart { id, name } = start {
            assert_eq!(id.0, "call_1");
            assert_eq!(name, "read");
        }
        let args_count = events
            .iter()
            .filter(|e| matches!(e, ProviderEvent::ToolCallArgsDelta { .. }))
            .count();
        assert_eq!(args_count, 2);
        let end = events
            .iter()
            .find(|e| matches!(e, ProviderEvent::ToolCallEnd { .. }))
            .expect("ToolCallEnd present");
        if let ProviderEvent::ToolCallEnd { id, input } = end {
            assert_eq!(id.0, "call_1");
            assert_eq!(input["path"], "/tmp");
        }
        if let Some(ProviderEvent::MessageEnd { stop_reason, .. }) = events.last() {
            assert_eq!(*stop_reason, StopReason::ToolUse);
        }
    }

    #[test]
    fn stream_falls_back_to_arguments_on_output_item_done_when_no_deltas() {
        let bytes: &[u8] = b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"ls\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"ls\",\"arguments\":\"{\\\"path\\\":\\\"/\\\"}\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let end = events
            .iter()
            .find(|e| matches!(e, ProviderEvent::ToolCallEnd { .. }))
            .expect("ToolCallEnd present");
        if let ProviderEvent::ToolCallEnd { id, input } = end {
            assert_eq!(id.0, "call_2");
            assert_eq!(input["path"], "/");
        }
    }

    #[test]
    fn stream_emits_decode_error_when_tool_args_are_malformed_json() {
        let bytes: &[u8] = b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"write\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{not json\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let s = stream_from_bytes(bytes);
        let events: Vec<_> = s.collect();
        let decode_err = events
            .iter()
            .find(|r| matches!(r, Err(ProviderError::Decode(_))))
            .expect("expected a Decode error event for malformed tool args");
        if let Err(ProviderError::Decode(msg)) = decode_err {
            assert!(msg.contains("call_1"));
            assert!(msg.contains("{not json"));
        }
    }

    #[test]
    fn stream_propagates_failed_event_as_decode_error() {
        let bytes: &[u8] = b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.failed\",\"error\":{\"message\":\"rate limited\"}}\n\n";
        let s = stream_from_bytes(bytes);
        let events: Vec<_> = s.collect();
        let err = events
            .iter()
            .find(|r| matches!(r, Err(ProviderError::Decode(_))))
            .expect("expected a Decode error for response.failed");
        if let Err(ProviderError::Decode(msg)) = err {
            assert!(msg.contains("rate limited"));
        }
    }

    #[test]
    fn stream_yields_cancelled_when_flag_set() {
        let bytes: &[u8] = b"data: {\"type\":\"response.created\"}\n\n";
        let cancel = CancelFlag::new();
        cancel.cancel();
        let mut s = ResponsesStream::new(Box::new(std::io::Cursor::new(bytes)), cancel);
        assert!(matches!(s.next(), Some(Err(ProviderError::Cancelled))));
        assert!(s.next().is_none());
    }

    /// A stream whose first frame is `[DONE]` invents no turn, matching
    /// the Chat Completions provider.
    #[test]
    fn done_before_any_output_invents_nothing() {
        let mut events = stream_from_bytes(b"data: [DONE]\n\n");
        assert!(events.next().is_none());
    }

    fn data_url(blocks: &[Content]) -> String {
        let block = convert_user_parts(blocks).remove(0);
        block["image_url"].as_str().expect("image url").to_owned()
    }

    #[test]
    fn base64_images_forward_their_mime() {
        let image = |mime: &str| Content::Image {
            source: kage_core::MediaSource::Base64 {
                data: "QUJD".into(),
            },
            mime: mime.to_owned(),
        };
        assert_eq!(
            data_url(&[image("image/jpeg")]),
            "data:image/jpeg;base64,QUJD"
        );
        assert_eq!(
            data_url(&[image("image/webp")]),
            "data:image/webp;base64,QUJD"
        );
        assert_eq!(data_url(&[image("")]), "data:image/png;base64,QUJD");
    }

    /// Argument deltas that arrive before their `output_item.added`
    /// are buffered and fold into the same call once it starts.
    #[test]
    fn args_deltas_before_output_item_added_are_buffered() {
        let bytes: &[u8] = b"data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"path\\\":\"}\n\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"\\\"/tmp\\\"}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":3}}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let start = events
            .iter()
            .position(|e| matches!(e, ProviderEvent::ToolCallStart { .. }))
            .expect("ToolCallStart present");
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ToolCallArgsDelta { partial, .. } => Some(partial.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, ["{\"path\":", "\"/tmp\"}"]);
        assert!(
            events[..start]
                .iter()
                .all(|e| !matches!(e, ProviderEvent::ToolCallArgsDelta { .. })),
            "no args delta precedes the start"
        );
        let end = events
            .iter()
            .find(|e| matches!(e, ProviderEvent::ToolCallEnd { .. }))
            .expect("ToolCallEnd present");
        if let ProviderEvent::ToolCallEnd { id, input } = end {
            assert_eq!(id.0, "call_1");
            assert_eq!(input["path"], "/tmp");
        }
    }

    #[test]
    fn request_headers_include_auth_then_extras_in_key_order() {
        let mut extras = BTreeMap::new();
        extras.insert("X-B".to_owned(), "2".to_owned());
        extras.insert("X-A".to_owned(), "1".to_owned());
        let provider = OpenAiResponsesProvider::new("k").with_extra_headers(extras);
        assert_eq!(
            provider.request_headers(None),
            vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("authorization".to_owned(), "Bearer k".to_owned()),
                ("X-A".to_owned(), "1".to_owned()),
                ("X-B".to_owned(), "2".to_owned()),
            ]
        );
    }

    #[test]
    fn request_headers_skip_auth_when_key_is_empty() {
        let mut extras = BTreeMap::new();
        extras.insert("X-A".to_owned(), "1".to_owned());
        let provider = OpenAiResponsesProvider::new("").with_extra_headers(extras);
        let headers = provider.request_headers(None);
        assert!(
            headers.iter().all(|(name, _)| name != "authorization"),
            "no credential header without a key: {headers:?}"
        );
        assert!(headers.contains(&("X-A".to_owned(), "1".to_owned())));
    }

    #[test]
    fn with_models_overrides_advertised_models() {
        let models = vec![ProviderModel {
            id: "test-model".to_owned(),
            name: "Test Model".to_owned(),
            context: Some(128_000),
            max_output: Some(8_192),
            ..ProviderModel::default()
        }];
        let provider = OpenAiResponsesProvider::new("k").with_models(models.clone());
        assert_eq!(provider.models(), models);
        assert!(OpenAiResponsesProvider::new("k").models().is_empty());
    }

    struct FixedBearer {
        token: std::sync::Mutex<String>,
        rejected: std::sync::Mutex<Vec<String>>,
    }

    impl FixedBearer {
        fn new(token: &str) -> Self {
            Self {
                token: std::sync::Mutex::new(token.to_owned()),
                rejected: std::sync::Mutex::default(),
            }
        }
    }

    impl crate::bearer::BearerSource for FixedBearer {
        fn token(&self) -> String {
            self.token.lock().unwrap().clone()
        }

        fn refresh(&self, rejected: &str) -> Result<String, ProviderError> {
            self.rejected.lock().unwrap().push(rejected.to_owned());
            "refreshed".clone_into(&mut self.token.lock().unwrap());
            Ok(self.token())
        }
    }

    #[test]
    fn request_headers_prefer_the_bearer_token_over_the_stored_key() {
        let provider = OpenAiResponsesProvider::new("static");
        let find = |headers: &[(String, String)]| {
            headers
                .iter()
                .find(|(n, _)| n == "authorization")
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(
            find(&provider.request_headers(Some("oauth"))),
            "Bearer oauth"
        );
        assert_eq!(find(&provider.request_headers(None)), "Bearer static");
    }

    #[test]
    fn the_chatgpt_backend_body_drops_refused_fields_and_keys_the_cache() {
        let mut req = StreamRequest::new("gpt-5", vec![Arc::new(user_msg("hi"))]);
        req.temperature = Some(0.2);
        req.session = Some("ses-1".to_owned());
        req.level = Some(kage_core::ThinkingLevel::Low);
        let public = OpenAiResponsesProvider::new("k").request_body(&req);
        assert!(public.get("max_output_tokens").is_some());
        assert!(public.get("temperature").is_some());
        assert!(public.get("prompt_cache_key").is_none());
        assert!(public["reasoning"].get("summary").is_none(), "{public}");
        let chatgpt = OpenAiResponsesProvider::new("k")
            .for_chatgpt_backend()
            .request_body(&req);
        assert!(chatgpt.get("max_output_tokens").is_none(), "{chatgpt}");
        assert!(chatgpt.get("temperature").is_none(), "{chatgpt}");
        assert_eq!(chatgpt["prompt_cache_key"], "ses-1");
        assert_eq!(chatgpt["reasoning"]["effort"], "low");
        assert_eq!(chatgpt["reasoning"]["summary"], "auto");
        assert_eq!(chatgpt["store"], false);
    }

    /// A zstd provider compresses the body and says so; the `ChatGPT`
    /// mode adds the session header.
    #[test]
    fn a_zstd_provider_sends_a_compressed_body_with_the_session_header() {
        use std::io::Write as _;

        const SSE: &str =
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: [DONE]\n\n";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            let head_end = loop {
                let n = conn.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let head = String::from_utf8_lossy(&raw[..head_end]).to_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length: "))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            while raw.len() < head_end + length {
                let n = conn.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
            }
            let _ = conn.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{SSE}",
                    SSE.len()
                )
                .as_bytes(),
            );
            (head, raw[head_end..head_end + length].to_vec())
        });
        let provider = OpenAiResponsesProvider::with_base_url("k", origin)
            .for_chatgpt_backend()
            .with_zstd_bodies();
        let mut req = StreamRequest::new("gpt-5", vec![Arc::new(user_msg("hi"))]);
        req.session = Some("ses-9".to_owned());
        collect_ok(provider.stream(req, &CancelFlag::new()).unwrap());
        let (head, body) = server.join().unwrap();
        assert!(head.contains("\r\ncontent-encoding: zstd\r\n"), "{head}");
        assert!(head.contains("\r\nsession-id: ses-9\r\n"), "{head}");
        let json: Value =
            serde_json::from_slice(&zstd::stream::decode_all(body.as_slice()).unwrap()).unwrap();
        assert_eq!(json["model"], "gpt-5");
        assert_eq!(json["prompt_cache_key"], "ses-9");
    }

    #[test]
    fn debug_redacts_the_api_key() {
        let debug = format!("{:?}", OpenAiResponsesProvider::new("sk-secret"));
        assert!(!debug.contains("sk-secret"), "{debug}");
        assert!(debug.contains("<set>"), "{debug}");
    }

    /// A 401 on a renewable credential refreshes once and replays the
    /// request with the fresh token; the stream then proceeds.
    #[test]
    fn a_rejected_bearer_refreshes_once_and_retries() {
        use std::io::Write as _;

        const SSE: &str =
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: [DONE]\n\n";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let tokens: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&tokens);
        std::thread::spawn(move || {
            for (i, conn) in listener.incoming().enumerate() {
                let Ok(mut conn) = conn else { return };
                let mut buf = [0u8; 8192];
                let _ = conn.read(&mut buf);
                let head = String::from_utf8_lossy(&buf).into_owned();
                let token = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("authorization"))
                    .and_then(|l| l.split_once(' '))
                    .map(|(_, t)| t.to_owned())
                    .unwrap_or_default();
                seen.lock().unwrap().push(token);
                if i == 0 {
                    let body = "{\"error\":{\"message\":\"token expired\"}}";
                    let _ = conn.write_all(
                        format!(
                            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .into_bytes()
                        .as_slice(),
                    );
                } else {
                    let _ = conn.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{SSE}",
                            SSE.len()
                        )
                        .into_bytes()
                        .as_slice(),
                    );
                }
            }
        });

        let bearer = Arc::new(FixedBearer::new("stale"));
        let provider = OpenAiResponsesProvider::with_base_url("", origin)
            .with_bearer(Arc::clone(&bearer) as _);
        let req = StreamRequest::new("gpt-5", vec![Arc::new(user_msg("hi"))]);
        let events = collect_ok(provider.stream(req, &CancelFlag::new()).unwrap());
        assert!(
            events.iter().any(|e| matches!(
                e,
                ProviderEvent::TextDelta { delta } if delta == "ok"
            )),
            "{events:?}"
        );
        assert_eq!(
            *tokens.lock().unwrap(),
            ["Bearer stale", "Bearer refreshed"]
        );
        assert_eq!(*bearer.rejected.lock().unwrap(), ["stale"]);
    }

    /// When refresh itself fails, the 401 surfaces as the auth error.
    #[test]
    fn a_failed_refresh_leaves_the_auth_error() {
        struct Refuses;
        impl crate::bearer::BearerSource for Refuses {
            fn token(&self) -> String {
                "stale".to_owned()
            }

            fn refresh(&self, _rejected: &str) -> Result<String, ProviderError> {
                Err(ProviderError::Auth("login expired".to_owned()))
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut buf = [0u8; 8192];
                let _ = conn.read(&mut buf);
                let body = "{\"error\":{\"message\":\"expired\"}}";
                let _ = std::io::Write::write_all(
                    &mut conn,
                    format!(
                        "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .into_bytes()
                    .as_slice(),
                );
            }
        });
        let provider =
            OpenAiResponsesProvider::with_base_url("", origin).with_bearer(Arc::new(Refuses));
        let req = StreamRequest::new("gpt-5", vec![Arc::new(user_msg("hi"))]);
        let Err(err) = provider.stream(req, &CancelFlag::new()) else {
            panic!("expected the auth error")
        };
        assert!(
            matches!(err, ProviderError::Auth(ref detail) if detail == "login expired"),
            "{err:?}"
        );
    }
}

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
    fn effort_models_send_their_own_effort_values() {
        let r = effort(
            &[Effort::None, Effort::Low, Effort::High, Effort::XHigh],
            false,
        );
        let body = build_request_body(&request(r, ThinkingLevel::XHigh), true);
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        let body = build_request_body(&request(r, ThinkingLevel::Off), true);
        assert_eq!(body["reasoning"]["effort"], "none");
        let body = build_request_body(&request(Reasoning::None, ThinkingLevel::High), true);
        assert!(body.get("reasoning").is_none());
    }
}
