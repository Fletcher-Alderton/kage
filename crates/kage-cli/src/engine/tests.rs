use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use kage_core::Content;
use kage_core::agent_report::{AgentLimit, AgentMail, AgentReport, ReportState};
use kage_core::agents::{AgentDefs, Isolation};
use kage_core::permissions::{PermissionAction, PermissionsConfig};
use kage_core::protocol::{EXIT_PLAN_TOOL, Envelope, Event, RunOutcome};
use kage_core::{LoopError, LoopEvent, StopReason, TokenUsage, ToolCallId, ToolOutput};
use kage_provider::testing::MockProvider;
use kage_provider::{ProviderError, ProviderEvent};
use kage_session::{EntryId, FORMAT_VERSION, Header, SessionWriter};
use kage_tools::{Tool, ToolContext, ToolError};

use super::shell::run_shell;
use super::*;

const WAIT: Duration = Duration::from_secs(20);

fn text_turn(text: &str) -> Vec<Result<ProviderEvent, ProviderError>> {
    vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::TextDelta { delta: text.into() }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input: 10,
                output: 2,
                ..TokenUsage::default()
            },
        }),
    ]
}

fn tool_turn(tool: &str) -> Vec<Result<ProviderEvent, ProviderError>> {
    let id = ToolCallId::new(format!("call_{tool}"));
    vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: tool.into(),
        }),
        Ok(ProviderEvent::ToolCallEnd {
            id,
            input: serde_json::json!({}),
        }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }),
    ]
}

/// Blocks until the test releases it or the run is cancelled.
#[derive(Debug)]
struct Gate {
    /// Registered tool name, so a test can host several independent
    /// gates.
    name: &'static str,
    release: Mutex<Receiver<()>>,
}

impl Tool for Gate {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &'static str {
        "waits for the test"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> kage_core::Risk {
        kage_core::Risk::Read
    }
    fn execution_mode(&self) -> Option<kage_tools::ExecMode> {
        Some(kage_tools::ExecMode::Parallel)
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let release = lock(&self.release);
        loop {
            if cx.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            if release.recv_timeout(Duration::from_millis(10)).is_ok() {
                return Ok(ToolOutput {
                    text: "released".into(),
                    ..ToolOutput::default()
                });
            }
        }
    }
}

struct Harness {
    engine: Engine,
    events: Receiver<Envelope>,
    release: mpsc::Sender<()>,
    tools: ToolRegistry,
}

fn harness(mock: MockProvider) -> Harness {
    harness_on(ProviderRegistry::new().with(Arc::new(mock)))
}

fn harness_on(registry: ProviderRegistry) -> Harness {
    let (release, release_rx) = channel();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Gate {
        name: "gate",
        release: Mutex::new(release_rx),
    }));
    let (tx, events) = channel();
    let collector: Subscriber = Box::new(move |envelope| {
        let _ = tx.send(envelope.clone());
    });
    let engine = Engine::start(Arc::new(registry));
    engine.subscribe(collector);
    Harness {
        engine,
        events,
        release,
        tools,
    }
}

impl Harness {
    fn open(&self, id: SessionId, recorder: Option<Recorder>) {
        self.open_with(
            id,
            recorder,
            PermissionGate::new(PermissionsConfig::default()),
        );
    }

    fn open_with(&self, id: SessionId, recorder: Option<Recorder>, gate: PermissionGate) {
        self.engine.open(SessionSpec {
            recorder,
            gate,
            ..self.spec(id)
        });
    }

    fn spec(&self, id: SessionId) -> SessionSpec {
        SessionSpec {
            id,
            model: "mock/m".into(),
            cx: AgentContext::new("m", "").with_workdir(std::env::temp_dir()),
            recorder: None,
            tools: self.tools.clone(),
            plugins: None,
            gate: PermissionGate::new(PermissionsConfig::default()),
            loop_cfg: LoopConfig::default(),
            mcp: None,
            interactive: true,
            title: false,
            agents: None,
            shell: None,
        }
    }
}

fn prompt(engine: &Engine, session: SessionId, text: &str, delivery: Delivery) {
    engine.send(Command::to(
        session,
        CommandKind::Prompt {
            content: vec![Content::Text { text: text.into() }],
            delivery,
        },
    ));
}

fn prompt_content(engine: &Engine, session: SessionId, content: Vec<Content>, delivery: Delivery) {
    engine.send(Command::to(
        session,
        CommandKind::Prompt { content, delivery },
    ));
}

/// Collect envelopes until `count` runs have ended.
fn until_runs_end(events: &Receiver<Envelope>, count: usize) -> Vec<Envelope> {
    let mut seen = Vec::new();
    let mut ended = 0;
    while ended < count {
        let envelope = events.recv_timeout(WAIT).expect("engine stalled");
        if matches!(envelope.event, Event::Host(HostEvent::RunEnded { .. })) {
            ended += 1;
        }
        seen.push(envelope);
    }
    seen
}

fn wait_for(events: &Receiver<Envelope>, pred: impl Fn(&Envelope) -> bool) -> Vec<Envelope> {
    let mut seen: Vec<Envelope> = Vec::new();
    loop {
        let Ok(envelope) = events.recv_timeout(WAIT) else {
            let recent: Vec<String> = seen
                .iter()
                .rev()
                .take(5)
                .map(|e| format!("{:?}", e.event))
                .collect();
            panic!("engine stalled; last events: {recent:?}")
        };
        let done = pred(&envelope);
        seen.push(envelope);
        if done {
            return seen;
        }
    }
}

fn appended_texts(events: &[Envelope]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Loop(LoopEvent::MessageAppended { message }) => {
                Some(crate::cli_loop_run::first_user_text(message))
            }
            _ => None,
        })
        .collect()
}

fn outcomes(events: &[Envelope]) -> Vec<RunOutcome> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Host(HostEvent::RunEnded { outcome }) => Some(outcome.clone()),
            _ => None,
        })
        .collect()
}

fn is_tool_start(envelope: &Envelope) -> bool {
    matches!(envelope.event, Event::Loop(LoopEvent::ToolCallStart { .. }))
}

/// A recorder writing `<dir>/<id>.jsonl`.
fn recorder_in(dir: &std::path::Path, id: SessionId) -> (Recorder, std::path::PathBuf) {
    let path = dir.join(format!("{id}.jsonl"));
    let writer = SessionWriter::create(
        &path,
        Header {
            version: FORMAT_VERSION,
            session: id,
            id: EntryId::new(),
            ts: chrono::Utc::now(),
            cwd: "/tmp".into(),
            model: "mock/m".into(),
            system_prompt: String::new(),
            parent_session: None,
            parent_entry: None,
        },
    )
    .unwrap();
    (Recorder::new(writer, None), path)
}

fn host_events(events: &[Envelope]) -> Vec<&HostEvent> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Host(host) => Some(host),
            Event::Loop(_) => None,
        })
        .collect()
}

fn notices(events: &[Envelope]) -> Vec<String> {
    host_events(events)
        .into_iter()
        .filter_map(|e| match e {
            HostEvent::Notice { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn is_notice(envelope: &Envelope) -> bool {
    matches!(envelope.event, Event::Host(HostEvent::Notice { .. }))
}

fn is_session_changed(envelope: &Envelope) -> bool {
    matches!(
        envelope.event,
        Event::Host(HostEvent::SessionChanged { .. })
    )
}

#[test]
fn prompt_runs_and_records_the_history() {
    let dir = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    let h = harness(MockProvider::replaying(text_turn("hello")));
    h.open(id, Some(recorder));
    prompt(&h.engine, id, "hi", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    h.engine.shutdown();

    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
    let usage = events.iter().rev().find_map(|e| match &e.event {
        Event::Host(HostEvent::UsageUpdated { usage }) => Some(*usage),
        _ => None,
    });
    assert_eq!(usage.unwrap().total.input, 10);

    let replay = kage_session::replay(&path).unwrap();
    assert_eq!(replay.history.len(), 2);
    assert_eq!(replay.history[1].parent, Some(replay.history[0].id));
    assert_eq!(replay.usage_total.input, 10);
}

#[test]
fn steered_prompt_is_delivered_at_the_next_turn() {
    let h = harness(MockProvider::sequence(vec![
        tool_turn("gate"),
        text_turn("done"),
    ]));
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "start", Delivery::Steer);
    wait_for(&h.events, is_tool_start);
    prompt(&h.engine, id, "also this", Delivery::Steer);
    std::thread::sleep(Duration::from_millis(50));
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 1);

    let roles: Vec<(Role, String)> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Loop(LoopEvent::MessageAppended { message }) => {
                Some((message.role, crate::cli_loop_run::first_user_text(message)))
            }
            _ => None,
        })
        .collect();
    let tool_result = roles.iter().position(|(r, _)| *r == Role::ToolResult);
    let steered = roles.iter().position(|(_, t)| t == "also this");
    assert!(
        steered > tool_result,
        "steer lands after the tool result: {roles:?}"
    );
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
}

/// A prompt carrying an image steers like a text prompt: it lands in
/// history after the in-flight tool result, not after the whole run.
#[test]
fn steered_image_prompt_is_delivered_at_the_next_turn() {
    let mock = MockProvider::sequence(vec![tool_turn("gate"), text_turn("done")]);
    let h = harness(mock.clone());
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "start", Delivery::Steer);
    wait_for(&h.events, is_tool_start);
    prompt_content(
        &h.engine,
        id,
        vec![
            Content::Text {
                text: "look at this".into(),
            },
            Content::Image {
                source: kage_core::MediaSource::Base64 {
                    data: "aGk=".to_owned(),
                },
                mime: "image/png".to_owned(),
            },
        ],
        Delivery::Steer,
    );
    std::thread::sleep(Duration::from_millis(50));
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 1);

    let roles: Vec<(Role, bool)> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Loop(LoopEvent::MessageAppended { message }) => Some((
                message.role,
                message
                    .content
                    .iter()
                    .any(|c| matches!(c, Content::Image { .. })),
            )),
            _ => None,
        })
        .collect();
    let tool_result = roles.iter().position(|(r, _)| *r == Role::ToolResult);
    let steered = roles
        .iter()
        .position(|(r, has_image)| *r == Role::User && *has_image);
    assert!(
        steered > tool_result,
        "steered image prompt lands after the tool result: {roles:?}"
    );
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
    // The image reached the provider request too.
    let request = mock.last_request().unwrap();
    assert!(request.messages.iter().any(|m| {
        m.role == Role::User && m.content.iter().any(|c| matches!(c, Content::Image { .. }))
    }));
}

#[test]
fn queued_prompt_starts_a_new_run() {
    let h = harness(MockProvider::sequence(vec![
        tool_turn("gate"),
        text_turn("first"),
        text_turn("second"),
    ]));
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "one", Delivery::Steer);
    wait_for(&h.events, is_tool_start);
    prompt(&h.engine, id, "two", Delivery::Queue);
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 2);

    assert_eq!(
        outcomes(&events),
        [RunOutcome::Completed, RunOutcome::Completed]
    );
    let run_ends = events
        .iter()
        .position(|e| matches!(e.event, Event::Host(HostEvent::RunEnded { .. })))
        .unwrap();
    assert!(appended_texts(&events[run_ends..]).contains(&"two".to_owned()));
}

#[test]
fn withdraw_prompt_pops_the_newest_of_its_queue() {
    let h = harness(MockProvider::sequence(vec![
        tool_turn("gate"),
        text_turn("done"),
    ]));
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "start", Delivery::Steer);
    wait_for(&h.events, is_tool_start);
    prompt(&h.engine, id, "first", Delivery::Steer);
    prompt(&h.engine, id, "second", Delivery::Steer);
    prompt(&h.engine, id, "queued", Delivery::Queue);
    std::thread::sleep(Duration::from_millis(50));

    h.engine.send(Command::to(
        id,
        CommandKind::WithdrawPrompt {
            delivery: Delivery::Steer,
        },
    ));
    h.engine.send(Command::to(
        id,
        CommandKind::WithdrawPrompt {
            delivery: Delivery::Queue,
        },
    ));
    h.engine.send(Command::to(
        id,
        CommandKind::WithdrawPrompt {
            delivery: Delivery::Queue,
        },
    ));
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 1);

    let withdrawn: Vec<Option<Vec<Content>>> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Host(HostEvent::PromptWithdrawn { content, .. }) => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        withdrawn,
        [
            Some(vec![Content::Text {
                text: "second".into()
            }]),
            Some(vec![Content::Text {
                text: "queued".into()
            }]),
            None,
        ],
        "newest first, and an empty queue answers None"
    );
    let texts = appended_texts(&events);
    assert!(texts.contains(&"first".to_owned()), "{texts:?}");
    assert!(!texts.contains(&"second".to_owned()), "{texts:?}");
    assert!(!texts.contains(&"queued".to_owned()), "{texts:?}");
}

#[test]
fn cancel_ends_the_run_as_cancelled() {
    let h = harness(MockProvider::replaying(tool_turn("gate")));
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "go", Delivery::Steer);
    wait_for(&h.events, is_tool_start);
    h.engine.send(Command::to(id, CommandKind::Cancel));
    let events = until_runs_end(&h.events, 1);
    assert_eq!(outcomes(&events), [RunOutcome::Cancelled]);
}

#[test]
fn shutdown_stops_a_running_session() {
    let h = harness(MockProvider::replaying(tool_turn("gate")));
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "go", Delivery::Steer);
    wait_for(&h.events, is_tool_start);

    let (done_tx, done_rx) = channel();
    std::thread::spawn(move || {
        h.engine.shutdown();
        let _ = done_tx.send(());
    });
    done_rx.recv_timeout(WAIT).expect("shutdown hung");
}

#[test]
fn sessions_are_sequenced_independently() {
    let h = harness(MockProvider::replaying(text_turn("ok")));
    let (a, b) = (SessionId::new(), SessionId::new());
    h.open(a, None);
    h.open(b, None);
    prompt(&h.engine, a, "a", Delivery::Steer);
    prompt(&h.engine, b, "b", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);

    for session in [a, b] {
        let seqs: Vec<u64> = events
            .iter()
            .filter(|e| e.session == session)
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    }
}

fn ask_for_gate() -> PermissionGate {
    let mut rules = PermissionsConfig::default();
    rules.tools.insert(
        "gate".into(),
        kage_core::permissions::ToolPermissionRules {
            default: PermissionAction::Ask,
            allow: Vec::new(),
            deny: Vec::new(),
        },
    );
    PermissionGate::new(rules)
}

fn permission_request(events: &Receiver<Envelope>) -> (RequestId, Option<ToolCallId>) {
    let seen = wait_for(events, |e| {
        matches!(e.event, Event::Host(HostEvent::PermissionRequested { .. }))
    });
    match &seen.last().unwrap().event {
        Event::Host(HostEvent::PermissionRequested {
            request_id,
            tool_call_id,
            ..
        }) => (*request_id, tool_call_id.clone()),
        _ => unreachable!(),
    }
}

#[test]
fn permission_asks_travel_over_the_bus() {
    let h = harness(MockProvider::sequence(vec![
        tool_turn("gate"),
        text_turn("done"),
    ]));
    let id = SessionId::new();
    h.open_with(id, None, ask_for_gate());
    prompt(&h.engine, id, "go", Delivery::Steer);

    let (request_id, call_id) = permission_request(&h.events);
    assert_eq!(call_id, Some(ToolCallId::new("call_gate")));
    h.engine.send(Command::to(
        id,
        CommandKind::ResolvePermission {
            request_id,
            decision: PermissionDecision::AllowOnce,
        },
    ));
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 1);

    let output = events.iter().find_map(|e| match &e.event {
        Event::Loop(LoopEvent::ToolCallEnd { output, .. }) => Some(output.clone()),
        _ => None,
    });
    assert_eq!(output.unwrap().text, "released");
}

#[test]
fn denied_permission_refuses_the_tool() {
    let h = harness(MockProvider::sequence(vec![
        tool_turn("gate"),
        text_turn("done"),
    ]));
    let id = SessionId::new();
    h.open_with(id, None, ask_for_gate());
    prompt(&h.engine, id, "go", Delivery::Steer);

    let (request_id, _) = permission_request(&h.events);
    h.engine.send(Command::to(
        id,
        CommandKind::ResolvePermission {
            request_id,
            decision: PermissionDecision::Deny,
        },
    ));
    let events = until_runs_end(&h.events, 1);

    let output = events.iter().find_map(|e| match &e.event {
        Event::Loop(LoopEvent::ToolCallEnd { output, .. }) => Some(output.clone()),
        _ => None,
    });
    assert!(output.unwrap().is_error);
}

#[cfg(unix)]
fn capture(command: &str, dir: &std::path::Path) -> (Option<i32>, String) {
    run_shell(command, None, &ToolContext::new(dir, &CancelFlag::new())).unwrap()
}

#[test]
#[cfg(unix)]
fn run_shell_capture_combines_streams_and_exit_code() {
    let dir = std::env::temp_dir();
    let (code, out) = capture("echo out; echo err >&2", &dir);
    assert_eq!(code, Some(0));
    assert!(out.contains("out"), "{out}");
    assert!(out.contains("err"), "{out}");
}

#[test]
#[cfg(unix)]
fn run_shell_capture_reports_failure_and_signal() {
    let dir = std::env::temp_dir();
    let (code, out) = capture("exit 3", &dir);
    assert_eq!(code, Some(3));
    assert_eq!(out, "");
    let (code, _) = capture("kill -9 $$", &dir);
    assert_eq!(code, None);
}

#[test]
#[cfg(unix)]
fn run_shell_capture_truncates_large_output() {
    let dir = std::env::temp_dir();
    let (_, out) = capture("yes | head -c 100000", &dir);
    assert!(
        out.chars().count() <= 8 * 1024 + 64,
        "truncated, len {}",
        out.chars().count()
    );
    assert!(
        out.contains("output truncated"),
        "{:?}",
        &out[..out.len().min(200)]
    );
}

#[test]
#[cfg(unix)]
fn run_shell_capture_runs_in_the_given_workdir() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("marker.txt"), "in the workdir").unwrap();
    let (_, out) = capture("cat marker.txt", dir.path());
    assert_eq!(out.trim(), "in the workdir");
}

/// A session with one recorded exchange in `dir`.
fn recorded_session(h: &Harness, dir: &std::path::Path) -> (SessionId, std::path::PathBuf) {
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir, id);
    h.open(id, Some(recorder));
    prompt(&h.engine, id, "hi", Delivery::Steer);
    until_runs_end(&h.events, 1);
    (id, path)
}

/// A session with `count` recorded exchanges in `dir`.
fn recorded_exchanges(
    h: &Harness,
    dir: &std::path::Path,
    count: usize,
) -> (SessionId, std::path::PathBuf) {
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir, id);
    h.open(id, Some(recorder));
    for i in 0..count {
        prompt(&h.engine, id, &format!("round {i}"), Delivery::Steer);
        until_runs_end(&h.events, 1);
    }
    (id, path)
}

#[test]
fn undo_branches_the_session_in_place_and_returns_its_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("reply")));
    let (id, path) = recorded_exchanges(&h, dir.path(), 2);
    assert_eq!(kage_session::replay(&path).unwrap().history.len(), 4);

    h.engine.send(Command::to(id, CommandKind::Undo));
    // Undone publishes after the SessionChanged reload, so waiting for
    // it collects both.
    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::Undone { .. }))
    });
    let (changed, messages, same_id) = seen
        .iter()
        .find_map(|e| match &e.event {
            Event::Host(HostEvent::SessionChanged { path, timeline, .. }) => {
                Some((path.clone(), timeline.len(), e.session))
            }
            _ => None,
        })
        .expect("undo reloads the session");
    assert_eq!(changed, path, "undo keeps the same file");
    assert_eq!(same_id, id, "undo keeps the same session id");
    assert_eq!(messages, 2, "the first exchange stays");
    let undone = seen
        .iter()
        .find_map(|e| match &e.event {
            Event::Host(HostEvent::Undone { prompt }) => prompt.clone(),
            _ => None,
        })
        .expect("undo reports the removed prompt");
    assert_eq!(
        undone.first(),
        Some(&Content::Text {
            text: "round 1".into()
        })
    );
    // The file holds a branch marker now; replay follows the active
    // branch, and the abandoned exchange stays in the file.
    assert_eq!(kage_session::replay(&path).unwrap().history.len(), 2);
    // The next prompt appends to the same file on the new branch.
    prompt(&h.engine, id, "again", Delivery::Steer);
    until_runs_end(&h.events, 1);
    let tree = kage_session::SessionTree::read(&path).unwrap();
    assert_eq!(
        tree.walk().len(),
        6,
        "the abandoned branch stays in the file"
    );
    assert_eq!(
        kage_session::replay(&path).unwrap().history.len(),
        4,
        "replay follows the active branch only"
    );
}

#[test]
fn undo_and_rewind_navigate_the_branch_tree() {
    fn text(prompt: &kage_core::protocol::PastPrompt) -> &str {
        match &prompt.prompt[0] {
            Content::Text { text } => text,
            other => panic!("expected text, got {other:?}"),
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("reply")));
    let (id, path) = recorded_exchanges(&h, dir.path(), 2);

    // Undo the last exchange and re-prompt on the new branch.
    h.engine.send(Command::to(id, CommandKind::Undo));
    wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::Undone { .. }))
    });
    prompt(&h.engine, id, "redo", Delivery::Steer);
    until_runs_end(&h.events, 1);

    // The tree: round 0 -> round 1 on the original thread, redo
    // branching off round 0 after the undo. Depth-first, the active
    // head comes last, and only the head is active.
    h.engine.send(Command::to(id, CommandKind::ListPrompts));
    let rows = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::PromptList { .. }))
    });
    let prompts = rows
        .iter()
        .find_map(|e| match &e.event {
            Event::Host(HostEvent::PromptList { prompts }) => Some(prompts.clone()),
            _ => None,
        })
        .expect("the prompt list");
    let texts: Vec<&str> = prompts.iter().map(text).collect();
    assert_eq!(texts, ["round 0", "round 1", "redo"]);
    let depths: Vec<u32> = prompts.iter().map(|p| p.depth).collect();
    assert_eq!(
        depths,
        [0, 0, 1],
        "the original thread is flat, redo branched once"
    );
    let actives: Vec<bool> = prompts.iter().map(|p| p.active).collect();
    assert_eq!(
        actives,
        [false, false, true],
        "only the head prompt is active"
    );

    // Rewind to the original second prompt: it stays, and the session
    // continues from it on the original branch.
    let round1 = prompts[1].at.clone();
    h.engine
        .send(Command::to(id, CommandKind::Rewind { at: round1 }));
    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::Undone { .. }))
    });
    let undone = seen
        .iter()
        .find_map(|e| match &e.event {
            Event::Host(HostEvent::Undone { prompt }) => Some(prompt.clone()),
            _ => None,
        })
        .expect("rewind answers with Undone");
    assert!(
        undone.is_none(),
        "the picked prompt is not moved to the editor"
    );
    assert_eq!(
        kage_session::replay(&path).unwrap().history.len(),
        4,
        "the session continues from round 1's reply"
    );

    // The kept prompt stays reachable for undo: it drops it back into
    // the editor.
    h.engine.send(Command::to(id, CommandKind::Undo));
    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::Undone { .. }))
    });
    let undone = seen
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            Event::Host(HostEvent::Undone { prompt }) => Some(prompt.clone()),
            _ => None,
        })
        .expect("undo answers with Undone");
    let undone = undone.expect("undo returns the kept prompt");
    assert_eq!(
        undone.first(),
        Some(&Content::Text {
            text: "round 1".into()
        })
    );
    assert_eq!(
        kage_session::replay(&path).unwrap().history.len(),
        2,
        "the session continues from round 0's reply"
    );
}

