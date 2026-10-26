# lua config

`init.lua` is kage's Lua configuration file. It sets options, maps
keys, reacts to events, restyles highlight groups and rearranges the
header, working row, input rule, footer and start card. `config.toml` keeps
working next to it. Its UI keys and `[keybindings]` table feed the
same options and keymap table, and `init.lua` runs after it, so
`init.lua` wins.

A complete, working example is at the [end of this page](#complete-example).

## init.lua and lua/ modules

kage reads `$XDG_CONFIG_HOME/kage/init.lua`, which is
`~/.config/kage/init.lua` by default. The file is optional.

- Only the TUI loads it. Print mode (`kage -p`), `kage rpc` and
  `kage mcp serve` read `config.toml` only.
- A project's `.kage/init.lua` is never loaded. Project settings go in
  `.kage/config.toml` (see [configuration](/guide/config)).
- An error in `init.lua` shows as an error block in the
  conversation, such as `init.lua: runtime error: init.lua:3: boom`
  followed by `Fix init.lua, then run /reload.` The Lua traceback is
  left out. kage still starts, with its defaults and your plugins
  loaded.

`require` loads modules from the `lua/` directory next to `init.lua`.
A name is dot-separated segments of letters, digits, `_` and `-`, and
`a.b` tries `lua/a/b.lua`, then `lua/a/b/init.lua`:

```lua
-- ~/.config/kage/lua/me/greet.lua
local M = {}

function M.hello(name)
  kage.notify("hello " .. name)
end

return M
```

```lua
-- ~/.config/kage/init.lua
require("me.greet").hello("world")
```

A module runs once per load and its return value is cached until the
next reload. A module that requires itself, directly or through
others, raises `loop requiring <name>`. Paths that leave `lua/`,
through `..`, an absolute name or a symlink, are rejected.

### trust

`init.lua` is trusted. Plugins are not.

| | `init.lua` and `lua/` | plugins |
| --- | --- | --- |
| Location | `~/.config/kage/init.lua` | `~/.config/kage/plugins/*.lua` |
| Capabilities | all of `exec`, `env`, `net` and `session_write`, without asking | only what `[plugins.capabilities]` grants and the plugin requests |
| `require` | confined to `lua/` | not available |
| Raw `io`, `os.execute`, `debug` | removed | removed |
| Loaded by | the TUI only | the TUI, print mode and `kage rpc` |

`kage.request_capabilities` still works in `init.lua` and reports every
capability as granted. Each plugin and `init.lua` run in their own
environment, so a plugin cannot reach the trusted functions or change
what another environment sees.

## load order and reload

Every load runs the same steps, and each step can override the ones
before it. The last set wins.

1. `_defaults.lua`, embedded in kage: the default keymaps and every
   slot (header, activity, input pill, footer and start card).
2. Plugins, sorted by file name.
3. `[keybindings] bindings` from `config.toml`.
4. `init.lua`.
5. The `color_scheme` event fires once for the current theme, so
   highlight overrides written as `color_scheme` autocmds apply.

Options are seeded from `config.toml` and `KAGE_*` environment
variables before step 1.

kage watches the plugins directory, `init.lua` and `lua/` (recursively)
and reloads when a Lua file changes. A `lua/` directory created while
kage runs is watched too. `/reload` reloads right away and reports the
result, such as `plugins reloaded (2 loaded, init.lua ok)`. A reload
clears keymaps, autocmds, slots, timers and everything plugins
registered, then runs the steps above again. It does not revert
options or highlight overrides. If you delete
`opt.theme = "kage-dawn"` from `init.lua`, the theme stays until you
restart kage.

## options

`kage.opt.<name>` reads an option. Assigning to it sets the option:

```lua
kage.opt.theme = "kage-dawn"
kage.opt.input_max_lines = 12
kage.notify("editor is " .. kage.opt.editor)
```

An unknown name or an invalid value raises an error that lists what is
valid. `kage.api.option_get(name)` returns the value and where it came
from (`default`, `toml`, `lua` or `runtime`). `kage.api.option_set(name,
value)` is the same as assigning.

| Option | TOML key | Type | Default | Applies |
| --- | --- | --- | --- | --- |
| `theme` | `ui.theme` | a bundled or user theme name | `"default"` | immediately |
| `theme_dark` | `ui.theme_dark` | a bundled or user theme name | `"kage-shadow"` | immediately |
| `theme_light` | `ui.theme_light` | a bundled or user theme name | `"kage-dawn"` | immediately |
| `mouse` | `ui.mouse` | boolean | `true` | immediately |
| `editor` | `ui.editor` | `"vim"` or `"modeless"` | `"modeless"` | immediately |
| `input_min_lines` | `ui.input_min_lines` | integer, 1 to 64 | `1` | immediately |
| `input_max_lines` | `ui.input_max_lines` | integer, 1 to 64 | `8` | immediately |
| `thinking_level` | `ui.thinking_level` | `""`, `"off"`, `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"` or `"ultra"` | `""` | next session |
| `transcript_on_exit` | `ui.transcript_on_exit` | `"full"`, `"last"` or `"none"` | `"full"` | at exit |
| `compaction_threshold` | `loop.compaction_threshold` | number, 0 to 1 (0 turns compaction off) | `0.8` | next session |
| `leader` | `keybindings.leader` | one key, such as `","` or `"<Space>"` | `"\\"` (backslash) | mappings set after it |
| `timeoutlen` | `keybindings.timeoutlen` | integer milliseconds, 0 to 5000 | `1000` | immediately |
| `agent_max_depth` | `agents.max_depth` | integer, 0 to 3 (0 turns the `agent` tool off) | `1` | at startup |
| `agent_max_running` | `agents.max_running` | integer, 1 to 16 | `4` | at startup |
| `swarm_max_items` | `agents.swarm_max_items` | integer, 2 to 128 | `32` | at startup |
| `swarm_timeout_ms` | `agents.swarm_timeout_ms` | integer milliseconds, 1,000 to 86,400,000 | `7200000` | at startup |
| `agent_model` | `agents.model` | `"provider/model"` or `"inherit"` | `"inherit"` | at startup |
| `agent_thinking` | `agents.thinking` | `"inherit"`, `"off"`, `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"` or `"ultra"` | `"inherit"` | at startup |
| `agent_max_turns` | `agents.max_turns` | integer, 0 to 10,000 (0 for no limit) | `0` | at startup |
| `agent_timeout` | `agents.timeout` | integer seconds, 0 to 86,400 (0 for no limit) | `0` | at startup |
| `agent_budget` | `agents.budget` | integer tokens, 0 to 1,000,000,000 (0 for no limit) | `0` | at startup |

`transcript_on_exit` picks what kage prints to the terminal after you
quit: the whole conversation as plain text, only the part from your
last prompt on, or nothing. When the session was recorded, the session
file path and a `kage resume <id>` hint follow.

`thinking_level` and `compaction_threshold` set in `init.lua` apply to
the first session, because kage starts it after `init.lua` has run. An
empty `thinking_level` means the automatic level: high, or the nearest
level the model accepts.

`agent_max_depth` limits how deep [agents](/guide/agents) nest: `1`
lets only the main session start agents. `agent_max_running` limits
how many agents run at once, and further agents wait their turn.
`swarm_max_items` and `swarm_timeout_ms` bound a
[`swarm`](/guide/agents#swarms) call: the most items it may run and
the time one child may run from its start. `agent_max_turns`,
`agent_timeout` and `agent_budget` bound what agents may spend (see
[limits](/guide/agents#limits)). All are read once when the TUI
starts, after `init.lua` has run.

Every set fires the [`option_set`](#configuration-events) event. `/theme set`,
`/mouse` and the `/settings` dialog set options too, with source
`runtime`. The settings dialog marks options last set from Lua, since
`init.lua` shadows a saved TOML value on the next start.

## keymaps

```lua
kage.keymap.set(mode, lhs, rhs, opts)
kage.keymap.del(mode, lhs)
```

`mode` is one letter or a list of letters, and each mode gets its own
entry:

| Mode | Where it applies |
| --- | --- |
| `n` | vim normal mode, in either pane |
| `b` | vim normal mode with the conversation pane focused |
| `i` | vim insert mode and the modeless editor |
| `v` | visual mode |
| `g` | every editing state above |

A key is looked up in these modes, first match wins:

| Editing state | Modes searched |
| --- | --- |
| vim normal, conversation pane | `b`, `n`, `g` |
| vim normal, input pane | `n`, `g` |
| vim insert, or modeless | `i`, `g` |
| visual | `v`, `g` |

Mappings never apply while an overlay (a picker, a dialog, the `:`
line, the `/` search line, the approval panel) is
open. While the
autocomplete popup is open it sees its own keys first.

### notation

`lhs` uses Vim notation:

| Form | Examples |
| --- | --- |
| characters, case-sensitive | `gg`, `zM`, `Y`, `?` |
| Ctrl, Alt, Shift, Super | `<C-l>`, `<M-p>`, `<S-Tab>`, `<D-k>`, `<C-S-x>` |
| named keys | `<CR>`, `<Esc>`, `<Tab>`, `<BS>`, `<Del>`, `<Space>`, `<Up>`, `<Down>`, `<Left>`, `<Right>`, `<Home>`, `<End>`, `<PageUp>`, `<PageDown>`, `<Insert>`, `<F1>` to `<F12>` |
| special characters | `<lt>`, `<gt>`, `<Bslash>`, `<Bar>` |
| the leader | `<leader>m` |

`<C-L>` is the same as `<C-l>`. Add `S-` for Ctrl with Shift. The chord
form from `config.toml` also works for a single key: `ctrl+shift+x`,
`alt+p`, `f5`.

`<leader>` expands to the `leader` option when the mapping is set, as
in Neovim. Set `kage.opt.leader` before your leader mappings. The
default leader is a backslash. A leader that is also a printable key,
such as `<Space>`, is best used in modes `n` and `b` only, since a
mapping in `i` or `g` would catch it while you type.

### rhs

| `rhs` | Effect |
| --- | --- |
| `kage.action.Name` | runs a built-in action |
| `kage.action.scroll(n)` | scrolls the conversation `n` lines (negative scrolls up) |
| `":command args"` | runs a command line, like typing it after `:` |
| a function | runs Lua. It may open `kage.ui.*` dialogs, and a non-empty string return is shown as a conversation block. |
| `"<Nop>"` | swallows the key |

`kage.action` holds: `Cancel`, `BeginCommand`, `BeginSearch`,
`ScrollToTop`, `ScrollToBottom`, `ToggleFold`, `UnfoldAll`, `FoldAll`,
`Yank`, `ClearSelection`, `OpenModelPicker`, `OpenSessionPicker`,
`SearchNext`, `SearchPrev`, `YankFocusedBlock`,
`CycleThinkingLevel`, `TogglePlanMode`, `CyclePane`, `FocusPrev`, `FocusNext`,
`OpenHelp`, `OpenJumpPicker`, `AttachClipboardImage`, `EnterVisual`,
`QueuePrompt`, `RecallPrompt`, `OpenAgents`, and the function
`scroll(n)`.

`OpenAgents` opens the [agents overlay](/guide/agents#the-agents-overlay).
`_defaults.lua` maps it to `<C-t>` in mode `g`.

`QueuePrompt` sends the draft to run after the current run ends. While
kage is idle it does nothing, so the default `<Tab>` mapping never
sends a prompt by accident. The completion popup sees `Tab` before any
mapping.

`RecallPrompt` takes the newest prompt waiting above the input back
out of the engine queue and puts it in the editor to edit, images
included. While nothing is pending it does nothing. It has no default
mapping: `Up` on the draft's top row does the same while a prompt is
pending, and walks the history otherwise.

`opts` takes `desc` and `group`. The `?` reference lists every mapping
that has a `desc`, under its `group` (`other` when unset). Mappings
without a `desc` still work but are hidden there.

```lua
local map, act = kage.keymap.set, kage.action

kage.opt.leader = ","
map("n", "<leader>m", act.OpenModelPicker, { desc = "pick a model", group = "mine" })
map("g", "<F6>", ":theme set kage-shadow", { desc = "dark theme", group = "mine" })
map("b", "<C-d>", act.scroll(20), { desc = "scroll a page", group = "mine" })
map({ "i", "n" }, "<F5>", function()
  local ok = kage.ui.confirm("compact", "Compact the conversation now?")
  if ok then
    kage.compact()
  end
end, { desc = "compact", group = "mine" })
```

### sequences and timeoutlen

A mapping can be several keys long. While the keys typed so far are
the start of a longer mapping, kage waits for more, and the `hint`
component (in the footer by default) shows the pending keys, such as
`g ...`. When the keys stop matching, the longest
mapping they complete fires and the rest are looked up again. If none
fires, the keys go to the editor in order, so `gg` still moves to the
start of the prompt in the input pane. A mapping that is also the
start of a longer one fires after `timeoutlen` milliseconds with no
further key.

### defaults and deleting

kage's own keys are mappings from `_defaults.lua`, owned by
`defaults`, so you can replace or delete any of them:

```lua
kage.keymap.set("g", "<C-p>", "<Nop>")  -- shadow the model picker key
kage.keymap.del("g", "<C-s>")           -- remove the session picker key
```

`kage.keymap.del` raises when the mapping does not exist. See
[keybindings](/guide/keybindings) for the default table.

The editor grammar stays in Rust and is not in the table: vim motions,
operators, counts, registers, `r`, undo and redo, readline edits and
the kill ring, `enter`, `shift+enter` and `alt+enter`, history `up`
and `down`, `esc`, insert-mode `ctrl+o`, `ctrl+g` (external editor), the modeless `/`,
`!` and `?` empty-prompt prefixes, and `i` and `a` in the conversation
pane. A mapping or `"<Nop>"` on one of these keys shadows it.
`kage.keymap.del` cannot remove it.

`ctrl+q` (quit) and `ctrl+c` work above every layer, including overlays.
`ctrl+c` interrupts the run, else arms quit (see
[keybindings](/guide/keybindings#esc-and-ctrl-c)). While kage is idle,
`ctrl+c` closes an open overlay instead. Both yield only
to a mapping owned by `init.lua` or `config.toml`. A plugin mapping on
them never fires and logs a warning.

`/keybindings` (alias `/keys`) lists the whole table per mode with the
owner of each mapping: `defaults`, a plugin name, `config.toml` or
`init.lua`.

## autocmds

`kage.api.autocmd_create(event, opts)` runs `opts.callback` when
`event` fires and returns an id.

```lua
local api = kage.api
local g = api.augroup_create("me")

api.autocmd_create("tool_call", {
  group = g,
  pattern = "shell",
  desc = "log shell calls",
  callback = function(ev)
    kage.log("info", "shell: " .. tostring(ev.data.input.command))
  end,
})
```

| Field | Meaning |
| --- | --- |
| `callback` | required. Receives `ev = { id, event, match, group, data }`, where `data` is the event payload. |
| `group` | a group id or name from `augroup_create` |
| `pattern` | a string or a list of strings compared exactly against the event's match value. `"*"`, the default, matches everything. |
| `once` | delete the autocmd before its first call |
| `desc` | shown in error messages |

The events and payloads are listed in the [Lua API](/plugins/api#events).
[Agent](/guide/agents) runs fire no events, so loop events such as
`tool_call` come only from the session you talk to.
These events have a match value, and every other event accepts only
`"*"`:

| Event | Match value |
| --- | --- |
| `tool_call`, `tool_result` | the tool name |
| `model_select`, `thinking_level_select` | the new value |
| `option_set` | the option name |
| `color_scheme` | the theme name |
| `user` | the `pattern` given to `autocmd_exec` |

### configuration events

Three events come from the configuration layer:

- `option_set` fires on every option set, with
  `{ name, old, new, source }`. `source` is `"lua"` or `"runtime"`.
- `color_scheme` fires with `{ name }` after the theme's base groups
  change, and once at the end of every load.
- `user` fires only through `kage.api.autocmd_exec("user", { pattern,
  data })`. `data` arrives as `ev.data`.

Other functions:

- `kage.api.autocmd_del(id)` deletes one autocmd. A missing id is
  ignored.
- `kage.api.augroup_create(name, { clear = true })` returns the group
  id. With `clear = true`, the default, an existing group loses its
  autocmds first, so running the same setup twice does not register
  twice.
- `kage.api.augroup_del(group)` deletes a group and its autocmds.
- `kage.api.autocmd_exec(event, { pattern, data })` fires any event now.
  Nesting deeper than 16 levels raises.

`kage.on(event, fn)` is the short form: `fn` receives the payload
alone, and the call returns an `off` function. Unknown event names
raise in `autocmd_create` but only warn once in `kage.on`.

## highlight groups

Every color kage paints comes from a highlight group. Themes provide
the base groups, and Lua can override them or add its own:

```lua
kage.api.hl_set("KageUserBubble", { bg = "#1b1e2b" })
kage.api.hl_set("MyCost", { link = "KageMuted" })
local spec = kage.api.hl_get("KageToolError")
```

A spec has `fg`, `bg`, `bold`, `italic`, `underline`, `dim`, `reverse`
and `link`. Colors are `#rrggbb`, one of the 16 names (`black`, `red`,
`green`, `yellow`, `blue`, `magenta`, `cyan`, `gray`, `darkgray`,
`lightred`, `lightgreen`, `lightyellow`, `lightblue`, `lightmagenta`,
`lightcyan`, `white`) or a palette index `"0"` to `"255"`. `link`
follows another group and wins over every other field. A link cycle
resolves to an empty style. An invalid color or an unknown field
raises.

- `hl_set` replaces the whole group. To change one color, read the
  group with `hl_get`, change the field and set it back.
- `hl_get(name)` returns the group as set, or `nil` when it does not
  exist. `hl_get(name, { link = false })` follows links and returns the
  effective spec.
- Switching theme replaces the base groups and drops your overrides of
  `Kage*` groups. Groups with other names persist. Set `Kage*`
  overrides from a `color_scheme` autocmd so they survive a switch:

```lua
kage.api.autocmd_create("color_scheme", {
  callback = function()
    kage.api.hl_set("KageUserBubble", { bg = "#1b1e2b" })
  end,
})
```

The built-in renderer reads only the colors of `Kage*` groups.
Attributes such as `bold` apply where a group is used by name: span
`hl` fields and slot component `hl`.

Spans in slots, `set_header`, `set_footer` and block renderers take
`hl = "Group"`. Their `fg` and `bg` accept a group name (its `fg` or
`bg`), a theme role name such as `muted_fg`, or a color. Styles
resolve when the row is painted, so they follow theme switches.

### group list

Each theme role (see [themes](/guide/themes#color-roles)) lives in one
group, as its foreground or its background.

| Group | fg role | bg role |
| --- | --- | --- |
| `KageNormal` | `assistant_fg` | `bg` |
| `KageUserBubble` | | `user_bg` |
| `KageUserRule` | `user_rule` | |
| `KageAssistantRule` | `assistant_rule` | |
| `KageThinking` | `thinking_fg` | |
| `KageTool` | `tool_result_fg` | `tool_bg` |
| `KageToolError` | `tool_error_fg` | `tool_error_bg` |
| `KageToolPending` | | `tool_pending_bg` |
| `KageToolRule` | `tool_rule` | |
| `KageToolErrorRule` | `tool_error_rule` | |
| `KageToolPendingRule` | `tool_pending_rule` | |
| `KageCustom` | `custom_fg` | |
| `KageStatus` | `status_dim_fg` | `status_bg` |
| `KageMuted` | `muted_fg` | |
| `KageMatch` | | `match_color` |
| `KageSelection` | `selection_fg` | `selection_color` |
| `KageFocus` | | `focus_color` |
| `KageInputBorderNormal` | `input_border_normal` | |
| `KageInputBorderInsert` | `input_border_insert` | |
| `KageInputBorderVisual` | `input_border_visual` | |
| `KageInputPillNormal` | `input_pill_normal_fg` | `input_pill_normal_bg` |
| `KageInputPillInsert` | `input_pill_insert_fg` | `input_pill_insert_bg` |
| `KageInputPillVisual` | `input_pill_visual_fg` | `input_pill_visual_bg` |
| `KageInputGlyph` | `input_glyph_fg` | |
| `KageInputPlaceholder` | `input_placeholder_fg` | |
| `KageInputHint` | `input_hint_fg` | |
| `KageModeline` | `modeline_fg` | `modeline_bg` |
| `KageOverlay` | `overlay_fg` | |
| `KageOverlayBorder` | `overlay_border` | |
| `KageOverlaySelected` | `overlay_selected_fg` | `overlay_selected_bg` |
| `KageWarning` | `warning_fg` | |
| `KageSuccess` | `success_fg` | |
| `KageMarkdownH1` | `md_h1_fg` | |
| `KageMarkdownH2` | `md_h2_fg` | |
| `KageMarkdownLink` | `md_link_fg` | |
| `KageMarkdownCode` | `md_code_fg` | |

Four more groups color parts of the chrome. They are not theme roles, and
every theme links them to another group until you set them:

| Group | Default link | Colors |
| --- | --- | --- |
| `KageWorking` | `KageMuted` | the working row |
| `KagePlan` | `KageInputBorderInsert` | the `plan` footer chip while [plan mode](/guide/plan-mode) is on |
| `KageInputCommand` | `KageInputGlyph` | a command that opens the draft, such as `/model` |
| `KageApproval` | `KageWarning` | approval panel rules, title and selected option, and the bullet of a tool call waiting for approval |
| `KageDiffAdd` | `KageSuccess` | `+` lines in edit rows and approvals |
| `KageDiffDelete` | `KageToolErrorRule` | `-` lines in edit rows and approvals |

A theme file can also set groups in its `[groups]` table (see
[themes](/guide/themes#groups)).

## slots

Slots are the fixed chrome regions, from top to bottom:

| Slot | Region | Spec |
| --- | --- | --- |
| `header` | the top row, collapsed while it paints nothing | `{ left, right, sep }` |
| `start` | the start card above the input while the conversation is empty | `{ lines }` |
| `activity` | the working row above the input, collapsed while it paints nothing | `{ left, right, sep }` |
| `input_pill` | the input's top rule | `{ left, right, sep }` |
| `footer` | the bottom rows | `{ left, right, sep }` for one row, or `{ rows = { row, ... } }` for up to four, each row a `{ left, right, sep }` |

`kage.ui.set_slot(name, spec)` replaces a slot's spec, and
`kage.ui.set_slot(name, nil)` restores the one `_defaults.lua` set.
To remove a row, give it an empty spec, such as
`kage.ui.set_slot("activity", { left = {} })`.
`left` items paint from the left edge and `right` items against the
right edge. `sep` goes between two items that both have output. When
a row is too narrow, the right side drops its items from the front.
A footer row that paints nothing collapses, and one row always stays.
The `:` command line and the `/` search line paint over the footer's
top row while they are open.

`start` paints while the conversation holds nothing but notices: no
prompt, reply, thinking, tool call or shell command yet. Errors such
as a broken `config.toml` stay above it. The card is bottom-aligned
directly above the input, and each item is one line, indented. Span
lines, such as the tip, wrap at the card width instead. When
the rows do not fit, the tip and other span or Lua lines go first,
then the recent sessions, then notices past the first two.
`{ lines = {} }` turns the card off.

kage's defaults are:

```lua
local dot = " \u{B7} "
kage.ui.set_slot("header", { left = { "breadcrumb", "title" }, right = { "widgets", "search" } })
kage.ui.set_slot("activity", { left = { "activity" } })
kage.ui.set_slot("input_pill", { left = { "mode" }, right = { "thinking" } })
kage.ui.set_slot("footer", {
  rows = {
    { left = { "hint" }, right = { "model", "context" }, sep = dot },
    { left = { "permission", "plan", "swarm", "agents", "tasks" }, right = { "cwd", "tokens" }, sep = dot },
  },
})

local blank = { text = "" }
kage.ui.set_slot("start", {
  lines = {
    "mark", "brand", blank,
    "model", "cwd", "permission", "thinking", blank,
    "sessions", "notices",
    { text = "Tip: " .. tips[math.random(#tips)], hl = "KageMuted" },
  },
})
```

`tips` is a list of short hints, and one is picked at random on each
load. So the header row shows only once the session has a title, an
agent is on screen, or a widget or a search is active, and the working
row only while a run is in flight.

An item is one of:

- a built-in component name, painted by kage every frame
- a span, `{ text, hl?, fg?, bg?, bold?, dim?, italic?, underline? }`
- a Lua component, `{ render, events?, interval?, hl? }`

### built-in components

| Component | Shows |
| --- | --- |
| `mark` | `start` only: the kage cat, drawn in half blocks seven rows tall |
| `brand` | `kage` |
| `breadcrumb` | in an agent view, the path to the agent and its task, then its state, time, tokens and tool count, such as `kage > explore: map exports` and `running`, `41s`, `22k tok`, `14 tools`. Nothing in the main view. |
| `title` | the session title, once there is one. Nothing while an agent is on screen. |
| `model` | the active model, by its model picker name |
| `widgets` | plugin widgets and `kage.set_status` entries |
| `search` | the search match count while a search is active, such as `match 2/5` |
| `session` | the session id, as `#<id>` |
| `working` | a spinner while a run is in flight |
| `activity` | what the run is doing and for how long, such as `Running cargo test (14s, esc to interrupt)`. The label is `Working`, `Thinking`, the running tool, `Waiting for N agents`, or `Waiting for your approval`. In an agent view it follows that agent. |
| `context` | context use against the window, such as `12% ctx` |
| `tokens` | input and output tokens, the cached share and the cost when known, such as `in 14k out 2k cached 9k (40.00%) $0.02`. Agents under the session on screen count too. |
| `thinking` | the thinking level the next run sends, such as `thinking high (auto)` (`auto` when you have not chosen one), hidden when off |
| `permission` | the session's permission mode: `ask when needed`, or an override such as `ask mode` |
| `plan` | `plan` while [plan mode](/guide/plan-mode) is on |
| `swarm` | `swarm` while swarm mode is on |
| `agents` | the live agents under the session on screen and how many of them run in the background, such as `3 agents (2 bg)`, hidden while none runs |
| `tasks` | the `!` shell commands still running, such as `1 shell` |
| `mode` | `NORMAL`, `INSERT` or `VISUAL` in vim mode, `shell` while `!` shell mode is armed, nothing otherwise |
| `hint` | what the next keys do: the pending keys of a mapping sequence, the keys of the open picker, dialog, approval panel, `:` line or search line, `ctrl+c again to quit` or `esc again to rewind`, else a hint for the current state such as `? for shortcuts`, `tab to queue`, `ctrl+t for agents`, or `enter to steer`, `esc to go back` and `ctrl+c to stop` in an agent view |
| `cwd` | the working directory with `~` for your home, and the git branch checked out, such as `~/dev/kage (main)` |
| `version` | the kage version |
| `sessions` | `start` only: the three most recent sessions with their titles and times |
| `notices` | `start` only: startup notices, such as a missing credential and the `/login` command that fixes it |

Keys named in hints follow your mappings. Remapping
`OpenModelPicker` changes the start card's `ctrl+p to change`, and
remapping `QueuePrompt` changes the footer's `tab to queue`, and
remapping `OpenAgents` changes `ctrl+t for agents`.

In `start`, some components paint differently. `mark` is the first
line to go when the card runs out of rows. `brand` adds the
version. `model`, `cwd`, `permission` and `thinking` become labeled
rows (`model`, `directory`, `permissions`, `thinking`) with a change
hint against the right edge. `permission` there summarizes the
configured rules when no override is active. `mark`, `sessions` and
`notices` paint nothing in a row slot.

An unknown component name raises.

### Lua components

```lua
{
  render = function(ctx) return "text" end,
  events = { "message_end", "user Tick" },
  interval = 5000,
  hl = "MyGroup",
}
```

`render(ctx)` returns a string, a span, or a list of those (one line
each). A row slot paints the first line only. `nil` or an empty string
paints nothing, and the item takes no separator. A render that raises
is logged and the previous output stays.

kage keeps the output and calls `render` again only when:

- an event in `events` fires. An entry is an event name, optionally
  followed by a space and a pattern: `"tool_result shell"`,
  `"user Tick"`.
- `interval` milliseconds pass (at least 50).
- `kage.api.redraw(slot)` is called. With no argument it redraws every
  slot.
- the slot is set.
- the terminal width changes.

Rendering never waits on Lua, and a render that runs too long is
aborted.

`ctx` holds:

| Field | Value |
| --- | --- |
| `width` | terminal width in columns |
| `model` | the active `provider/model` id |
| `thinking` | the thinking level the next run sends, `"off"` when it sends none |
| `thinking_auto` | whether that level is automatic rather than chosen |
| `permission_mode` | the session permission override, or `nil` |
| `working` | whether a turn is running |
| `usage` | `{ total = { input, output, cache_read, cache_write }, context_used, context_window, cost }`, or `nil` before the first report |
| `session` | `{ id, title }` |
| `cwd` | the working directory |
| `mode` | `"normal"`, `"insert"` or `"visual"` |

`kage.ui.set_header(fn)` and `kage.ui.set_footer(fn)` are shorthands
that replace the whole row with one component calling `fn(width)`
every 500 ms. `nil` restores the default row.

## timers

| Function | Runs `fn` |
| --- | --- |
| `kage.schedule(fn)` | once, right after the current Lua call returns |
| `kage.defer(fn, ms)` | once, `ms` milliseconds from now. Returns `stop`, which cancels it. |
| `kage.timer(fn, ms)` | every `ms` milliseconds (at least 50) until the returned `stop` is called |

Callbacks run on the Lua thread between other Lua work. A callback that
raises is logged, and a timer that raises stops. Callbacks cannot open
`kage.ui.*` dialogs. A reload cancels every timer.

```lua
local stop = kage.timer(function()
  kage.api.autocmd_exec("user", { pattern = "Tick" })
end, 60000)
```

## complete example

This `init.lua` uses every part of this page. It switches to vim mode
with a space leader, adds a few mappings, logs shell calls, keeps a
user bubble color across theme switches, puts a cost and a clock in the
footer and trims the start card.

```lua
-- ~/.config/kage/init.lua
local opt, map, act = kage.opt, kage.keymap.set, kage.action
local api = kage.api

-- Options. These win over config.toml.
opt.theme = "kage-dawn"
opt.editor = "vim"
opt.input_max_lines = 12
opt.leader = " " -- set the leader before any <leader> mapping
opt.timeoutlen = 600
opt.transcript_on_exit = "last"

-- Keymaps.
map("n", "<leader>m", act.OpenModelPicker, { desc = "pick a model", group = "mine" })
map("n", "<leader>t", ":theme set kage-shadow", { desc = "dark theme", group = "mine" })
map("n", "<leader>y", act.YankFocusedBlock, { desc = "yank the focused block", group = "mine" })
map("b", "<C-d>", act.scroll(20), { desc = "scroll down a page", group = "mine" })
map("i", "<C-l>", function()
  kage.notify("hello from init.lua")
end, { desc = "say hello", group = "mine" })
kage.keymap.del("g", "<C-s>") -- give ctrl+s back

-- Autocmds. The group makes a reload replace them instead of adding more.
local g = api.augroup_create("me")

api.autocmd_create("tool_call", {
  group = g,
  pattern = "shell",
  desc = "log shell calls",
  callback = function(ev)
    kage.log("info", "shell: " .. tostring(ev.data.input.command))
  end,
})

api.autocmd_create("option_set", {
  group = g,
  pattern = "editor",
  callback = function(ev)
    kage.notify("editor: " .. ev.data.old .. " -> " .. ev.data.new)
  end,
})

-- Highlights. Kage* overrides reset on a theme switch, so set them
-- from color_scheme. Other groups persist.
api.autocmd_create("color_scheme", {
  group = g,
  callback = function()
    api.hl_set("KageUserBubble", { bg = "#1b1e2b" })
    -- hl_set replaces the whole group, so keep the theme's bg.
    local err = api.hl_get("KageToolError") or {}
    err.fg = "#ff6b6b"
    api.hl_set("KageToolError", err)
  end,
})
api.hl_set("MyCost", { link = "KageMuted" })
api.hl_set("MyClock", { fg = "cyan", bold = true })

-- Slots.
kage.ui.set_slot("footer", {
  left = { "hint" },
  right = {
    "model",
    "permission",
    "context",
    {
      events = { "message_end", "model_select" },
      hl = "MyCost",
      render = function(ctx)
        local u = ctx.usage
        if not u then
          return ""
        end
        return string.format("$%.3f", u.cost or 0)
      end,
    },
    {
      events = { "user Tick" },
      render = function()
        return { text = os.date("%H:%M") .. " ", hl = "MyClock" }
      end,
    },
  },
  sep = " \u{B7} ",
})

kage.ui.set_slot("start", {
  lines = {
    "brand",
    { text = "" },
    "model",
    "cwd",
    { text = "" },
    "sessions",
    "notices",
    { text = "Press Space then m for the model picker.", hl = "KageMuted" },
  },
})

-- Timers.
kage.timer(function()
  api.autocmd_exec("user", { pattern = "Tick" })
end, 30000)

kage.defer(function()
  kage.notify("init.lua loaded")
end, 200)
```

The options above, written in `config.toml` instead:

```toml
[ui]
theme = "kage-dawn"
editor = "vim"
input_max_lines = 12
transcript_on_exit = "last"

[keybindings]
leader = " "
timeoutlen = 600
bindings = { "f6" = "theme set kage-shadow", "<F2>" = "action:OpenModelPicker" }
```

`[keybindings] bindings` always maps in mode `g`, so keep mappings that
start with a printable leader in `init.lua`, where you can pick mode
`n`.
