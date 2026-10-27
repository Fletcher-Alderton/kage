//! Tests for the Anthropic provider and SSE stream.

use std::sync::Arc;

use super::*;
use crate::testing::{collect_ok, user_msg};
use kage_core::{Content, Message, Role, ThinkingSignature};

fn assistant_tool_call(id: &str, name: &str) -> Message {
    Message::new(
        Role::Assistant,
        vec![Content::ToolCall {
            id: ToolCallId::new(id),
            name: name.to_owned(),
            input: serde_json::json!({}),
        }],
        None,
    )
}

fn tool_result(id: &str, output: &str) -> Message {
    Message::new(
        Role::ToolResult,
        vec![Content::ToolResultBlock {
            call_id: ToolCallId::new(id),
            output: output.to_owned(),
            is_error: false,
        }],
        None,
    )
}

fn signed(model: &str, data: &str, redacted: bool) -> ThinkingSignature {
    ThinkingSignature {
        model: model.to_owned(),
        data: data.to_owned(),
        redacted,
    }
}

fn thinking_tool_call(text: &str, signature: ThinkingSignature) -> Message {
    Message::new(
        Role::Assistant,
        vec![
            Content::Thinking {
                text: text.to_owned(),
                signature: Some(signature),
                duration_ms: Some(2_000),
            },
            Content::ToolCall {
                id: ToolCallId::new("call_1"),
                name: "shell".to_owned(),
                input: serde_json::json!({}),
            },
        ],
        None,
    )
}

fn continuation(assistant: Message) -> StreamRequest {
    let mut req = StreamRequest::new(
        "claude-x",
        vec![
            Arc::new(user_msg("run it")),
            Arc::new(assistant),
            Arc::new(tool_result("call_1", "file.rs")),
        ],
    );
    req.thinking = Some(crate::ThinkingConfig {
        budget_tokens: 12_000,
    });
    req
}

#[test]
fn continuation_keeps_signed_thinking_first_and_thinking_on() {
    let req = continuation(thinking_tool_call(
        "list files",
        signed("claude-x", "sig", false),
    ));
    let body = build_request_body(&req, true);
    assert_eq!(body["thinking"]["type"], "enabled");
    let blocks = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        blocks[0],
        serde_json::json!({"type": "thinking", "thinking": "list files", "signature": "sig"})
    );
    assert_eq!(blocks[1]["type"], "tool_use");
}

#[test]
fn thinking_duration_is_never_sent() {
    for model in ["claude-x", "gemini-3"] {
        let req = continuation(thinking_tool_call("plan", signed(model, "sig", false)));
        let body = build_request_body(&req, true).to_string();
        assert!(!body.contains("duration"), "{body}");
    }
}

#[test]
fn redacted_thinking_goes_back_as_redacted_block() {
    let req = continuation(thinking_tool_call("", signed("claude-x", "enc", true)));
    let body = build_request_body(&req, true);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(
        body["messages"][1]["content"][0],
        serde_json::json!({"type": "redacted_thinking", "data": "enc"})
    );
}

#[test]
fn unsigned_thinking_is_dropped_for_claude_and_drops_budget_thinking() {
    let req = continuation(thinking_tool_call("plan", signed("gemini-3", "sig", false)));
    let body = build_request_body(&req, true);
    assert!(body.get("thinking").is_none());
    let blocks = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "tool_use");
}

#[test]
fn unsigned_thinking_goes_back_native_for_an_anthropic_compatible_model() {
    let mut req = continuation(thinking_tool_call("plan", signed("gemini-3", "sig", false)));
    req.model = "deepseek-v4-1-flash".to_owned();
    let body = build_request_body(&req, true);
    let blocks = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        blocks[0],
        serde_json::json!({"type": "thinking", "thinking": "plan"})
    );
    assert!(blocks[0].get("signature").is_none());
    assert_eq!(blocks[1]["type"], "tool_use");
}

#[test]
fn empty_unsigned_thinking_is_preserved_for_an_anthropic_compatible_model() {
    let mut req = continuation(thinking_tool_call("", signed("gemini-3", "sig", false)));
    req.model = "deepseek-v4-1-flash".to_owned();
    let body = build_request_body(&req, true);
    let blocks = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        blocks[0],
        serde_json::json!({"type": "thinking", "thinking": ""})
    );
}

