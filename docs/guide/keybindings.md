# keybindings

This page covers the keys kage's TUI listens for. Inside the TUI,
press `?` on an empty prompt (or run `/help`) for the same reference
as an overlay.

The TUI supports two editor modes: **modeless** (the default) and
**vim** (`[ui] editor = "vim"` in `config.toml`, or
`kage.opt.editor = "vim"` in `init.lua`). Both share the same
buffer navigation keys. Every key on this page that is not part of
the editor grammar can be remapped. See
[remapping keys](#remapping-keys).

## buffer navigation (works in both modes)

These keys scroll and navigate the conversation buffer from inside
Insert mode (vim) or the always-editing state (modeless). No need to
switch modes or panes.

| Key              | Effect                                       |
| ---------------- | -------------------------------------------- |
| `ctrl+down`      | Scroll the buffer down 1 line                |
| `ctrl+up`        | Scroll the buffer up 1 line                  |
| `ctrl+home`      | Snap to the top of the conversation         |
| `ctrl+end`       | Snap to the bottom, re-arm auto-follow      |
| `ctrl+p`         | Open the model picker                       |
| `ctrl+s`         | Open the session picker                     |
| `ctrl+t`         | Open the agents overlay                     |
| `ctrl+f`         | Search the conversation (Insert mode or modeless) |
| `f3`             | Open the message jump picker (type to filter, `enter` jumps) |
| `alt+p` / `alt+n` | Jump to the previous / next block          |
| `ctrl+n`         | Jump to the next block                      |
| `ctrl+o`         | Toggle fold on the focused block            |
| `shift+tab`      | Toggle [plan mode](/guide/plan-mode)        |
| `alt+t`          | Cycle the thinking level                    |
| `ctrl+v`         | Attach an image from the clipboard          |
| `ctrl+g`         | Edit the prompt draft in `$VISUAL`/`$EDITOR` |
| `!`              | Shell escape: `!` on an empty prompt, then `enter` runs the line |
| `?`              | Open the keyboard reference (Normal mode, or an empty modeless prompt)   |

`ctrl+o` also expands a collapsed bracketed paste if one is present
in the input. When no paste is collapsed, it toggles the fold.

Terminals that do not send `shift+tab` can run `/plan` instead, or
rebind `kage.action.TogglePlanMode` to another key.

## vim modes

| Key             | From    | Effect                              |
| --------------- | ------- | ----------------------------------- |
| `esc`           | Insert, Visual | Return to Normal mode        |
| `i`             | Normal  | Enter Insert mode                   |
| `v`             | Normal  | Enter Visual mode                   |
| `ctrl+w`        | Normal  | Cycle focused pane (input / buffer) |
| `?`             | Normal  | Open the keyboard reference         |
| `:`             | Normal  | Open the `:` command line           |
| `ctrl+q`        | any     | Quit                                |
| `ctrl+c`        | any     | Interrupt the run, else arm quit |

In vim Insert mode, `ctrl+w` kills the previous word instead (see
[input editing](#input-editing)).

## modeless mode

In modeless mode the editor is always in an insert-like state.
`esc` never enters Normal. It interrupts the run, and idle a second
`esc` opens the rewind picker (see [esc and ctrl+c](#esc-and-ctrl-c)).
All Emacs/readline keys and
the buffer navigation keys above work without any mode switching.

| Key  | Effect                          |
| ---- | ------------------------------- |
| `enter` | Send the prompt, or steer it into the running turn |
| `tab` | Queue the prompt until the running turn ends |
| `shift+enter` / `alt+enter` | Insert a newline |
| `esc` | Interrupt the run, else clear the search highlight; `esc` `esc` opens the rewind picker (see [esc and ctrl+c](#esc-and-ctrl-c)) |
| `ctrl+c` | Interrupt the run, else arm quit |
| `pageup` / `pagedown` | Scroll the conversation buffer 10 lines |
| `ctrl+w` | Kill the previous word |
| `ctrl+g` | Edit the prompt draft in `$VISUAL`/`$EDITOR` |
| `shift+tab` | Toggle [plan mode](/guide/plan-mode) |
| `alt+t` | Cycle the thinking level |
| `!`   | Switch to shell mode (empty prompt only) |
| `?`   | Open the keyboard reference (empty prompt only) |
| `ctrl+q` | Quit |

A draft starting with `/` runs as a command on `enter` instead of
sending a prompt. The `?` (keys) and `!` (shell escape)
prefixes key off an empty prompt, so every surface stays one
keystroke away without a mode switch. With text in the prompt they
are typed as literal characters. In shell mode the placeholder reads
`Run a shell command (backspace leaves shell mode)`. `enter` runs the
line with `shell` and shows its output live, and `esc` stops the
command while it runs. `backspace` or `esc` on the empty shell prompt
leaves shell mode. See
[running shell commands](/guide/quickstart#running-shell-commands).

## esc and ctrl+c

`esc` in modeless mode and `ctrl+c` in every mode step through the
same escalation. Neither key touches the draft: interrupting leaves
an unsent draft in the editor, and submitting a prompt is what puts
it in the history that `up` walks.

1. While kage works, they interrupt the run. The
   conversation shows `Interrupted`.
2. Idle with a search highlight active, `esc` clears it
   (the footer then reads `esc to clear the search`). A
   second `esc` within 0.6 seconds opens the
   [rewind picker](/guide/commands#commands): one row per prompt the
   session's branch tree holds, oldest first, branches indented one
   level per rewind, opened on the active head; picking one rewinds
   the conversation to that prompt, which stays while its reply and
   everything after it are abandoned (`/rewind` does the same; `/undo`
   is the variant that takes the last prompt back into the editor).
   The first `esc` arms it, and
   the footer reads `esc again to rewind`. `ctrl+c` arms quit instead.
   The footer reads `ctrl+c again to quit`, and a second `ctrl+c`
   within 2 seconds quits.

In an agent view (see [agents](#agents)), the last two steps change:
`esc` goes back one level and never stops the agent, and `ctrl+c`
stops the agent while it runs, else goes back. Quit is only armed
from the main view.

An open popup, such as the completion popup,
takes `esc` first. In vim mode `esc` keeps its vim meaning. While an
overlay is open (a picker, a dialog, the `:` line, the search line or
the approval panel) and kage works, `ctrl+c` only interrupts the run
and leaves the draft and the overlay alone. When kage is idle,
`ctrl+c` closes the overlay like `esc`.

## sending while kage works

The prompt stays editable during a run, and there are two ways to send
what you type:

- `enter` steers. The prompt joins the running turn at the next turn
  boundary, after the current tool call.
- `tab` queues. The prompt waits and starts a new run once the current
  one ends. Idle, `tab` does nothing, so a stray press never sends a
  prompt.

Prompts that were sent but not delivered yet show above the input,
each with `after the current tool call` or `when this run ends`. Up to
three rows show, then `+N more`. A row disappears when kage delivers
its prompt. `up` on the draft's top row takes the newest row back out
of the queue and puts its text and images into the editor, so a typo
in a sent prompt is fixable before it lands. Pressing it again takes
the next one back, and once nothing is pending `up` walks the prompt
history as usual. The footer says so while something is pending. A
prompt that mentions an MCP resource (`@server:uri`) or starts with
an MCP prompt command (`/server:prompt`) still waits for the run to
end, because kage expands those only when a run starts (see
[mcp](/guide/mcp#resources-and-mentions)).

While kage works, the working row above the input shows what it is
doing and for how long, such as
`Running cargo test (14s, esc to interrupt)`. While agents run it
counts them instead, such as `Waiting for 3 agents (41s, esc to
interrupt)`.

In an agent view, `enter` and `tab` send to that agent instead of the
main session.

## approvals

When a tool call needs your approval (see
[permissions](/guide/permissions)), a panel replaces the input box.
Its title names the action, such as `Run this command?` or
`Edit src/lib.rs?`, and it shows the command, the diff or the
arguments. Your draft is kept and comes back once no approval is left.

| Key                   | Effect                                          |
| --------------------- | ----------------------------------------------- |
| `1` / `y`             | Yes, run this call                              |
| `2` / `s`             | Yes, and allow the tool for the rest of the session |
| `3` / `a`             | Yes, and always allow the tool (saved to `config.toml`) |
| `4` / `n` / `esc`     | No                                              |
| `5` / `t`             | No, and tell kage what to do instead            |
| `up` / `down`         | Move the selection                              |
| `enter`               | Confirm the selection. `Yes` starts selected.   |
| `ctrl+c`              | Interrupt the run, which denies the call        |

Keys pressed in the first 400 ms after a panel opens are dropped, so
typing meant for the prompt cannot answer it. Option 5 opens a
one-line field: `enter` denies the call and sends your text to the
model, and `esc` goes back to the options. When several calls wait,
the title shows `1 of 3`.

Requests from [agents](/guide/agents#approvals-from-agents) join the
same queue. The title then starts with the agent's name, and option 5
sends your text to that agent.

## agents

These keys drive [agents](/guide/agents#agents-in-the-tui). Click a
row of the pinned agent list above the input, or pick one in the
agents overlay, to open that agent. The whole view then shows it,
with a breadcrumb in the header.

| Key | Where | Effect |
| --- | --- | --- |
| `ctrl+t` | anywhere | Open the agents overlay (also `/agents`) |
| `enter` | agent view | Steer the running agent; a finished agent is read-only |
| `tab` | agent view | Queue the prompt until the agent's run ends |
| `esc` | agent view, empty prompt | Go back one level, to the parent agent or the main view |
| `ctrl+c` | agent view, empty prompt | Stop the agent while it runs, else go back |
| `up` / `down`, `k` / `j` | overlay | Move the selection |
| `home` / `end` | overlay | Jump to the first / last row |
| `enter` | overlay | Open the selected agent, or the main view from the `kage` row |
| `x` | overlay | Stop the selected agent and the agents under it |
| `X` | overlay | Stop every live agent of the session |
| `esc` | overlay | Close the overlay |

In vim mode, `esc` in Insert mode still enters Normal mode, and `esc`
in Normal mode goes back from an agent view.

## search

`ctrl+f` (modeless mode and vim Insert) and `/` in vim Normal open the
search line on the bottom row. It always opens empty. Typing searches
as you go and shows the match count, such as `match 2/5`. While the
line is open, `up` and `down` walk the matches of what you typed.
`enter` closes the line and keeps the pattern, so `n` and `N` in vim
Normal mode walk it later. Walking past the last match wraps to the
first, and the other way round. `esc` closes the line and restores the
previous pattern and view.

A kept pattern stays highlighted. `/noh` clears it, and so does `esc`
on an idle, empty modeless prompt.

## pickers and overlays

The model picker (`ctrl+p`), the session picker (`ctrl+s`), the jump
picker (`f3`) and the other lists share these keys. Type to filter.

| Key | Effect |
| --- | --- |
| `up` / `down` | Move the selection |
| `pageup` / `pagedown`, `home` / `end` | Move by a page, or to the first / last row |
| `enter` | Pick the selected row |
| `esc` / `ctrl+c` | Close the picker |
| `backspace` | Delete the last filter character |
| `ctrl+u` / `ctrl+w` | Clear the filter / delete its last word |
| `ctrl+a` | Session picker only: switch between this directory and all directories |

While kage works, `ctrl+c` interrupts the run instead and the picker
stays open. The keyboard reference (`?`) scrolls with `up`/`down`,
`j`/`k`, `pageup`/`pagedown` and `home`/`end`, and `esc`, `enter`, `q`
or `ctrl+c` close it. While any picker, dialog or popup is open, the
left side of the footer shows its keys, such as `enter to pick` and
`esc to close`.

## mouse

With mouse capture on (`/mouse on`, the default), the wheel scrolls
the conversation. A click on a block focuses it, and a click on a
block's first row folds or unfolds it. A click on the
`new output below` mark, shown when output streams in while you are
scrolled up, snaps back to the bottom. Dragging selects text, and
dragging past the top or bottom edge scrolls. Releasing the button
copies the selection to the clipboard and shows
`copied N characters`. A right-click opens a menu for the block under
the pointer. `/mouse off` hands selection back to the terminal.

## command pathways

A draft starting with `/` is a command. While you type it, a popup
above the input box lists matching commands with their argument
hints, and argument positions offer their values. `enter` or `tab`
accepts the highlighted completion; with the popup closed, `enter`
runs the command. This works in modeless mode and in vim Insert mode.

In vim mode there is also the `:` ex line on the bottom row, opened
from Normal mode. It shares the prompt's command registry, parser,
completion, and dispatch, so `:model anthropic/claude-sonnet-4-6` and
`/model anthropic/claude-sonnet-4-6` have identical effect. `/model`
without an id opens the model picker, like `ctrl+p`.

| Key   | From                       | Effect                          |
| ----- | -------------------------- | ------------------------------- |
| `/name enter` | prompt            | Run the command                 |
| `:`   | vim Normal                 | Open the colon command line     |
| `/`   | vim Normal                 | Begin a buffer search           |

## command line autocomplete

`tab` completion matches vim's `wildmode=longest:full,full`:

| Key              | Effect                                              |
| ---------------- | --------------------------------------------------- |
| `tab`            | Extend to the longest common prefix, then cycle     |
| `shift+tab`      | Cycle in reverse                                    |
| `down` / `up`    | Cycle through completions when the popup is open    |
| `enter`          | Submit. A validation error keeps the line open.     |
| `esc`            | Dismiss the popup, else cancel the line             |
| `backspace`      | Delete the previous character, or cancel an empty line |
| `left` / `right` | Move the cursor                                     |
| `home` / `end`   | Jump to start / end                                 |
| `ctrl+c`         | Interrupt the run in flight (the line stays open), else cancel the line |

Completions are recomputed on every edit. In the `:` line the popup
appears only after the first `tab` step that does more than insert the
longest common prefix, so a single match completes and closes in one
keystroke. On the prompt, a `/` draft shows its list at once,
`up` and `down` move without a `tab` first, and `enter` accepts the
highlighted row before it sends.

## validation

The `:` line keeps the line open on an invalid command and shows an
inline error next to it. A `/` draft on the prompt restores the draft
and shows the reason as a toast. Examples:

- `/mouse maybe` shows ``argument `state` must be one of: on, off, toggle (got `maybe`)``
- `/theme set` shows ``missing required argument `name` ``, adds a
  space after the command and lists the values it accepts
- `/quut` shows `unknown command: quut (did you mean /quit?)`

Editing the line clears the error.

## vim normal-mode keys (buffer pane)

These keys work when the buffer pane is focused in vim Normal mode
(press `ctrl+w` in Normal mode to switch panes, or press `esc` from
Insert).

| Key       | Effect                                       |
| --------- | -------------------------------------------- |
| `j` / `k` | Scroll buffer 1 line                         |
| `G`       | Snap to bottom and re-arm auto-follow        |
| `gg`      | Snap to top                                  |
| `[` / `]` | Focus previous / next block                  |
| `zo` / `zc` | Toggle fold on focused block               |
| `zM`      | Fold all blocks                              |
| `zR`      | Unfold all blocks                            |
| `n` / `N` | Jump to next / previous search match         |
| `y`       | Yank the selection, else the focused block   |
| `Y`       | Yank focused block                           |
| `v`       | Enter visual (cell selection)                |
| `pageup` / `pagedown` | Scroll buffer up / down 10 lines |

The thinking level the next run sends shows as `thinking <level>` on
the right of the input's top rule, marked `(auto)` when you have not
chosen one, and hidden when off.

## input editing

The prompt input is a readline / Emacs-style line editor. These keys
work in both vim Insert mode and modeless mode.

| Key            | Effect                                            |
| -------------- | ------------------------------------------------- |
| `enter`        | Send the prompt (steer it during a run)           |
| `tab`          | Queue the prompt until the run ends               |
| `shift+enter`  | Insert a newline (`alt+enter` also works)         |
| `up` / `down`  | Move between lines, then walk the prompt history  |
| `ctrl+a` / `ctrl+e` | Start / end of the current line              |
| `ctrl+w`       | Kill the word before the cursor (in vim Normal mode it cycles panes instead) |
| `ctrl+u`       | Kill to start of line                              |
| `ctrl+k`       | Kill to end of line                                |
| `alt+backspace` | Kill the previous word                           |
| `alt+d`        | Kill the next word                                 |
| `alt+b` / `alt+f` | Move backward / forward one word               |
| `ctrl+y`       | Yank (paste) the most recent kill                  |
| `ctrl+/`       | Undo the last edit (also `ctrl+_`)                 |
| `ctrl+o`       | Toggle fold (or expand a collapsed paste)          |
| `ctrl+s`       | Open session picker                                |
| `ctrl+p`       | Open model picker                                  |
| `ctrl+g`       | Edit the draft in `$VISUAL`/`$EDITOR`              |

`ctrl+w`, `ctrl+u`, `ctrl+k`, `alt+backspace`, and `alt+d` feed a
kill ring, and `ctrl+y` yanks the most recent entry. A bracketed
paste of 10 or more lines collapses to a `[paste #N: M lines]`
placeholder so it does not flood the input. A paste of more than 1000
characters on fewer lines collapses to `[paste #N: M chars]`. The full
text is still sent on submit, and `ctrl+o` expands it inline if you
want to edit it first.

## file path completion

Type `@` in the prompt to complete a path under the working directory.
The popup lists files and directories that `.gitignore` and
`.kageignore` do not exclude, skipping hidden ones, and narrows as you
type.

| Key | Effect |
| --- | --- |
| `up` / `down`, `ctrl+p` / `ctrl+n` | Move the selection |
| `tab` / `enter` | Insert the highlighted path |
| `esc` | Close the popup |

Accepting inserts the path as text, such as `@src/main.rs`, and a
directory keeps the popup open for the next segment. When the text
already matches the highlighted path, `enter` sends the prompt. The
file is not attached: the model sees the literal `@src/main.rs` and
reads the file with its tools when it needs it. Only MCP resource
mentions (`@server:uri`) are expanded (see
[mcp](/guide/mcp#resources-and-mentions)).

## vim normal-mode keys (input pane)

When the input pane is focused in Normal mode, full vim motions and
operators are available for editing the prompt text.

| Key           | Effect                                      |
| ------------- | -------------------------------------------- |
| `h` / `l`     | Move cursor left / right                    |
| `j` / `k`     | Move cursor down / up (multi-line input)    |
| `w` / `b` / `e` | Word motions                              |
| `0` / `$` / `^` | Line start / end / first non-blank        |
| `G` / `gg`    | End / start of text                         |
| `x` / `X`     | Delete char at / before cursor              |
| `r{ch}`       | Replace char at cursor                      |
| `D` / `C`     | Delete / change to end of line              |
| `dd`          | Delete current line                         |
| `cc`          | Change current line (delete + insert)       |
| `yy`          | Yank current line                           |
| `dw` / `cw` / `yw` | Delete / change / yank word           |
| `p` / `P`     | Paste after / before cursor                 |
| `u`           | Undo                                        |
| `ctrl+r`      | Redo                                        |
| `v`           | Visual select (char-wise)                   |
| `i` / `a`     | Insert before / after cursor                |
| `I` / `A`     | Insert at line start / end                  |
| `o` / `O`     | Open line below / above                     |
| `3dw`         | Delete 3 words (count prefix)               |

## remapping keys

Most keys above are mappings in one keymap table, which kage fills
from its embedded defaults at startup. Plugins, `config.toml` and
`init.lua` add to the same table in that order, and the last mapping
set for a key wins. So `init.lua` beats `config.toml`, which beats
plugins, which beat the defaults.

`init.lua` is the full interface: modes, key sequences, a leader,
Lua functions and deleting defaults. See
[lua config](/guide/lua-config#keymaps). The `[keybindings]` table in
`config.toml` covers the common case.

The editor grammar is not in the table: vim motions, operators,
counts, registers, undo and redo, readline edits, `enter`, `esc`,
history `up` and `down`, `ctrl+o` in insert mode, `ctrl+g` and the
modeless `/`, `!` and `?` prefixes. A mapping on one of these keys shadows it, but
cannot remove it.

### `[keybindings]` in config.toml

```toml
[keybindings]
# the key <leader> expands to: one key, default a backslash.
leader = "<C-x>"
# ms a mapping that is also the start of a longer one waits for
# more keys (0 to 5000). Default 1000.
timeoutlen = 600
bindings = { "f6" = "theme set kage-dawn", "ctrl+l" = "action:OpenModelPicker", "<leader>s" = "settings", "<leader>q" = "quit" }
```

With this table, `ctrl+x` then `s` opens the settings dialog and
`ctrl+x` then `q` quits.

Mappings go in the `bindings` table, written inline as above or as a
`[keybindings.bindings]` table. A key written directly under
`[keybindings]`, as older versions of this page showed, is reported as
an error at startup instead of being dropped.

Every binding maps in mode `g`, which covers every editing state. A
key is either the chord form (`ctrl+shift+x`, `alt+p`, `f5`) or Vim
notation (`<C-t>`, `<F2>`, `<leader>s`, `gs`). `<leader>` expands with
the `leader` value from the same table. A key sequence waits up to
`timeoutlen` for the next key. Prefer modified keys. A bare letter, or
a leader that is a letter or punctuation, catches that key while you
type in the prompt.

The value is a command line, run through the same executor as the
`:` line, so anything `/` can do is bindable, including `quit`
and plugin commands. A value starting with `action:` names a built-in
action instead and is never run as a command. A key or action that
does not parse is reported inline at startup, never silently dropped.

These action names work after `action:`:

| Group      | Name                   | Effect                            |
| ---------- | ---------------------- | --------------------------------- |
| navigation | `ScrollToTop`          | snap the buffer to the top        |
| navigation | `ScrollToBottom`       | snap the buffer to the bottom     |
| navigation | `FocusPrev`            | focus the previous block          |
| navigation | `FocusNext`            | focus the next block              |
| navigation | `CyclePane`            | cycle pane focus (input / buffer) |
| overlays   | `OpenModelPicker`      | open the model picker             |
| overlays   | `OpenSessionPicker`    | open the session picker           |
| overlays   | `OpenJumpPicker`       | open the message jump picker      |
| overlays   | `OpenHelp`             | open the keyboard reference       |
| overlays   | `OpenAgents`           | open the agents overlay           |
| folds      | `ToggleFold`           | toggle the focused block's fold   |
| folds      | `UnfoldAll`            | open every fold                   |
| folds      | `FoldAll`              | close every fold                  |
| search     | `BeginSearch`          | open the `/` search line          |
| search     | `SearchNext`           | jump to the next match            |
| search     | `SearchPrev`           | jump to the previous match        |
| misc       | `BeginCommand`         | open the `:` command line         |
| misc       | `Cancel`               | cancel the in-flight turn         |
| misc       | `Yank`                 | copy the active selection         |
| misc       | `YankFocusedBlock`     | copy the focused block            |
| misc       | `ClearSelection`       | drop the active selection         |
| misc       | `EnterVisual`          | start a visual selection          |
| misc       | `AttachClipboardImage` | attach an image from the clipboard |
| misc       | `CycleThinkingLevel`   | step the thinking level           |
| misc       | `TogglePlanMode`       | turn [plan mode](/guide/plan-mode) on or off |
| misc       | `QueuePrompt`          | queue the prompt until the run ends (does nothing while idle) |
| misc       | `RecallPrompt`         | pull the newest pending prompt back into the editor to edit (does nothing while idle; `up` does this by default) |

Scrolling by a line count needs an argument, so it is only available
from Lua as `kage.action.scroll(n)`.

`CycleThinkingLevel` steps the thinking level (also `alt+t`),
visiting only the levels the model accepts. The level a new TUI
session starts on comes from `[ui] thinking_level` (one of `off`,
`minimal`, `low`, `medium`, `high`, `xhigh`, `max`). Left unset, it is
automatic: high, or the nearest level the model accepts. The cycle
still overrides it per session. See
[thinking](/guide/providers#thinking).

### quit and cancel hatches

`ctrl+q` quits and `ctrl+c` escalates (see
[esc and ctrl+c](#esc-and-ctrl-c)) from anywhere, even a stuck
overlay. They yield **only** to a mapping from `config.toml` or
`init.lua` on the same key. Then your mapping wins, and quit stays
reachable through whatever key you mapped `quit` to. A plugin mapping
on either key never fires and logs a warning.

### listing mappings

Run `:keybindings` (alias `:keys`) to list the table per mode, with
each mapping's action or command and its owner (`defaults`, a plugin
name, `config.toml` or `init.lua`), followed by the editor grammar
keys and the two hatches. The `?` reference is built from the same
table: it shows every mapping with a description.

## plugin keybindings

Plugins bind keys with
[`kage.register_keybinding`](/plugins/api#keybindings), which maps a
chord in mode `g`, or with `kage.keymap.set`. Plugins load after the
defaults, so a plugin mapping replaces a default one on the same key.
`config.toml` and `init.lua` load after plugins, so your mappings
replace plugin ones. No mapping applies while a modal layer is open.
