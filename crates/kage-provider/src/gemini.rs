//! Google Gemini provider.
//!
//! Implements `streamGenerateContent` on the Generative Language API. The
//! streaming endpoint returns a JSON array as Server-Sent Events with
//! `data:` chunks. Function calls and text are emitted in the response
//! `candidates[0].content.parts` array.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufReader, Read};

use kage_core::{CancelFlag, Content, Message, Reasoning, Role, ToolCallId};
use serde_json::Value;

use crate::{
    EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, ProviderModel,
    StopReason, StreamRequest, ToolSpec,
};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
/// Placeholder Google documents for function calls that carry no real
/// thought signature, such as history from another model.
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// Google Gemini provider.
#[derive(Debug)]
pub struct GeminiProvider {
    api_key: String,
    base_url: String,
    metadata: ProviderMetadata,
    /// Extra headers sent on every request, after the protocol's own.
    extra_headers: BTreeMap<String, String>,
    /// Models advertised from `Provider::models` (custom providers);
    /// empty lets the catalog drive the picker.
    models: Vec<ProviderModel>,
}

impl GeminiProvider {
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
                id: "google".into(),
                display_name: "Google Gemini".into(),
                supports_caching: false,
                supports_thinking: false,
                supports_tool_use: true,
            },
            extra_headers: BTreeMap::new(),
            models: Vec::new(),
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

    /// Headers every request carries: content type, the key header
    /// (skipped when no key is configured), then the configured extras
    /// in key order. The key travels in a header, never the query
    /// string, so it cannot leak through URL logging or proxy records.
    fn request_headers(&self) -> Vec<(String, String)> {
        let credential =
            (!self.api_key.is_empty()).then(|| ("x-goog-api-key", self.api_key.clone()));
        crate::headers::request_headers(
            &[("content-type", "application/json".to_owned())],
            credential,
            &self.extra_headers,
        )
    }
}

impl Provider for GeminiProvider {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn models(&self) -> Vec<ProviderModel> {
        self.models.clone()
    }

    /// Thought signatures go back on their function calls, and thinking
    /// text as `<thinking>` text, see `build_request_body`.
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
        let body = build_request_body(&req);
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.base_url, req.model
        );
        let headers = self.request_headers();
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
        let inner: EventStream = Box::new(GeminiStream::new(reader, cancel.clone()));
        Ok(crate::cancelable::make_cancelable(
            inner,
            cancel.clone(),
            kill,
        ))
    }
}

/// Build the JSON body for a Gemini streamGenerateContent request.
pub(crate) fn build_request_body(req: &StreamRequest) -> Value {
    // Gemini's functionResponse is keyed by function NAME, but our
    // result blocks only carry the correlation id; recover the name
    // from the assistant turn that issued each call.
    let names_by_id: HashMap<String, String> = req
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|c| match c {
            Content::ToolCall { id, name, .. } => Some((id.0.clone(), name.clone())),
            _ => None,
        })
        .collect();
    let turn_start = req
        .messages
        .iter()
        .rposition(|m| {
            m.role == Role::User && m.content.iter().any(|c| matches!(c, Content::Text { .. }))
        })
        .unwrap_or(0);
    let enforced = enforces_signatures(&req.model);
    // A model that draws gets its own images back, so it can keep
    // working on them; any other model reads a note in their place.
    let images = req.produces.contains(kage_core::Modality::Image);
    let contents: Vec<Value> = req
        .messages
        .iter()
        .enumerate()
        .filter_map(|(i, msg)| {
            let placeholder = enforced && i >= turn_start;
            internal_message_to_gemini(
                msg,
                &names_by_id,
                &req.tools,
                &req.model,
                placeholder,
                images,
            )
        })
        .collect();

    let mut body = serde_json::json!({
        "contents": contents,
        "generationConfig": {
            "maxOutputTokens": req.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        },
    });

    if let Some(system) = &req.system {
        body["systemInstruction"] = serde_json::json!({
            "parts": [{"text": system}],
        });
    }
    if let Some(temp) = req.temperature {
        body["generationConfig"]["temperature"] = serde_json::json!(temp);
    }
    if let Some(config) = thinking_config(req) {
        body["generationConfig"]["thinkingConfig"] = config;
    }
    if req.produces.contains(kage_core::Modality::Image) {
        body["generationConfig"]["responseModalities"] = serde_json::json!(["TEXT", "IMAGE"]);
    }
    if !req.tools.is_empty() {
        body["tools"] = serde_json::json!([{
            "functionDeclarations": req.tools.iter().map(tool_spec_to_gemini).collect::<Vec<_>>(),
        }]);
    }
    body
}

/// The `thinkingConfig` for a Gemini request, or `None` to leave it
/// out. An explicit [`crate::ThinkingConfig`] budget wins. Otherwise
/// effort models get a `thinkingLevel`, toggle models the dynamic
/// budget (`-1`), and budget models a budget within their bounds. Off
/// sends a zero budget to models that can switch thinking off.
fn thinking_config(req: &StreamRequest) -> Option<Value> {
    if let Some(thinking) = &req.thinking {
        return Some(serde_json::json!({"thinkingBudget": thinking.budget_tokens}));
    }
    let level = req.level?;
    let budget = |tokens: i64| Some(serde_json::json!({"thinkingBudget": tokens}));
    match req.reasoning {
        Reasoning::None | Reasoning::Fixed => None,
        Reasoning::Effort { .. } if !level.is_off() => req
            .reasoning
            .effort(level)
            .map(|e| serde_json::json!({"thinkingLevel": e.as_str()})),
        _ if level.is_off() => budget(0).filter(|_| req.reasoning.has_toggle()),
        Reasoning::Toggle => budget(-1),
        Reasoning::Unknown | Reasoning::Effort { .. } | Reasoning::Budget { .. } => req
            .reasoning
            .budget(level)
            .and_then(|tokens| budget(i64::from(tokens))),
    }
}