#[test]
fn claude_models_are_recognized_by_name() {
    for model in [
        "claude-x",
        "claude-sonnet-5-5",
        "sonnet-5-5",
        "5-5-sonnet",
        "opus-4-6",
        "claude-mythos-preview",
    ] {
        assert!(is_claude_model(model), "{model}");
    }
    for model in ["deepseek-v4-1-flash", "glm-5.3", "kimi-k3", "gpt-6-luna"] {
        assert!(!is_claude_model(model), "{model}");
    }
}

#[test]
fn earlier_turns_keep_their_signed_thinking() {
    let mut req = StreamRequest::new(
        "claude-x",
        vec![
            Arc::new(user_msg("run it")),
            Arc::new(thinking_tool_call("first", signed("claude-x", "s1", false))),
            Arc::new(tool_result("call_1", "ok")),
            Arc::new(Message::new(
                Role::Assistant,
                vec![Content::Text {
                    text: "done".into(),
                }],
                None,
            )),
            Arc::new(user_msg("next")),
        ],
    );
    req.thinking = Some(crate::ThinkingConfig {
        budget_tokens: 12_000,
    });
    let body = build_request_body(&req, false);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["messages"][1]["content"][0]["signature"], "s1");
}

#[test]
fn add_beta_joins_a_configured_beta_header() {
    let mut headers = vec![("Anthropic-Beta".to_owned(), "other".to_owned())];
    add_beta(&mut headers, INTERLEAVED_THINKING_BETA);
    assert_eq!(
        headers,
        vec![(
            "Anthropic-Beta".to_owned(),
            format!("other,{INTERLEAVED_THINKING_BETA}")
        )]
    );
    let mut headers = Vec::new();
    add_beta(&mut headers, INTERLEAVED_THINKING_BETA);
    assert_eq!(
        headers,
        vec![(
            "anthropic-beta".to_owned(),
            INTERLEAVED_THINKING_BETA.to_owned()
        )]
    );
}

#[test]
fn body_sets_model_and_messages() {
    let req = StreamRequest::new("claude-sonnet-4-6", vec![Arc::new(user_msg("hi"))]);
    let body = build_request_body(&req, false);
    assert_eq!(body["model"], "claude-sonnet-4-6");
    assert_eq!(body["stream"], false);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    let blocks = messages[0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "hi");
}

#[test]
fn body_promotes_system_to_cached_array() {
    let mut req = StreamRequest::new("m", vec![Arc::new(user_msg("hi"))]);
    req.system = Some("you are kage".into());
    let body = build_request_body(&req, false);
    let system = body["system"].as_array().expect("system is array");
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["type"], "text");
    assert_eq!(system[0]["text"], "you are kage");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn body_marks_last_message_block_for_caching() {
    let req = StreamRequest::new(
        "m",
        vec![Arc::new(user_msg("hello")), Arc::new(user_msg("again"))],
    );
    let body = build_request_body(&req, false);
    let messages = body["messages"].as_array().unwrap();
    let last = &messages[messages.len() - 1];
    let blocks = last["content"].as_array().unwrap();
    let last_block = &blocks[blocks.len() - 1];
    assert_eq!(last_block["cache_control"]["type"], "ephemeral");
}

/// The gateway detection in `uncached_input` assumes every request ends
/// with a breakpoint, including the tool results of an agent turn.
#[test]
fn body_marks_trailing_tool_result_for_caching() {
    let req = StreamRequest::new(
        "m",
        vec![
            Arc::new(user_msg("go")),
            Arc::new(assistant_tool_call("t1", "read")),
            Arc::new(tool_result("t1", "contents")),
        ],
    );
    let body = build_request_body(&req, true);
    let messages = body["messages"].as_array().unwrap();
    let blocks = messages[messages.len() - 1]["content"].as_array().unwrap();
    let last_block = &blocks[blocks.len() - 1];
    assert_eq!(last_block["type"], "tool_result");
    assert_eq!(last_block["cache_control"]["type"], "ephemeral");
}

