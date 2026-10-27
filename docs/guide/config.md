# configuration

kage reads its configuration from `~/.config/kage/config.toml`. The file is
optional, and sane defaults work without it. `kage init` writes a
starter one. A config that does not load stops kage with the error
instead of running on defaults, and `kage doctor` names the file to
fix.

The TUI also runs `~/.config/kage/init.lua` after this file, so
anything set there wins. The `[ui]`, `[loop]` and `[agents]` keys
that are options and the `[keybindings]` table feed the same settings `init.lua` sets.
See [lua config](/guide/lua-config).

Keys are grouped into tables:

```toml
[provider]
# model used when -m/--model is absent. it beats the last-used model
# but only while its provider has credentials. otherwise kage uses the
# last model you ran, then the first provider with credentials.
default_model = "anthropic/claude-sonnet-4-6"

[ui]
# bundled theme name, or a user theme under ~/.config/kage/themes/<name>.toml.
theme = "default"
# the themes "default" picks on a dark and on a light terminal background.
# theme_dark = "kage-shadow"
# theme_light = "kage-dawn"
# capture terminal mouse events. Toggle at runtime with /mouse.
mouse = true
# prompt editing style: "modeless" or "vim".
editor = "modeless"
# input box sizing (content rows, not counting its two rules).
# the box grows with what you type, from input_min_lines up to
# input_max_lines, then scrolls internally. raise the max for a
# bigger composing area. min floored at 1, max capped at 64.
input_min_lines = 1
input_max_lines = 8
# default thinking level for new sessions: off, minimal, low,
# medium, high, xhigh, max, or ultra. left unset, kage uses high, or the nearest
# level the model accepts, and shows it as "high (auto)". alt+t
# still cycles it per session.
# thinking_level = "medium"
# what prints to the terminal after you quit the TUI: "full" (the
# whole conversation as plain text), "last" (from your last prompt
# on) or "none". the session file path follows when one was recorded.
transcript_on_exit = "full"

[plugins]
# override the plugin directory (default ~/.config/kage/plugins/).
# `~` expands to home, and relative paths resolve against ~/.config/kage.
# dir = "/path/to/plugins"
# plugins that do not load, by file stem or package directory. a
# project config can add to this list but never shorten it.
# (`enabled`, an allowlist, still works but is deprecated.)
disabled = []
# the plugin registry `kage plugin` installs from (default: kura).
# read from this file only; a project config cannot set it.
# registry = "https://kura.kage.qaidvoid.dev/"

[shell]
# Program the shell tool runs commands with: `bash` (the default on
# Linux and macOS), `pwsh` (the default on Windows), `fish`, `zsh`, or
# an absolute path. The tool description and the prompt's environment
# block name it, so the model writes commands in that shell's syntax.
# User `!` commands run with the same program. Known Windows shells
# (powershell, pwsh, cmd) get their own command flag; every other
# program is driven with `-c`.
# program = "fish"
# Glob patterns of environment variables to hide from shell commands
# (case-sensitive, matched against the whole name, so "*_TOKEN" covers
# GITHUB_TOKEN). Empty leaves the environment untouched.
# scrub_env = ["*_TOKEN", "*_SECRET", "*_KEY", "*_PASSWORD"]
scrub_env = []

[tools.rename]
# advertise a tool under another name: the model sees only the new
# name, while execution and permission rules keep working under the
# real one. keys are real tool names, values are advertised names.
# unknown tools are ignored, and plugin tools can rename themselves
# with `kage.rename_tool({ from = "shell", to = "run_command" })`.
# tool cards in the transcript, the ACP kind hints, and read-only
# grouping keep using the real name.
# shell = "run_command"
# web_fetch = "browse"

[tools.web_search]
# the engine `web_search` asks: "duckduckgo" (the default, no key),
# "searxng", a SearXNG instance at `url` that allows the json format,
# or "brave", the Brave Search API with the key in `api_key_env`
# (default BRAVE_API_KEY).
# engine = "searxng"
# url = "https://searx.example.org"
# engine = "brave"
# api_key_env = "BRAVE_API_KEY"

[tools.generate_image]
# the images endpoint the generate_image tool calls. Without one the
# tool is not offered. See "image generation" below.
# provider = "openai"
# model = "gpt-image-2"

[tools.edit_image]
# the images endpoint the edit_image tool calls. Without one the
# tool is not offered. See "image editing" below.
# provider = "openai"
# model = "gpt-image-2"

[tools.generate_speech]
# the speech endpoint the generate_speech tool calls. Without one the
# tool is not offered. See "speech" below.
# base_url = "https://speech.example.com/v1"
# model = "speech-small"
# voice = "alloy"

[keybindings]
# the key <leader> expands to in bindings: one key, default a backslash.
# leader = "<C-x>"
# ms a key that starts a longer mapping waits for more keys.
timeoutlen = 1000
# key -> command line, or `action:<Name>` for a built-in action
# (see the keybindings guide). Every binding maps in mode g.
# bindings = { "f6" = "theme set kage-dawn", "<leader>s" = "settings" }

[loop]
# compact older history once the prompt fills this fraction of the
# model context window (0.0 to 1.0). Compaction also fires early when
# the remaining room can no longer hold a full reply. Applies to the
# next session, and the /settings dialog edits it too.
compaction_threshold = 0.8

[agents]
# how deep agents may nest (0 to 3). 0 removes the agent tool, and 1
# lets only the main session start agents.
max_depth = 1
# how many agents run at once (1 to 16). further agents wait their turn.
max_running = 4
# most items one swarm call may run, one child agent per item (2 to
# 128). a call listing more fails.
swarm_max_items = 32
# milliseconds one swarm child may run, from its run start (1000 to
# 86400000). on deadline the child is cancelled.
swarm_timeout_ms = 7200000
# model agents run when neither the agent call nor the agent's
# definition names one, as provider/model. "inherit" uses the parent's.
model = "inherit"
# thinking level agents run with when neither the call nor the
# definition names one: inherit, off, minimal, low, medium, high, xhigh,
# max or ultra.
thinking = "inherit"
# turns with tool calls one agent run may take (0 to 10000, 0 for no
# limit). the agent gets one warning turn to reply first.
max_turns = 0
# seconds one agent run may take (0 to 86400, 0 for no limit).
timeout = 0
# tokens all agents of a session may use between two of your prompts
# (0 to 1000000000, 0 for no limit).
budget = 0

[permissions]
# tool permission rules. built-in tools are allowed unless configured
# and MCP tools ask. see the permissions guide for the full reference.
# confine_paths = false
# [permissions.tools.shell]
# default = "ask"
# allow = ["git *"]
# deny = ["rm -rf *"]
# [permissions.mcp]
# github = "allow"   # action for every tool of one MCP server

[mcp]
# let MCP servers ask your default model for completions. default false.
allow_sampling = false
# one table per server. see the mcp guide for the full reference.
# [mcp.servers.linear]
# url = "https://mcp.linear.app/mcp"
# optional OAuth settings for a server that needs a login:
# [mcp.servers.linear.oauth]
# client_id = "kage-4f2c"   # pre-registered client, skips registration
# scope = "read write"      # overrides the scope the server advertises
```

