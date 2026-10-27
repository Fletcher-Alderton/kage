//! Streaming request shape passed to [`Provider::stream`](crate::Provider::stream).

use std::sync::Arc;

pub use kage_core::ThinkingLevel;
use kage_core::{Message, Modalities, Reasoning, ToolSpec};
use serde::{Deserialize, Serialize};

/// Optional thinking-budget configuration.
///
/// Carries an explicit per-turn budget in thinking tokens. Providers
/// that take a numeric budget (Anthropic, Gemini) forward it directly;
/// providers that take an effort string (`OpenAI` Chat Completions and
/// Responses) ignore it and read [`StreamRequest::level`] instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ThinkingConfig {
    /// Soft budget on thinking tokens emitted by the model.
    pub budget_tokens: u32,
}

/// A single streaming request to a provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamRequest {
    /// Model id, the part after `provider:` in the registry resolver.
    pub model: String,
    /// Conversation history, ending with the most recent user turn.
    /// Shared with the caller's history buffer, so cloning a request
    /// bumps refcounts instead of copying message bodies.
    pub messages: Vec<Arc<Message>>,
    /// System prompt, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Tools available to the model this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    /// Maximum number of output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Optional thinking budget. Budget-based providers send it in
    /// place of [`Self::level`]; this raw budget exists for callers
    /// that need to bypass the catalog mapping (Lua hooks, tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// Unified thinking effort for this turn, already fitted to the
    /// model. Providers translate it to their native shape (Anthropic
    /// adaptive effort or budget tokens, `OpenAI` `reasoning_effort`,
    /// Gemini `thinkingConfig`). [`Self::thinking`] takes precedence
    /// when both are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<ThinkingLevel>,
    /// Thinking settings the model accepts, which pick the request
    /// shape for [`Self::level`]. [`Reasoning::Unknown`] keeps each
    /// provider's generic mapping.
    #[serde(default, skip_serializing_if = "is_unknown")]
    pub reasoning: Reasoning,
    /// Media the model produces besides text. A provider asks for image
    /// output only when this holds [`kage_core::Modality::Image`].
    /// Empty means text only.
    #[serde(default, skip_serializing_if = "Modalities::is_empty")]
    pub produces: Modalities,
    /// Media the model reads besides text. A provider sends the user's
    /// audio as audio only when this holds [`kage_core::Modality::Audio`],
    /// and as a short note otherwise. Empty means not known.
    #[serde(default, skip_serializing_if = "Modalities::is_empty")]
    pub accepts: Modalities,
    /// Id of the conversation the request belongs to, for gateways
    /// that route or cache per conversation. `None` for one-off
    /// requests outside a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

fn is_unknown(reasoning: &Reasoning) -> bool {
    *reasoning == Reasoning::Unknown
}

impl StreamRequest {
    /// Construct a minimal request with just a model id and message history.
    pub fn new(model: impl Into<String>, messages: Vec<Arc<Message>>) -> Self {
        Self {
            model: model.into(),
            messages,
            system: None,
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
            thinking: None,
            level: None,
            reasoning: Reasoning::Unknown,
            produces: Modalities::default(),
            accepts: Modalities::default(),
            session: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kage_core::Role;

    #[test]
    fn new_omits_optional_fields() {
        let req = StreamRequest::new("anthropic:claude-sonnet-4-6", vec![]);
        assert_eq!(req.model, "anthropic:claude-sonnet-4-6");
        assert!(req.messages.is_empty());
        assert!(req.system.is_none());
        assert!(req.tools.is_empty());
        assert!(req.max_output_tokens.is_none());
        assert!(req.temperature.is_none());
        assert!(req.thinking.is_none());
        assert!(req.level.is_none());
    }

    #[test]
    fn empty_optional_fields_omitted_from_json() {
        let req = StreamRequest::new("m", vec![]);
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("system").is_none());
        assert!(json.get("tools").is_none());
        assert!(json.get("max_output_tokens").is_none());
        assert!(json.get("temperature").is_none());
        assert!(json.get("thinking").is_none());
        assert!(json.get("level").is_none());
    }

    #[test]
    fn populated_request_roundtrips() {
        let mut req = StreamRequest::new(
            "openai:gpt-4o",
            vec![Arc::new(Message::new(Role::User, vec![], None))],
        );
        req.system = Some("you are helpful".into());
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            schema: serde_json::json!({"type":"object"}),
        }];
        req.max_output_tokens = Some(2_000);
        req.temperature = Some(0.7);
        req.thinking = Some(ThinkingConfig {
            budget_tokens: 5_000,
        });
        req.level = Some(ThinkingLevel::Medium);

        let s = serde_json::to_string(&req).unwrap();
        let back: StreamRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);
    }
}