#[test]
fn body_does_not_mark_earlier_messages() {
    let req = StreamRequest::new(
        "m",
        vec![Arc::new(user_msg("first")), Arc::new(user_msg("second"))],
    );
    let body = build_request_body(&req, false);
    let messages = body["messages"].as_array().unwrap();
    let first = &messages[0];
    let blocks = first["content"].as_array().unwrap();
    assert!(blocks[0].get("cache_control").is_none());
}

#[test]
fn body_drops_system_role_messages() {
    let mut req = StreamRequest::new(
        "m",
        vec![
            Arc::new(Message::new(
                Role::System,
                vec![Content::Text {
                    text: "ignored".into(),
                }],
                None,
            )),
            Arc::new(user_msg("hi")),
        ],
    );
    req.system = Some("the real system prompt".into());
    let body = build_request_body(&req, false);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
}

#[test]
fn body_includes_tools_when_present() {
    let mut req = StreamRequest::new("m", vec![Arc::new(user_msg("hi"))]);
    req.tools = vec![ToolSpec {
        name: "read".into(),
        description: "read a file".into(),
        schema: serde_json::json!({"type":"object"}),
    }];
    let body = build_request_body(&req, false);
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "read");
    assert_eq!(tools[0]["description"], "read a file");
    assert_eq!(
        tools[0]["input_schema"],
        serde_json::json!({"type":"object"})
    );
}

#[test]
fn body_includes_thinking_when_configured() {
    let mut req = StreamRequest::new("m", vec![Arc::new(user_msg("hi"))]);
    req.thinking = Some(crate::ThinkingConfig {
        budget_tokens: 12_000,
    });
    let body = build_request_body(&req, false);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 12_000);
}

#[test]
fn body_resolves_thinking_level_to_default_budget() {
    let mut req = StreamRequest::new("unknown-model", vec![Arc::new(user_msg("hi"))]);
    req.level = Some(crate::ThinkingLevel::High);
    let body = build_request_body(&req, false);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(
        body["thinking"]["budget_tokens"],
        crate::ThinkingLevel::High.default_budget_tokens().unwrap()
    );
}

#[test]
fn body_omits_thinking_when_level_off() {
    let mut req = StreamRequest::new("m", vec![Arc::new(user_msg("hi"))]);
    req.level = Some(crate::ThinkingLevel::Off);
    let body = build_request_body(&req, false);
    assert!(body.get("thinking").is_none());
}

#[test]
fn explicit_thinking_config_wins_over_level() {
    let mut req = StreamRequest::new("m", vec![Arc::new(user_msg("hi"))]);
    req.thinking = Some(crate::ThinkingConfig { budget_tokens: 999 });
    req.level = Some(crate::ThinkingLevel::XHigh);
    let body = build_request_body(&req, false);
    assert_eq!(body["thinking"]["budget_tokens"], 999);
}

#[test]
fn continuation_request_drops_thinking() {
    let mut req = StreamRequest::new(
        "m",
        vec![
            Arc::new(user_msg("run it")),
            Arc::new(assistant_tool_call("call_1", "shell")),
            Arc::new(tool_result("call_1", "file.rs")),
        ],
    );
    req.thinking = Some(crate::ThinkingConfig {
        budget_tokens: 12_000,
    });
    let body = build_request_body(&req, true);
    assert!(body.get("thinking").is_none());
}

#[test]
fn completed_tool_turn_keeps_thinking_on_next_request() {
    let mut req = StreamRequest::new(
        "m",
        vec![
            Arc::new(user_msg("run it")),
            Arc::new(assistant_tool_call("call_1", "shell")),
            Arc::new(tool_result("call_1", "file.rs")),
            Arc::new(Message::new(
                Role::Assistant,
                vec![Content::Text {
                    text: "done".into(),
                }],
                None,
            )),
            Arc::new(user_msg("thanks")),
        ],
    );
    req.thinking = Some(crate::ThinkingConfig {
        budget_tokens: 12_000,
    });
    let body = build_request_body(&req, false);
    assert_eq!(body["thinking"]["type"], "enabled");
}

#[test]
fn steering_after_tool_results_drops_thinking() {
    let mut req = StreamRequest::new(
        "m",
        vec![
            Arc::new(user_msg("run it")),
            Arc::new(assistant_tool_call("call_1", "shell")),
            Arc::new(tool_result("call_1", "file.rs")),
            Arc::new(user_msg("also check the logs")),
        ],
    );
    req.thinking = Some(crate::ThinkingConfig {
        budget_tokens: 12_000,
    });
    let body = build_request_body(&req, true);
    assert!(body.get("thinking").is_none());
}