/// Whether `model` rejects a current-turn function call without a
/// thought signature, which Gemini 3 and later do.
///
/// Name-based: the last path segment must carry a `gemini` marker
/// (anywhere in it, case-insensitive, so gateway ids like
/// `my-gemini-3` count), and the version is the digit run that follows
/// the marker once separators are skipped. A segment whose next
/// character is a letter names no version (`gemini-exp-1206`) and
/// enforces nothing.
fn enforces_signatures(model: &str) -> bool {
    let name = model.rsplit('/').next().unwrap_or(model);
    let name = name.to_ascii_lowercase();
    let Some((_, rest)) = name.split_once("gemini") else {
        return false;
    };
    let rest = rest.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let version: String = rest.chars().take_while(char::is_ascii_digit).collect();
    version.parse::<u32>().is_ok_and(|major| major >= 3)
}

fn tool_spec_to_gemini(spec: &ToolSpec) -> Value {
    serde_json::json!({
        "name": spec.name,
        "description": spec.description,
        "parameters": gemini_schema(&spec.schema),
    })
}

/// Map a draft-2020-12 JSON Schema (what `schemars` emits, or what an
/// MCP server ships) into the `OpenAPI` subset Gemini's
/// `functionDeclarations.parameters` accepts: `$ref`s inlined from
/// `$defs`, draft keys dropped, `type` arrays (how `Option<T>` fields
/// arrive) turned into Gemini's `nullable` flag, `const` folded into
/// the `enum` Gemini knows, `uint*` formats renamed to the ones Gemini
/// knows, and any key outside Gemini's `Schema` field set dropped. The
/// drop is the point: Gemini rejects the whole request over one
/// unknown name, and a third-party MCP schema can carry anything.
fn gemini_schema(schema: &Value) -> Value {
    let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
    convert_schema(schema, &defs, 0)
}

/// Gemini `Schema` fields besides `type`, `format`, `const` and the
/// draft keys, which [`convert_schema`] handles specially. Keys not on
/// this list are dropped.
const GEMINI_SCHEMA_KEYS: &[&str] = &[
    "title",
    "description",
    "nullable",
    "enum",
    "items",
    "minItems",
    "maxItems",
    "required",
    "minProperties",
    "maxProperties",
    "minLength",
    "maxLength",
    "pattern",
    "minimum",
    "maximum",
    "anyOf",
    "oneOf",
    "propertyOrdering",
];

/// Gemini `Schema` fields whose value is arbitrary JSON rather than a
/// schema; kept verbatim instead of converted.
const VERBATIM_KEYS: &[&str] = &["default", "example"];

fn convert_schema(value: &Value, defs: &Value, depth: usize) -> Value {
    // Cycle guard: schemars output for plain structs is acyclic, but a
    // hand-written schema could reference itself.
    if depth > 16 {
        return value.clone();
    }
    match value {
        Value::Object(map) => {
            // Inline the referenced definition whole; it is converted
            // like any other schema node.
            if let Some(Value::String(reference)) = map.get("$ref")
                && let Some(target) = defs.get(reference.trim_start_matches("#/$defs/"))
            {
                return convert_schema(target, defs, depth + 1);
            }
            let mut out = serde_json::Map::new();
            for (key, val) in map {
                match key.as_str() {
                    // Draft tooling keys, and the map-value schema
                    // Gemini has no analogue for: dropped rather than
                    // failing the request.
                    "$schema" | "$id" | "$comment" | "$ref" | "$defs" | "additionalProperties" => {}
                    "type" => match val {
                        Value::String(t) => {
                            out.insert("type".to_owned(), Value::String(t.clone()));
                        }
                        // `[T, "null"]` is OpenAPI's forbidden spelling
                        // of an optional value; Gemini wants `nullable`.
                        Value::Array(types) => {
                            let base = types.iter().find(|t| t.as_str() != Some("null"));
                            if let Some(base) = base {
                                out.insert("type".to_owned(), base.clone());
                                if types.contains(&Value::String("null".into())) {
                                    out.insert("nullable".to_owned(), Value::Bool(true));
                                }
                            }
                        }
                        other => {
                            out.insert("type".to_owned(), other.clone());
                        }
                    },
                    "format" => {
                        let name = val.as_str().unwrap_or_default();
                        let mapped = match name {
                            "uint32" => "int32",
                            "uint64" => "int64",
                            other => other,
                        };
                        out.insert("format".to_owned(), Value::String(mapped.to_owned()));
                    }
                    // A fixed value is Gemini's one-element `enum`,
                    // which only takes strings.
                    "const" => {
                        if let Some(text) = val.as_str() {
                            out.insert("enum".to_owned(), serde_json::json!([text]));
                        }
                    }
                    // Gemini's `enum` is an array of strings; a numeric
                    // or mixed one would fail the request the same way
                    // an unknown key would.
                    "enum" => {
                        let strings: Vec<String> = val
                            .as_array()
                            .map(|values| {
                                values
                                    .iter()
                                    .filter_map(|v| v.as_str())
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default();
                        if !strings.is_empty() {
                            out.insert(
                                "enum".to_owned(),
                                Value::Array(strings.into_iter().map(Value::String).collect()),
                            );
                        }
                    }
                    // A map of property name to schema: the names are
                    // property names, not schema keys, so each value is
                    // converted on its own.
                    "properties" => {
                        if let Value::Object(props) = val {
                            let converted = props
                                .iter()
                                .map(|(name, schema)| {
                                    (name.clone(), convert_schema(schema, defs, depth + 1))
                                })
                                .collect();
                            out.insert("properties".to_owned(), Value::Object(converted));
                        }
                    }
                    key if VERBATIM_KEYS.contains(&key) => {
                        out.insert(key.to_owned(), val.clone());
                    }
                    key if GEMINI_SCHEMA_KEYS.contains(&key) => {
                        out.insert(key.to_owned(), convert_schema(val, defs, depth + 1));
                    }
                    // Anything else is no Gemini field; dropped so one
                    // unknown name cannot reject the whole request.
                    _ => {}
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| convert_schema(item, defs, depth + 1))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn internal_message_to_gemini(
    msg: &Message,
    names_by_id: &HashMap<String, String>,
    declared: &[ToolSpec],
    model: &str,
    placeholder: bool,
    images: bool,
) -> Option<Value> {
    let (role, parts) = match msg.role {
        Role::User => ("user", convert_user_parts(&msg.content)),
        Role::Assistant => (
            "model",
            convert_assistant_parts(&msg.content, model, placeholder, images),
        ),
        Role::ToolResult => (
            "user",
            convert_tool_result_parts(&msg.content, names_by_id, declared),
        ),
        Role::System => return None,
    };
    if parts.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "role": role,
        "parts": parts,
    }))
}

fn convert_user_parts(blocks: &[Content]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"text": text})),
            Content::Image { source, mime } => Some(image_part(source, mime)),
            _ => None,
        })
        .collect()
}

