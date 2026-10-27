//! Tests for the agent loop.

use std::sync::Arc;

use kage_core::{CancelFlag, Content, Message, Role, TokenUsage};
use kage_provider::testing::{MockProvider, user_msg};
use kage_provider::{ProviderEvent, StopReason};
use kage_tools::ToolRegistry;

use super::*;
use crate::NoopHooks;
use crate::test_support::{Meet, MeetTool};

/// Portable absolute workdir for loop tests, unique per test name. A
/// literal `/tmp` is not absolute on Windows, where it lacks a drive.
fn temp_workdir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("kage-loop-test-{name}"))
}

/// A single text block, the shape a steering hook returns.
fn text_steering(text: impl Into<String>) -> Vec<Content> {
    vec![Content::Text { text: text.into() }]
}

#[test]
fn build_request_forwards_max_output_tokens_from_context() {
    let mut cx = AgentContext::new("m", "");
    cx.history.push(Arc::new(user_msg("hi")));
    cx = cx.with_max_output_tokens(32_000);
    let tools = ToolRegistry::new();
    let provider = MockProvider::sequence(vec![]);
    let req = build_request(&cx, &tools, &provider);
    assert_eq!(req.max_output_tokens, Some(32_000));
}

#[test]
fn build_request_answers_calls_a_quit_left_open() {
    let call = |id: &str| Content::ToolCall {
        id: ToolCallId::new(id),
        name: "agent".into(),
        input: serde_json::json!({}),
    };
    let result = |id: &str| {
        Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new(id),
                output: "done".into(),
                is_error: false,
            }],
            None,
        )
    };
    let mut cx = AgentContext::new("m", "");
    for message in [
        user_msg("go"),
        Message::new(Role::Assistant, vec![call("a"), call("b")], None),
        result("a"),
        user_msg("continue"),
    ] {
        cx.history.push(Arc::new(message));
    }
    let provider = MockProvider::sequence(vec![]);
    let req = build_request(&cx, &ToolRegistry::new(), &provider);
    let roles: Vec<Role> = req.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::Assistant,
            Role::ToolResult,
            Role::ToolResult,
            Role::User
        ]
    );
    assert!(matches!(
        &req.messages[3].content[..],
        [Content::ToolResultBlock { call_id, is_error: true, .. }] if *call_id == ToolCallId::new("b")
    ));
    assert_eq!(cx.history.len(), 4, "the stored history keeps the gap");

    cx.history.truncate(2);
    let req = build_request(&cx, &ToolRegistry::new(), &provider);
    assert_eq!(req.messages.len(), 4, "a trailing call is answered too");
}

#[test]
fn build_request_leaves_max_output_tokens_unset_when_context_default() {
    let mut cx = AgentContext::new("m", "");
    cx.history.push(Arc::new(user_msg("hi")));
    let tools = ToolRegistry::new();
    let provider = MockProvider::sequence(vec![]);
    let req = build_request(&cx, &tools, &provider);
    assert!(req.max_output_tokens.is_none());
}

/// A context shaped like the session that hit the 400: a 262K window,
/// a 128K output limit, and 158K of history billed on the last turn.
fn filled_context(current_context: u64) -> AgentContext {
    let mut cx = AgentContext::new("m", "")
        .with_context_window(262_144)
        .with_max_output_tokens(128_000);
    cx.history.push(Arc::new(user_msg("go")));
    cx.history.push(Arc::new(assistant_msg(vec![Content::Text {
        text: "ok".into(),
    }])));
    cx.budget.current_context = current_context;
    cx
}

#[test]
fn build_request_fits_the_output_ceiling_into_the_window() {
    let cx = filled_context(158_005);
    let req = build_request(&cx, &ToolRegistry::new(), &MockProvider::sequence(vec![]));
    let ceiling = u64::from(req.max_output_tokens.unwrap());
    assert!(ceiling < 128_000);
    assert!(158_005 + ceiling <= 262_144);
}

#[test]
fn build_request_keeps_the_full_ceiling_when_it_fits() {
    let cx = filled_context(20_000);
    let req = build_request(&cx, &ToolRegistry::new(), &MockProvider::sequence(vec![]));
    assert_eq!(req.max_output_tokens, Some(128_000));
}

#[test]
fn build_request_counts_results_added_since_the_last_turn() {
    let mut cx = filled_context(100_000);
    cx.history.push(Arc::new(user_msg(&"x".repeat(400_000))));
    let req = build_request(&cx, &ToolRegistry::new(), &MockProvider::sequence(vec![]));
    let ceiling = u64::from(req.max_output_tokens.unwrap());
    assert!(100_000 + 100_000 + ceiling <= 262_144);
}

#[test]
fn build_request_never_drops_below_reply_and_thinking_room() {
    let mut cx = filled_context(261_000);
    cx.reasoning = kage_core::Reasoning::Budget {
        min: 1_024,
        max: None,
        toggle: true,
    };
    let req = build_request(&cx, &ToolRegistry::new(), &MockProvider::sequence(vec![]));
    assert_eq!(req.max_output_tokens, Some(8_192 + 16_384));
}

#[test]
fn build_request_forwards_what_the_model_reads_and_produces() {
    let mut cx = filled_context(1_000);
    cx.accepts =
        kage_core::Modalities::of(&[kage_core::Modality::Text, kage_core::Modality::Audio]);
    cx.produces =
        kage_core::Modalities::of(&[kage_core::Modality::Text, kage_core::Modality::Image]);
    let req = build_request(&cx, &ToolRegistry::new(), &MockProvider::sequence(vec![]));
    assert_eq!(req.accepts, cx.accepts);
    assert_eq!(req.produces, cx.produces);
}

fn assistant_msg(content: Vec<Content>) -> Message {
    Message::new(Role::Assistant, content, None)
}

#[test]
fn flatten_thinking_rewrites_thinking_to_tagged_text() {
    let history = vec![Arc::new(assistant_msg(vec![
        Content::Thinking {
            text: "weigh the options".to_owned(),
            signature: None,
            duration_ms: None,
        },
        Content::Text {
            text: "the answer is 42".to_owned(),
        },
    ]))];
    let flat = flatten_thinking(&history);
    assert_eq!(
        flat[0].content,
        vec![
            Content::Text {
                text: "<thinking>\nweigh the options\n</thinking>".to_owned(),
            },
            Content::Text {
                text: "the answer is 42".to_owned(),
            },
        ]
    );
}

#[test]
fn flatten_thinking_drops_empty_thinking_blocks() {
    let history = vec![Arc::new(assistant_msg(vec![
        Content::Thinking {
            text: "   ".to_owned(),
            signature: None,
            duration_ms: None,
        },
        Content::Text {
            text: "done".to_owned(),
        },
    ]))];
    let flat = flatten_thinking(&history);
    assert_eq!(
        flat[0].content,
        vec![Content::Text {
            text: "done".to_owned(),
        }]
    );
}

#[test]
fn flatten_thinking_leaves_non_thinking_messages_unchanged() {
    let history = vec![
        Arc::new(user_msg("hi")),
        Arc::new(assistant_msg(vec![Content::Text {
            text: "hello".to_owned(),
        }])),
    ];
    assert_eq!(flatten_thinking(&history), history);
}

#[test]
fn build_request_shares_history_arcs_without_cloning_text() {
    let mut cx = AgentContext::new("m", "");
    cx.history.push(Arc::new(user_msg("hi")));
    cx.history
        .push(Arc::new(assistant_msg(vec![Content::Thinking {
            text: "reason".to_owned(),
            signature: None,
            duration_ms: None,
        }])));
    cx.history.push(Arc::new(assistant_msg(vec![Content::Text {
        text: "done".to_owned(),
    }])));
    let tools = ToolRegistry::new();
    let provider = MockProvider::sequence(vec![]);
    let req = build_request(&cx, &tools, &provider);

    // Messages without thinking blocks are shared, not copied: the
    // request holds the same allocation as the history and nothing else.
    assert!(Arc::ptr_eq(&req.messages[0], &cx.history[0]));
    assert!(Arc::ptr_eq(&req.messages[2], &cx.history[2]));
    assert_eq!(Arc::strong_count(&req.messages[0]), 2);

    // A thinking message is rewritten into a fresh allocation held only
    // by the request.
    assert!(!Arc::ptr_eq(&req.messages[1], &cx.history[1]));
    assert_eq!(Arc::strong_count(&req.messages[1]), 1);
}

