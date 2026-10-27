# commands

Commands are typed into the prompt with a leading `/` and run when
you press `enter`, in place of sending a prompt. `/swarm review the
crates` runs the `swarm` command; text with no leading `/` goes to
the model as usual.

## command completion

While you type a command, a popup above the input lists the matching
names with their argument hints and descriptions. `up` / `down` move
the selection, `tab` or `enter` accepts the highlighted one, and with
the popup closed `enter` runs the line. A submitted command is kept
in the prompt history, so `up` recalls it like any other draft.

Invalid input on submit restores the draft and shows the reason as a
toast. For example, `/quut` shows `unknown command: quut (did you
mean /quit?)`.

Commands that answer with text, such as `/theme list`,
`/permission` without a mode, `/keybindings` and `/events`, write it
into the conversation as a block. Errors from a command that ran
show there too.

::: tip vim mode
With `editor = "vim"`, the same commands are also available from the
ex line: press `:` in Normal mode to open it on the bottom row. The
`:` line and the `/` prompt share one parser, one autocomplete and
one set of handlers, so `:model <id>` and `/model <id>` do the same
thing.
:::

## built-in commands

The table lists every built-in command.

| Command                   | What it does                                    |
| ------------------------- | ----------------------------------------------- |
| `/model [id]`             | Switch the active model (`provider/model`). Without an id, open the model picker. |
| `/usage` | Show the session's token usage (input, output, cache, cost) and a context window bar as a block in the conversation |
| `/new`                    | Start a fresh empty session, keeping the current model |
| `/compact`                | Run a compaction pass right now                 |
| `/agents`                 | Open the agents overlay: every agent of the session, live or finished (see [agents](/guide/agents#the-agents-overlay)) |
| `/plan [on\|off\|<task>]` | Turn plan mode on or off (a bare `/plan` toggles it), or plan one task now (see [plan mode](/guide/plan-mode)) |
| `/todo_list` / `/todos` | Open every task of the session's todo list in a scroll-only modal |
| `/goal <goal>\|clear` | Set what done looks like. Setting one sends it to the agent as its own turn, right away when idle or after the one running. After each completed turn the model judges the goal against the work since the goal was set: the replies, the tool calls and what they returned. While it is not met, kage says what is missing and keeps working toward it on its own, up to 8 turns in a row, and says so when it is met; later turns are checked too, and a goal that slips is picked back up. A check that fails or gives no clear answer stops the loop and says so. Your own prompts reset the count. `/goal clear` drops it |
| `/swarm [on\|off\|<task>]` | Turn swarm mode on or off, or hand one task to a swarm now (see [swarms](/guide/agents#the-swarm-command)) |
| `/permission [mode]` / `/perm` | Without a mode, show the session permission mode. `allow`, `ask` or `deny` override the configured rules for this session (`allow` runs everything; configured denies still deny), and `default` returns to them (see [permissions](/guide/permissions)) |
| `/login [provider]`       | Add or update a provider credential: suspends the TUI, runs the interactive login, then refreshes the model list in place |
| `/mcp`                    | Open the MCP servers picker: status per server, `enter` restarts a server or logs in to one that needs it (see [mcp](/guide/mcp#the-mcp-picker)) |
| `/mcp restart <server>`   | Restart an MCP server: now when idle, else when the next run starts |
| `/mcp login <server>`     | Log in to a remote MCP server with OAuth: suspends the TUI, runs the browser login, then reconnects the server |
| `/settings`               | Open the settings dialog, which lists every option (see [settings](#settings)) |
| `/help`                   | Open the keyboard reference overlay             |
| `/theme list`             | List bundled and user themes, `*` marking the active one |
| `/theme set <name>`       | Switch to a bundled or user theme for this session |
| `/theme current`          | Show the active theme name                      |
| `/export [file]`          | Write the session transcript to a Markdown file (defaults to `<session-id-prefix>.md` in the working directory) |
| `/image [open]`           | Show the newest image the model or a tool produced in the system image viewer |
| `/image save [file]`      | Write the newest image the model or a tool produced into the working directory (defaults to `generated/<date>-<time>.<ext>`). An existing file is never replaced: the name gets `-2`, `-3`, and so on |
| `/media [open]`           | Show or play the newest image or audio clip the model or a tool produced, with the system viewer or player |
| `/media save [file]`      | Write the newest image or audio clip the model or a tool produced into the working directory (defaults to `generated/<date>-<time>.<ext>`). An existing file is never replaced: the name gets `-2`, `-3`, and so on |
| `/tree`                   | Browse the session fork forest (resume, fork, delete). Agent sessions sit under their parent with an `agent: ` label. |
| `/clone`                  | Duplicate the session to a new id and continue in the clone |
| `/undo` | Drop the last exchange: the session branches in place, continuing from just before its last prompt, and the removed prompt goes back into the editor to edit and resend. Earlier turns stay in the file as an abandoned branch, so `/rewind` can still reach them. Refused while a run is in flight |
| `/rewind` | Open the rewind picker: one row per prompt the session's branch tree holds, oldest first, branches indented one level per rewind, opened on the active head. Picking one keeps that prompt and abandons its reply and everything after it; the session continues from the pick. Same rules as `/undo` |
| `/keybindings` / `/keys`  | List active key mappings per mode with their owners |
| `/attach [path]` / `/img` | Attach a png, jpeg, gif or webp image to the next prompt: a file `path`, or the OS clipboard image when no path is given. A toast warns when the model does not accept images. To include a text file, mention it as `@path` in the prompt instead |
| `/mouse [on\|off\|toggle]` | Control terminal mouse capture                |
| `/fold all`               | Fold every block                                |
| `/unfold all`             | Unfold every block                              |
| `/clear`                  | Clear the conversation buffer                   |
| `/noh`                    | Clear search highlighting                       |
| `/events`                 | List events plugins can hook with `kage.on`     |
| `/cancel`                 | Cancel the current turn                         |
| `/reload`                 | Reload `init.lua` and plugins now, without waiting for a file change. A toast reports the result, and each error shows in the conversation. |
| `/quit` / `/q`            | Exit the TUI                                    |

## settings

`/settings` opens one list with a row per option: its name, its
value, and `set in init.lua` when Lua set it last. The selected
option's description shows below the list.

- `up` / `down` move the selection.
- `left` / `right` or `space` cycle a choice, a boolean or the theme,
  and step a number. Typing digits edits a number. On the `leader`
  row, press the key you want.
- Every edit applies live as you make it.
- `enter` or `ctrl+s` saves the edits to your user `config.toml`,
  changing only those keys. Comments and every other line stay as
  written.
- `esc` or `ctrl+c` restores the values the dialog opened with.

The dialog does not pick models or edit key bindings. Use `ctrl+p` or
`/model` for the model and `?` or `/keybindings` for bindings. It
does not write `default_model` either. Set that under `[provider]` in
`config.toml`.

## mcp prompt commands

Each prompt of a connected MCP server is a command named
`/<server>:<prompt>`, such as `/everything:complex_prompt`. Completion
tags it `[mcp]` and shows its arguments as the hint, `<name>` for a
required one and `[name]` for an optional one. Arguments are
whitespace separated in declared order, and the last one takes the
rest of the line. The prompt's messages become your prompt. A prompt
whose name a built-in or plugin command takes is not listed. See
[mcp](/guide/mcp#prompts-as-commands).

## plugin commands

Plugins register their own commands via `kage.register_command`. They
appear in command completion tagged `[plugin]` and accept the same
argument grammar as built-ins. See
[plugins / lua api](/plugins/api#commands).