#[test]
fn undo_on_a_session_without_prompts_reports_nothing_to_undo() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("reply")));
    let (id, path) = recorded_session(&h, dir.path());
    // Strip the recorded exchange: only the header stays.
    let header = kage_session::replay(&path).unwrap().header;
    std::fs::write(
        &path,
        serde_json::to_string(&{ kage_session::SessionEntry::Header(header) }).unwrap() + "\n",
    )
    .unwrap();

    h.engine.send(Command::to(id, CommandKind::Undo));
    let seen = wait_for(&h.events, |e| {
        matches!(
            e.event,
            Event::Host(HostEvent::Notice {
                level: NoticeLevel::Error,
                ..
            })
        )
    });
    assert!(format!("{:?}", seen.last().unwrap().event).contains("nothing to undo"));
    assert_eq!(kage_session::replay(&path).unwrap().history.len(), 0);
}

#[test]
fn undo_keeps_entries_older_than_the_last_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("reply")));
    // A label recorded between the exchanges, so it precedes the last
    // prompt and sits on the branch the undo opens.
    let id = SessionId::new();
    let path = dir.path().join(format!("{id}.jsonl"));
    let message = |role: kage_core::Role,
                   text: &str,
                   parent: Option<kage_core::MessageId>|
     -> kage_session::MessageEntry {
        kage_session::MessageEntry {
            id: EntryId::new(),
            ts: chrono::Utc::now(),
            message: std::sync::Arc::new(kage_core::Message::new(
                role,
                vec![Content::Text {
                    text: text.to_owned(),
                }],
                parent,
            )),
            usage: None,
        }
    };
    let u0 = message(kage_core::Role::User, "round 0", None);
    let u0_entry = u0.id;
    let a0 = message(kage_core::Role::Assistant, "reply", Some(u0.message.id));
    let u1 = message(kage_core::Role::User, "round 1", Some(a0.message.id));
    let a1 = message(kage_core::Role::Assistant, "reply", Some(u1.message.id));
    let header = Header {
        version: FORMAT_VERSION,
        session: id,
        id: EntryId::new(),
        ts: chrono::Utc::now(),
        cwd: "/tmp".into(),
        model: "mock/m".into(),
        system_prompt: String::new(),
        parent_session: None,
        parent_entry: None,
    };
    {
        let mut writer = SessionWriter::create(&path, header).unwrap();
        for entry in [
            kage_session::SessionEntry::Message(u0),
            kage_session::SessionEntry::Message(a0),
            kage_session::SessionEntry::Label(kage_session::Label {
                id: EntryId::new(),
                ts: chrono::Utc::now(),
                text: "milestone".to_owned(),
                anchor: u0_entry,
            }),
            kage_session::SessionEntry::Message(u1),
            kage_session::SessionEntry::Message(a1),
        ] {
            writer.append(&entry).unwrap();
        }
    }
    let writer = SessionWriter::open(&path).unwrap();
    h.open(id, Some(Recorder::new(writer, None)));

    h.engine.send(Command::to(id, CommandKind::Undo));
    let seen = wait_for(&h.events, is_session_changed);
    let Event::Host(HostEvent::SessionChanged { timeline, .. }) = &seen.last().unwrap().event
    else {
        unreachable!()
    };
    assert_eq!(timeline.len(), 2, "only the first exchange survives");
    // The label stays in the file, on the active chain: the branch
    // marker continues the session from it.
    let kept: Vec<kage_session::SessionEntry> = kage_session::SessionReader::iter(&path)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        kept.len(),
        7,
        "header, the first exchange, the label, the abandoned second exchange, the marker"
    );
    assert!(matches!(&kept[3], kage_session::SessionEntry::Label(l) if l.text == "milestone"));
    assert!(
        kept.iter()
            .any(|e| matches!(e, kage_session::SessionEntry::Branch(_)))
    );
    assert_eq!(
        kage_session::replay(&path).unwrap().history.len(),
        2,
        "replay follows the branch the undo opened"
    );
}

/// The entry ids of `path`'s user prompts, in file order.
fn user_prompt_ids(path: &std::path::Path) -> Vec<EntryId> {
    kage_session::SessionReader::iter(path)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry {
            kage_session::SessionEntry::Message(m) if m.message.role == kage_core::Role::User => {
                Some(m.id)
            }
            _ => None,
        })
        .collect()
}

#[test]
fn rewind_keeps_the_picked_prompt_and_drops_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("reply")));
    let (id, path) = recorded_exchanges(&h, dir.path(), 3);
    let ids = user_prompt_ids(&path);
    assert_eq!(ids.len(), 3);

    // A prefix is enough to name the middle prompt, but it must be
    // long enough to be unambiguous: entry ids share their leading
    // timestamp characters when written in the same millisecond,
    // like every entry this test writes.
    let at = ids[1].to_string();
    let cut = at.len() - 4;
    h.engine.send(Command::to(
        id,
        CommandKind::Rewind {
            at: at[..cut].to_owned(),
        },
    ));
    // Undone publishes after the SessionChanged reload, so waiting for
    // it collects both.
    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::Undone { .. }))
    });
    let (changed, messages) = seen
        .iter()
        .find_map(|e| match &e.event {
            Event::Host(HostEvent::SessionChanged { path, timeline, .. }) => {
                Some((path.clone(), timeline.len()))
            }
            _ => None,
        })
        .expect("rewind reloads the session");
    assert_eq!(changed, path, "rewind keeps the same file");
    // The picked prompt's full turn stays: the prompt and its reply.
    assert_eq!(messages, 4, "the picked turn is kept whole");
    assert_eq!(
        kage_session::replay(&path).unwrap().history.len(),
        4,
        "the session continues from the turn's reply"
    );
    let tree = kage_session::SessionTree::read(&path).unwrap();
    assert_eq!(
        tree.walk().len(),
        6,
        "the abandoned branch stays in the file"
    );
    let undone = seen
        .iter()
        .find_map(|e| match &e.event {
            Event::Host(HostEvent::Undone { prompt }) => Some(prompt.clone()),
            _ => None,
        })
        .expect("rewind answers with Undone");
    assert!(
        undone.is_none(),
        "the picked turn stays in the conversation, not the editor"
    );
}

#[test]
fn rewind_with_an_unknown_entry_reports_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("reply")));
    let (id, path) = recorded_exchanges(&h, dir.path(), 1);

    h.engine.send(Command::to(
        id,
        CommandKind::Rewind {
            at: "zzzzzz".to_owned(),
        },
    ));
    let seen = wait_for(&h.events, |e| {
        matches!(
            e.event,
            Event::Host(HostEvent::Notice {
                level: NoticeLevel::Error,
                ..
            })
        )
    });
    assert!(format!("{:?}", seen.last().unwrap().event).contains("no user prompt matches"));
    assert_eq!(
        kage_session::replay(&path).unwrap().history.len(),
        2,
        "the session is untouched"
    );
}

#[test]
fn list_prompts_lists_every_user_prompt_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("reply")));
    let (id, path) = recorded_exchanges(&h, dir.path(), 2);
    let ids = user_prompt_ids(&path);

    h.engine.send(Command::to(id, CommandKind::ListPrompts));
    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::PromptList { .. }))
    });
    let Event::Host(HostEvent::PromptList { prompts }) = &seen.last().unwrap().event else {
        unreachable!()
    };
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].at, ids[0].to_string());
    assert_eq!(prompts[1].at, ids[1].to_string());
    assert_eq!(
        prompts[0].prompt.first(),
        Some(&Content::Text {
            text: "round 0".into()
        })
    );
    assert_eq!(
        prompts[1].prompt.first(),
        Some(&Content::Text {
            text: "round 1".into()
        })
    );
}

#[test]
fn load_session_switches_to_the_stored_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let first = harness(MockProvider::replaying(text_turn("hello")));
    let (_, path) = recorded_session(&first, dir.path());
    first.engine.shutdown();

    let h = harness(MockProvider::replaying(text_turn("hello")));
    let id = SessionId::new();
    h.open(id, None);
    h.engine.send(Command::to(
        id,
        CommandKind::LoadSession { path: path.clone() },
    ));
    let seen = wait_for(&h.events, is_session_changed);
    match &seen.last().unwrap().event {
        Event::Host(HostEvent::SessionChanged {
            timeline, path: p, ..
        }) => {
            assert_eq!(p, &path);
            assert_eq!(timeline.len(), 2);
        }
        _ => unreachable!(),
    }
    prompt(
        &h.engine,
        seen.last().unwrap().session,
        "more",
        Delivery::Steer,
    );
    until_runs_end(&h.events, 1);
    assert_eq!(kage_session::replay(&path).unwrap().history.len(), 4);
}

#[test]
fn clone_continues_on_a_copy() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("hello")));
    let (id, path) = recorded_session(&h, dir.path());

    h.engine.send(Command::to(id, CommandKind::Clone));
    let seen = wait_for(&h.events, is_session_changed);
    let Event::Host(HostEvent::SessionChanged {
        path: copy,
        timeline,
        ..
    }) = &seen.last().unwrap().event
    else {
        unreachable!()
    };
    assert_ne!(copy, &path);
    assert!(copy.exists());
    assert_eq!(timeline.len(), 2);
}

#[test]
fn clone_and_resume_keep_the_title() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("hello")));
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        title: true,
        ..h.spec(id)
    });
    prompt(&h.engine, id, "hi", Delivery::Steer);
    wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::TitleChanged { .. }))
    });

    h.engine.send(Command::to(id, CommandKind::Clone));
    let seen = wait_for(&h.events, is_session_changed);
    let last = seen.last().unwrap();
    let Event::Host(HostEvent::SessionChanged {
        path: copy, title, ..
    }) = &last.event
    else {
        unreachable!()
    };
    assert_eq!(title.as_deref(), Some("hello"));
    assert_eq!(
        kage_session::replay(copy).unwrap().title.as_deref(),
        Some("hello")
    );

    h.engine
        .send(Command::to(last.session, CommandKind::LoadSession { path }));
    let seen = wait_for(&h.events, is_session_changed);
    let Event::Host(HostEvent::SessionChanged { title, .. }) = &seen.last().unwrap().event else {
        unreachable!()
    };
    assert_eq!(title.as_deref(), Some("hello"));
}

fn state_of(envelope: &Envelope) -> Option<&SessionState> {
    match &envelope.event {
        Event::Host(HostEvent::StateChanged { state }) => Some(state),
        _ => None,
    }
}

#[test]
fn switching_sessions_drops_the_permission_mode() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("hello")));
    let id = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), id);
    let gate = PermissionGate::new(PermissionsConfig::default());
    h.open_with(id, Some(recorder), gate.clone());
    prompt(&h.engine, id, "hi", Delivery::Steer);
    until_runs_end(&h.events, 1);

    h.engine.send(Command::to(
        id,
        CommandKind::SetPermissionMode {
            mode: Some(PermissionAction::Ask),
        },
    ));
    h.engine.send(Command::to(id, CommandKind::Clone));
    let before = wait_for(&h.events, is_session_changed);
    assert!(
        before
            .iter()
            .filter_map(state_of)
            .any(|s| s.permission_mode == Some(PermissionAction::Ask))
    );
    let after = wait_for(&h.events, |e| state_of(e).is_some());
    assert_eq!(
        state_of(after.last().unwrap()).unwrap().permission_mode,
        None
    );
    assert_eq!(gate.mode(), None);
}

fn set_model(engine: &Engine, id: SessionId, model: &str) {
    engine.send(Command::to(
        id,
        CommandKind::SetModel {
            model: model.into(),
        },
    ));
}

fn open_with_env(h: &Harness, id: SessionId, recorder: Option<Recorder>) {
    let mut spec = h.spec(id);
    spec.recorder = recorder;
    spec.cx.system_prompt = crate::runtime_env::build_system_prompt(
        "You are kage.",
        &spec.cx.workdir,
        "mock/m",
        &[],
        None,
    );
    h.engine.open(spec);
}

#[test]
fn a_model_switch_refreshes_the_system_prompt() {
    let mock = MockProvider::replaying(text_turn("hello"));
    let h = harness(mock.clone());
    let id = SessionId::new();
    open_with_env(&h, id, None);

    for model in ["mock/m", "mock/other", "mock/m"] {
        set_model(&h.engine, id, model);
        prompt(&h.engine, id, "what model are you?", Delivery::Steer);
        until_runs_end(&h.events, 1);
        let request = mock.last_request().unwrap();
        assert_eq!(request.model, model.strip_prefix("mock/").unwrap());
        assert!(
            request
                .system
                .unwrap()
                .contains(&format!("\nmodel: {model}\n")),
            "system prompt should identify the selected model {model}"
        );
    }
}

#[test]
fn a_model_switch_survives_a_reload() {
    let dir = tempfile::tempdir().unwrap();
    let first = harness(MockProvider::replaying(text_turn("hello")));
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    first.open(id, Some(recorder));
    set_model(&first.engine, id, "mock/other");
    wait_for(&first.events, |e| {
        state_of(e).is_some_and(|s| s.model == "mock/other")
    });
    first.engine.shutdown();

    let mock = MockProvider::replaying(text_turn("hello"));
    let h = harness(mock.clone());
    let fresh = SessionId::new();
    open_with_env(&h, fresh, None);
    h.engine
        .send(Command::to(fresh, CommandKind::LoadSession { path }));
    let seen = wait_for(&h.events, is_session_changed);
    let loaded = seen.last().unwrap().session;
    let seen = wait_for(&h.events, |e| e.session == loaded && state_of(e).is_some());
    assert_eq!(state_of(seen.last().unwrap()).unwrap().model, "mock/other");
    prompt(&h.engine, loaded, "what model are you?", Delivery::Steer);
    until_runs_end(&h.events, 1);
    let request = mock.last_request().unwrap();
    assert_eq!(request.model, "other");
    assert!(request.system.unwrap().contains("\nmodel: mock/other\n"));
}

#[test]
fn a_model_switch_during_a_run_is_recorded_at_the_next_run() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(vec![
        tool_turn("gate"),
        text_turn("done"),
        text_turn("again"),
    ]);
    let h = harness(mock.clone());
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    open_with_env(&h, id, Some(recorder));
    prompt(&h.engine, id, "go", Delivery::Steer);
    wait_for(&h.events, is_tool_start);

    set_model(&h.engine, id, "mock/other");
    h.release.send(()).unwrap();
    until_runs_end(&h.events, 1);
    assert_eq!(kage_session::replay(&path).unwrap().model, "mock/m");
    for request in mock.requests() {
        assert_eq!(request.model, "m");
        assert!(request.system.unwrap().contains("\nmodel: mock/m\n"));
    }

    prompt(&h.engine, id, "again", Delivery::Steer);
    until_runs_end(&h.events, 1);
    assert_eq!(kage_session::replay(&path).unwrap().model, "mock/other");
    let request = mock.last_request().unwrap();
    assert_eq!(request.model, "other");
    assert!(request.system.unwrap().contains("\nmodel: mock/other\n"));
}

#[test]
fn fork_export_and_delete_report_through_notices() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("hello")));
    let (id, path) = recorded_session(&h, dir.path());

    h.engine.send(Command::to(
        id,
        CommandKind::Fork {
            at: None,
            switch: false,
        },
    ));
    let fork = wait_for(&h.events, is_notice);
    assert!(notices(&fork)[0].starts_with("forked session: "));

    let out = dir.path().join("out.md");
    h.engine.send(Command::to(
        id,
        CommandKind::Export {
            path: Some(out.clone()),
        },
    ));
    wait_for(&h.events, is_notice);
    assert!(
        std::fs::read_to_string(&out)
            .unwrap()
            .contains("## Assistant")
    );

    h.engine.send(Command::to(
        id,
        CommandKind::DeleteSession { path: path.clone() },
    ));
    let refused = wait_for(&h.events, is_notice);
    assert_eq!(notices(&refused), ["cannot delete an open session"]);
    assert!(path.exists());
}

#[test]
fn swarm_mode_and_shell_count_reach_the_state_snapshot() {
    let mock = MockProvider::replaying(text_turn("ok"));
    let h = harness(mock.clone());
    let id = SessionId::new();
    h.open(id, None);
    h.engine
        .send(Command::to(id, CommandKind::SwarmMode { on: true }));
    let on = wait_for(&h.events, |e| state_of(e).is_some_and(|s| s.swarm));
    assert!(
        state_of(on.last().unwrap()).is_some_and(|s| s.swarm),
        "the /swarm toggle publishes its state"
    );

    h.engine.send(Command::to(
        id,
        CommandKind::Shell {
            command: "echo from-shell".into(),
        },
    ));
    let started = wait_for(&h.events, |e| state_of(e).is_some_and(|s| s.shells == 1));
    assert!(state_of(started.last().unwrap()).is_some_and(|s| s.shells == 1));
    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::ShellFinished { .. }))
    });
    let finished = wait_for(&h.events, |e| state_of(e).is_some_and(|s| s.shells == 0));
    assert!(state_of(finished.last().unwrap()).is_some_and(|s| s.shells == 0));
    drop(seen);
    h.engine.shutdown();
}

#[test]
fn shell_output_reaches_the_next_request() {
    let mock = MockProvider::replaying(text_turn("ok"));
    let h = harness(mock.clone());
    let id = SessionId::new();
    h.open(id, None);
    h.engine.send(Command::to(
        id,
        CommandKind::Shell {
            command: "echo from-shell".into(),
        },
    ));
    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::ShellFinished { .. }))
    });
    let Event::Host(HostEvent::ShellFinished {
        output, exit_code, ..
    }) = &seen.last().unwrap().event
    else {
        unreachable!()
    };
    assert_eq!(output.trim(), "from-shell");
    assert_eq!(*exit_code, Some(0));

    prompt(&h.engine, id, "what did it print?", Delivery::Steer);
    until_runs_end(&h.events, 1);
    let request = mock.requests().pop().unwrap();
    let texts: Vec<String> = request
        .messages
        .iter()
        .map(|m| crate::cli_loop_run::first_user_text(m))
        .collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("[shell] ran `echo from-shell`"))
    );
}

#[test]
#[cfg(unix)]
fn a_shell_command_streams_its_output_and_a_cancel_kills_it() {
    let h = harness(MockProvider::replaying(text_turn("ok")));
    let id = SessionId::new();
    h.open(id, None);
    let started = std::time::Instant::now();
    h.engine.send(Command::to(
        id,
        CommandKind::Shell {
            command: "echo first; sleep 5; echo done".into(),
        },
    ));
    let seen = wait_for(
        &h.events,
        |e| matches!(&e.event, Event::Host(HostEvent::ShellOutput { tail, .. }) if tail == "first"),
    );
    assert!(host_events(&seen).iter().any(|e| matches!(
        e,
        HostEvent::StateChanged { state } if state.working
    )));
    h.engine.send(Command::to(id, CommandKind::Cancel));
    let seen = wait_for(
        &h.events,
        |e| matches!(&e.event, Event::Host(HostEvent::StateChanged { state }) if !state.working),
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    let finished = host_events(&seen).into_iter().find_map(|e| match e {
        HostEvent::ShellFinished {
            output, exit_code, ..
        } => Some((output.clone(), *exit_code)),
        _ => None,
    });
    assert_eq!(finished, Some(("first\ncancelled".to_owned(), None)));
}

#[test]
#[cfg(unix)]
fn a_prompt_waits_for_the_shell_command_before_it() {
    let mock = MockProvider::replaying(text_turn("ok"));
    let h = harness(mock.clone());
    let id = SessionId::new();
    h.open(id, None);
    h.engine.send(Command::to(
        id,
        CommandKind::Shell {
            command: "sleep 0.2; echo late".into(),
        },
    ));
    prompt(&h.engine, id, "go", Delivery::Steer);
    let seen = until_runs_end(&h.events, 1);
    let order: Vec<&str> = host_events(&seen)
        .into_iter()
        .filter_map(|e| match e {
            HostEvent::ShellFinished { .. } => Some("shell"),
            HostEvent::RunStarted => Some("run"),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["shell", "run"]);
    let texts: Vec<String> = mock.requests()[0]
        .messages
        .iter()
        .map(|m| crate::cli_loop_run::first_user_text(m))
        .collect();
    assert!(texts[0].starts_with("[shell] ran `sleep 0.2; echo late`"));
    assert_eq!(texts[1], "go");
}

#[test]
#[cfg(unix)]
fn a_finished_shell_command_fires_user_shell() {
    let runtime = Arc::new(PluginRuntime::new().unwrap());
    runtime
        .eval(
            "seen = {} \
            kage.on('user_shell', function(p) \
                table.insert(seen, p.cmd .. '=' .. tostring(p.exit_code)) \
            end)",
        )
        .unwrap();
    let h = harness(MockProvider::replaying(text_turn("ok")));
    let id = SessionId::new();
    h.engine.open(SessionSpec {
        plugins: Some(Arc::clone(&runtime)),
        ..h.spec(id)
    });
    for command in ["exit 3", "kill -9 $$"] {
        h.engine.send(Command::to(
            id,
            CommandKind::Shell {
                command: command.into(),
            },
        ));
        wait_for(&h.events, |e| {
            matches!(e.event, Event::Host(HostEvent::ShellFinished { .. }))
        });
    }
    let seen = runtime.eval("return table.concat(seen, ';')").unwrap();
    assert_eq!(seen.to_string().unwrap(), "exit 3=3;kill -9 $$=nil");
}

#[test]
fn a_compacted_session_replays_the_live_history() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(vec![
        text_turn("first"),
        tool_turn("gate"),
        tool_turn("gate"),
        text_turn("done"),
        text_turn("summary"),
        text_turn("ok"),
    ]);
    let h = harness(mock.clone());
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    h.open(id, Some(recorder));
    prompt(&h.engine, id, "one", Delivery::Steer);
    until_runs_end(&h.events, 1);
    h.engine.send(Command::to(
        id,
        CommandKind::Shell {
            command: "echo from-shell".into(),
        },
    ));
    wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::ShellFinished { .. }))
    });
    h.release.send(()).unwrap();
    h.release.send(()).unwrap();
    prompt(&h.engine, id, "two", Delivery::Steer);
    until_runs_end(&h.events, 1);
    h.engine.send(Command::to(id, CommandKind::Compact));
    until_runs_end(&h.events, 1);
    prompt(&h.engine, id, "three", Delivery::Steer);
    until_runs_end(&h.events, 1);
    h.engine.shutdown();

    let live = mock.requests().pop().unwrap().messages;
    let replayed = kage_session::replay(&path).unwrap().history;
    let shape = |m: &Message| (m.role, m.content.clone());
    let live_shape = |m: &Arc<Message>| (m.role, m.content.clone());
    assert_eq!(
        replayed[..live.len()].iter().map(shape).collect::<Vec<_>>(),
        live.iter().map(live_shape).collect::<Vec<_>>()
    );
    let mut calls = std::collections::HashSet::new();
    for block in replayed.iter().flat_map(|m| &m.content) {
        match block {
            Content::ToolCall { id, .. } => {
                calls.insert(id.clone());
            }
            Content::ToolResultBlock { call_id, .. } => {
                assert!(calls.contains(call_id), "orphan result {call_id}");
            }
            _ => {}
        }
    }
}

