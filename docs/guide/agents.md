# agents

kage can hand a task to an agent. An agent is a separate session with
a fresh context: it gets one task, works on it with its own tool
calls, and its final reply comes back as the result. The main
conversation keeps its context for the work that matters, while
agents do the searching, reading and test running.

Every agent stays in view. Its tool row shows what it does right now,
a list above the prompt keeps the running ones visible, and you can
open any agent to read its whole transcript, steer it or stop it.

## how it works

The model starts an agent with the `agent` tool. A call names the
agent definition, a short description you see, and the whole task:

```json
{"agent": "explore", "description": "map exports under src/components",
 "prompt": "Map the named exports of every file under src/components. Reply with one JSON object that maps each file to its export names, and nothing else."}
```

`agent` defaults to `general` when the call leaves it out. A call may
also set `model` (`provider/model`) and `thinking` (a level name) for
this one child. They override the definition's `model` and `thinking`;
left out, the definition decides, then `[agents] model` and
`[agents] thinking` (see [limits](#limits)), and otherwise the parent's
current model and level apply. The call
waits until the agent's run ends, then returns the text of the agent's
last reply, wrapped in an element that names the agent, its session
and how the run ended:

```text
<agent name="explore" session="01K62W8Q3T9V5M2C7X4B1N0R6S" state="completed">
{"Button.tsx": ["Button", "ButtonProps"], "Modal.tsx": ["Modal"]}
</agent>
```

`state` is `completed`, `cancelled` or `failed`. A cancelled agent
returns its partial reply and a failed one returns its error. Both
come back as error results, so the model knows the task did not
finish. When a limit ended the run, a `limit` attribute says which:
`turns`, `time` or `budget` (see [limits](#limits)). A worktree
agent's reply ends with where its work is (see
[worktree agents](#worktree-agents)). A reply over 20,000 characters is cut, and a trailer names the
agent's session, which holds the full transcript.

The header also records the child's tool count, token usage and run
time (`tools`, `in`, `out`, `cache_read`, `cache_write`, `cost`,
`ctx`, `win`, `run_ms`). When a session with finished agents reopens,
the agents list reads these back, so tokens, cost and times survive a
restart. The same attrs ride along on the `<agent>` headers inside a
swarm aggregate.

The agent sees nothing of the conversation that started it. The
prompt is everything it knows, and nobody answers its questions.

When every tool call in one assistant message is an `agent` call, the
agents run at the same time. A message that mixes `agent` calls with
other tools runs its calls one after another. The tool description
tells the model to use agents for independent work that needs many
tool calls, and to let agents edit in parallel only while each
touches its own files, leaving shared files to one of them.

## background agents

The main session can start an agent in the background. The `agent`
call then takes `background: true`:

```json
{"agent": "general", "description": "run the whole test suite", "background": true,
 "prompt": "Run cargo test --workspace and reply with the failing tests and their first error line."}
```

The call returns at once with `state="started"`, and the model goes on
with other work or ends its turn. When the agent ends, its result
reaches the main session as a message of its own, the same `<agent>`
element a foreground call returns. A running main session reads it at
its next turn boundary. An idle one depends on the client:

| Client | An idle main session |
| --- | --- |
| The TUI, the desktop and browser clients | starts a run to read the result |
| Editors over ACP | keeps the result for your next prompt, whose run reads it first |

Several results that arrive together are read as one message. A
result of an agent you stopped waits for the next run instead of
starting one.

Only the main session starts background agents. Print mode offers no
`background` parameter, since its process ends with its run.

Background agents outlive the run that started them. `esc` on the
main run leaves them running, and they stop when you stop them (`x` or
`X` in the agents overlay), when the session closes, or when kage
quits. Starting a new session, resuming another one or cloning waits
until they end, as for any running agent.

## swarms

The `swarm` tool starts the same agent once per item from one call.
It takes a prompt template, a list of items, and the agent every
child runs; each child's prompt is the template with every `{{item}}`
replaced by its item:

```json
{"description": "fix clippy in each crate",
 "agent": "general",
 "prompt_template": "Fix every clippy warning in {{item}}. Run `cargo clippy -p {{item}}` to see them. Reply with the warnings you fixed.",
 "items": ["kage-core", "kage-loop", "kage-tui"]}
```

A `model` (`provider/model`) and `thinking` on the call apply to every
new child, over the definition's, the same way an `agent` call's do.
Resumed children keep the model their session file records.

The call blocks until every child settles, then returns one aggregate:
a summary line, then each child's `<agent>` element wrapped in a
`<swarm>` element that names its item:

```text
completed: 2, failed: 1, cancelled: 0
<swarm description="fix clippy in each crate" item="kage-core">
<agent name="general" session="01K62W8Q3T9V5M2C7X4B1N0R6S" state="completed">
fixed 3 warnings: ...
</agent>
</swarm>
<swarm description="fix clippy in each crate" item="kage-loop">
...
</swarm>
```

The children go through the same queue as `agent` calls, so
`agent_max_running` throttles them, and each child shows up as a live
card and an agent-tree entry. Cards of one batch name the item their
child works on instead of repeating the batch description, and while
the batch runs, the working row shows its progress: `Swarm 3/12
done · 4 running`. The call is an error only when
every child failed. Each child gets its own budget: it may run for
`swarm_timeout_ms` from the moment its run starts, and a child still
running at its deadline is cancelled and renders as cancelled in the
aggregate. A child waiting in the queue does not burn its budget. A
child the provider rate limits past the loop's own retries is
requeued with a growing backoff, at most five times within its
budget, instead of failing at once. The
same happens to all children when you cancel the session. A swarm
call must be the only tool call in its message.

Use a swarm when many children can run the same kind of task over
different inputs, such as the same fix across many crates or a review
of many files, and prefer more, smaller, independent items over one
big one. For a few different tasks, make several `agent` calls in one
message instead. Coordination rules the tool description repeats:
every child starts with zero context, so each expanded prompt must be
self-contained; and keep scopes disjoint, so no work is duplicated
and children may edit in parallel while each touches its own files,
with files several children need left to one of them.

### resuming a swarm

Every child keeps its own session file, and the aggregate names each
child's session id. When children did not complete, the aggregate
ends with a hint, and a later `swarm` call can continue them: pass
`resume`, a map from child session id to a follow-up prompt, instead
of or mixed with `items`:

```json
{"description": "finish the clippy fixes",
 "resume": {"01K62W8Q3T9V5M2C7X4B1N0R6S": "The build is fixed now; rerun clippy and fix what is left."}}
```

The engine checks every id before anything runs, and refuses the
whole call when one id is wrong: it must name a swarm child of this
session, so a plain `agent` child, a stranger's session or a typo is
rejected up front. A child still hosted gets the follow-up as its
next run. A child that only exists on disk anymore (it delivered its
result, the session was resumed, or kage restarted) is reopened from
its file with its history, model and agent definition intact, and
keeps appending to the same file.

### forking children

By default every child starts with zero context, which is why each
expanded prompt must be self-contained. `fork: true` trades that
away: each new child starts from a snapshot of this conversation,
so it can see the plan, the paths and the mistakes already made
without them being restated:

```json
{"description": "review each crate with full context",
 "prompt_template": "Review {{item}} for API mistakes.",
 "items": ["kage-core", "kage-loop"],
 "fork": true}
```

The snapshot is the parent's session file forked into the child's
own file up to the latest complete message, so the child's
transcript is self-contained from the first entry and keeps
appending there. A notice after the copied history tells the child
that the conversation is inherited reference material, not its own
past, and that the next message carries its task. Its model, system
prompt and tools still come from the agent definition; only the
conversation carries over. The copy also counts against the child's
token budget, and forked children resume like any other child.
`fork` cannot be combined with `resume`, since resume continues
children that already exist. Prefer zero-context children when the
task is self-contained: a fork per item multiplies the prompt cost
by the item count.

### the /swarm command

`/swarm on` turns swarm mode on: the next run opens with a block that
steers the model to split the work early and delegate it through
`swarm` calls, and `/swarm off` turns it off again. Both notes land
in the history once, so the model sees the switch. `/swarm <task>` is a one-shot: it turns
the mode on, submits the task as a prompt, and turns the mode off
when that run ends. It refuses to run while a run is already in
flight.

The mode survives restarts: every toggle is recorded in the session
file, so resuming the session restores it without injecting the block
a second time. While it is on, the statusline shows a `swarm`
segment next to the permission mode; while background shell commands
run, an `N bg` segment names their count.

## the send_message mailbox

Every agent-enabled session, the parent and its children alike, gets
a `send_message` tool. It drops a message into another running
session's inbox and returns at once, so the main session can steer a
background agent, and a child can report a finding or ask the parent
a question without blocking on an answer:

```json
{"to": "01K62W8Q3T9V5M2C7X4B1N0R6S", "message": "Also run the doc tests, then reply with both results."}
```

`to` is `parent` or the session id of another session of the same
conversation, such as an agent's id from its `started` result or a
sibling. A session under another main session refuses the message.

An agent that has finished resumes to read it. kage reopens it from its
session file with its history, definition and model, and it runs as a
background agent: its new result arrives in its parent's inbox as an
`<agent>` message, and stopping the parent's run leaves it running. A
parent that has closed cannot take the result, so the message is
refused. An agent that worked in a worktree cannot resume, since its
checkout is gone.

A running target reads the message at its next turn boundary, wrapped
so it knows who sent it:

```text
<message from="kage" session="01K62W7ZB1D6XKQ5H8M3T2V9CE">
Also run the doc tests, then reply with both results.
</message>
```

`from` is `kage` for a main session, else the sending agent's name. A
target waiting for a slot reads it when it starts. An idle main
session reads it the way it reads a background result. An agent whose
run ends before it read a message runs again to read it, and only
then reports, so the sender's words reach its result.

Delivery is fire-and-forget: the call returns once the message is in
the inbox, never with the answer. An agent you started covers it in
its own result. When you need an answer in hand before continuing,
make an `agent` call or a `swarm` resume instead.

An agent that delivered its result is dropped from memory, and its
transcript stays in its session file, and a `swarm` resume reopens a
swarm child from that file with its history intact, so finished
agents cost no memory.

## built-in agents

| Agent | Tools | For |
| --- | --- | --- |
| `general` | every tool of the parent | a self-contained task that needs many tool calls. It can read, edit and run commands. |
| `explore` | `read`, `grep`, `find`, `ls`, `web_fetch`, `web_search` | searching and reading files and web pages to answer a question. It cannot change anything. |

Both reply with exactly what the task asked for, since the reply is
all the caller sees. A file of the same name in your config directory
or a trusted project replaces a built-in.

## writing an agent

An agent definition is a markdown file with frontmatter. kage loads
every `*.md` file directly under these directories:

| Directory | Scope |
| --- | --- |
| `~/.config/kage/agents/` | your agents, in every project |
| `<workdir>/.kage/agents/` | the project's agents, once you trust the project (see [project agents and trust](#project-agents-and-trust)) |

The file stem is the agent's name. It uses lowercase letters, digits
and single hyphens, at most 64 characters, like a skill name. The
built-ins load first, then your files, then the project's files, and a
later definition replaces an earlier one of the same name.

```markdown
---
description: Reviews a diff for bugs and missing tests. Give it the change to review.
tools: read, grep, find, ls, shell
model: anthropic/claude-sonnet-4-6
thinking: high
---
You review code changes. Read the files involved, run the tests that
cover them, and reply with a list of concrete problems, most severe
first. Do not edit files.
```

Saved as `~/.config/kage/agents/reviewer.md`, this defines the agent
`reviewer`. The model reads every agent's name and description in the
`agent` tool's description and picks one by it.

| Key | Required | Value |
| --- | --- | --- |
| `description` | yes | What the agent is for, at most 1024 characters. The model picks agents by it, so say when to use this one and what to give it. |
| `tools` | no | A comma list of tool names. Without it the agent gets every tool its parent has. With it, the agent may start agents, swarms or send messages only when the list names `agent`, `swarm` or `send_message`. A listed name that matches no tool of the parent shows a warning when the agent starts. |
| `model` | no | The model, as `provider/model`, or `inherit` (the default) for the parent's current model. A model that is not available fails the agent's run. An `agent` or `swarm` call's `model` overrides this. |
| `thinking` | no | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`, or `inherit` (the default) for the parent's current level. The level is fitted to the agent's model like the main session's. A call's `thinking` overrides this. |
| `max_turns` | no | Turns with tool calls one run may take, a positive integer. `maxTurns` works too. Without it, `agent_max_turns` applies (see [limits](#limits)). |
| `timeout` | no | How long one run may take: seconds (`90`), or a number with `s`, `m` or `h` (`90s`, `10m`, `1h`). Without it, `agent_timeout` applies. |
| `isolation` | no | `none` (the default) or `worktree`, which gives the agent a checkout of its own (see [worktree agents](#worktree-agents)). |
| `name` | no | Must equal the file stem when present. |

The frontmatter takes one `key: value` per line. A value may be
wrapped in double quotes, and lines starting with `#` are comments.
Unknown keys are ignored, so agent files written for other tools load,
and their extra keys do nothing.

The body is the agent's role text. The agent's system prompt is the
body plus an environment block with the working directory, the OS,
the shell, the date and the model. Skills are not listed there.

The `agent` tool itself is not a name for `tools`. An agent can start
agents of its own whenever the depth limit allows it (see
[limits](#limits)).

A file that fails to load, such as one without a `description` or
with a malformed `model`, is skipped. The TUI shows the error as a
block at startup. Print mode and `kage rpc` print it on stderr.

## worktree agents

An agent whose definition says `isolation: worktree` works in a
checkout of its own, so it can edit and build without touching yours:

```markdown
---
description: Implements one change in its own checkout. Give it the whole change to make.
isolation: worktree
max_turns: 60
timeout: 20m
---
You implement the change you are given in your own checkout ...
```

At each start of such an agent, kage makes the checkout under
`~/.local/state/kage/worktrees/<agent session id>`:

- In a jj repository, a jj workspace named `kage-<id>` whose working
  copy starts on top of yours, so it sees your uncommitted changes.
- Otherwise in a git repository, a git worktree on a new branch
  `kage/agent-<id>` from `HEAD`. It starts without your uncommitted
  changes.
- Outside a repository the agent does not start, and the call returns
  `worktree isolation needs a git or jj repository`.

The agent works in the same subdirectory of the checkout that its
parent works in, with paths confined to it. A `shell` command can
still `cd` elsewhere. At the end of each run kage records the work,
committing it on the branch for git, and the agent's reply ends with
where it is and how to take it:

```text
Worktree: jj change qzmtwvpn in workspace kage-6s0r1xkq (2 files changed, 48 insertions(+), 3 deletions(-)). To take it: jj squash --from qzmtwvpn. Nothing was merged.
```

Nothing is ever merged for you. Once the agent is done, kage removes
the checkout: jj forgets the workspace, and git removes the worktree
and deletes the branch when it holds no commits. The change or the
branch with the agent's work stays in the repository until you take
or drop it.

## project agents and trust

A project agent can pick its tools and its model, so project agents
only load once you trust the project. They share one trust decision
with the risky tables of the project's `.kage/config.toml` (see
[project config and trust](/guide/config#project-config-and-trust)).

When the TUI starts in a project with untrusted agents, the prompt
lists each one, such as `project agent reviewer
(.kage/agents/reviewer.md)`, next to any risky config settings.
Answering yes trusts all of them. Any other answer starts kage without
the project's agents and without those settings. Print mode and
`kage rpc` print one warning that names the skipped agents. Run
`kage trust` in the project directory to allow them.

Trust records the full text of every project agent file. Adding,
removing or editing one makes the project untrusted again, and kage
asks on the next start.

## what an agent gets

| Aspect | The agent gets |
| --- | --- |
| Model | the call's `model`, else the definition's, else `agent_model`, else the parent's current model |
| Thinking | the call's `thinking`, else the definition's, else `agent_thinking`, else the parent's current level |
| Tools | the parent's tools narrowed by `tools`, including plugin and MCP tools, plus `agent` while the depth limit allows it |
| Permissions | the parent's rules, permission mode and session approvals, shared live (see [permissions](/guide/permissions#agents)) |
| Asking | it asks you when the parent can ask. Print mode refuses its asks, like the main session's. |
| Working directory | the parent's, or its own checkout for a [worktree agent](#worktree-agents) |
| History | only the task prompt |
| Plugins | plugin tools only. Plugins get no events from agent runs, and their hooks do not run there. |
| Session file | a file next to the parent's when the parent is recorded |

An agent never gets more permission than its parent. It uses the same
permission gate, so `/permission deny` in the main session makes the
agents read-only too, and approving a tool "for the rest of this session"
covers every agent of the session.

## limits

| Option | TOML key | Range | Default | What it limits |
| --- | --- | --- | --- | --- |
| `agent_max_depth` | `agents.max_depth` | 0 to 3 | `1` | How deep agents nest. `0` removes the `agent` tool, and `1` lets only the main session start agents. |
| `agent_max_running` | `agents.max_running` | 1 to 16 | `4` | How many agents run at once. Further agents wait in a queue and start in order as others finish. |
| `swarm_max_items` | `agents.swarm_max_items` | 2 to 128 | `32` | Most members one `swarm` call may run: items plus resumed children together. |
| `swarm_timeout_ms` | `agents.swarm_timeout_ms` | 1,000 to 86,400,000 | `7,200,000` | Milliseconds one swarm child may run, measured from its run start (not while queued). On deadline the child is cancelled and renders as cancelled in the aggregate. |
| `agent_model` | `agents.model` | `provider/model` or `inherit` | `inherit` | Model agents run when neither the call nor the definition names one. `inherit` uses the parent's current model. |
| `agent_thinking` | `agents.thinking` | a thinking level or `inherit` | `inherit` | Thinking level agents run with when neither the call nor the definition names one. `inherit` uses the parent's current level. |
| `agent_max_turns` | `agents.max_turns` | 0 to 10,000 | `0` | Turns with tool calls one agent run may take. `0` means no limit. |
| `agent_timeout` | `agents.timeout` | 0 to 86,400 | `0` | Seconds one agent run may take. `0` means no limit. |
| `agent_budget` | `agents.budget` | 0 to 1,000,000,000 | `0` | Tokens (input plus output) all agents of a session may use between two of your prompts. `0` means no limit. |

Set them in `config.toml`:

```toml
[agents]
max_depth = 2
max_running = 8
```

or in `init.lua`:

```lua
kage.opt.agent_max_depth = 2
kage.opt.agent_max_running = 8
```

Both apply when kage starts. `init.lua` only applies to the TUI.
Print mode and `kage rpc` read the `[agents]` table of your config
files. With `agent_max_depth = 0` the model sees neither the `agent`
nor the `swarm` tool. The swarm limits in the table above apply the
same way, and are described in [swarms](#swarms).

A definition's `max_turns` and `timeout` take the place of
`agent_max_turns` and `agent_timeout` for that agent.

- **Turn limit.** When a turn with tool calls reaches the limit, the
  agent is told `Turn limit reached. Do not call more tools. Reply now
  with what you have and what is left.` and gets one more turn. If it
  calls tools again, the run ends there without running them.
  Either way its result says `limit="turns"`.
- **Timeout.** A run past its time is stopped like `x` stops it, its
  own agents included, and its result reads `state="cancelled"
  limit="time"` with the partial reply.
- **Budget.** Every agent turn counts its input and output tokens
  toward the session's budget. The main session's own turns never
  count. The agent run that crosses it stops every agent of the
  session with `limit="budget"`, the main session shows `agents
  stopped: they used the agent budget of N tokens since your last
  prompt`, and no agent starts until your next prompt resets the
  count. Results and wake-ups do not reset it.

An agent that waits for its own agents does not count against
`agent_max_running`, so nested agents cannot stall the queue. A
result over 20,000 characters is cut, as described in
[how it works](#how-it-works).

## agents in the TUI

::: tip mockups
The screens on this page are ASCII: `.` stands for the middle dot
between facts, `/` for the spinner, `*` for done, `o` for stopped and
`!` for an agent waiting for approval.
:::

### cards

Each `agent` call is a tool row whose body shows the agent live: its
latest tool, described like a tool row, and a line with its tool count
and tokens. A queued agent reads `queued`, and an agent that asks for
approval reads `Waiting for approval` with what it asks for.

```text
 / Agent explore: map exports under src/components                                              41s
     Searched "export " in src/components
     14 tools . 22k tok
 / Agent general: check the router tests                                                        12s
     Running cargo test -p router
     3 tools . 4k tok
```

A finished card shows the head of the agent's reply and its end state
on the right: `done`, `stopped` or `failed`, or the limit that ended
it: `turn limit`, `timed out` or `over budget`. The wrapper the model
reads is hidden. `ctrl+o` unfolds the full reply, like any tool row.
A background agent's card ends at once with `background`.

When a background agent's result reaches the main session, it shows
as a report block, not as a prompt of yours, folded to the first line
of the reply:

```text
 Agent general finished . done . 4m 40s
   412 passed, 2 failed: provider::stream::retries, tui::view::tests::toast_width
   ... 14 more lines
```

A message from another session shows as one row in the target's
view, such as `< message from kage: Also run the doc tests`.

```text
 * Agent explore: map exports under src/components                                       done . 52s
     {"Button.tsx": ["Button", "ButtonProps"], "Modal.tsx": ["Modal"], ...}
     ... 9 more lines
 o Agent explore: find dead code                                                      stopped . 12s
     Stopped by you. Partial reply: two unused helpers in src/util.ts so far
```

### the working row and the pinned list

While agents run, the working row counts the ones the run waits for,
such as `Waiting for 3 agents (41s, esc to interrupt)`. Background
agents do not count there, since nothing waits for them. Below it, a pinned
list keeps every queued, running or waiting agent in view after its
card scrolls away, in the order of their cards. Agents started by
agents sit indented under their parent. The list shows at most four
rows, then `+N more . ctrl+t for agents`, and it hides while the
approval panel is open. A background agent's row tags it `bg`, and the
row stays after the main run ends.

```text
  Waiting for 3 agents (41s, esc to interrupt)
  / explore  map exports under src/components            Searched "export " in src/components . 41s
  / explore  map exports under src/routes                            Read src/routes/index.ts . 18s
  / general  check the router tests                              Running cargo test -p router . 12s
```

While agents are queued or running and the prompt is empty, the
footer names the key that opens the agents overlay, `ctrl+t for
agents` by default, in place of `tab to queue`, also while the main
session is idle. Quitting then says how many agents it stops, such as
`ctrl+c again to quit and stop 2 agents`. When every agent has
finished, the pinned list gives way to one summary row, such as
`2 agents · ctrl+t for agents`, and a click on it opens the list.

### opening an agent

Click a pinned row, or pick an agent in the agents overlay, to open
it. The whole view then shows that agent: its transcript, its working
row, its pinned agents and the input. The header shows a breadcrumb
with the path to the agent, its task, state, time, tokens and tool
count:

```text
 kage > explore: map exports under src/components                running . 41s . 22k tok . 14 tools
```

The first message in the transcript is the task the parent wrote. The
footer's model and context figures are the agent's own, and its tokens
and cost count the agent with every agent under it. In the main view
the same totals cover the whole conversation.

| Key | In an agent view |
| --- | --- |
| `enter` | While the agent runs, steer it at its next turn boundary. |
| `tab` | Queue the prompt until the agent's run ends |
| `esc` | On an empty prompt, go back one level: to the parent agent, or to the main view. It never stops the agent. |
| `ctrl+c` | On an empty prompt, stop the agent while it runs, else go back |

The footer shows the keys that apply, such as `enter to steer . esc to
go back . ctrl+c to stop`.

Only a running agent takes input. Once an agent finishes, live or
restored from a resumed session, its view is read-only: the input
goes away and you can read the transcript and go back. Its result
already reached the parent, so a reply to a later message, and any
change that message made, would never reach the main conversation.
Ask the main session instead, or start a new agent. Going back
restores the main view at the scroll position you left.
Quitting with `ctrl+c` twice only works from the main view.

### the agents overlay

`ctrl+t` or `/agents` opens a list of every agent of the session,
live or finished, as a tree under the main session. The top border
counts the agents per state and totals their tokens and cost.

```text
+- agents -------------------------------------- 3 running . 1 waiting . 2 done . 71k tok . $0.21 -+
|   kage        fix the router and map the exports                      running    4m 02s  14k tok |
| > / explore   map exports under src/components   Searched "export "                 41s  22k tok |
|   / explore   map exports under src/routes       Read src/routes/i..                18s   9k tok |
|   ! general   refactor the provider crate        waiting for approval            2m 10s  31k tok |
|     / test    run the provider tests             Ran cargo test                      4s   3k tok |
|   * general   check the router tests             done                            1m 05s  12k tok |
|   o explore   find dead code                     stopped                            12s   2k tok |
+- enter to open . x to stop . X to stop all . esc to close ---------------------------------------+
```

| Key | Effect |
| --- | --- |
| `up` / `down`, `k` / `j` | Move the selection |
| `home` / `end` | Jump to the first / last row |
| `enter` | Open the selected agent. On the `kage` row, return to the main view. |
| `x` | Stop the selected agent and the agents under it. Only live agents stop. |
| `X` | Stop every live agent of the session |
| `esc` | Close the overlay |

As over any overlay, `ctrl+c` interrupts the run of the session on
screen and leaves the overlay open. With no run in flight, `ctrl+c`
closes it. With no agent in the session yet, `ctrl+t` shows `no agents
in this session yet`. The overlay also opens over an approval panel,
and keys it does not use still answer the panel.

### approvals from agents

An agent's approval request joins the one approval queue, wherever you
are looking. The panel's title puts the agent's name before the
question, and option 5 reads
`No, and tell general what to do instead`: your text goes to that
agent, not to the main session.

### stopping agents

- `esc` or `ctrl+c` on an empty prompt in the main view interrupts the
  main run, which stops every agent under it except background
  agents.
- `X` in the agents overlay stops every live agent.
- `ctrl+c` in an agent view, or `x` in the agents overlay, stops that
  agent and the agents under it. Its parent keeps running and reads
  the partial reply as a cancelled result.
- Stopping a queued agent ends it before it starts.

Starting a new session, resuming another one or cloning the session
waits until no agent runs. Until then kage refuses with a warning
such as `new session: stop or wait for the agents first`. Afterwards the agents overlay starts empty.

## sessions on disk

When the main session is recorded, each agent is recorded too, in its
own file next to the main session's in `~/.local/share/kage/sessions/`.
The file's header names the parent session, its first entry records
the agent and the task, and its title is the task description.

Agent sessions stay out of `kage list`, the session picker and the
start card's recent sessions. `/tree` shows them under their parent
with an `agent: ` label, and resuming one there continues that agent
as the main session. The parent's own file records each `agent` call
and its result, which names the agent's session id.

## print mode

`kage -p` runs agents like the TUI does, with the limits from the
`[agents]` table. Print mode cannot ask, so an agent's approval
request is refused the same way the main session's is. Text output
shows only the main session's replies and tool calls. Notices from
any session go to stderr. An `agent` call prints
`[agent explore: map exports]` when it starts and
`[agent explore completed]` when it ends. A cancelled agent prints
`cancelled`, and a failed one prints `failed` followed by its error.
A limit follows the state, such as
`[agent general completed: turn limit]`.

`kage -p --json` prints every envelope, the agents' included. Each
envelope names its session, and an agent's first envelope is
`agent_spawned`:

```json
{"session":"01K62W8Q3T9V5M2C7X4B1N0R6S","seq":1,"type":"agent_spawned","parent":"01K62W7ZB1D6XKQ5H8M3T2V9CE","tool_call_id":"toolu_01","agent":"explore","description":"map exports under src/components"}
```

`parent` is the session whose `agent` call started this one, and
`tool_call_id` is that call. A background agent's envelope also
carries `"background": true`. The agent's later envelopes carry its own
session id, so a reader can build the tree from these events.

## editors over ACP

`kage rpc` gives editor sessions the `agent` tool as well. How the
editor shows an agent depends on the editor.

An editor that advertises the ACP `subagents` capability sees each
agent as its own child session:

- The parent session gets a `subagent_update` naming the child's
  session, the agent and its task.
- The agent's messages, tool calls and approval requests arrive on
  its own session.
- A final `subagent_update` marks it `completed`, `failed` or
  `cancelled`, and the editor can cancel a running agent.

Any other editor sees agents through the top-level `agent` call in
its own session:

- The `agent` call's content shows the agent's progress, one line at a
  time, such as `explore: Read src/lib.rs`, then
  `explore: done`, `explore: stopped` or `explore: failed`.
- An agent's approval request arrives as `session/request_permission`
  on the editor's session. The tool call is the top-level `agent`
  call, its title names the agent and the tool, such as
  `explore: shell`, and `rawInput` is the agent's tool input.

In both cases `session/cancel` on the editor's session stops the run
and every agent under it except background agents. See
[zed](/editors/zed#agents) for the wire details.

A prompt that starts a background agent ends with the main run and
does not wait for the agent. The agent's `subagent_update` carries
`background: true`, and its final `subagent_update` arrives whenever
it ends. Its result waits for your next prompt, whose run reads it
first. An editor without the `subagents` capability sees the cost of
the session's agents in the session's own `usage_update`. The kage
desktop and browser clients identify themselves to `kage rpc`, and
their idle sessions start a run to read a result, as the TUI does.

ACP asks for every tool without a config entry, and `agent` is no
exception, so the editor approves the start of each agent unless your
config allows it (see [permissions](/guide/permissions#agents)).
Editor sessions are recorded like TUI sessions, and so are their
agents. Agent sessions stay out of the editor's session list.
