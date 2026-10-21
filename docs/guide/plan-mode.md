# plan mode

Plan mode makes kage think before it touches anything. While it is on,
the agent investigates the code, then presents a plan for you to review.
Nothing changes until you approve the plan.

## turning it on

- `shift+tab` toggles plan mode in any editing state (terminals that
  do not send it can use `/plan`, or rebind `kage.action.TogglePlanMode`).
- `/plan on` and `/plan off` set it, and a bare `/plan` toggles it.
- `/plan <task>` turns plan mode on and sends the task as a prompt,
  for example `/plan move the retry logic into its own module`. It
  refuses while a run is in flight.

The footer shows `plan` while the mode is on, and the empty prompt reads
"Describe what to plan". Each switch adds a short note to the
conversation once, so the model knows which mode it is in.

## what the agent may do

| Tools | In plan mode |
| --- | --- |
| `read`, `grep`, `find`, `ls`, `web_search`, `web_fetch`, `todo_list` | run under the normal permission rules |
| `ask_user_question` | asks you, as outside plan mode (see [questions](/guide/questions)) |
| `write`, `edit`, `generate_image` | refused, with a message that names plan mode |
| `shell`, `agent`, `swarm`, `send_message`, MCP tools | always ask, even in `allow` mode and even when approved for the session |

Asking before commands lets you approve a read-only command such as
`git log` while refusing anything else. A `deny` rule in
`[permissions]` and `/permission deny` still deny. Tools from plugins
follow their declared risk, and a tool of unknown risk asks.

## reviewing the plan

When the agent understands the task, it calls `exit_plan` with the whole
plan in Markdown. The conversation shows the plan as a card titled with
its first heading, marked `awaiting review`, and the approval panel asks
whether to build it:

| Key | Answer |
| --- | --- |
| `1` or `y` | Approve. Plan mode turns off and the agent carries out the plan in the same run. |
| `2` or `r` | Revise. Type what should change and press `enter`. The run ends, plan mode stays on, and the agent presents a new plan. |
| `3` or `n` | Reject. The run ends and plan mode turns off. |
| `esc` | Keep planning. The run ends and plan mode stays on, so your next prompt goes on with the plan. |

Approving never adds a session or config permission: the next plan asks
again.

## across sessions

Plan mode is recorded in the session file. Resuming a session restores
it without adding the note again. `/new` starts with plan mode off.

## from Lua

The footer component is `plan`, and its color is the `KagePlan`
highlight group (see [themes](/guide/themes)). `kage.action.TogglePlanMode`
is the action `shift+tab` runs, so a config can bind it elsewhere:

```lua
kage.keymap.set("g", "<F4>", kage.action.TogglePlanMode, { desc = "plan mode" })
```