#[test]
fn a_manual_compaction_shows_its_estimate_before_the_next_turn() {
    let h = harness(MockProvider::sequence(vec![
        text_turn("one"),
        text_turn("two"),
        text_turn("three"),
        text_turn("summary"),
    ]));
    let id = SessionId::new();
    h.open(id, None);
    for text in ["one", "two", "three"] {
        prompt(&h.engine, id, text, Delivery::Steer);
        until_runs_end(&h.events, 1);
    }
    h.engine.send(Command::to(id, CommandKind::Compact));
    let seen = wait_for(&h.events, |e| {
        matches!(
            e.event,
            Event::Host(HostEvent::UsageUpdated { usage }) if usage.context_estimated
        )
    });
    assert!(
        seen.iter()
            .any(|e| matches!(e.event, Event::Loop(LoopEvent::Compaction { .. })))
    );
    until_runs_end(&h.events, 1);
}

#[test]
fn a_run_tells_the_provider_its_session() {
    let mock = MockProvider::replaying(text_turn("ok"));
    let h = harness(mock.clone());
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "hi", Delivery::Steer);
    until_runs_end(&h.events, 1);
    let request = mock.requests().pop().unwrap();
    assert_eq!(request.session, Some(id.to_string()));
}

#[test]
fn compact_without_history_is_a_no_op() {
    let h = harness(MockProvider::replaying(text_turn("ok")));
    let id = SessionId::new();
    h.open(id, None);
    h.engine.send(Command::to(id, CommandKind::Compact));
    let events = until_runs_end(&h.events, 1);
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
    assert!(notices(&events).contains(&"compact: not enough history yet".to_owned()));
}

/// A manual compaction whose summarizer fails must be visible: the
/// engine emits the error event that renders in every transcript, and
/// the run outcome carries the compaction failure for reports.
#[test]
fn a_failed_manual_compaction_emits_the_error_event() {
    let h = harness(MockProvider::replaying(vec![Err(
        kage_provider::ProviderError::Auth("no key".into()),
    )]));
    let id = SessionId::new();
    let mut cx = AgentContext::new("m", "").with_workdir(std::env::temp_dir());
    for i in 0..8 {
        cx.history.push(Arc::new(Message::new(
            kage_core::Role::User,
            vec![Content::Text {
                text: format!("turn {i}"),
            }],
            None,
        )));
    }
    h.engine.open(SessionSpec {
        id,
        cx,
        ..h.spec(id)
    });
    h.engine.send(Command::to(id, CommandKind::Compact));
    let events = until_runs_end(&h.events, 1);
    assert!(events.iter().any(|e| matches!(
        &e.event,
        Event::Loop(LoopEvent::Error {
            kind: LoopError::Compaction { message },
        }) if message.contains("authentication failed")
    )));
    let run_outcomes = outcomes(&events);
    assert_eq!(run_outcomes.len(), 1);
    match &run_outcomes[0] {
        RunOutcome::Failed { error } => assert!(
            matches!(error, LoopError::Compaction { message } if message.contains("authentication failed")),
            "{error}"
        ),
        other => panic!("expected a failed compact run, got {other:?}"),
    }
}

#[test]
fn first_exchange_records_a_title() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("hello")));
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    h.engine.open(SessionSpec {
        id,
        model: "mock/m".into(),
        cx: AgentContext::new("m", "").with_workdir(std::env::temp_dir()),
        recorder: Some(recorder),
        tools: h.tools.clone(),
        plugins: None,
        gate: PermissionGate::new(PermissionsConfig::default()),
        loop_cfg: LoopConfig::default(),
        mcp: None,
        interactive: true,
        title: true,
        agents: None,
        shell: None,
    });
    prompt(&h.engine, id, "hi", Delivery::Steer);
    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::TitleChanged { .. }))
    });
    let Event::Host(HostEvent::TitleChanged { title }) = &seen.last().unwrap().event else {
        unreachable!()
    };
    assert_eq!(title, "hello");
    h.engine.shutdown();
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.contains("\"type\":\"title\""), "{file}");
}

#[test]
fn a_named_session_keeps_its_name_over_a_generated_title() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("hello")));
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        title: true,
        ..h.spec(id)
    });
    h.engine.send(Command::to(
        id,
        CommandKind::SetTitle {
            title: "Mine".into(),
        },
    ));
    prompt(&h.engine, id, "hi", Delivery::Queue);
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 1);
    h.engine.shutdown();
    let titles: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Host(HostEvent::TitleChanged { title }) => Some(title.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(titles, ["Mine"]);
    let file = std::fs::read_to_string(&path).unwrap();
    assert_eq!(file.matches("\"type\":\"title\"").count(), 1, "{file}");
}

#[test]
fn a_title_that_arrives_during_the_next_run_is_recorded_when_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::sequence(vec![
        tool_turn("gate"),
        text_turn("hello"),
        tool_turn("gate"),
        tool_turn("gate"),
        text_turn("done"),
    ]));
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        title: true,
        ..h.spec(id)
    });
    prompt(&h.engine, id, "hi", Delivery::Queue);
    prompt(&h.engine, id, "again", Delivery::Queue);
    h.release.send(()).unwrap();
    wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::TitleChanged { .. }))
    });
    h.release.send(()).unwrap();
    until_runs_end(&h.events, 1);
    h.engine.shutdown();
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.contains("\"type\":\"title\""), "{file}");
}

#[test]
fn plugin_turn_end_entries_land_in_the_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut caps = std::collections::BTreeMap::new();
    caps.insert("t".to_owned(), vec!["session_write".to_owned()]);
    let runtime = Arc::new(PluginRuntime::builder().capabilities(caps).build().unwrap());
    runtime
        .eval_plugin(
            "t",
            "kage.request_capabilities({'session_write'}); \
            kage.on('turn_end', function() \
                kage.session.append_entry('plugin:mark', { ok = true }) \
            end)",
        )
        .unwrap();
    let h = harness(MockProvider::replaying(text_turn("hello")));
    let id = SessionId::new();
    let (_, path) = recorder_in(dir.path(), id);
    let writer = SessionWriter::open(&path).unwrap();
    h.engine.open(SessionSpec {
        id,
        model: "mock/m".into(),
        cx: AgentContext::new("m", "").with_workdir(std::env::temp_dir()),
        recorder: Some(Recorder::new(writer, Some(Arc::clone(&runtime)))),
        tools: h.tools.clone(),
        plugins: Some(runtime),
        gate: PermissionGate::new(PermissionsConfig::default()),
        loop_cfg: LoopConfig::default(),
        mcp: None,
        interactive: true,
        shell: None,
        title: false,
        agents: None,
    });
    prompt(&h.engine, id, "hi", Delivery::Steer);
    until_runs_end(&h.events, 1);
    h.engine.shutdown();
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.contains("plugin:mark"), "{file}");
}

fn agent_setup(max_depth: u8, max_running: usize) -> AgentSetup {
    AgentSetup {
        defs: Arc::new(AgentDefs::builtin()),
        max_depth,
        max_running,
        swarm_max_items: 32,
        swarm_timeout_ms: 60_000,
        background: Background::Off,
        max_turns: 0,
        timeout: None,
        budget: 0,
        model: None,
        thinking: None,
    }
}

fn task(prompt: &str) -> serde_json::Value {
    serde_json::json!({"description": "a task", "prompt": prompt})
}

/// One assistant turn that calls `agent` once per `(call id, input)`.
fn agent_turn(calls: &[(&str, serde_json::Value)]) -> Vec<Result<ProviderEvent, ProviderError>> {
    let mut turn = vec![Ok(ProviderEvent::MessageStart)];
    for (id, input) in calls {
        let id = ToolCallId::new(*id);
        turn.push(Ok(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: "agent".into(),
        }));
        turn.push(Ok(ProviderEvent::ToolCallEnd {
            id,
            input: input.clone(),
        }));
    }
    turn.push(Ok(ProviderEvent::MessageEnd {
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage::default(),
    }));
    turn
}

impl Harness {
    fn open_parent(
        &self,
        recorder: Option<Recorder>,
        gate: PermissionGate,
        agents: Option<AgentSetup>,
    ) -> SessionId {
        let id = SessionId::new();
        self.engine.open(SessionSpec {
            recorder,
            gate,
            agents,
            ..self.spec(id)
        });
        id
    }
}

/// Children in spawn order, with the call that started each.
fn spawned(events: &[Envelope]) -> Vec<(SessionId, ToolCallId)> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Host(HostEvent::AgentSpawned { tool_call_id, .. }) => {
                Some((e.session, tool_call_id.clone()))
            }
            _ => None,
        })
        .collect()
}

fn tool_output(events: &[Envelope], session: SessionId, call: &str) -> ToolOutput {
    events
        .iter()
        .find_map(|e| match &e.event {
            Event::Loop(LoopEvent::ToolCallEnd { id, output })
                if e.session == session && id.0 == call =>
            {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("tool call ended")
}

fn outcome_of(events: &[Envelope], session: SessionId) -> Vec<RunOutcome> {
    let mine: Vec<Envelope> = events
        .iter()
        .filter(|e| e.session == session)
        .cloned()
        .collect();
    outcomes(&mine)
}

fn is_tool_start_outside(parent: SessionId) -> impl Fn(&Envelope) -> bool {
    move |e| e.session != parent && is_tool_start(e)
}

#[test]
fn agent_call_returns_the_child_reply_and_records_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("find it"))]),
        text_turn("child reply"),
        text_turn("parent done"),
    ]));
    let parent_id = SessionId::new();
    let (recorder, parent_path) = recorder_in(dir.path(), parent_id);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(agent_setup(1, 1)),
        ..h.spec(parent_id)
    });
    prompt(&h.engine, parent_id, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    h.engine.shutdown();

    let [(child, call)] = &spawned(&events)[..] else {
        panic!("one child expected");
    };
    let child = *child;
    assert_eq!(call.0, "call_a");
    let first = events.iter().find(|e| e.session == child).unwrap();
    assert_eq!(first.seq, 1);
    assert!(matches!(
        &first.event,
        Event::Host(HostEvent::AgentSpawned { parent, agent, .. })
            if *parent == parent_id && agent == "general"
    ));
    let output = tool_output(&events, parent_id, "call_a");
    assert!(
        output.text.starts_with(&format!(
            "<agent name=\"general\" session=\"{child}\" state=\"completed\" model=\"mock/m\" \
             tools=\"0\" in=\"10\" out=\"2\" cache_read=\"0\" cache_write=\"0\" cost=\"0.0000\" \
             ctx=\"12\" win=\"200000\" run_ms=\""
        )),
        "{}",
        output.text
    );
    assert!(output.text.contains("\nchild reply\n</agent>"));
    assert!(!output.is_error);
    assert_eq!(outcome_of(&events, parent_id), [RunOutcome::Completed]);

    let child_path = dir.path().join(format!("{child}.jsonl"));
    let entries: Vec<kage_session::SessionEntry> = kage_session::SessionReader::iter(&child_path)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    match &entries[..3] {
        [
            kage_session::SessionEntry::Header(header),
            kage_session::SessionEntry::Custom(marker),
            kage_session::SessionEntry::Title(title),
        ] => {
            assert_eq!(header.parent_session, Some(parent_id));
            assert_eq!(marker.kind, kage_session::list::AGENT_ENTRY_KIND);
            assert_eq!(marker.data["agent"], "general");
            assert_eq!(marker.data["tool_call_id"], "call_a");
            assert_eq!(marker.data["parent"], parent_id.to_string());
            assert_eq!(title.title, "a task");
        }
        other => panic!("unexpected entries {other:?}"),
    }
    let summaries = kage_session::list(dir.path()).unwrap();
    let child_summary = summaries.iter().find(|s| s.id == child).unwrap();
    assert_eq!(child_summary.agent.as_deref(), Some("general"));
    let parent_file = std::fs::read_to_string(&parent_path).unwrap();
    assert!(
        parent_file.contains(&format!("session=\\\"{child}\\\"")),
        "{parent_file}"
    );
}

#[test]
fn agent_tool_follows_the_depth_limit() {
    let tool_names = |request: &kage_provider::StreamRequest| -> Vec<String> {
        request.tools.iter().map(|t| t.name.clone()).collect()
    };
    let mock = MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("look"))]),
        text_turn("child reply"),
        text_turn("parent done"),
    ]);
    let h = harness(mock.clone());
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    until_runs_end(&h.events, 2);
    let requests = mock.requests();
    assert!(tool_names(&requests[0]).contains(&"agent".to_owned()));
    let child_tools = tool_names(&requests[1]);
    assert!(
        !child_tools.contains(&"agent".to_owned()),
        "{child_tools:?}"
    );
    assert!(
        !child_tools.contains(&"swarm".to_owned()),
        "{child_tools:?}"
    );
    assert!(
        child_tools.contains(&"send_message".to_owned()),
        "{child_tools:?}"
    );

    let mock = MockProvider::replaying(text_turn("ok"));
    let h = harness(mock.clone());
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(0, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    until_runs_end(&h.events, 1);
    let main_tools = tool_names(&mock.requests()[0]);
    assert!(!main_tools.contains(&"agent".to_owned()), "{main_tools:?}");
    assert!(
        main_tools.contains(&"send_message".to_owned()),
        "{main_tools:?}"
    );
}

#[test]
fn unknown_agent_names_the_valid_ones() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[(
            "call_a",
            serde_json::json!({"agent": "nope", "description": "d", "prompt": "p"}),
        )]),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    let output = tool_output(&events, parent, "call_a");
    assert!(output.is_error);
    assert!(output.text.contains("explore, general"), "{}", output.text);
    assert!(spawned(&events).is_empty());
}

#[test]
fn cancelling_the_parent_stops_the_child() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("wait"))]),
        tool_turn("gate"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    wait_for(&h.events, is_tool_start_outside(parent));
    h.engine.send(Command::to(parent, CommandKind::Cancel));
    let events = until_runs_end(&h.events, 2);
    assert_eq!(
        outcomes(&events),
        [RunOutcome::Cancelled, RunOutcome::Cancelled]
    );
}

#[test]
fn cancelling_only_the_child_lets_the_parent_continue() {
    let mock = MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("wait"))]),
        tool_turn("gate"),
        text_turn("parent done"),
    ]);
    let h = harness(mock.clone());
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let seen = wait_for(&h.events, is_tool_start_outside(parent));
    let child = seen.last().unwrap().session;
    h.engine.send(Command::to(child, CommandKind::Cancel));
    let events = until_runs_end(&h.events, 2);

    assert_eq!(outcome_of(&events, child), [RunOutcome::Cancelled]);
    assert_eq!(outcome_of(&events, parent), [RunOutcome::Completed]);
    let output = tool_output(&events, parent, "call_a");
    assert!(output.is_error);
    assert!(
        output.text.contains("state=\"cancelled\""),
        "{}",
        output.text
    );
    assert_eq!(mock.call_count(), 3);
}

#[test]
fn running_limit_queues_agents_in_spawn_order() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("one")), ("call_b", task("two"))]),
        text_turn("first reply"),
        text_turn("second reply"),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 3);

    let children = spawned(&events);
    assert_eq!(children.len(), 2);
    let (first, second) = (&children[0], &children[1]);
    let position = |pred: &dyn Fn(&Envelope) -> bool| events.iter().position(pred).unwrap();
    let first_ended = position(&|e| {
        e.session == first.0 && matches!(e.event, Event::Host(HostEvent::RunEnded { .. }))
    });
    let second_started = position(&|e| {
        e.session == second.0 && matches!(e.event, Event::Host(HostEvent::RunStarted))
    });
    assert!(first_ended < second_started);
    assert!(
        tool_output(&events, parent, &first.1.0)
            .text
            .contains("first reply")
    );
    assert!(
        tool_output(&events, parent, &second.1.0)
            .text
            .contains("second reply")
    );
    let results: Vec<String> = events
        .iter()
        .filter(|e| e.session == parent)
        .filter_map(|e| match &e.event {
            Event::Loop(LoopEvent::MessageAppended { message }) => Some(message.content.clone()),
            _ => None,
        })
        .flatten()
        .filter_map(|c| match c {
            Content::ToolResultBlock { call_id, .. } => Some(call_id.0),
            _ => None,
        })
        .collect();
    assert_eq!(results, ["call_a", "call_b"]);
}

#[test]
fn child_asks_and_resolutions_use_the_child_session() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("ask"))]),
        tool_turn("gate"),
        text_turn("child done"),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(None, ask_for_gate(), Some(agent_setup(1, 1)));
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::PermissionRequested { .. }))
    });
    let asked = seen.last().unwrap();
    let child = asked.session;
    assert_ne!(child, parent);
    let Event::Host(HostEvent::PermissionRequested { request_id, .. }) = asked.event else {
        unreachable!()
    };
    h.engine
        .send(Command::active(CommandKind::ResolvePermission {
            request_id,
            decision: PermissionDecision::AllowOnce,
        }));
    let resolved = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::PermissionResolved { .. }))
    });
    assert_eq!(resolved.last().unwrap().session, child);
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 2);
    assert_eq!(outcome_of(&events, parent), [RunOutcome::Completed]);
}

#[test]
fn steering_a_running_child_reaches_it() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        tool_turn("gate"),
        text_turn("child done"),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let child = wait_for(&h.events, is_tool_start_outside(parent))
        .last()
        .unwrap()
        .session;
    prompt(&h.engine, child, "also this", Delivery::Steer);
    std::thread::sleep(Duration::from_millis(50));
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 2);
    let child_events: Vec<Envelope> = events
        .iter()
        .filter(|e| e.session == child)
        .cloned()
        .collect();
    assert!(appended_texts(&child_events).contains(&"also this".to_owned()));
}

#[test]
fn prompting_an_idle_child_leaves_the_parent_alone() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        text_turn("child reply"),
        text_turn("parent done"),
        text_turn("second child reply"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    let (child, _) = spawned(&events)[0].clone();
    prompt(&h.engine, child, "more", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    assert!(
        events
            .iter()
            .filter(|e| e.session == parent)
            .all(|e| state_of(e).is_some()),
        "only the parent's trailing state change: {events:?}"
    );
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
}

#[test]
fn an_idle_child_of_a_cancelled_parent_can_run_again() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("wait"))]),
        tool_turn("gate"),
        text_turn("child again"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let child = wait_for(&h.events, is_tool_start_outside(parent))
        .last()
        .unwrap()
        .session;
    h.engine.send(Command::to(parent, CommandKind::Cancel));
    until_runs_end(&h.events, 2);
    prompt(&h.engine, child, "again", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    assert_eq!(outcome_of(&events, child), [RunOutcome::Completed]);
}

#[test]
fn new_session_waits_for_agents_then_drops_them() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        text_turn("child reply"),
        text_turn("parent done"),
        tool_turn("gate"),
        text_turn("child again"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    let (child, _) = spawned(&events)[0].clone();
    prompt(&h.engine, child, "more", Delivery::Steer);
    wait_for(&h.events, is_tool_start);

    h.engine.send(Command::to(parent, CommandKind::NewSession));
    let refused = wait_for(&h.events, is_notice);
    assert_eq!(
        notices(&refused),
        ["new session: stop or wait for the agents first"]
    );
    h.engine.send(Command::to(child, CommandKind::NewSession));
    let refused = wait_for(&h.events, is_notice);
    assert_eq!(
        notices(&refused),
        ["new session: not available in an agent session"]
    );

    h.release.send(()).unwrap();
    until_runs_end(&h.events, 1);
    h.engine.send(Command::to(parent, CommandKind::NewSession));
    wait_for(&h.events, is_session_changed);
    prompt(&h.engine, child, "gone?", Delivery::Steer);
    let unknown = wait_for(&h.events, |e| {
        is_notice(e) && notices(std::slice::from_ref(e))[0].starts_with("unknown session")
    });
    assert_eq!(unknown.last().unwrap().session, child);
}

#[test]
fn agent_runs_skip_plugin_events_and_session_ops() {
    let dir = tempfile::tempdir().unwrap();
    let mut caps = std::collections::BTreeMap::new();
    caps.insert("t".to_owned(), vec!["session_write".to_owned()]);
    let runtime = Arc::new(PluginRuntime::builder().capabilities(caps).build().unwrap());
    runtime
        .eval_plugin(
            "t",
            "kage.request_capabilities({'session_write'}); \
            kage.session.append_entry('plugin:queued', {}) \
            kage.on('turn_start', function() \
                kage.session.append_entry('plugin:turn', {}) \
            end)",
        )
        .unwrap();
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        text_turn("child reply"),
        text_turn("parent done"),
    ]));
    let parent = SessionId::new();
    let (_, path) = recorder_in(dir.path(), parent);
    let writer = SessionWriter::open(&path).unwrap();
    h.engine.open(SessionSpec {
        recorder: Some(Recorder::new(writer, Some(Arc::clone(&runtime)))),
        plugins: Some(runtime),
        agents: Some(agent_setup(1, 1)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    h.engine.shutdown();

    let (child, _) = spawned(&events)[0].clone();
    let parent_file = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        parent_file.matches("plugin:turn").count(),
        2,
        "{parent_file}"
    );
    assert!(parent_file.contains("plugin:queued"), "{parent_file}");
    let child_file = std::fs::read_to_string(dir.path().join(format!("{child}.jsonl"))).unwrap();
    assert!(!child_file.contains("plugin:"), "{child_file}");
}

#[test]
fn shutdown_with_running_and_waiting_agents_returns() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("one")), ("call_b", task("two"))]),
        tool_turn("gate"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    wait_for(&h.events, is_tool_start_outside(parent));
    let (done_tx, done_rx) = channel();
    std::thread::spawn(move || {
        h.engine.shutdown();
        let _ = done_tx.send(());
    });
    done_rx.recv_timeout(WAIT).expect("shutdown hung");
}

#[test]
fn print_mode_text_shows_only_the_main_session() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        text_turn("child reply"),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);

    let mut text = Vec::new();
    let mut json = Vec::new();
    for envelope in &events {
        crate::cli_loop_run::print_envelope(&mut text, envelope, parent, false);
        crate::cli_loop_run::print_envelope(&mut json, envelope, parent, true);
    }
    let text = String::from_utf8(text).unwrap();
    assert!(text.contains("parent done"), "{text}");
    assert!(!text.contains("child reply"), "{text}");
    let json = String::from_utf8(json).unwrap();
    assert!(json.contains("\"agent_spawned\""), "{json}");
    assert!(json.contains("child reply"), "{json}");
}

#[test]
fn cancel_during_a_child_ask_returns_the_cancelled_wrapper() {
    for stop in [CommandKind::Cancel, CommandKind::Shutdown] {
        let h = harness(MockProvider::sequence(vec![
            agent_turn(&[("call_a", task("ask"))]),
            tool_turn("gate"),
        ]));
        let parent = h.open_parent(None, ask_for_gate(), Some(agent_setup(1, 1)));
        prompt(&h.engine, parent, "go", Delivery::Steer);
        let child = wait_for(&h.events, |e| {
            matches!(e.event, Event::Host(HostEvent::PermissionRequested { .. }))
        })
        .last()
        .unwrap()
        .session;
        h.engine.send(Command::to(parent, stop));
        let events = until_runs_end(&h.events, 2);
        let output = tool_output(&events, parent, "call_a");
        assert!(output.is_error);
        assert!(
            output.text.starts_with(&format!(
                "<agent name=\"general\" session=\"{child}\" state=\"cancelled\" "
            )),
            "{}",
            output.text
        );
        assert_eq!(outcome_of(&events, parent), [RunOutcome::Cancelled]);
    }
}