/// Model parts on the wire. A thought signature `model` produced goes
/// back on the function call that follows it, which Gemini 3 requires
/// on the first call of every step in the current turn. With
/// `placeholder`, a first call that has no real signature carries
/// [`SKIP_SIGNATURE`] instead. Other thinking goes as `<thinking>` text.
/// With `images`, an image the model produced goes back as inline data,
/// carrying the signature before it; without, as a short note, and the
/// signature is dropped so it never lands on a later call.
fn convert_assistant_parts(
    blocks: &[Content],
    model: &str,
    placeholder: bool,
    images: bool,
) -> Vec<Value> {
    let mut parts = Vec::new();
    let mut signature: Option<&str> = None;
    let mut first_call = true;
    for block in blocks {
        match block {
            Content::Text { text } => parts.push(serde_json::json!({"text": text})),
            Content::Thinking {
                signature: Some(sig),
                ..
            } if sig.model == model && !sig.redacted => signature = Some(&sig.data),
            Content::Thinking { text, .. } => {
                if let Some(text) = Content::flattened_thinking(text) {
                    parts.push(serde_json::json!({"text": text}));
                }
            }
            Content::ToolCall { name, input, .. } => {
                let mut part = serde_json::json!({
                    "functionCall": {
                        "name": name,
                        "args": input,
                    },
                });
                if let Some(sig) = signature.take() {
                    part["thoughtSignature"] = Value::String(sig.to_owned());
                } else if placeholder && first_call {
                    part["thoughtSignature"] = Value::String(SKIP_SIGNATURE.to_owned());
                }
                first_call = false;
                parts.push(part);
            }
            Content::Image { source, mime } if images => {
                let mut part = image_part(source, mime);
                if let Some(sig) = signature.take()
                    && part.get("inlineData").is_some()
                {
                    part["thoughtSignature"] = Value::String(sig.to_owned());
                }
                parts.push(part);
            }
            Content::Image { mime, .. } => {
                signature = None;
                parts.push(serde_json::json!({"text": Content::produced_media_note(mime)}));
            }
            _ => {}
        }
    }
    parts
}

fn convert_tool_result_parts(
    blocks: &[Content],
    names_by_id: &HashMap<String, String>,
    declared: &[ToolSpec],
) -> Vec<Value> {
    kage_core::tool_results(blocks)
        .iter()
        .map(|result| {
            let call_id = result.call_id;
            let name = names_by_id
                .get(&call_id.0)
                .map_or_else(|| tool_name_from_call_id(call_id, declared), String::as_str);
            serde_json::json!({
                "functionResponse": {
                    "name": name,
                    "response": {"output": result.text_with_media_notes()},
                },
            })
        })
        .collect()
}

/// Recover the function name from a synthesized `gemini_{name}_{n}`
/// correlation id when no matching assistant tool call is in history
/// (resumed sessions written by older kage versions). The remainder is
/// first matched against the request's declared tool names, because a
/// legacy counter-less id for a tool literally named `tool_2` would
/// otherwise decode as `tool`; ids without the trailing counter decode
/// as the whole `gemini_` suffix.
fn tool_name_from_call_id<'a>(id: &'a ToolCallId, declared: &'a [ToolSpec]) -> &'a str {
    let rest = id.0.strip_prefix("gemini_").unwrap_or(&id.0);
    if let Some(tool) = declared.iter().find(|t| t.name == rest) {
        return tool.name.as_str();
    }
    match rest.rsplit_once('_') {
        Some((name, counter))
            if !counter.is_empty() && counter.bytes().all(|b| b.is_ascii_digit()) =>
        {
            name
        }
        _ => rest,
    }
}

fn image_part(source: &kage_core::MediaSource, mime: &str) -> Value {
    match source {
        kage_core::MediaSource::Base64 { data } => serde_json::json!({
            "inlineData": {
                "mimeType": mime,
                "data": data,
            },
        }),
        kage_core::MediaSource::Url { url } => serde_json::json!({
            "fileData": {
                "mimeType": mime,
                "fileUri": url,
            },
        }),
        kage_core::MediaSource::Blob { .. } => serde_json::json!({
            "text": source.missing_note(mime),
        }),
    }
}

/// Iterator over a Gemini streaming response.
pub struct GeminiStream {
    reader: BufReader<Box<dyn Read + Send>>,
    cancel: CancelFlag,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
    done: bool,
    started: bool,
    /// Function calls seen in this stream. Gemini emits no per-call ids,
    /// so each call gets a fresh `gemini_{name}_{n}` correlation id;
    /// name-keyed reuse would collide when one turn makes parallel
    /// same-name calls.
    tool_call_count: usize,
    finish_reason: StopReason,
    usage: kage_core::TokenUsage,
    /// Cumulative `candidatesTokenCount`, overwrite-only per chunk.
    output_candidates: u64,
    /// Cumulative `thoughtsTokenCount`, overwrite-only per chunk. Kept
    /// apart from the candidates count so a chunk that omits either
    /// field cannot double-count the other when the fold happens.
    output_thoughts: u64,
}

impl GeminiStream {
    /// Construct a stream from any byte source carrying Gemini SSE.
    #[must_use]
    pub fn new(reader: Box<dyn Read + Send>, cancel: CancelFlag) -> Self {
        Self {
            reader: BufReader::new(reader),
            cancel,
            pending: VecDeque::new(),
            done: false,
            started: false,
            tool_call_count: 0,
            finish_reason: StopReason::Other,
            usage: kage_core::TokenUsage::default(),
            output_candidates: 0,
            output_thoughts: 0,
        }
    }