#[test]
fn build_request_flattens_history_thinking() {
    let mut cx = AgentContext::new("m", "");
    cx.history.push(Arc::new(user_msg("question")));
    cx.history
        .push(Arc::new(assistant_msg(vec![Content::Thinking {
            text: "reason".to_owned(),
            signature: None,
            duration_ms: None,
        }])));
    let tools = ToolRegistry::new();
    let provider = MockProvider::sequence(vec![]);
    let req = build_request(&cx, &tools, &provider);
    assert!(
        !req.messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|c| matches!(c, Content::Thinking { .. })),
        "no native thinking blocks should survive into the request"
    );
    assert!(req.messages[1].content.iter().any(|c| matches!(
        c,
        Content::Text { text } if text.contains("<thinking>") && text.contains("reason")
    )));
}

struct OneFollowup(bool);
impl Hooks for OneFollowup {
    fn get_followup(&mut self) -> Option<String> {
        if self.0 {
            self.0 = false;
            Some("anything else?".into())
        } else {
            None
        }
    }
}

struct OneShotFollowup {
    text: Option<String>,
}
impl Hooks for OneShotFollowup {
    fn get_followup(&mut self) -> Option<String> {
        self.text.take()
    }
}

struct Steering(bool);
impl Hooks for Steering {
    fn get_steering(&mut self) -> Option<Vec<Content>> {
        if self.0 {
            self.0 = false;
            Some(text_steering("be terse"))
        } else {
            None
        }
    }
}

#[derive(Default)]
struct CountingSteering {
    polls: u32,
}
impl Hooks for CountingSteering {
    fn get_steering(&mut self) -> Option<Vec<Content>> {
        self.polls = self.polls.saturating_add(1);
        None
    }
}

#[test]
fn shell_returns_after_provider_emits_text_only_turn() {
    let mock = MockProvider::replaying(vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::TextDelta { delta: "hi".into() }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }),
    ]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    let cfg = LoopConfig::default();
    let mut hooks = NoopHooks;
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let mut events = Vec::new();
    let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |ev| {
        events.push(ev);
    });
    assert!(res.is_ok());
    assert_eq!(mock.call_count(), 1);
}

#[test]
fn shell_re_enters_inner_loop_on_followup() {
    let mock = MockProvider::sequence(vec![
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
    ]);

    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hi")));
    let cfg = LoopConfig::default();
    let mut hooks = OneFollowup(true);
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
    assert!(res.is_ok());
    assert_eq!(mock.call_count(), 2, "follow-up should trigger second turn");
}

#[test]
fn shell_emits_steering_message_before_first_turn() {
    let mock = MockProvider::replaying(vec![Ok(ProviderEvent::MessageEnd {
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    })]);

    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hi")));
    let cfg = LoopConfig::default();
    let mut hooks = Steering(true);
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let _ = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
    let req = mock.last_request().unwrap();
    let last = req.messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(matches!(
        &last.content[0],
        Content::Text { text } if text == "be terse"
    ));
}

/// Turn boundaries seen in `events`, as `(start or end, index)`.
fn turns(events: &[LoopEvent]) -> Vec<(&'static str, u32)> {
    events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::TurnStarted { index } => Some(("start", *index)),
            LoopEvent::TurnEnded { index, .. } => Some(("end", *index)),
            _ => None,
        })
        .collect()
}

#[test]
fn turn_boundaries_fire_once_for_text_only_turn() {
    let mock = MockProvider::replaying(vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::TextDelta { delta: "hi".into() }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }),
    ]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    let cfg = LoopConfig::default();
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();
    let mut events = Vec::new();

    run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &cancel,
        |e| {
            events.push(e);
        },
    )
    .unwrap();
    assert_eq!(turns(&events), [("start", 0), ("end", 0)]);
}

#[test]
fn turn_index_advances_across_followup_rounds() {
    let mock = MockProvider::sequence(vec![
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
    ]);

    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hi")));
    let cfg = LoopConfig::default();
    let mut hooks = OneFollowup(true);
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();
    let mut events = Vec::new();

    run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |e| {
        events.push(e);
    })
    .unwrap();
    assert_eq!(
        turns(&events),
        [("start", 0), ("end", 0), ("start", 1), ("end", 1)]
    );
}

struct StaticTransform {
    injection: String,
    calls: u32,
}
impl Hooks for StaticTransform {
    fn transform_context(
        &mut self,
        messages: &mut Vec<std::sync::Arc<kage_core::Message>>,
    ) -> Result<(), String> {
        self.calls = self.calls.saturating_add(1);
        messages.push(Arc::new(kage_core::Message::new(
            kage_core::Role::User,
            vec![Content::Text {
                text: self.injection.clone(),
            }],
            messages.last().map(|m| m.id),
        )));
        Ok(())
    }
}

