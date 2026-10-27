//! Integration tests: every public type with serde derives must round-trip
//! losslessly through JSON.

use std::collections::BTreeMap;

use kage_core::{
    Config, Content, LoopError, LoopEvent, MediaSource, Message, MessageId, Risk, Role, StopReason,
    TokenUsage, ToolCallId, ToolOutput,
};

fn roundtrip<T>(value: &T)
where
    T: serde::Serialize + for<'de> serde::Deserialize<'de> + PartialEq + std::fmt::Debug,
{
    let s = serde_json::to_string(value).expect("serialize");
    let back: T = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(value, &back);
}

#[test]
fn message_roundtrips() {
    let msg = Message::new(Role::User, vec![Content::Text { text: "hi".into() }], None);
    roundtrip(&msg);
}

#[test]
fn all_content_variants_roundtrip() {
    let blocks = [
        Content::Text { text: "x".into() },
        Content::Thinking {
            text: "y".into(),
            signature: None,
            duration_ms: Some(1_500),
        },
        Content::Image {
            source: MediaSource::Url {
                url: "https://example.com/x.png".into(),
            },
            mime: "image/png".into(),
        },
        Content::Image {
            source: MediaSource::Base64 {
                data: "AAAA".into(),
            },
            mime: "image/jpeg".into(),
        },
        Content::ToolCall {
            id: ToolCallId::new("call_1"),
            name: "read".into(),
            input: serde_json::json!({"path": "/etc/hosts"}),
        },
        Content::ToolResultBlock {
            call_id: ToolCallId::new("call_1"),
            output: "127.0.0.1".into(),
            is_error: false,
        },
        Content::Custom {
            kind: "plugin:tps".into(),
            data: serde_json::json!({"tps": 42}),
        },
    ];
    for c in blocks {
        roundtrip(&c);
    }
}

#[test]
fn audio_blocks_roundtrip_with_base64_and_blob_sources() {
    let blocks = [
        Content::Audio {
            source: MediaSource::Base64 {
                data: "AAAA".into(),
            },
            mime: "audio/mpeg".into(),
        },
        Content::Audio {
            source: MediaSource::Blob {
                sha256: "a".repeat(64),
                size: 12,
            },
            mime: "audio/wav".into(),
        },
    ];
    for c in &blocks {
        roundtrip(c);
    }
    let json = serde_json::to_value(&blocks[0]).unwrap();
    assert_eq!(json["type"], "audio");
}

#[test]
fn all_roles_roundtrip() {
    for r in [Role::User, Role::Assistant, Role::ToolResult, Role::System] {
        roundtrip(&r);
    }
}

#[test]
fn all_loop_events_roundtrip() {
    let mid = MessageId::new();
    let cid = ToolCallId::new("c");
    let events = [
        LoopEvent::MessageStart { id: mid },
        LoopEvent::TextDelta {
            id: mid,
            delta: "x".into(),
        },
        LoopEvent::ThinkingDelta {
            id: mid,
            delta: "y".into(),
        },
        LoopEvent::ToolCallStart {
            id: cid.clone(),
            name: "n".into(),
            input_partial: serde_json::Value::Null,
        },
        LoopEvent::ToolCallArgsDelta {
            id: cid.clone(),
            name: "n".into(),
            input_partial: serde_json::json!({"path": "."}),
        },
        LoopEvent::ToolCallEnd {
            id: cid.clone(),
            output: ToolOutput {
                is_error: false,
                text: "ok".into(),
                structured: None,
                terminate: false,
                media: Vec::new(),
            },
        },
        LoopEvent::MessageEnd {
            id: mid,
            usage: TokenUsage::default(),
            stop_reason: StopReason::EndTurn,
        },
        LoopEvent::Compaction {
            kept: 5,
            summarized: 30,
            summary: "[summary of 30 earlier turns]\nfoo".into(),
            before: 150_000,
            estimate: 0,
        },
        LoopEvent::Error {
            kind: LoopError::Cancelled,
        },
    ];
    for ev in events {
        roundtrip(&ev);
    }
}

#[test]
fn all_loop_errors_roundtrip() {
    let errors = [
        LoopError::Provider {
            message: "x".into(),
        },
        LoopError::Tool {
            name: "shell".into(),
            message: "y".into(),
        },
        LoopError::Cancelled,
        LoopError::ContextOverflow,
        LoopError::InvalidPrompt {
            message: "w".into(),
        },
        LoopError::Other {
            message: "z".into(),
        },
    ];
    for err in errors {
        roundtrip(&err);
    }
}

#[test]
fn all_risks_roundtrip() {
    for r in [Risk::Read, Risk::Write, Risk::Exec, Risk::Network] {
        roundtrip(&r);
    }
}

#[test]
fn config_default_roundtrips() {
    roundtrip(&Config::default());
}

/// Non-default values across the nested tables must survive a JSON
/// round trip unchanged, so a config rewrite never silently resets
/// settings the way a lost field would.
#[test]
fn config_non_default_values_roundtrip() {
    let mut config = Config::default();
    config.provider.default_model = "zai/glm-5.3-flash".to_owned();
    config.loop_settings.compaction_threshold = 0.75;
    config.permissions.confine_paths = false;
    config.permissions.tools.insert(
        "shell".to_owned(),
        kage_core::permissions::ToolPermissionRules {
            default: kage_core::permissions::PermissionAction::Ask,
            allow: vec!["git *".to_owned()],
            deny: vec!["git push *".to_owned()],
        },
    );
    config.mcp.servers.insert(
        "docs".to_owned(),
        kage_core::config::McpServer {
            startup_timeout_secs: Some(5),
            command: Some("docs-server".to_owned()),
            args: vec!["--stdio".to_owned()],
            env: BTreeMap::from([("DOCS_TOKEN".to_owned(), "x".to_owned())]),
            url: None,
            headers: BTreeMap::new(),
            disabled: false,
            oauth: None,
            disabled_tools: vec!["slow_tool".to_owned()],
        },
    );
    roundtrip(&config);
}

#[test]
fn token_usage_with_cache_roundtrips() {
    roundtrip(&TokenUsage {
        input: 1_000,
        output: 500,
        cache_read: 800,
        cache_write: 200,
    });
}

#[test]
fn tool_output_with_structured_payload_roundtrips() {
    roundtrip(&ToolOutput {
        is_error: true,
        text: "out of bounds".into(),
        structured: Some(serde_json::json!({"line": 42, "kind": "syntax"})),
        terminate: false,
        media: Vec::new(),
    });
}

#[test]
fn message_tolerates_unknown_fields() {
    let real = Message::new(Role::User, vec![], None);
    let mut json = serde_json::to_value(&real).expect("encode");
    let obj = json.as_object_mut().expect("object");
    obj.insert("future_field".into(), serde_json::json!("ignored"));
    obj.insert("another_extra".into(), serde_json::json!(42));
    let parsed: Message = serde_json::from_value(json).expect("lenient parse");
    assert_eq!(parsed.role, Role::User);
    assert_eq!(parsed.id, real.id);
}

#[test]
fn message_id_serializes_as_ulid_string() {
    let mid = MessageId::new();
    let s = serde_json::to_string(&mid).expect("encode");
    assert!(s.starts_with('"') && s.ends_with('"'));
    assert_eq!(s.len(), 28);
    assert_eq!(s, format!("\"{mid}\""), "the string form is the bare ULID");
    let back: MessageId = serde_json::from_str(&s).expect("decode");
    assert_eq!(back, mid, "the string decodes to the same id");
}