#[test]
fn body_uses_default_max_tokens_when_unset() {
    let req = StreamRequest::new("m", vec![Arc::new(user_msg("hi"))]);
    let body = build_request_body(&req, false);
    assert_eq!(body["max_tokens"], 4_096);
}

#[test]
fn assistant_message_with_tool_call_serializes() {
    let assistant = Message::new(
        Role::Assistant,
        vec![Content::ToolCall {
            id: ToolCallId::new("call_1"),
            name: "read".into(),
            input: serde_json::json!({"path":"/etc/hosts"}),
        }],
        None,
    );
    let req = StreamRequest::new(
        "m",
        vec![Arc::new(user_msg("read hosts")), Arc::new(assistant)],
    );
    let body = build_request_body(&req, false);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[1]["role"], "assistant");
    let blocks = messages[1]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "tool_use");
    assert_eq!(blocks[0]["id"], "call_1");
    assert_eq!(blocks[0]["name"], "read");
    assert_eq!(blocks[0]["input"]["path"], "/etc/hosts");
}

#[test]
fn tool_result_message_uses_user_role() {
    let result = Message::new(
        Role::ToolResult,
        vec![Content::ToolResultBlock {
            call_id: ToolCallId::new("call_1"),
            output: "127.0.0.1".into(),
            is_error: false,
        }],
        None,
    );
    let req = StreamRequest::new("m", vec![Arc::new(result)]);
    let body = build_request_body(&req, false);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    let blocks = messages[0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "tool_result");
    assert_eq!(blocks[0]["tool_use_id"], "call_1");
    assert_eq!(blocks[0]["content"], "127.0.0.1");
    assert_eq!(blocks[0]["is_error"], false);
}

/// Round-trip the real Anthropic API. Opt-in: requires `ANTHROPIC_API_KEY`
/// in the environment. Run with:
///
/// ```sh
/// ANTHROPIC_API_KEY=sk-ant-... cargo test -p kage-provider -- --ignored anthropic_live
/// ```
#[test]
#[ignore = "requires ANTHROPIC_API_KEY"]
fn anthropic_live_smoke() {
    let key = std::env::var("ANTHROPIC_API_KEY").expect("set ANTHROPIC_API_KEY to run this test");
    let provider = AnthropicProvider::new(key);
    let req = StreamRequest::new(
        "claude-haiku-4-5-20251001",
        vec![Arc::new(Message::new(
            Role::User,
            vec![Content::Text {
                text: "Reply with exactly the word: pong".into(),
            }],
            None,
        ))],
    );
    let stream = provider
        .stream(req, &CancelFlag::new())
        .expect("request succeeds");
    let events = collect_ok(stream);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProviderEvent::TextDelta { .. })),
        "response has at least one text delta"
    );
    let Some(ProviderEvent::MessageEnd { usage, .. }) = events.last() else {
        panic!("stream ends with MessageEnd");
    };
    assert!(usage.input > 0, "input tokens reported");
    assert!(usage.output > 0, "output tokens reported");
}

fn stream_from_bytes(bytes: &'static [u8]) -> AnthropicStream {
    AnthropicStream::new(Box::new(std::io::Cursor::new(bytes)), CancelFlag::new())
}

