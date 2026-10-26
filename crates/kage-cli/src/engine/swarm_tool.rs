//! The `swarm` tool: starts one child agent per item from a prompt
//! template and waits for every child, then returns one aggregated
//! result.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossbeam_channel::select_biased;
use kage_core::agent_report::AgentReport;
use kage_core::agents::AgentDefs;
use kage_core::protocol::{Command, CommandKind};
use kage_core::{Risk, SessionId, ToolCallId, ToolOutput};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};
use serde::Deserialize;
use ulid::Ulid;

use super::agent_tool::{self, Spawn};
use super::{Attach, Input, ResumeChild};

/// Name the model calls the tool by.
pub(super) const SWARM_TOOL: &str = "swarm";

/// Placeholder every item replaces in the prompt template.
const PLACEHOLDER: &str = "{{item}}";

/// How long a timed-out swarm still waits for the children the engine
/// is cancelling, so their states land in the aggregate.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// Longest whole swarm result, in bytes. Per-child replies are already
/// capped at the `agent` tool's limit; when the aggregate is still over
/// this, bodies are cut before any status line. It stays under the
/// loop's own cap on tool results, which would otherwise cut the middle
/// children's status lines out.
pub(super) const RESULT_CAP: usize = kage_core::MAX_TOOL_RESULT_BYTES - 256;

/// Longest item or description an aggregate repeats per child, in
/// characters. Both land in every `<swarm>` element, so a long one
/// would eat the bodies' share of the result cap.
const ATTR_CHARS: usize = 160;

/// Marks a body the result cap cut.
const BODY_CUT: &str = "\n[body truncated to fit the result cap]";

/// How long the tool waits for the engine's answer to a resume
/// check. The engine answers on its own thread, so this only guards
/// against a stopped or wedged engine.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// Appended when any child did not complete, so the model knows the
/// fleet can be continued.
const RESUME_HINT: &str = "[hint: some children did not complete. Call swarm again with \
resume, mapping their session ids above to a follow-up prompt.]";

/// Context block injected once when a session's swarm mode turns on.
pub(crate) const SWARM_MODE_ON: &str = "[swarm mode on] Split the work early and delegate: \
prefer many small, independent `swarm` items over one big task. Every child starts with zero \
context, so each expanded prompt must be self-contained, holding the paths and details the \
child needs. Keep scopes disjoint: children may edit in parallel while each touches its own \
files, and files several children need belong to one of them. The swarm's aggregate is the \
tracking, so don't mirror its items into the todo list. When the tasks differ, make several \
`agent` calls instead. Turn swarm mode off with `/swarm off`.";

/// Context block injected once when a session's swarm mode turns off.
pub(crate) const SWARM_MODE_OFF: &str = "[swarm mode off] Back to the normal workflow: do the \
work yourself or use `agent` calls for separate tasks.";

/// Identifies one child of a `swarm` call, recorded in the child's
/// session marker.
#[derive(Clone, Debug)]
/// What one call runs: the batch facts a child carries so cards and
/// the working row can show batch progress.
pub(crate) struct SwarmInfo {
    /// The child's session id, assigned by the tool so the swarm can
    /// cancel children that never reported.
    pub id: SessionId,
    /// Id of the swarm call, shared by every child of one call.
    pub batch_id: ToolCallId,
    /// Position of this child's item, 0-based.
    pub index: usize,
    /// The item this child was spawned for.
    pub item: String,
    /// How many children the whole batch has.
    pub total: usize,
}

#[derive(Deserialize)]
struct SwarmInput {
    description: String,
    #[serde(default = "default_agent")]
    agent: String,
    #[serde(default)]
    prompt_template: Option<String>,
    #[serde(default)]
    items: Vec<String>,
    #[serde(default)]
    resume: BTreeMap<String, String>,
    /// Spawn every new child from a snapshot of this conversation
    /// instead of zero context.
    #[serde(default)]
    fork: bool,
    /// Model every new child runs, over the agent definition's.
    #[serde(default)]
    model: Option<String>,
    /// Thinking level for every new child, over the definition's.
    #[serde(default)]
    thinking: Option<String>,
}

fn default_agent() -> String {
    "general".to_owned()
}

/// Starts a swarm for one session's run. The dispatcher registers it
/// into the run's tools next to the `agent` tool.
#[derive(Debug)]
pub(super) struct SwarmTool {
    parent: SessionId,
    engine: mpsc::Sender<Input>,
    defs: AgentDefs,
    description: String,
    schema: serde_json::Value,
    max_items: usize,
    /// Per-child run budget in milliseconds, from its run start.
    timeout: Duration,
}

