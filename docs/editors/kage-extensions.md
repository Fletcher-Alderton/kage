# kage extensions

`kage rpc` and `kage serve` speak the Agent Client Protocol, plus a
few kage-only methods and session updates under the `_kage/` prefix.
An ACP client that knows nothing of them still works; a client that
does gets the file tree, forks, the model catalog and the rest the
kage desktop app uses. Field names are camelCase on the wire.

## initialize

The `initialize` result carries `_meta.kage.cwd`: the directory a
session opened with an empty `cwd` runs in. A client with no
directory of its own, such as a browser, names its sessions' project
by it. `_meta.kage.configOptions` lists the config options a new
session opens with, so a client can show the default model, thinking
level and mode before it opens one.

A client may send `_meta.kage.unconfiguredTools: "allow"` in its
`clientCapabilities` to get the TUI's permission rules: tools without
a config rule run instead of asking. A client that cannot answer any
permission request starts `kage rpc --yolo` instead, which opens every
session in allow mode. Advertising `subagents` (any
value but `false`) turns on `subagent_update` and child sessions.

## requests

| Method | Params | Result |
| --- | --- | --- |
| `_kage/config/get` | `sessionId?` | The configuration snapshot, read for the session's directory, with secret values redacted. Without a known session, the server's own directory. `acp` holds `[acp.agents]` with environment values redacted. `installedPlugins` lists the plugins, scripts and packages, as `{ name, enabled, version?, fromRegistry }`, where `enabled` says whether `[plugins] disabled` (and the deprecated `enabled` allowlist) let one load, `version` is a package's own, and `fromRegistry` says it was installed from the plugin registry. `providerKeys` maps each provider id kage registers or the config defines to `{ env, source }`: the variable its key is read from, and whether the key is in it (`env`), in the credential store (`auth`), `missing` or not needed (`unneeded`). |
| `_kage/config/set` | `sessionId?`, `path`, `value` | The snapshot after replacing the user `config.toml` entry at `path` (such as `["mcp", "servers", "github"]`) with `value`, in the snapshot's shape, or removing it when `value` is `null`. `path` starts with `providers`, `mcp`, `permissions`, `plugins` or `acp`, or is the two-key `ui.theme_dark` or `ui.theme_light`. The edited file must load and pass kage's startup checks first, and its comments survive; a refused edit is an error and writes nothing. A redacted string in `value` keeps the value on file. An edited `[providers]` or `[acp]` section applies at once; the others apply to sessions opened after. |
| `_kage/config/test` | one of `provider`, `mcp`, `acp` | `{ ok, status?, message, millis, models, tools?, agent? }`, worked out by the engine. `provider: { id, kind?, baseUrl?, apiKeyEnv?, apiKey?, headers? }` lists the provider's models, as saved or as a form holds it; omitted fields fall back to the saved entry, then to the provider's defaults, and a typed `apiKey` is used for this request only. Each model is `{ id, name?, context?, maxOutput? }`, with gaps filled from the model catalog. `mcp: { name, server }` connects to the server (`server` in the snapshot's shape) and lists its `tools` as `{ name, description }`. `acp: { name, command, args, env }` starts the agent, runs `initialize` and names it in `agent`. A redacted value keeps the one saved under the name. An unreachable or refusing endpoint answers `ok: false` with the reason. |
| `_kage/providers/directory` | `url?`, `apiKey?` | `{ providers }`: the providers of a directory in the models.dev `api.json` shape (models.dev itself without `url`), fetched by the engine and kept ten minutes. Each is `{ id, name, api?, env, kind?, models }`, `kind` being the wire protocol kage speaks to it when it knows one (`openai`, `anthropic` or `gemini`), and each model `{ id, name, context?, output?, reasoning, input, cost? }`. Only models that call tools are listed. |
| `_kage/plugins/install` | `source`, `name?`, `replace?` | `{ name }`: installs one `.lua` file from an `https://` URL or a path on the engine's machine into the plugin directory, after checking it compiles. It loads with the next session. |
| `_kage/plugins/remove` | `name` | `{}`: removes an installed plugin, script or package, and its `kura.lock` entry. |
| `_kage/image/save` | `sessionId`, `sha256`, `path?` | `{}`: asks the engine to write the image or audio clip the model or a tool produced in the session whose bytes hash to `sha256` (lowercase hex) into the session's directory: at `path`, or a new file under `generated/`. An existing file is never replaced. Where it went, or why it could not, arrives as a `_kage/notice` on the session. |
| `_kage/registry/search` | `query?`, `refresh?` | `{ registry, stale?, packages }`: the plugin registry's packages whose name, description or tags hold every word of `query` (all without one), read from the index the engine caches for an hour (`refresh` fetches it again). Each is `{ name, description, tags?, homepage?, version?, unavailable?, capabilities, installed? }`: `version` is the release an install picks, `unavailable` why none fits the engine, `capabilities` that release's `{ name, summary }`, and `installed` the version installed from the registry. `stale` says why a cached index stood in for an unreachable registry. |
| `_kage/registry/install` | `name`, `version?`, `force?` | `{ name, version, previous?, capabilities }`: installs the package's newest release that fits the engine (or `version`), after checking every file against the index. `force` replaces a script or local package of the same name, or discards local edits. It grants nothing: `capabilities` lists what the package may ask for. |
| `_kage/auth/set` | `provider`, `key` | `{}`: saves the API key in the credential store (`auth.json`, never `config.toml`), or removes it when `key` is `null`, and reloads the providers. |
| `_kage/options/list` | `sessionId?` | `{ options }`: every engine option a client can change, with its `toml` key, `kind` (`bool`, `int`, `fraction`, `choice`, `str`, `key`), bounds or `values`, `default`, `value`, `configured` and `live`. |
| `_kage/options/set` | `name`, `value` | `{ options }` after the write, validated and stored in the user `config.toml` with its comments kept. A refused value is an error. |
| `_kage/models/list` | none | `{ providers }`: each provider with credentials, `{ id, name, models }`, and each model `{ id, name, context?, inputCost?, outputCost?, thinking, images, released? }`. `id` is `provider/model`, the value the `model` config option takes; costs are USD per million tokens; `thinking` lists the `thinking` option values the model accepts. |
| `_kage/fs` | `sessionId`, `op` (`list` or `read`), `path` | `list`: `{ entries, truncated }`, a capped subtree of the session's workdir that skips `.git` and gitignored paths; continue a truncated listing by listing a subdirectory. `read`: `{ content, truncated, binary }`, capped at 512 KB. Paths are relative to the workdir. |
| `_kage/folders` | `path?` | `{ path, parent?, home?, folders, truncated, skipped }`: the folders inside the folder `path` names, or the home folder when it is missing or empty (`~`, `~/...` and `~\\...` join onto home). Hidden folders are left out, the rest sorts case-insensitively, and the list is capped at 2000 names (`truncated` says when). `skipped` counts unreadable entries and non-UTF-8 names. A folder that cannot be read is an error. |
| `_kage/themes` | none | `{ themes, skipped? }`: one entry per `.toml` file in the themes folder, sorted by name; each is `{ name, base?, gui }` with `name` the file name without `.toml`, `base` the bundled theme it starts from, and `gui` its `[gui]` palette (token to color). Files that cannot be read or parsed are left out and named in `skipped` as `"<file name>: <reason>"`. |
| `_kage/session/fork` | `sessionId`, `before?` | `{ sessionId }` of a recorded copy, whole or up to the prompt `before` names (`{ text, occurrence }`), which `session/load` opens. |
| `_kage/session/export` | `sessionId` | `{ markdown }`: the transcript as Markdown. |
| `_kage/session/compact` | `sessionId` | `{}`: summarizes older turns now. |
| `_kage/session/rename` | `sessionId`, `title` | `{}`: a named session keeps its name over generated titles. |
| `_kage/swarm/resume` | `sessionId`, `members` | `{ resumed }`: continues swarm children (child session id to a follow-up prompt; empty continues the task). It answers once every member is checked and attached, and refuses the whole request when one is not a swarm child of the session or is still working. Members report under the call that first spawned them. Once all have reported, the session gets a notice with the counts and its next turn reads their results. |

## session updates

| `sessionUpdate` | Fields | Meaning |
| --- | --- | --- |
| `_kage/turn` | `phase` (`start`, `end`), `reason?` (`tool_calls`, `no_tool_calls`), `at?`, `tookMs?` | One provider round trip of the running prompt. A loaded session's history closes each run that ended with an `end` carrying when it ended (`at`, Unix seconds) and how long it took. |
| `_kage/notice` | `tone` (`info`, `warn`, `error`, `success`), `text` | A message for the user that is not part of the conversation, such as `goal met: ...`. |
| `_kage/compaction` | `phase` (`started`, `complete`, `aborted`), `kept?`, `before?`, `after?`, `summarized?`, `summary?` | The compaction lifecycle in one update. `started`: the agent began summarizing, naming the turns the summarizer will read (`summarized`) and how many stay verbatim (`kept`). `complete`: older turns were summarized, with context tokens `before` and `after`, the turns the summary replaced and the framed `summary` text; `after` is absent when no turn reported usage after the compaction. `aborted`: the summarizer failed; history is intact and the run continues, with the reason as an error notice. An update without a `phase` is an older engine's `complete`. |
| `_kage/mcp_status` | `name`, `status` (`connected`, `starting`, `needs_auth`, or `{ failed: { error } }`) | One MCP server's reachability changed. |

A loaded session's history also carries the durations the engine
recorded: `_meta.kage.durationMs` on each `agent_thought_chunk` and on
the `tool_call_update` that ends each tool call.

`session/list` entries carry `_meta.kage.parentSessionId` for a
session forked from another, so a client can draw the fork tree.

Media a model produces, such as the images of a model that draws,
arrive as standard ACP `image` (or `audio`) blocks in
`agent_message_chunk`, carrying their bytes as base64, between the text
chunks around them. A loaded session replays them the same way. The
bytes ride the stream rather than a `file://` link, so a client that
does not share the engine's filesystem shows them too. An image whose
bytes the engine no longer holds replays as a text chunk saying so.
Images a tool produced arrive the same way inside the `content` of the
`tool_call_update` that ends the call, after its text.

## sessions an agent started

A subagent's session streams on its own id while it runs. Loading a
finished agent's session with `session/load` shows its transcript but
does not open it in the engine: prompting it is refused, because an
agent belongs to the call that started it. Loading a session rebuilds
`subagent_update` records for the agents its history started, at
every depth, from the results the engine recorded.

A `subagent_update` may also carry `model`, the child's
`provider/model`, and `usage`: `{ input, output, cacheRead,
cacheWrite, cost, runMs? }`, the child's token totals, its cost in
USD and how long it has run. The engine sends them as the child
reports usage and on the update that ends it.