fn swarm_setup(max_running: usize, timeout_ms: u64) -> AgentSetup {
    AgentSetup {
        defs: Arc::new(AgentDefs::builtin()),
        max_depth: 1,
        max_running,
        swarm_max_items: 32,
        swarm_timeout_ms: timeout_ms,
        background: Background::Off,
        max_turns: 0,
        timeout: None,
        budget: 0,
        model: None,
        thinking: None,
    }
}

fn swarm_task(items: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "description": "a swarm",
        "prompt_template": "handle {{item}}",
        "items": items,
    })
}

/// One assistant turn that calls `swarm` once per `(call id, input)`.
fn swarm_turn(calls: &[(&str, serde_json::Value)]) -> Vec<Result<ProviderEvent, ProviderError>> {
    let mut turn = vec![Ok(ProviderEvent::MessageStart)];
    for (id, input) in calls {
        let id = ToolCallId::new(*id);
        turn.push(Ok(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: "swarm".into(),
        }));
        turn.push(Ok(ProviderEvent::ToolCallEnd {
            id,
            input: input.clone(),
        }));
    }
    turn.push(Ok(ProviderEvent::MessageEnd {
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage::default(),
    }));
    turn
}

#[test]
fn a_swarm_aggregates_its_children_in_item_order() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::sequence(vec![
        swarm_turn(&[("call_s", swarm_task(&["a", "b", "c"]))]),
        text_turn("reply one"),
        text_turn("reply two"),
        text_turn("reply three"),
        text_turn("parent done"),
    ]));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(swarm_setup(1, 60_000)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 4);
    h.engine.shutdown();

    let children = spawned(&events);
    assert_eq!(children.len(), 3);
    assert!(children.iter().all(|(id, _)| events
        .iter()
        .any(|e| e.session == *id
            && matches!(&e.event, Event::Host(HostEvent::AgentSpawned { parent: p, .. }) if *p == parent))));
    let output = tool_output(&events, parent, "call_s");
    assert!(!output.is_error);
    assert!(
        output
            .text
            .starts_with("completed: 3, failed: 0, cancelled: 0\n")
    );
    let at = |needle: &str| output.text.find(needle).unwrap();
    assert!(at("item=\"a\"") < at("item=\"b\"") && at("item=\"b\"") < at("item=\"c\""));
    assert!(
        at("reply one") < at("reply two") && at("reply two") < at("reply three"),
        "{}",
        output.text
    );

    let mut batch_ids = Vec::new();
    for (index, (child, _)) in children.iter().enumerate() {
        let path = dir.path().join(format!("{child}.jsonl"));
        let entries: Vec<kage_session::SessionEntry> = kage_session::SessionReader::iter(&path)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let kage_session::SessionEntry::Custom(marker) = &entries[1] else {
            panic!("marker expected")
        };
        assert_eq!(marker.data["index"], index);
        assert_eq!(marker.data["item"], ["a", "b", "c"][index]);
        batch_ids.push(marker.data["batch_id"].clone());
    }
    assert!(batch_ids[0].as_str().unwrap().starts_with("swarm_"));
    assert!(batch_ids.windows(2).all(|w| w[0] == w[1]), "one batch id");
}

#[test]
fn an_agent_call_overrides_the_child_model_and_thinking() {
    let mock = MockProvider::sequence(vec![
        agent_turn(&[(
            "call_a",
            serde_json::json!({
                "description": "a task",
                "prompt": "find it",
                "model": "mock/other",
                "thinking": "high"
            }),
        )]),
        text_turn("child reply"),
        text_turn("parent done"),
    ]);
    let h = harness(mock.clone());
    let parent_id = SessionId::new();
    h.engine.open(SessionSpec {
        agents: Some(agent_setup(1, 1)),
        ..h.spec(parent_id)
    });
    prompt(&h.engine, parent_id, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    h.engine.shutdown();

    assert_eq!(spawned(&events).len(), 1);
    let output = tool_output(&events, parent_id, "call_a");
    assert!(!output.is_error, "{}", output.text);
    assert!(
        output.text.contains("model=\"mock/other\""),
        "{}",
        output.text
    );
    let requests = mock.requests();
    assert_eq!(requests[0].model, "m");
    assert_eq!(requests[0].level, None);
    assert_eq!(requests[1].model, "other");
    assert_eq!(requests[1].level, Some(ThinkingLevel::High));
}

#[test]
fn the_agents_model_and_thinking_fill_in_for_the_parent() {
    let mock = MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("find it"))]),
        text_turn("child reply"),
        text_turn("parent done"),
    ]);
    let h = harness(mock.clone());
    let parent_id = SessionId::new();
    h.engine.open(SessionSpec {
        agents: Some(AgentSetup {
            model: Some("mock/other".into()),
            thinking: Some(ThinkingLevel::Low),
            ..agent_setup(1, 1)
        }),
        ..h.spec(parent_id)
    });
    prompt(&h.engine, parent_id, "go", Delivery::Steer);
    until_runs_end(&h.events, 2);
    h.engine.shutdown();

    let requests = mock.requests();
    assert_eq!(requests[0].model, "m");
    assert_eq!(requests[1].model, "other");
    assert_eq!(requests[1].level, Some(ThinkingLevel::Low));
}

#[test]
fn an_agent_call_rejects_a_model_without_a_provider() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[(
            "call_a",
            serde_json::json!({
                "description": "a task",
                "prompt": "find it",
                "model": "sonnet",
                "thinking": "much"
            }),
        )]),
        text_turn("parent done"),
    ]));
    let parent_id = SessionId::new();
    h.engine.open(SessionSpec {
        agents: Some(agent_setup(1, 1)),
        ..h.spec(parent_id)
    });
    prompt(&h.engine, parent_id, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    h.engine.shutdown();

    assert!(spawned(&events).is_empty(), "no child spawned");
    let output = tool_output(&events, parent_id, "call_a");
    assert!(output.is_error);
    assert!(output.text.contains("provider/model"), "{}", output.text);
}

