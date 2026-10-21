//! Print-mode run driver and the hooks wrapper.

use std::io;
use std::process::ExitCode;
use std::sync::Arc;

use kage_core::{Content, LoopEvent, Message};
use kage_loop::{AgentContext, LoopConfig};
use kage_provider::ProviderRegistry;
use kage_session::{SessionId, SessionWriter};

use crate::cli_printing::{print_envelope_json, print_event};
use crate::engine::WithImageGeneration as _;
use kage_core::agent_report::{AgentReport, ReportState};

/// The layered config for `workdir`, refused when its permission rules
/// or shell policy do not validate.
fn checked_config(workdir: &std::path::Path) -> Result<kage_core::config::Config, String> {
    let layered = kage_core::config::Config::load_layered(workdir).map_err(|e| e.to_string())?;
    layered.permissions.validate().map_err(|e| e.to_string())?;
    layered.shell.validate().map_err(|e| e.to_string())?;
    Ok(layered)
}

/// Drive one print-mode run on the engine. Streams events to stdout as
/// text or JSONL, records the conversation when a writer is supplied, and
/// maps the outcome to a process exit code. Text mode prints the opened
/// session only, while JSON mode prints the envelopes of its agents too.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is separately built run state"
)]
pub(crate) fn execute_print_run(
    registry: Arc<ProviderRegistry>,
    model: &str,
    tools: kage_tools::ToolRegistry,
    mut cx: AgentContext,
    prompt: String,
    writer: Option<SessionWriter>,
    plugin_runtime: Option<Arc<kage_plugin::PluginRuntime>>,
    mcp: Option<kage_mcp::McpManager>,
    json_mode: bool,
    yolo: bool,
) -> ExitCode {
    use kage_core::protocol::{Command, CommandKind, Delivery, Event, HostEvent, RunOutcome};

    let workdir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::trust::warn_if_untrusted(&workdir);
    let layered = match checked_config(&workdir) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let tools = tools
        .with_shell_config(&layered.shell)
        .with_web_search(&layered.tools.web_search)
        .with_image_generation(&layered);
    if layered.permissions.confine_paths {
        cx = cx.with_confine_paths();
    } else {
        cx = cx.without_confine_paths();
        eprintln!(
            "kage: path confinement is OFF for this session (permissions.confine_paths = false)"
        );
    }
    if let Err(err) = crate::sigint::install() {
        eprintln!("kage: {err}; Ctrl-C will kill the process");
    }

    let (defs, agent_errors) = crate::agents::load(&workdir);
    for err in agent_errors {
        eprintln!("kage: {err}");
    }
    let agents =
        crate::engine::AgentSetup::from_config(defs, &layered, crate::engine::Background::Off);

    let session = writer
        .as_ref()
        .and_then(|w| crate::engine::session_id_of(w.path()))
        .unwrap_or_default();
    let (ended_tx, ended_rx) = std::sync::mpsc::channel();
    let printer: crate::engine::Subscriber = Box::new(move |envelope| {
        print_envelope(&mut io::stdout().lock(), envelope, session, json_mode);
        if envelope.session == session
            && let Event::Host(HostEvent::RunEnded { outcome }) = &envelope.event
        {
            let _ = ended_tx.send(outcome.clone());
        }
    });

    let mcp_servers = mcp
        .as_ref()
        .map(|m| m.server_names().map(str::to_owned).collect())
        .unwrap_or_default();
    let engine = crate::engine::Engine::start(registry);
    engine.subscribe(printer);
    let gate = crate::permissions::PermissionGate::new(layered.permissions)
        .with_mcp_servers(mcp_servers)
        .with_aliases(tools.alias_map());
    // A yolo run parks the gate in allow mode: no asks, but a
    // configured deny still denies.
    if yolo {
        gate.set_mode(Some(kage_core::permissions::PermissionAction::Allow));
    }
    engine.open(crate::engine::SessionSpec {
        id: session,
        model: model.to_owned(),
        cx,
        recorder: writer.map(|w| crate::engine::Recorder::new(w, plugin_runtime.clone())),
        tools,
        plugins: plugin_runtime,
        gate,
        loop_cfg: LoopConfig {
            compaction_threshold: layered.loop_settings.compaction_threshold,
            ..LoopConfig::default()
        },
        mcp,
        interactive: false,
        title: false,
        agents: Some(agents),
        shell: layered.shell.program.clone(),
    });
    engine.send(Command::active(CommandKind::Prompt {
        content: vec![Content::Text { text: prompt }],
        delivery: Delivery::Queue,
    }));

    let outcome = loop {
        match ended_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(outcome) => break Some(outcome),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if crate::sigint::requested() {
                    engine.send(Command::active(CommandKind::Cancel));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break None,
        }
    };
    engine.shutdown();
    if !json_mode {
        println!();
    }
    if crate::sigint::requested() {
        return ExitCode::from(130);
    }
    match outcome {
        Some(RunOutcome::Completed) => ExitCode::SUCCESS,
        _ => ExitCode::from(1),
    }
}

/// Print one envelope: every envelope as a JSON line in JSON mode, else
/// the loop events of `session` as text and notices on stderr.
pub(crate) fn print_envelope<W: io::Write>(
    out: &mut W,
    envelope: &kage_core::protocol::Envelope,
    session: SessionId,
    json_mode: bool,
) {
    use kage_core::protocol::{Event, HostEvent};

    if json_mode {
        print_envelope_json(out, envelope);
        return;
    }
    match &envelope.event {
        Event::Loop(event) if envelope.session == session => print_loop_event(out, event),
        Event::Host(HostEvent::Notice { text, .. }) => eprintln!("kage: {text}"),
        _ => {}
    }
}

/// Print a loop event as text. An `agent` call prints the agent and its
/// description when it starts and how the agent ended, instead of the
/// generic tool lines and the wrapper the model reads.
fn print_loop_event<W: io::Write>(out: &mut W, event: &LoopEvent) {
    match event {
        LoopEvent::ToolCallStart {
            name,
            input_partial,
            ..
        } if name == "agent" => {
            let field = |key| input_partial.get(key).and_then(serde_json::Value::as_str);
            let agent = field("agent").unwrap_or("general");
            let description = field("description").unwrap_or_default();
            let _ = writeln!(out, "\n[agent {agent}: {description}]");
            let _ = out.flush();
        }
        LoopEvent::ToolCallEnd { output, .. } => match AgentReport::parse(&output.text) {
            Some(report) if report.state == ReportState::Failed => {
                let _ = writeln!(out, "[agent {} failed] {}", report.name, report.body);
                let _ = out.flush();
            }
            Some(report) => {
                let state = report.state.as_str();
                match report.limit {
                    Some(limit) => {
                        let _ = writeln!(out, "[agent {} {state}: {}]", report.name, limit.label());
                    }
                    None => {
                        let _ = writeln!(out, "[agent {} {state}]", report.name);
                    }
                }
                let _ = out.flush();
            }
            None => print_event(out, event),
        },
        _ => print_event(out, event),
    }
}

/// Extract the first text block from a user message, joined with newlines
/// if there are multiple. Returns an empty string when the message carries
/// no text (image-only, tool-result-only, etc.).
pub(crate) fn first_user_text(msg: &Message) -> String {
    let mut out = String::new();
    for block in &msg.content {
        if let Content::Text { text } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}
