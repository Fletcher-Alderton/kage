# mcp

kage speaks the [Model Context Protocol](https://modelcontextprotocol.io)
(JSON-RPC 2.0) in both directions:

- **client**: connect to external MCP servers and use their tools,
  resources and prompts alongside the built-ins.
- **server**: `kage mcp serve` exposes a chosen set of kage's built-in
  tools to another MCP-aware agent.

kage asks for protocol revision `2025-06-18` in `initialize` and keeps
working with the revision the server answers with. An HTTP server gets
that negotiated revision in the `MCP-Protocol-Version` header of every
later request.

## using external mcp servers

Declare servers in `config.toml` under `[mcp.servers.<name>]`. Each
server uses exactly one of two transports, chosen by which key is set.

A **stdio** server sets `command`. kage spawns the child and speaks
newline-delimited JSON-RPC over its stdin and stdout:

```toml
[mcp.servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.servers.filesystem.env]
SOME_TOKEN = "..."
```

A **Streamable HTTP** server sets `url`. kage POSTs each JSON-RPC
message to that endpoint and sends `headers` on every request, which
is where a static `Authorization` header goes:

```toml
[mcp.servers.github]
url = "https://example.com/mcp"

[mcp.servers.github.headers]
Authorization = "Bearer ..."
```

A server that uses OAuth needs no header. See
[logging in to remote servers](#logging-in-to-remote-servers).

Setting both `command` and `url`, or neither, is an error. Add
`disabled = true` to keep a server configured without connecting to
it.

To hide some of a server's tools from the model, list them by the
server's own names in `disabled_tools`:

```toml
[mcp.servers.github]
url = "https://api.githubcopilot.com/mcp/"
disabled_tools = ["delete_repository", "merge_pull_request"]
```

Startup has its own deadline, separate from the request timeouts
[below](#progress-and-cancel): `startup_timeout_secs` (optional
integer, default 10) raises how long kage waits for a server's
`initialize` handshake before it gives up. Raise it for a server whose
cold start is slow, such as a first `npx` download.

Servers in your user config start right away. Servers in a
project's `.kage/config.toml` start only once you trust that project:
the TUI asks at startup, and print mode, `kage rpc` and
`kage mcp serve` ignore them with a warning until you run `kage trust`
in the project directory. The same applies to a project's
`[mcp] allow_sampling`, its `oauth` tables and `[permissions.mcp]`. See
[project config and trust](/guide/config#project-config-and-trust).

Each enabled server is connected, handshaked, and its tools,
resources and prompts are listed. In the TUI this happens after the
UI is up, off the launch path: `/mcp` lists a server as `starting`
until it turns `connected` or `failed`. A stdio server's `stderr` is
captured, never drawn over your terminal; a server that crashes
quotes its last lines in the crash notice. A server that fails to
start or to list its tools is reported (inline in the TUI, on stderr
in `-p` and `kage rpc`) and the rest still load. kage never swallows
the failure.

kage advertises the working directory to every server as its single
filesystem root (`roots/list`).

When kage exits normally it kills every stdio child it spawned. If
kage itself is killed or crashes, the children are not cleaned up and
may keep running.

### sampling

A server may ask the host to run an LLM completion
(`sampling/createMessage`). This is off by default because it spends
your token budget on the server's behalf. Opt in with:

```toml
[mcp]
allow_sampling = true
```

The request runs against your default model.

### tool names

A server's tools are namespaced `<server>__<tool>` (for example
`filesystem__read_file`) so a server can never shadow a built-in.

When a server announces that its tools, resources or prompts changed
(`notifications/*/list_changed`), kage lists them again before the
next run. Changed tools are swapped in place, and tools the server no
longer offers are removed, not left dangling.

### progress and cancel

Every tool call carries a progress token. When the server sends
`notifications/progress`, the tool's row shows the server's message,
or `progress/total` when there is no message. An editor sees the same
text as a `tool_call_update`.

When kage stops waiting for a request, it sends the server
`notifications/cancelled` with the request id, so the server can stop
the work too. That happens when you cancel the turn during a tool
call, and when a request passes its deadline. Tool calls have no
deadline. Listing, reading a resource and fetching a prompt give up
after 15 seconds, and `initialize` after 10 seconds. `initialize` is
never cancelled, because MCP forbids it.

### images in results

An `image` item in a tool's result stays an image: it shows with the
call in the TUI, the desktop and editors, `/image save` can write it
into the project, and a model that reads images in tool results
(Claude) sees it. Other models read a note naming it. Other binary
items are named in the result text.

### failed servers

kage keeps every configured server, even one that failed to start, so
it can show the failure and restart the server later:

- In the TUI the servers start after the UI is up, so a server that has
  not connected yet is listed as `starting` until it turns `connected`
  or `failed`.
- A server that fails to start is listed as `failed` with its error.
- A server whose process exits or whose connection drops is noticed
  before the next run: its tools are removed, a notice says
  ``server `name` crashed: ...`` with the server's last stderr lines
  quoted, and it is listed as `failed`.
- An HTTP server that refuses kage's token, or has none, is listed as
  `needs login`. Its error names the fix:
  ``server `linear` needs authorization: run kage mcp login linear, or /mcp in the TUI``.
  A server with a configured `Authorization` header gets no such hint,
  because a login would not change what kage sends.
- A connected server turns into `needs login` before the next run when
  it refused any request, a tool call included, or when its stored
  token is gone after `kage mcp logout`. A restart that the server
  refuses also takes the running server down.

Restart a failed server with [`/mcp restart`](#the-mcp-picker). Log
in to a server that needs it with `/mcp login` or `kage mcp login`.

## resources and `@` mentions

Servers that list resources can have them attached to a prompt. Type
`@` in the TUI: the completion popup lists files, plus every
server that lists resources or resource templates as `<server>:`,
tagged `mcp server`. Pick a server, or type `@<server>:`, and the popup
lists that server's resources, matched against their URI and name,
followed by its resource templates tagged `template: <name>`. A
template is inserted as written with the cursor on its first `{...}`
part, so type over it. A mention that still has one is refused before
any request,
for example `mcp fix: test://item/{id}: fill in {id} first`.

```text
> what is in @everything:test://static/resource/1
```

A file picked from the popup is only a path in your text, such as
`@src/main.rs`. The model reads the file with its tools if it needs
to. A server mention is different.

When you send the prompt, kage reads every mentioned resource with `resources/read`,
once per distinct URI, and appends it to the same user message. The
text you typed stays as it is. The transcript shows one line per
resource:

```text
 > what is in @everything:test://static/resource/1
   attached everything:test://static/resource/1 (4 KB)
```

The model receives the text inside a resource block, which is also
what the session file records:

```text
<resource uri="test://static/resource/1" server="everything" mime="text/plain">
...contents...
</resource>
```

The rules:

- A mention is `@<server>:<uri>` at the start of the prompt or after
  whitespace. The URI runs to the next whitespace, and one trailing
  `.`, `,` or `;` is dropped.
- Only names of configured servers count. Other `@name:...` text is
  left alone and sent as typed.
- A mention of a configured server that is not connected fails the
  prompt with `mcp <server>: failed: <error>` or `mcp <server>: needs
  login`. A resource that cannot be read fails it with
  `mcp <server>: read <uri>: <reason>`. The notice shows in the
  transcript and nothing is sent to the model.
- Image resources are attached as images. Other binary resources
  become an empty resource block with their URI, MIME type and size.
  The transcript shows every attachment as `attached <server>:<uri>`
  with its size, or its MIME type for an image.
- Text is capped at 64 KiB per resource and 256 KiB per prompt. A
  truncated resource ends with a line saying how many bytes were kept.

kage keeps at most 500 resources, 100 resource templates and 200
prompts per server. Completion reads that cached list, so typing
never waits on the network.

Mentions are expanded when a run starts. In the TUI a prompt with a
mention therefore always waits for the current run to end, even when
you press `enter` to steer. Print mode and editors expand mentions the
same way: `kage -p "summarize @docs:file:///notes.md"` works.

### the `mcp_resource` tool

While at least one connected server offers resources, the model also
gets a read-only `mcp_resource` tool. Its description names the
servers that have resources.

- With only `server`, it lists that server's cached resources and
  templates, without a request.
- With `server` and `uri`, it reads the resource, with the same caps
  as a mention. Binary parts become one line each. Errors read like a
  mention's, and a URI with a `{...}` placeholder is refused.

The tool goes away when no connected server offers resources. It is
not an MCP tool name, so it follows the built-in rules rather than
the MCP ask default. See [permissions](/guide/permissions#mcp-tools).

## prompts as commands

A server's prompts show up in the TUI as slash commands,
`/<server>:<prompt>`. The argument hint lists required arguments as
`<name>` and optional ones as `[name]`, and the description ends in
`[mcp]`:

```text
/everything:simple_prompt      A prompt without arguments  [mcp]
/everything:complex_prompt     <temperature> [style]  A prompt with arguments  [mcp]
```

Running `/everything:complex_prompt 0.7 terse` fetches the prompt with
`prompts/get`, puts its messages in place of the command, and sends
the result as your prompt.

- Arguments are whitespace separated words, bound in the order the
  server declares them. The last argument takes the rest of the line,
  spaces included.
- A missing required argument fails before anything reaches the
  model: `mcp everything:complex_prompt: missing argument temperature`.
- Text stays text, images stay images, audio stays audio, and
  embedded resources become resource blocks. A resource link becomes one line,
  `Referenced resource: <uri> (<name>)`.
- Messages the prompt gives the assistant role are kept in the same
  user message as text starting with `Assistant: `, because providers
  disagree on which role sequences they accept.
- Only prompts of connected servers are listed. A prompt whose name a
  built-in or plugin command already takes is left out.

The same text works in print mode (`kage -p "/everything:simple_prompt"`)
and from an editor's command menu. Only the start of the first text
block is checked, and only against connected servers and their listed
prompts. Anything else is sent as typed. Like a mention, a prompt
command in the TUI waits for the current run to end.

## the `/mcp` picker

`/mcp` opens a picker with one row per configured server: its name,
its status and a detail.

```text
everything   connected     11 tools, 3 prompts, 100 resources, 2 templates
linear       needs login   enter to log in
booting      starting      waiting for the server
broken       failed        spawn `nope`: No such file or directory
```

A connected row counts the server's tools, then its prompts,
resources and resource templates when it has any. A failed row shows
the first line of its error.

- `enter` on a `needs login` row starts the login.
- `enter` on any other row restarts the server.

With no servers configured, `/mcp` says so instead:
`no MCP servers configured. Add one under [mcp.servers.<name>] in
config.toml, then restart kage.`

`/mcp restart <server>` and `/mcp login <server>` do the same without
the picker.

A restart spawns the server again from its configuration and swaps it
in only once the new process is up, so a restart that fails leaves a
running server in place. A notice reports the restart, such as
``restarted `linear` ``, or its error. When kage is idle the restart
happens at once. During a run it waits for the next run to start, because
tool calls in flight hold the old connection.

## logging in to remote servers

An HTTP server that needs OAuth works without extra config: kage finds
the authorization server from the MCP server's `401` answer and its
metadata, registers itself as a client when the server allows it, and
logs in with PKCE. Configure only what the server needs:

```toml
[mcp.servers.linear]
url = "https://mcp.linear.app/mcp"

[mcp.servers.linear.oauth]
client_id = "kage-4f2c"   # optional: a pre-registered client, skips registration
scope = "read write"      # optional: overrides the scope the server advertises
```

Both keys are optional. Without `client_id`, a login reuses the client
an earlier login registered with the same authorization server. The
first login needs dynamic client registration, or it fails and names
the `client_id` key.

### `kage mcp login`

```sh
kage mcp login linear
```

The login prints the authorization URL, opens it in a browser when one
is available (`open` on macOS, `xdg-open` when `DISPLAY` or
`WAYLAND_DISPLAY` is set), and waits up to 5 minutes:

```text
kage: authorize kage for MCP server `linear` in your browser:

  https://mcp.linear.app/authorize?response_type=code&client_id=kage-4f2c&...

kage: waiting for the browser to return (5 minutes).
kage: on another machine? paste the URL your browser was sent to and press Enter:
>
kage: linear authorized
```

The browser returns to a one-shot listener on
`http://127.0.0.1:<port>/callback`. On a machine without a browser,
such as over SSH, open the URL elsewhere, then paste the address the
browser was sent to (the page will not load) and press Enter.

In the TUI, `/mcp login linear` or `enter` on a `needs login` row runs
the same flow: the TUI steps aside the way `/login` does, and when the
login succeeds it says `mcp linear: logged in, reconnecting` and
restarts the server.

`kage mcp login` reads the servers of the config for the current
directory, so a project's servers need a trusted project. Only HTTP
servers log in. A stdio server is refused.

```sh
kage mcp logout linear
```

`logout` forgets the stored token. The server needs a new login before
it connects again. A running kage notices at the next restart of the
server or before its next run, and lists it as `needs login`.

### where tokens live

Tokens are stored in `$XDG_DATA_HOME/kage/mcp-auth.json`
(`~/.local/share/kage/mcp-auth.json` by default), next to `auth.json`,
and written with mode `0600`. Entries are keyed by the server's
canonical URL, not its name, so a project that reuses a server name
with another URL never receives your token. The file is read on every
request, so a login in another kage process applies without a
restart.

- A configured `Authorization` header wins. kage sends a stored token
  only when the server config has none.
- A token that expires within a minute is refreshed before the request.
  A `401` triggers one refresh and one retry. Concurrent requests that
  raced on the same stale token share the winner's refresh instead of
  rotating once each, which matters with rotating refresh tokens. When
  the refresh fails, the server shows as `needs login`.
- Tokens, codes and verifiers never appear in notices, errors, session
  files or `Debug` output. HTTP redirects never carry the
  `Authorization` header.
- Every authorization server endpoint must use HTTPS, except on
  loopback hosts, and the authorization server must support PKCE with
  S256.

## permissions

Built-in tools run without asking unless you configure rules. MCP
tools are different: a server is opaque, so every call to an MCP tool
**asks by default**. In the TUI a prompt opens, and in an editor over
`kage rpc` the client is asked. Print mode (`kage -p`) cannot prompt,
so the call is refused with a message naming the fix.

This holds for every configured server, however it came up: at
startup, after a restart, after a login, or passed by an editor.

Allow or deny a whole server under `[permissions.mcp]`:

```toml
[permissions.mcp]
github = "allow"
filesystem = "deny"
```

Servers not listed there ask. A `[permissions.tools.<name>]` entry for
one tool wins over the server's action, so you can allow a server and
still gate one of its tools:

```toml
[permissions.mcp]
github = "allow"

[permissions.tools.github__create_issue]
default = "ask"
```

Choosing "Yes, and always allow" in the TUI approval panel writes
such a per-tool entry. If you run MCP servers from `kage -p` scripts, add
`[permissions.mcp]` entries for them, or their tools will be refused.
See [permissions](/guide/permissions) for the full reference.

Mentions and prompt commands need no approval. You typed them, and
they only read from the server.

## what clients see

Each session publishes an `mcp_servers` event when it opens, after a
restart, and when a server's catalog changes. `kage -p --json` prints
it like every envelope:

```json
{"session":"01K62W7ZB1D6XKQ5H8M3T2V9CE","seq":3,"type":"mcp_servers","servers":[{"name":"everything","status":"connected","tools":11,"resources":[{"uri":"test://static/resource/1","name":"Resource 1","mime_type":"text/plain"}],"templates":[],"prompts":[]},{"name":"linear","status":"needs_auth","tools":0,"resources":[],"templates":[],"prompts":[]}]}
```

`status` is `connected`, `failed` (with an `error` field) or
`needs_auth`. The latest snapshot replaces the previous one. It is
never recorded in the session file.

## declaring servers from a plugin

A plugin declares a server and kage spawns it. Naming a command for
the host to spawn is process execution, so `add_server` and `restart`
require the `exec` capability (`[plugins.capabilities]
your-plugin = ["exec"]`, requested at load); `list_servers` needs no
grant. From Lua:

```lua
kage.request_capabilities({ "exec" })
kage.mcp.add_server({
  name = "filesystem",
  command = "npx",
  args = { "-y", "@modelcontextprotocol/server-filesystem", "/tmp" },
  env = { SOME_TOKEN = "..." },
  disabled = false,   -- optional, default false
})

local names = kage.mcp.list_servers()  -- { "filesystem", ... }

kage.mcp.restart("filesystem")          -- respawn a wedged server
```

`name` and `command` are required. A plugin-declared server overrides
a `config.toml` entry of the same name, so a plugin can ship a
working default a user can still replace. `kage.mcp.list_servers()`
returns the plugin-declared servers only.

`kage.mcp.restart(name)` enqueues a restart that the host applies
when the next run starts, like `/mcp restart`. It works for any
configured server, including one that failed to start or crashed. A
name no server has is reported as unknown.

## running kage as an mcp server

Expose kage's built-in tools to another agent over stdio:

```sh
kage mcp serve
```

By default only the read-only tools are served: `read`, `grep`,
`find`, and `ls`. Pick the set with `--tools`, a comma-separated list
of built-in names (`read`, `write`, `edit`, `shell`, `ls`, `find`,
`grep`, `web_fetch`). An unknown name is an error.

```sh
kage mcp serve --tools read,grep,find,ls,edit
```

Every call is also checked against the `[permissions]` rules of the
working directory. A project's rules count only when the project is
trusted. A `deny` verdict refuses the call, and so does
`ask`, because there is no one to ask. Path confinement (on by
default) keeps the file tools inside the working directory.

Point any MCP client's server command at `kage mcp serve`. Tool
failures and refusals come back as a normal result with
`isError: true`, so the calling agent sees the message. Only unknown
JSON-RPC methods are protocol-level errors.