#[test]
fn a_swarm_override_reaches_every_new_child() {
    let mut task = swarm_task(&["a", "b"]);
    task["model"] = serde_json::json!("mock/other");
    task["thinking"] = serde_json::json!("high");
    let mock = MockProvider::sequence(vec![
        swarm_turn(&[("call_s", task)]),
        text_turn("reply a"),
        text_turn("reply b"),
        text_turn("parent done"),
    ]);
    let h = harness(mock.clone());
    let parent = SessionId::new();
    h.engine.open(SessionSpec {
        agents: Some(swarm_setup(1, 60_000)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 3);
    h.engine.shutdown();

    assert_eq!(spawned(&events).len(), 2);
    let output = tool_output(&events, parent, "call_s");
    assert!(!output.is_error, "{}", output.text);
    assert_eq!(
        output.text.matches("model=\"mock/other\"").count(),
        2,
        "{}",
        output.text
    );
    let requests = mock.requests();
    assert_eq!(requests[1].model, "other");
    assert_eq!(requests[1].level, Some(ThinkingLevel::High));
    assert_eq!(requests[2].model, "other");
    assert_eq!(requests[2].level, Some(ThinkingLevel::High));
}

#[test]
fn a_forked_child_follows_the_call_model_override() {
    let dir = tempfile::tempdir().unwrap();
    let mut task = forked_swarm_task(&["a", "b"]);
    task["model"] = serde_json::json!("mock/other");
    let h = harness(MockProvider::sequence(vec![
        swarm_turn(&[("call_s", task)]),
        text_turn("child a one"),
        text_turn("child b one"),
        text_turn("parent done"),
    ]));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(swarm_setup(1, 60_000)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 3);
    h.engine.shutdown();

    for (child, _) in spawned(&events) {
        let path = dir.path().join(format!("{child}.jsonl"));
        let replay = kage_session::replay(&path).unwrap();
        assert_eq!(replay.header.model, "mock/other");
    }
    let output = tool_output(&events, parent, "call_s");
    assert_eq!(
        output.text.matches("model=\"mock/other\"").count(),
        2,
        "{}",
        output.text
    );
}

#[test]
fn a_swarm_timeout_cancels_stragglers_and_renders() {
    let h = harness(MockProvider::sequence(vec![
        swarm_turn(&[("call_s", swarm_task(&["quick", "slow"]))]),
        text_turn("quick reply"),
        tool_turn("gate"),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(swarm_setup(1, 300)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 3);
    h.engine.shutdown();

    let output = tool_output(&events, parent, "call_s");
    assert!(
        output
            .text
            .starts_with("completed: 1, failed: 0, cancelled: 1\n")
    );
    assert!(output.text.contains("quick reply"), "{}", output.text);
    assert!(output.text.contains("state=\"cancelled\""));
    assert!(!output.is_error, "a cancelled child is not a failed one");
}

#[test]
fn a_queued_child_keeps_its_own_timeout_budget() {
    // The stalled child is cancelled at its deadline; the queued child
    // then runs and completes even though the batch by then has
    // outlived `swarm_timeout_ms` many times over.
    let h = harness(MockProvider::sequence(vec![
        swarm_turn(&[("call_s", swarm_task(&["held", "late"]))]),
        tool_turn("gate"),
        text_turn("late reply"),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(swarm_setup(1, 300)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 3);
    h.engine.shutdown();

    let output = tool_output(&events, parent, "call_s");
    assert!(
        output
            .text
            .starts_with("completed: 1, failed: 0, cancelled: 1\n"),
        "{}",
        output.text
    );
    assert!(output.text.contains("late reply"), "{}", output.text);
    assert!(!output.is_error, "{}", output.text);
}

/// One provider call that fails with a rate limit carrying a tiny
/// retry hint, so the loop's own retries stay fast in tests.
fn rate_limited() -> Vec<Result<ProviderEvent, ProviderError>> {
    vec![Err(ProviderError::RateLimited {
        retry_after: Some(Duration::from_millis(5)),
        body: None,
    })]
}

#[test]
fn requeue_backoff_doubles_then_refuses() {
    let budget = Duration::from_secs(600);
    let waits: Vec<_> = (0..MAX_REQUEUES)
        .map(|attempts| requeue_backoff(attempts, Duration::ZERO, budget, None).unwrap())
        .collect();
    assert_eq!(waits, [3, 6, 12, 24, 48].map(Duration::from_secs));
    assert_eq!(
        requeue_backoff(MAX_REQUEUES, Duration::ZERO, budget, None),
        None
    );
    assert_eq!(requeue_backoff(0, budget, budget, None), None);
    assert_eq!(
        requeue_backoff(0, Duration::ZERO, budget, Some(120)),
        Some(Duration::from_secs(60)),
        "a provider hint raises the wait, capped"
    );
    assert_eq!(
        requeue_backoff(3, Duration::from_secs(590), budget, None),
        Some(Duration::from_secs(10)),
        "the budget clamps the wait"
    );
}

#[test]
fn a_rate_limited_swarm_child_is_requeued_and_completes() {
    let mut scripts = vec![swarm_turn(&[("call_s", swarm_task(&["a", "b"]))])];
    // The child's first run burns the loop's own retries, then the
    // engine requeues it once and the retry succeeds.
    for _ in 0..5 {
        scripts.push(rate_limited());
    }
    scripts.push(text_turn("b done"));
    scripts.push(text_turn("a retry done"));
    scripts.push(text_turn("parent done"));
    let mock = MockProvider::sequence(scripts);
    let h = harness(mock.clone());
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(swarm_setup(1, 60_000)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 4);
    h.engine.shutdown();

    let ended = outcomes(&events);
    assert!(
        matches!(
            &ended[0],
            RunOutcome::Failed {
                error: LoopError::RateLimited {
                    retry_after_secs: Some(_),
                    ..
                }
            }
        ),
        "{ended:?}"
    );
    assert_eq!(
        &ended[1..],
        [
            RunOutcome::Completed,
            RunOutcome::Completed,
            RunOutcome::Completed
        ]
    );
    let output = tool_output(&events, parent, "call_s");
    assert!(
        output
            .text
            .starts_with("completed: 2, failed: 0, cancelled: 0\n"),
        "{}",
        output.text
    );
    assert!(output.text.contains("a retry done"), "{}", output.text);
    assert!(!output.is_error);
    // Parent turn, five failed attempts, b once, a's retry, parent
    // turn.
    assert_eq!(mock.call_count(), 9);
}

#[test]
fn a_rate_limited_plain_agent_is_not_requeued() {
    let mut scripts = vec![agent_turn(&[("call_a", task("do it"))])];
    for _ in 0..5 {
        scripts.push(rate_limited());
    }
    scripts.push(text_turn("parent done"));
    let mock = MockProvider::sequence(scripts);
    let h = harness(mock.clone());
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    h.engine.shutdown();

    let [(child, call)] = &spawned(&events)[..] else {
        panic!("one child expected");
    };
    assert_eq!(call.0, "call_a");
    let child = *child;
    let child_outcome = &outcome_of(&events, child)[0];
    assert!(
        matches!(
            child_outcome,
            RunOutcome::Failed {
                error: LoopError::RateLimited { .. }
            }
        ),
        "{child_outcome:?}"
    );
    assert_eq!(outcome_of(&events, parent), [RunOutcome::Completed]);
    let output = tool_output(&events, parent, "call_a");
    assert!(output.is_error);
    assert!(output.text.contains("state=\"failed\""), "{}", output.text);
    assert_eq!(mock.call_count(), 7);
}

#[test]
fn cancelling_the_parent_mid_swarm_renders_the_fleet_cancelled() {
    let h = harness(MockProvider::sequence(vec![
        swarm_turn(&[("call_s", swarm_task(&["a", "b", "c"]))]),
        tool_turn("gate"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(swarm_setup(1, 60_000)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    wait_for(&h.events, is_tool_start_outside(parent));
    h.engine.send(Command::to(parent, CommandKind::Cancel));
    let events = until_runs_end(&h.events, 4);
    h.engine.shutdown();

    assert_eq!(outcomes(&events), vec![RunOutcome::Cancelled; 4]);
    let output = tool_output(&events, parent, "call_s");
    assert!(
        output
            .text
            .starts_with("completed: 0, failed: 0, cancelled: 3\n"),
        "{}",
        output.text
    );
}

/// One assistant turn with a `swarm` call and a `gate` call together.
fn mixed_turn() -> Vec<Result<ProviderEvent, ProviderError>> {
    let mut turn = vec![Ok(ProviderEvent::MessageStart)];
    for (name, call, input) in [
        ("swarm", "call_s", swarm_task(&["a", "b"])),
        ("gate", "call_g", serde_json::json!({})),
    ] {
        let id = ToolCallId::new(call);
        turn.push(Ok(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: name.into(),
        }));
        turn.push(Ok(ProviderEvent::ToolCallEnd { id, input }));
    }
    turn.push(Ok(ProviderEvent::MessageEnd {
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage::default(),
    }));
    turn
}

#[test]
fn a_swarm_call_beside_another_call_errors_the_swarm_only() {
    let h = harness(MockProvider::sequence(vec![
        mixed_turn(),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(swarm_setup(1, 60_000)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    wait_for(&h.events, is_tool_start);
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 1);
    h.engine.shutdown();

    assert!(spawned(&events).is_empty(), "no swarm children");
    let swarm = tool_output(&events, parent, "call_s");
    assert!(swarm.is_error);
    assert!(
        swarm.text.contains("only tool call in the message"),
        "{}",
        swarm.text
    );
    assert_eq!(tool_output(&events, parent, "call_g").text, "released");
}

#[test]
fn a_swarm_and_an_agent_call_work_in_sequence() {
    let h = harness(MockProvider::sequence(vec![
        swarm_turn(&[("call_s", swarm_task(&["a", "b"]))]),
        text_turn("swarm reply a"),
        text_turn("swarm reply b"),
        agent_turn(&[("call_a", task("one agent"))]),
        text_turn("agent reply"),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(swarm_setup(2, 60_000)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 4);
    h.engine.shutdown();

    let swarm = tool_output(&events, parent, "call_s");
    assert!(
        swarm
            .text
            .starts_with("completed: 2, failed: 0, cancelled: 0\n")
    );
    let agent = tool_output(&events, parent, "call_a");
    assert!(
        agent.text.starts_with(&format!(
            "<agent name=\"general\" session=\"{}\" state=\"completed\" model=\"mock/m\" \
             tools=\"0\" in=\"10\" out=\"2\" cache_read=\"0\" cache_write=\"0\" cost=\"0.0000\" \
             ctx=\"12\" win=\"200000\" run_ms=\"",
            spawned(&events).last().unwrap().0
        )),
        "{}",
        agent.text
    );
    assert!(agent.text.contains("\nagent reply\n</agent>"));
}

#[test]
fn a_swarm_resume_reprompts_children_from_their_files() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(vec![
        swarm_turn(&[("call_s1", swarm_task(&["a", "b"]))]),
        text_turn("child a one"),
        text_turn("child b one"),
        text_turn("parent done"),
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(swarm_setup(1, 60_000)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let first = until_runs_end(&h.events, 3);
    let children = spawned(&first);
    assert_eq!(children.len(), 2);

    let resume = serde_json::json!({
        "description": "a swarm",
        "resume": {
            (children[0].0.to_string()): "continue a",
            (children[1].0.to_string()): "continue b",
        },
    });
    mock.push_script(swarm_turn(&[("call_s2", resume)]));
    mock.push_script(text_turn("child a two"));
    mock.push_script(text_turn("child b two"));
    mock.push_script(text_turn("parent done again"));
    prompt(&h.engine, parent, "again", Delivery::Steer);
    let second = until_runs_end(&h.events, 3);
    h.engine.shutdown();

    let output = tool_output(&second, parent, "call_s2");
    assert!(
        output
            .text
            .starts_with("completed: 2, failed: 0, cancelled: 0\n"),
        "{}",
        output.text
    );
    assert!(output.text.contains("child a two"), "{}", output.text);
    assert!(output.text.contains("child b two"), "{}", output.text);
    assert!(
        output
            .text
            .contains(&format!("session=\"{}\"", children[0].0))
    );
    assert!(
        output
            .text
            .contains(&format!("session=\"{}\"", children[1].0))
    );
    assert!(output.text.contains("item=\"a\""));
    assert!(output.text.contains("item=\"b\""));
    assert!(!output.is_error);
    // The batch reaped both children, so the resume reopens each from
    // its session file and publishes a fresh spawn card, with the same
    // session id.
    let mut reopened: Vec<SessionId> = spawned(&second).into_iter().map(|(id, _)| id).collect();
    reopened.sort();
    let mut first_ids = vec![children[0].0, children[1].0];
    first_ids.sort();
    assert_eq!(reopened, first_ids, "resumed children keep their ids");
}

#[test]
fn a_swarm_resume_refuses_ids_that_are_not_swarm_children() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("look around"))]),
        text_turn("agent reply"),
        text_turn("parent done"),
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(swarm_setup(1, 60_000)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let first = until_runs_end(&h.events, 2);
    let (plain, _) = spawned(&first).remove(0);
    let resume = serde_json::json!({
        "description": "a swarm",
        "resume": {
            (plain.to_string()): "go on",
            (SessionId::new().to_string()): "go on too",
        },
    });
    mock.push_script(swarm_turn(&[("call_s", resume)]));
    mock.push_script(text_turn("done"));
    prompt(&h.engine, parent, "again", Delivery::Steer);
    let second = until_runs_end(&h.events, 1);
    h.engine.shutdown();

    let output = tool_output(&second, parent, "call_s");
    assert!(output.is_error);
    assert!(
        output.text.contains("is not a swarm child of this session"),
        "{}",
        output.text
    );
    assert!(spawned(&second).is_empty(), "nothing attached");
}

#[test]
fn a_resumed_parent_restores_its_swarm_children_from_their_files() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(vec![
        swarm_turn(&[("call_s1", swarm_task(&["a", "b"]))]),
        text_turn("child a one"),
        text_turn("child b one"),
        text_turn("parent done"),
    ]);
    let parent = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), parent);
    let children = {
        let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
        h.engine.open(SessionSpec {
            recorder: Some(recorder),
            agents: Some(swarm_setup(1, 60_000)),
            ..h.spec(parent)
        });
        prompt(&h.engine, parent, "go", Delivery::Steer);
        let first = until_runs_end(&h.events, 3);
        let children = spawned(&first);
        assert_eq!(children.len(), 2);
        h.engine.shutdown();
        children
    };

    // A new engine loads the parent's file: no child is hosted, so a
    // resume has to open them from disk.
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let fresh = SessionId::new();
    h.engine.open(SessionSpec {
        agents: Some(swarm_setup(1, 60_000)),
        ..h.spec(fresh)
    });
    h.engine.send(Command::to(
        fresh,
        CommandKind::LoadSession { path: path.clone() },
    ));
    let seen = wait_for(&h.events, is_session_changed);
    assert_eq!(seen.last().unwrap().session, parent);

    let resume = serde_json::json!({
        "description": "a swarm",
        "resume": {
            (children[0].0.to_string()): "continue a",
            (children[1].0.to_string()): "continue b",
        },
    });
    mock.push_script(swarm_turn(&[("call_s2", resume)]));
    mock.push_script(text_turn("resumed"));
    mock.push_script(text_turn("resumed"));
    mock.push_script(text_turn("parent done again"));
    prompt(&h.engine, parent, "again", Delivery::Steer);
    let second = until_runs_end(&h.events, 3);
    h.engine.shutdown();

    let resumed = spawned(&second);
    assert_eq!(resumed.len(), 2, "both children re-opened");
    assert!(resumed.iter().any(|(id, _)| *id == children[0].0));
    assert!(resumed.iter().any(|(id, _)| *id == children[1].0));
    let output = tool_output(&second, parent, "call_s2");
    assert!(
        output
            .text
            .starts_with("completed: 2, failed: 0, cancelled: 0\n"),
        "{}",
        output.text
    );
    assert_eq!(output.text.matches("resumed").count(), 2, "{}", output.text);

    let replay =
        kage_session::replay(&dir.path().join(format!("{}.jsonl", children[0].0))).unwrap();
    let texts: Vec<String> = replay
        .history
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(texts.iter().any(|t| t == "continue a"), "{texts:?}");
    assert!(texts.iter().any(|t| t == "resumed"), "{texts:?}");
}

#[test]
fn swarm_mode_injects_its_block_once_at_the_next_run() {
    let mock = MockProvider::sequence(vec![text_turn("ok"), text_turn("done")]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let parent = SessionId::new();
    h.open(parent, None);

    h.engine
        .send(Command::to(parent, CommandKind::SwarmMode { on: true }));
    h.engine
        .send(Command::to(parent, CommandKind::SwarmMode { on: true }));
    prompt(&h.engine, parent, "go", Delivery::Steer);
    until_runs_end(&h.events, 1);

    let requests = mock.requests();
    let history = &requests.last().unwrap().messages;
    let on = history
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .filter(|t| t.starts_with("[swarm mode on]"))
        .count();
    assert_eq!(on, 1, "injected once despite the repeated command");

    h.engine
        .send(Command::to(parent, CommandKind::SwarmMode { on: false }));
    prompt(&h.engine, parent, "more", Delivery::Steer);
    until_runs_end(&h.events, 1);

    let requests = mock.requests();
    let history = &requests.last().unwrap().messages;
    let texts: Vec<&str> = history
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts
            .iter()
            .filter(|t| t.starts_with("[swarm mode off]"))
            .count(),
        1
    );
    assert_eq!(
        requests[0]
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .filter(|t| t.starts_with("[swarm mode off]"))
            .count(),
        0,
        "the exit note only lands after the turn off"
    );
}

#[test]
fn swarm_mode_survives_a_restart_without_reinjecting_the_block() {
    let dir = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    let mock = MockProvider::sequence(vec![text_turn("done")]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    h.open(id, Some(recorder));
    h.engine
        .send(Command::to(id, CommandKind::SwarmMode { on: true }));
    prompt(&h.engine, id, "go", Delivery::Steer);
    until_runs_end(&h.events, 1);
    h.engine.shutdown();

    // A fresh engine loads the recorded session and finds the mode on.
    let mock = MockProvider::sequence(vec![text_turn("done again"), text_turn("done more")]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let fresh = SessionId::new();
    h.open(fresh, None);
    h.engine
        .send(Command::to(fresh, CommandKind::LoadSession { path }));
    let seen = wait_for(&h.events, is_session_changed);
    let loaded = seen.last().unwrap().session;

    // Toggling on again must not inject a second block.
    h.engine
        .send(Command::to(loaded, CommandKind::SwarmMode { on: true }));
    prompt(&h.engine, loaded, "more", Delivery::Steer);
    until_runs_end(&h.events, 1);
    let count_on = |req: &kage_provider::StreamRequest| {
        req.messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .filter(|t| t.starts_with("[swarm mode on]"))
            .count()
    };
    let requests = mock.requests();
    assert_eq!(count_on(requests.last().unwrap()), 1);

    // The restored flag is on, so turning it off does inject the note.
    h.engine
        .send(Command::to(loaded, CommandKind::SwarmMode { on: false }));
    prompt(&h.engine, loaded, "even more", Delivery::Steer);
    until_runs_end(&h.events, 1);
    let requests = mock.requests();
    let off = requests
        .last()
        .unwrap()
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .filter(|t| t.starts_with("[swarm mode off]"))
        .count();
    assert_eq!(off, 1, "the restored mode must be on, so off injects");
}

/// One assistant turn that calls `send_message` once.
fn send_message_turn(
    id: &str,
    to: &str,
    message: &str,
) -> Vec<Result<ProviderEvent, ProviderError>> {
    vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::ToolCallStart {
            id: ToolCallId::new(id),
            name: "send_message".into(),
        }),
        Ok(ProviderEvent::ToolCallEnd {
            id: ToolCallId::new(id),
            input: serde_json::json!({"to": to, "message": message}),
        }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }),
    ]
}

#[test]
fn a_child_messages_its_busy_parent_at_its_next_turn() {
    let mock = MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("watch the build"))]),
        send_message_turn("call_m", "parent", "found the bug"),
        text_turn("child done"),
        text_turn("parent done"),
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    h.engine.shutdown();

    let child = spawned(&events).first().unwrap().0;
    let ack = tool_output(&events, child, "call_m");
    assert!(!ack.is_error, "{}", ack.text);
    assert!(ack.text.contains("next turn boundary"), "{}", ack.text);

    let requests = mock.requests();
    let last = requests.last().unwrap().messages.clone();
    let mail = AgentMail {
        from: "general".into(),
        session: child,
        body: "found the bug".into(),
    };
    assert!(
        last.iter()
            .flat_map(|m| &m.content)
            .any(|c| matches!(c, Content::Text { text } if *text == mail.to_text())),
        "{last:?}"
    );
    assert_eq!(outcome_of(&events, parent), [RunOutcome::Completed]);
}

#[test]
fn a_message_to_a_missing_target_or_parent_is_refused() {
    let h = harness(MockProvider::sequence(vec![
        send_message_turn("call_m1", "parent", "anyone there?"),
        send_message_turn("call_m2", &SessionId::new().to_string(), "hello?"),
        text_turn("done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    h.engine.shutdown();

    let orphan = tool_output(&events, parent, "call_m1");
    assert!(orphan.is_error);
    assert!(orphan.text.contains("has no parent"), "{}", orphan.text);
    let stranger = tool_output(&events, parent, "call_m2");
    assert!(stranger.is_error);
    assert!(
        stranger
            .text
            .contains("is not an agent of this conversation"),
        "{}",
        stranger.text
    );
}

/// A swarm task whose children start from the parent's snapshot.
fn forked_swarm_task(items: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "description": "a swarm",
        "prompt_template": "handle {{item}}",
        "items": items,
        "fork": true,
    })
}

#[test]
fn a_forked_swarm_child_starts_from_the_parent_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::sequence(vec![
        swarm_turn(&[("call_s", forked_swarm_task(&["a", "b"]))]),
        text_turn("child a one"),
        text_turn("child b one"),
        text_turn("parent done"),
    ]));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(swarm_setup(1, 60_000)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 3);
    h.engine.shutdown();

    let output = tool_output(&events, parent, "call_s");
    assert!(
        output
            .text
            .starts_with("completed: 2, failed: 0, cancelled: 0\n"),
        "{}",
        output.text
    );
    let children = spawned(&events);
    assert_eq!(children.len(), 2);
    for ((child, _call), (item, reply)) in children
        .iter()
        .zip([("a", "child a one"), ("b", "child b one")])
    {
        let path = dir.path().join(format!("{child}.jsonl"));
        let replay = kage_session::replay(&path).unwrap();
        assert_eq!(replay.header.parent_session, Some(parent));
        let texts: Vec<String> = replay
            .history
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts.first().map(String::as_str), Some("go"), "{texts:?}");
        let notice = texts
            .iter()
            .position(|t| t.contains("snapshot inherited from the session that forked you"));
        let task = texts.iter().position(|t| t == &format!("handle {item}"));
        assert!(notice.is_some(), "{texts:?}");
        assert!(task.is_some(), "{texts:?}");
        assert!(notice.unwrap() < task.unwrap(), "{texts:?}");
        assert!(texts.contains(&reply.to_owned()), "{texts:?}");
        let entries: Vec<kage_session::SessionEntry> = kage_session::SessionReader::iter(&path)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(entries.iter().any(|e| matches!(
            e,
            kage_session::SessionEntry::Custom(c)
                if c.kind == kage_session::list::AGENT_ENTRY_KIND
        )));
    }
}

#[test]
fn a_fork_refuses_when_the_parent_is_not_recorded() {
    let h = harness(MockProvider::sequence(vec![
        swarm_turn(&[("call_s", forked_swarm_task(&["a", "b"]))]),
        text_turn("done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(swarm_setup(1, 60_000)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    h.engine.shutdown();

    let output = tool_output(&events, parent, "call_s");
    assert!(output.is_error);
    assert!(output.text.contains("cannot fork"), "{}", output.text);
    assert!(output.text.contains("not recorded"), "{}", output.text);
    assert!(spawned(&events).is_empty(), "nothing spawned");
}

#[test]
fn print_mode_text_names_agents_and_how_they_ended() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[
            ("call_a", task("look")),
            (
                "call_b",
                serde_json::json!({"agent": "explore", "description": "map it", "prompt": "p"}),
            ),
        ]),
        text_turn("child reply"),
        vec![Err(ProviderError::Auth("bad key".into()))],
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 3);

    let mut text = Vec::new();
    for envelope in &events {
        crate::cli_loop_run::print_envelope(&mut text, envelope, parent, false);
    }
    let text = String::from_utf8(text).unwrap();
    let name = |call: &ToolCallId| {
        if call.0 == "call_a" {
            "general"
        } else {
            "explore"
        }
    };
    let children = spawned(&events);
    let (first, second) = (name(&children[0].1), name(&children[1].1));
    let (ends, last) = text
        .strip_prefix("\n[agent general: a task]\n\n[agent explore: map it]\n")
        .and_then(|rest| rest.rsplit_once('\n'))
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(last, "parent done");
    let mut ends: Vec<&str> = ends.lines().collect();
    ends.sort_unstable();
    let mut expected = [
        format!("[agent {first} completed]"),
        format!("[agent {second} failed] authentication failed: bad key"),
    ];
    expected.sort_unstable();
    assert_eq!(ends, expected, "{text}");
}

/// An in-process MCP server with tool `t`, resource `test://doc` whose
/// text is `doc body`, and prompt `greet(name)`. Reading any other URI
/// fails with `resource not found`.
fn mcp_connection() -> Arc<kage_mcp::McpConnection> {
    use kage_jsonrpc::{Inbound, RpcError};
    use serde_json::json;

    let (cli_r, srv_w) = std::io::pipe().unwrap();
    let (srv_r, cli_w) = std::io::pipe().unwrap();
    let (cli_peer, cli_in, _c) = kage_jsonrpc::connect(std::io::BufReader::new(cli_r), cli_w);
    let (srv_peer, srv_in, _s) = kage_jsonrpc::connect(std::io::BufReader::new(srv_r), srv_w);
    std::thread::spawn(move || {
        for msg in srv_in {
            let Inbound::Request { id, method, params } = msg else {
                continue;
            };
            let outcome = match method.as_str() {
                "initialize" => Ok(json!({
                    "protocolVersion": kage_mcp::PROTOCOL_VERSION,
                    "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
                })),
                "tools/list" => Ok(json!({ "tools": [{ "name": "t", "inputSchema": {} }] })),
                "resources/list" => Ok(json!({
                    "resources": [{ "uri": "test://doc", "name": "Doc", "mimeType": "text/plain" }],
                })),
                "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
                "prompts/list" => Ok(json!({
                    "prompts": [{ "name": "greet", "arguments": [{ "name": "name", "required": true }] }],
                })),
                "prompts/get" => Ok(json!({ "messages": [{
                    "role": "user",
                    "content": { "type": "text", "text": format!("Hello, {}", params["arguments"]["name"].as_str().unwrap()) },
                }] })),
                "resources/read" if params["uri"] == "test://doc" => Ok(json!({
                    "contents": [{ "uri": "test://doc", "mimeType": "text/plain", "text": "doc body" }],
                })),
                "resources/read" => Err(RpcError::new(-32002, "resource not found")),
                other => Err(RpcError::method_not_found(other)),
            };
            let _ = srv_peer.respond(&id, outcome);
        }
    });
    let conn = kage_mcp::McpConnection::initialize("srv", cli_peer, cli_in, &[], None).unwrap();
    Arc::new(conn)
}

const BROKEN_COMMAND: &str = "definitely-not-a-real-binary-xyz";

/// A manager with `broken`, a server that cannot spawn, and the live
/// server `srv`, whose tools are registered into `tools`.
fn mcp_manager(tools: &mut ToolRegistry) -> McpManager {
    let cfg: kage_core::config::McpConfig = serde_json::from_value(serde_json::json!({
        "servers": { "broken": { "command": BROKEN_COMMAND } },
    }))
    .unwrap();
    let (mut mcp, _errors) = McpManager::spawn_all(&cfg, Vec::new(), None);
    mcp.adopt("srv", mcp_connection());
    assert!(mcp.register_into(tools).is_empty());
    mcp
}

impl Harness {
    fn open_mcp(&self, id: SessionId, recorder: Option<Recorder>) {
        let mut tools = self.tools.clone();
        let mcp = mcp_manager(&mut tools);
        self.engine.open(SessionSpec {
            recorder,
            tools,
            mcp: Some(mcp),
            ..self.spec(id)
        });
    }
}

fn mcp_snapshots(events: &[Envelope]) -> Vec<Vec<kage_core::protocol::McpServerInfo>> {
    host_events(events)
        .into_iter()
        .filter_map(|e| match e {
            HostEvent::McpServers { servers } => Some(servers.clone()),
            _ => None,
        })
        .collect()
}

fn is_mcp_servers(envelope: &Envelope) -> bool {
    matches!(envelope.event, Event::Host(HostEvent::McpServers { .. }))
}

fn first_user_content(events: &[Envelope]) -> Option<Vec<Content>> {
    events.iter().find_map(|e| match &e.event {
        Event::Loop(LoopEvent::MessageAppended { message }) if message.role == Role::User => {
            Some(message.content.clone())
        }
        _ => None,
    })
}

fn text(text: &str) -> Content {
    Content::Text { text: text.into() }
}

#[test]
fn opening_publishes_the_mcp_catalog() {
    use kage_core::protocol::McpServerStatus;

    let h = harness(MockProvider::replaying(text_turn("ok")));
    let (plain, id) = (SessionId::new(), SessionId::new());
    h.open(plain, None);
    h.open_mcp(id, None);
    let events = wait_for(&h.events, |e| e.session == id && is_mcp_servers(e));
    let (plain_events, events): (Vec<Envelope>, Vec<Envelope>) =
        events.into_iter().partition(|e| e.session == plain);
    assert_eq!(mcp_snapshots(&plain_events), [Vec::new()]);

    let servers = mcp_snapshots(&events).remove(0);
    let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["broken", "srv"]);
    assert!(
        matches!(&servers[0].status, McpServerStatus::Failed { error } if error.contains(BROKEN_COMMAND))
    );
    assert_eq!(servers[1].status, McpServerStatus::Connected);
    assert_eq!(servers[1].tools, 1);
    assert_eq!(servers[1].resources[0].uri, "test://doc");
    assert_eq!(servers[1].prompts[0].name, "greet");

    h.engine.send(Command::to(id, CommandKind::Compact));
    let events = until_runs_end(&h.events, 1);
    assert!(
        mcp_snapshots(&events).is_empty(),
        "an unchanged catalog is not republished"
    );
}

#[test]
fn a_mention_is_expanded_recorded_and_sent() {
    let dir = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    let mock = MockProvider::replaying(text_turn("ok"));
    let h = harness(mock.clone());
    h.open_mcp(id, Some(recorder));
    let typed = "what is in @srv:test://doc.";
    prompt(&h.engine, id, typed, Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    h.engine.shutdown();

    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
    let expected = vec![
        text(typed),
        text(&kage_core::resource_block::render(
            "test://doc",
            Some("srv"),
            Some("text/plain"),
            "doc body",
        )),
    ];
    assert_eq!(first_user_content(&events), Some(expected.clone()));
    assert_eq!(mock.last_request().unwrap().messages[0].content, expected);
    let replay = kage_session::replay(&path).unwrap();
    assert_eq!(replay.history[0].content, expected);
}

#[test]
fn a_prompt_command_runs_the_mcp_prompt() {
    let mock = MockProvider::replaying(text_turn("ok"));
    let h = harness(mock.clone());
    let id = SessionId::new();
    h.open_mcp(id, None);
    prompt(&h.engine, id, "/srv:greet Ada Lovelace", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);

    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
    // The expansion keeps the typed text after the prompt messages, so
    // the model sees both the greeting and the original words.
    assert_eq!(
        mock.last_request().unwrap().messages[0].content,
        [text("Hello, Ada Lovelace"), text("Ada Lovelace")]
    );
}

#[test]
fn a_failing_read_fails_the_run_and_leaves_history_alone() {
    let dir = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    let mock = MockProvider::replaying(text_turn("ok"));
    let h = harness(mock.clone());
    h.open_mcp(id, Some(recorder));
    prompt(&h.engine, id, "read @srv:test://missing", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    h.engine.shutdown();

    let message = "mcp srv: read test://missing: resource not found".to_owned();
    assert!(notices(&events).contains(&message));
    assert_eq!(
        outcomes(&events),
        [RunOutcome::Failed {
            error: LoopError::Other { message }
        }]
    );
    assert_eq!(first_user_content(&events), None);
    assert_eq!(mock.call_count(), 0);
    assert!(kage_session::replay(&path).unwrap().history.is_empty());
}

fn restart(engine: &Engine, id: SessionId, server: &str) {
    engine.send(Command::to(
        id,
        CommandKind::RestartMcp {
            server: server.into(),
        },
    ));
}

fn restart_notices(events: &[Envelope]) -> Vec<String> {
    notices(events)
        .into_iter()
        .filter(|n| n.starts_with("mcp restart"))
        .collect()
}

#[test]
fn restart_while_idle_republishes_the_catalog() {
    use kage_core::protocol::McpServerStatus;

    let h = harness(MockProvider::replaying(text_turn("ok")));
    let id = SessionId::new();
    h.open_mcp(id, None);
    wait_for(&h.events, is_mcp_servers);

    restart(&h.engine, id, "broken");
    let events = wait_for(&h.events, is_mcp_servers);
    let notices = restart_notices(&events);
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].starts_with(&format!("mcp restart `broken`: spawn `{BROKEN_COMMAND}`")),
        "{notices:?}"
    );
    let servers = mcp_snapshots(&events).remove(0);
    assert!(matches!(servers[0].status, McpServerStatus::Failed { .. }));
    assert_eq!(servers[1].status, McpServerStatus::Connected);

    restart(&h.engine, id, "ghost");
    let events = wait_for(&h.events, is_mcp_servers);
    assert_eq!(
        restart_notices(&events),
        ["mcp restart `ghost`: no mcp server named `ghost`"]
    );
}

#[test]
fn a_prompt_during_an_idle_restart_runs_after_it() {
    let mock = MockProvider::replaying(text_turn("ok"));
    let h = harness(mock.clone());
    let id = SessionId::new();
    h.open_mcp(id, None);
    wait_for(&h.events, is_mcp_servers);

    restart(&h.engine, id, "broken");
    prompt(&h.engine, id, "@srv:test://doc", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);

    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
    let republished = events.iter().position(is_mcp_servers).unwrap();
    let started = events
        .iter()
        .position(|e| matches!(e.event, Event::Host(HostEvent::RunStarted)))
        .unwrap();
    assert!(republished < started);
    assert_eq!(mock.last_request().unwrap().messages[0].content.len(), 2);
}

/// A manager whose servers are configured but not started, as the TUI
/// hands them over so its UI is up before MCP connects.
fn deferred_mcp_manager() -> McpManager {
    let cfg: kage_core::config::McpConfig = serde_json::from_value(serde_json::json!({
        "servers": { "broken": { "command": BROKEN_COMMAND } },
    }))
    .unwrap();
    McpManager::unstarted(&cfg, Vec::new(), None, None)
}

#[test]
fn deferred_mcp_servers_start_after_open() {
    use kage_core::protocol::McpServerStatus;

    let h = harness(MockProvider::replaying(text_turn("ok")));
    let id = SessionId::new();
    h.engine.open(SessionSpec {
        tools: h.tools.clone(),
        mcp: Some(deferred_mcp_manager()),
        ..h.spec(id)
    });

    // Opening publishes the catalog with the server still starting.
    let events = wait_for(&h.events, |e| e.session == id && is_mcp_servers(e));
    let servers = mcp_snapshots(&events).remove(0);
    assert_eq!(servers[0].name, "broken");
    assert_eq!(servers[0].status, McpServerStatus::Starting);
    assert_eq!(servers[0].tools, 0);

    // The worker starts it off the dispatcher, fails, and the catalog
    // flips to failed.
    let events = wait_for(&h.events, |e| {
        e.session == id
            && matches!(
                &e.event,
                Event::Host(HostEvent::McpServers { servers })
                    if matches!(servers[0].status, McpServerStatus::Failed { .. })
            )
    });
    assert_eq!(restart_notices(&events).len(), 1);

    // The session came back idle, so a prompt runs.
    prompt(&h.engine, id, "hi", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
}

#[test]
fn restart_while_running_waits_for_the_next_run() {
    let h = harness(MockProvider::sequence(vec![
        tool_turn("gate"),
        text_turn("done"),
        text_turn("again"),
    ]));
    let id = SessionId::new();
    h.open_mcp(id, None);
    prompt(&h.engine, id, "go", Delivery::Steer);
    wait_for(&h.events, is_tool_start);

    restart(&h.engine, id, "broken");
    h.engine.send(Command::to(
        id,
        CommandKind::SetThinking {
            level: Some(ThinkingLevel::High),
        },
    ));
    let mut events = wait_for(&h.events, |e| {
        state_of(e).is_some_and(|s| s.thinking == Some(ThinkingLevel::High))
    });
    h.release.send(()).unwrap();
    events.extend(until_runs_end(&h.events, 1));
    assert!(restart_notices(&events).is_empty());
    assert!(mcp_snapshots(&events).is_empty());

    prompt(&h.engine, id, "again", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    assert_eq!(restart_notices(&events).len(), 1);
    assert_eq!(mcp_snapshots(&events).len(), 1);
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
}

#[test]
fn switching_sessions_publishes_the_mcp_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("hello")));
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    h.open_mcp(id, Some(recorder));
    prompt(&h.engine, id, "hi", Delivery::Steer);
    until_runs_end(&h.events, 1);

    let switch = |from: SessionId, kind: CommandKind| {
        h.engine.send(Command::to(from, kind));
        let events = wait_for(&h.events, is_mcp_servers);
        let changed = events.iter().rfind(|e| is_session_changed(e)).unwrap();
        let catalog = events.last().unwrap();
        assert_eq!(catalog.session, changed.session);
        let names: Vec<String> = mcp_snapshots(&events)
            .remove(0)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["broken", "srv"]);
        catalog.session
    };
    let cloned = switch(id, CommandKind::Clone);
    let fresh = switch(cloned, CommandKind::NewSession);
    assert_eq!(switch(fresh, CommandKind::LoadSession { path }), id);
}

/// A mock provider that declares one model's thinking and inputs, like
/// a custom provider from config.
#[derive(Debug)]
struct Declared {
    mock: MockProvider,
    model: kage_provider::ProviderModel,
}

impl kage_provider::Provider for Declared {
    fn metadata(&self) -> &kage_provider::ProviderMetadata {
        self.mock.metadata()
    }

    fn stream(
        &self,
        req: kage_provider::StreamRequest,
        cancel: &CancelFlag,
    ) -> Result<kage_provider::EventStream, ProviderError> {
        self.mock.stream(req, cancel)
    }

    fn models(&self) -> Vec<kage_provider::ProviderModel> {
        vec![self.model.clone()]
    }
}

fn windowed(mock: MockProvider, context: u64) -> ProviderRegistry {
    let declared = Declared {
        mock,
        model: kage_provider::ProviderModel {
            id: "m".into(),
            context: Some(context),
            ..kage_provider::ProviderModel::default()
        },
    };
    ProviderRegistry::new().with(Arc::new(declared))
}

fn window_of(event: &Envelope) -> Option<u64> {
    match &event.event {
        Event::Host(HostEvent::UsageUpdated { usage }) => Some(usage.context_window),
        _ => None,
    }
}

#[test]
fn a_model_switch_refreshes_the_context_window() {
    let h = harness_on(windowed(MockProvider::replaying(text_turn("ok")), 500_000));
    let id = SessionId::new();
    h.open(id, None);

    set_model(&h.engine, id, "mock/other");
    set_model(&h.engine, id, "mock/m");
    wait_for(&h.events, |e| window_of(e) == Some(500_000));
}

#[test]
fn a_loaded_session_reports_its_own_model_window() {
    let dir = tempfile::tempdir().unwrap();
    let first = harness_on(windowed(MockProvider::replaying(text_turn("ok")), 500_000));
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    first.open(id, Some(recorder));
    first.engine.shutdown();

    let h = harness_on(windowed(MockProvider::replaying(text_turn("ok")), 500_000));
    let fresh = SessionId::new();
    h.open(fresh, None);
    set_model(&h.engine, fresh, "mock/other");
    wait_for(&h.events, |e| window_of(e).is_some());
    h.engine
        .send(Command::to(fresh, CommandKind::LoadSession { path }));
    let seen = wait_for(&h.events, is_session_changed);
    let loaded = seen.last().unwrap().session;
    let seen = wait_for(&h.events, |e| e.session == loaded && window_of(e).is_some());
    assert_eq!(window_of(seen.last().unwrap()), Some(500_000));
}

#[test]
fn thinking_fits_the_model_and_images_skip_text_only_models() {
    use kage_core::{Effort, Efforts, Modalities, Modality, Reasoning};
    let mock = MockProvider::replaying(text_turn("ok"));
    let reasoning = Reasoning::Effort {
        efforts: Efforts::of(&[Effort::Low, Effort::Medium]),
        toggle: false,
    };
    let declared = Declared {
        mock: mock.clone(),
        model: kage_provider::ProviderModel {
            id: "m".into(),
            reasoning,
            input: Modalities::of(&[Modality::Text]),
            ..kage_provider::ProviderModel::default()
        },
    };
    let h = harness_on(ProviderRegistry::new().with(Arc::new(declared)));
    let id = SessionId::new();
    h.open(id, None);
    let seen = wait_for(&h.events, |e| state_of(e).is_some());
    let state = seen.iter().find_map(state_of).unwrap();
    assert_eq!(state.thinking, None);
    assert_eq!(state.thinking_effective, Some(ThinkingLevel::Medium));
    assert_eq!(
        state.thinking_levels,
        [ThinkingLevel::Low, ThinkingLevel::Medium]
    );

    h.engine.send(Command::to(
        id,
        CommandKind::Prompt {
            content: vec![
                Content::Text {
                    text: "look".into(),
                },
                Content::Image {
                    source: kage_core::MediaSource::Base64 {
                        data: "AA==".into(),
                    },
                    mime: "image/png".into(),
                },
            ],
            delivery: Delivery::Steer,
        },
    ));
    let events = until_runs_end(&h.events, 1);
    assert!(
        notices(&events)
            .iter()
            .any(|n| n.contains("does not accept images")),
        "{:?}",
        notices(&events)
    );
    let request = mock.last_request().unwrap();
    assert_eq!(request.level, Some(ThinkingLevel::Medium));
    assert_eq!(request.reasoning, reasoning);
    let sent = &request.messages.last().unwrap().content;
    assert!(sent.iter().all(|c| !matches!(c, Content::Image { .. })));
}

#[test]
fn run_cost_comes_from_the_declared_model_price() {
    let declared = Declared {
        mock: MockProvider::replaying(text_turn("ok")),
        model: kage_provider::ProviderModel {
            id: "m".into(),
            cost: Some(kage_core::ModelCost {
                input: 100.0,
                output: 1000.0,
                cache_read: None,
                cache_write: None,
            }),
            ..kage_provider::ProviderModel::default()
        },
    };
    let h = harness_on(ProviderRegistry::new().with(Arc::new(declared)));
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "hi", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    let usage = events.iter().rev().find_map(|e| match &e.event {
        Event::Host(HostEvent::UsageUpdated { usage }) => Some(*usage),
        _ => None,
    });
    assert!((usage.unwrap().cost - 0.003).abs() < 1e-12);
}

/// A tool of a given risk that reports it ran.
#[derive(Debug)]
struct Stub {
    name: &'static str,
    risk: kage_core::Risk,
}

impl Tool for Stub {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &'static str {
        "a stub"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> kage_core::Risk {
        self.risk
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            text: format!("ran {}", self.name),
            ..ToolOutput::default()
        })
    }
}

fn plan_turn(plan: &str) -> Vec<Result<ProviderEvent, ProviderError>> {
    let id = ToolCallId::new("call_plan");
    vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: EXIT_PLAN_TOOL.into(),
        }),
        Ok(ProviderEvent::ToolCallEnd {
            id,
            input: serde_json::json!({ "plan": plan }),
        }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }),
    ]
}

/// Open `id` with a write stub and a command stub next to the harness
/// tools, and turn plan mode on.
fn open_planning(h: &Harness, id: SessionId, recorder: Option<Recorder>) {
    let mut tools = h.tools.clone();
    tools.register(Arc::new(Stub {
        name: "write_stub",
        risk: kage_core::Risk::Write,
    }));
    tools.register(Arc::new(Stub {
        name: "exec_stub",
        risk: kage_core::Risk::Exec,
    }));
    h.engine.open(SessionSpec {
        recorder,
        tools,
        ..h.spec(id)
    });
    h.engine
        .send(Command::to(id, CommandKind::PlanMode { on: true }));
}

fn tool_outputs(events: &[Envelope]) -> Vec<ToolOutput> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Loop(LoopEvent::ToolCallEnd { output, .. }) => Some(output.clone()),
            _ => None,
        })
        .collect()
}