Every table and key is optional. Omitted values fall back to the
defaults shown above. The two `swarm` keys under `[agents]` tune the
[swarm command](/guide/agents#the-swarm-command): `swarm_max_items`
caps how many items one call may run, and `swarm_timeout_ms` caps one
child's runtime in milliseconds.

When kage writes this file itself (saving `/settings`, or answering
"always allow" to a permission prompt), it changes only the keys
involved. Your comments, key order and every other line stay as you
wrote them. See [permissions](/guide/permissions) for the
rules reference, [agents](/guide/agents#limits) for the `[agents]`
limits, and [mcp](/guide/mcp) for MCP servers and OAuth logins.

## layering

Configuration is merged lowest-to-highest precedence:

1. built-in defaults
2. `~/.config/kage/config.toml` (user)
3. `<workdir>/.kage/config.toml` (project-local, commit it to share
   team settings)
4. `KAGE_*` environment variables

Env vars use `KAGE_` with `__` for nesting, e.g.
`KAGE_UI__THEME=kage-shadow` overrides the `[ui].theme` key.

In the TUI, `init.lua` runs after all of these. An option it sets, such
as `kage.opt.theme`, wins over every layer above.

## project config and trust

A project can start processes and loosen your tool rules, so these
parts of it only apply once you trust the project:

- `[mcp]` in `.kage/config.toml` (servers, their `oauth` tables and
  `allow_sampling`)
- `[permissions]` in `.kage/config.toml` (including
  `[permissions.mcp]`)
- `[plugins.capabilities]` in `.kage/config.toml`
- the agent definitions in `.kage/agents/` (see
  [agents](/guide/agents#project-agents-and-trust))

All of them share one trust decision. Every other project key, such
as `[ui]`, `[loop]` or `[agents]`, applies without trust. Provider
settings, `[acp.agents]`, `[tools.generate_image]`,
`[tools.edit_image]`, `[tools.generate_speech]`,
`provider.default_model` and `plugins.dir` are only read from your
user config.

When the TUI starts in a project that sets any of these, it lists what
the project asks for (server commands and URLs, sampling, capability
grants, permission changes, project agents) and asks `Trust this
project? [y/N]`. Answering yes records the trust. Any other answer
starts kage with those tables and agents ignored.

Print mode, `kage rpc` and `kage mcp serve` cannot ask. They print one
warning on stderr and ignore the tables and agents. Run `kage trust`
in the project directory to allow them, and `kage trust --revoke` to
take the trust back. An editor driving `kage rpc` needs `kage trust` once per
project.

Trust covers the values as they are when you approve them. Editing a
server command, a permission rule or a capability grant, or adding,
removing or editing a project agent file, makes kage ask again.
Reordering keys does not. The whole table is ignored while untrusted,
even settings that only tighten your rules, such as a
project that only adds `deny` patterns. `kage doctor` reports an
untrusted project file.

Trusted projects are recorded in `~/.local/state/kage/trust.json`,
keyed by the project directory.

## image generation

The `generate_image` tool lets any model make an image from a prompt,
through an endpoint that speaks the OpenAI Images API
(`POST {base_url}/images/generations`). A model that draws on its own
needs no tool (see [providers](/guide/providers)); this is for the
others. kage offers it only when `[tools.generate_image]` names an
endpoint and a model:

```toml
# Reuse a provider kage already reaches, with its base URL, key and
# headers.
[tools.generate_image]
provider = "openai"
model = "gpt-image-2"
```

Or name the endpoint itself. `base_url` wins when both are set.

```toml
[tools.generate_image]
base_url = "https://images.example.com/v1"
api_key_env = "IMAGES_API_KEY"   # or a key saved for the tool, below
headers = { X-Team = "art" }
model = "image-small"
timeout_secs = 180               # the whole request, 1 to 3600
```

To keep the key out of the environment, save it for the tool with
`kage auth login generate_image` and leave out `api_key_env`. A
variable named in `api_key_env` comes first when it is set, and the
saved key fills in when it is not. With neither, no key is sent.

`provider` names `openai`, an OpenAI-compatible provider, or a
`[providers.custom.<id>]` of kind `openai`. kage's `anthropic` and
`google` providers speak other APIs and cannot be named; a server's
OpenAI-compatible images endpoint can be given as `base_url` instead.
When the table does not resolve (a key that is not set, a provider of
another kind, no model), kage says why on stderr and starts without
the tool.

The image comes back with the call: the TUI, the desktop and editors
show it, it stays in the session, and a model that reads images in
tool results (Claude) sees it. `/image save` writes it into the
project. A call may also name a `path` in the workspace to save it
there at once; an existing file is never replaced, and a path outside
the workspace is refused before the server is asked. A server that
answers with a link instead of the image is followed only when the link
is on the endpoint's own host: kage fetches it without the key and
without following a redirect. A link to any other host is refused.

The tool counts as a write: plan mode and a read-only session refuse
it, as they refuse `write`, and permission rules name it
`generate_image`. Each call costs a generation, so to approve each one
first:

```toml
[permissions.tools.generate_image]
default = "ask"
```

## image editing

The `edit_image` tool changes an image you already have from a
prompt. It takes one PNG, JPEG or WebP image plus the prompt: a file
from the workspace, or, when the call names none, the newest image in
an earlier message, such as one `generate_image` made in a previous
step or one you attached. It sends both to an OpenAI-compatible Images API
(`POST {base_url}/images/edits`) as a multipart form. The edited
image comes back with the call, exactly as a generated one does. kage
offers it only when `[tools.edit_image]` names an endpoint and a
model:

```toml
# Reuse a provider kage already reaches, with its base URL, key and
# headers.
[tools.edit_image]
provider = "openai"
model = "gpt-image-2"
```

Or name the endpoint itself. `base_url` wins when both are set.

```toml
[tools.edit_image]
base_url = "https://images.example.com/v1"
api_key_env = "IMAGES_API_KEY"   # or a key saved for the tool, below
model = "image-edit"
```

The table takes the same keys as `[tools.generate_image]`, and the
rules for what `provider` may name are as for image generation. It is
its own table because an edit server often serves another model name,
and because it has its own permission rule. To keep the key out of
the environment, save it for the tool with
`kage auth login edit_image` and leave out `api_key_env`.

A source file is read only from inside the workspace, even with
`confine_paths = false`, because the file is uploaded. Either source
may be at most 16 MiB. One image goes in per call. There is no mask. A
`path` saves the result and never replaces an existing file, the
source included: the name gets `-2`, `-3` and so on.

The tool counts as a write and uploads a file: plan mode and a
read-only session refuse it, as they refuse `write`, and permission
rules name it `edit_image`. Each call uploads a file and costs an
edit, so to approve each one first:

```toml
[permissions.tools.edit_image]
default = "ask"
```

## speech

The `generate_speech` tool reads a text aloud. It sends the text to an
endpoint that speaks the OpenAI Speech API
(`POST {base_url}/audio/speech`), and an audio clip comes back with
the call. kage sniffs the clip's type from its bytes, so it is MP3 or
WAV whatever the response headers say. kage offers the tool only when
`[tools.generate_speech]` names an endpoint and a model.

```toml
[tools.generate_speech]
base_url = "https://speech.example.com/v1"
api_key_env = "SPEECH_API_KEY"   # or a key saved for the tool, below
model = "speech-small"
voice = "alloy"
```

The table takes the same keys as the image tables, plus `voice`, the
voice asked when a call names none. The example names no provider
because OpenAI lists every model on this route for shutdown on
2027-01-06, so a server that still serves the route is needed. To keep
the key out of the environment, save it for the tool with
`kage auth login generate_speech` and leave out `api_key_env`.

The clip plays where the client plays audio, and it stays in the
session: a loaded session replays it. Editors over ACP get an `audio`
block, the TUI shows a `[audio/mpeg from the tool, <size>]` row, and
the desktop shows the clip with buttons to play it and save it.
In the TUI, `/media open` plays the newest clip with the system's
player and `/media save` writes it into the project, unless an image
came after it. A call writes a file itself only when it names a
`path`, and it never replaces an existing file. When the file cannot
be written, the clip still comes with the call and the result says
why.

The server's limits on text length and voices are its own. When it
refuses a call, its message reaches the model as written.

The tool counts as a write: plan mode and a read-only session refuse
it, as they refuse `write`, and permission rules name it
`generate_speech`. Each call costs a synthesis, so to approve each one
first:

```toml
[permissions.tools.generate_speech]
default = "ask"
```

## environment variables

API keys are read from environment variables:

| Variable                | Provider                                      |
| ----------------------- | --------------------------------------------- |
| `ANTHROPIC_API_KEY`     | Anthropic Claude                              |
| `OPENAI_API_KEY`        | OpenAI                                        |
| `GEMINI_API_KEY`        | Google Gemini                                 |
| `ZAI_API_KEY`           | Z.AI                                          |
| `ZAI_CODING_API_KEY`    | Z.AI and Zhipu AI coding plans                |
| `DEEPSEEK_API_KEY`      | DeepSeek                                      |
| `GROQ_API_KEY`          | Groq                                          |
| `MISTRAL_API_KEY`       | Mistral                                       |
| `CEREBRAS_API_KEY`      | Cerebras                                      |
| `XAI_API_KEY`           | xAI                                           |
| `OPENROUTER_API_KEY`    | OpenRouter                                    |
| `FIREWORKS_API_KEY`     | Fireworks AI                                  |
| `MOONSHOT_API_KEY`      | Moonshot                                      |
| `KIMI_API_KEY`          | Kimi for Coding                               |
| `XIAOMI_API_KEY`        | Xiaomi / Xiaomi Token Plan                    |
| `CMD_API_KEY`           | Command Code                                  |
| `KENARI_API_KEY`        | Kenari                                        |

The model id picks the provider. Without `-m`, kage uses
`provider.default_model` when its provider has credentials, then the
last model you ran, then the preferred model of the first provider
with credentials. When the configured `default_model` has no
credentials, the TUI starts on the fallback and says which model it
used instead.

See [providers](/guide/providers) for the full provider list, custom
endpoints, and per-provider overrides (base URL, headers, key env var).

## directories

| Path                                | Contents                                                       |
| ----------------------------------- | -------------------------------------------------------------- |
| `~/.config/kage/config.toml`        | user config                                                    |
| `~/.config/kage/init.lua`           | trusted Lua config, TUI only ([lua config](/guide/lua-config)) |
| `~/.config/kage/lua/`               | modules `init.lua` can `require`                               |
| `<workdir>/.kage/config.toml`       | project-local config overlay                                   |
| `~/.config/kage/agents/`            | agent definition `.md` files ([agents](/guide/agents))         |
| `<workdir>/.kage/agents/`           | project agent definitions, loaded once the project is trusted  |
| `~/.config/kage/themes/`            | user theme TOML files                                          |
| `~/.config/kage/plugins/`           | Lua plugin scripts                                             |
| `~/.config/kage/skills/`            | `SKILL.md` skill directories                                   |
| `~/.config/kage/templates/`         | prompt template `.md` files                                    |
| `~/.local/share/kage/sessions/`     | append-only session JSONL files                                |
| `~/.local/share/kage/auth.json`     | saved provider credentials (`0600`)                            |
| `~/.local/share/kage/mcp-auth.json` | OAuth tokens for remote MCP servers (`0600`)                   |
| `~/.local/share/kage/plugin-state/` | per-plugin `kage.store` JSON files                             |
| `~/.local/state/kage/`              | session state (`state.json`) and input history (`history.txt`) |
| `~/.local/state/kage/trust.json`    | trusted project configs                                        |
| `~/.cache/kage/models.json`         | model catalog written by `kage models refresh`                 |
| `~/.cache/kage/chatgpt-models.json` | ChatGPT plan models, written at sign-in and by `kage models refresh` |

`XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME` /
`XDG_CACHE_HOME` override the `~/.config`, `~/.local/share`,
`~/.local/state` and `~/.cache` roots. `kage doctor` prints the four
directories it resolved. Skills and
templates are also discovered
under the project-local `<workdir>/.kage/skills/` and
`<workdir>/.kage/templates/`, plus any directory a plugin
contributes via the `resources_discover` event.

## skills and templates

A skill is a directory with a `SKILL.md` (YAML frontmatter `name`,
`description`, optional `disable_model_invocation`) whose body is
injected into the system prompt so the agent always sees it. Each
skill is also a slash command: `/name arguments` sends the body as
your prompt with the arguments appended, so invoking one is a normal
message the model answers with the skill applied. Pass
`disable_model_invocation: true` to keep the `/name` command out;
the body still reaches the system prompt.

A prompt template is a single `.md` file (frontmatter `name`,
`description`, `argument-hint`) whose body becomes a user message
with positional substitution. `$1`, `$2` and so on are single
arguments, `$@` or `$ARGUMENTS` is all of them joined, and `${@:N:L}`
is a bash-style slice. Drop files in the directories above. No Lua is
required.
