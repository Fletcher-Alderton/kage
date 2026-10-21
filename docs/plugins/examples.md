# examples

The kage repo ships example plugins under `plugins/examples/`. They
double as integration-test fixtures, so they stay in sync with the
runtime. Copy whichever fits and tweak.

## tokens-per-second readout

`plugins/examples/tps.lua` reports the throughput of each run as a
toast. It adds up the output tokens of every `message_end` payload and
divides by the run's wall-clock time when `agent_end` fires.
`message_end` is one of the events gated behind the `context`
[capability](/plugins/capabilities#context); the plugin requests it at
load and you grant it with:

```toml
[plugins.capabilities]
tps = ["context"]
```

```lua
kage.request_capabilities({ "context" })

local started_at, total_output = nil, 0

kage.on("agent_start", function()
  started_at, total_output = kage.now_ms(), 0
end)

kage.on("message_end", function(ev)
  if ev.usage and ev.usage.output then
    total_output = total_output + ev.usage.output
  end
end)

kage.on("agent_end", function()
  if started_at == nil then return end
  local elapsed_ms = kage.now_ms() - started_at
  if elapsed_ms <= 0 then
    kage.notify(string.format("tps: %d output tokens (no elapsed time)", total_output))
    return
  end
  local tps = total_output * 1000 / elapsed_ms
  kage.notify(string.format("tps: %d tokens in %.2fs (%.1f tok/s)",
    total_output, elapsed_ms / 1000, tps))
  started_at = nil
  total_output = 0
end)
```

## git branch in the status bar

`plugins/examples/git-status.lua` reads `.git/HEAD` directly (the
sandbox forbids spawning `git`) and announces the branch, or the short
hash of a detached HEAD, when a run starts:

```lua
local function read_head()
  local ok, head = pcall(kage.fs.read, ".git/HEAD")
  if not ok or head == nil or #head == 0 then return nil end
  head = head:gsub("%s+$", "")
  local ref = head:match("^ref: (.+)$")
  if ref then return (ref:gsub("^refs/heads/", "")) end
  return head:sub(1, 7) .. " (detached)"
end

kage.on("agent_start", function()
  kage.notify("git: " .. (read_head() or "not a repo"))
end)
```

## blocking dialogs and a keybinding

`plugins/examples/select_demo.lua` exercises the whole `kage.ui.*`
surface plus `kage.register_keybinding`, which maps a chord in mode
`g` and returns an `off` function. Each command opens a modal and the
coroutine suspends until the user answers:

```lua
kage.register_command({
  name = "pick-color",
  description = "Pick a color via the ui.select dialog",
  handler = function()
    local color = kage.ui.select("Pick a color", { "red", "green", "blue" })
    if color == nil then return "cancelled" end
    kage.ui.notify("pick-color: " .. color)
    return "you picked " .. color
  end,
})

kage.register_keybinding({ key = "ctrl+alt+k", description = "Quick pick" },
  function()
    local color = kage.ui.select("Quick pick", { "red", "green", "blue" })
    return color or "cancelled"
  end)
```

It also registers `/confirm-delete` (`kage.ui.confirm`), `/ask-name`
(`kage.ui.input`), and `/compose-note` (`kage.ui.editor`).

## throughput in the footer

The same readout as a footer component instead of a toast. The
component recomputes only when `message_end` fires (a `context`
event, so the same grant applies), so it costs
nothing between turns. Autocmds fire in creation order, so the `kage.on`
handler updates `last` before the component reads it:

```lua
kage.request_capabilities({ "context" })

local start_ms, last = nil, ""

kage.api.hl_set("TpsReadout", { link = "KageMuted" })

kage.on("agent_start", function()
  start_ms = kage.now_ms()
end)

kage.on("message_end", function(ev)
  if not start_ms then return end
  local elapsed = (kage.now_ms() - start_ms) / 1000
  local out = (ev.usage and ev.usage.output) or 0
  last = string.format("%.1f tok/s", out / math.max(elapsed, 0.001))
end)

kage.ui.set_slot("footer", {
  left = { "hint" },
  right = {
    "model", "tokens", "context", "permission",
    { events = { "message_end" }, hl = "TpsReadout", render = function() return last end },
  },
  sep = " \u{B7} ",
})
```

The shipped default footer stacks two rows; this spec paints a single
row that keeps its main components and adds the readout at the end.
`TpsReadout` is not a `Kage*` group, so it survives theme
switches. A user who prefers another footer can replace it from
`init.lua`, which loads after every plugin.

## keys and autocmds

A plugin that shows the running shell command in the header's
widget area and adds a key to copy the last one into a toast:

```lua
local group = kage.api.augroup_create("shell-watch")
local last

kage.api.autocmd_create("tool_call", {
  group = group,
  pattern = "shell",
  callback = function(ev)
    last = tostring(ev.data.input.command)
    kage.set_status("shell", "$ " .. last)
  end,
})

kage.api.autocmd_create("agent_end", {
  group = group,
  callback = function()
    kage.clear_status("shell")
  end,
})

kage.keymap.set("g", "<F6>", function()
  kage.ui.notify(last or "no shell command yet")
end, { desc = "show the last shell command", group = "shell watch" })
```

`pattern = "shell"` limits the first autocmd to the `shell` tool. The
mapping has a `desc`, so it appears in the `?` reference under
`shell watch`.

## safer shell

Override the built-in `shell` tool to refuse destructive commands. The
override replaces the tool, so it runs the command itself through
`kage.exec`, which needs the `exec` [capability](/plugins/capabilities):

```lua
local caps = kage.request_capabilities({ "exec" })
if not caps.exec then return end

local blocked = { "rm %-rf /", "mkfs", ":(){" }

kage.override_tool({
  name = "shell",
  description = "Run a shell command, refusing destructive ones.",
  schema = {
    type = "object",
    properties = { command = { type = "string" } },
    required = { "command" },
  },
  risk = "exec",
  execute = function(input)
    local cmd = input.command or ""
    for _, pattern in ipairs(blocked) do
      if cmd:find(pattern) then
        return { is_error = true, text = "blocked: " .. pattern }
      end
    end
    local r = kage.exec({ cmd = "bash", args = { "-c", cmd } })
    return { is_error = r.code ~= 0, text = r.stdout .. r.stderr }
  end,
})
```

`kage.exec` waits until the command exits and kills it after 30
seconds, so this override has no live output. Cancelling the call stops
the wait, but unlike the built-in tool it leaves the command running
until it ends. The snippet is Unix-only: on
Windows the shell tool uses PowerShell.

## conversation and file rewind

The built-in `/undo` and `/rewind` commands (or `esc` `esc`) move the
conversation back, but they leave the working tree as it is.
`plugins/examples/rewind.lua` is the worked example for the
[capability tier](/plugins/capabilities) that adds the file half. Grant
it both capabilities:

```toml
[plugins.capabilities]
rewind = ["session_write", "exec"]
```

It snapshots tracked files with `git stash create` on every
`turn_end`, keyed by the session's last entry id:

```lua
local checkpoints, redo = {}, {} -- redo survives later turns: a prompt
                                 -- between restore and redo does not wipe it

kage.on("turn_end", function()
  if not files or not in_git_repo() then return end
  local id = last_entry_id()
  if not id then return end
  checkpoints[#checkpoints + 1] = { id = id, sha = snapshot() }
end)
```

The guard order matters: `files` is the exec capability, checked
before `in_git_repo()`, so a plugin without the grant returns before
any command runs; the git check follows, because it is the one that
needs the grant.

`/checkpoint-restore [at]` brings the files along after a rewind: run
the built-in `/undo` or `/rewind` first, then restore the tracked
changes to the checkpoint of the turn you landed on. With an
entry-id prefix it restores directly; without one it offers a picker
built from `kage.session.entries()`. The state you leave behind goes
to the redo stack, and `/redo` re-applies it:

```lua
local cp = checkpoint_for(at)       -- newest checkpoint at or before `at`
local pre = snapshot()              -- keep the state you leave behind
if pre then redo[#redo + 1] = pre end
restore(cp.sha)                     -- kage.exec git checkout
```

It degrades honestly. Without `session_write` the plugin disables
itself. Without `exec` it tracks nothing and the commands say so. File
snapshots use `git stash create`, so they cover tracked changes only,
not untracked files.

## testing without the TUI

`crates/kage-plugin/tests/examples.rs` loads every file under
`plugins/examples/` into a fresh `PluginRuntime` and drives it without
the TUI. Bridged handlers (commands and keybindings) are stepped with
`bridge_call`, `bridge_resume` and `bridge_cancel`, which is how the
dialog round-trips are asserted. Follow the same pattern for your own
plugin.