fn last_plan_state(events: &[Envelope]) -> Option<bool> {
    host_events(events).into_iter().rev().find_map(|e| match e {
        HostEvent::StateChanged { state } => Some(state.plan),
        _ => None,
    })
}

fn texts_starting(req: &kage_provider::StreamRequest, prefix: &str) -> usize {
    req.messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|c| matches!(c, Content::Text { text } if text.starts_with(prefix)))
        .count()
}

#[test]
fn plan_mode_refuses_writes_and_asks_before_commands() {
    let mock = MockProvider::sequence(vec![
        tool_turn("write_stub"),
        tool_turn("exec_stub"),
        tool_turn("gate"),
        text_turn("done"),
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let id = SessionId::new();
    open_planning(&h, id, None);
    h.engine.send(Command::to(
        id,
        CommandKind::SetPermissionMode {
            mode: Some(PermissionAction::Allow),
        },
    ));
    h.engine
        .send(Command::to(id, CommandKind::PlanMode { on: true }));
    prompt(&h.engine, id, "plan it", Delivery::Steer);

    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::PermissionRequested { .. }))
    });
    let asked = host_events(&seen).into_iter().find_map(|e| match e {
        HostEvent::PermissionRequested {
            request_id, tool, ..
        } => Some((*request_id, tool.clone())),
        _ => None,
    });
    let (request_id, tool) = asked.unwrap();
    assert_eq!(tool, "exec_stub", "commands ask even in allow mode");
    h.engine.send(Command::to(
        id,
        CommandKind::ResolvePermission {
            request_id,
            decision: PermissionDecision::AllowOnce,
        },
    ));
    h.release.send(()).unwrap();
    let mut events = seen;
    events.extend(until_runs_end(&h.events, 1));

    let outputs = tool_outputs(&events);
    assert!(outputs[0].is_error);
    assert!(
        outputs[0].text.contains("plan mode is on"),
        "{}",
        outputs[0].text
    );
    assert_eq!(outputs[1].text, "ran exec_stub");
    assert_eq!(outputs[2].text, "released", "read tools follow the rules");
    assert_eq!(last_plan_state(&events), Some(true));

    let requests = mock.requests();
    assert_eq!(
        texts_starting(&requests[0], "[plan mode on]"),
        1,
        "injected once"
    );
    assert!(
        requests[0].tools.iter().any(|t| t.name == EXIT_PLAN_TOOL),
        "exit_plan is offered in plan mode"
    );
}

#[test]
fn an_approved_plan_turns_plan_mode_off_and_the_run_goes_on() {
    let mock = MockProvider::sequence(vec![
        plan_turn("# Fix\n\n1. Edit a.rs"),
        tool_turn("write_stub"),
        text_turn("built"),
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let id = SessionId::new();
    open_planning(&h, id, None);
    prompt(&h.engine, id, "plan it", Delivery::Steer);

    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::PermissionRequested { .. }))
    });
    let (request_id, tool, input) = host_events(&seen)
        .into_iter()
        .find_map(|e| match e {
            HostEvent::PermissionRequested {
                request_id,
                tool,
                input,
                ..
            } => Some((*request_id, tool.clone(), input.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(tool, EXIT_PLAN_TOOL);
    assert_eq!(input["plan"], "# Fix\n\n1. Edit a.rs");
    // An allow for the session must not outlive this one review.
    h.engine.send(Command::to(
        id,
        CommandKind::ResolvePermission {
            request_id,
            decision: PermissionDecision::AllowSession,
        },
    ));
    let mut events = seen;
    events.extend(until_runs_end(&h.events, 1));

    let outputs = tool_outputs(&events);
    assert!(outputs[0].text.contains("approved"), "{}", outputs[0].text);
    assert_eq!(
        outputs[1].text, "ran write_stub",
        "writes run once approved"
    );
    assert_eq!(outputs.len(), 2);
    assert_eq!(last_plan_state(&events), Some(false));
    assert!(
        notices(&events)
            .iter()
            .any(|n| n == "plan approved; plan mode off"),
        "{:?}",
        notices(&events)
    );
    assert_eq!(mock.requests().len(), 3);
}

#[test]
fn a_plan_that_is_not_approved_ends_the_run_and_keeps_plan_mode() {
    let mock = MockProvider::sequence(vec![plan_turn("# Fix"), text_turn("never")]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let id = SessionId::new();
    open_planning(&h, id, None);
    prompt(&h.engine, id, "plan it", Delivery::Steer);

    let (request_id, _) = permission_request(&h.events);
    h.engine.send(Command::to(
        id,
        CommandKind::ResolvePermission {
            request_id,
            decision: PermissionDecision::Deny,
        },
    ));
    let events = until_runs_end(&h.events, 1);

    let outputs = tool_outputs(&events);
    assert!(outputs[0].is_error);
    assert!(
        outputs[0].text.contains("did not approve"),
        "{}",
        outputs[0].text
    );
    assert_eq!(mock.requests().len(), 1, "the run ends at the refusal");
    assert!(matches!(outcomes(&events)[..], [RunOutcome::Completed]));

    h.engine
        .send(Command::to(id, CommandKind::PlanMode { on: false }));
    let seen = wait_for(
        &h.events,
        |e| matches!(&e.event, Event::Host(HostEvent::Notice { text, .. }) if text == "plan mode off"),
    );
    assert_eq!(last_plan_state(&seen), Some(false));
}

#[test]
fn plan_mode_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    let h = harness(MockProvider::sequence(vec![text_turn("ok")]));
    open_planning(&h, id, Some(recorder));
    prompt(&h.engine, id, "go", Delivery::Steer);
    until_runs_end(&h.events, 1);
    h.engine.shutdown();

    let h = harness(MockProvider::sequence(vec![]));
    let fresh = SessionId::new();
    h.open(fresh, None);
    h.engine
        .send(Command::to(fresh, CommandKind::LoadSession { path }));
    let seen = wait_for(
        &h.events,
        |e| matches!(&e.event, Event::Host(HostEvent::StateChanged { state }) if state.plan),
    );
    assert_eq!(last_plan_state(&seen), Some(true));
}

#[test]
fn finishing_a_swarm_child_reaps_it() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::sequence(vec![
        swarm_turn(&[("call_s", swarm_task(&["a", "b"]))]),
        text_turn("a reply"),
        text_turn("b reply"),
        text_turn("parent done"),
    ]));
    let (recorder, _) = recorder_in(dir.path(), SessionId::new());
    let parent = h.open_parent(
        Some(recorder),
        PermissionGate::new(PermissionsConfig::default()),
        Some(swarm_setup(2, 60_000)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 3);
    let children: Vec<SessionId> = spawned(&events).into_iter().map(|(id, _)| id).collect();
    assert_eq!(children.len(), 2);

    let hosted = h.engine.hosted_sessions();
    assert!(
        hosted.iter().any(|(id, _)| *id == parent),
        "the parent stays hosted"
    );
    for child in &children {
        assert!(
            !hosted.iter().any(|(id, _)| id == child),
            "child {child} should be reaped"
        );
        // The transcript survives in the session file the reaped child
        // was writing.
        let entries: Vec<kage_session::SessionEntry> =
            kage_session::SessionReader::iter(dir.path().join(format!("{child}.jsonl")))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, kage_session::SessionEntry::Message(_)))
        );
    }
    h.engine.shutdown();
}

#[test]
fn a_reaped_swarm_child_resumes_from_its_file() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(vec![
        swarm_turn(&[("call_s", swarm_task(&["a", "b"]))]),
        text_turn("a reply"),
        text_turn("b reply"),
        text_turn("parent done"),
        text_turn("resumed reply"),
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(swarm_setup(1, 60_000)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 3);
    let (child, _) = spawned(&events)[0].clone();
    assert!(
        !h.engine
            .hosted_sessions()
            .iter()
            .any(|(id, _)| *id == child)
    );

    // Re-prompt the reaped child the way a second swarm call would.
    let (reply, reply_rx) = crossbeam_channel::bounded(1);
    let _ = h.engine.commander.0.send(Input::Attach(Box::new(Attach {
        parent,
        id: child,
        agent: "general".into(),
        description: "a swarm".into(),
        tool_call_id: ToolCallId::new("call_r"),
        prompt: "continue".into(),
        reply,
        swarm: None,
    })));

    // The file path publishes a fresh spawn card; the hosted path does
    // not.
    let reopened = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::AgentSpawned { .. })) && e.session == child
    });
    assert!(matches!(
        &reopened.last().unwrap().event,
        Event::Host(HostEvent::AgentSpawned { parent: p, .. }) if *p == parent
    ));
    let output = reply_rx.recv_timeout(WAIT).expect("no reply");
    assert!(!output.is_error, "{}", output.text);
    assert!(output.text.contains("resumed reply"), "{}", output.text);
    let request = mock.last_request().unwrap();
    let texts: Vec<String> = request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts.last().map(String::as_str), Some("continue"));
    assert_eq!(texts.len(), 3, "the recorded history came back: {texts:?}");
    assert!(
        !h.engine
            .hosted_sessions()
            .iter()
            .any(|(id, _)| *id == child)
    );
    h.engine.shutdown();
}

#[test]
fn a_run_ending_mid_ask_resolves_the_pending_ask() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("ask"))]),
        tool_turn("gate"),
    ]));
    let parent = h.open_parent(None, ask_for_gate(), Some(agent_setup(1, 1)));
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let child = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::PermissionRequested { .. }))
    })
    .last()
    .unwrap()
    .session;
    h.engine.send(Command::to(parent, CommandKind::Cancel));
    let mut events = until_runs_end(&h.events, 2);
    // The denial is published after the child's RunEnded, sometimes
    // after the parent's too, so drain until it shows up.
    let count = |events: &[Envelope]| {
        events
            .iter()
            .filter(|e| {
                e.session == child
                    && matches!(e.event, Event::Host(HostEvent::PermissionResolved { .. }))
            })
            .count()
    };
    while count(&events) == 0 {
        let envelope = h.events.recv_timeout(WAIT).expect("engine stalled");
        events.push(envelope);
    }
    assert_eq!(count(&events), 1, "the ask is denied once, unprompted");
    h.engine.shutdown();
}

#[test]
fn a_queued_fork_child_loads_its_snapshot_at_its_first_run() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::sequence(vec![
        swarm_turn(&[("call_s", forked_swarm_task(&["a", "b"]))]),
        tool_turn("gate"),
        text_turn("child a one"),
        text_turn("child b one"),
        text_turn("parent done"),
    ]));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(swarm_setup(1, 60_000)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    // Child "a" runs and blocks in the gate tool, so child "b" stays
    // queued behind the running limit. Child ids come from everything
    // seen so far, since b's spawn card may land before or after a's
    // tool call.
    let mut seen = wait_for(&h.events, |e| is_tool_start(e) && e.session != parent);
    let a = seen.last().unwrap().session;
    let b = loop {
        if let Some(b) = spawned(&seen)
            .into_iter()
            .map(|(id, _)| id)
            .find(|id| *id != a)
        {
            break b;
        }
        let more = wait_for(&h.events, |e| {
            matches!(e.event, Event::Host(HostEvent::AgentSpawned { .. }))
        });
        seen.extend(more);
    };
    let hosted = h.engine.hosted_sessions();
    assert_eq!(
        hosted.iter().find(|(id, _)| *id == b),
        Some(&(b, Some(0))),
        "a queued fork child holds no snapshot"
    );
    h.release.send(()).unwrap();

    let events = until_runs_end(&h.events, 3);
    h.engine.shutdown();
    let output = tool_output(&events, parent, "call_s");
    assert!(
        output
            .text
            .starts_with("completed: 2, failed: 0, cancelled: 0\n"),
        "{}",
        output.text
    );
    // The snapshot was there when the queued child's first prompt
    // ran: the prompt chains onto the fork notice.
    let prompt_of_b = events
        .iter()
        .find_map(|e| match &e.event {
            Event::Loop(LoopEvent::MessageAppended { message })
                if e.session == b && message.role == Role::User =>
            {
                Some(message.parent.is_some())
            }
            _ => None,
        })
        .expect("child b appended its prompt");
    assert!(prompt_of_b, "the snapshot loaded before the first prompt");
}

#[test]
fn an_image_only_prompt_to_a_text_only_model_ends_the_run_at_once() {
    use kage_core::{Modalities, Modality};
    let declared = Declared {
        mock: MockProvider::replaying(text_turn("ok")),
        model: kage_provider::ProviderModel {
            id: "m".into(),
            input: Modalities::of(&[Modality::Text]),
            ..kage_provider::ProviderModel::default()
        },
    };
    let h = harness_on(ProviderRegistry::new().with(Arc::new(declared)));
    let id = SessionId::new();
    h.open(id, None);
    h.engine.send(Command::to(
        id,
        CommandKind::Prompt {
            content: vec![Content::Image {
                source: kage_core::MediaSource::Base64 {
                    data: "AA==".into(),
                },
                mime: "image/png".into(),
            }],
            delivery: Delivery::Steer,
        },
    ));
    let events = until_runs_end(&h.events, 1);

    assert_eq!(
        notices(&events),
        ["mock/m does not accept images; nothing to send"]
    );
    let ends: Vec<RunOutcome> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Host(HostEvent::RunEnded { outcome }) => Some(outcome.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ends,
        [RunOutcome::Failed {
            error: LoopError::InvalidPrompt {
                message: "mock/m does not accept images; nothing to send".into(),
            },
        }]
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.event, Event::Host(HostEvent::RunStarted))),
        "no run started: {events:?}"
    );

    prompt(&h.engine, id, "plain text", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
    h.engine.shutdown();
}

#[test]
fn an_unsubscribed_subscriber_receives_nothing_more() {
    let h = harness(MockProvider::replaying(text_turn("ok")));
    let id = SessionId::new();
    h.open(id, None);
    wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::McpServers { .. }))
    });

    let (tx, other) = channel();
    let subscription = h.engine.subscribe(Box::new(move |envelope| {
        let _ = tx.send(envelope.clone());
    }));
    h.engine.unsubscribe(subscription);

    prompt(&h.engine, id, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
    assert!(
        other.try_recv().is_err(),
        "the unsubscribed subscriber stayed quiet"
    );
    h.engine.shutdown();
}

#[test]
fn hold_events_delays_a_publish_from_another_thread_until_it_returns() {
    let h = harness(MockProvider::replaying(text_turn("ok")));
    let id = SessionId::new();
    h.open(id, None);
    wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::McpServers { .. }))
    });

    let (acked, acked_rx) = channel();
    let held_bus = Arc::clone(&h.engine.bus);
    let held = h.engine.hold_events(move || {
        let bus = Arc::clone(&held_bus);
        let publisher_thread = std::thread::spawn(move || {
            bus.publish(
                id,
                HostEvent::Notice {
                    level: NoticeLevel::Info,
                    text: "held back".into(),
                    transient: false,
                },
            );
            let _ = acked.send(());
        });
        // The publish cannot finish while this closure holds the bus
        // lock, so the ack waits for hold_events to return.
        let waited = acked_rx.recv_timeout(Duration::from_millis(100)).is_err();
        (publisher_thread, waited)
    });
    let (publisher_thread, waited) = held;
    assert!(waited, "the publish landed while the bus lock was held");
    publisher_thread.join().unwrap();
    let seen = wait_for(
        &h.events,
        |e| matches!(&e.event, Event::Host(HostEvent::Notice { text, .. }) if text == "held back"),
    );
    assert_eq!(seen.last().unwrap().session, id);
    h.engine.shutdown();
}

#[test]
fn close_drops_an_idle_session_and_its_idle_agents() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        text_turn("child reply"),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    let (child, _) = spawned(&events)[0].clone();
    assert_eq!(h.engine.hosted_sessions().len(), 2);

    h.engine.send(Command::to(parent, CommandKind::Close));
    prompt(&h.engine, child, "gone?", Delivery::Steer);
    let unknown = wait_for(&h.events, |e| {
        is_notice(e) && notices(std::slice::from_ref(e))[0].starts_with("unknown session")
    });
    assert_eq!(unknown.last().unwrap().session, child);
    assert!(h.engine.hosted_sessions().is_empty());
    h.engine.shutdown();
}

#[test]
fn close_during_a_run_warns_and_keeps_the_session() {
    let h = harness(MockProvider::replaying(tool_turn("gate")));
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "go", Delivery::Steer);
    wait_for(&h.events, is_tool_start);

    h.engine.send(Command::to(id, CommandKind::Close));
    let refused = wait_for(&h.events, is_notice);
    assert_eq!(
        notices(&refused),
        ["close: wait for the current run to finish or cancel it"]
    );

    h.engine.send(Command::to(id, CommandKind::Cancel));
    let events = until_runs_end(&h.events, 1);
    assert_eq!(outcomes(&events), [RunOutcome::Cancelled]);
    assert!(
        h.engine
            .hosted_sessions()
            .iter()
            .any(|(hosted, _)| *hosted == id),
        "the session stayed hosted"
    );
    h.engine.shutdown();
}

