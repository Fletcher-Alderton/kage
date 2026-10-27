//! Tests for tool-call dispatch.

use std::sync::Arc;

use kage_core::{Risk, ToolCallId};
use kage_tools::Tool;

use super::*;
use crate::AgentContext;
use crate::NoopHooks;
use crate::test_support::{Meet, MeetTool};

#[derive(Debug)]
struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echo input back"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            is_error: false,
            text: input.to_string(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

#[derive(Debug)]
struct ErrTool;

impl Tool for ErrTool {
    fn name(&self) -> &'static str {
        "err"
    }
    fn description(&self) -> &'static str {
        "always fail"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Other("planned failure".into()))
    }
}

#[derive(Debug)]
struct ProgressTool;

impl Tool for ProgressTool {
    fn name(&self) -> &'static str {
        "progress"
    }
    fn description(&self) -> &'static str {
        "emits two progress updates before returning"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        cx.update(ToolUpdate {
            content: "step 1".into(),
            structured: None,
        });
        cx.update(ToolUpdate {
            content: "step 2".into(),
            structured: Some(serde_json::json!({"phase": "done"})),
        });
        Ok(ToolOutput {
            is_error: false,
            text: "ok".into(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

/// Emits one update, then waits until the test signals that the dispatcher
/// delivered an event. Fails if the signal only comes after it returns.
#[derive(Debug)]
struct WaitsForDeliveryTool {
    delivered: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl Tool for WaitsForDeliveryTool {
    fn name(&self) -> &'static str {
        "waits"
    }
    fn description(&self) -> &'static str {
        "blocks until its progress update was emitted"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        cx.update(ToolUpdate {
            content: "working".into(),
            structured: None,
        });
        let delivered = self
            .delivered
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok();
        Ok(ToolOutput {
            is_error: !delivered,
            text: if delivered { "live" } else { "buffered" }.into(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

/// Replies with the call id its context carries.
#[derive(Debug)]
struct CallIdTool;

impl Tool for CallIdTool {
    fn name(&self) -> &'static str {
        "call_id"
    }
    fn description(&self) -> &'static str {
        "reply with the call id"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            is_error: false,
            text: cx.call_id().map(ToString::to_string).unwrap_or_default(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

fn registry_with_echo() -> ToolRegistry {
    ToolRegistry::new()
        .with(Arc::new(EchoTool))
        .with(Arc::new(ErrTool))
        .with(Arc::new(ProgressTool))
        .with(Arc::new(AloneTool))
}

/// Stands in for a tool that must own its message, like `swarm`.
#[derive(Debug)]
struct AloneTool;

impl Tool for AloneTool {
    fn name(&self) -> &'static str {
        "swarm"
    }
    fn description(&self) -> &'static str {
        "must run alone"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Exec
    }
    fn runs_alone(&self) -> bool {
        true
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            is_error: false,
            text: "ran".into(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

#[test]
fn a_tool_that_runs_alone_is_refused_beside_others() {
    let tools = registry_with_echo();
    let calls = vec![
        pending("echo", serde_json::json!({"n": 1})),
        pending("swarm", serde_json::json!({})),
        pending("echo", serde_json::json!({"n": 2})),
    ];
    let outputs = |outcome: DispatchOutcome| -> Vec<(bool, String)> {
        outcome
            .results
            .iter()
            .filter_map(|m| match &m.content[0] {
                Content::ToolResultBlock {
                    output, is_error, ..
                } => Some((*is_error, output.clone())),
                _ => None,
            })
            .collect()
    };
    let cancel = CancelFlag::new();
    let workdir = std::path::Path::new("/tmp");

    let sequential = dispatch_tool_calls(
        calls.clone(),
        &tools,
        workdir,
        &cancel,
        false,
        MessageId::new(),
        &mut NoopHooks,
        &mut |_| {},
    );
    assert!(matches!(&outputs(sequential)[..], [
        (false, first),
        (true, swarm),
        (false, second)
    ] if first.contains('1') && second.contains('2')
        && swarm.contains("only tool call in the message")));

    let parallel = dispatch_tool_calls_parallel(
        calls,
        &tools,
        workdir,
        &cancel,
        false,
        MessageId::new(),
        &mut NoopHooks,
        &mut |_| {},
    );
    assert!(matches!(&outputs(parallel)[..], [
        (false, _),
        (true, swarm),
        (false, _)
    ] if swarm.contains("only tool call in the message")));
}

#[test]
fn a_tool_that_runs_alone_runs_by_itself() {
    let tools = registry_with_echo();
    let outcome = dispatch_tool_calls(
        vec![pending("swarm", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &CancelFlag::new(),
        false,
        MessageId::new(),
        &mut NoopHooks,
        &mut |_| {},
    );
    assert_eq!(outcome.error, None);
    assert_eq!(outputs_of(&outcome), ["ran".to_owned()]);
}

fn outputs_of(outcome: &DispatchOutcome) -> Vec<String> {
    outcome
        .results
        .iter()
        .filter_map(|m| match &m.content[0] {
            Content::ToolResultBlock { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

fn pending(name: &str, input: serde_json::Value) -> PendingToolCall {
    PendingToolCall {
        id: ToolCallId::new(format!("call_{name}")),
        name: name.to_owned(),
        input,
    }
}

#[test]
fn tools_see_their_own_call_id() {
    let tools = ToolRegistry::new().with(Arc::new(CallIdTool));
    let calls = || {
        ["first", "second"].map(|id| PendingToolCall {
            id: ToolCallId::new(id),
            name: "call_id".to_owned(),
            input: serde_json::json!({}),
        })
    };
    let outputs = |outcome: DispatchOutcome| -> Vec<String> {
        outcome
            .results
            .iter()
            .filter_map(|m| match &m.content[0] {
                Content::ToolResultBlock { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect()
    };
    let cancel = CancelFlag::new();
    let workdir = std::path::Path::new("/tmp");

    let sequential = dispatch_tool_calls(
        calls().into(),
        &tools,
        workdir,
        &cancel,
        false,
        MessageId::new(),
        &mut NoopHooks,
        &mut |_| {},
    );
    assert_eq!(outputs(sequential), ["first", "second"]);

    let parallel = dispatch_tool_calls_parallel(
        calls().into(),
        &tools,
        workdir,
        &cancel,
        false,
        MessageId::new(),
        &mut NoopHooks,
        &mut |_| {},
    );
    assert_eq!(outputs(parallel), ["first", "second"]);
}

#[test]
fn tool_updates_are_emitted_before_tool_call_end() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;
    let mut emitted = Vec::new();

    let outcome = dispatch_tool_calls(
        vec![pending("progress", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    );
    assert_eq!(outcome.results.len(), 1);

    let updates: Vec<_> = emitted
        .iter()
        .filter_map(|e| match e {
            LoopEvent::ToolUpdate { update, .. } => Some(update.content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(updates, vec!["step 1".to_owned(), "step 2".to_owned()]);

    let update_idx = emitted
        .iter()
        .position(|e| matches!(e, LoopEvent::ToolUpdate { .. }))
        .unwrap();
    let end_idx = emitted
        .iter()
        .position(|e| matches!(e, LoopEvent::ToolCallEnd { .. }))
        .unwrap();
    assert!(
        update_idx < end_idx,
        "ToolUpdate must fire before ToolCallEnd"
    );
}

#[test]
fn unknown_tool_error_names_the_registered_tools() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;
    let mut emitted = Vec::new();

    let outcome = dispatch_tool_calls(
        vec![pending("websearch", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    );
    assert_eq!(outcome.results.len(), 1);
    let Some(Content::ToolResultBlock {
        output, is_error, ..
    }) = outcome.results[0].content.first()
    else {
        panic!("expected tool result: {:?}", outcome.results[0].content);
    };
    assert!(*is_error);
    assert!(
        output.contains("tool 'websearch' is not registered"),
        "{output}"
    );
    assert!(
        output.contains("Available tools: echo, err, progress"),
        "{output}"
    );
}

#[test]
fn tool_updates_are_emitted_while_the_tool_runs() {
    let (tx, rx) = std::sync::mpsc::channel();
    let tools = ToolRegistry::new().with(Arc::new(WaitsForDeliveryTool {
        delivered: std::sync::Mutex::new(rx),
    }));
    let cancel = CancelFlag::new();
    let mut hooks = NoopHooks;

    let outcome = dispatch_tool_calls(
        vec![pending("waits", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        MessageId::new(),
        &mut hooks,
        &mut |ev| {
            if matches!(ev, LoopEvent::ToolUpdate { .. }) {
                let _ = tx.send(());
            }
        },
    );

    assert!(matches!(
        outcome.results[0].content[0],
        Content::ToolResultBlock {
            is_error: false,
            ..
        }
    ));
}

#[test]
fn parallel_dispatch_emits_tool_updates_per_call() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;
    let mut emitted = Vec::new();

    let outcome = dispatch_tool_calls_parallel(
        vec![
            pending("progress", serde_json::json!({})),
            pending("progress", serde_json::json!({})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    );
    assert_eq!(outcome.results.len(), 2);

    let updates = emitted
        .iter()
        .filter(|e| matches!(e, LoopEvent::ToolUpdate { .. }))
        .count();
    assert_eq!(updates, 4, "two tools x two updates each");
}

#[test]
fn dispatches_in_input_order_and_appends_results() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let pendings = vec![
        pending("echo", serde_json::json!({"n": 1})),
        pending("echo", serde_json::json!({"n": 2})),
    ];
    let mut hooks = NoopHooks;
    let mut emitted = Vec::new();

    let results = dispatch_tool_calls(
        pendings,
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    )
    .results;
    assert_eq!(results.len(), 2);
    for result in &results {
        assert_eq!(result.role, Role::ToolResult);
        assert_eq!(result.parent, Some(parent));
    }
    // Order: result 1 corresponds to first pending.
    if let Content::ToolResultBlock { output, .. } = &results[0].content[0] {
        assert!(output.contains("\"n\":1"));
    }
    // Two ToolCallEnd events emitted.
    let ends = emitted
        .iter()
        .filter(|e| matches!(e, LoopEvent::ToolCallEnd { .. }))
        .count();
    assert_eq!(ends, 2);
}

#[test]
fn unknown_tool_yields_error_output_not_loop_failure() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let results = dispatch_tool_calls(
        vec![pending("does_not_exist", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    assert_eq!(results.len(), 1);
    match &results[0].content[0] {
        Content::ToolResultBlock {
            is_error, output, ..
        } => {
            assert!(*is_error);
            assert!(output.contains("does_not_exist"));
        }
        other => panic!("unexpected content: {other:?}"),
    }
}

#[test]
fn tool_error_converts_to_error_output() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let results = dispatch_tool_calls(
        vec![pending("err", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    assert_eq!(results.len(), 1);
    if let Content::ToolResultBlock {
        is_error, output, ..
    } = &results[0].content[0]
    {
        assert!(*is_error);
        assert!(output.contains("planned failure"));
    }
}

#[test]
fn before_tool_call_can_short_circuit_execution() {
    struct Allowlist;
    impl Hooks for Allowlist {
        fn before_tool_call(
            &mut self,
            _id: &kage_core::ToolCallId,
            name: &str,
            _input: &serde_json::Value,
        ) -> Option<ToolOutput> {
            if name == "err" {
                Some(ToolOutput {
                    is_error: true,
                    text: "blocked by host policy".into(),
                    structured: None,
                    terminate: false,
                    media: Vec::new(),
                })
            } else {
                None
            }
        }
    }

    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = Allowlist;

    let results = dispatch_tool_calls(
        vec![pending("err", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    assert_eq!(results.len(), 1);
    match &results[0].content[0] {
        Content::ToolResultBlock {
            output, is_error, ..
        } => {
            assert!(*is_error);
            assert_eq!(output, "blocked by host policy");
        }
        other => panic!("unexpected content: {other:?}"),
    }
}

#[test]
fn after_tool_call_can_rewrite_output() {
    struct Redact;
    impl Hooks for Redact {
        fn after_tool_call(&mut self, _name: &str, mut output: ToolOutput) -> ToolOutput {
            output.text = format!("[redacted] {}", output.text);
            output
        }
    }

    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = Redact;

    let results = dispatch_tool_calls(
        vec![pending("echo", serde_json::json!({"x": 1}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    match &results[0].content[0] {
        Content::ToolResultBlock { output, .. } => {
            assert!(output.starts_with("[redacted] "));
        }
        other => panic!("unexpected content: {other:?}"),
    }
}

#[derive(Debug)]
struct SleepTool {
    millis: u64,
}

impl Tool for SleepTool {
    fn name(&self) -> &'static str {
        "sleep"
    }
    fn description(&self) -> &'static str {
        "sleep for a fixed duration"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        std::thread::sleep(std::time::Duration::from_millis(self.millis));
        Ok(ToolOutput {
            is_error: false,
            text: format!("slept_{}", self.millis),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

#[test]
fn parallel_dispatch_preserves_input_order() {
    let tools = ToolRegistry::new()
        .with(Arc::new(SleepTool { millis: 50 }))
        .with(Arc::new(EchoTool));
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let pendings = vec![
        pending("sleep", serde_json::json!({})),
        pending("echo", serde_json::json!({"x": 1})),
        pending("sleep", serde_json::json!({})),
    ];

    let results = dispatch_tool_calls_parallel(
        pendings,
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    assert_eq!(results.len(), 3);
    assert!(matches!(
        &results[0].content[0],
        Content::ToolResultBlock { output, .. } if output == "slept_50"
    ));
    assert!(matches!(
        &results[1].content[0],
        Content::ToolResultBlock { output, .. } if output.contains("\"x\":1")
    ));
    assert!(matches!(
        &results[2].content[0],
        Content::ToolResultBlock { output, .. } if output == "slept_50"
    ));
}

#[test]
fn parallel_dispatch_actually_runs_concurrently() {
    let tools = ToolRegistry::new().with(Arc::new(MeetTool {
        name: "meet",
        mode: None,
        meet: Meet::new(3, std::time::Duration::from_secs(5)),
    }));
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let pendings: Vec<_> = (0..3)
        .map(|i| pending(&format!("call_{i}"), serde_json::json!({})))
        .map(|mut c| {
            c.name = "meet".to_owned();
            c
        })
        .collect();

    let results = dispatch_tool_calls_parallel(
        pendings,
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;

    assert_eq!(results.len(), 3);
    for result in &results {
        assert!(matches!(
            &result.content[0],
            Content::ToolResultBlock { output, .. } if output == "met"
        ));
    }
}

#[test]
fn parallel_dispatch_honors_before_tool_call_short_circuit() {
    struct BlockSecond {
        seen: u32,
    }
    impl Hooks for BlockSecond {
        fn before_tool_call(
            &mut self,
            _id: &kage_core::ToolCallId,
            _name: &str,
            _input: &serde_json::Value,
        ) -> Option<ToolOutput> {
            self.seen = self.seen.saturating_add(1);
            if self.seen == 2 {
                Some(ToolOutput {
                    is_error: true,
                    text: "blocked".into(),
                    structured: None,
                    terminate: false,
                    media: Vec::new(),
                })
            } else {
                None
            }
        }
    }

    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = BlockSecond { seen: 0 };

    let pendings = vec![
        pending("echo", serde_json::json!({"i": 0})),
        pending("echo", serde_json::json!({"i": 1})),
        pending("echo", serde_json::json!({"i": 2})),
    ];

    let results = dispatch_tool_calls_parallel(
        pendings,
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    assert_eq!(results.len(), 3);
    if let Content::ToolResultBlock {
        output, is_error, ..
    } = &results[1].content[0]
    {
        assert!(*is_error);
        assert_eq!(output, "blocked");
    }
}

#[test]
fn cancelled_batch_synthesizes_error_results() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    cancel.cancel();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;
    let mut emitted = Vec::new();

    let outcome = dispatch_tool_calls(
        vec![pending("echo", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    );
    assert_eq!(outcome.error, Some(LoopError::Cancelled));
    assert_eq!(outcome.results.len(), 1);
    match &outcome.results[0].content[0] {
        Content::ToolResultBlock {
            is_error,
            output,
            call_id,
        } => {
            assert!(*is_error);
            assert!(output.contains("cancelled"));
            assert_eq!(*call_id, ToolCallId::new("call_echo"));
        }
        other => panic!("unexpected content: {other:?}"),
    }
    let ends = emitted
        .iter()
        .filter(|e| matches!(e, LoopEvent::ToolCallEnd { .. }))
        .count();
    assert_eq!(ends, 1, "ToolCallEnd must still fire for the aborted call");
}

/// Tool that always reports cancellation, like a host interrupt mid-run.
#[derive(Debug)]
struct CancelTool;

impl Tool for CancelTool {
    fn name(&self) -> &'static str {
        "cancel"
    }
    fn description(&self) -> &'static str {
        "always reports cancellation"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Cancelled)
    }
}

#[test]
fn mid_batch_cancel_synthesizes_remaining_results() {
    let tools = ToolRegistry::new()
        .with(Arc::new(EchoTool))
        .with(Arc::new(CancelTool));
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;
    let mut emitted = Vec::new();

    let outcome = dispatch_tool_calls(
        vec![
            pending("echo", serde_json::json!({"i": 0})),
            pending("cancel", serde_json::json!({})),
            pending("echo", serde_json::json!({"i": 2})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    );
    assert_eq!(outcome.error, Some(LoopError::Cancelled));
    assert_eq!(outcome.results.len(), 3);

    let as_block = |msg: &Message| match &msg.content[0] {
        Content::ToolResultBlock {
            call_id,
            output,
            is_error,
        } => (call_id.clone(), output.clone(), *is_error),
        other => panic!("unexpected content: {other:?}"),
    };
    let (id0, out0, err0) = as_block(&outcome.results[0]);
    assert!(!err0, "call before the cancel keeps its real output");
    assert!(out0.contains("\"i\":0"));
    let (_, out1, err1) = as_block(&outcome.results[1]);
    assert!(err1 && out1.contains("cancelled"));
    let (_, out2, err2) = as_block(&outcome.results[2]);
    assert!(err2 && out2.contains("cancelled"));
    assert_eq!(id0, ToolCallId::new("call_echo"));

    let ends = emitted
        .iter()
        .filter(|e| matches!(e, LoopEvent::ToolCallEnd { .. }))
        .count();
    assert_eq!(ends, 3, "every call in the batch gets a ToolCallEnd");
}

/// Tool that panics, standing in for a tool bug blowing up its thread.
#[derive(Debug)]
struct PanicTool;

impl Tool for PanicTool {
    fn name(&self) -> &'static str {
        "panic"
    }
    fn description(&self) -> &'static str {
        "always panics"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        panic!("boom");
    }
}

#[test]
fn parallel_batch_keeps_successful_outputs_when_one_panics() {
    let tools = ToolRegistry::new()
        .with(Arc::new(EchoTool))
        .with(Arc::new(PanicTool));
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let outcome = dispatch_tool_calls_parallel(
        vec![
            pending("echo", serde_json::json!({"i": 0})),
            pending("panic", serde_json::json!({})),
            pending("echo", serde_json::json!({"i": 2})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    );
    assert!(matches!(outcome.error, Some(LoopError::Other { .. })));
    assert_eq!(outcome.results.len(), 3);
    let as_block = |msg: &Message| match &msg.content[0] {
        Content::ToolResultBlock {
            output, is_error, ..
        } => (output.clone(), *is_error),
        other => panic!("unexpected content: {other:?}"),
    };
    let (out0, err0) = as_block(&outcome.results[0]);
    assert!(
        !err0 && out0.contains("\"i\":0"),
        "panic must not discard sibling outputs"
    );
    let (_, err1) = as_block(&outcome.results[1]);
    assert!(err1);
    let (out2, err2) = as_block(&outcome.results[2]);
    assert!(!err2 && out2.contains("\"i\":2"));
}

/// A panicking tool in a sequential batch must surface the panic. The
/// synthetic cancel for the calls that did not run is bookkeeping, not
/// the cause, and must not overwrite the concrete error.
#[test]
fn sequential_batch_reports_the_panic_not_a_synthetic_cancel() {
    let tools = ToolRegistry::new()
        .with(Arc::new(EchoTool))
        .with(Arc::new(PanicTool));
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let outcome = dispatch_tool_calls(
        vec![
            pending("echo", serde_json::json!({"i": 0})),
            pending("panic", serde_json::json!({})),
            pending("echo", serde_json::json!({"i": 2})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    );
    assert_eq!(
        outcome.error,
        Some(LoopError::Other {
            message: "tool thread panicked".into()
        })
    );
    assert_eq!(outcome.results.len(), 3);
    let as_block = |msg: &Message| match &msg.content[0] {
        Content::ToolResultBlock {
            output, is_error, ..
        } => (output.clone(), *is_error),
        other => panic!("unexpected content: {other:?}"),
    };
    let (out0, err0) = as_block(&outcome.results[0]);
    assert!(
        !err0 && out0.contains("\"i\":0"),
        "panic must not discard earlier outputs"
    );
    let (_, err1) = as_block(&outcome.results[1]);
    assert!(err1);
    let (out2, err2) = as_block(&outcome.results[2]);
    assert!(err2);
    assert!(out2.contains("tool did not run"), "{out2}");
}

#[test]
fn parallel_cancelled_call_synthesizes_and_keeps_others() {
    let tools = ToolRegistry::new()
        .with(Arc::new(EchoTool))
        .with(Arc::new(CancelTool));
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let outcome = dispatch_tool_calls_parallel(
        vec![
            pending("echo", serde_json::json!({"i": 0})),
            pending("cancel", serde_json::json!({})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    );
    assert_eq!(outcome.error, Some(LoopError::Cancelled));
    assert_eq!(outcome.results.len(), 2);
    let as_block = |msg: &Message| match &msg.content[0] {
        Content::ToolResultBlock {
            output, is_error, ..
        } => (output.clone(), *is_error),
        other => panic!("unexpected content: {other:?}"),
    };
    let (out0, err0) = as_block(&outcome.results[0]);
    assert!(!err0 && out0.contains("\"i\":0"));
    let (out1, err1) = as_block(&outcome.results[1]);
    assert!(err1 && out1.contains("cancelled"));
}

/// Tool that reports whether its context is confined, standing in for
/// the path-confinement plumbing the dispatcher must propagate.
#[derive(Debug)]
struct ConfineProbe;

impl Tool for ConfineProbe {
    fn name(&self) -> &'static str {
        "confine_probe"
    }
    fn description(&self) -> &'static str {
        "reports context confinement"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            is_error: false,
            text: cx.is_confined().to_string(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

#[test]
fn dispatch_propagates_confine_flag_to_tool_context() {
    let tools = ToolRegistry::new().with(Arc::new(ConfineProbe));
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let results = dispatch_tool_calls(
        vec![pending("confine_probe", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    match &results[0].content[0] {
        Content::ToolResultBlock { output, .. } => assert_eq!(output, "false"),
        other => panic!("unexpected content: {other:?}"),
    }

    let results = dispatch_tool_calls_parallel(
        vec![pending("confine_probe", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        true,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    match &results[0].content[0] {
        Content::ToolResultBlock { output, .. } => assert_eq!(output, "true"),
        other => panic!("unexpected content: {other:?}"),
    }
}

/// Short-circuits every `err` call.
struct BlockErr;

impl Hooks for BlockErr {
    fn before_tool_call(
        &mut self,
        _id: &kage_core::ToolCallId,
        name: &str,
        _input: &serde_json::Value,
    ) -> Option<ToolOutput> {
        (name == "err").then(|| ToolOutput {
            is_error: true,
            text: "blocked".into(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

/// The ids of `ToolExecutionStart` and `ToolCallEnd` events, tagged.
fn execution_trace(events: &[LoopEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::ToolExecutionStart { id } => Some(format!("start {id}")),
            LoopEvent::ToolCallEnd { id, .. } => Some(format!("end {id}")),
            _ => None,
        })
        .collect()
}

#[test]
fn execution_start_marks_each_call_right_before_it_runs() {
    let tools = registry_with_echo();
    let mut emitted = Vec::new();
    dispatch_tool_calls(
        vec![
            pending("echo", serde_json::json!({})),
            pending("err", serde_json::json!({})),
            pending("progress", serde_json::json!({})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &CancelFlag::new(),
        false,
        MessageId::new(),
        &mut BlockErr,
        &mut |ev| emitted.push(ev),
    );
    assert_eq!(
        execution_trace(&emitted),
        [
            "start call_echo",
            "end call_echo",
            "end call_err",
            "start call_progress",
            "end call_progress",
        ]
    );
}

#[test]
fn parallel_execution_start_skips_short_circuited_calls() {
    let tools = registry_with_echo();
    let mut emitted = Vec::new();
    dispatch_tool_calls_parallel(
        vec![
            pending("echo", serde_json::json!({})),
            pending("err", serde_json::json!({})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &CancelFlag::new(),
        false,
        MessageId::new(),
        &mut BlockErr,
        &mut |ev| emitted.push(ev),
    );
    assert_eq!(
        execution_trace(&emitted),
        ["end call_err", "start call_echo", "end call_echo"]
    );
}

#[test]
fn parallel_dispatch_ends_each_call_when_it_finishes() {
    let (tx, rx) = std::sync::mpsc::channel();
    let tools = ToolRegistry::new()
        .with(Arc::new(WaitsForDeliveryTool {
            delivered: std::sync::Mutex::new(rx),
        }))
        .with(Arc::new(EchoTool));
    let mut emitted = Vec::new();

    let outcome = dispatch_tool_calls_parallel(
        vec![
            pending("waits", serde_json::json!({})),
            pending("echo", serde_json::json!({"i": 1})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &CancelFlag::new(),
        false,
        MessageId::new(),
        &mut NoopHooks,
        &mut |ev| {
            if matches!(&ev, LoopEvent::ToolCallEnd { id, .. } if id.to_string() == "call_echo") {
                let _ = tx.send(());
            }
            emitted.push(ev);
        },
    );

    assert_eq!(
        execution_trace(&emitted),
        [
            "start call_waits",
            "start call_echo",
            "end call_echo",
            "end call_waits",
        ]
    );
    let blocks: Vec<_> = outcome
        .results
        .iter()
        .map(|m| match &m.content[0] {
            Content::ToolResultBlock {
                call_id,
                output,
                is_error,
            } => (call_id.to_string(), output.clone(), *is_error),
            other => panic!("unexpected content: {other:?}"),
        })
        .collect();
    assert_eq!(blocks[0], ("call_waits".into(), "live".into(), false));
    assert_eq!(blocks[1].0, "call_echo");
    assert!(!blocks[1].2 && blocks[1].1.contains("\"i\":1"));
    assert!(
        outcome.results[1].ts < outcome.results[0].ts,
        "each result is stamped when its call finishes"
    );
}

#[test]
fn parallel_panic_yields_an_error_for_its_own_call() {
    let tools = ToolRegistry::new()
        .with(Arc::new(EchoTool))
        .with(Arc::new(ErrTool))
        .with(Arc::new(PanicTool));
    let mut emitted = Vec::new();

    let outcome = dispatch_tool_calls_parallel(
        vec![
            pending("err", serde_json::json!({})),
            pending("panic", serde_json::json!({})),
            pending("echo", serde_json::json!({"i": 2})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &CancelFlag::new(),
        false,
        MessageId::new(),
        &mut BlockErr,
        &mut |ev| emitted.push(ev),
    );

    assert!(matches!(outcome.error, Some(LoopError::Other { .. })));
    let blocks: Vec<_> = outcome
        .results
        .iter()
        .map(|m| match &m.content[0] {
            Content::ToolResultBlock {
                call_id,
                output,
                is_error,
            } => (call_id.to_string(), output.clone(), *is_error),
            other => panic!("unexpected content: {other:?}"),
        })
        .collect();
    assert_eq!(blocks[0], ("call_err".into(), "blocked".into(), true));
    assert_eq!(blocks[1].0, "call_panic");
    assert!(blocks[1].2 && blocks[1].1.contains("panicked"));
    assert_eq!(blocks[2].0, "call_echo");
    assert!(!blocks[2].2 && blocks[2].1.contains("\"i\":2"));

    let panic_end = emitted.iter().find_map(|e| match e {
        LoopEvent::ToolCallEnd { id, output } if id.to_string() == "call_panic" => Some(output),
        _ => None,
    });
    assert!(panic_end.is_some_and(|output| output.is_error));
}

/// Tool that resolves `../x` through its context, standing in for the
/// built-in file tools.
#[derive(Debug)]
struct EscapeProbe;

impl Tool for EscapeProbe {
    fn name(&self) -> &'static str {
        "escape_probe"
    }
    fn description(&self) -> &'static str {
        "resolves a workdir-escaping path"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let resolved = cx.resolve_path(Path::new("../x"))?;
        Ok(ToolOutput {
            is_error: false,
            text: resolved.display().to_string(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

/// A default `AgentContext` confines dispatch, so a model-supplied `../x`
/// path surfaces as a `ToolError::Path` rejection instead of a resolved
/// escape.
#[test]
fn confined_dispatch_rejects_workdir_escape() {
    let root = std::env::temp_dir().join("kage-loop-dispatch-confine");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let tools = ToolRegistry::new().with(Arc::new(EscapeProbe));
    let cx = AgentContext::new("m", "").with_workdir(&work);
    assert!(cx.confine_paths, "a default AgentContext confines paths");

    let sequential = dispatch_tool_calls(
        vec![pending("escape_probe", serde_json::json!({}))],
        &tools,
        cx.workdir.as_path(),
        &CancelFlag::new(),
        cx.confine_paths,
        MessageId::new(),
        &mut NoopHooks,
        &mut |_| {},
    );
    assert!(sequential.error.is_none());
    match &sequential.results[0].content[0] {
        Content::ToolResultBlock {
            output, is_error, ..
        } => {
            assert!(*is_error);
            assert!(output.contains("escapes workdir"), "{output}");
        }
        other => panic!("unexpected content: {other:?}"),
    }

    let parallel = dispatch_tool_calls_parallel(
        vec![pending("escape_probe", serde_json::json!({}))],
        &tools,
        cx.workdir.as_path(),
        &CancelFlag::new(),
        cx.confine_paths,
        MessageId::new(),
        &mut NoopHooks,
        &mut |_| {},
    );
    assert!(parallel.error.is_none());
    match &parallel.results[0].content[0] {
        Content::ToolResultBlock {
            output, is_error, ..
        } => {
            assert!(*is_error);
            assert!(output.contains("escapes workdir"), "{output}");
        }
        other => panic!("unexpected content: {other:?}"),
    }
}

/// Sleeps for a fixed duration and deliberately never polls the cancel
/// flag, standing in for a plugin or MCP tool stuck in a tight loop.
#[derive(Debug)]
struct SleepsIgnoringCancel {
    millis: u64,
}

impl Tool for SleepsIgnoringCancel {
    fn name(&self) -> &'static str {
        "ignores_cancel"
    }
    fn description(&self) -> &'static str {
        "sleeps without ever polling cancel"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(self.millis);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(ToolOutput {
            is_error: false,
            text: "finished".into(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

/// A non-cooperative tool must not park the dispatch: cancel trips out of
/// the wait and every unfinished call gets a synthesized Cancelled result.
#[test]
fn cancelable_wait_aborts_a_parallel_batch_that_ignores_cancel() {
    let tools = ToolRegistry::new().with(Arc::new(SleepsIgnoringCancel { millis: 5_000 }));
    let cancel = CancelFlag::new();
    let canceller = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        canceller.cancel();
    });
    let mut hooks = NoopHooks;
    let start = std::time::Instant::now();
    let outcome = dispatch_tool_calls_parallel(
        vec![
            pending("ignores_cancel", serde_json::json!({"i": 0})),
            pending("ignores_cancel", serde_json::json!({"i": 1})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        MessageId::new(),
        &mut hooks,
        &mut |_| {},
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "dispatch parked on a tool that ignores cancel: {elapsed:?}"
    );
    assert_eq!(outcome.error, Some(LoopError::Cancelled));
    assert_eq!(outcome.results.len(), 2);
    for result in &outcome.results {
        match &result.content[0] {
            Content::ToolResultBlock {
                output, is_error, ..
            } => {
                assert!(*is_error);
                assert!(output.contains("cancelled"), "{output}");
            }
            other => panic!("unexpected content: {other:?}"),
        }
    }
}

/// Same guarantee for the sequential path: the running call is abandoned
/// and the not-yet-started call is answered without running.
#[test]
fn cancelable_wait_aborts_a_sequential_batch_that_ignores_cancel() {
    let tools = ToolRegistry::new().with(Arc::new(SleepsIgnoringCancel { millis: 5_000 }));
    let cancel = CancelFlag::new();
    let canceller = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        canceller.cancel();
    });
    let mut hooks = NoopHooks;
    let start = std::time::Instant::now();
    let outcome = dispatch_tool_calls(
        vec![
            pending("ignores_cancel", serde_json::json!({"i": 0})),
            pending("ignores_cancel", serde_json::json!({"i": 1})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        MessageId::new(),
        &mut hooks,
        &mut |_| {},
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "dispatch parked on a tool that ignores cancel: {elapsed:?}"
    );
    assert_eq!(outcome.error, Some(LoopError::Cancelled));
    assert_eq!(outcome.results.len(), 2);
    assert!(outcome.results.iter().all(|m| matches!(
        &m.content[0],
        Content::ToolResultBlock { is_error: true, .. }
    )));
}

/// Counts concurrent executions and the peak, standing in for a host
/// auditing thread usage.
#[derive(Debug)]
struct ConcurrencyProbe {
    active: Arc<std::sync::atomic::AtomicUsize>,
    peak: Arc<std::sync::atomic::AtomicUsize>,
}

impl Tool for ConcurrencyProbe {
    fn name(&self) -> &'static str {
        "probe"
    }
    fn description(&self) -> &'static str {
        "tracks its concurrency peak"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        use std::sync::atomic::Ordering;
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(30));
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolOutput {
            is_error: false,
            text: "done".into(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

/// An oversized batch runs in waves on the bounded pool: the concurrency
/// peak never exceeds the worker count and every result keeps input order.
#[test]
fn worker_pool_bounds_concurrent_tool_threads() {
    use std::sync::atomic::Ordering;
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tools = ToolRegistry::new().with(Arc::new(ConcurrencyProbe {
        active: Arc::clone(&active),
        peak: Arc::clone(&peak),
    }));
    let calls: Vec<PendingToolCall> = (0..32)
        .map(|i| PendingToolCall {
            id: ToolCallId::new(format!("call_{i}")),
            name: "probe".to_owned(),
            input: serde_json::json!({"i": i}),
        })
        .collect();

    let outcome = dispatch_tool_calls_parallel(
        calls,
        &tools,
        std::path::Path::new("/tmp"),
        &CancelFlag::new(),
        false,
        MessageId::new(),
        &mut NoopHooks,
        &mut |_| {},
    );
    assert!(outcome.error.is_none());
    assert_eq!(outcome.results.len(), 32);
    assert!(
        peak.load(Ordering::SeqCst) <= MAX_TOOL_WORKERS,
        "peak concurrency {} exceeded the pool",
        peak.load(Ordering::SeqCst)
    );
    for (i, result) in outcome.results.iter().enumerate() {
        match &result.content[0] {
            Content::ToolResultBlock {
                call_id,
                output,
                is_error,
            } => {
                assert_eq!(call_id.to_string(), format!("call_{i}"), "input order");
                assert!(!*is_error && output == "done");
            }
            other => panic!("unexpected content: {other:?}"),
        }
    }
}

/// Emits far more updates than the progress channel holds, standing in
/// for a chatty build tool against a slow host sink.
#[derive(Debug)]
struct ChattyTool;

impl Tool for ChattyTool {
    fn name(&self) -> &'static str {
        "chatty"
    }
    fn description(&self) -> &'static str {
        "emits one thousand updates"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        for i in 0..1_000 {
            cx.update(ToolUpdate {
                content: format!("step {i}"),
                structured: None,
            });
        }
        Ok(ToolOutput {
            is_error: false,
            text: "done".into(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

/// The bounded progress channel applies backpressure instead of growing
/// the queue: a slow host still receives every update, in order.
#[test]
fn bounded_progress_channel_delivers_every_update_under_a_slow_host() {
    let tools = ToolRegistry::new().with(Arc::new(ChattyTool));
    let mut updates = Vec::new();
    let outcome = dispatch_tool_calls(
        vec![pending("chatty", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &CancelFlag::new(),
        false,
        MessageId::new(),
        &mut NoopHooks,
        &mut |ev| {
            if let LoopEvent::ToolUpdate { update, .. } = ev {
                updates.push(update.content);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        },
    );
    assert!(outcome.error.is_none());
    assert_eq!(updates.len(), 1_000, "every update is delivered");
    assert_eq!(updates[0], "step 0");
    assert_eq!(updates[999], "step 999");
}

/// Returns structured output sized by the `bytes` input, standing in for
/// a tool returning bulky JSON alongside its text.
#[derive(Debug)]
struct StructuredTool;

impl Tool for StructuredTool {
    fn name(&self) -> &'static str {
        "structured"
    }
    fn description(&self) -> &'static str {
        "returns structured output of a requested size"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let size = usize::try_from(input["bytes"].as_u64().unwrap_or(0)).unwrap_or(usize::MAX);
        Ok(ToolOutput {
            is_error: false,
            text: "text".into(),
            structured: Some(serde_json::json!({ "blob": "x".repeat(size) })),
            terminate: false,
            media: Vec::new(),
        })
    }
}

/// Structured output over the tool-result cap is dropped from the
/// `ToolCallEnd` event; the persisted result message carries only the
/// capped text either way.
#[test]
fn oversized_structured_output_is_dropped_from_the_end_event() {
    let tools = ToolRegistry::new().with(Arc::new(StructuredTool));
    let big = serde_json::json!({"bytes": kage_core::MAX_TOOL_RESULT_BYTES * 2});
    let small = serde_json::json!({"bytes": 8});

    let mut big_events = Vec::new();
    let big_outcome = dispatch_tool_calls(
        vec![pending("structured", big)],
        &tools,
        std::path::Path::new("/tmp"),
        &CancelFlag::new(),
        false,
        MessageId::new(),
        &mut NoopHooks,
        &mut |ev| big_events.push(ev),
    );
    let mut small_events = Vec::new();
    let small_outcome = dispatch_tool_calls_parallel(
        vec![pending("structured", small)],
        &tools,
        std::path::Path::new("/tmp"),
        &CancelFlag::new(),
        false,
        MessageId::new(),
        &mut NoopHooks,
        &mut |ev| small_events.push(ev),
    );

    let big_end = big_events.iter().find_map(|e| match e {
        LoopEvent::ToolCallEnd { output, .. } => Some(output.clone()),
        _ => None,
    });
    let big_structured = big_end.as_ref().and_then(|o| o.structured.clone());
    assert_eq!(big_structured, None, "over-cap structured must be dropped");
    assert_eq!(big_end.expect("a ToolCallEnd").text, "text");

    let small_end = small_events.iter().find_map(|e| match e {
        LoopEvent::ToolCallEnd { output, .. } => Some(output.clone()),
        _ => None,
    });
    assert!(
        small_end.and_then(|o| o.structured).is_some(),
        "under-cap structured survives"
    );

    // History carries only the text, for both sizes and both paths.
    for outcome in [big_outcome, small_outcome] {
        assert_eq!(outcome.results.len(), 1);
        match &outcome.results[0].content[0] {
            Content::ToolResultBlock {
                output, is_error, ..
            } => {
                assert_eq!(output, "text");
                assert!(!*is_error);
            }
            other => panic!("unexpected content: {other:?}"),
        }
    }
}

#[test]
fn a_tool_s_images_follow_its_result_in_history() {
    let output = ToolOutput {
        text: "drawn".into(),
        media: vec![
            kage_core::ToolMedia {
                mime: "image/png".into(),
                data: "iVBO".into(),
            },
            kage_core::ToolMedia {
                mime: "audio/wav".into(),
                data: "UklG".into(),
            },
        ],
        ..ToolOutput::default()
    };
    let message = result_message(ToolCallId::new("c1"), output, MessageId::new());
    assert_eq!(message.role, Role::ToolResult);
    assert!(matches!(
        &message.content[0],
        Content::ToolResultBlock { output, .. }
            if output == "drawn\n[audio/wav output is not kept]"
    ));
    assert_eq!(
        message.content[1],
        Content::Image {
            source: MediaSource::Base64 {
                data: "iVBO".into()
            },
            mime: "image/png".into(),
        }
    );
    assert_eq!(message.content.len(), 2);
}

#[test]
fn a_tool_s_audio_is_named_in_the_text_and_its_image_is_kept() {
    let output = ToolOutput {
        text: "made media".into(),
        media: vec![
            kage_core::ToolMedia {
                mime: "image/png".into(),
                data: "iVBO".into(),
            },
            kage_core::ToolMedia {
                mime: "audio/mpeg".into(),
                data: "SUQz".into(),
            },
        ],
        ..ToolOutput::default()
    };
    let message = result_message(ToolCallId::new("c1"), output, MessageId::new());
    assert_eq!(message.role, Role::ToolResult);
    assert!(matches!(
        &message.content[0],
        Content::ToolResultBlock { output, .. }
            if output == "made media\n[audio/mpeg output is not kept]"
    ));
    assert_eq!(
        message.content[1],
        Content::Image {
            source: MediaSource::Base64 {
                data: "iVBO".into()
            },
            mime: "image/png".into(),
        }
    );
    assert_eq!(message.content.len(), 2);
}
