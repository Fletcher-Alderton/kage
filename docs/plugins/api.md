# lua api

Every function below is reachable as `kage.<name>` from inside a
plugin script and from `init.lua`. Types are described in
TypeScript-ish notation for readability. Lua is dynamically typed.

Plugins load in file-name order, so `a.lua` always runs before
`b.lua` or the package `b/`. Registrations that replace each other (a header, a block
renderer for the same kind, a key mapping) end with the last file's
value. `config.toml` bindings and `init.lua` load after every plugin.
See [lua config](/guide/lua-config#load-order-and-reload) for the
whole load order.

## api versions

Every function carries the API generation that introduced it. The
current generation is 3. A function is available since API 1 unless
its heading says **since API 2**. API 3 added
[packages](/plugins/packages), whose `plugin.toml` states the
generation they need in `api`. The generated editor stub
`plugins/types/kage.lua` shows the generation of every function as
`Since API N`. A script that needs API 2 can say so at load time:

```lua
kage.requires({ api = 2 })
```

## layers

The API has two layers:

- `kage.api.*` holds the low-level primitives, implemented in Rust.
  All of them are since API 2. See the [reference](#kage-api-reference)
  at the end of this page.
- The stdlib is Lua embedded in kage and loaded before any plugin. It
  builds the friendly functions on top of `kage.api`. The functions
  that existed before `kage.api` keep their names and behavior as
  aliases:

| Stdlib function | Built on |
| --- | --- |
| `kage.on(event, fn)` | `kage.api.autocmd_create`. `fn` gets the payload, and the call returns `off`. |
| `kage.register_keybinding(spec, fn)` | `kage.api.keymap_set` in mode `g`. Returns `off`. |
| `kage.keymap.set(modes, lhs, rhs, opts)` | `kage.api.keymap_set`, once per mode |
| `kage.keymap.del(modes, lhs)` | `kage.api.keymap_del`, once per mode |
| `kage.ui.set_slot(name, spec)` | `kage.api.slot_set` |
| `kage.ui.set_header(fn)`, `kage.ui.set_footer(fn)` | `kage.api.slot_set` with one component that calls `fn(width)` every 500 ms |

## trust tiers

Plugins and `init.lua` share this API but not the same privileges.

| | plugins | `init.lua` and its `lua/` modules |
| --- | --- | --- |
| Capabilities (`session_write`, `exec`, `env`, `net`, `crypto`, `context`, `provider`, `fs_write`) | only when granted in `[plugins.capabilities]` and requested | all, without asking |
| `require` | packages only, confined to their own `lua/` | confined to `~/.config/kage/lua/` |
| the `io` library, `os.execute`, `debug` | removed | removed |
| Loaded by | the TUI, print mode and `kage rpc` | the TUI only |

Each plugin and `init.lua` get their own copy of the `kage` tables. A
plugin that replaces `kage.ui.set_header` changes only its own copy,
and no plugin can reach the functions `init.lua` holds. See
[capabilities](/plugins/capabilities) and
[lua config](/guide/lua-config#trust).

## host

### `kage.now_ms()`

Wall-clock milliseconds since the Unix epoch as an integer.

### `kage.api_version()` / `kage.host_version()`

The plugin API generation as an integer (currently 2), and the kage
version string.

### `kage.requires({ api })`

Raise at load time when the host's API generation is older than `api`,
so a plugin fails with a clear message instead of part-way through a
missing function.

### `kage.log(level: string, message: string)`

Record a structured log line. `level` is one of `"trace"`, `"debug"`,
`"info"`, `"warn"`, `"error"`. An unrecognized level falls back to
`"info"`, unlike `kage.ui.notify`, which raises. In the TUI the line
also shows in the conversation, in order with the turn that logged
it. Lines logged while kage starts show as `kage:log` blocks instead.

### `kage.sleep_ms(ms: integer)`

Sleep for `ms` milliseconds, at most 500 per call. Loop the call to
wait longer. Inside a tool's `execute` or a provider's `stream` handler
the wait lets other plugin work run meanwhile; anywhere else it holds
the Lua thread.

### `kage.json.encode(value)` / `kage.json.decode(raw: string)`

Encode a Lua value as a JSON string, and decode a JSON string into the
equivalent Lua value.

### `kage.config()`

Return a copy of the host-supplied table `{ model, cwd, system_prompt }`.
Changing the copy does not reach the host. `system_prompt` is
conversation text, so it is present only for a plugin granted the
`context` [capability](/plugins/capabilities#context); without the
grant the key is absent.

### `kage.plugin_config()`

Return this plugin's own settings from `[plugins.config.<stem>]` in
`config.toml`, or an empty table. A plugin sees only its own table.

```toml
[plugins.config.branch]
remote = "upstream"
```

### `kage.store`

Private key-value state that survives reloads and restarts, one JSON
file per plugin under `~/.local/share/kage/plugin-state/`.

| Call | Effect |
| --- | --- |
| `kage.store.get(key)` | the saved value, or `nil` |
| `kage.store.set(key, value)` | save any JSON-serializable value |
| `kage.store.delete(key)` | remove a key |
| `kage.store.keys()` | every saved key |

### `kage.request_capabilities(names)` -> `{ name = granted }`

Ask for the elevated APIs (`session_write`, `exec`, `env`, `net`,
`crypto`, `context`, `provider`, `fs_write`) the
user granted this plugin. See [capabilities](/plugins/capabilities).

## ui

### `kage.ui.notify(message: string, level?: string)`

Show a transient toast in the TUI (stderr in print mode). `level` is
`"info"` (default), `"warning"`, or `"error"`. Other levels are also
recorded through the log sink so the severity is not lost. An
unrecognized level raises an error.

`kage.notify(message, level?)` is an alias for the same function.

### blocking dialogs

`kage.ui.select`, `confirm`, `input`, and `editor` look synchronous
but do not block the host. The calling handler's coroutine *suspends*
while the host opens the overlay, and the host resumes it with the
user's answer. Write them like ordinary blocking code:

```lua
local color = kage.ui.select("Pick a color", { "red", "green", "blue" })
if color == nil then return end          -- user cancelled
kage.ui.notify("you picked " .. color)
```

These may be called from any bridged handler: a
[command](#commands) handler or a [keybinding](#keybindings) handler.
Only one dialog can be open at a time per plugin runtime.

### `kage.ui.select(title: string, items)` -> value | nil

Open a fuzzy picker. `items` is an array. Each entry is either a
string (label and value both that string) or a table
`{ label, value?, detail? }` (`value` defaults to `label`). Returns
the chosen entry's `value`, or `nil` if the user cancelled.

### `kage.ui.confirm(title: string, message: string)` -> boolean

Open a yes/no overlay. `y` and `n` answer directly. Returns `true` or
`false`. Cancelling with `esc` or `ctrl+c` counts as `false`, so the
result is always a boolean.

### `kage.ui.input(title: string, placeholder?: string)` -> string | nil

Open a single-line input. Returns the entered string, or `nil` if
cancelled. The placeholder is dimmed help text and is not part of the
result.

### `kage.ui.editor(title: string, prefill?: string)` -> string | nil

Open a multi-line editor seeded with `prefill`. `ctrl+s` saves, and
`esc` or `ctrl+c` cancels. Returns the final buffer, or `nil` if
cancelled.

### `kage.ui.set_header(fn | nil)` / `kage.ui.set_footer(fn | nil)`

Take over the top row (`set_header`) or the bottom row
(`set_footer`). kage calls `fn(width)` every 500 ms and when the
width changes, and paints the first line it returns in place of the
built-in row. Passing `nil` restores the default row. The header row
collapses while `fn` returns nothing. The `:` command line and `/`
search line paint over the footer row, custom or not, while they are
open. Both are shorthands for [`kage.ui.set_slot`](#kage-ui-set-slot-name-spec-nil).

`fn(width)` returns one of: a plain string (one unstyled span), a
span table, or an array of those, one line per element. An element
that is itself an array of spans is a multi-span line. A span table
is `{ text, hl?, fg?, bg?, bold?, dim?, italic?, underline? }`. `hl`
names a [highlight group](/guide/lua-config#highlight-groups) whose
colors and attributes apply first. A color is a group name (its `fg`
or `bg`), a theme role name such as `"muted_fg"` or `"tool_error_fg"`
(any key of a theme's `[colors]` table), or a fixed color (`"red"`,
`"#1f1f28"`, `"42"`). Group and role colors follow the active theme.
An unknown color is ignored and the span keeps the row's default.

A `nil` return, an empty string or a non-conforming value paints an
empty row. An error is logged and the previous output stays on screen.

```lua
kage.ui.set_footer(function(width)
  return { { text = "branch: ", hl = "KageMuted" },
           { text = "main", fg = "green", bold = true } }
end)
```

kage keeps the last output on screen, so the screen never waits on
Lua. Keep it cheap anyway: no blocking dialogs, no network. A render
call gets a much smaller CPU budget than a tool or command. A render
that runs away is aborted within a fraction of a second, logged, and
the previous output stays on screen.

### `kage.ui.set_slot(name, spec | nil)`

**Since API 2.** Fill one of the chrome slots: `header`, `activity`
(the working row above the input), `input_pill` (the input's top
rule), `footer` or `start` (the start card). A row slot takes
`{ left = items, right = items, sep = string? }` and `start` takes
`{ lines = items }`. The `footer` instead takes
`{ rows = { row_spec, ... } }` to stack several status rows, each a
`{ left, right, sep }` table: the region grows to the rows that paint
something, one row minimum and four at most, and the open `:` or `/`
line keeps the top row. An item is a built-in component name (`brand`,
`breadcrumb`, `title`, `model`, `widgets`, `search`, `session`, `working`,
`activity`, `context`, `tokens`, `thinking`, `permission`, `plan`, `swarm`,
`agents`, `tasks`, `mode`,
`hint`, `cwd`, `version`, and in `start` also `mark`, `sessions` and
`notices`), a span table, or a Lua component
`{ render = fn(ctx), events?, interval?, hl? }` whose output kage
keeps and recomputes only when a listed event fires, the interval
(at least 50 ms) passes, the slot is set, `kage.api.redraw` is called
or the width changes. An `events` entry may add a pattern after a
space, such as `"tool_result shell"`. `nil` restores the default spec.
Unknown slots, components and events raise.

```lua
kage.ui.set_slot("header", {
  left = { "brand", "title" },
  right = { "widgets", "search", { events = { "turn_end" }, render = function(ctx)
    return ctx.working and "" or os.date("%H:%M ")
  end } },
})
```

The `header` and `activity` rows collapse while they paint nothing,
so this header stays visible because `brand` always paints. See
[lua config](/guide/lua-config#slots) for the defaults, what each
component shows and the fields of `ctx`.

`breadcrumb` paints only while an [agent](/guide/agents) is on screen:
the path to it, such as `kage > explore`, its task, state, time,
tokens and tool count. `title` paints nothing then. The default header
is `{ left = { "breadcrumb", "title" }, right = { "widgets", "search" } }`,
so a custom header that drops `breadcrumb` shows no trail in an agent
view.

### `kage.register_block_renderer(kind, render | nil)`

Own how a custom conversation block draws. Any custom block whose
`kind` matches (for example a
`kage.session.append_entry("myplugin:card", ...)` entry) is painted
by your `render` instead of the built-in header and body card. Pass
`nil` to remove the renderer.

`render(block)` receives `{ kind, text, folded, width }` and returns
the **same shape** as `set_header`: a string, a span table, or an
array of either (one line each). The host still adds the
conversation's focus rule and spacing, and the plugin owns the
content. An error, a non-conforming value or an empty return paints
a visible `[block renderer ... produced no output]` marker, never a
silent blank.

```lua
kage.register_block_renderer("myplugin:card", function(b)
  return {
    { text = ".----.", fg = "cyan" },
    { { text = "| ", fg = "cyan" },
      { text = b.text, fg = "green", bold = true } },
    { text = "'----'", fg = "cyan" },
  }
end)
```

**Overriding built-in blocks.** Pass one of the reserved kinds
instead of a custom one to re-skin a built-in block type:

| kind          | block                              |
| ------------- | ---------------------------------- |
| `user`        | `{ text }`                         |
| `assistant`   | `{ text, live }`                   |
| `thinking`    | `{ text, folded, live }`           |
| `tool_call`   | `{ name, input_summary, input_pretty, folded }` (unpaired) |
| `tool_result` | `{ name, output, is_error, folded, duration_ms }` (orphan) |
| `custom`      | default `{ kind, text, folded }` fallback for any unhandled custom kind |

Every payload also carries `kind` and `width`. `tool_call` and
`tool_result` overrides only affect *unpaired* tool blocks. A merged
call and result pair spans two blocks and cannot be overridden through
this single-block path.

While your output for a block is still being computed, an override of
a built-in kind paints the built-in widget, so a streaming `assistant`
block does not flicker. A custom kind shows a dim `...` until its
output arrives.

Same retained-output and cost rule as `set_header`. For an
interactive picker, use [`kage.ui.select`](#blocking-dialogs). See
`plugins/examples/block_renderer_demo.lua`.

## tools

### `kage.register_tool(spec)`

Register a new tool. The agent can call it the same way it calls
built-in `read` or `shell`. Spec fields:

```lua
{
  name        = "echo",                       -- string, required
  description = "echo back the input",        -- string, required
  schema      = { type = "object" },          -- json schema, required
  risk        = "read",                       -- "read" | "write" | "exec" | "network"
  parallel    = false,                        -- let a batch of calls run at once
  execute     = function(input) ... end,      -- (table) -> string or table
}
```

`risk` defaults to `"read"` when omitted. Any other value raises an
error and the tool is not registered.

Every plugin shares one Lua thread, and `execute` runs on it as a
coroutine. While it waits in `kage.http.*`, `kage.exec` or
`kage.sleep_ms` it yields, so other tool calls, commands, events and
renders run in between, and calls can overlap. Code that busy-loops
without calling any of these still holds the thread until it returns.
Cancelling a call ends `execute` where it is waiting: it never resumes,
and a request already on the wire, or a process already started, runs
to its end unread. Since calls can overlap, state a handler
keeps between waits may change under it.

With `parallel = true`, a batch made only of calls to such tools runs
at once, as the built-in `web_search` does. Leave it off for a tool
whose calls must not overlap.

`execute` may return a string (the tool output text) or a table:

```lua
return {
  is_error    = false,
  text        = "ok",
  structured  = { count = 7 },
  media       = { { mime = "image/png", data = png_base64 } },
}
```

`media` lists images the tool produced, each with its MIME type and
base64 bytes. They show with the call in every client, stay in the
session, and reach a model that reads images in tool results; other
models read a note naming them.

### `kage.override_tool(spec)`

Same shape as `register_tool` but replaces the existing entry by name.
Useful for filtering `shell` or auditing `write`. The override replaces
the tool, so it must do the work itself. The host logs a warning if no
tool with that name was previously registered.

### `kage.rename_tool(spec)`

**Since API 2.** Take `{ from, to }` and advertise the tool `from`
under the name `to`: the model sees only `to`, while execution and
permission rules keep using the real tool. Empty names raise. The
host applies renames after tools and overrides; a rename whose `from`
names no tool sits unused until one registers.

## commands

### `kage.register_command(spec)`

Register a slash or colon command:

```lua
{
  name        = "branch",
  aliases     = { "br", "git-branch" },  -- optional
  description = "current git branch",
  args        = {
    { name = "remote", kind = "text", optional = true },
  },
  handler     = function(raw, ctx, args)
    return "raw: " .. raw .. ", remote: " .. (args.remote or "origin")
  end,
}
```

The handler receives three arguments:

- `raw`: the text typed after the command name, unparsed.
- `ctx`: reserved for host context. It is currently `nil`.
- `args`: a table keyed by arg name, holding the values parsed from
  `raw` using the declared `args` list. Omitted optional args are
  absent.

The handler may return `nil`, a string (the command output), or a
table `{ text, is_error? }`.

Argument `kind` values: `"text"`, `"choice"`, `"path"`, `"session"`,
`"flag"`. For `"choice"`, also supply `choices = { "...", ... }`. A
`"text"` argument may set `hint`, the placeholder shown in completion.

`aliases` are alternate names that resolve to the same command, so
`:br` runs `:branch`. They appear in completion and `:help`. A
command is rejected whole if its name *or* any alias collides with a
built-in. Use `kage.override_command` to shadow a built-in on
purpose.

The handler runs through the coroutine bridge, so it may call the
blocking [`kage.ui.*`](#ui) dialogs directly.

### `kage.override_command(spec)`

Same `spec` as `register_command`, but the command is **allowed to
shadow a built-in** of the same name and is dispatched ahead of it
(parity with `kage.override_tool`):

```lua
kage.override_command({
  name = "help",
  description = "my help",
  handler = function() return "see :keybindings and :events too" end,
})
```

Now `:help` runs your handler instead of the built-in. Overrides
live in their own registry, so removing the plugin restores the
built-in.

## keybindings

Keys resolve against one keymap table. `_defaults.lua` fills it, then
plugins, `config.toml` and `init.lua` add to it, and the last mapping
set for a key wins. See [lua config](/guide/lua-config#keymaps) for
modes, notation, sequences and the leader.

### `kage.keymap.set(mode, lhs, rhs, opts?)`

**Since API 2.** Map `lhs` in `mode`, one of `n`, `b`, `i`, `v`, `g`,
or a list of them. `rhs` is a `kage.action` value, a `":command"`
string, a function, or `"<Nop>"`. `opts` takes `desc` (shown in the
`?` reference) and `group` (its section there).

```lua
kage.keymap.set("n", "<leader>m", kage.action.OpenModelPicker, { desc = "pick a model" })
kage.keymap.set("g", "<F6>", ":theme set kage-dawn")
kage.keymap.set("i", "<C-l>", function() kage.ui.notify("hi") end)
```

A function rhs runs through the coroutine bridge, so it may open
[`kage.ui.*`](#ui) dialogs. A non-empty string return is shown as a
conversation block, like a command.

### `kage.keymap.del(mode, lhs)`

**Since API 2.** Remove a mapping. Raises when there is none. Keys the
editor grammar handles (vim motions, readline edits, `enter`, `esc`)
are not mappings. Shadow them with `"<Nop>"` instead.

### `kage.action`

**Since API 2.** Built-in actions to use as an rhs, such as
`kage.action.OpenModelPicker` or `kage.action.FoldAll`.
`kage.action.OpenAgents` opens the agents overlay, mapped to `<C-t>`
by default. `kage.action.scroll(n)` scrolls by `n` lines. See
[lua config](/guide/lua-config#rhs) for the full list.

### `kage.register_keybinding(spec, handler)` -> off

Bind a chord to a handler in mode `g`. `spec` is either a chord string
or a table `{ key = "...", description? = "..." }`:

```lua
local off = kage.register_keybinding("ctrl+shift+x", function()
  kage.ui.notify("hello from a chord")
end)

kage.register_keybinding({ key = "f5", description = "reload" }, reload)
```

The call returns an `off` function that removes the mapping while it
is still this one. Calling it again does nothing. A mapping with a
`description` shows in the `?` reference under `plugins`.

Chord grammar (case-insensitive, modifiers in any order):

- modifiers: `ctrl`, `alt`, `shift`, `super` (aliases: `control`,
  `option`/`opt`, `cmd`/`command`/`meta`/`win`)
- key: a single character, a named key (`enter`, `esc`, `tab`,
  `space`, `backspace`, `delete`, `up`, `down`, `left`, `right`,
  `home`, `end`, `pageup`, `pagedown`, `insert`), or `f1`..`f12`

Vim notation (`<C-S-x>`, `<F5>`) works too.

A plugin mapping replaces a default mapping on the same key, and a
mapping from `config.toml` or `init.lua` replaces the plugin one.
Mappings never apply while a modal layer, such as a picker or the
approval panel, is open. `ctrl+q` and `ctrl+c` stay with kage's quit
and interrupt keys, and a plugin mapping on them never fires and logs
a warning. Only a mapping from `config.toml` or `init.lua` can take
them over. The handler runs through the
coroutine bridge, so it too may open [`kage.ui.*`](#ui) dialogs, and a
non-empty string return is shown as a conversation block.

## autocomplete

### `kage.add_autocomplete_provider({ name, complete })`

Add a completion provider for the prompt input. Providers form a
stack. On each input change the host consults them in reverse
registration order (the most recently added wins) and shows the first
non-empty result in a popup above the input box. Re-adding a
provider with the same `name` replaces it in place.

`complete(prefix, ctx)` is called with the run of non-whitespace
characters before the cursor and `ctx = { text, cursor }`, the full
input and the cursor byte offset, so a provider can tokenize
differently. It returns an array of items:

```lua
kage.add_autocomplete_provider({
  name = "emoji",
  complete = function(prefix, _ctx)
    if prefix:sub(1, 1) ~= ":" then return {} end
    return {
      { value = ":tada:", label = ":tada:", detail = "party" },
    }
  end,
})
```

Item fields:

- `value` (required): the replacement text.
- `label`: the row text, `value` by default.
- `detail`: an optional dim annotation.
- `range`: optional `{ from, to }` 0-based byte offsets to overwrite.
  Without it the host replaces the matched prefix.

A `nil` or non-table return, or an error, yields no items.

In the popup, `up` and `down` (or `ctrl+p` and `ctrl+n`) move,
`tab` or `enter` accepts, and `esc` dismisses. Any other key passes
through to normal editing and queries the providers again. Providers
run synchronously on the Lua thread and return nothing while it is
busy with a tool, so keep them cheap.

Built-in completion sits at the bottom of the stack and needs no
plugin. When the token under the cursor starts with `@`, it
fuzzy-matches workdir-relative paths, honoring `.gitignore` and
`.kageignore`, and directories end in `/`. Accepting one inserts the
path as text, such as `@src/main.rs`. The file is not attached: the
model sees the literal text and reads the file with its tools when it
needs to. `@server:` completes the resources of an MCP server, which
kage does expand (see [mcp](/guide/mcp#resources-and-mentions)).

## raw input

### `kage.on_terminal_input(handler) -> off`

Register a handler the host calls for every key *before* any modal
layer or built-in binding sees it. Returning `true` consumes the
event. The call returns an `off` function that unregisters the
handler. Calling it again does nothing.

```lua
local off = kage.on_terminal_input(function(ev)
  -- ev = { code, char?, ctrl, alt, shift }
  if ev.ctrl and ev.code == "char" and ev.char == "g" then
    kage.ui.notify("intercepted ctrl+g")
    return true                              -- consume
  end
  return false
end)
```

`code` is `"char"` (with `char` set), `"enter"`, `"esc"`, `"tab"`,
`"backtab"`, `"backspace"`, an arrow or navigation key, `"f1"` to
`"f12"`, or `"other"`. Handlers run synchronously on the Lua thread.
A handler that takes longer than 20 ms lets the key through.

This is a sharp tool. Prefer
[`kage.register_keybinding`](#keybindings) for "run X on chord Y":
it is declarative, appears in help, and cannot wedge the UI. A
handler that always returns truthy makes the editor unusable, so
the host still handles its `ctrl+q` quit and `ctrl+c` interrupt keys
ahead of these hooks. A handler error or non-boolean return is treated
as "not consumed".

## widgets and status

### `kage.register_widget({ key, render })`

Register a status widget. `render(width)` returns a string painted by
the `widgets` component, on the right of the header row by default. It
follows the same retained output and render budget rules as
`set_header`.

```lua
kage.register_widget({
  key = "clock",
  render = function(_width)
    return os.date("%H:%M")
  end,
})
```

### `kage.set_status(key: string, text: string | nil)`

Push or clear a transient status entry. It is plain text only, and
the `widgets` component paints it after the widgets.

### `kage.clear_status(key: string)`

Remove a status entry. Equivalent to `kage.set_status(key, nil)`.

## events

Events and hooks come from the session you talk to. Runs of
[agents](/guide/agents) send plugins no events and run no hooks:
`transform_context`, `before_provider_request` and
`should_stop_after_turn` do not see them either. An agent can still
call plugin tools, whose handlers run as usual.

### `kage.on(event: string, handler)` -> off

Subscribe to an event. Multiple handlers per event fire in
registration order. A handler that raises is logged and skipped, so
one bad plugin does not silence the rest. An unknown event name logs
one warning and subscribes to nothing. `kage.on` is an alias over
[`kage.api.autocmd_create`](#autocmds): the handler receives the
payload alone.

Eleven events carry conversation text or tool payloads
(`transform_context`, `before_provider_request`, `compact_prepare`,
`before_agent_start`, `message_start`, `message_update`,
`message_end`, `after_provider_response`, `tool_call`, `tool_result`
and `user`) and need the `context`
[capability](/plugins/capabilities#context). Registering one without
the grant logs a warning naming it and subscribes to nothing. The
rest of the table below is open to every plugin.

The call returns an `off` function that removes this subscription.
Calling `off` more than once does nothing. A handler may call `off`
while it runs, for itself or another subscription, and the change
applies from the next dispatch.

```lua
local off
off = kage.on("message_end", function(ev)
  kage.ui.notify("first reply done")
  off()
end)
```

Plain notification events (the handler's return value is ignored):

| Event                    | Handler argument                                  |
| ------------------------ | ------------------------------------------------- |
| `before_agent_start`     | `{ system_prompt, first_user_message }`           |
| `agent_start`            | `{}`                                              |
| `agent_end`              | `{ ok }`                                           |
| `turn_start`             | `{ index }`                                       |
| `turn_end`               | `{ index, had_tool_calls }`                       |
| `message_start`          | `{ id }`                                           |
| `message_update`         | `{ id, delta }`                                   |
| `message_end`            | `{ id, usage }`                                   |
| `after_provider_response`| `{ id, usage }`                                   |
| `tool_call`              | `{ id, name, input }`                             |
| `tool_update`            | `{ id, content, structured? }`                    |
| `tool_result`            | `{ id, name, is_error, text }`                    |
| `model_select`           | `{ prev, next, source }`                          |
| `thinking_level_select`  | `{ prev, next, source }`                          |
| `user_shell`              | `{ cmd, exit_code }`                              |
| `permission_mode_select` | `{ prev, next, source }`                          |
| `option_set`             | `{ name, old, new, source }`                      |
| `color_scheme`           | `{ name }`                                        |
| `user`                   | the `data` passed to `kage.api.autocmd_exec`      |

`usage` is `{ input, output, cache_read, cache_write }`.
`model_select`, `thinking_level_select` and `permission_mode_select`
fire in the TUI only. For `model_select`, `source` is `"set"`. For
`thinking_level_select`, `prev` and `next` are level names
(`"default"` for the automatic level) and `source` is `"cycle"` or
`"settings"`. For `permission_mode_select`, `prev` and `next` are
`"default"`, `"ask"` or `"deny"` and `source` is `"command"`.
`user_shell` fires when a `!` shell command ends. `exit_code` is `nil`
when a signal or a cancel ended the command, or when it failed to
start.
`tool_update` only fires when at least one handler is subscribed. `option_set` fires when
`kage.opt.<name>` is assigned (`source` is `"lua"`) or a command or
the settings dialog changes an option (`source` is `"runtime"`).
`color_scheme` fires after the theme's base highlight groups change,
and once at the end of every load. `user` fires only through
`kage.api.autocmd_exec("user", { pattern, data })`.

### transform hooks

All three need the `context`
[capability](/plugins/capabilities#context): they read and rewrite
conversation text. These chain: each handler receives the value the
previous one produced and returns a replacement, or `nil` for "no
change".

- `transform_context`: the argument is the message-history array,
  and the loop replaces history with whatever the last handler
  returns. Use it to redact secrets or trim old tool output per turn.
- `before_provider_request`: the argument is the serialized provider
  request. Rewrite it to inject a system header, strip a tool, or
  swap the model.

  See `plugins/examples/transform_demo.lua` for a worked example
  that scrubs secret tokens via `transform_context` and stamps the
  current date into the system prompt via `before_provider_request`.
- `compact_prepare`: fired right before history compaction calls
  the summarizer model. The argument is
  `{ transcript, instruction, prompt, model, summarized, kept }`.
  Return a table with `prompt` or `instruction` (or both) to steer the
  summary, or `summary` to skip the model call entirely and use
  that text as the summary body. `nil` passes through unchanged, and
  an error aborts compaction.

### predicate hook

- `should_stop_after_turn`: the argument is the turn summary
  `{ index, had_tool_calls, usage }`. Any handler returning `true`
  halts the run after `turn_end`, so a plan-mode plugin can stop
  before execution.

### cancellable session-op hooks

`session_before_switch` and `session_before_fork` fire before the
host runs the action. The argument is the target string (a session id
or an entry id). Return `nil` to proceed, `{ cancel = "reason" }` to
veto, or `{ patch = "new-target" }` to redirect.

### `resources_discover`

Fires once at startup with no argument. Return
`{ skills? = {paths}, templates? = {paths}, themes? = {paths} }`.
The loader adds those directories to the filesystem-discovered set.

### autocmds

**Since API 2.** `kage.api.autocmd_create(event, opts)` is the full
form of `kage.on`. It returns an integer id and raises on an unknown
event name.

```lua
local group = kage.api.augroup_create("myplugin")
kage.api.autocmd_create("tool_result", {
  group = group,
  pattern = { "shell", "write" },
  callback = function(ev)
    -- ev = { id, event, match, group, data }
    if ev.data.is_error then kage.ui.notify(ev.match .. " failed") end
  end,
})
```

`opts` takes `callback` (required), `group`, `pattern` (a string or a
list of exact values, `"*"` by default), `once` and `desc`. Patterns
compare against the tool name for `tool_call` and `tool_result`, the
new value for `model_select` and `thinking_level_select`, the option
name for `option_set`, the theme name for `color_scheme`, and the exec
pattern for `user`. Other events accept only `"*"`. The four dispatch
kinds above (notification, transform, predicate, session op) apply to
autocmds the same way, reading the callback's return value.

- `kage.api.autocmd_del(id)` removes one autocmd.
- `kage.api.augroup_create(name, { clear = true })` returns a group
  id. With `clear = true`, the default, an existing group loses its
  autocmds, so a re-run registers once.
- `kage.api.augroup_del(group)` removes a group, by id or name, and
  its autocmds.
- `kage.api.autocmd_exec(event, { pattern, data })` fires an event
  now. Nesting deeper than 16 levels raises.

## options

### `kage.opt`

**Since API 2.** Read an option with `kage.opt.<name>` and set it by
assigning. A set validates the value, records its source and fires
`option_set`. Unknown names and invalid values raise with what is
valid. `kage.api.option_get(name)` returns the value and its source
(`default`, `toml`, `lua` or `runtime`). `kage.api.option_set(name,
value)` is the same as assigning. The options are listed in
[lua config](/guide/lua-config#options).

```lua
if kage.opt.editor == "vim" then
  kage.opt.timeoutlen = 500
end
```

## highlights

### `kage.api.hl_set(name, spec)` / `kage.api.hl_get(name, opts?)`

**Since API 2.** Set highlight group `name` to
`{ fg?, bg?, bold?, italic?, underline?, dim?, reverse?, link? }`,
replacing any earlier value of that group. A `link` follows another
group and wins over the other fields. An invalid color or an unknown
field raises. `hl_get` returns the group as set, or `nil`. With
`{ link = false }` it follows links and returns the effective spec.

Overrides of `Kage*` groups reset when the theme changes. Set them
from a `color_scheme` autocmd to keep them. Groups with other names
persist, which makes them a good home for a plugin's own colors:

```lua
kage.api.hl_set("MyPluginAccent", { link = "KageWarning" })
kage.ui.set_footer(function() return { text = "ready", hl = "MyPluginAccent" } end)
```

See [lua config](/guide/lua-config#group-list) for the group list.

## theme

### `kage.theme.current()` / `kage.theme.list()`

The active theme name, and every name `kage.theme.set` accepts
(bundled themes plus files in `~/.config/kage/themes/`).

### `kage.theme.set(name: string)`

Set the `theme` option. The base highlight groups switch before the
call returns, then `color_scheme` and `option_set` fire. Raises on a
non-string, empty or unknown name.

## scheduling

**Since API 2.** These run a callback on the Lua thread later. A
callback that raises is logged, and a timer that raises stops.
Callbacks cannot open `kage.ui.*` dialogs. A reload cancels all of
them.

### `kage.schedule(fn)`

Run `fn` once, right after the current Lua call returns and before the
next queued one. Useful to move work out of an event handler.

### `kage.defer(fn, ms: integer)` -> stop

Run `fn` once, `ms` milliseconds from now. Calling `stop` before then
cancels it.

### `kage.timer(fn, ms: integer)` -> stop

Run `fn` every `ms` milliseconds (at least 50) until `stop` is called.

```lua
local stop = kage.timer(function()
  kage.api.redraw("footer")
end, 1000)
```

## session

### `kage.session.list()`

Return an array of session entries the host knows about:

```lua
for _, s in ipairs(kage.session.list()) do
  print(s.id, s.value) -- short id, absolute path
end
```

### `kage.session.fork(at?: string)`

Ask the host to fork the current session at entry-id prefix `at`
(or the latest entry when omitted). Returns `nil`. The host takes the
request between turns and writes a new session file.

### `kage.session.append_entry(kind: string, data?: table)`

Requires the `session_write`
[capability](/plugins/capabilities#session_write). Append a custom
entry to the session JSONL. `kind` is a non-empty
namespaced string such as `"my-plugin:bookmark"`. `data` is any
table, JSON-serialized (defaults to `{}`). The host writes it
between turns. You can register a block renderer for your kind and
queue custom blocks, but custom content in replayed transcripts is
not displayed yet: the host drops it when it rebuilds the
conversation.

### `kage.session.set_label(anchor: string, label?: string)`

Write a label entry pointing at the entry id `anchor`. Passing
`label = nil` clears it. Use it to bookmark a point in the
conversation.

## conversation

### `kage.send_message(text: string, opts?: table)`

Requires the `session_write`
[capability](/plugins/capabilities#session_write): a synthetic
message is session content. Queue a synthetic message the host
delivers between turns:

```lua
kage.send_message("re-run the tests", { trigger_turn = true })
```

`opts` fields: `trigger_turn` (bool, default `true`) and
`deliver_as` (default `"user"`). Only `"user"` is wired.
`"assistant"` and `"system"` raise an error rather than silently
doing the wrong thing.

## context inspection

### `kage.context_usage()`

Snapshot the session's token usage:

```lua
local u = kage.context_usage()
if u then print(u.model, u.current_context, u.context_window) end
```

The table holds `model`, `input_tokens`, `output_tokens`,
`cache_read_tokens`, `cache_write_tokens`, `current_context`,
`context_window` and `working`. The TUI fills it. It is `nil` before
that, and always in print mode and `kage rpc`.

### `kage.compact(prompt?: string)`

Ask the TUI to compact the conversation, as `/compact` does. kage does
not use `prompt`. To steer the summary, subscribe to the
[`compact_prepare`](#transform-hooks) transform event, which can
rewrite the prompt and instruction or replace the summary outright.

## fs

### `kage.fs.read(path: string)`

Read a file relative to the session workdir. Paths outside the
workdir tree raise an error.

### `kage.fs.write(path: string, contents: string)`

Requires the `fs_write`
[capability](/plugins/capabilities#fs_write). Write a file under the
workdir. Same path restriction as `read`, including through
symlinks; missing parent directories are created.

## env

`kage.env` and `kage.credential` are gated behind the `env`
capability. A plugin must be granted `env` in `[plugins.capabilities]`
and request it at load time before they are attached to its
environment.

### `kage.env(name: string)` -> string | nil

Return the value of one variable from the host process environment,
or `nil` when it is unset. Raises when the value is not valid UTF-8.
Access is read-only, with no setter and no per-variable allowlist: a
granted plugin can read every variable, including secrets. See
[capabilities](/plugins/capabilities#env).

### `kage.credential(provider: string)` -> string | nil

Return the token the host holds for a provider id, the same store the
login flow writes, or `nil` when nothing is stored. The same `env`
grant attaches it: stored tokens are secrets of the same class as
environment secrets.

## http

`kage.http` is gated behind the `net` capability. A plugin must be
granted `net` in `[plugins.capabilities]` and request it at load time
before `kage.http.get`, `post`, `delete` and `post_stream` are
attached to its environment. Only SSRF filtering applies: the scheme
must be http(s) and the host must resolve to a routable address. There
is no host allow-list. See [capabilities](/plugins/capabilities#net)
for the options and results.

## crypto

`kage.crypto` is gated behind the `crypto` capability. A plugin must be
granted `crypto` in `[plugins.capabilities]` and request it at load time
before the primitives are attached to its environment. The calls are
stateless and synchronous: random bytes, SHA-256, SHA-512, HMAC-SHA256,
HKDF-SHA256, AES-256-GCM decrypt, Ed25519 sign, and base64 and hex
conversion, all over byte strings. Since API 2. See
[capabilities](/plugins/capabilities#crypto) for the table.

## acp and mcp

`kage.acp.add_agent(spec)` declares an upstream ACP agent and
`kage.on_acp_permission(fn)` decides its tool requests. See
[acp client](/editors/acp-client#configure-from-a-plugin).
Declaring an agent names a command the host will spawn, so
`kage.acp.add_agent` requires the `exec` capability.

`kage.mcp.add_server(spec)`, `kage.mcp.list_servers()` and
`kage.mcp.restart(name)` declare, list and restart MCP servers. See
[mcp](/guide/mcp#declaring-servers-from-a-plugin). Declaring and
restarting name commands the host spawns, so both require the `exec`
capability; `list_servers` needs no grant.

## providers

### `kage.register_provider(spec)`

Requires the `provider`
[capability](/plugins/capabilities#provider). Register a new LLM
provider implementation. This is advanced. A
streaming provider makes outbound requests via `kage.http.post_stream`,
so it also needs the `net` capability. See `plugins/types/kage.lua` for the
full spec shape.

Every plugin shares one Lua thread, and a `stream` handler runs on it
as a coroutine. While it waits in `kage.http.*`, `kage.sleep_ms` or an
`emit` the agent is not yet reading, it yields: other streams,
commands, events and renders run in between, so several agents can
stream from the same provider at once. Code that busy-loops without
calling any of these still holds the thread until it returns.

### `kage.provider_error(kind, message, status?)`

Requires the `provider` capability. Raise a typed
provider error so the agent loop applies the right retry policy. Never
returns.

A bare `error("...")` from a provider handler reaches the
loop as a permanent decode error, so a plugin that fails on a dropped
connection gets no retry even though the loop has a retry-with-backoff
path for exactly that case. This is how a plugin says which kind of
failure it hit.

| kind | retried |
| --- | --- |
| `transport` | yes, a timeout or reset is the pipe's fault |
| `rate_limited` | yes |
| `http` | on 5xx, 408 and 429; pass the status |
| `auth` | no |
| `unknown_model` | no |
| `decode` | no |

The message you pass reaches the user for every kind; for
`rate_limited` it is shown after `rate limited:` instead of a canned
phrase.

An unknown kind raises rather than defaulting, naming the known ones,
so a typo is visible instead of silently non-retried.

```lua
kage.register_provider({
    id = "mine",
    stream = function(req, emit)
        local res = kage.http.post(url, opts)
        if res.status == 429 then
            kage.provider_error("http", "rate limited", res.status)
        end
        -- A connect timeout is transport, so the loop retries it.
        kage.provider_error("transport", "connect timed out")
    end,
})
```

## `kage.api` reference

Every `kage.api` function is since API 2.

| Function | Purpose |
| --- | --- |
| `autocmd_create(event, opts)` -> id | subscribe to an event ([autocmds](#autocmds)) |
| `autocmd_del(id)` | remove an autocmd |
| `augroup_create(name, opts?)` -> id | create or clear a group |
| `augroup_del(group)` | remove a group and its autocmds |
| `autocmd_exec(event, opts?)` | fire an event now |
| `option_get(name)` -> value, source | read an option ([options](#options)) |
| `option_set(name, value)` | set an option |
| `hl_set(name, spec)` | set a highlight group ([highlights](#highlights)) |
| `hl_get(name, opts?)` -> spec or nil | read a highlight group |
| `slot_set(name, spec)` | fill a slot, same as `kage.ui.set_slot` |
| `redraw(name?)` | recompute the Lua components of one slot, or of every slot |
| `keymap_set(mode, lhs, rhs, opts?)` | map a key in one mode, see `kage.keymap.set` |
| `keymap_del(mode, lhs)` | remove a mapping in one mode |