#[test]
fn closing_the_active_session_clears_active() {
    let h = harness(MockProvider::replaying(text_turn("ok")));
    let (a, b) = (SessionId::new(), SessionId::new());
    h.open(a, None);
    h.open(b, None);
    let marker = |text: &str| HostEvent::Notice {
        level: NoticeLevel::Info,
        text: text.into(),
        transient: false,
    };
    h.engine.commander.publish(marker("first"));
    let seen = wait_for(&h.events, |e| {
        is_notice(e) && notices(std::slice::from_ref(e))[0] == "first"
    });
    assert_eq!(
        seen.last().unwrap().session,
        a,
        "a opened first, so it is active"
    );

    h.engine.send(Command::to(a, CommandKind::Close));
    h.engine.commander.publish(marker("second"));
    prompt(&h.engine, b, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
    assert!(
        !notices(&events).contains(&"second".to_owned()),
        "{:?}",
        notices(&events)
    );
    h.engine.shutdown();
}

#[test]
fn an_agent_without_delegation_tools_in_its_list_cannot_delegate() {
    let mock = MockProvider::sequence(vec![
        agent_turn(&[(
            "call_a",
            serde_json::json!({"agent": "explore", "description": "d", "prompt": "look"}),
        )]),
        text_turn("found it"),
        text_turn("parent done"),
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(2, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    until_runs_end(&h.events, 2);
    h.engine.shutdown();

    let names = |at: usize| -> Vec<String> {
        mock.requests()[at]
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect()
    };
    for tool in ["agent", "swarm", "send_message"] {
        assert!(
            names(0).iter().any(|n| n == tool),
            "the main session has {tool}"
        );
        assert!(!names(1).iter().any(|n| n == tool), "explore lacks {tool}");
    }
}

#[test]
fn cancelling_an_idle_session_leaves_its_agents_free_to_run() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        text_turn("child reply"),
        text_turn("parent done"),
        text_turn("child again"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    let (child, _) = spawned(&events)[0].clone();
    h.engine.send(Command::to(parent, CommandKind::Cancel));
    prompt(&h.engine, child, "again", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    h.engine.shutdown();
    assert_eq!(outcome_of(&events, child), [RunOutcome::Completed]);
}

#[test]
fn a_message_resumes_a_finished_agent() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("first"))]),
        text_turn("a done"),
        text_turn("parent done"),
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(agent_setup(1, 1)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    let (first, _) = spawned(&events)[0].clone();

    mock.push_script(send_message_turn(
        "call_m",
        &first.to_string(),
        "one more thing",
    ));
    // A parent still running when the result lands reads it with one
    // more turn, so either order takes a spare reply.
    for _ in 0..3 {
        mock.push_script(text_turn("reply"));
    }
    prompt(&h.engine, parent, "again", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);

    let sent = tool_output(&events, parent, "call_m");
    assert!(!sent.is_error, "{}", sent.text);
    assert!(sent.text.contains("which had finished"), "{}", sent.text);
    assert_eq!(outcome_of(&events, first), [RunOutcome::Completed]);
    assert!(
        events.iter().any(|e| e.session == first
            && matches!(
                e.event,
                Event::Host(HostEvent::AgentSpawned {
                    background: true,
                    ..
                })
            )),
        "the reopened agent is announced as a background agent"
    );

    mock.push_script(text_turn("seen"));
    prompt(&h.engine, parent, "next", Delivery::Steer);
    until_runs_end(&h.events, 1);
    h.engine.shutdown();
    let head = format!("<agent name=\"general\" session=\"{first}\"");
    assert!(
        mock.requests()
            .iter()
            .flat_map(|r| &r.messages)
            .flat_map(|m| &m.content)
            .any(|c| matches!(c, Content::Text { text } if text.contains(&head))),
        "the resumed agent's result reaches its parent"
    );
}

#[test]
fn a_message_to_another_conversation_is_refused() {
    let mock = MockProvider::sequence(vec![]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let gate = || PermissionGate::new(PermissionsConfig::default());
    let other = h.open_parent(None, gate(), Some(agent_setup(1, 1)));
    let parent = h.open_parent(None, gate(), Some(agent_setup(1, 1)));
    mock.push_script(send_message_turn("call_m", &other.to_string(), "psst"));
    mock.push_script(text_turn("done"));
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    h.engine.shutdown();

    let refused = tool_output(&events, parent, "call_m");
    assert!(refused.is_error);
    assert!(
        refused.text.contains("belongs to another conversation"),
        "{}",
        refused.text
    );
    assert_eq!(outcome_of(&events, other), []);
}

#[test]
fn resuming_a_busy_swarm_child_leaves_its_running_call_alone() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::sequence(vec![
        swarm_turn(&[("call_s", swarm_task(&["a", "b"]))]),
        tool_turn("gate"),
        tool_turn("gate"),
        text_turn("child done"),
        text_turn("child done"),
        text_turn("parent done"),
    ]));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(swarm_setup(2, 60_000)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let mut seen = wait_for(&h.events, is_tool_start_outside(parent));
    seen.extend(wait_for(&h.events, is_tool_start_outside(parent)));
    let busy = spawned(&seen)[0].0;
    let refused = h
        .engine
        .resume_swarm(parent, [(busy, "start over".to_owned())].into())
        .unwrap_err();
    assert!(refused.contains("still working"), "{refused}");
    h.release.send(()).unwrap();
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 3);
    h.engine.shutdown();

    let output = tool_output(&events, parent, "call_s");
    assert!(
        output
            .text
            .starts_with("completed: 2, failed: 0, cancelled: 0\n"),
        "{}",
        output.text
    );
}

#[test]
fn cancelling_the_parent_ends_a_rate_limited_child_for_good() {
    let mut scripts = vec![swarm_turn(&[("call_s", swarm_task(&["a", "b"]))])];
    for _ in 0..5 {
        scripts.push(rate_limited());
    }
    scripts.push(tool_turn("gate"));
    let mock = MockProvider::sequence(scripts);
    let h = harness(mock.clone());
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(swarm_setup(1, 60_000)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let seen = wait_for(&h.events, is_tool_start_outside(parent));
    let paused = seen
        .iter()
        .find(|e| matches!(e.event, Event::Host(HostEvent::AgentPaused { .. })))
        .expect("the first child paused")
        .session;
    h.engine.send(Command::to(parent, CommandKind::Cancel));
    let events = until_runs_end(&h.events, 3);

    let output = tool_output(&events, parent, "call_s");
    assert!(
        output
            .text
            .starts_with("completed: 0, failed: 0, cancelled: 2\n"),
        "{}",
        output.text
    );
    assert_eq!(outcome_of(&events, paused), [RunOutcome::Cancelled]);
    let calls = mock.call_count();
    std::thread::sleep(REQUEUE_BASE + Duration::from_millis(500));
    assert_eq!(mock.call_count(), calls, "the requeue found nothing to run");
    h.engine.shutdown();
}

#[test]
fn a_forked_child_reports_only_its_own_work() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(vec![tool_turn("gate"), text_turn("parent says hi")]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(swarm_setup(1, 60_000)),
        ..h.spec(parent)
    });
    h.release.send(()).unwrap();
    prompt(&h.engine, parent, "hello", Delivery::Steer);
    until_runs_end(&h.events, 1);

    mock.push_script(swarm_turn(&[("call_s", forked_swarm_task(&["a", "b"]))]));
    mock.push_script(text_turn("child a"));
    mock.push_script(text_turn("child b"));
    mock.push_script(text_turn("parent done"));
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 3);
    h.engine.shutdown();

    let output = tool_output(&events, parent, "call_s");
    assert_eq!(
        output.text.matches("tools=\"0\"").count(),
        2,
        "the parent's gate call is not the children's: {}",
        output.text
    );
}

#[test]
fn a_delivered_agent_is_dropped_and_takes_no_more_prompts() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        text_turn("child reply"),
        text_turn("parent done"),
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(agent_setup(1, 1)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    let child = spawned(&events)[0].0;
    let hosted: Vec<SessionId> = h
        .engine
        .hosted_sessions()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(hosted, [parent], "the delivered child is dropped");

    prompt(&h.engine, child, "more", Delivery::Steer);
    let seen = wait_for(&h.events, is_notice);
    h.engine.shutdown();
    assert_eq!(notices(&seen), [format!("unknown session {child}")]);
    assert_eq!(mock.call_count(), 3, "nothing ran");
}

#[test]
fn a_client_resume_keeps_the_member_on_its_card_and_reports_to_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(vec![
        swarm_turn(&[("call_s", swarm_task(&["a", "b"]))]),
        text_turn("a one"),
        text_turn("b one"),
        text_turn("parent done"),
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(swarm_setup(1, 60_000)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let first = until_runs_end(&h.events, 3);
    let member = |events: &[Envelope], id: SessionId| {
        events.iter().rev().find_map(|e| match &e.event {
            Event::Host(HostEvent::AgentSpawned {
                tool_call_id,
                swarm,
                ..
            }) if e.session == id => Some((tool_call_id.clone(), swarm.clone())),
            _ => None,
        })
    };
    let child = spawned(&first)[0].0;
    let (call, swarm) = member(&first, child).unwrap();

    mock.push_script(text_turn("a two"));
    let accepted = h
        .engine
        .resume_swarm(parent, [(child, String::new())].into())
        .unwrap();
    assert_eq!(accepted, [child]);
    let events = until_runs_end(&h.events, 1);
    let (again, swarm_again) = member(&events, child).expect("announced again");
    assert_eq!(again, call, "the member reports under its first call");
    assert_eq!(swarm_again, swarm, "in its old place");
    let done = wait_for(&h.events, |e| {
        e.session == parent
            && notices(std::slice::from_ref(e))
                .iter()
                .any(|n| n.contains("finished"))
    });
    assert!(
        notices(&done)
            .iter()
            .any(|n| n == "resumed swarm members finished: completed: 1, failed: 0, cancelled: 0"),
        "{:?}",
        notices(&done)
    );

    mock.push_script(text_turn("noted"));
    prompt(&h.engine, parent, "what changed?", Delivery::Steer);
    until_runs_end(&h.events, 1);
    h.engine.shutdown();
    let request = mock.last_request().unwrap();
    let note = request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|c| match c {
            Content::Text { text } if text.starts_with("[swarm resume]") => Some(text.clone()),
            _ => None,
        })
        .expect("the parent reads the results");
    assert!(note.contains("a two"), "{note}");
}

#[test]
fn a_set_goal_reaches_the_agent_and_keeps_the_session_working_until_met() {
    let mock = MockProvider::sequence(vec![
        text_turn("working"),
        text_turn("NO"),
        text_turn("more"),
        text_turn("YES"),
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let id = SessionId::new();
    h.open(id, None);
    h.engine.send(Command::to(
        id,
        CommandKind::SetGoal {
            goal: Some("ship it".into()),
        },
    ));
    let events = until_runs_end(&h.events, 2);
    assert_eq!(
        outcomes(&events),
        [RunOutcome::Completed, RunOutcome::Completed]
    );
    let met = wait_for(&h.events, |e| {
        notices(std::slice::from_ref(e))
            .iter()
            .any(|n| n.starts_with("goal met"))
    });
    h.engine.shutdown();
    let requests = mock.requests();
    let intro = requests[0].messages.last().unwrap();
    let Content::Text { text } = &intro.content[0] else {
        panic!("text intro");
    };
    assert!(text.contains("Work toward this goal: ship it"), "{text}");
    let mut seen = notices(&events);
    seen.extend(notices(&met));
    assert!(
        seen.iter()
            .any(|n| n == "goal not met yet; continuing (1/8)"),
        "{seen:?}"
    );
    assert!(seen.iter().any(|n| n == "goal met: ship it"), "{seen:?}");
    assert_eq!(mock.call_count(), 4, "no turn after the goal is met");
}

#[test]
fn a_goal_set_mid_turn_queues_its_message_for_the_next_turn() {
    let mock = MockProvider::sequence(vec![
        tool_turn("gate"),    // the running turn, held on the gate
        text_turn("working"), // its continuation
        text_turn("YES"),     // the check after it: met
        text_turn("on it"),   // the queued goal turn
        text_turn("YES"),     // the check after it
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "start", Delivery::Queue);
    wait_for(&h.events, is_tool_start);
    h.engine.send(Command::to(
        id,
        CommandKind::SetGoal {
            goal: Some("ship it".into()),
        },
    ));
    h.release.send(()).unwrap();
    until_runs_end(&h.events, 2);
    // The check after the queued turn runs on its own thread, so wait
    // for it to land before the shutdown cuts it short.
    let deadline = Instant::now() + WAIT;
    while mock.call_count() < 5 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    h.engine.shutdown();
    let requests = mock.requests();
    // The judge runs on its own thread, so it may interleave with the
    // queued intro turn; find the intro request by its content instead
    // of its position.
    let intro = requests
        .iter()
        .find(|r| {
            r.messages.last().is_some_and(|m| {
                crate::cli_loop_run::first_user_text(m).contains("Work toward this goal: ship it")
            })
        })
        .unwrap_or_else(|| panic!("no queued goal intro in {requests:?}"));
    let Content::Text { text } = &intro.messages.last().unwrap().content[0] else {
        panic!("text intro");
    };
    assert!(text.contains("Work toward this goal: ship it"), "{text}");
    assert_eq!(mock.call_count(), 5, "the queued turn ran, then the check");
}

#[test]
fn a_met_goal_is_checked_again_and_reopens_when_a_later_turn_fails_it() {
    let mock = MockProvider::sequence(vec![
        text_turn("working"),
        text_turn("YES"),
        text_turn("broke it"),
        text_turn("NO"),
        text_turn("fixed"),
        text_turn("YES"),
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let id = SessionId::new();
    h.open(id, None);
    h.engine.send(Command::to(
        id,
        CommandKind::SetGoal {
            goal: Some("ship it".into()),
        },
    ));
    until_runs_end(&h.events, 1);
    let first = wait_for(&h.events, |e| {
        notices(std::slice::from_ref(e))
            .iter()
            .any(|n| n == "goal met: ship it")
    });
    prompt(&h.engine, id, "now break it", Delivery::Queue);
    let more = until_runs_end(&h.events, 2);
    let reopened = wait_for(&h.events, |e| {
        notices(std::slice::from_ref(e))
            .iter()
            .any(|n| n == "goal met: ship it")
    });
    h.engine.shutdown();
    let mut seen = notices(&first);
    seen.extend(notices(&more));
    seen.extend(notices(&reopened));
    assert!(
        seen.iter()
            .any(|n| n == "goal not met yet; continuing (1/8)"),
        "{seen:?}"
    );
    assert_eq!(mock.call_count(), 6, "no turn after the goal is met again");
}

#[test]
fn a_goal_check_without_a_verdict_stops_instead_of_looping() {
    let mock = MockProvider::sequence(vec![text_turn("working"), text_turn("Hmm, hard to say")]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let id = SessionId::new();
    h.open(id, None);
    h.engine.send(Command::to(
        id,
        CommandKind::SetGoal {
            goal: Some("ship it".into()),
        },
    ));
    let stopped = wait_for(&h.events, |e| {
        notices(std::slice::from_ref(e))
            .iter()
            .any(|n| n.starts_with("could not check the goal"))
    });
    h.engine.shutdown();
    assert_eq!(outcomes(&stopped).len(), 1, "no turn follows");
    assert_eq!(mock.call_count(), 2);
}

#[test]
fn a_missing_part_is_named_in_the_notice() {
    let mock = MockProvider::sequence(vec![
        text_turn("working"),
        text_turn("NO: the tests still fail"),
        text_turn("fixed"),
        text_turn("YES"),
    ]);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let id = SessionId::new();
    h.open(id, None);
    h.engine.send(Command::to(
        id,
        CommandKind::SetGoal {
            goal: Some("tests pass".into()),
        },
    ));
    let met = wait_for(&h.events, |e| {
        notices(std::slice::from_ref(e))
            .iter()
            .any(|n| n.starts_with("goal met"))
    });
    h.engine.shutdown();
    let seen = notices(&met);
    assert!(
        seen.iter()
            .any(|n| n == "goal not met yet: the tests still fail; continuing (1/8)"),
        "{seen:?}"
    );
    let nudge = &mock.requests()[2];
    let last = nudge.messages.last().unwrap();
    let Content::Text { text } = &last.content[0] else {
        panic!("text nudge");
    };
    assert!(
        text.contains("Still missing: the tests still fail"),
        "{text}"
    );
}

#[test]
fn an_unmet_goal_stops_after_its_turn_cap() {
    let mut scripts = Vec::new();
    for _ in 0..=MAX_GOAL_TURNS {
        scripts.push(text_turn("trying"));
        scripts.push(text_turn("NO"));
    }
    let mock = MockProvider::sequence(scripts);
    let h = harness_on(ProviderRegistry::new().with(Arc::new(mock.clone())));
    let id = SessionId::new();
    h.open(id, None);
    h.engine.send(Command::to(
        id,
        CommandKind::SetGoal {
            goal: Some("impossible".into()),
        },
    ));
    let stopped = wait_for(&h.events, |e| {
        notices(std::slice::from_ref(e))
            .iter()
            .any(|n| n.starts_with("goal not met after"))
    });
    h.engine.shutdown();
    let runs = outcomes(&stopped).len();
    assert_eq!(
        runs,
        1 + MAX_GOAL_TURNS as usize,
        "the prompt plus the capped turns"
    );
}

/// Serves the main session's turns and its agents' turns from their own
/// scripts, told apart by the system prompt only agents have, so a
/// background agent and its parent never race for one queue.
#[derive(Debug)]
struct Split {
    main: MockProvider,
    agents: MockProvider,
}

impl kage_provider::Provider for Split {
    fn metadata(&self) -> &kage_provider::ProviderMetadata {
        self.main.metadata()
    }

    fn stream(
        &self,
        req: kage_provider::StreamRequest,
        cancel: &kage_core::CancelFlag,
    ) -> Result<kage_provider::EventStream, ProviderError> {
        if req.system.as_deref().is_none_or(str::is_empty) {
            self.main.stream(req, cancel)
        } else {
            self.agents.stream(req, cancel)
        }
    }
}

type Script = Vec<Result<ProviderEvent, ProviderError>>;

/// A harness whose main session reads `main` and whose agents read
/// `agents`, with both mocks kept for their requests.
fn split_harness(main: Vec<Script>, agents: Vec<Script>) -> (Harness, MockProvider, MockProvider) {
    let main = MockProvider::sequence(main);
    let agents = MockProvider::sequence(agents);
    let split = Split {
        main: main.clone(),
        agents: agents.clone(),
    };
    let h = harness_on(ProviderRegistry::new().with(Arc::new(split)));
    (h, main, agents)
}

fn background_task(prompt: &str) -> serde_json::Value {
    serde_json::json!({"description": "a task", "prompt": prompt, "background": true})
}

fn background_setup(background: Background) -> AgentSetup {
    AgentSetup {
        background,
        ..agent_setup(2, 2)
    }
}

fn run_ended_on(session: SessionId) -> impl Fn(&Envelope) -> bool {
    move |e| e.session == session && matches!(e.event, Event::Host(HostEvent::RunEnded { .. }))
}

/// The agent reports appended to `session` as user messages.
fn reports_on(events: &[Envelope], session: SessionId) -> Vec<AgentReport> {
    events
        .iter()
        .filter(|e| e.session == session)
        .filter_map(|e| match &e.event {
            Event::Loop(LoopEvent::MessageAppended { message }) if message.role == Role::User => {
                Some(AgentReport::all_in(&crate::cli_loop_run::first_user_text(
                    message,
                )))
            }
            _ => None,
        })
        .flatten()
        .collect()
}

fn agent_schema(request: &kage_provider::StreamRequest) -> Option<serde_json::Value> {
    request
        .tools
        .iter()
        .find(|tool| tool.name == "agent")
        .map(|tool| tool.schema.clone())
}

#[test]
fn a_background_agent_returns_at_once_and_wakes_its_idle_parent() {
    let (h, main, agents) = split_harness(
        vec![
            agent_turn(&[("call_a", background_task("test everything"))]),
            text_turn("waiting for the tests"),
            text_turn("the tests pass"),
        ],
        vec![tool_turn("gate"), text_turn("412 passed")],
    );
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(background_setup(Background::Wake)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = wait_for(&h.events, run_ended_on(parent));
    let started = AgentReport::parse(&tool_output(&events, parent, "call_a").text).unwrap();
    assert_eq!(started.state, ReportState::Started);
    assert!(events.iter().any(|e| matches!(
        e.event,
        Event::Host(HostEvent::AgentSpawned {
            background: true,
            ..
        })
    )));
    assert_eq!(outcome_of(&events, parent), [RunOutcome::Completed]);

    h.release.send(()).unwrap();
    let events = wait_for(&h.events, run_ended_on(parent));
    let reports = reports_on(&events, parent);
    assert_eq!(reports.len(), 1, "{events:?}");
    assert_eq!(reports[0].session, started.session);
    assert_eq!(reports[0].state, ReportState::Completed);
    assert_eq!(reports[0].body, "412 passed");

    let first = &main.requests()[0];
    assert!(agent_schema(first).unwrap()["properties"]["background"].is_object());
    let child = &agents.requests()[0];
    let nested = agent_schema(child).expect("depth 1 may start agents");
    assert!(nested["properties"]["background"].is_null(), "{nested}");
}

/// Runs until its run is cancelled.
#[derive(Debug)]
struct Stall;

impl Tool for Stall {
    fn name(&self) -> &'static str {
        "stall"
    }
    fn description(&self) -> &'static str {
        "runs until cancelled"
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
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        while !cx.is_cancelled() {
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(ToolError::Cancelled)
    }
}

#[test]
fn stopping_the_parent_run_leaves_a_background_agent_running() {
    let (h, _main, _agents) = split_harness(
        vec![
            agent_turn(&[("call_a", background_task("test everything"))]),
            tool_turn("stall"),
            text_turn("read it"),
        ],
        vec![tool_turn("gate"), text_turn("412 passed")],
    );
    let mut tools = h.tools.clone();
    tools.register(Arc::new(Stall));
    let parent = SessionId::new();
    h.engine.open(SessionSpec {
        tools,
        agents: Some(background_setup(Background::Wake)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let mut events = wait_for(&h.events, |e| {
        e.session == parent
            && matches!(&e.event, Event::Loop(LoopEvent::ToolCallStart { name, .. }) if name == "stall")
    });
    h.engine.send(Command::to(parent, CommandKind::Cancel));
    events.extend(wait_for(&h.events, run_ended_on(parent)));
    assert_eq!(outcome_of(&events, parent), [RunOutcome::Cancelled]);
    let (child, _) = spawned(&events)[0];
    assert!(outcome_of(&events, child).is_empty(), "{events:?}");

    h.release.send(()).unwrap();
    let events = wait_for(&h.events, run_ended_on(child));
    assert_eq!(outcome_of(&events, child), [RunOutcome::Completed]);
    let events = wait_for(&h.events, run_ended_on(parent));
    assert_eq!(reports_on(&events, parent)[0].body, "412 passed");
}

#[test]
fn a_holding_parent_reads_a_background_result_with_its_next_prompt() {
    let (h, main, _agents) = split_harness(
        vec![
            agent_turn(&[("call_a", background_task("test everything"))]),
            text_turn("waiting for the tests"),
            text_turn("the tests pass"),
        ],
        vec![tool_turn("gate"), text_turn("412 passed")],
    );
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(background_setup(Background::Hold)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = wait_for(&h.events, run_ended_on(parent));
    let (child, _) = spawned(&events)[0];
    h.release.send(()).unwrap();
    wait_for(&h.events, run_ended_on(child));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(main.call_count(), 2, "no run starts on its own");

    prompt(&h.engine, parent, "anything new?", Delivery::Steer);
    let events = wait_for(&h.events, run_ended_on(parent));
    assert_eq!(main.call_count(), 3);
    let texts: Vec<String> = main.requests()[2]
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| crate::cli_loop_run::first_user_text(m))
        .collect();
    assert_eq!(texts[texts.len() - 2], "anything new?");
    let report = AgentReport::parse(&texts[texts.len() - 1]).unwrap();
    assert_eq!(report.body, "412 passed");
    assert_eq!(reports_on(&events, parent).len(), 1);
}

#[test]
fn a_running_background_agent_reads_a_message_at_its_next_turn() {
    let (h, _main, agents) = split_harness(
        vec![
            agent_turn(&[("call_a", background_task("test everything"))]),
            text_turn("waiting for the tests"),
            text_turn("the tests pass"),
        ],
        vec![tool_turn("gate"), text_turn("412 passed, doc tests too")],
    );
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(background_setup(Background::Wake)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = wait_for(&h.events, run_ended_on(parent));
    let (child, _) = spawned(&events)[0];
    let (reply, ack) = crossbeam_channel::bounded(1);
    h.engine
        .commander()
        .0
        .send(Input::Deliver {
            from: parent,
            to: Some(child),
            message: "also run the doc tests".into(),
            reply,
        })
        .unwrap();
    let ack = ack.recv_timeout(WAIT).unwrap().unwrap();
    assert!(ack.contains("next turn boundary"), "{ack}");

    h.release.send(()).unwrap();
    let events = wait_for(&h.events, run_ended_on(parent));
    let mail = AgentMail {
        from: "kage".into(),
        session: parent,
        body: "also run the doc tests".into(),
    };
    let read = agents.requests()[1]
        .messages
        .iter()
        .any(|m| m.role == Role::User && crate::cli_loop_run::first_user_text(m) == mail.to_text());
    assert!(read, "{:?}", agents.requests()[1].messages);
    assert_eq!(
        reports_on(&events, parent)[0].body,
        "412 passed, doc tests too"
    );
}

/// `script` with its turn's usage set to `input` tokens in.
fn costing(mut script: Script, input: u64) -> Script {
    for event in &mut script {
        if let Ok(ProviderEvent::MessageEnd { usage, .. }) = event {
            usage.input = input;
        }
    }
    script
}

fn report_of(events: &[Envelope], session: SessionId, call: &str) -> AgentReport {
    let output = tool_output(events, session, call);
    AgentReport::parse(&output.text).unwrap_or_else(|| panic!("{}", output.text))
}

#[test]
fn an_agent_at_its_turn_limit_is_warned_then_stopped() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("loop"))]),
        tool_turn("noop"),
        tool_turn("noop"),
        tool_turn("noop"),
        text_turn("parent done"),
    ]));
    let mut tools = h.tools.clone();
    tools.register(Arc::new(Stub {
        name: "noop",
        risk: kage_core::Risk::Read,
    }));
    let parent = SessionId::new();
    h.engine.open(SessionSpec {
        tools,
        agents: Some(AgentSetup {
            max_turns: 2,
            ..agent_setup(1, 1)
        }),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = wait_for(&h.events, run_ended_on(parent));
    let (child, _) = spawned(&events)[0];
    let report = report_of(&events, parent, "call_a");
    assert_eq!(
        (report.state, report.limit),
        (ReportState::Completed, Some(AgentLimit::Turns))
    );
    let child_events: Vec<Envelope> = events
        .iter()
        .filter(|e| e.session == child)
        .cloned()
        .collect();
    let warnings = appended_texts(&child_events)
        .into_iter()
        .filter(|text| text.starts_with("Turn limit reached"))
        .count();
    assert_eq!(warnings, 1);
    assert_eq!(outcome_of(&events, parent), [RunOutcome::Completed]);
}

#[test]
fn an_agent_past_its_timeout_is_cancelled_and_its_parent_goes_on() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("wait"))]),
        tool_turn("gate"),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(AgentSetup {
            timeout: Some(Duration::from_millis(200)),
            ..agent_setup(1, 1)
        }),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = wait_for(&h.events, run_ended_on(parent));
    let report = report_of(&events, parent, "call_a");
    assert_eq!(
        (report.state, report.limit),
        (ReportState::Cancelled, Some(AgentLimit::Time))
    );
    assert_eq!(outcome_of(&events, parent), [RunOutcome::Completed]);
}

#[test]
fn agents_past_the_budget_stop_until_the_next_prompt() {
    let (h, _main, _agents) = split_harness(
        vec![
            costing(
                agent_turn(&[("call_a", task("one")), ("call_b", task("two"))]),
                100,
            ),
            agent_turn(&[("call_c", task("three"))]),
            text_turn("stopped"),
            agent_turn(&[("call_d", task("four"))]),
            text_turn("done"),
        ],
        vec![
            costing(tool_turn("gate"), 15),
            costing(tool_turn("gate"), 15),
            text_turn("four done"),
        ],
    );
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(AgentSetup {
            budget: 20,
            ..agent_setup(1, 2)
        }),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = wait_for(&h.events, run_ended_on(parent));
    for call in ["call_a", "call_b"] {
        let report = report_of(&events, parent, call);
        assert_eq!(
            (report.state, report.limit),
            (ReportState::Cancelled, Some(AgentLimit::Budget)),
            "{call}"
        );
    }
    let refused = tool_output(&events, parent, "call_c");
    assert!(
        refused.is_error && refused.text.contains("agent budget of 20"),
        "{}",
        refused.text
    );
    assert!(events.iter().any(|e| e.session == parent
        && matches!(&e.event, Event::Host(HostEvent::Notice { text, .. })
            if text.contains("agent budget of 20 tokens"))));

    prompt(&h.engine, parent, "again", Delivery::Steer);
    let events = wait_for(&h.events, run_ended_on(parent));
    let report = report_of(&events, parent, "call_d");
    assert_eq!((report.state, report.limit), (ReportState::Completed, None));
}

fn question_turn() -> Script {
    let id = ToolCallId::new("call_q");
    let input = serde_json::json!({"questions": [{
        "header": "Store",
        "question": "Where should sessions live?",
        "options": [
            {"label": "Disk", "description": "Survives restarts"},
            {"label": "Memory", "description": "Faster, lost on exit"}
        ]
    }]});
    vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: kage_core::protocol::ASK_USER_QUESTION_TOOL.into(),
        }),
        Ok(ProviderEvent::ToolCallEnd { id, input }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }),
    ]
}