impl SwarmTool {
    pub(super) fn new(
        parent: SessionId,
        engine: mpsc::Sender<Input>,
        defs: &AgentDefs,
        max_items: usize,
        timeout: Duration,
    ) -> Self {
        let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
        let description = format!(
            "Start a swarm: one call spawns the same agent once per item and waits for \
             every child. Its final reply is one aggregated result with a summary line \
             and each child's session id, state and reply.\n\n\
             Use a swarm when many children can run the same kind of task over different \
             inputs, such as the same fix across many crates or a review of many files, \
             and prefer more, smaller, independent items over one big one. For a few \
             differently-shaped tasks, make several `agent` calls in one message \
             instead.\n\n\
             Every child starts with zero context, so each expanded prompt must be \
             self-contained, holding the paths and details the child needs. Split the \
             work so scopes are disjoint: no duplicated work, and children may edit in \
             parallel while each touches its own files; files several children need \
             belong to one of them.\n\n\
             Agents:\n{}",
            defs.tool_listing()
        );
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short label for the whole swarm."
                },
                "agent": {
                    "type": "string",
                    "enum": names,
                    "description": "The agent every child runs. Defaults to general."
                },
                "prompt_template": {
                    "type": "string",
                    "description":
                        format!("Task template for the items; every {PLACEHOLDER} is replaced with the item. Required with items.")
                },
                "items": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 2,
                    "maxItems": max_items,
                    "description": "One new child per entry, substituted into the template. Required without resume."
                },
                "resume": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description":
                        "Map of child session id to a follow-up prompt, to continue children of an earlier swarm call of this session instead of spawning new ones. Mixes with items. Items and resume entries together count toward the swarm cap."
                },
                "fork": {
                    "type": "boolean",
                    "description": "Spawn every new child from a snapshot of this conversation instead of zero context. Default false. Cannot be combined with resume."
                },
                "model": {
                    "type": "string",
                    "description": "Model every new child runs, as provider/model. Overrides the agent definition's model. Resumed children keep their own."
                },
                "thinking": {
                    "type": "string",
                    "enum": ["off", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"],
                    "description": "Thinking level for every new child. Overrides the agent definition's thinking. Resumed children keep their own."
                }
            },
            "required": ["description"]
        });
        Self {
            parent,
            engine,
            defs: defs.clone(),
            description,
            schema,
            max_items,
            timeout,
        }
    }
}