/// The protocol ends with `message_stop`; a stream cut before it must
/// still complete the turn with the stop reason and usage it absorbed.
#[test]
fn eof_without_message_stop_completes_the_turn() {
    let bytes: &[u8] = b"event: message_start\n\
         data: {\"type\":\"message_start\"}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n";
    let mut events = stream_from_bytes(bytes);
    let mut text = String::new();
    loop {
        match events.next() {
            Some(Ok(ProviderEvent::TextDelta { delta })) => text.push_str(&delta),
            Some(Ok(ProviderEvent::MessageEnd { usage, stop_reason })) => {
                assert_eq!(text, "hello");
                assert_eq!(stop_reason, StopReason::EndTurn);
                assert_eq!(usage.output, 7);
                break;
            }
            Some(Ok(_)) => {}
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert!(events.next().is_none());
}

/// Some compatible endpoints repeat the usage object in
/// `message_delta` with zeros for the prompt counts. They must not wipe
/// the counts `message_start` reported.
#[test]
fn zero_counts_in_message_delta_keep_the_start_usage() {
    let bytes: &[u8] = b"event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5000,\"cache_read_input_tokens\":90000,\"output_tokens\":1}}}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":40}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n\n";
    let usage = stream_from_bytes(bytes)
        .find_map(|event| match event {
            Ok(ProviderEvent::MessageEnd { usage, .. }) => Some(usage),
            _ => None,
        })
        .unwrap();
    assert_eq!(usage.input, 5000);
    assert_eq!(usage.cache_read, 90000);
    assert_eq!(usage.output, 40);
}

#[test]
fn a_gateway_counting_cache_inside_input_counts_it_once() {
    let bytes: &[u8] = b"event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":340412,\"cache_read_input_tokens\":339875,\"cache_creation_input_tokens\":200,\"output_tokens\":1}}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n\n";
    let usage = uncached_input(Box::new(stream_from_bytes(bytes)), true)
        .find_map(|event| match event {
            Ok(ProviderEvent::MessageEnd { usage, .. }) => Some(usage),
            _ => None,
        })
        .unwrap();
    assert_eq!(usage.input, 337);
    assert_eq!(usage.cache_read, 339_875);
    assert_eq!(usage.cache_write, 200);
}

fn usage_after_correction(input: u64, read: u64, write: u64) -> kage_core::TokenUsage {
    let body = format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":{input},\"cache_read_input_tokens\":{read},\"cache_creation_input_tokens\":{write},\"output_tokens\":1}}}}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    );
    uncached_input(
        Box::new(AnthropicStream::new(
            Box::new(std::io::Cursor::new(body.into_bytes())),
            CancelFlag::new(),
        )),
        false,
    )
    .find_map(|event| match event {
        Ok(ProviderEvent::MessageEnd { usage, .. }) => Some(usage),
        _ => None,
    })
    .unwrap()
}

#[test]
fn an_unflagged_gateway_that_includes_cache_in_input_is_detected() {
    let usage = usage_after_correction(25_231, 18_901, 0);
    assert_eq!(usage.input, 6_330);
    assert_eq!(usage.cache_read, 18_901);
}

#[test]
fn an_unflagged_endpoint_with_anthropic_counts_is_left_alone() {
    let usage = usage_after_correction(2, 33_847, 15_482);
    assert_eq!(usage.input, 2);
    let usage = usage_after_correction(5_000, 90_000, 0);
    assert_eq!(usage.input, 5_000);
}

#[test]
fn eof_before_message_start_invents_nothing() {
    let mut events = stream_from_bytes(b"");
    assert!(events.next().is_none());
}

#[test]
fn sse_parser_extracts_event_and_data() {
    let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let mut reader = std::io::BufReader::new(std::io::Cursor::new(bytes));
    let first = crate::sse::read_sse_event(&mut reader).unwrap().unwrap();
    assert_eq!(first.name, "message_start");
    assert!(first.data.contains("input_tokens"));
    let second = crate::sse::read_sse_event(&mut reader).unwrap().unwrap();
    assert_eq!(second.name, "message_stop");
    assert!(crate::sse::read_sse_event(&mut reader).unwrap().is_none());
}

#[test]
fn sse_parser_ignores_comments_and_blank_lines() {
    let bytes: &[u8] = b": this is a comment\n\nevent: ping\ndata: {}\n\nevent: ping\ndata: {}\n\n";
    let mut reader = std::io::BufReader::new(std::io::Cursor::new(bytes));
    let first = crate::sse::read_sse_event(&mut reader).unwrap().unwrap();
    assert_eq!(first.name, "ping");
    let second = crate::sse::read_sse_event(&mut reader).unwrap().unwrap();
    assert_eq!(second.name, "ping");
}

#[test]
fn stream_emits_text_deltas() {
    let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let events = collect_ok(stream_from_bytes(bytes));
    assert!(matches!(events[0], ProviderEvent::MessageStart));
    assert!(
        matches!(&events[1], ProviderEvent::TextDelta { delta } if delta == "hello"),
        "got {:?}",
        events[1]
    );
    assert!(
        matches!(&events[2], ProviderEvent::TextDelta { delta } if delta == " world"),
        "got {:?}",
        events[2]
    );
    if let ProviderEvent::MessageEnd { stop_reason, usage } = events.last().unwrap() {
        assert_eq!(*stop_reason, StopReason::EndTurn);
        assert_eq!(usage.input, 5);
        assert_eq!(usage.output, 2);
    } else {
        panic!("expected MessageEnd at end, got {:?}", events.last());
    }
}

#[test]
fn stream_emits_thinking_delta() {
    let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reasoning...\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let events = collect_ok(stream_from_bytes(bytes));
    assert!(
        events.iter().any(
            |e| matches!(e, ProviderEvent::ThinkingDelta { delta } if delta == "reasoning...")
        )
    );
}

#[test]
fn stream_captures_signature_and_redacted_thinking() {
    let bytes: &[u8] = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"EqQB\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"enc\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let events = collect_ok(stream_from_bytes(bytes));
    assert_eq!(
        events,
        vec![
            ProviderEvent::ThinkingDelta {
                delta: "hmm".into()
            },
            ProviderEvent::ThinkingSignature {
                data: "EqQB".into()
            },
            ProviderEvent::RedactedThinking { data: "enc".into() },
            ProviderEvent::MessageEnd {
                stop_reason: StopReason::default(),
                usage: kage_core::TokenUsage::default(),
            },
        ]
    );
}

#[test]
fn stream_assembles_tool_call() {
    let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"read\",\"input\":{}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"/tmp\\\"}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":3}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let events = collect_ok(stream_from_bytes(bytes));

    let start_idx = events
        .iter()
        .position(|e| matches!(e, ProviderEvent::ToolCallStart { .. }))
        .expect("ToolCallStart present");
    if let ProviderEvent::ToolCallStart { id, name } = &events[start_idx] {
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
    } else {
        panic!("expected MessageEnd");
    }
}

