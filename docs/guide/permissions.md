# permissions

By default kage runs every built-in tool call without asking. The
`[permissions]` table lets you opt specific tools into rules instead. MCP tools are the exception:
they ask unless you allow their server (see
[MCP tools](#mcp-tools)).

```toml
[permissions]
# path confinement for the built-in file tools (read, write, edit,
# grep, find, ls). paths that escape the working directory through
# `..`, absolute paths outside it, or symlinks that point outside it
# are rejected. on by default; set false to turn it off.
# confine_paths = false

[permissions.tools.shell]
# what happens when neither the deny nor the allow list matches.
# "allow" (the default), "ask", or "deny".
default = "ask"
# glob patterns. a match runs the call without prompting.
allow = ["git *", "cargo *"]
# glob patterns. a match refuses the call outright.
deny = ["rm -rf *"]

[permissions.mcp]
# action for the tools of one MCP server: "allow", "ask", or "deny".
# servers not listed here fall back to the "*" wildcard, then ask.
github = "allow"
# "*" = "allow"
```

A `[permissions]` table in a project's `.kage/config.toml` applies only
once you trust the project (see
[project config and trust](/guide/config#project-config-and-trust)).

## how rules evaluate

Tool names are literal: `shell`, `write`, `edit`, `web_fetch`,
`agent`, or any registered MCP tool name such as `github__create_issue`. A built-in
tool with no `[permissions.tools.<name>]` entry is always allowed.

When an entry exists, the `deny` patterns are checked first, then the
`allow` patterns, then `default`. First match wins. Within a list,
patterns are tried in written order.

Patterns match against the tool's command line: the `command` string
for shell-style tools, or the compact JSON encoding of the whole input
for everything else. The globs are the standard `globset` syntax
(`*`, `?`, `[...]`).

An entry with only a `deny` list keeps everything else allowed, so you
can block a few dangerous calls without opting into prompts:

```toml
[permissions.tools.shell]
deny = ["curl *", "wget *"]
```

## mcp tools

An MCP tool is named `<server>__<tool>`. Its verdict is decided in
this order:

1. a `[permissions.tools.<server>__<tool>]` entry, evaluated as above;
2. otherwise the server's action under `[permissions.mcp]`;
3. otherwise the `"*"` wildcard under `[permissions.mcp]`;
4. otherwise `ask`.

So an unconfigured MCP tool always asks, while a per-tool entry can
tighten or loosen one tool of an allowed or denied server:

```toml
[permissions.mcp]
github = "allow"

[permissions.tools.github__create_issue]
default = "ask"
```

The wildcard yolo's every server at once; an explicit server entry
beats it, so `"*" = "allow"` with `shell = "deny"` allows everything
but the shell server:

```toml
[permissions.mcp]
"*" = "allow"
shell = "deny"
```

This matters most in print mode, which cannot ask: MCP tools there are
refused until you allow the server, the tool, or all of them through
the wildcard.

The ask default covers every configured server, whenever its tools
appear: at startup, after `/mcp restart`, after an OAuth login, or
when a server that failed to start comes up later. Servers an editor
passes over ACP are covered the same way, and `[permissions.mcp]`
applies to them by name.

`mcp_resource`, the tool that lets the model list and read MCP
resources, is not named `<server>__<tool>`. It follows the built-in
rules: it runs without asking in the TUI and print mode, and asks over
ACP like every tool without a rule. Restrict it like any built-in:

```toml
[permissions.tools.mcp_resource]
default = "ask"
```

Resource mentions and MCP prompt commands you type never ask. They
are part of your prompt, not tool calls.

## what "ask" does per mode

| mode | ask behavior |
|---|---|
| TUI | an approval panel replaces the input box. See [approving in the TUI](#approving-in-the-tui). |
| print (`kage -p`) | the call is denied with an error telling you to add an allow rule. There is no interactive prompt. For an MCP tool the error names both the `[permissions.mcp]` and the per-tool fix. |
| ACP (`kage rpc`) | the editor client is asked through `session/request_permission`, with the options allow, allow for this session, and reject. Every tool without a config entry asks here, built-ins included, unless the client asks for the TUI's rules (the kage desktop app does): then tools without an entry run, as they do in the TUI. A config `allow` skips the round-trip, and a config `deny` refuses locally. When the last client holding the session disconnects while a request is open, the request is denied, so the run goes on. |
| MCP server (`kage mcp serve`) | the call is refused, since there is no one to ask. |

`ask_user_question` never asks for approval in any mode, since the
question itself goes to you (see [questions](/guide/questions)). Only a
config `deny` refuses it.

Over ACP, "Allow for this session" works like the TUI's session
scope below: the tool stops asking until the session closes, for the
session and its agents, and nothing is written to disk. The editor's
Mode selector sets the session's permission mode (see
[zed](/editors/zed#config-options)).

## approving in the TUI

The panel's title names the action, such as `Run this command?`,
`Edit src/lib.rs?` or `Allow github.create_issue?`. Below it, a shell
call shows its command, an edit shows its diff, a write shows the path
and the first lines, and any other tool shows its arguments as
`key value` rows. Five options follow, naming the tool, with `Yes`
selected:

| Option | Keys | Effect |
|---|---|---|
| Yes | `1`, `y` | Run this call. The next call of the tool asks again. |
| Yes, and allow `<tool>` for the rest of this session | `2`, `s` | Run this call and stop asking for this tool for the rest of the session. Nothing is written to disk. |
| Yes, and always allow `<tool>` (saved to config.toml) | `3`, `a` | Run this call, stop asking for this tool for the rest of the session, and save the rule described below. |
| No | `4`, `n`, `esc` | Refuse the call. The model sees `denied by user`. |
| No, and tell kage what to do instead | `5`, `t` | Open a one-line field. `enter` refuses the call and sends your text to the model with the denial. `esc` goes back to the options. |

`up`, `down` and `enter` pick an option too, and the footer lists
`y/s/a/n/t or 1-5`, `enter` and `esc no`. Keys pressed in the first 400 ms
after the panel opens are dropped, so typing meant for the prompt
cannot answer it. When several calls wait, the title shows `1 of 3`,
and the panels come one after another. `ctrl+c` interrupts the run,
which refuses the call, and `ctrl+t` opens the agents overlay so you
can look before you answer. The tool's row in the conversation reads
`waiting` until you answer, then runs or reads `denied`.

For an MCP tool the panel says so and the grants widen to the whole
server: option 2 allows every tool of `github` until the session ends,
and option 3 saves `[permissions.mcp] github = "allow"` instead of a
per-tool rule. The server scope is the point: per-tool prompts are
exactly the noise an approval is meant to end.

Both allow scopes cover the tool (or server) by name, every call, for
the rest of the session. `/new`, a session opened from the picker and a
clone start without them. They are checked before `/permission ask`,
so an approved tool stops asking even in ask mode, and `/permission
default` keeps them. They never lift a refusal:
`/permission deny` and the tool's `deny` patterns in `[permissions]`
still apply.

"Always allow" also persists `[permissions.tools.<name>] default =
"allow"` into your user config file (`~/.config/kage/config.toml`),
editing only that key and keeping the rest of the file and its
comments. Project `.kage/config.toml` layers are never baked in. After a restart the saved rule applies like any other, so
the tool's `deny` patterns count again.

## agents

[Agents](/guide/agents) use their parent's permission gate, so an
agent never has more permission than the session that started it:

- The same `[permissions]` rules apply to an agent's tool calls.
- The permission mode is shared. `/permission ask` or `/permission
  deny` covers every agent of the session.
- Approvals are shared. "Yes, and allow the tool for the rest of this
  session" covers the main session and every agent under it, and an
  approval given in one agent applies to the others.
- An agent asks you when its parent can ask. Its request joins the one
  approval queue, and the panel's title starts with the agent's name.
  Print mode refuses an agent's ask, as it refuses the main session's.

`agent` is a built-in tool, so starting an agent never asks unless you
add a rule. Its rule matches the compact JSON of the call's input,
which holds `agent`, `description` and `prompt`:

```toml
[permissions.tools.agent]
default = "ask"
allow = ['*"agent":"explore"*']
```

With this rule `explore` starts without asking, and every other agent
asks first with `Start agent <name>?`. A call without an `agent` field
starts `general`.

Over ACP every tool without a rule asks, so the editor approves the
start of each agent, and each tool call of the agent, unless your
config allows them.

Project agents can set their own tools and model, so they only load
once you trust the project. See
[project config and trust](/guide/config#project-config-and-trust).

## path confinement

Confined paths (the default) route the built-in file tools through
escape-checked resolution: a read or write must stay under the working
directory. Set `confine_paths = false` to turn it off.
`shell` is unaffected, because a shell can always reach the whole
filesystem. Confine it with `deny` rules instead. Agents inherit the setting.

## runtime mode (`/permission`)

Switch modes without touching the config file. In the TUI:

    /permission allow   # every call runs this session: the yolo switch
    /permission ask     # every tool call prompts this session
    /permission deny    # read-only: reads, searches and fetches run, the rest is refused
    /permission default # back to the configured rules (allow-all
                        # unless you configured [permissions])

`allow` overrides the configured rules for the session, MCP servers
included, so nothing prompts; only the tools' `deny` patterns and
`/permission deny` still refuse. `/perm` is short for
`/permission`. The `:` command line takes the same command. With no
argument, `/permission` shows the current mode, such as
`permission mode: ask`. An active override shows as `allow mode`, `ask mode` or
`read-only mode` in the footer, and as `ask mode for this session` on the
start card while the conversation is empty.

The override lives for the current session only and is never written
to the config file. `/new`, a session opened from the picker and a
clone start with the configured rules again. It short-circuits the
per-tool rules, except a configured deny still denies.
While `deny` is active the session is read-only: `read`, `grep`,
`find`, `ls`, `web_fetch` and other read or fetch tools keep their
configured verdict, and everything else, even allow-listed tools and
tools approved in the panel, is refused. While `ask` is active, even never-configured tools
prompt, except the tools you approved for the session or always.

## plan mode

[Plan mode](/guide/plan-mode) layers its own rules on top: `write`,
`edit`, `generate_image`, `edit_image` and `generate_speech` are
refused, and commands (`shell`, agents, MCP tools, and tools of
unknown risk) always ask, even in `allow` mode and even when approved
for the session. Configured denies and `/permission deny` still deny.

## yolo (`--yolo`)

`kage --yolo` starts the session in allow mode, the same effect as
`/permission allow` at startup: every tool call runs without asking,
MCP servers included, while configured `deny` patterns still refuse.
With print mode, `kage -p --yolo "..."` lifts the usual
non-interactive refusal, so asks run instead of failing. `kage rpc
--yolo` opens every ACP session in allow mode, for clients nobody
watches. A client can still change the mode afterwards. `--yolo` is
never written to the config file and does not survive the session.

## validation

Broken configuration refuses to start: empty tool or server names,
empty patterns, or patterns that do not compile print an error and
exit 1.