impl Tool for SwarmTool {
    fn name(&self) -> &str {
        SWARM_TOOL
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    fn risk(&self) -> Risk {
        Risk::Exec
    }

    fn execution_mode(&self) -> Option<ExecMode> {
        Some(ExecMode::Sequential)
    }

    fn runs_alone(&self) -> bool {
        true
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: SwarmInput = serde_json::from_value(input)?;
        let call = match expand(&input, &self.defs, self.max_items) {
            Ok(call) => call,
            Err(text) => return Ok(agent_tool::error_output(text)),
        };
        let call_id = cx
            .call_id()
            .cloned()
            .ok_or_else(|| ToolError::InvalidInput("the swarm tool needs a call id".into()))?;
        let total = call.items.len() + call.resume.len();
        let batch_id = ToolCallId::new(format!("swarm_{}", Ulid::generate()));
        // One channel for the whole batch: every child delivers its
        // result once, so the tool receives exactly `total` results.
        let (reply, results) = crossbeam_channel::bounded(total);
        let mut members: Vec<Member> = Vec::with_capacity(total);
        if !call.resume.is_empty() {
            let children = match self.verify_resume(&call.resume) {
                Ok(children) => children,
                Err(text) => return Ok(agent_tool::error_output(text)),
            };
            for (position, (child, (_, prompt))) in
                children.into_iter().zip(&call.resume).enumerate()
            {
                let attach = Attach {
                    parent: self.parent,
                    id: child.id,
                    agent: child.agent.clone(),
                    description: child.description,
                    tool_call_id: call_id.clone(),
                    prompt: prompt.clone(),
                    reply: reply.clone(),
                    swarm: Some(SwarmInfo {
                        id: child.id,
                        batch_id: batch_id.clone(),
                        index: call.items.len() + position,
                        item: child.item.clone(),
                        total,
                    }),
                };
                if self.engine.send(Input::Attach(Box::new(attach))).is_err() {
                    break;
                }
                members.push(Member {
                    id: child.id,
                    item: child.item.clone(),
                    agent: child.agent.clone(),
                });
            }
        }
        for (index, (item, prompt)) in call.items.into_iter().enumerate() {
            let id = SessionId::new();
            let spawn = Spawn {
                parent: self.parent,
                tool_call_id: call_id.clone(),
                agent: input.agent.clone(),
                description: input.description.clone(),
                prompt,
                reply: reply.clone(),
                fork: call.fork,
                swarm: Some(SwarmInfo {
                    id,
                    batch_id: batch_id.clone(),
                    index,
                    item: item.clone(),
                    total,
                }),
                model: call.overrides.model.clone(),
                thinking: call.overrides.thinking,
                background: false,
            };
            if self.engine.send(Input::Spawn(Box::new(spawn))).is_err() {
                break;
            }
            members.push(Member {
                id,
                item,
                agent: input.agent.clone(),
            });
        }
        drop(reply);
        if members.is_empty() {
            return Ok(agent_tool::error_output("the engine stopped".to_owned()));
        }
        let ids: Vec<SessionId> = members.iter().map(|member| member.id).collect();
        let backstop = self
            .timeout
            .saturating_mul(u32::try_from(total).unwrap_or(u32::MAX).saturating_add(1));
        let arrived = collect(total, &ids, backstop, &results, &self.engine, cx);
        Ok(render(RESULT_CAP, &input.description, &members, &arrived))
    }
}

/// Wait for every child to deliver. Each child carries its own
/// deadline (the engine's watchdog cancels it at `swarm_timeout_ms`
/// from its run start), so a batch of many children is not cut off by
/// one whole-call timer; `backstop` is only a last resort for a
/// wedged engine. When the parent is cancelled, whatever arrived is
/// kept and the rest renders as cancelled. On backstop or engine
/// shutdown, the children that never reported are cancelled and the
/// results that still land within a short grace are kept.
fn collect(
    total: usize,
    children: &[SessionId],
    backstop: Duration,
    results: &crossbeam_channel::Receiver<ToolOutput>,
    engine: &mpsc::Sender<Input>,
    cx: &ToolContext<'_>,
) -> Vec<ToolOutput> {
    let watch = cx.cancel_flag().watch();
    let deadline = Instant::now() + backstop;
    let mut arrived: Vec<ToolOutput> = Vec::with_capacity(total);
    let cancelled = loop {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break false;
        };
        select_biased! {
            recv(results) -> output => match output {
                Ok(output) => arrived.push(output),
                Err(_) => break false,
            },
            recv(watch.receiver()) -> _ => break true,
            recv(crossbeam_channel::after(left)) -> _ => break false,
        }
    };
    if cancelled {
        while let Ok(output) = results.try_recv() {
            arrived.push(output);
        }
        return arrived;
    }
    if arrived.len() < total {
        for id in unreported(children, &arrived) {
            let _ = engine.send(Input::Command(Command::to(id, CommandKind::Cancel)));
        }
        let grace = Instant::now() + CANCEL_GRACE;
        while arrived.len() < total {
            let Some(left) = grace.checked_duration_since(Instant::now()) else {
                break;
            };
            match results.recv_timeout(left) {
                Ok(output) => arrived.push(output),
                Err(_) => break,
            }
        }
    }
    arrived
}

/// The children to cancel when a batch gives up: the ones that never
/// reported. A child that reported was reaped once its result was
/// delivered, so cancelling it only draws an `unknown session` error
/// notice onto its id.
fn unreported(children: &[SessionId], arrived: &[ToolOutput]) -> Vec<SessionId> {
    let reported: HashSet<SessionId> = arrived.iter().filter_map(session_in).collect();
    children
        .iter()
        .copied()
        .filter(|id| !reported.contains(id))
        .collect()
}

impl SwarmTool {
    /// Ask the engine to check every resume id before anything is
    /// attached. Blocks until the engine answers.
    fn verify_resume(&self, resume: &[(SessionId, String)]) -> Result<Vec<ResumeChild>, String> {
        let (reply, verified) = crossbeam_channel::bounded(1);
        let ids: Vec<SessionId> = resume.iter().map(|(id, _)| *id).collect();
        if self
            .engine
            .send(Input::VerifyResume {
                parent: self.parent,
                ids,
                reply,
            })
            .is_err()
        {
            return Err("the engine stopped".to_owned());
        }
        verified
            .recv_timeout(VERIFY_TIMEOUT)
            .map_err(|_| "the engine did not answer the resume check".to_owned())?
    }
}