#[test]
fn transform_context_runs_before_each_provider_call() {
    let mock = MockProvider::replaying(vec![Ok(ProviderEvent::MessageEnd {
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    })]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    let cfg = LoopConfig::default();
    let mut hooks = StaticTransform {
        injection: "from-hook".into(),
        calls: 0,
    };
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
    assert_eq!(hooks.calls, 1);
    let req = mock.last_request().unwrap();
    let last = req.messages.last().unwrap();
    assert!(matches!(
        &last.content[0],
        Content::Text { text } if text == "from-hook"
    ));
}

struct FailingTransform;
impl Hooks for FailingTransform {
    fn transform_context(
        &mut self,
        _messages: &mut Vec<std::sync::Arc<kage_core::Message>>,
    ) -> Result<(), String> {
        Err("transform exploded".into())
    }
}

#[test]
fn transform_context_error_aborts_with_hook_failed() {
    let mock = MockProvider::replaying(vec![]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hi")));
    let cfg = LoopConfig::default();
    let mut hooks = FailingTransform;
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let mut errors = Vec::new();
    let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |ev| {
        if let LoopEvent::Error { kind } = ev {
            errors.push(kind);
        }
    });
    match res {
        Err(LoopError::HookFailed { hook, message }) => {
            assert_eq!(hook, "transform_context");
            assert_eq!(message, "transform exploded");
        }
        other => panic!("expected HookFailed, got {other:?}"),
    }
    assert_eq!(errors.len(), 1);
    assert_eq!(mock.call_count(), 0, "provider never called on hook error");
}

struct StopAfterFirstTurn {
    polls: u32,
}
impl Hooks for StopAfterFirstTurn {
    fn should_stop_after_turn(&mut self, _summary: &crate::TurnSummary) -> bool {
        self.polls = self.polls.saturating_add(1);
        true
    }
    fn get_followup(&mut self) -> Option<String> {
        Some("must-not-be-asked".into())
    }
}

#[test]
fn should_stop_after_turn_suppresses_followup_and_returns_ok() {
    let mock = MockProvider::replaying(vec![Ok(ProviderEvent::MessageEnd {
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    })]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hi")));
    let cfg = LoopConfig::default();
    let mut hooks = StopAfterFirstTurn { polls: 0 };
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
    assert_eq!(hooks.polls, 1, "predicate fired exactly once");
    assert_eq!(
        mock.call_count(),
        1,
        "followup never dequeued, no second turn"
    );
}

#[derive(Debug)]
struct AlwaysCallTool;

impl kage_tools::Tool for AlwaysCallTool {
    fn name(&self) -> &'static str {
        "noop"
    }
    fn description(&self) -> &'static str {
        "no-op tool"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> kage_core::Risk {
        kage_core::Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &kage_tools::ToolContext,
    ) -> Result<kage_core::ToolOutput, kage_tools::ToolError> {
        Ok(kage_core::ToolOutput {
            is_error: false,
            text: "ran".into(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

#[test]
fn should_stop_after_turn_short_circuits_pending_tool_calls() {
    let mock = MockProvider::replaying(vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::ToolCallStart {
            id: kage_core::ToolCallId::new("call_1"),
            name: "noop".into(),
        }),
        Ok(ProviderEvent::ToolCallArgsDelta {
            id: kage_core::ToolCallId::new("call_1"),
            partial: "{}".into(),
        }),
        Ok(ProviderEvent::ToolCallEnd {
            id: kage_core::ToolCallId::new("call_1"),
            input: serde_json::json!({}),
        }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }),
    ]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("plan it")));
    let cfg = LoopConfig::default();
    let mut hooks = StopAfterFirstTurn { polls: 0 };
    let cancel = CancelFlag::new();
    let mut registry = ToolRegistry::new();
    registry.register(std::sync::Arc::new(AlwaysCallTool));

    run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
    assert_eq!(hooks.polls, 1);
    assert_eq!(mock.call_count(), 1);
    let results: Vec<_> = cx
        .history
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            kage_core::Content::ToolResultBlock {
                call_id,
                output,
                is_error,
            } => Some((call_id.to_string(), output.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        vec![(
            "call_1".to_owned(),
            "tool did not run: the run stopped after this turn",
            true
        )],
        "the call is answered without running"
    );
}

struct QueuedSteering(std::collections::VecDeque<String>);
impl Hooks for QueuedSteering {
    fn get_steering(&mut self) -> Option<Vec<Content>> {
        self.0.pop_front().map(text_steering)
    }
}

struct CombinedSteering {
    queued: QueuedSteering,
    followup: OneFollowup,
}
impl Hooks for CombinedSteering {
    fn get_steering(&mut self) -> Option<Vec<Content>> {
        self.queued.get_steering()
    }
    fn get_followup(&mut self) -> Option<String> {
        self.followup.get_followup()
    }
}

#[test]
fn steering_one_at_a_time_drains_one_per_turn() {
    let mock = MockProvider::sequence(vec![
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
    ]);

    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hi")));
    let cfg = LoopConfig::default();
    let mut hooks = CombinedSteering {
        queued: QueuedSteering(["one".into(), "two".into()].into_iter().collect()),
        followup: OneFollowup(true),
    };
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
    let req1 = mock.last_request().unwrap();
    let texts: Vec<&str> = req1
        .messages
        .iter()
        .filter_map(|m| match m.content.first() {
            Some(Content::Text { text }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(texts.contains(&"one"));
    assert!(texts.contains(&"two"));
}

#[test]
fn steering_all_mode_concatenates_in_one_turn() {
    let mock = MockProvider::replaying(vec![Ok(ProviderEvent::MessageEnd {
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    })]);

    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hi")));
    let cfg = LoopConfig {
        steering_mode: SteeringMode::All,
        ..LoopConfig::default()
    };
    let mut hooks = QueuedSteering(["one".into(), "two".into(), "three".into()].into());
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
    let req = mock.last_request().unwrap();
    let merged = req
        .messages
        .iter()
        .filter_map(|m| match m.content.first() {
            Some(Content::Text { text }) => Some(text.as_str()),
            _ => None,
        })
        .find(|t| t.contains("one") && t.contains("two") && t.contains("three"))
        .expect("one merged user message with all three texts");
    assert_eq!(merged, "one\n\ntwo\n\nthree");
}

struct RequestSpy {
    seen_model: Option<String>,
    rewrite_system_to: Option<String>,
}
impl Hooks for RequestSpy {
    fn transform_provider_request(
        &mut self,
        req: &mut kage_provider::StreamRequest,
    ) -> Result<(), String> {
        self.seen_model = Some(req.model.clone());
        if let Some(s) = &self.rewrite_system_to {
            req.system = Some(s.clone());
        }
        Ok(())
    }
}

#[test]
fn transform_provider_request_observes_and_can_rewrite() {
    let mock = MockProvider::replaying(vec![Ok(ProviderEvent::MessageEnd {
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    })]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hi")));
    let cfg = LoopConfig::default();
    let mut hooks = RequestSpy {
        seen_model: None,
        rewrite_system_to: Some("rewritten".into()),
    };
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
    assert_eq!(hooks.seen_model.as_deref(), Some("mock:m"));
    let req = mock.last_request().unwrap();
    assert_eq!(req.system.as_deref(), Some("rewritten"));
}

fn tool_call_turn(names: &[&str]) -> Vec<Result<ProviderEvent, kage_provider::ProviderError>> {
    let mut events = vec![Ok(ProviderEvent::MessageStart)];
    for (i, name) in names.iter().enumerate() {
        let id = kage_core::ToolCallId::new(format!("call_{i}"));
        events.push(Ok(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: (*name).into(),
        }));
        events.push(Ok(ProviderEvent::ToolCallArgsDelta {
            id: id.clone(),
            partial: "{}".into(),
        }));
        events.push(Ok(ProviderEvent::ToolCallEnd {
            id,
            input: serde_json::json!({}),
        }));
    }
    events.push(Ok(ProviderEvent::MessageEnd {
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage::default(),
    }));
    events
}

/// Run one turn calling `tools` in order, then an empty final turn,
/// and return the tool result texts in call order.
fn run_meet_batch(tools: Vec<MeetTool>, parallel_tools: bool) -> Vec<String> {
    let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
    let mock = MockProvider::sequence(vec![
        tool_call_turn(&names),
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
    ]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("go")));
    let cfg = LoopConfig {
        parallel_tools,
        ..LoopConfig::default()
    };
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(std::sync::Arc::new(tool));
    }
    run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &CancelFlag::new(),
        |_| {},
    )
    .unwrap();
    cx.history
        .iter()
        .filter(|m| m.role == Role::ToolResult)
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::ToolResultBlock { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn sequential_tool_in_batch_downgrades_parallel_dispatch() {
    let meet = Meet::new(2, std::time::Duration::from_millis(100));
    let tools = vec![
        MeetTool {
            name: "seq",
            mode: Some(kage_tools::ExecMode::Sequential),
            meet: meet.clone(),
        },
        MeetTool {
            name: "a",
            mode: None,
            meet: meet.clone(),
        },
        MeetTool {
            name: "b",
            mode: None,
            meet,
        },
    ];
    assert_eq!(run_meet_batch(tools, true), ["alone", "met", "met"]);
}

#[test]
fn all_parallel_batch_runs_in_parallel_with_parallel_tools_off() {
    let meet = Meet::new(2, std::time::Duration::from_secs(5));
    let tools = ["a", "b"]
        .map(|name| MeetTool {
            name,
            mode: Some(kage_tools::ExecMode::Parallel),
            meet: meet.clone(),
        })
        .into();
    assert_eq!(run_meet_batch(tools, false), ["met", "met"]);
}

#[test]
fn parallel_tool_mixed_with_default_tool_runs_sequentially() {
    let meet = Meet::new(2, std::time::Duration::from_millis(100));
    let tools = vec![
        MeetTool {
            name: "a",
            mode: Some(kage_tools::ExecMode::Parallel),
            meet: meet.clone(),
        },
        MeetTool {
            name: "b",
            mode: None,
            meet,
        },
    ];
    assert_eq!(run_meet_batch(tools, false), ["alone", "met"]);
}

#[test]
fn sequential_tool_forces_sequential_dispatch_with_parallel_tools_on() {
    let meet = Meet::new(2, std::time::Duration::from_millis(100));
    let tools = vec![
        MeetTool {
            name: "a",
            mode: Some(kage_tools::ExecMode::Parallel),
            meet: meet.clone(),
        },
        MeetTool {
            name: "b",
            mode: Some(kage_tools::ExecMode::Sequential),
            meet,
        },
    ];
    assert_eq!(run_meet_batch(tools, true), ["alone", "met"]);
}

#[derive(Debug)]
struct TaskDoneTool;

impl kage_tools::Tool for TaskDoneTool {
    fn name(&self) -> &'static str {
        "task_done"
    }
    fn description(&self) -> &'static str {
        "signals successful completion"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> kage_core::Risk {
        kage_core::Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &kage_tools::ToolContext<'_>,
    ) -> Result<kage_core::ToolOutput, kage_tools::ToolError> {
        Ok(kage_core::ToolOutput {
            is_error: false,
            text: "done".into(),
            structured: None,
            terminate: true,
            media: Vec::new(),
        })
    }
}

struct AlwaysFollowup;
impl Hooks for AlwaysFollowup {
    fn get_followup(&mut self) -> Option<String> {
        Some("ignored".into())
    }
}

#[test]
fn terminate_flag_short_circuits_run_with_no_followup() {
    let mock = MockProvider::sequence(vec![
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::ToolCallStart {
                id: kage_core::ToolCallId::new("call_done"),
                name: "task_done".into(),
            }),
            Ok(ProviderEvent::ToolCallArgsDelta {
                id: kage_core::ToolCallId::new("call_done"),
                partial: "{}".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: kage_core::ToolCallId::new("call_done"),
                input: serde_json::json!({}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
    ]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("do it")));
    let cfg = LoopConfig::default();
    let mut hooks = AlwaysFollowup;
    let cancel = CancelFlag::new();
    let mut registry = ToolRegistry::new();
    registry.register(std::sync::Arc::new(TaskDoneTool));

    run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
    assert_eq!(
        mock.call_count(),
        1,
        "followup never dequeued after terminate"
    );
    let last = cx.history.last().unwrap();
    assert_eq!(last.role, kage_core::Role::ToolResult);
}

#[test]
fn shell_returns_cancelled_when_cancel_flagged_up_front() {
    let mock = MockProvider::replaying(vec![]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hi")));
    let cfg = LoopConfig::default();
    let mut hooks = NoopHooks;
    let cancel = CancelFlag::new();
    cancel.cancel();
    let registry = ToolRegistry::new();

    let mut errors = Vec::new();
    let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |ev| {
        if let LoopEvent::Error { kind } = ev {
            errors.push(kind);
        }
    });
    assert!(matches!(res, Err(LoopError::Cancelled)));
    assert_eq!(errors, vec![LoopError::Cancelled]);
}

#[derive(Debug)]
struct StaticTool;

impl kage_tools::Tool for StaticTool {
    fn name(&self) -> &'static str {
        "static"
    }
    fn description(&self) -> &'static str {
        "returns a fixed string"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> kage_core::Risk {
        kage_core::Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &kage_tools::ToolContext<'_>,
    ) -> Result<kage_core::ToolOutput, kage_tools::ToolError> {
        Ok(kage_core::ToolOutput {
            is_error: false,
            text: "static-result".into(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

#[test]
fn followup_text_appears_in_next_provider_request() {
    let mock = MockProvider::sequence(vec![
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
    ]);

    let mut cx = AgentContext::new("mock:m", "").with_workdir(temp_workdir("followup_text"));
    cx.history.push(Arc::new(user_msg("first ask")));
    let cfg = LoopConfig::default();
    let mut hooks = OneShotFollowup {
        text: Some("now also do this".into()),
    };
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
    assert!(res.is_ok());
    assert_eq!(mock.call_count(), 2);

    // The second request must include the followup as the most recent user message.
    let req2 = mock.requests().into_iter().nth(1).unwrap();
    let last = req2.messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(matches!(
        &last.content[0],
        Content::Text { text } if text == "now also do this"
    ));
}

#[test]
fn steering_is_polled_before_every_inner_turn() {
    // Two-turn run: first turn returns a tool call, second turn ends.
    let call_id = kage_core::ToolCallId::new("call_x");
    let mock = MockProvider::sequence(vec![
        vec![
            Ok(ProviderEvent::ToolCallStart {
                id: call_id.clone(),
                name: "static".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: call_id.clone(),
                input: serde_json::json!({}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
    ]);

    let mut cx = AgentContext::new("mock:m", "").with_workdir(temp_workdir("steering_poll"));
    cx.history.push(Arc::new(user_msg("go")));
    let cfg = LoopConfig::default();
    let mut hooks = CountingSteering::default();
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new().with(std::sync::Arc::new(StaticTool));

    let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
    assert!(res.is_ok(), "loop failed: {res:?}");
    assert_eq!(hooks.polls, 2, "steering polled once per inner-loop turn");
}

#[derive(Debug, Clone)]
struct CancellingTool {
    cancel: kage_core::CancelFlag,
}

impl kage_tools::Tool for CancellingTool {
    fn name(&self) -> &'static str {
        "cancel_now"
    }
    fn description(&self) -> &'static str {
        "trips the loop's cancel flag mid-run"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> kage_core::Risk {
        kage_core::Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &kage_tools::ToolContext<'_>,
    ) -> Result<kage_core::ToolOutput, kage_tools::ToolError> {
        self.cancel.cancel();
        Ok(kage_core::ToolOutput {
            is_error: false,
            text: "cancelled".into(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

#[test]
fn cancellation_triggered_inside_tool_terminates_run_before_next_turn() {
    let call_id = kage_core::ToolCallId::new("call_1");
    let mock = MockProvider::sequence(vec![vec![
        Ok(ProviderEvent::ToolCallStart {
            id: call_id.clone(),
            name: "cancel_now".into(),
        }),
        Ok(ProviderEvent::ToolCallEnd {
            id: call_id.clone(),
            input: serde_json::json!({}),
        }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }),
    ]]);

    let mut cx = AgentContext::new("mock:m", "").with_workdir(temp_workdir("cancel_in_tool"));
    cx.history.push(Arc::new(user_msg("go")));
    let cfg = LoopConfig::default();
    let mut hooks = NoopHooks;
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new().with(std::sync::Arc::new(CancellingTool {
        cancel: cancel.clone(),
    }));

    let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
    assert!(matches!(res, Err(LoopError::Cancelled)));
    // First turn ran (provider called once); second never did.
    assert_eq!(mock.call_count(), 1);
}

#[derive(Debug)]
struct AlwaysFailTool;

impl kage_tools::Tool for AlwaysFailTool {
    fn name(&self) -> &'static str {
        "always_fail"
    }
    fn description(&self) -> &'static str {
        "always returns is_error=true"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> kage_core::Risk {
        kage_core::Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &kage_tools::ToolContext<'_>,
    ) -> Result<kage_core::ToolOutput, kage_tools::ToolError> {
        Ok(kage_core::ToolOutput {
            is_error: true,
            text: "boom".into(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

#[test]
fn doom_loop_steers_after_three_repeat_failures() {
    // Four turns: each emits the same failing tool call. After the third
    // dispatch, the doom tracker injects steering before the fourth
    // provider call.
    let mut counter = 0u32;
    let mut make_turn = || {
        counter += 1;
        let id = kage_core::ToolCallId::new(format!("call_{counter}"));
        vec![
            Ok(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name: "always_fail".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id,
                input: serde_json::json!({"x": 1}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ]
    };
    let mock = MockProvider::sequence(vec![
        make_turn(),
        make_turn(),
        make_turn(),
        // Fourth turn ends the loop without another tool call.
        vec![
            Ok(ProviderEvent::TextDelta {
                delta: "ok stopping".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ],
    ]);

    let mut cx = AgentContext::new("mock:m", "").with_workdir(temp_workdir("doom_steer"));
    cx.history.push(Arc::new(user_msg("try the thing")));
    let cfg = LoopConfig::default();
    let mut hooks = NoopHooks;
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new().with(std::sync::Arc::new(AlwaysFailTool));

    let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
    assert!(res.is_ok(), "loop failed: {res:?}");

    // The fourth provider request should have a steering user message in
    // its tail (after the last tool result).
    let req4 = mock.requests().into_iter().nth(3).unwrap();
    let tail = req4.messages.last().unwrap();
    assert_eq!(tail.role, Role::User);
    match &tail.content[0] {
        Content::Text { text } => {
            assert!(text.contains("'always_fail'"));
            assert!(text.contains("different approach"));
        }
        other => panic!("expected steering Text, got {other:?}"),
    }
}

#[test]
fn on_doom_loop_hook_can_suppress_steering() {
    struct SuppressDoom;
    impl Hooks for SuppressDoom {
        fn on_doom_loop(&mut self, _name: &str, _suggested: String) -> Option<String> {
            None
        }
    }

    let mut counter = 0u32;
    let mut make_turn = || {
        counter += 1;
        let id = kage_core::ToolCallId::new(format!("call_{counter}"));
        vec![
            Ok(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name: "always_fail".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id,
                input: serde_json::json!({"x": 1}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ]
    };
    let mock = MockProvider::sequence(vec![
        make_turn(),
        make_turn(),
        make_turn(),
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
    ]);

    let mut cx = AgentContext::new("mock:m", "").with_workdir(temp_workdir("doom_suppress"));
    cx.history.push(Arc::new(user_msg("try the thing")));
    let mut hooks = SuppressDoom;
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new().with(std::sync::Arc::new(AlwaysFailTool));

    run(
        &mock,
        &registry,
        &mut cx,
        LoopConfig::default(),
        &mut hooks,
        &cancel,
        |_| {},
    )
    .expect("loop runs");

    let req4 = mock.requests().into_iter().nth(3).unwrap();
    let has_steering = req4.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|c| matches!(c, Content::Text { text } if text.contains("different approach")))
    });
    assert!(!has_steering, "hook returning None must suppress the nudge");
}

#[derive(Debug, Default)]
struct CountingTool {
    calls: std::sync::Mutex<Vec<serde_json::Value>>,
}

impl kage_tools::Tool for CountingTool {
    fn name(&self) -> &'static str {
        "counting"
    }
    fn description(&self) -> &'static str {
        "records every input it receives"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> kage_core::Risk {
        kage_core::Risk::Read
    }
    fn execute(
        &self,
        input: serde_json::Value,
        _cx: &kage_tools::ToolContext<'_>,
    ) -> Result<kage_core::ToolOutput, kage_tools::ToolError> {
        self.calls.lock().expect("not poisoned").push(input.clone());
        Ok(kage_core::ToolOutput {
            is_error: false,
            text: format!("ran #{}", self.calls.lock().expect("not poisoned").len()),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

fn event_tag(event: &LoopEvent) -> String {
    serde_json::to_value(event).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Records hook calls and emitted events in one shared log.
#[derive(Default)]
struct OrderRecording {
    order: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}
impl Hooks for OrderRecording {
    fn before_tool_call(
        &mut self,
        _id: &kage_core::ToolCallId,
        name: &str,
        _input: &serde_json::Value,
    ) -> Option<kage_core::ToolOutput> {
        self.order.borrow_mut().push(format!("before:{name}"));
        None
    }
    fn after_tool_call(
        &mut self,
        name: &str,
        output: kage_core::ToolOutput,
    ) -> kage_core::ToolOutput {
        self.order.borrow_mut().push(format!("after:{name}"));
        output
    }
}

#[test]
fn end_to_end_event_ordering_and_hook_callbacks() {
    let call_id = kage_core::ToolCallId::new("call_e2e");
    let mock = MockProvider::sequence(vec![
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "calling tool".into(),
            }),
            Ok(ProviderEvent::ToolCallStart {
                id: call_id.clone(),
                name: "counting".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: call_id.clone(),
                input: serde_json::json!({"k": "v"}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![
            Ok(ProviderEvent::TextDelta {
                delta: "done".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ],
    ]);

    let counting = std::sync::Arc::new(CountingTool::default());
    let registry = ToolRegistry::new().with(counting.clone());

    let mut cx = AgentContext::new("mock:m", "be helpful").with_workdir(temp_workdir("events"));
    cx.history.push(Arc::new(user_msg("kick off")));
    let mut hooks = OrderRecording::default();
    let log = std::rc::Rc::clone(&hooks.order);
    let cancel = CancelFlag::new();

    let res = run(
        &mock,
        &registry,
        &mut cx,
        LoopConfig::default(),
        &mut hooks,
        &cancel,
        |event| {
            log.borrow_mut()
                .push(format!("event:{}", event_tag(&event)));
        },
    );
    assert!(res.is_ok());

    // Tool ran exactly once with the expected input.
    let recorded = counting.calls.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["k"], "v");

    // History: user, assistant(text+tool_call), tool_result, assistant(text).
    assert_eq!(cx.history.len(), 4);
    assert_eq!(cx.history[0].role, Role::User);
    assert_eq!(cx.history[1].role, Role::Assistant);
    assert_eq!(cx.history[2].role, Role::ToolResult);
    assert_eq!(cx.history[3].role, Role::Assistant);

    // Hook ordering across the whole run.
    let order = hooks.order.borrow().clone();
    let order: Vec<&str> = order.iter().map(String::as_str).collect();
    let before_pos = order
        .iter()
        .position(|s| *s == "before:counting")
        .expect("before fired");
    let after_pos = order
        .iter()
        .position(|s| *s == "after:counting")
        .expect("after fired");
    assert!(before_pos < after_pos, "before must precede after");

    let event_seq: Vec<&str> = order
        .iter()
        .filter_map(|s| s.strip_prefix("event:"))
        .collect();
    assert_eq!(
        event_seq,
        [
            "turn_started",
            "message_start",
            "text_delta",
            "tool_call_args_delta",
            "tool_call_start",
            "message_end",
            "message_appended",
            "turn_ended",
            "tool_execution_start",
            "tool_call_end",
            "message_appended",
            "turn_started",
            "message_start",
            "text_delta",
            "message_end",
            "message_appended",
            "turn_ended",
        ]
    );
    // The before/after hook must bracket the tool_call_end event.
    let tcend = order
        .iter()
        .position(|s| *s == "event:tool_call_end")
        .unwrap();
    assert!(before_pos < tcend && tcend == after_pos + 1);
}

#[test]
fn message_appended_events_mirror_history() {
    let call_id = kage_core::ToolCallId::new("call_1");
    let mock = MockProvider::sequence(vec![
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::ToolCallStart {
                id: call_id.clone(),
                name: "static".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: call_id,
                input: serde_json::json!({}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "done".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ],
    ]);
    let registry = ToolRegistry::new().with(std::sync::Arc::new(StaticTool));
    let mut cx = AgentContext::new("mock:m", "").with_workdir(temp_workdir("appended"));
    cx.history.push(Arc::new(user_msg("go")));
    let mut appended = Vec::new();

    run(
        &mock,
        &registry,
        &mut cx,
        LoopConfig::default(),
        &mut NoopHooks,
        &CancelFlag::new(),
        |ev| {
            if let LoopEvent::MessageAppended { message } = ev {
                appended.push(message);
            }
        },
    )
    .unwrap();

    assert_eq!(appended, cx.history[1..]);
    for pair in cx.history.windows(2) {
        assert_eq!(pair[1].parent, Some(pair[0].id));
    }
}

#[test]
fn end_to_end_tool_call_loop() {
    // Turn 1: model emits a tool call. Turn 2: model emits final text.
    let call_id = kage_core::ToolCallId::new("call_1");
    let mock = MockProvider::sequence(vec![
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::ToolCallStart {
                id: call_id.clone(),
                name: "static".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: call_id.clone(),
                input: serde_json::json!({}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage {
                    input: 5,
                    output: 5,
                    cache_read: 0,
                    cache_write: 0,
                },
            }),
        ],
        vec![
            Ok(ProviderEvent::TextDelta {
                delta: "all done".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input: 10,
                    output: 5,
                    cache_read: 0,
                    cache_write: 0,
                },
            }),
        ],
    ]);

    let mut cx = AgentContext::new("mock:m", "").with_workdir(temp_workdir("e2e_tool_call"));
    cx.history.push(Arc::new(user_msg("do the thing")));
    let cfg = LoopConfig::default();
    let mut hooks = NoopHooks;
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new().with(std::sync::Arc::new(StaticTool));

    let mut events = Vec::new();
    let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |ev| {
        events.push(ev);
    });
    assert!(res.is_ok(), "loop failed: {res:?}");
    assert_eq!(mock.call_count(), 2);
    assert_eq!(cx.budget.total(), 25);
    // History: user, assistant(tool_call), tool_result, assistant(final).
    assert_eq!(cx.history.len(), 4);
    assert_eq!(cx.history[1].role, Role::Assistant);
    assert_eq!(cx.history[2].role, Role::ToolResult);
    assert_eq!(cx.history[3].role, Role::Assistant);
    match &cx.history[2].content[0] {
        Content::ToolResultBlock {
            output, is_error, ..
        } => {
            assert_eq!(output, "static-result");
            assert!(!is_error);
        }
        other => panic!("expected ToolResultBlock, got {other:?}"),
    }
    // Provider request 2 should include the tool result in messages.
    let req2 = mock.requests().into_iter().nth(1).unwrap();
    assert_eq!(req2.messages.len(), 3);
    assert_eq!(req2.messages[2].role, Role::ToolResult);
    // Provider request should advertise registered tool.
    let req1 = mock.requests().into_iter().next().unwrap();
    assert_eq!(req1.tools.len(), 1);
    assert_eq!(req1.tools[0].name, "static");
}

#[derive(Default)]
struct EventLog {
    events: std::cell::RefCell<Vec<LoopEvent>>,
    cancel_on_notice: Option<CancelFlag>,
}
impl EventLog {
    fn record(&self, event: LoopEvent) {
        if let LoopEvent::ProviderRetry { .. } = event
            && let Some(c) = &self.cancel_on_notice
        {
            c.cancel();
        }
        self.events.borrow_mut().push(event);
    }

    fn notices(&self) -> usize {
        self.events
            .borrow()
            .iter()
            .filter(|e| matches!(e, LoopEvent::ProviderRetry { .. }))
            .count()
    }
}

fn transient_turn() -> Vec<Result<ProviderEvent, kage_provider::ProviderError>> {
    vec![Err(kage_provider::ProviderError::RateLimited {
        retry_after: Some(Duration::from_millis(1)),
        body: None,
    })]
}

fn good_turn() -> Vec<Result<ProviderEvent, kage_provider::ProviderError>> {
    vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::TextDelta { delta: "hi".into() }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }),
    ]
}

fn overflow_turn() -> Vec<Result<ProviderEvent, kage_provider::ProviderError>> {
    vec![Err(kage_provider::ProviderError::Http {
        status: 400,
        body: "prompt is too long: 300000 tokens > 200000 maximum".into(),
    })]
}

fn plain_assistant(text: &str) -> Message {
    Message::new(
        Role::Assistant,
        vec![Content::Text {
            text: text.to_owned(),
        }],
        None,
    )
}

fn summary_call() -> Vec<Result<ProviderEvent, kage_provider::ProviderError>> {
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

/// A turn the provider rejects for size is answered by compacting
/// harder and re-issuing a rebuilt request, not by failing the run.
#[test]
fn context_overflow_compacts_harder_and_the_turn_succeeds() {
    let mock = MockProvider::sequence(vec![overflow_turn(), summary_call(), good_turn()]);
    let mut cx = AgentContext::new("mock:m", "");
    for i in 0..12 {
        cx.history.push(Arc::new(user_msg(&format!("turn {i}"))));
        cx.history
            .push(Arc::new(plain_assistant(&format!("reply {i}"))));
    }
    let cfg = LoopConfig::default();
    let log = EventLog::default();
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let res = run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &cancel,
        |e| log.record(e),
    );
    assert!(
        res.is_ok(),
        "run should recover via compaction, got {res:?}"
    );
    assert_eq!(mock.call_count(), 3, "overflow, summarizer, retried turn");
    assert!(
        cx.history
            .iter()
            .any(|m| matches!(&m.content[0], kage_core::Content::Text { text } if text.contains("<summary>"))),
        "history must carry the synthetic summary"
    );
    assert!(!log.events.borrow().iter().any(|e| matches!(
        e,
        LoopEvent::Error {
            kind: LoopError::ContextOverflow
        }
    )));
}

/// With compaction off (`compaction_threshold: 0`), an overflowing
/// turn ends the run with `ContextOverflow` instead of compacting
/// against the user's setting.
#[test]
fn overflow_with_compaction_off_ends_with_context_overflow() {
    let mock = MockProvider::sequence(vec![overflow_turn()]);
    let mut cx = AgentContext::new("mock:m", "");
    for i in 0..12 {
        cx.history.push(Arc::new(user_msg(&format!("turn {i}"))));
        cx.history
            .push(Arc::new(plain_assistant(&format!("reply {i}"))));
    }
    let cfg = LoopConfig {
        compaction_threshold: 0.0,
        ..LoopConfig::default()
    };
    let log = EventLog::default();
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let res = run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &cancel,
        |e| log.record(e),
    );
    assert_eq!(res, Err(LoopError::ContextOverflow));
    assert_eq!(mock.call_count(), 1, "no summarizer call");
    assert_eq!(cx.history.len(), 24, "history untouched");
}

/// An automatic compaction whose summarizer fails does not end the
/// run: the failure is surfaced as an error event and the turn
/// proceeds with the full history.
#[test]
fn a_failed_auto_compaction_warns_and_the_run_continues() {
    let mock = MockProvider::sequence(vec![
        vec![Err(kage_provider::ProviderError::Auth("no key".into()))],
        good_turn(),
    ]);
    let mut cx = AgentContext::new("mock:m", "");
    for i in 0..12 {
        cx.history.push(Arc::new(user_msg(&format!("turn {i}"))));
        cx.history
            .push(Arc::new(plain_assistant(&format!("reply {i}"))));
    }
    // Over the threshold before the first turn, so the summarizer is
    // the run's first provider call.
    cx.budget.current_context = 150_000;
    cx.context_window = 200_000;
    let cfg = LoopConfig {
        compaction_threshold: 0.5,
        ..LoopConfig::default()
    };
    let log = EventLog::default();
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let res = run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &cancel,
        |e| log.record(e),
    );
    assert!(res.is_ok(), "the run must survive the failed compaction");
    assert_eq!(mock.call_count(), 2, "summarizer, then the real turn");
    assert_eq!(
        cx.history.len(),
        25,
        "nothing was compacted away; the reply appended"
    );
    assert!(
        !cx.history
            .iter()
            .any(|m| matches!(&m.content[0], Content::Text { text } if text.contains("<summary>"))),
        "no summary was spliced in"
    );
    assert!(
        log.events.borrow().iter().any(|e| matches!(
            e,
            LoopEvent::Error {
                kind: LoopError::Compaction { .. }
            }
        )),
        "the failure must be surfaced"
    );
}

/// Once the recovery rounds are spent, an overflowing request ends the
/// run with `ContextOverflow`, the variant that names the real cause.
#[test]
fn exhausted_overflow_compactions_end_with_context_overflow() {
    let mock = MockProvider::sequence(vec![
        overflow_turn(),
        summary_call(),
        overflow_turn(),
        summary_call(),
        overflow_turn(),
        summary_call(),
        overflow_turn(),
    ]);
    let mut cx = AgentContext::new("mock:m", "");
    for i in 0..12 {
        cx.history.push(Arc::new(user_msg(&format!("turn {i}"))));
        cx.history
            .push(Arc::new(plain_assistant(&format!("reply {i}"))));
    }
    let cfg = LoopConfig::default();
    let log = EventLog::default();
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let res = run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &cancel,
        |e| log.record(e),
    );
    assert_eq!(res, Err(LoopError::ContextOverflow));
    assert_eq!(
        mock.call_count(),
        7,
        "3 overflow rounds (turn + summarizer each), then the fatal turn"
    );
    assert_eq!(
        cx.history.len(),
        1,
        "the last round summarizes the whole history"
    );
}

#[test]
fn transient_provider_failure_is_retried_then_succeeds() {
    let mock = MockProvider::sequence(vec![transient_turn(), good_turn()]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    let cfg = LoopConfig::default();
    let log = EventLog::default();
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let res = run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &cancel,
        |e| log.record(e),
    );
    assert!(res.is_ok(), "run should recover, got {res:?}");
    assert_eq!(mock.call_count(), 2, "one retry after the transient fail");
    assert_eq!(log.notices(), 1, "exactly one retry notice");
    assert!(cx.history.iter().any(|m| m.role == Role::Assistant));
}

#[test]
fn non_transient_failure_is_not_retried() {
    let mock = MockProvider::replaying(vec![Err(kage_provider::ProviderError::Decode(
        "bad frame".into(),
    ))]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    let cfg = LoopConfig::default();
    let log = EventLog::default();
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let res = run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &cancel,
        |e| log.record(e),
    );
    assert!(matches!(res, Err(LoopError::Provider { .. })));
    assert_eq!(mock.call_count(), 1, "decode error must not retry");
    assert_eq!(log.notices(), 0);
}

#[test]
fn auth_failure_ends_the_run_with_auth_and_no_retry() {
    let mock = MockProvider::replaying(vec![Err(kage_provider::ProviderError::Auth(
        "token expired".into(),
    ))]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    let cfg = LoopConfig::default();
    let log = EventLog::default();
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let res = run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &cancel,
        |e| log.record(e),
    );
    assert_eq!(
        res,
        Err(LoopError::Auth {
            message: "token expired".into()
        })
    );
    assert_eq!(mock.call_count(), 1, "auth error must not retry");
    assert_eq!(log.notices(), 0);
}

#[test]
fn retries_are_bounded_then_surface_the_error() {
    let mock = MockProvider::sequence(vec![transient_turn(), transient_turn()]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    let cfg = LoopConfig {
        max_provider_retries: 1,
        ..LoopConfig::default()
    };
    let log = EventLog::default();
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let res = run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &cancel,
        |e| log.record(e),
    );
    assert!(matches!(
        res,
        Err(LoopError::RateLimited {
            retry_after_secs: Some(0),
            ..
        })
    ));
    assert_eq!(mock.call_count(), 2, "initial attempt + one bounded retry");
    assert_eq!(log.notices(), 1);
}

#[test]
fn an_exhausted_rate_limit_carries_the_upstream_body() {
    let json = "{\"error\":{\"message\":\"quota exceeded, retries today: 4\"}}";
    let rate_limited = vec![Err(kage_provider::ProviderError::RateLimited {
        retry_after: None,
        body: Some(json.to_owned()),
    })];
    let mock = MockProvider::sequence(vec![rate_limited]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    let cfg = LoopConfig {
        max_provider_retries: 0,
        ..LoopConfig::default()
    };
    let log = EventLog::default();
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let res = run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &cancel,
        |e| log.record(e),
    );
    assert_eq!(
        res,
        Err(LoopError::RateLimited {
            message: json.to_owned(),
            retry_after_secs: None,
        }),
        "the upstream body must reach the user verbatim"
    );
}

#[test]
fn steering_message_is_announced_then_lands_in_history() {
    #[derive(Default)]
    struct Steerer {
        queued: Vec<String>,
    }
    impl Hooks for Steerer {
        fn get_steering(&mut self) -> Option<Vec<Content>> {
            if self.queued.is_empty() {
                None
            } else {
                Some(text_steering(self.queued.remove(0)))
            }
        }
    }

    let mock = MockProvider::sequence(vec![good_turn(), good_turn()]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("first")));
    let cfg = LoopConfig::default();
    let mut hooks = Steerer {
        queued: vec!["mid-run nudge".into()],
    };
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new();

    let mut announced = Vec::new();
    run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |e| {
        if let LoopEvent::MessageAppended { message } = e
            && message.role == Role::User
        {
            announced.push(message);
        }
    })
    .unwrap();

    let texts: Vec<&str> = announced
        .iter()
        .filter_map(|m| match m.content.first() {
            Some(Content::Text { text }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        ["mid-run nudge"],
        "the steering text must be announced as an appended user message",
    );
    let last_user = cx
        .history
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .expect("steering must produce a user message in history");
    let last_text = last_user.content.iter().find_map(|c| match c {
        Content::Text { text } => Some(text.as_str()),
        _ => None,
    });
    assert_eq!(last_text, Some("mid-run nudge"));
}

#[test]
fn provider_retry_event_surfaces_server_retry_after() {
    let rate_limited = vec![Err(kage_provider::ProviderError::RateLimited {
        retry_after: Some(std::time::Duration::from_secs(300)),
        body: None,
    })];
    let mock = MockProvider::sequence(vec![rate_limited, good_turn()]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    // Cancel during the (capped) backoff so the test does not actually
    // sleep 60s while still letting the ProviderRetry event fire.
    let cancel = CancelFlag::new();
    let log = EventLog {
        events: std::cell::RefCell::default(),
        cancel_on_notice: Some(cancel.clone()),
    };
    let cfg = LoopConfig::default();
    let registry = ToolRegistry::new();

    let _ = run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &cancel,
        |e| log.record(e),
    );

    let retry = log
        .events
        .borrow()
        .iter()
        .find_map(|e| match e {
            LoopEvent::ProviderRetry {
                wait_secs,
                requested_secs,
                ..
            } => Some((*wait_secs, *requested_secs)),
            _ => None,
        })
        .expect("loop should have emitted ProviderRetry");
    let (wait, requested) = retry;
    assert!(wait <= 60, "wait should be capped at 60s, got {wait}");
    assert_eq!(
        requested,
        Some(300),
        "server-requested delay must survive in requested_secs",
    );
}

#[test]
fn sub_second_backoff_reports_milliseconds() {
    let mock = MockProvider::sequence(vec![transient_turn(), good_turn()]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    let cfg = LoopConfig::default();
    let cancel = CancelFlag::new();
    let log = EventLog {
        events: std::cell::RefCell::default(),
        cancel_on_notice: None,
    };
    let registry = ToolRegistry::new();

    let _ = run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &cancel,
        |e| log.record(e),
    );

    let (wait_secs, wait_ms) = log
        .events
        .borrow()
        .iter()
        .find_map(|e| match e {
            LoopEvent::ProviderRetry {
                wait_secs, wait_ms, ..
            } => Some((*wait_secs, *wait_ms)),
            _ => None,
        })
        .expect("loop should have emitted ProviderRetry");
    assert_eq!(
        wait_secs, 0,
        "a 1ms backoff must not round up to a whole second"
    );
    assert_eq!(wait_ms, 1);
}

#[test]
fn cancel_during_backoff_aborts_cleanly() {
    let mock = MockProvider::sequence(vec![transient_turn(), good_turn()]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    let cfg = LoopConfig::default();
    let cancel = CancelFlag::new();
    let log = EventLog {
        events: std::cell::RefCell::default(),
        cancel_on_notice: Some(cancel.clone()),
    };
    let registry = ToolRegistry::new();

    let res = run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut NoopHooks,
        &cancel,
        |e| log.record(e),
    );
    assert!(matches!(res, Err(LoopError::Cancelled)));
    assert_eq!(mock.call_count(), 1, "retry never issued after cancel");
}

#[test]
fn provider_error_after_cancel_ends_the_run_as_cancelled() {
    let mock = MockProvider::sequence(vec![vec![
        Ok(ProviderEvent::MessageStart),
        Err(kage_provider::ProviderError::Transport("cancelled".into())),
    ]]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    let cancel = CancelFlag::new();
    let res = run(
        &mock,
        &ToolRegistry::new(),
        &mut cx,
        LoopConfig::default(),
        &mut NoopHooks,
        &cancel,
        |e| {
            if matches!(e, LoopEvent::MessageStart { .. }) {
                cancel.cancel();
            }
        },
    );
    assert!(matches!(res, Err(LoopError::Cancelled)), "{res:?}");
}

/// Tool that always reports cancellation, like a host interrupt mid-run.
#[derive(Debug)]
struct CancelTool;

impl kage_tools::Tool for CancelTool {
    fn name(&self) -> &'static str {
        "cancel"
    }
    fn description(&self) -> &'static str {
        "always reports cancellation"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> kage_core::Risk {
        kage_core::Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &kage_tools::ToolContext<'_>,
    ) -> Result<kage_core::ToolOutput, kage_tools::ToolError> {
        Err(kage_tools::ToolError::Cancelled)
    }
}

#[test]
fn cancel_mid_dispatch_answers_every_tool_call_in_history() {
    let call_id = kage_core::ToolCallId::new("call_1");
    let mock = MockProvider::replaying(vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::ToolCallStart {
            id: call_id.clone(),
            name: "cancel".into(),
        }),
        Ok(ProviderEvent::ToolCallEnd {
            id: call_id.clone(),
            input: serde_json::json!({}),
        }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }),
    ]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("go")));
    let cfg = LoopConfig::default();
    let mut hooks = NoopHooks;
    let cancel = CancelFlag::new();
    let registry = ToolRegistry::new().with(std::sync::Arc::new(CancelTool));

    let mut events = Vec::new();
    let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |ev| {
        events.push(ev);
    });
    assert!(matches!(res, Err(LoopError::Cancelled)));

    // The assistant message carrying the tool_use must be answered by a
    // tool_result, or a resumed session would send a dangling tool_use.
    let tool_uses = cx
        .history
        .iter()
        .flat_map(|m| &m.content)
        .filter(|c| matches!(c, Content::ToolCall { .. }))
        .count();
    let tool_results = cx
        .history
        .iter()
        .flat_map(|m| &m.content)
        .filter(|c| matches!(c, Content::ToolResultBlock { .. }))
        .count();
    assert_eq!(tool_uses, 1);
    assert_eq!(
        tool_results, 1,
        "cancelled tool call must still be answered"
    );

    let answered_is_error = cx.history.iter().rev().find_map(|m| match &m.content[0] {
        Content::ToolResultBlock { is_error, .. } => Some(*is_error),
        _ => None,
    });
    assert_eq!(answered_is_error, Some(true));
    assert!(matches!(
        events.last(),
        Some(kage_core::LoopEvent::Error {
            kind: kage_core::LoopError::Cancelled,
        })
    ));
}

#[test]
fn sleep_cancelable_wakes_when_cancelled_from_another_thread() {
    let cancel = CancelFlag::new();
    let flag = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        flag.cancel();
    });
    let start = std::time::Instant::now();
    assert!(!sleep_cancelable(&cancel, Duration::from_secs(60)));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn sleep_cancelable_returns_true_after_an_uncancelled_wait() {
    assert!(sleep_cancelable(
        &CancelFlag::new(),
        Duration::from_millis(20)
    ));
}

/// A context whose workdir is relative (the historical `.` fallback for
/// a failing `current_dir`) must fail the run instead of silently making
/// whatever the process CWD later is the session root.
#[test]
fn run_rejects_a_relative_workdir_instead_of_silently_using_it() {
    let mock = MockProvider::replaying(vec![]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.workdir = std::path::PathBuf::from(".");
    cx.history.push(Arc::new(user_msg("hi")));
    let cancel = CancelFlag::new();

    let mut errors = Vec::new();
    let res = run(
        &mock,
        &ToolRegistry::new(),
        &mut cx,
        LoopConfig::default(),
        &mut NoopHooks,
        &cancel,
        |ev| {
            if let LoopEvent::Error { kind } = ev {
                errors.push(kind);
            }
        },
    );
    match res {
        Err(LoopError::Other { message }) => {
            assert!(message.contains("not absolute"), "{message}");
        }
        other => panic!("expected a workdir error, got {other:?}"),
    }
    assert_eq!(errors.len(), 1);
    assert_eq!(mock.call_count(), 0, "provider never called");
}

#[test]
fn an_absolute_workdir_runs_normally() {
    let mock = MockProvider::replaying(vec![Ok(ProviderEvent::MessageEnd {
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    })]);
    let mut cx = AgentContext::new("mock:m", "").with_workdir(temp_workdir("absolute"));
    cx.history.push(Arc::new(user_msg("hi")));

    let res = run(
        &mock,
        &ToolRegistry::new(),
        &mut cx,
        LoopConfig::default(),
        &mut NoopHooks,
        &CancelFlag::new(),
        |_| {},
    );
    assert!(res.is_ok());
    assert_eq!(mock.call_count(), 1);
}

/// The opt-in `max_turns` failsafe stops the run with an error after the
/// cap, answering any pending tool calls so history carries no dangling
/// `tool_use`.
#[test]
fn max_turns_stops_the_run_after_the_cap() {
    let call_id = kage_core::ToolCallId::new("call_1");
    let mock = MockProvider::sequence(vec![
        good_turn(),
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::ToolCallStart {
                id: call_id.clone(),
                name: "static".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: call_id.clone(),
                input: serde_json::json!({}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
    ]);
    let mut cx = AgentContext::new("mock:m", "").with_workdir(temp_workdir("max_turns"));
    cx.history.push(Arc::new(user_msg("go")));
    let cfg = LoopConfig {
        max_turns: Some(2),
        ..LoopConfig::default()
    };
    let registry = ToolRegistry::new().with(std::sync::Arc::new(StaticTool));
    let mut hooks = AlwaysFollowup;

    let res = run(
        &mock,
        &registry,
        &mut cx,
        cfg,
        &mut hooks,
        &CancelFlag::new(),
        |_| {},
    );
    match res {
        Err(LoopError::Other { message }) => {
            assert!(message.contains("turn limit"), "{message}");
        }
        other => panic!("expected a turn-limit error, got {other:?}"),
    }
    assert_eq!(mock.call_count(), 2, "the third turn never ran");
    // The pending call of the capped turn is answered synthetically.
    let answered = cx
        .history
        .iter()
        .flat_map(|m| &m.content)
        .any(|c| matches!(c, Content::ToolResultBlock { is_error: true, .. }));
    assert!(answered, "the pending tool call must be answered");
}

#[test]
fn max_turns_none_keeps_the_run_going() {
    let mock = MockProvider::sequence(vec![good_turn(), good_turn()]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hi")));
    let cfg = LoopConfig::default();
    let mut hooks = OneFollowup(true);

    let res = run(
        &mock,
        &ToolRegistry::new(),
        &mut cx,
        cfg,
        &mut hooks,
        &CancelFlag::new(),
        |_| {},
    );
    assert!(res.is_ok(), "default config has no iteration cap");
    assert_eq!(mock.call_count(), 2);
}

/// An always-producing hook must not hang the drain: the All-mode cap
/// stops it, appends a truncation note, and drops the overflow.
#[test]
fn drain_messages_stops_at_the_cap_and_appends_a_note() {
    let cancel = CancelFlag::new();
    let mut polls = 0u32;
    let text = drain_messages(SteeringMode::All, &cancel, || {
        polls += 1;
        Some(format!("m{}", polls - 1))
    })
    .unwrap()
    .expect("an always-producing hook yields text");
    assert_eq!(polls, 65, "the cap stops the poll loop");
    assert!(text.starts_with("m0"));
    assert!(text.contains("\nm63"), "64 messages made it in: {text}");
    assert!(!text.contains("\nm64"), "the overflow is dropped");
    assert!(text.contains("stopped draining after 64"));
}

#[test]
fn drain_messages_polls_cancel_between_messages() {
    let cancel = CancelFlag::new();
    let mut polls = 0u32;
    let res = drain_messages(SteeringMode::All, &cancel, || {
        polls += 1;
        if polls == 3 {
            cancel.cancel();
        }
        Some(format!("m{polls}"))
    });
    assert!(matches!(res, Err(LoopError::Cancelled)));
    assert_eq!(polls, 3, "cancel trips out of a never-ending drain");
}

/// End to end: a 100-message steering queue in All mode drains 64 on the
/// first turn (with the truncation note) and the rest on the next.
#[test]
fn steering_all_mode_caps_the_drain_per_turn() {
    struct BigQueue(std::collections::VecDeque<String>);
    impl Hooks for BigQueue {
        fn get_steering(&mut self) -> Option<Vec<Content>> {
            self.0.pop_front().map(text_steering)
        }
    }
    /// Splits steering and followup across two hook values, since a
    /// single `Hooks` impl cannot borrow two mutable states here.
    struct SplitHooks<'a> {
        steering: &'a mut BigQueue,
        followup: &'a mut OneShotFollowup,
    }
    impl Hooks for SplitHooks<'_> {
        fn get_steering(&mut self) -> Option<Vec<Content>> {
            self.steering.get_steering()
        }
        fn get_followup(&mut self) -> Option<String> {
            self.followup.get_followup()
        }
    }
    let mock = MockProvider::sequence(vec![good_turn(), good_turn()]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("go")));
    let cfg = LoopConfig {
        steering_mode: SteeringMode::All,
        ..LoopConfig::default()
    };
    let queue: std::collections::VecDeque<String> = (0..100).map(|i| format!("note {i}")).collect();
    let mut steering = BigQueue(queue);
    let mut followup = OneShotFollowup {
        text: Some("ignored".into()),
    };
    let mut hooks = SplitHooks {
        steering: &mut steering,
        followup: &mut followup,
    };

    run(
        &mock,
        &ToolRegistry::new(),
        &mut cx,
        cfg,
        &mut hooks,
        &CancelFlag::new(),
        |_| {},
    )
    .unwrap();

    let requests = mock.requests().into_iter().collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    // The last note-prefixed message: earlier drains stay in the
    // history, so every request after the first also carries them.
    let steering_text = |req: &kage_provider::StreamRequest| {
        req.messages
            .iter()
            .filter_map(|m| match m.content.first() {
                Some(Content::Text { text }) if text.starts_with("note ") => Some(text.clone()),
                _ => None,
            })
            .next_back()
            .expect("a drained steering message")
    };
    let first = steering_text(&requests[0]);
    assert!(first.contains("note 0"));
    assert!(first.contains("note 63"));
    assert!(first.contains("stopped draining after 64"));
    let second = steering_text(&requests[1]);
    assert!(second.contains("note 99"));
    assert!(!second.contains("stopped draining"), "{second}");
}

/// Pins the retry contract: the retried turn opens a new message id and
/// streams from scratch, so receivers must discard everything since the
/// failed attempt's `MessageStart`.
#[test]
fn provider_retry_uses_a_fresh_message_id_and_replays_no_deltas() {
    let mock = MockProvider::sequence(vec![
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "partial ".into(),
            }),
            Err(kage_provider::ProviderError::RateLimited {
                retry_after: Some(Duration::from_millis(1)),
                body: None,
            }),
        ],
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "complete".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ],
    ]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("hello")));

    let mut events = Vec::new();
    run(
        &mock,
        &ToolRegistry::new(),
        &mut cx,
        LoopConfig::default(),
        &mut NoopHooks,
        &CancelFlag::new(),
        |ev| events.push(ev),
    )
    .unwrap();

    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::MessageStart { id } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 2, "one start per attempt");
    assert_ne!(starts[0], starts[1], "the retry must reuse no message id");
    let second_attempt_deltas: Vec<&str> = events
        .iter()
        .skip_while(|e| !matches!(e, LoopEvent::ProviderRetry { .. }))
        .filter_map(|e| match e {
            LoopEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        second_attempt_deltas,
        ["complete"],
        "the retry streams the full turn, no dropped-attempt deltas replay"
    );
}
