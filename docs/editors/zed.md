# zed

`kage rpc` is a spec-conformant **Agent Client Protocol** agent:
JSON-RPC 2.0 over stdio, one message per line (newline-delimited),
protocol version 1. Zed speaks ACP natively, so it drives kage the
same way it drives its other external agents, with no shim or wrapper.

## start the agent

```sh
kage rpc
```

Optional flags:

- `-m, --model <provider/model>` pins the model for the connection.
- `--system <text>` overrides the system-prompt role.
- `--system-file <path>` appends the contents of a file to that role.
  Use it for a prompt too long or too private to spell out on the
  command line, which `/proc/<pid>/cmdline` publishes to every user on
  the host.
- `--yolo` opens every session in allow mode: every tool call runs
  without asking, while configured denies still deny. Meant for
  unattended clients that cannot answer a permission request.

Credentials resolve as for the TUI and print mode (OS keyring,
`kage auth login`, or an API-key env var). With no provider
configured `kage rpc` prints a message and exits non-zero.

## connect over the network

The same protocol is reachable over WebSocket with `kage serve`, so a
Zed instance on another machine can drive your kage. Start the server,
then point the client at the connect URL it prints:

```sh
kage serve                       # prints ws://127.0.0.1:7433/acp?token=...
ssh -N -L 7433:127.0.0.1:7433 you@kage-host   # from the client machine
```

The client opens `ws://127.0.0.1:7433/acp?token=...` through the
tunnel. The token rides the URL, the `Authorization` header, or a
`Sec-WebSocket-Protocol` entry. Sessions attach with full replay, and
several clients can watch one session. See [remote](/editors/remote)
for the token forms, TLS, and the security notes.

## configure zed

Add kage as an agent server in Zed's `settings.json`:

```json
{
  "agent_servers": {
    "kage": {
      "command": "kage",
      "args": ["rpc"]
    }
  }
}
```

Use an absolute `command` if `kage` is not on Zed's `PATH`. Pass a
model with `"args": ["rpc", "-m", "anthropic/claude-sonnet-4-6"]`.
Pick kage from Zed's agent panel and prompt as usual. Tool calls
surface as Zed permission prompts (kage never auto-approves), and the
agent's text and reasoning stream in as it works.

## what the editor gets

- **History.** Editor sessions are recorded like TUI sessions, so the
  agent panel can list earlier kage threads for the project, newest
  first, with their titles. Agent sessions are not listed.
- **Loading.** Opening a thread replays the whole transcript: your
  messages and images, the agent's text and thinking, and each tool
  call with its result, then the title. The thread keeps its model,
  thinking level and token totals. A client that keeps its own copy
  can resume instead and gets no replay.
- **Usage and title.** After each turn the editor gets the tokens in
  context against the model's window, and the cost so far when the
  model has pricing. The title updates when kage generates one.
- **Selectors.** Model, Thinking and Mode drop-downs change the model,
  the thinking level and the permission mode from the next turn on.
- **Commands.** Typing `/` lists the prompts of your MCP servers, such
  as `everything:complex_prompt`.