/// What one call runs: one new child per item, plus the earlier
/// children the resume map re-prompts.
#[derive(Debug)]
struct Call {
    /// (item, prompt) per new child.
    items: Vec<(String, String)>,
    /// Session id and prompt per resumed child, in map order.
    resume: Vec<(SessionId, String)>,
    /// New children start from a snapshot of the parent's
    /// conversation instead of zero context.
    fork: bool,
    /// Model and thinking every new child runs with.
    overrides: agent_tool::Overrides,
}

/// Expand the call into one prompt per item plus the resume entries,
/// or the reason it is invalid. The rules are whole-call: nothing
/// spawns or attaches unless every check passes.
fn expand(input: &SwarmInput, defs: &AgentDefs, max_items: usize) -> Result<Call, String> {
    let overrides = agent_tool::parse_overrides(input.model.as_deref(), input.thinking.as_deref())?;
    if input.fork && !input.resume.is_empty() {
        return Err(
            "fork applies to new children; drop resume to spawn forked children instead".to_owned(),
        );
    }
    if input.items.is_empty() && input.resume.is_empty() {
        return Err(
            "a swarm needs at least 2 items, or a resume map naming the children to continue"
                .to_owned(),
        );
    }
    let total = input.items.len() + input.resume.len();
    if total > max_items {
        return Err(format!(
            "a swarm is capped at {max_items} items (swarm_max_items), the call lists {total} \
             ({} items + {} resume entries)",
            input.items.len(),
            input.resume.len()
        ));
    }
    let mut items = Vec::new();
    if !input.items.is_empty() {
        if defs.get(&input.agent).is_none() {
            let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
            return Err(format!(
                "unknown agent `{}`. Available agents: {}",
                input.agent,
                names.join(", ")
            ));
        }
        let Some(template) = &input.prompt_template else {
            return Err("items need a prompt_template".to_owned());
        };
        if input.items.len() < 2 {
            return Err(format!(
                "a swarm needs at least 2 items (got {})",
                input.items.len()
            ));
        }
        if !template.contains(PLACEHOLDER) {
            return Err(format!(
                "prompt_template must contain {PLACEHOLDER}, so every item has \
                 its place in the prompt"
            ));
        }
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (index, item) in input.items.iter().enumerate() {
            if item.trim().is_empty() {
                return Err(format!("items[{}] is empty", index + 1));
            }
            let prompt = template.replace(PLACEHOLDER, item);
            if let Some(first) = seen.get(&prompt) {
                return Err(format!(
                    "items {} and {} expand to the same prompt (\"{}\"); \
                     drop one or make them differ",
                    first + 1,
                    index + 1,
                    item
                ));
            }
            seen.insert(prompt.clone(), index);
            items.push((item.clone(), prompt));
        }
    }
    let mut resume: Vec<(SessionId, String)> = Vec::with_capacity(input.resume.len());
    for (key, prompt) in &input.resume {
        let id = Ulid::from_string(key)
            .map(SessionId)
            .map_err(|_| format!("resume: `{key}` is not a session id"))?;
        if prompt.trim().is_empty() {
            return Err(format!("resume: the prompt for {key} is empty"));
        }
        if resume.iter().any(|(seen, _)| *seen == id) {
            return Err(format!("resume: {key} names session {id} twice"));
        }
        resume.push((id, prompt.clone()));
    }
    Ok(Call {
        items,
        resume,
        fork: input.fork,
        overrides,
    })
}

/// One slot in the swarm: a child session, the item it runs, and the
/// agent definition it uses.
pub(super) struct Member {
    pub(super) id: SessionId,
    pub(super) item: String,
    pub(super) agent: String,
}

/// One child's slot in the aggregate result: the lines around its
/// reply and the reply itself, so a result over the cap loses body
/// before it loses status.
struct Block {
    open: String,
    body: String,
    close: &'static str,
}

impl Block {
    /// The bytes the block costs beyond its body, with the newline
    /// that sets it apart from the one before.
    fn overhead(&self) -> usize {
        self.open.len() + self.close.len() + 2
    }

    fn render(&self, body: &str) -> String {
        format!("{}\n{body}{}", self.open, self.close)
    }
}