    fn process_chunk(&mut self, data: &str) {
        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                self.pending
                    .push_back(Err(ProviderError::Decode(e.to_string())));
                return;
            }
        };
        if let Some(err) = value.get("error").filter(|e| !e.is_null()) {
            let kind = err
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| err.get("code").map(Value::to_string))
                .unwrap_or_else(|| "error".to_owned());
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider stream failed");
            self.pending
                .push_back(Err(ProviderError::from_stream_error(&kind, msg)));
            self.done = true;
            return;
        }
        if !self.started {
            self.pending.push_back(Ok(ProviderEvent::MessageStart));
            self.started = true;
        }
        if let Some(usage) = value.get("usageMetadata") {
            self.absorb_usage(usage);
        }
        let Some(candidates) = value.get("candidates").and_then(Value::as_array) else {
            return;
        };
        for candidate in candidates {
            if let Some(parts) = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
            {
                for part in parts {
                    self.process_part(part);
                }
            }
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                self.finish_reason = parse_finish_reason(reason);
            }
        }
    }

    fn process_part(&mut self, part: &Value) {
        if let Some(inline) = part.get("inlineData") {
            // An image the model drew while thinking is not its answer.
            let thought = part.get("thought").and_then(Value::as_bool) == Some(true);
            let mime = inline.get("mimeType").and_then(Value::as_str).unwrap_or("");
            let data = inline.get("data").and_then(Value::as_str).unwrap_or("");
            if let Some(kind) =
                kage_core::Modality::of_mime(mime).filter(|_| !thought && !data.is_empty())
            {
                // An image model signs its images; the signature goes
                // back on the image when the conversation continues.
                if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
                    self.pending.push_back(Ok(ProviderEvent::ThinkingSignature {
                        data: signature.to_owned(),
                    }));
                }
                self.pending.push_back(Ok(ProviderEvent::Media {
                    kind,
                    mime: mime.to_owned(),
                    data: data.to_owned(),
                }));
            }
            return;
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                self.pending.push_back(Ok(ProviderEvent::TextDelta {
                    delta: text.to_owned(),
                }));
            }
            return;
        }
        if let Some(call) = part.get("functionCall") {
            if let Some(data) = part.get("thoughtSignature").and_then(Value::as_str) {
                self.pending.push_back(Ok(ProviderEvent::ThinkingSignature {
                    data: data.to_owned(),
                }));
            }
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let args = call
                .get("args")
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            self.tool_call_count += 1;
            let id = ToolCallId::new(format!("gemini_{name}_{}", self.tool_call_count));
            self.pending.push_back(Ok(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name,
            }));
            self.pending
                .push_back(Ok(ProviderEvent::ToolCallEnd { id, input: args }));
        }
    }

    fn absorb_usage(&mut self, usage: &Value) {
        if let Some(v) = usage.get("promptTokenCount").and_then(Value::as_u64) {
            self.usage.input = v;
        }
        // Chunks are cumulative but may omit either count, so each is
        // kept as an overwrite-only value; the output fold happens at
        // emit.
        if let Some(v) = usage.get("candidatesTokenCount").and_then(Value::as_u64) {
            self.output_candidates = v;
        }
        // Gemini 2.5+ reports thinking tokens separately and excludes
        // them from `candidatesTokenCount`; every other provider counts
        // them in the output total, so fold them in for consistent
        // cost and context accounting.
        if let Some(v) = usage.get("thoughtsTokenCount").and_then(Value::as_u64) {
            self.output_thoughts = v;
        }
    }

    fn emit_message_end(&mut self) {
        let usage = kage_core::TokenUsage {
            output: self.output_candidates.saturating_add(self.output_thoughts),
            ..self.usage
        };
        self.pending.push_back(Ok(ProviderEvent::MessageEnd {
            stop_reason: self.finish_reason,
            usage,
        }));
        self.done = true;
    }
}

impl crate::sse::SseStreamCore for GeminiStream {
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
        self.process_chunk(data);
    }
    fn on_eof(&mut self) {
        // Gemini's SSE ends without an explicit terminal frame, so a
        // turn that produced any output gets a synthesized MessageEnd
        // (the `started` guard mirrors the original EOF branch).
        if self.started {
            self.emit_message_end();
        }
    }
}

impl Iterator for GeminiStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        crate::sse::sse_next(self)
    }
}