#[test]
fn stream_emits_decode_error_when_tool_input_partial_json_is_malformed() {
    let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"write\",\"input\":{}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{not json\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let s = stream_from_bytes(bytes);
    let events: Vec<_> = s.collect();
    let decode_err = events
        .iter()
        .find(|r| matches!(r, Err(ProviderError::Decode(_))))
        .expect("expected a Decode error event for malformed tool input");
    if let Err(ProviderError::Decode(msg)) = decode_err {
        assert!(msg.contains("call_1"), "error should name the tool call id");
        assert!(
            msg.contains("{not json"),
            "error should include the raw partial input"
        );
    }
    assert!(
        !events
            .iter()
            .any(|r| matches!(r, Ok(ProviderEvent::ToolCallEnd { .. }))),
        "no ToolCallEnd should fire when input fails to parse"
    );
}

#[test]
fn index_less_content_blocks_get_distinct_synthetic_slots() {
    let bytes: &[u8] = b"event: message_start\n\
         data: {\"type\":\"message_start\"}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"read\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"/a\\\"}\"}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\"}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_2\",\"name\":\"write\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"text\\\":\\\"x\\\"}\"}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\"}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n\n";
    let events = collect_ok(stream_from_bytes(bytes));
    let starts: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::ToolCallStart { id, name } => Some((id.0.as_str(), name.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(starts, [("call_1", "read"), ("call_2", "write")]);
    let deltas: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::ToolCallArgsDelta { id, partial } => {
                Some((id.0.as_str(), partial.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        [
            ("call_1", "{\"path\":\"/a\"}"),
            ("call_2", "{\"text\":\"x\"}")
        ]
    );
    let ends: Vec<(&str, Value)> = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::ToolCallEnd { id, input } => Some((id.0.as_str(), input.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        ends,
        [
            ("call_1", serde_json::json!({"path": "/a"})),
            ("call_2", serde_json::json!({"text": "x"})),
        ]
    );
}

#[test]
fn stream_yields_cancelled_when_flag_set() {
    let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let cancel = CancelFlag::new();
    cancel.cancel();
    let stream = AnthropicStream::new(Box::new(std::io::Cursor::new(bytes)), cancel);
    let mut events = stream;
    let first = events.next();
    assert!(matches!(first, Some(Err(ProviderError::Cancelled))));
    assert!(events.next().is_none());
}

#[test]
fn stream_propagates_error_event_as_transient() {
    let bytes: &[u8] = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"servers are overloaded\"}}\n\n";
    let mut events = stream_from_bytes(bytes);
    let first = events.next().unwrap();
    match first {
        Err(err) => {
            assert!(err.is_transient(), "overloaded errors retry: got {err:?}");
            assert!(err.to_string().contains("overloaded"), "got {err}");
        }
        other => panic!("expected an error event, got {other:?}"),
    }
}

#[test]
fn stream_error_event_rate_limit_classifies_as_rate_limited() {
    let bytes: &[u8] = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"Number of requests too high\"}}\n\n";
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
        "rate_limit_error should surface as RateLimited, got {first:?}"
    );
}

#[test]
fn request_headers_include_version_key_then_extras_in_key_order() {
    let mut extras = BTreeMap::new();
    extras.insert("X-B".to_owned(), "2".to_owned());
    extras.insert("X-A".to_owned(), "1".to_owned());
    let provider = AnthropicProvider::new("k").with_extra_headers(extras);
    assert_eq!(
        provider.request_headers(),
        vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()),
            ("x-api-key".to_owned(), "k".to_owned()),
            ("X-A".to_owned(), "1".to_owned()),
            ("X-B".to_owned(), "2".to_owned()),
        ]
    );
}

#[test]
fn request_headers_skip_key_when_key_is_empty() {
    let mut extras = BTreeMap::new();
    extras.insert("X-A".to_owned(), "1".to_owned());
    let provider = AnthropicProvider::new("").with_extra_headers(extras);
    let headers = provider.request_headers();
    assert!(
        headers.iter().all(|(name, _)| name != "x-api-key"),
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
    let provider = AnthropicProvider::new("k").with_models(models.clone());
    assert_eq!(provider.models(), models);
    assert!(AnthropicProvider::new("k").models().is_empty());
}

#[test]
fn a_tool_s_images_go_inside_its_tool_result() {
    let msg = Message::new(
        Role::ToolResult,
        vec![
            Content::ToolResultBlock {
                call_id: kage_core::ToolCallId::new("c1"),
                output: "drawn".into(),
                is_error: false,
            },
            Content::Image {
                source: kage_core::MediaSource::Base64 {
                    data: "iVBO".into(),
                },
                mime: "image/png".into(),
            },
        ],
        None,
    );
    let req = StreamRequest::new("claude-sonnet-4-6", vec![Arc::new(msg)]);
    let body = build_request_body(&req, false);
    let content = &body["messages"][0]["content"][0]["content"];
    assert_eq!(
        content[0],
        serde_json::json!({"type": "text", "text": "drawn"})
    );
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["data"], "iVBO");
}

/// Audio history goes back as its note text in every position, and no
/// clip's bytes ride the request.
#[test]
fn audio_becomes_note_text_and_never_carries_its_bytes() {
    let audio = || kage_core::Content::Audio {
        source: kage_core::MediaSource::Base64 {
            data: "QUJD".into(),
        },
        mime: "audio/mpeg".into(),
    };
    let req = StreamRequest::new(
        "claude-sonnet-4-6",
        vec![
            Arc::new(Message::new(Role::User, vec![audio()], None)),
            Arc::new(Message::new(Role::Assistant, vec![audio()], None)),
            Arc::new(Message::new(
                Role::ToolResult,
                vec![
                    Content::ToolResultBlock {
                        call_id: kage_core::ToolCallId::new("c1"),
                        output: "clipped".into(),
                        is_error: false,
                    },
                    audio(),
                ],
                None,
            )),
        ],
    );
    let body = build_request_body(&req, false);
    let text = body.to_string();
    assert!(!text.contains("QUJD"), "audio bytes leaked: {text}");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(
        messages[0]["content"][0],
        serde_json::json!({"type": "text", "text": Content::audio_note("audio/mpeg")})
    );
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"][0]["text"],
        Content::produced_media_note("audio/mpeg")
    );
    let content = &messages[2]["content"][0]["content"];
    assert_eq!(
        content[0],
        serde_json::json!({"type": "text", "text": "clipped"})
    );
    assert_eq!(content[1]["text"], Content::audio_note("audio/mpeg"));
}