/// The aggregate result: a summary line, then one `<swarm>` element
/// per member wrapping the child's `<agent>` element. Children that
/// never reported render as cancelled, session id included, and a
/// hint names the resume path. The call is an error only when every
/// child failed.
pub(super) fn render(
    cap: usize,
    description: &str,
    members: &[Member],
    results: &[ToolOutput],
) -> ToolOutput {
    // Every result is an `<agent>` element naming its child, engine
    // refusals included.
    let mut slots: Vec<Option<&ToolOutput>> = vec![None; members.len()];
    for result in results.iter().filter(|result| is_agent(result)) {
        let at =
            session_in(result).and_then(|id| members.iter().position(|member| member.id == id));
        if let Some(at) = at {
            slots[at] = Some(result);
        }
    }
    let description = escape(&clip(description));
    let mut completed = 0;
    let mut failed = 0;
    let mut cancelled = 0;
    let mut blocks = Vec::with_capacity(members.len());
    for (member, slot) in members.iter().zip(slots) {
        let swarm_open = format!(
            "<swarm description=\"{description}\" item=\"{}\">",
            escape(&clip(&member.item))
        );
        let (open, body, state) = match slot {
            Some(result) => {
                let (header, rest) = result.text.split_once('\n').unwrap_or((&result.text, ""));
                (
                    format!("{swarm_open}\n{header}"),
                    rest.strip_suffix("\n</agent>").unwrap_or(rest).to_owned(),
                    state_in(result).unwrap_or("failed"),
                )
            }
            None => (
                format!(
                    "{swarm_open}\n<agent name=\"{}\" session=\"{}\" state=\"cancelled\">",
                    escape(&member.agent),
                    member.id
                ),
                "no result arrived".to_owned(),
                "cancelled",
            ),
        };
        match state {
            "completed" => completed += 1,
            "failed" => failed += 1,
            _ => cancelled += 1,
        }
        blocks.push(Block {
            open,
            body,
            close: "\n</agent>\n</swarm>",
        });
    }
    let summary = format!("completed: {completed}, failed: {failed}, cancelled: {cancelled}");
    let hint = (failed + cancelled > 0).then_some(RESUME_HINT);
    let mut budget = cap
        .saturating_sub(summary.len())
        .saturating_sub(hint.map_or(0, |hint| hint.len() + 1));
    for block in &blocks {
        budget = budget.saturating_sub(block.overhead());
    }
    let mut out = summary;
    for (block, body) in blocks.iter().zip(fit_bodies(&blocks, budget)) {
        let _ = write!(out, "\n{}", block.render(&body));
    }
    if let Some(hint) = hint {
        let _ = write!(out, "\n{hint}");
    }
    ToolOutput {
        text: out,
        is_error: !members.is_empty() && failed == members.len(),
        ..ToolOutput::default()
    }
}

/// Each block's body within `budget` bytes in all. Short bodies go
/// first and whole, so the long ones split what they leave; a body cut
/// to its share ends with [`BODY_CUT`], or is dropped when the share
/// cannot hold even that.
fn fit_bodies(blocks: &[Block], mut budget: usize) -> Vec<String> {
    let mut order: Vec<usize> = (0..blocks.len()).collect();
    order.sort_by_key(|&at| blocks[at].body.len());
    let mut bodies = vec![String::new(); blocks.len()];
    for (done, &at) in order.iter().enumerate() {
        let share = budget / (blocks.len() - done);
        let body = &blocks[at].body;
        bodies[at] = if body.len() <= share {
            body.clone()
        } else if share >= BODY_CUT.len() {
            format!("{}{BODY_CUT}", truncate(body, share - BODY_CUT.len()))
        } else {
            String::new()
        };
        budget -= bodies[at].len();
    }
    bodies
}

/// `value` cut to [`ATTR_CHARS`] characters, marked when cut.
fn clip(value: &str) -> String {
    match value.char_indices().nth(ATTR_CHARS) {
        Some((at, _)) => format!("{}...", &value[..at]),
        None => value.to_owned(),
    }
}

/// Cut `text` to at most `cap` bytes, on a char boundary.
fn truncate(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut at = cap;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    text[..at].to_owned()
}

/// Whether a result is an `<agent>` element the `agent` tool rendered.
fn is_agent(result: &ToolOutput) -> bool {
    result
        .text
        .lines()
        .next()
        .is_some_and(|line| line.starts_with("<agent "))
}

/// The session id in a result's `<agent ...>` header line.
fn session_in(result: &ToolOutput) -> Option<SessionId> {
    AgentReport::header(result.text.lines().next()?).map(|report| report.session)
}

/// The run state in a result's `<agent ...>` header line.
fn state_in(result: &ToolOutput) -> Option<&'static str> {
    AgentReport::header(result.text.lines().next()?).map(|report| report.state.as_str())
}