fn parse_finish_reason(value: &str) -> StopReason {
    match value {
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        _ => StopReason::Other,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::testing::{collect_ok, user_msg};

    #[test]
    fn body_has_contents_and_generation_config() {
        let req = StreamRequest::new("gemini-2.0-flash", vec![Arc::new(user_msg("hi"))]);
        let body = build_request_body(&req);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "hi");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 4_096);
    }

    #[test]
    fn body_promotes_system_to_system_instruction() {
        let mut req = StreamRequest::new("m", vec![Arc::new(user_msg("hi"))]);
        req.system = Some("you are kage".into());
        let body = build_request_body(&req);
        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "you are kage"
        );
    }

    #[test]
    fn body_translates_thinking_level_to_budget() {
        let mut req = StreamRequest::new("gemini-2.5-pro", vec![Arc::new(user_msg("hi"))]);
        req.level = Some(crate::ThinkingLevel::High);
        let body = build_request_body(&req);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            crate::ThinkingLevel::High.default_budget_tokens().unwrap()
        );
    }

    #[test]
    fn body_omits_thinking_config_when_level_off() {
        let mut req = StreamRequest::new("gemini-2.5-pro", vec![Arc::new(user_msg("hi"))]);
        req.level = Some(crate::ThinkingLevel::Off);
        let body = build_request_body(&req);
        assert!(body["generationConfig"].get("thinkingConfig").is_none());
    }

    #[test]
    fn body_wraps_tools_in_function_declarations() {
        let mut req = StreamRequest::new("m", vec![Arc::new(user_msg("hi"))]);
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            schema: serde_json::json!({"type":"object"}),
        }];
        let body = build_request_body(&req);
        let decls = body["tools"][0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls[0]["name"], "read");
        assert_eq!(decls[0]["parameters"]["type"], "object");
    }

    /// `edit`'s derived schema is the shape that used to go over the
    /// wire raw: `$schema`, `$defs` + `$ref`, a `type` array from the
    /// `Option<Vec<..>>` field, and a `uint32` format. Gemini's
    /// `OpenAPI` subset rejects those.
    #[test]
    fn tool_schema_is_mapped_to_the_openapi_subset() {
        let schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {
                "Range": {
                    "type": "object",
                    "properties": {
                        "start": {"format": "uint32", "minimum": 0, "type": "integer"}
                    }
                }
            },
            "properties": {
                "path": {"type": "string"},
                "changes": {
                    "type": ["array", "null"],
                    "items": {"$ref": "#/$defs/Range"}
                }
            },
            "required": ["path"],
            "type": "object"
        });
        let mut req = StreamRequest::new("m", vec![Arc::new(user_msg("hi"))]);
        req.tools = vec![ToolSpec {
            name: "edit".into(),
            description: "edit a file".into(),
            schema,
        }];
        let body = build_request_body(&req);
        let params = &body["tools"][0]["functionDeclarations"][0]["parameters"];
        let text = params.to_string();
        assert!(!text.contains("$schema"), "{text}");
        assert!(!text.contains("$ref"), "{text}");
        assert!(!text.contains("$defs"), "{text}");
        let changes = &params["properties"]["changes"];
        assert_eq!(changes["type"], "array");
        assert_eq!(changes["nullable"], true);
        let start = &changes["items"]["properties"]["start"];
        assert_eq!(start["format"], "int32");
        assert_eq!(params["properties"]["path"]["type"], "string");
    }

    /// `todo_list`'s derived schema carries a `oneOf` of `const`
    /// variants (how schemars 1 renders a unit enum). Gemini knows
    /// `oneOf` but not `const`; each variant becomes a one-element
    /// `enum`, the shape Gemini documents for fixed values.
    #[test]
    fn const_variants_become_one_element_enums() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "status": {
                                "oneOf": [
                                    {"type": "string", "const": "pending"},
                                    {"type": "string", "const": "in_progress"},
                                    {"type": "string", "const": "done"}
                                ]
                            }
                        }
                    }
                }
            }
        });
        let mut req = StreamRequest::new("m", vec![Arc::new(user_msg("hi"))]);
        req.tools = vec![ToolSpec {
            name: "todo_list".into(),
            description: "the list".into(),
            schema,
        }];
        let body = build_request_body(&req);
        let text = body.to_string();
        assert!(!text.contains("const"), "{text}");
        assert!(!text.contains("additionalProperties"), "{text}");
        let status = &body["tools"][0]["functionDeclarations"][0]["parameters"]["properties"]["todos"]
            ["items"]["properties"]["status"];
        assert_eq!(
            status["oneOf"],
            serde_json::json!([
                {"type": "string", "enum": ["pending"]},
                {"type": "string", "enum": ["in_progress"]},
                {"type": "string", "enum": ["done"]}
            ])
        );
    }

    /// Keys outside Gemini's `Schema` field set are dropped rather
    /// than sent, because Gemini rejects the whole request over one
    /// unknown name. A non-string `const` or `enum` goes too: Gemini's
    /// `enum` is an array of strings only.
    #[test]
    fn keys_gemini_does_not_know_are_dropped() {
        let schema = serde_json::json!({
            "$comment": "internal",
            "$id": "https://example.com/x",
            "type": "object",
            "additionalProperties": false,
            "allOf": [{"type": "object"}],
            "properties": {
                "table": {
                    "type": "object",
                    "additionalProperties": {"type": "string"}
                },
                "count": {"type": "integer", "const": 3, "exclusiveMinimum": 0},
                "mode": {"enum": [1, 2]}
            }
        });
        let mut req = StreamRequest::new("m", vec![Arc::new(user_msg("hi"))]);
        req.tools = vec![ToolSpec {
            name: "mcp".into(),
            description: "a third-party tool".into(),
            schema,
        }];
        let body = build_request_body(&req);
        let params = &body["tools"][0]["functionDeclarations"][0]["parameters"];
        let text = params.to_string();
        for unknown in [
            "$comment",
            "$id",
            "additionalProperties",
            "const",
            "exclusiveMinimum",
            "allOf",
        ] {
            assert!(!text.contains(unknown), "{unknown} survived: {text}");
        }
        assert_eq!(params["properties"]["count"]["type"], "integer");
        assert!(params["properties"]["count"].get("enum").is_none());
        assert!(params["properties"]["mode"].get("enum").is_none());
    }

    /// `default` and `example` carry values, not schemas; they pass
    /// through untouched instead of being converted.
    #[test]
    fn value_positions_pass_through_unconverted() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "flag": {
                    "type": "boolean",
                    "default": false,
                    "example": {"const": "kept as a value"}
                }
            }
        });
        let mut req = StreamRequest::new("m", vec![Arc::new(user_msg("hi"))]);
        req.tools = vec![ToolSpec {
            name: "probe".into(),
            description: "probe".into(),
            schema,
        }];
        let body = build_request_body(&req);
        let flag = &body["tools"][0]["functionDeclarations"][0]["parameters"]["properties"]["flag"];
        assert_eq!(flag["default"], false);
        assert_eq!(
            flag["example"],
            serde_json::json!({"const": "kept as a value"})
        );
    }

    #[test]
    fn assistant_with_tool_call_emits_function_call_part() {
        let assistant = Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId::new("ignored"),
                name: "read".into(),
                input: serde_json::json!({"path":"/x"}),
            }],
            None,
        );
        let req = StreamRequest::new("m", vec![Arc::new(user_msg("read")), Arc::new(assistant)]);
        let body = build_request_body(&req);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["name"], "read");
        assert_eq!(
            contents[1]["parts"][0]["functionCall"]["args"]["path"],
            "/x"
        );
    }

    #[test]
    fn thought_signature_goes_back_on_its_function_call() {
        let signed = |model: &str| {
            Some(kage_core::ThinkingSignature {
                model: model.into(),
                data: "sig".into(),
                redacted: false,
            })
        };
        let call = |id: &str| Content::ToolCall {
            id: ToolCallId::new(id),
            name: "read".into(),
            input: serde_json::json!({}),
        };
        let assistant = Message::new(
            Role::Assistant,
            vec![
                Content::Thinking {
                    text: String::new(),
                    signature: signed("gemini-3"),
                    duration_ms: None,
                },
                call("a"),
                call("b"),
                Content::Thinking {
                    text: "other model".into(),
                    signature: signed("claude-x"),
                    duration_ms: None,
                },
                call("c"),
            ],
            None,
        );
        let req = StreamRequest::new(
            "gemini-3",
            vec![Arc::new(user_msg("read")), Arc::new(assistant)],
        );
        let body = build_request_body(&req);
        let parts = body["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0]["thoughtSignature"], "sig");
        assert_eq!(parts[0]["functionCall"]["name"], "read");
        assert!(parts[1].get("thoughtSignature").is_none());
        assert_eq!(parts[2]["text"], "<thinking>\nother model\n</thinking>");
        assert!(parts[3].get("thoughtSignature").is_none());
    }

    fn unsigned_loop() -> Vec<std::sync::Arc<Message>> {
        let call = |id: &str| Content::ToolCall {
            id: ToolCallId::new(id),
            name: "read".into(),
            input: serde_json::json!({}),
        };
        let result = |id: &str| {
            Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: ToolCallId::new(id),
                    output: "ok".into(),
                    is_error: false,
                }],
                None,
            )
        };
        let assistant = |content| Message::new(Role::Assistant, content, None);
        vec![
            user_msg("earlier"),
            assistant(vec![call("x")]),
            result("x"),
            assistant(vec![Content::Text {
                text: "done".into(),
            }]),
            user_msg("now"),
            assistant(vec![call("a"), call("b")]),
            result("a"),
            result("b"),
            assistant(vec![
                Content::Thinking {
                    text: "switched".into(),
                    signature: Some(kage_core::ThinkingSignature {
                        model: "claude-x".into(),
                        data: "foreign".into(),
                        redacted: false,
                    }),
                    duration_ms: None,
                },
                call("c"),
            ]),
            result("c"),
            assistant(vec![Content::Text {
                text: "all read".into(),
            }]),
        ]
        .into_iter()
        .map(Arc::new)
        .collect()
    }

    #[test]
    fn placeholder_signs_first_call_of_each_current_step() {
        let req = StreamRequest::new("gemini-3-pro-preview", unsigned_loop());
        let body = build_request_body(&req);
        let contents = body["contents"].as_array().unwrap();
        let sig =
            |content: usize, part: usize| contents[content]["parts"][part].get("thoughtSignature");
        assert!(sig(1, 0).is_none());
        assert!(sig(3, 0).is_none());
        assert_eq!(sig(5, 0).unwrap(), SKIP_SIGNATURE);
        assert!(sig(5, 1).is_none());
        assert_eq!(
            contents[8]["parts"][0]["text"],
            "<thinking>\nswitched\n</thinking>"
        );
        assert_eq!(sig(8, 1).unwrap(), SKIP_SIGNATURE);
        assert!(sig(10, 0).is_none());
    }

    #[test]
    fn placeholder_is_left_out_before_gemini_3() {
        let req = StreamRequest::new("gemini-2.5-pro", unsigned_loop());
        let body = build_request_body(&req);
        let signed = body["contents"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["parts"].as_array().unwrap())
            .any(|p| p.get("thoughtSignature").is_some());
        assert!(!signed);
    }

    #[test]
    fn signature_enforcement_starts_at_gemini_3() {
        for model in [
            "gemini-3-pro",
            "gemini-3-flash-preview",
            "gemini-3.1-pro-preview",
            "models/gemini-3-pro",
            "google/gemini-3-pro",
            "google/gemini-3.5-flash",
            "my-gemini-3",
            "gemini_3_flash",
            "gemini-10-pro",
        ] {
            assert!(enforces_signatures(model), "{model}");
        }
        for model in [
            "gemini-2.5-pro",
            "gemini-exp-1206",
            "gemini-",
            "gemini_2_flash",
            "claude-x",
            "m",
        ] {
            assert!(!enforces_signatures(model), "{model}");
        }
    }

    #[test]
    fn stream_captures_thought_signature_before_its_call() {
        let bytes: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{}},\"thoughtSignature\":\"sig\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        assert!(matches!(
            &events[1],
            ProviderEvent::ThinkingSignature { data } if data == "sig"
        ));
        assert!(matches!(&events[2], ProviderEvent::ToolCallStart { .. }));
    }

    #[test]
    fn tool_result_emits_function_response() {
        let result = Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new("read"),
                output: "127.0.0.1".into(),
                is_error: false,
            }],
            None,
        );
        let req = StreamRequest::new("m", vec![Arc::new(result)]);
        let body = build_request_body(&req);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "user");
        let fr = &contents[0]["parts"][0]["functionResponse"];
        assert_eq!(fr["name"], "read");
        assert_eq!(fr["response"]["output"], "127.0.0.1");
    }

    fn stream_from_bytes(bytes: &'static [u8]) -> GeminiStream {
        GeminiStream::new(Box::new(std::io::Cursor::new(bytes)), CancelFlag::new())
    }

    #[test]
    fn stream_error_chunk_resource_exhausted_classifies_as_rate_limited() {
        let bytes: &[u8] = b"data: {\"error\":{\"code\":429,\"message\":\"Resource has been exhausted\",\"status\":\"RESOURCE_EXHAUSTED\"}}\n\n";
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
        assert!(events.next().is_none(), "stream ends after the error chunk");
    }

    #[test]
    fn stream_error_chunk_unavailable_is_transient() {
        let bytes: &[u8] = b"data: {\"error\":{\"code\":503,\"message\":\"The service is currently unavailable\",\"status\":\"UNAVAILABLE\"}}\n\n";
        let mut events = stream_from_bytes(bytes);
        let first = events.next().unwrap();
        match first {
            Err(err) => assert!(err.is_transient(), "503 should retry: {err:?}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn stream_error_chunk_internal_is_transient() {
        let bytes: &[u8] = b"data: {\"error\":{\"code\":500,\"message\":\"Internal error\",\"status\":\"INTERNAL\"}}\n\n";
        let mut events = stream_from_bytes(bytes);
        let first = events.next().unwrap();
        match first {
            Err(err) => assert!(err.is_transient(), "INTERNAL should retry: {err:?}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn stream_error_chunk_deadline_exceeded_is_transient() {
        let bytes: &[u8] = b"data: {\"error\":{\"code\":504,\"message\":\"Request deadline exceeded\",\"status\":\"DEADLINE_EXCEEDED\"}}\n\n";
        let mut events = stream_from_bytes(bytes);
        let first = events.next().unwrap();
        match first {
            Err(err) => assert!(
                err.is_transient(),
                "DEADLINE_EXCEEDED should retry: {err:?}"
            ),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn stream_emits_text_deltas() {
        let bytes: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello\"}],\"role\":\"model\"},\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":1}}\n\ndata: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" world\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2}}\n\n";
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
            assert_eq!(usage.input, 5);
            assert_eq!(usage.output, 2);
        } else {
            panic!("expected MessageEnd");
        }
    }

    #[test]
    fn stream_emits_the_images_a_model_answers_with() {
        let bytes: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a cat\"},{\"inlineData\":{\"mimeType\":\"image/png\",\"data\":\"draft\"},\"thought\":true},{\"inlineData\":{\"mimeType\":\"image/png\",\"data\":\"iVBORw0K\"}}],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let media: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::Media { kind, mime, data } => {
                    Some((*kind, mime.as_str(), data.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            media,
            [(kage_core::Modality::Image, "image/png", "iVBORw0K")],
            "the image drawn while thinking is left out"
        );
    }

    #[test]
    fn an_image_keeps_its_signature_and_goes_back_to_a_model_that_draws() {
        let bytes: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"inlineData\":{\"mimeType\":\"image/png\",\"data\":\"iVBO\"},\"thoughtSignature\":\"isig\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let at = |want: fn(&ProviderEvent) -> bool| events.iter().position(want).unwrap();
        assert!(
            at(|e| matches!(e, ProviderEvent::ThinkingSignature { data } if data == "isig"))
                < at(|e| matches!(e, ProviderEvent::Media { .. })),
            "{events:?}"
        );

        let reply = vec![
            Content::Thinking {
                text: String::new(),
                signature: Some(kage_core::ThinkingSignature {
                    model: "draw".into(),
                    data: "isig".into(),
                    redacted: false,
                }),
                duration_ms: None,
            },
            Content::Image {
                source: kage_core::MediaSource::Base64 {
                    data: "iVBO".into(),
                },
                mime: "image/png".into(),
            },
            Content::ToolCall {
                id: kage_core::ToolCallId::new("c1"),
                name: "read".into(),
                input: serde_json::json!({}),
            },
        ];
        let drawn = convert_assistant_parts(&reply, "draw", false, true);
        assert_eq!(drawn[0]["inlineData"]["data"], "iVBO");
        assert_eq!(drawn[0]["thoughtSignature"], "isig");
        assert!(drawn[1].get("thoughtSignature").is_none());

        let noted = convert_assistant_parts(&reply, "draw", false, false);
        assert_eq!(noted[0]["text"], "[you produced an image here: image/png]");
        assert!(
            noted[1].get("thoughtSignature").is_none(),
            "the image's signature does not move onto the call: {noted:?}"
        );
    }

    #[test]
    fn an_image_only_reply_replays_as_a_note() {
        let msg = |role, content| std::sync::Arc::new(kage_core::Message::new(role, content, None));
        let text = |t: &str| kage_core::Content::Text { text: t.into() };
        let req = StreamRequest::new(
            "draw",
            vec![
                msg(kage_core::Role::User, vec![text("a cat")]),
                msg(
                    kage_core::Role::Assistant,
                    vec![kage_core::Content::Image {
                        source: kage_core::MediaSource::Base64 {
                            data: "iVBO".into(),
                        },
                        mime: "image/png".into(),
                    }],
                ),
                msg(kage_core::Role::User, vec![text("make it blue")]),
            ],
        );
        let body = build_request_body(&req);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3, "the reply is not dropped: {contents:?}");
        assert_eq!(
            contents[1]["parts"][0]["text"],
            "[you produced an image here: image/png]"
        );
    }

    #[test]
    fn body_asks_for_images_only_from_a_model_that_draws() {
        let user = kage_core::Message::new(
            kage_core::Role::User,
            vec![kage_core::Content::Text {
                text: "a cat".into(),
            }],
            None,
        );
        let mut req = StreamRequest::new("draw", vec![std::sync::Arc::new(user)]);
        assert!(
            build_request_body(&req)["generationConfig"]
                .get("responseModalities")
                .is_none()
        );
        req.produces =
            kage_core::Modalities::of(&[kage_core::Modality::Text, kage_core::Modality::Image]);
        assert_eq!(
            build_request_body(&req)["generationConfig"]["responseModalities"],
            serde_json::json!(["TEXT", "IMAGE"])
        );
    }

    #[test]
    fn usage_folds_thinking_tokens_into_output() {
        // Gemini 2.5+ excludes thinking from `candidatesTokenCount`;
        // every other provider counts thinking in the output total.
        let bytes: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2,\"thoughtsTokenCount\":4}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        if let Some(ProviderEvent::MessageEnd { usage, .. }) = events.last() {
            assert_eq!(usage.input, 5);
            assert_eq!(usage.output, 6);
        } else {
            panic!("expected MessageEnd");
        }
    }

    #[test]
    fn stream_emits_function_call_as_start_and_end() {
        let bytes: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"/tmp\"}}}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":3}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let start = events
            .iter()
            .find(|e| matches!(e, ProviderEvent::ToolCallStart { .. }))
            .expect("ToolCallStart present");
        if let ProviderEvent::ToolCallStart { name, .. } = start {
            assert_eq!(name, "read");
        }
        let end = events
            .iter()
            .find(|e| matches!(e, ProviderEvent::ToolCallEnd { .. }))
            .expect("ToolCallEnd present");
        if let ProviderEvent::ToolCallEnd { input, .. } = end {
            assert_eq!(input["path"], "/tmp");
        }
    }

    #[test]
    fn parallel_same_name_function_calls_get_distinct_ids() {
        let bytes: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"/a\"}}},{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"/b\"}}}],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ToolCallStart { id, .. } => Some(id.0.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2, "both calls must surface");
        assert_ne!(ids[0], ids[1], "same-name parallel calls need unique ids");
    }

    #[test]
    fn tool_result_response_names_the_function_from_its_call() {
        let assistant = Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId::new("gemini_read_1"),
                name: "read".into(),
                input: serde_json::json!({"path": "/a"}),
            }],
            None,
        );
        let result = Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new("gemini_read_1"),
                output: "ok".into(),
                is_error: false,
            }],
            None,
        );
        let req = StreamRequest::new(
            "m",
            vec![
                Arc::new(user_msg("go")),
                Arc::new(assistant),
                Arc::new(result),
            ],
        );
        let body = build_request_body(&req);
        let contents = body["contents"].as_array().unwrap();
        let fr = &contents[2]["parts"][0]["functionResponse"];
        assert_eq!(fr["name"], "read");
    }

    #[test]
    fn tool_name_from_call_id_parses_new_and_legacy_formats() {
        let none: Vec<ToolSpec> = Vec::new();
        assert_eq!(
            tool_name_from_call_id(&ToolCallId::new("gemini_read_1"), &none),
            "read"
        );
        assert_eq!(
            tool_name_from_call_id(&ToolCallId::new("gemini_my_tool_12"), &none),
            "my_tool"
        );
        assert_eq!(
            tool_name_from_call_id(&ToolCallId::new("gemini_read"), &none),
            "read"
        );
        assert_eq!(
            tool_name_from_call_id(&ToolCallId::new("gemini_my_tool"), &none),
            "my_tool"
        );
        assert_eq!(
            tool_name_from_call_id(&ToolCallId::new("call_abc"), &none),
            "call_abc"
        );
    }

    /// A legacy counter-less id for a tool whose name itself ends in
    /// `_<digits>` must match the declared name before the counter
    /// stripping would cut it short.
    #[test]
    fn legacy_ids_try_declared_names_before_counter_stripping() {
        let declared = |name: &str| {
            vec![ToolSpec {
                name: name.into(),
                description: String::new(),
                schema: serde_json::json!({}),
            }]
        };
        let id = ToolCallId::new("gemini_tool_2");
        assert_eq!(tool_name_from_call_id(&id, &declared("tool_2")), "tool_2");
        assert_eq!(tool_name_from_call_id(&id, &declared("tool")), "tool");
        assert_eq!(tool_name_from_call_id(&id, &[]), "tool");
        let id = ToolCallId::new("gemini_tool2_1");
        assert_eq!(tool_name_from_call_id(&id, &declared("tool2")), "tool2");
    }

    /// Gemini's SSE may interleave empty-data keepalive lines with
    /// content chunks; they must not surface as decode failures.
    #[test]
    fn empty_data_keepalives_do_not_break_the_stream() {
        let bytes: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}],\"role\":\"model\"}}]}\n\ndata:\n\ndata: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ProviderEvent::TextDelta { delta } if delta == "hi"))
        );
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::MessageEnd { .. })
        ));
    }

    /// A later chunk may omit either usage count; the output fold must
    /// use the last value of each, not double-count what a chunk left
    /// out.
    #[test]
    fn incomplete_usage_chunks_fold_last_candidates_plus_last_thoughts() {
        let bytes: &[u8] = b"data: {\"candidates\":[],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":4,\"thoughtsTokenCount\":3}}\n\ndata: {\"candidates\":[],\"usageMetadata\":{\"promptTokenCount\":5,\"thoughtsTokenCount\":2}}\n\ndata: {\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":7}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        if let Some(ProviderEvent::MessageEnd { usage, .. }) = events.last() {
            assert_eq!(usage.input, 5);
            assert_eq!(usage.output, 9);
        } else {
            panic!("expected MessageEnd");
        }
    }

    #[test]
    fn stream_yields_cancelled_when_flag_set() {
        let bytes: &[u8] = b"data: {\"candidates\":[]}\n\n";
        let cancel = CancelFlag::new();
        cancel.cancel();
        let mut s = GeminiStream::new(Box::new(std::io::Cursor::new(bytes)), cancel);
        assert!(matches!(s.next(), Some(Err(ProviderError::Cancelled))));
        assert!(s.next().is_none());
    }

    #[test]
    fn request_headers_include_auth_then_extras_in_key_order() {
        let mut extras = BTreeMap::new();
        extras.insert("X-B".to_owned(), "2".to_owned());
        extras.insert("X-A".to_owned(), "1".to_owned());
        let provider = GeminiProvider::new("k").with_extra_headers(extras);
        assert_eq!(
            provider.request_headers(),
            vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("x-goog-api-key".to_owned(), "k".to_owned()),
                ("X-A".to_owned(), "1".to_owned()),
                ("X-B".to_owned(), "2".to_owned()),
            ]
        );
    }

    #[test]
    fn request_headers_skip_auth_when_key_is_empty() {
        let mut extras = BTreeMap::new();
        extras.insert("X-A".to_owned(), "1".to_owned());
        let provider = GeminiProvider::new("").with_extra_headers(extras);
        let headers = provider.request_headers();
        assert!(
            headers.iter().all(|(name, _)| name != "x-goog-api-key"),
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
        let provider = GeminiProvider::new("k").with_models(models.clone());
        assert_eq!(provider.models(), models);
        assert!(GeminiProvider::new("k").models().is_empty());
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

    fn config(reasoning: Reasoning, level: ThinkingLevel) -> Value {
        build_request_body(&request(reasoning, level))["generationConfig"]["thinkingConfig"].clone()
    }

    #[test]
    fn effort_models_get_a_thinking_level() {
        let r = effort(&[Effort::Minimal, Effort::Low, Effort::High], false);
        assert_eq!(
            config(r, ThinkingLevel::High),
            serde_json::json!({"thinkingLevel": "high"})
        );
    }

    #[test]
    fn budget_models_stay_within_bounds_and_toggle_off_to_zero() {
        let r = Reasoning::Budget {
            min: 0,
            max: Some(24_576),
            toggle: true,
        };
        assert_eq!(
            config(r, ThinkingLevel::XHigh),
            serde_json::json!({"thinkingBudget": 24_576})
        );
        assert_eq!(
            config(r, ThinkingLevel::Off),
            serde_json::json!({"thinkingBudget": 0})
        );
        assert_eq!(
            config(Reasoning::Toggle, ThinkingLevel::High),
            serde_json::json!({"thinkingBudget": -1})
        );
        assert!(config(Reasoning::None, ThinkingLevel::High).is_null());
    }
}