- **Context.** Images and files attached in the editor reach the
  model. See [prompt content](#prompt-content).
- **Editor MCP servers.** MCP servers configured in the editor run for
  the session. See [mcp servers from the editor](#mcp-servers-from-the-editor).
- **Agents.** In a client that supports subagents, each agent is its
  own child thread. Other clients show agents on the `agent` tool
  call. See [agents](#agents).
- **Permission prompts.** Each prompt offers "Allow for this session",
  and a prompt kage stops waiting for is withdrawn.

## what kage implements

Client to agent:

| method                      | params -> result |
| --------------------------- | ---------------- |
| `initialize`                | `{protocolVersion, clientCapabilities}` -> `{protocolVersion, agentCapabilities, agentInfo, authMethods}` |
| `session/new`               | `{cwd, mcpServers}` -> `{sessionId, configOptions}` |
| `session/load`              | `{sessionId, cwd, mcpServers}` -> replays history as `session/update`, then `{configOptions}` |
| `session/resume`            | `{sessionId, cwd, mcpServers}` -> `{configOptions}`, no replay |
| `session/list`              | `{cwd?, cursor?}` -> `{sessions: [{sessionId, cwd, title?, updatedAt?}], nextCursor?}` |
| `session/close`             | `{sessionId}` -> `{}`, releasing the caller's attachment; a session no connection holds and that is idle closes once released |
| `session/set_config_option` | `{sessionId, configId, value}` -> `{configOptions}` |
| `session/prompt`            | `{sessionId, prompt: ContentBlock[]}` -> `{stopReason}` |
| `session/cancel`            | notification `{sessionId}` |

Agent to client:

- `session/update` notification `{sessionId, update}` where `update`
  is tagged by `sessionUpdate`: `user_message_chunk` (replay only),
  `agent_message_chunk`, `agent_thought_chunk`, `tool_call`,
  `tool_call_update`, `usage_update`, `session_info_update`,
  `config_option_update`, `available_commands_update`, and
  `subagent_update` for clients that support subagents.
- `session/request_permission` request `{sessionId, toolCall,
  options}` -> `{outcome}`. The client returns
  `{outcome: {outcome: "selected", optionId}}` or
  `{outcome: {outcome: "cancelled"}}`. kage blocks the tool until the
  client answers and never auto-approves.
- `$/cancel_request` notification `{requestId}` when kage withdraws a
  permission request it no longer waits for.

kage advertises:

- `loadSession: true`, and `sessionCapabilities` with `list`,
  `resume` and `close`.
- `steer: true`: a `session/prompt` may set `delivery: "steer"` to
  join a run already in flight at its next turn boundary; a prompt
  without the field, or with `"queue"`, runs once the current run
  ends.
- `promptCapabilities` with `image` and `embeddedContext`.
- `mcpCapabilities` with `http: true` and `sse: false`.
- empty `authMethods`, and `agentInfo {name: "kage", version}`.

It does not request `fs` or `terminal` client capabilities: kage runs
its own tools in-process and gates them through
`session/request_permission`.

## sessions

`session/list` returns the sessions recorded in kage's sessions
directory, 50 per page, newest activity first. With `cwd` it returns
only the sessions recorded in that directory. Agent sessions are left
out. `nextCursor` is present while more pages follow.

`session/load` accepts a full session id or a unique prefix of one. It
replays the history, sends a `session_info_update` with the stored
title, and answers with the config options. The session opens on its
recorded model when that model is still available (else the default
model, with a note on stderr), with its thinking level and token
totals. `session/resume` opens the session the same way without the
replay.

After each turn kage sends `usage_update` with `used` (tokens in
context), `size` (the model's context window) and, when the model has
pricing, `cost` in `USD`. Nothing is sent while the window is unknown.
A generated title arrives as `session_info_update`.

## config options

Every session has five options, four selects and one text, returned
by `session/new`, `session/load`, `session/resume` and
`session/set_config_option`:

| id         | category        | kind   | values |
| ---------- | --------------- | ------ | ------ |
| `model`    | `model`         | select | the models the TUI model picker lists, plus the current one |
| `thinking` | `thought_level` | select | `default`, named `auto` (high, or the nearest level the model accepts), then the levels the model accepts from `off`, `minimal`, `low`, `medium`, `high`, `xhigh` |
| `mode`     | `mode`          | select | `default` (the configured rules decide), `ask`, `allow`, `deny`, and `plan` chained in after them |
| `swarm`    | `mode`          | select | `off` (the session does the work itself) or `on` (repeated work is delegated to swarm batches) |
| `goal`     | none            | text   | free-form text; empty clears the goal |

A change applies from the next turn on. `session/set_config_option`
answers with the options as they will be once it applies. When a
value changes by other means, such as a loaded session's model, kage
sends `config_option_update`.

The `mode` values override the permission rules for the session:
`ask` asks before every tool call, `deny` refuses every call, and
`allow` runs every call without asking. Picking `plan` puts the
session in plan mode, which rides above the permission mode: the
option reads back `plan` until a regular mode is picked, and picking
one leaves plan mode in the same change. `allow` is the same override
the TUI's `/permission allow` sets, and `default` matches
`/permission default`.

## prompt content

Each block of a `session/prompt` reaches the model:

| block | what the model gets |
| ----- | ------------------- |
| `text` | the text |
| `image` | the image |
| `resource` with text | the text in a `<resource uri="..." mime="...">` block; text wins when a resource carries both text and a blob |
| `resource` with an image blob | the image |
| `resource` with another blob | `[binary resource <uri>: <mime>]`, with `application/octet-stream` when the resource names no MIME type |
| `resource` with no text or blob | `[resource omitted]` |
| `resource_link` to a `file://` URI | `Referenced file: <path>` |
| other `resource_link` | `Referenced resource: <uri> (<name>)` |
| `audio` | `[audio omitted]` |
| a block type this kage does not know | `[unsupported block omitted]` |

Prompt text can mention MCP resources and run MCP prompts exactly as
in the TUI. See [mcp](/guide/mcp#resources-and-mentions). A mention or
prompt that fails to expand fails the `session/prompt` request with
the reason, and nothing is sent to the model. A prompt command that
misses a required argument, or a mention with a `{...}` placeholder
left, is an invalid params error (`-32602`). Other expansion failures
are internal errors (`-32603`).

## mcp prompts as commands

When a session opens, and whenever its MCP servers change, kage sends
`available_commands_update` with one command per prompt of a
connected server:

```json
{"name": "everything:complex_prompt", "description": "A prompt with arguments", "input": {"hint": "<temperature> [style]"}}
```

`<name>` marks a required argument and `[name]` an optional one. A
prompt without arguments has no `input`. The editor sends the command
back as prompt text, such as `/everything:complex_prompt 0.7 terse`,
and kage expands it.

Updates for a session never arrive before the `session/new`,
`session/load` or `session/resume` response that names it. kage holds
them until that response is written.

## mcp servers from the editor

The `mcpServers` of `session/new`, `session/load` and `session/resume`
start for that session, next to your configured and plugin servers:

- stdio entries `{name, command, args, env: [{name, value}]}`.
- HTTP entries `{type: "http", name, url, headers: [{name, value}]}`.
- `sse` entries are refused with an invalid params error, because
  kage has no SSE transport. Use `http` instead.

An editor server replaces a configured server of the same name, and
stderr says so. Editor servers count as your own configuration, so no
trust prompt applies. Their tools ask before running like every MCP
tool, unless `[permissions.mcp]` allows the server. An editor server
that fails to start is reported on stderr, without the `kage mcp
login` hint, because `kage mcp login` only knows configured servers.

Tool calls are titled like the TUI shows them: `server.tool` for an
MCP tool (`github.create_issue` for `github__create_issue`), else the
tool name. A call is announced once as `tool_call`. Streamed input
sends a `tool_call_update` only when it changed.

## permission prompts

Every `session/request_permission` offers three options:

| optionId        | kind           | effect |
| --------------- | -------------- | ------ |
| `allow`         | `allow_once`   | run this call |
| `allow_session` | `allow_always` | run this call and stop asking for this tool for the rest of the session |
| `reject`        | `reject_once`  | refuse the call |

"Allow for this session" is never saved to a config file. It covers
the agents of the session too, and it never lifts a configured `deny`
or the `deny` mode.

When a run ends while a permission request is still open, for example
because it was cancelled, kage sends `$/cancel_request` for that
request so the editor can close the dialog.

## questions

The model asks [questions](/guide/questions) with `ask_user_question`.
Each question arrives as its own `session/request_permission`, one
after another:

- The tool call is the `ask_user_question` call, titled with the
  question's header and the question, such as
  `Store: Where should sessions live?`.
- The options are `choice-0`, `choice-1` and so on, one per choice
  with its label as the name and kind `allow_once`, then `skip` with
  kind `reject_once`.
- `_meta.kage.question.prompt` carries the whole question:
  `header`, `question`, `options` (`label`, `description`) and
  `multiSelect`.

Picking a `choice-<n>` option answers with that choice. A client may
send `_meta.kage.question.answer`, a list of strings, with its
response to answer with several choices or the user's own words. It
takes the place of the option picked. Picking `skip`, or cancelling,
declines that question and every question after it.

## agents

Editor sessions can start [agents](/guide/agents). How the editor sees
them depends on whether it advertises the `subagents` client
capability (the draft ACP subagents RFD). Any value other than `false`
counts.

**With subagents.** Each agent is its own child session:

- A `subagent_update` on the parent's session announces the child with
  `subagentSessionId`, `name` (the agent definition), `task` (the
  description) and `capabilities: {cancel: true}`. A child without a
  state is running.
- The child's messages, tool calls, usage and permission requests
  arrive on its own session id, like any session.
- When the child ends, a `subagent_update` on the parent sets `state`
  to `completed`, `failed` or `cancelled`. It comes after every update
  of the child, and every foreground child ends before its parent's
  `session/prompt` answers.
- A [background agent](/guide/agents#background-agents) is announced
  with `background: true`. The parent's `session/prompt` answers when
  the main run ends, and the child's final `subagent_update` arrives
  whenever it ends, also between prompts. Its result waits for your
  next prompt, whose run reads it first.
- `session/cancel` with the child's id stops that agent and its own
  agents. Clients cannot prompt a child.
- The parent's `agent` tool call is still announced and completed.

**Without subagents.** Agents are not ACP sessions, so they show up on
the top-level `agent` tool call of your session:

- `tool_call_update` notifications replace that call's content with
  the agent's latest step, such as `explore: Read src/lib.rs`, and end
  with `explore: done`, `explore: stopped` or `explore: failed`. While
  an agent waits for approval the step reads `explore: Waiting for
  approval: ...`, and once the call is allowed it shows the running
  call again.
- An agent's `session/request_permission` arrives on your session
  with the `agent` call as its tool call, a title that names the agent
  and the tool, such as `explore: shell`, and the agent's tool input as
  `rawInput`.

Either way, `session/cancel` on your session stops the turn and every
agent under it except background agents. Without subagents, the
session's `usage_update` cost includes what its agents spent. Every tool without a config rule asks over ACP, so Zed
asks before each agent starts and before each of its tool calls,
unless your `[permissions]` allow them. Limits come from the
`[agents]` table of your config files.

## hand-driven smoke test

```sh
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}' \
  | kage rpc
```

Two framed JSON-RPC results come back, one per line: the agent's
capabilities, then a fresh `sessionId` with its `configOptions`.
Session updates such as `available_commands_update` follow the
`session/new` result. When stdin closes, kage still answers every
request it has read except `session/prompt` before it exits.

The `cwd` of `session/new`, `session/load` and `session/resume` is a
plain absolute path in the host's spelling: no `~` expansion and no
`file://` form. An empty `cwd` means the server's own working
directory, the one the `initialize` result names in
`_meta.kage.cwd`.