fn question_asked(events: &[Envelope]) -> Option<kage_core::protocol::RequestId> {
    events.iter().find_map(|e| match &e.event {
        Event::Host(HostEvent::QuestionAsked { request_id, .. }) => Some(*request_id),
        _ => None,
    })
}

#[test]
fn a_question_waits_for_the_answer_and_the_model_reads_it() {
    let mock = MockProvider::sequence(vec![question_turn(), text_turn("disk it is")]);
    let h = harness(mock.clone());
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "store the sessions", Delivery::Steer);
    let events = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::QuestionAsked { .. }))
    });
    let request_id = question_asked(&events).unwrap();
    h.engine.send(Command::to(
        id,
        CommandKind::AnswerQuestion {
            request_id,
            answers: Some(vec![vec!["Disk".into()]]),
        },
    ));
    let events = until_runs_end(&h.events, 1);
    assert!(events.iter().any(|e| matches!(
        e.event,
        Event::Host(HostEvent::QuestionClosed { request_id: closed }) if closed == request_id
    )));
    let output = tool_output(&events, id, "call_q");
    assert_eq!(
        output.text,
        "The user answered:\n- Where should sessions live?: Disk"
    );
    assert_eq!(outcome_of(&events, id), [RunOutcome::Completed]);
}

#[test]
fn cancelling_a_run_closes_its_open_question() {
    let h = harness(MockProvider::sequence(vec![question_turn()]));
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "store the sessions", Delivery::Steer);
    let events = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::QuestionAsked { .. }))
    });
    let request_id = question_asked(&events).unwrap();
    h.engine.send(Command::to(id, CommandKind::Cancel));
    let events = until_runs_end(&h.events, 1);
    assert!(events.iter().any(|e| matches!(
        e.event,
        Event::Host(HostEvent::QuestionClosed { request_id: closed }) if closed == request_id
    )));
    assert_eq!(outcome_of(&events, id), [RunOutcome::Cancelled]);
}

#[test]
fn only_an_answerable_main_session_gets_the_question_tool() {
    let has_tool = |interactive: bool, agents: bool| {
        let mock = MockProvider::sequence(vec![
            agent_turn(&[("call_a", task("look"))]),
            text_turn("child done"),
            text_turn("parent done"),
        ]);
        let h = harness(mock.clone());
        let id = SessionId::new();
        h.engine.open(SessionSpec {
            interactive,
            agents: agents.then(|| agent_setup(1, 1)),
            ..h.spec(id)
        });
        prompt(&h.engine, id, "go", Delivery::Steer);
        until_runs_end(&h.events, if agents { 2 } else { 1 });
        mock.requests()
            .iter()
            .map(|req| {
                req.tools
                    .iter()
                    .any(|t| t.name == kage_core::protocol::ASK_USER_QUESTION_TOOL)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        has_tool(true, true),
        [true, false, true],
        "the agent has none"
    );
    assert!(
        has_tool(false, false).iter().all(|has| !has),
        "nobody to ask"
    );
}

#[test]
fn closing_the_root_ends_a_queued_descendant_as_cancelled() {
    let (h, _main, _agents) = split_harness(
        vec![
            agent_turn(&[("call_a", background_task("fan out"))]),
            text_turn("waiting for the tests"),
        ],
        vec![
            agent_turn(&[("call_g1", task("one")), ("call_g2", task("two"))]),
            tool_turn("gate"),
            text_turn("spare"),
        ],
    );
    let root = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(AgentSetup {
            max_running: 1,
            ..background_setup(Background::Wake)
        }),
    );
    prompt(&h.engine, root, "go", Delivery::Steer);
    // The root may end before or after the queued grandchild spawns,
    // so wait for both without assuming an order.
    let root_ended = std::cell::Cell::new(false);
    let g2_spawned = std::cell::Cell::new(false);
    let mut events = wait_for(&h.events, |e| {
        if run_ended_on(root)(e) {
            root_ended.set(true);
        }
        if matches!(&e.event,
            Event::Host(HostEvent::AgentSpawned { tool_call_id, .. }) if tool_call_id.0 == "call_g2")
        {
            g2_spawned.set(true);
        }
        root_ended.get() && g2_spawned.get()
    });
    let g2 = events
        .iter()
        .rev()
        .find_map(|e| {
            matches!(&e.event,
                Event::Host(HostEvent::AgentSpawned { tool_call_id, .. }) if tool_call_id.0 == "call_g2")
                .then_some(e.session)
        })
        .expect("g2 spawned");

    // The root is idle while its background child runs one grandchild
    // and waits on the queued other.
    let children = spawned(&events);
    assert_eq!(children[0].1.0, "call_a");
    let b = children[0].0;
    assert_eq!(outcome_of(&events, g2).len(), 0, "still queued");

    h.engine.send(Command::to(root, CommandKind::Close));
    events.extend(wait_for(&h.events, |e| {
        e.session == g2
            && matches!(&e.event, Event::Host(HostEvent::RunEnded { outcome })
                if *outcome == RunOutcome::Cancelled)
    }));
    // The queued child's cancelled result reaches the waiting call on
    // the parent's worker, a step after its `RunEnded`.
    let ended = wait_for(&h.events, |e| {
        matches!(&e.event, Event::Loop(LoopEvent::ToolCallEnd { id, .. })
            if e.session == b && id.0 == "call_g2")
    });
    h.engine.shutdown();

    let Event::Loop(LoopEvent::ToolCallEnd { output, .. }) = &ended.last().unwrap().event else {
        unreachable!("the wait matched a ToolCallEnd");
    };
    assert!(
        output
            .text
            .contains(&format!("session=\"{g2}\" state=\"cancelled\"")),
        "{}",
        output.text
    );
    assert!(
        !output.text.contains("the engine stopped"),
        "{}",
        output.text
    );
}

#[test]
fn a_reaped_agent_forgets_its_event_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        text_turn("child reply"),
        text_turn("parent done"),
    ]));
    let (recorder, _) = recorder_in(dir.path(), SessionId::new());
    let parent = h.open_parent(
        Some(recorder),
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    let (child, _) = spawned(&events)[0];
    assert!(events.iter().any(|e| e.session == child && e.seq >= 1));

    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline
        && h.engine
            .hosted_sessions()
            .iter()
            .any(|(id, _)| *id == child)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !h.engine
            .hosted_sessions()
            .iter()
            .any(|(id, _)| *id == child),
        "the delivered child is dropped"
    );
    assert_eq!(
        h.engine.bus.seq_of(child),
        None,
        "the reaped child's sequence is forgotten"
    );
    assert!(
        h.engine.bus.seq_of(parent).is_some(),
        "the parent keeps its sequence"
    );
    h.engine.shutdown();
}

#[test]
fn a_failed_fork_spawn_releases_the_snapshot_when_the_batch_is_done() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("ok")));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(swarm_setup(4, 60_000)),
        ..h.spec(parent)
    });
    let batch = ToolCallId::new("swarm_batch");
    let spawn = |id: SessionId, index: usize, reply: crossbeam_channel::Sender<ToolOutput>| {
        Input::Spawn(Box::new(Spawn {
            parent,
            tool_call_id: ToolCallId::new("call_s"),
            agent: "general".to_owned(),
            description: "a task".to_owned(),
            prompt: "work".to_owned(),
            reply,
            fork: true,
            swarm: Some(SwarmInfo {
                id,
                batch_id: batch.clone(),
                index,
                item: "a".to_owned(),
                total: 3,
            }),
            model: None,
            thinking: None,
            background: false,
        }))
    };
    let probe = |h: &Harness| {
        let (reply, answer) = crossbeam_channel::bounded(1);
        h.engine
            .commander()
            .0
            .send(Input::ForkSnapshot(reply))
            .unwrap();
        answer.recv_timeout(WAIT).unwrap()
    };
    let refused = |h: &Harness, id: SessionId, index: usize| {
        let (reply, answer) = crossbeam_channel::bounded(1);
        h.engine
            .commander()
            .0
            .send(spawn(id, index, reply))
            .unwrap();
        answer.recv_timeout(WAIT).unwrap()
    };

    // Spawning the same id twice leaves the first child's file in the
    // way, so later forks of the batch cannot write their copies.
    let dup = SessionId::new();
    let (first_reply, first) = crossbeam_channel::bounded(1);
    h.engine
        .commander()
        .0
        .send(spawn(dup, 0, first_reply))
        .unwrap();
    assert_eq!(
        probe(&h),
        Some(batch.clone()),
        "the first child cached the parent transcript"
    );
    let mid = refused(&h, dup, 1);
    assert!(mid.text.contains("cannot fork"), "{}", mid.text);
    assert_eq!(
        probe(&h),
        Some(batch.clone()),
        "a mid-batch failure keeps the cache for the later children"
    );
    let last = refused(&h, dup, 2);
    assert!(last.text.contains("cannot fork"), "{}", last.text);
    assert_eq!(
        probe(&h),
        None,
        "the last failure releases the parent transcript"
    );
    let done = first.recv_timeout(WAIT).unwrap();
    assert!(done.text.contains("state=\"completed\""), "{}", done.text);
    h.engine.shutdown();
}

#[test]
fn fork_children_spawned_out_of_order_keep_the_snapshot_until_the_batch_is_done() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("ok")));
    let parent = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), parent);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(swarm_setup(4, 60_000)),
        ..h.spec(parent)
    });
    let batch = ToolCallId::new("swarm_batch");
    let spawn = |id: SessionId, index: usize, reply: crossbeam_channel::Sender<ToolOutput>| {
        Input::Spawn(Box::new(Spawn {
            parent,
            tool_call_id: ToolCallId::new("call_s"),
            agent: "general".to_owned(),
            description: "a task".to_owned(),
            prompt: "work".to_owned(),
            reply,
            fork: true,
            swarm: Some(SwarmInfo {
                id,
                batch_id: batch.clone(),
                index,
                item: "a".to_owned(),
                total: 3,
            }),
            model: None,
            thinking: None,
            background: false,
        }))
    };
    let probe = |h: &Harness| {
        let (reply, answer) = crossbeam_channel::bounded(1);
        h.engine
            .commander()
            .0
            .send(Input::ForkSnapshot(reply))
            .unwrap();
        answer.recv_timeout(WAIT).unwrap()
    };
    let refused = |h: &Harness, id: SessionId, index: usize| {
        let (reply, answer) = crossbeam_channel::bounded(1);
        h.engine
            .commander()
            .0
            .send(spawn(id, index, reply))
            .unwrap();
        answer.recv_timeout(WAIT).unwrap()
    };

    // The last index arrives first, as when the children's worktrees
    // finish out of order. Spawning its id again makes the later forks
    // fail, which still counts them as handled.
    let dup = SessionId::new();
    let (first_reply, first) = crossbeam_channel::bounded(1);
    h.engine
        .commander()
        .0
        .send(spawn(dup, 2, first_reply))
        .unwrap();
    assert_eq!(
        probe(&h),
        Some(batch.clone()),
        "the last index arriving first keeps the cache for the others"
    );
    let mid = refused(&h, dup, 0);
    assert!(mid.text.contains("cannot fork"), "{}", mid.text);
    assert_eq!(probe(&h), Some(batch.clone()));
    let last = refused(&h, dup, 1);
    assert!(last.text.contains("cannot fork"), "{}", last.text);
    assert_eq!(
        probe(&h),
        None,
        "the last member handled releases the parent transcript"
    );
    let done = first.recv_timeout(WAIT).unwrap();
    assert!(done.text.contains("state=\"completed\""), "{}", done.text);
    h.engine.shutdown();
}

/// A worktree agent's checkout runs off the engine thread, and a failed
/// checkout still refuses the spawn.
#[test]
fn a_worktree_spawn_outside_a_repository_is_refused_after_its_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("ok")));
    let parent = SessionId::new();
    let mut defs = AgentDefs::builtin();
    let mut def = defs.get("general").unwrap().clone();
    def.isolation = Isolation::Worktree;
    defs.insert(def);
    h.engine.open(SessionSpec {
        cx: AgentContext::new("m", "").with_workdir(dir.path().to_path_buf()),
        agents: Some(AgentSetup {
            defs: Arc::new(defs),
            ..agent_setup(1, 1)
        }),
        ..h.spec(parent)
    });
    let (reply, answer) = crossbeam_channel::bounded(1);
    let spawn = Spawn {
        parent,
        tool_call_id: ToolCallId::new("call_a"),
        agent: "general".to_owned(),
        description: "a task".to_owned(),
        prompt: "work".to_owned(),
        reply,
        fork: false,
        swarm: None,
        model: None,
        thinking: None,
        background: false,
    };
    h.engine
        .commander()
        .0
        .send(Input::Spawn(Box::new(spawn)))
        .unwrap();
    let refused = answer.recv_timeout(WAIT).unwrap();
    assert!(refused.is_error);
    assert!(
        refused.text.contains("needs a git or jj repository"),
        "{}",
        refused.text
    );
    h.engine.shutdown();
}

/// Live `git rev-parse` processes working in `dir`.
#[cfg(target_os = "linux")]
fn rev_parse_in(dir: &std::path::Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let cwd = std::fs::read_link(entry.path().join("cwd")).ok()?;
            let cmdline = std::fs::read(entry.path().join("cmdline")).ok()?;
            let rev_parse = cmdline.windows(9).any(|w| w == b"rev-parse");
            (cwd == dir && rev_parse).then_some(pid)
        })
        .collect()
}

/// A git repository whose `git rev-parse` hangs: its config includes
/// a FIFO nobody writes, so a checkout blocks before it writes anything.
/// `None` without git.
#[cfg(target_os = "linux")]
fn hung_git_repo() -> Option<(tempfile::TempDir, std::path::PathBuf)> {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .ok()?;
    let repo = tempfile::tempdir().unwrap();
    let path = repo.path().canonicalize().unwrap();
    let fifo = path.join("hang");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
    for args in [
        &["init", "-q"][..],
        &["config", "include.path", fifo.to_str().unwrap()],
    ] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&path)
            .status()
            .unwrap();
        assert!(status.success());
    }
    Some((repo, path))
}

/// The parent spec for a session in `workdir` whose `general` agent
/// asks for a worktree.
#[cfg(target_os = "linux")]
fn worktree_parent(h: &Harness, id: SessionId, workdir: &std::path::Path) -> SessionSpec {
    let mut defs = AgentDefs::builtin();
    let mut def = defs.get("general").unwrap().clone();
    def.isolation = Isolation::Worktree;
    defs.insert(def);
    SessionSpec {
        cx: AgentContext::new("m", "").with_workdir(workdir.to_path_buf()),
        agents: Some(AgentSetup {
            defs: Arc::new(defs),
            ..agent_setup(1, 1)
        }),
        ..h.spec(id)
    }
}

/// Wait until a `git rev-parse` runs in `repo` (`running`), or until
/// none does.
#[cfg(target_os = "linux")]
fn wait_for_rev_parse(repo: &std::path::Path, running: bool) {
    let deadline = Instant::now() + WAIT;
    while rev_parse_in(repo).is_empty() == running {
        assert!(
            Instant::now() < deadline,
            "rev-parse running is not {running}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A checkout that hangs runs off the engine thread: other inputs are
/// still served meanwhile, and cancelling the parent stops the hung
/// VCS command without opening the child.
#[cfg(target_os = "linux")]
#[test]
fn a_hung_worktree_checkout_does_not_stall_the_engine() {
    let Some((_repo, repo_path)) = hung_git_repo() else {
        return;
    };
    let h = harness(MockProvider::sequence(vec![agent_turn(&[(
        "call_a",
        task("work"),
    )])]));
    let parent = SessionId::new();
    h.engine.open(worktree_parent(&h, parent, &repo_path));
    prompt(&h.engine, parent, "go", Delivery::Steer);
    wait_for_rev_parse(&repo_path, true);
    let (reply, answer) = crossbeam_channel::bounded(1);
    h.engine
        .commander()
        .0
        .send(Input::HostedSessions(reply))
        .unwrap();
    let hosted = answer
        .recv_timeout(Duration::from_secs(2))
        .expect("the engine stalled behind the checkout");
    assert_eq!(hosted.len(), 1, "only the parent is hosted: {hosted:?}");

    h.engine.send(Command::to(parent, CommandKind::Cancel));
    let events = until_runs_end(&h.events, 1);
    assert_eq!(outcomes(&events), [RunOutcome::Cancelled]);
    wait_for_rev_parse(&repo_path, false);
    let (reply, answer) = crossbeam_channel::bounded(1);
    h.engine
        .commander()
        .0
        .send(Input::HostedSessions(reply))
        .unwrap();
    assert_eq!(answer.recv_timeout(WAIT).unwrap().len(), 1);
    assert!(spawned(&events).is_empty());
    h.engine.shutdown();
}

/// A cancel sent to a child that is still waiting on its worktree, as
/// a swarm sends to the children that never reported, stops the
/// checkout and refuses the child.
#[cfg(target_os = "linux")]
#[test]
fn cancelling_a_child_in_checkout_stops_the_checkout() {
    let Some((_repo, repo_path)) = hung_git_repo() else {
        return;
    };
    let h = harness(MockProvider::replaying(text_turn("ok")));
    let parent = SessionId::new();
    h.engine.open(worktree_parent(&h, parent, &repo_path));
    let child = SessionId::new();
    let (reply, answer) = crossbeam_channel::bounded(1);
    let spawn = Spawn {
        parent,
        tool_call_id: ToolCallId::new("call_s"),
        agent: "general".to_owned(),
        description: "a task".to_owned(),
        prompt: "work".to_owned(),
        reply,
        fork: false,
        swarm: Some(SwarmInfo {
            id: child,
            batch_id: ToolCallId::new("swarm_batch"),
            index: 0,
            item: "a".to_owned(),
            total: 1,
        }),
        model: None,
        thinking: None,
        background: false,
    };
    h.engine
        .commander()
        .0
        .send(Input::Spawn(Box::new(spawn)))
        .unwrap();
    wait_for_rev_parse(&repo_path, true);

    h.engine.send(Command::to(child, CommandKind::Cancel));
    let refused = answer.recv_timeout(WAIT).unwrap();
    assert!(refused.is_error);
    assert!(refused.text.contains("cancelled"), "{}", refused.text);
    wait_for_rev_parse(&repo_path, false);
    let hosted = h.engine.hosted_sessions();
    assert!(!hosted.iter().any(|(id, _)| *id == child), "{hosted:?}");
    h.engine.shutdown();
}