/// Make `value` safe inside a double-quoted attribute on one line.
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\n', "&#10;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::AgentSetup;
    use kage_core::thinking::ThinkingLevel;

    fn input(items: &[&str]) -> SwarmInput {
        SwarmInput {
            description: "a swarm".into(),
            agent: "general".into(),
            prompt_template: Some("handle {{item}}".into()),
            items: items.iter().map(|item| (*item).to_owned()).collect(),
            resume: BTreeMap::new(),
            fork: false,
            model: None,
            thinking: None,
        }
    }

    #[test]
    fn expand_replaces_every_placeholder() {
        let mut call = input(&["a", "b"]);
        call.prompt_template = Some("{{item}} then {{item}} again".into());
        let call = expand(&call, &AgentDefs::builtin(), 32).unwrap();
        let prompts: Vec<&str> = call
            .items
            .iter()
            .map(|(_, prompt)| prompt.as_str())
            .collect();
        assert_eq!(prompts, ["a then a again", "b then b again"]);
    }

    #[test]
    fn expand_rejects_a_bad_call() {
        let defs = AgentDefs::builtin();
        let mut call = input(&[]);
        assert!(
            expand(&call, &defs, 32)
                .unwrap_err()
                .contains("needs at least 2 items, or a resume map")
        );

        call = input(&["a"]);
        assert!(expand(&call, &defs, 32).unwrap_err().contains("at least 2"));

        call = input(&["a", "b"]);
        call.prompt_template = Some("no placeholder".into());
        assert!(
            expand(&call, &defs, 32)
                .unwrap_err()
                .contains("must contain {{item}}")
        );

        call = input(&["a", "a"]);
        let err = expand(&call, &defs, 32).unwrap_err();
        assert!(err.contains("items 1 and 2"), "{err}");
        assert!(err.contains("\"a\""), "{err}");

        call = input(&["a", "b"]);
        assert!(expand(&call, &defs, 2).is_ok());
        let err = expand(&call, &defs, 1).unwrap_err();
        assert!(err.contains("capped at 1 items"), "{err}");

        call = input(&["a", " "]);
        assert!(expand(&call, &defs, 32).unwrap_err().contains("empty"));

        call = input(&["a", "b"]);
        call.agent = "nope".into();
        let err = expand(&call, &defs, 32).unwrap_err();
        assert!(err.contains("explore, general"), "{err}");
    }

    #[test]
    fn expand_validates_the_model_and_thinking_overrides() {
        let defs = AgentDefs::builtin();
        let mut call = input(&["a", "b"]);
        call.model = Some("sonnet".into());
        let err = expand(&call, &defs, 32).unwrap_err();
        assert!(err.contains("provider/model"), "{err}");

        call = input(&["a", "b"]);
        call.thinking = Some("much".into());
        let err = expand(&call, &defs, 32).unwrap_err();
        assert!(err.contains("thinking"), "{err}");

        call = input(&["a", "b"]);
        call.model = Some("mock:other".into());
        call.thinking = Some("high".into());
        let call = expand(&call, &defs, 32).unwrap();
        assert_eq!(call.overrides.model.as_deref(), Some("mock/other"));
        assert_eq!(call.overrides.thinking, Some(ThinkingLevel::High));
    }

    #[test]
    fn expand_caps_items_and_resume_together() {
        let defs = AgentDefs::builtin();
        let mut call = input(&["a", "b"]);
        call.resume
            .insert(SessionId::new().to_string(), "go on".into());
        let err = expand(&call, &defs, 2).unwrap_err();
        assert!(err.contains("capped at 2 items"), "{err}");
        assert!(err.contains("lists 3"), "{err}");
        assert!(err.contains("2 items + 1 resume"), "{err}");

        // A resume map alone is capped too.
        let mut call = input(&[]);
        for i in 0..3 {
            call.resume
                .insert(SessionId::new().to_string(), format!("go on {i}"));
        }
        let err = expand(&call, &defs, 2).unwrap_err();
        assert!(err.contains("lists 3"), "{err}");
    }

    #[test]
    fn expand_parses_the_resume_map() {
        let defs = AgentDefs::builtin();
        let mut call = input(&[]);
        call.resume
            .insert("not-a-session-id".into(), "go on".into());
        let err = expand(&call, &defs, 32).unwrap_err();
        assert!(err.contains("is not a session id"), "{err}");

        let id = SessionId::new();
        call.resume.clear();
        call.resume.insert(id.to_string(), "   ".into());
        let err = expand(&call, &defs, 32).unwrap_err();
        assert!(err.contains("the prompt for"), "{err}");

        call.resume.insert(id.to_string(), "go on".into());
        let expanded = expand(&call, &defs, 32).unwrap();
        assert_eq!(expanded.resume, [(id, "go on".to_owned())]);

        let mut mixed = input(&["a", "b"]);
        mixed.resume.insert(id.to_string(), "go on".into());
        let expanded = expand(&mixed, &defs, 32).unwrap();
        assert_eq!(expanded.items.len(), 2);
        assert_eq!(expanded.resume.len(), 1);
    }

    #[test]
    fn expand_rejects_fork_with_resume_and_passes_fork_through() {
        let defs = AgentDefs::builtin();
        let id = SessionId::new();
        let mut call = input(&["a", "b"]);
        call.fork = true;
        call.resume.insert(id.to_string(), "go on".into());
        let err = expand(&call, &defs, 32).unwrap_err();
        assert!(err.contains("fork applies to new children"), "{err}");

        call.resume.clear();
        let call = expand(&call, &defs, 32).unwrap();
        assert!(call.fork);
        assert!(call.resume.is_empty());
        assert_eq!(call.items.len(), 2);

        let plain = expand(&input(&["a", "b"]), &defs, 32).unwrap();
        assert!(!plain.fork);
    }

    #[test]
    fn config_defaults_reach_the_setup() {
        let defaults = AgentSetup::from_config(
            AgentDefs::builtin(),
            &kage_core::config::Config::default(),
            super::super::Background::Off,
        );
        assert_eq!(defaults.swarm_max_items, 32);
        assert_eq!(defaults.swarm_timeout_ms, 7_200_000);
    }

    /// One `<agent>` element like `agent_result` renders it.
    fn agent_block(session: SessionId, state: &str, body: &str) -> ToolOutput {
        ToolOutput {
            text: format!(
                "<agent name=\"general\" session=\"{session}\" state=\"{state}\">\n{body}\n</agent>"
            ),
            is_error: state != "completed",
            ..ToolOutput::default()
        }
    }

    fn two_members() -> (Vec<Member>, Vec<SessionId>) {
        let children = vec![SessionId::new(), SessionId::new()];
        let items = ["a".to_owned(), "b".to_owned()];
        let members: Vec<Member> = children
            .iter()
            .zip(items)
            .map(|(id, item)| Member {
                id: *id,
                item,
                agent: "general".into(),
            })
            .collect();
        (members, children)
    }

    #[test]
    fn render_counts_and_wraps_each_child() {
        let (members, children) = two_members();
        let results = vec![
            agent_block(children[0], "completed", "did a"),
            agent_block(children[1], "failed", "boom"),
        ];
        let out = render(RESULT_CAP, "fix things", &members, &results);
        assert!(
            out.text
                .starts_with("completed: 1, failed: 1, cancelled: 0\n"),
            "{}",
            out.text
        );
        assert!(out.text.contains(&format!(
            "<swarm description=\"fix things\" item=\"a\">\n<agent name=\"general\" \
             session=\"{}\" state=\"completed\">\ndid a\n</agent>\n</swarm>",
            children[0]
        )));
        assert!(out.text.contains("item=\"b\""));
        assert!(!out.is_error, "not every child failed");
    }

    #[test]
    fn render_marks_missing_children_cancelled() {
        let (members, children) = two_members();
        let out = render(RESULT_CAP, "d", &members, &[]);
        assert!(out.text.contains("completed: 0, failed: 0, cancelled: 2"));
        assert!(out.text.contains(&format!(
            "<agent name=\"general\" session=\"{}\" state=\"cancelled\">",
            children[1]
        )));
        assert!(out.text.contains("no result arrived"));
        assert!(out.text.contains("resume"));
        assert!(!out.is_error);
    }

    #[test]
    fn render_is_error_only_when_every_child_failed() {
        let (members, children) = two_members();
        let all_failed = vec![
            agent_block(children[0], "failed", "x"),
            agent_block(children[1], "failed", "y"),
        ];
        assert!(render(RESULT_CAP, "d", &members, &all_failed).is_error);
        let mixed = vec![
            agent_block(children[0], "completed", "x"),
            agent_block(children[1], "failed", "y"),
        ];
        assert!(!render(RESULT_CAP, "d", &members, &mixed).is_error);
    }

    #[test]
    fn truncation_keeps_every_status_line_and_cuts_bodies() {
        let (members, children) = two_members();
        let body = "x".repeat(500);
        let results = vec![
            agent_block(children[0], "completed", &body),
            agent_block(children[1], "completed", &body),
        ];
        let out = render(600, "d", &members, &results);
        let count = |needle: &str| out.text.matches(needle).count();
        assert_eq!(count("item=\"a\""), 1, "{}", out.text);
        assert_eq!(count("item=\"b\""), 1);
        assert_eq!(count("state=\"completed\""), 2);
        assert_eq!(count(BODY_CUT.trim_start()), 2);
        assert!(out.text.chars().count() < 1_000, "cut to the cap");
    }

    #[test]
    fn a_result_without_an_agent_header_is_left_out() {
        let (members, _children) = two_members();
        let odd = agent_tool::error_output("the engine stopped".into());
        let out = render(RESULT_CAP, "d", &members, &[odd]);
        assert!(
            out.text
                .starts_with("completed: 0, failed: 0, cancelled: 2\n")
        );
        assert!(!out.text.contains("the engine stopped"));
    }

    #[test]
    fn an_engine_refusal_lands_on_the_child_it_names() {
        let (members, children) = two_members();
        let refusal = agent_tool::refused(children[1], "general", "cannot fork: not recorded");
        let results = [refusal, agent_block(children[0], "completed", "did a")];
        let out = render(RESULT_CAP, "d", &members, &results);
        assert!(
            out.text
                .starts_with("completed: 1, failed: 1, cancelled: 0\n"),
            "{}",
            out.text
        );
        let second = out.text.split("item=\"b\"").nth(1).unwrap();
        assert!(second.contains("cannot fork"), "{}", out.text);
        assert!(!out.is_error);

        let both = [
            agent_tool::refused(children[0], "general", "no"),
            agent_tool::refused(children[1], "general", "no"),
        ];
        assert!(render(RESULT_CAP, "d", &members, &both).is_error);
    }

    #[test]
    fn long_items_are_cut_and_escaped_onto_one_line() {
        let id = SessionId::new();
        let item = format!("a\"b>\nc{}", "x".repeat(500));
        let members = [Member {
            id,
            item,
            agent: "general".into(),
        }];
        let out = render(
            RESULT_CAP,
            "d",
            &members,
            &[agent_block(id, "completed", "ok")],
        );
        let open = out.text.lines().nth(1).unwrap();
        assert!(
            open.starts_with("<swarm description=\"d\" item=\"a&quot;b&gt;&#10;cxx"),
            "{open}"
        );
        assert!(open.ends_with("...\">"), "{open}");
        assert!(open.len() < ATTR_CHARS + 60, "{open}");
    }

    #[test]
    fn a_large_fleet_stays_within_the_cap_and_keeps_short_bodies_whole() {
        let members: Vec<Member> = (0..128)
            .map(|n| Member {
                id: SessionId::new(),
                item: format!("item {n} {}", "y".repeat(400)),
                agent: "general".into(),
            })
            .collect();
        let results: Vec<ToolOutput> = members
            .iter()
            .enumerate()
            .map(|(n, m)| {
                let body = if n == 0 {
                    "short".to_owned()
                } else {
                    "z".repeat(5_000)
                };
                agent_block(m.id, "completed", &body)
            })
            .collect();
        let out = render(RESULT_CAP, &"d".repeat(400), &members, &results);
        assert!(out.text.len() <= RESULT_CAP, "{}", out.text.len());
        assert_eq!(out.text.matches("<swarm ").count(), 128);
        assert!(out.text.contains(">\nshort\n</agent>"));
    }

    #[test]
    fn a_resume_map_may_not_name_a_session_twice() {
        let id = SessionId::new();
        let mut call = input(&[]);
        call.resume.insert(id.to_string(), "go on".into());
        call.resume
            .insert(id.to_string().to_lowercase(), "go on too".into());
        let err = expand(&call, &AgentDefs::builtin(), 32).unwrap_err();
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn the_backstop_cancels_only_children_that_never_reported() {
        let (_members, children) = two_members();
        let reported = vec![agent_block(children[1], "completed", "did b")];
        assert_eq!(unreported(&children, &reported), [children[0]]);

        let none: Vec<ToolOutput> = Vec::new();
        assert_eq!(unreported(&children, &none), children);

        // A refusal is a result the child delivered, session id
        // included, so its sibling is the only one left to cancel.
        let refused = vec![agent_tool::refused(children[0], "general", "no")];
        assert_eq!(unreported(&children, &refused), [children[1]]);
    }
}
