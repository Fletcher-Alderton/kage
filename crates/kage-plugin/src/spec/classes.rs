//! Generated-stub class (`---@class`) record declarations.

use super::{Class, Field};

pub(super) const CLASSES: &[Class] = &[
    Class {
        name: "kage.ToolResult",
        doc: &["One result a tool may return instead of a bare string."],
        fields: &[
            Field {
                name: "text",
                ty: "string",
                doc: "Output text shown to the agent.",
            },
            Field {
                name: "is_error?",
                ty: "boolean",
                doc: "Mark the call as failed.",
            },
            Field {
                name: "structured?",
                ty: "table",
                doc: "Machine-readable detail.",
            },
            Field {
                name: "media?",
                ty: "kage.ToolMedia[]",
                doc: "Images the tool produced, shown with the call and kept in the session.",
            },
        ],
    },
    Class {
        name: "kage.ToolMedia",
        doc: &["One image a tool produced."],
        fields: &[
            Field {
                name: "mime",
                ty: "string",
                doc: "MIME type, such as `image/png`.",
            },
            Field {
                name: "data",
                ty: "string",
                doc: "The bytes, base64-encoded.",
            },
        ],
    },
    Class {
        name: "kage.ToolSpec",
        doc: &["Spec passed to `kage.register_tool` / `kage.override_tool`."],
        fields: &[
            Field {
                name: "name",
                ty: "string",
                doc: "Tool name the agent calls.",
            },
            Field {
                name: "description",
                ty: "string",
                doc: "One-line description for the model.",
            },
            Field {
                name: "schema",
                ty: "table",
                doc: "JSON schema for the input object.",
            },
            Field {
                name: "risk?",
                ty: "kage.ToolRisk",
                doc: "Permission tier; defaults to read.",
            },
            Field {
                name: "parallel?",
                ty: "boolean",
                doc: "Let a batch of calls to this tool run at once; defaults to false.",
            },
            Field {
                name: "execute",
                ty: "fun(input: table, ctx: { update: fun(text: string, structured?: table) }): string|kage.ToolResult|boolean|number|nil",
                doc: "Tool body. `ctx.update` reports progress while it runs.",
            },
        ],
    },
    Class {
        name: "kage.CommandArg",
        doc: &["One declared command argument."],
        fields: &[
            Field {
                name: "name",
                ty: "string",
                doc: "Becomes `args.<name>`.",
            },
            Field {
                name: "kind",
                ty: "kage.ArgKind",
                doc: "Required.",
            },
            Field {
                name: "optional?",
                ty: "boolean",
                doc: "Defaults to false.",
            },
            Field {
                name: "choices?",
                ty: "string[]",
                doc: "Required when kind == choice.",
            },
            Field {
                name: "hint?",
                ty: "string",
                doc: "Placeholder for kind == text; defaults to value.",
            },
        ],
    },
    Class {
        name: "kage.CommandSpec",
        doc: &[
            "Spec passed to `kage.register_command`. The handler runs",
            "through the coroutine bridge, so it may call the blocking",
            "`kage.ui.*` dialogs directly.",
        ],
        fields: &[
            Field {
                name: "name",
                ty: "string",
                doc: "No leading / or :.",
            },
            Field {
                name: "aliases?",
                ty: "string[]",
                doc: "Alternate names that resolve to this command.",
            },
            Field {
                name: "description",
                ty: "string",
                doc: "Shown in completion and :help.",
            },
            Field {
                name: "args?",
                ty: "kage.CommandArg[]",
                doc: "",
            },
            Field {
                name: "handler",
                ty: "fun(raw: string, ctx: table, args: table): string|kage.CommandResult|nil",
                doc: "raw text, host ctx, parsed args by name.",
            },
        ],
    },
    Class {
        name: "kage.WidgetSpec",
        doc: &["Spec passed to `kage.register_widget`."],
        fields: &[
            Field {
                name: "key",
                ty: "string",
                doc: "Re-registering replaces in place.",
            },
            Field {
                name: "render",
                ty: "fun(width: integer): string?",
                doc: "Runs once per redraw.",
            },
        ],
    },
    Class {
        name: "kage.CompletionItem",
        doc: &["One autocomplete candidate."],
        fields: &[
            Field {
                name: "value",
                ty: "string",
                doc: "Replacement text (required).",
            },
            Field {
                name: "label?",
                ty: "string",
                doc: "Row label; defaults to value.",
            },
            Field {
                name: "detail?",
                ty: "string",
                doc: "Dim annotation.",
            },
            Field {
                name: "range?",
                ty: "integer[]",
                doc: "{ from, to } 0-based byte span to replace.",
            },
        ],
    },
    Class {
        name: "kage.AutocompleteSpec",
        doc: &["Spec passed to `kage.add_autocomplete_provider`."],
        fields: &[
            Field {
                name: "name",
                ty: "string",
                doc: "Re-adding replaces in place.",
            },
            Field {
                name: "complete",
                ty: "fun(prefix: string, ctx: { text: string, cursor: integer }): kage.CompletionItem[]",
                doc: "",
            },
        ],
    },
    Class {
        name: "kage.KeyEvent",
        doc: &["Key descriptor handed to a `kage.on_terminal_input` handler."],
        fields: &[
            Field {
                name: "code",
                ty: "string",
                doc: "char|enter|esc|tab|backtab|backspace|up|down|left|right|home|end|pageup|pagedown|delete|insert|f1..f12|other.",
            },
            Field {
                name: "char?",
                ty: "string",
                doc: "Present only when code == char.",
            },
            Field {
                name: "ctrl",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "alt",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "shift",
                ty: "boolean",
                doc: "",
            },
        ],
    },
    Class {
        name: "kage.SendOpts",
        doc: &["Options for `kage.send_message`."],
        fields: &[
            Field {
                name: "trigger_turn?",
                ty: "boolean",
                doc: "Default true.",
            },
            Field {
                name: "deliver_as?",
                ty: "\"user\"",
                doc: "Only user is wired.",
            },
        ],
    },
    Class {
        name: "kage.Usage",
        doc: &[
            "Snapshot returned by `kage.context_usage`. The TUI fills",
            "it in. Token counts are session totals.",
        ],
        fields: &[
            Field {
                name: "model",
                ty: "string",
                doc: "Provider-qualified model id.",
            },
            Field {
                name: "input_tokens",
                ty: "integer",
                doc: "Input tokens charged across every turn.",
            },
            Field {
                name: "output_tokens",
                ty: "integer",
                doc: "Output tokens across every turn.",
            },
            Field {
                name: "cache_read_tokens",
                ty: "integer",
                doc: "Cache-read tokens across every turn.",
            },
            Field {
                name: "cache_write_tokens",
                ty: "integer",
                doc: "Cache-write tokens across every turn.",
            },
            Field {
                name: "current_context",
                ty: "integer",
                doc: "Tokens the latest turn used. 0 before the first turn.",
            },
            Field {
                name: "context_window",
                ty: "integer",
                doc: "Context window of `model`. 0 when unknown.",
            },
            Field {
                name: "working",
                ty: "boolean",
                doc: "Whether a run is in flight.",
            },
        ],
    },
    Class {
        name: "kage.CommandResult",
        doc: &["Rich result a command handler may return instead of a string."],
        fields: &[
            Field {
                name: "text?",
                ty: "string",
                doc: "Output text shown to the user.",
            },
            Field {
                name: "is_error?",
                ty: "boolean",
                doc: "Mark the invocation as failed.",
            },
            Field {
                name: "structured?",
                ty: "table",
                doc: "Machine-readable detail.",
            },
        ],
    },
    Class {
        name: "kage.AcpAgentSpec",
        doc: &["Spec passed to `kage.acp.add_agent`."],
        fields: &[
            Field {
                name: "name",
                ty: "string",
                doc: "Agent id; required.",
            },
            Field {
                name: "command",
                ty: "string",
                doc: "Executable to spawn; required.",
            },
            Field {
                name: "args?",
                ty: "string[]",
                doc: "Command arguments.",
            },
            Field {
                name: "env?",
                ty: "table",
                doc: "String-to-string environment overrides.",
            },
        ],
    },
    Class {
        name: "kage.McpServerSpec",
        doc: &["Spec passed to `kage.mcp.add_server`."],
        fields: &[
            Field {
                name: "name",
                ty: "string",
                doc: "Server id; required.",
            },
            Field {
                name: "command",
                ty: "string",
                doc: "Executable to spawn; required.",
            },
            Field {
                name: "args?",
                ty: "string[]",
                doc: "Command arguments.",
            },
            Field {
                name: "env?",
                ty: "table",
                doc: "String-to-string environment overrides.",
            },
            Field {
                name: "disabled?",
                ty: "boolean",
                doc: "Declare but do not spawn; defaults to false.",
            },
        ],
    },
    Class {
        name: "kage.ProviderSpec",
        doc: &["Spec passed to `kage.register_provider`."],
        fields: &[
            Field {
                name: "id",
                ty: "string",
                doc: "Provider id; required.",
            },
            Field {
                name: "display_name?",
                ty: "string",
                doc: "Defaults to id.",
            },
            Field {
                name: "supports_caching?",
                ty: "boolean",
                doc: "Defaults to false.",
            },
            Field {
                name: "supports_thinking?",
                ty: "boolean",
                doc: "Defaults to false.",
            },
            Field {
                name: "supports_tool_use?",
                ty: "boolean",
                doc: "Defaults to true.",
            },
            Field {
                name: "preserves_thinking?",
                ty: "boolean",
                doc: "Skip flatten-thinking when replaying history. Defaults to false.",
            },
            Field {
                name: "models?",
                ty: "kage.ProviderModel[]",
                doc: "Models the provider advertises in the picker.",
            },
            Field {
                name: "stream",
                ty: "fun(req: table, emit: fun(event: table)): table[]|fun(): table?",
                doc: "Yields provider event tables through `emit` or the return value; required. `emit` raises once the consumer cancels the stream, so stop yielding and return then.",
            },
        ],
    },
    Class {
        name: "kage.ProviderModel",
        doc: &["One model entry surfaced in the picker."],
        fields: &[
            Field {
                name: "id",
                ty: "string",
                doc: "Model id, used as the part after `provider:`.",
            },
            Field {
                name: "name?",
                ty: "string",
                doc: "Display name; defaults to id.",
            },
            Field {
                name: "context?",
                ty: "integer",
                doc: "Context window in tokens, for the modeline percent.",
            },
            Field {
                name: "max_output?",
                ty: "integer",
                doc: "Per-turn max output tokens forwarded to the provider.",
            },
        ],
    },
    Class {
        name: "kage.ExecSpec",
        doc: &["Spec passed to `kage.exec`."],
        fields: &[
            Field {
                name: "cmd",
                ty: "string",
                doc: "Executable; resolved via PATH. No shell.",
            },
            Field {
                name: "args?",
                ty: "string[]",
                doc: "Arguments, passed verbatim.",
            },
            Field {
                name: "cwd?",
                ty: "string",
                doc: "Workdir-relative dir; defaults to the workdir.",
            },
            Field {
                name: "timeout_secs?",
                ty: "integer",
                doc: "Kill the process after this many seconds. Default 30, at least 1.",
            },
        ],
    },
    Class {
        name: "kage.ExecResult",
        doc: &["Result returned by `kage.exec`."],
        fields: &[
            Field {
                name: "code",
                ty: "integer",
                doc: "Exit code; -1 if killed by a signal.",
            },
            Field {
                name: "timed_out",
                ty: "boolean",
                doc: "Whether `timeout_secs` elapsed and the process was killed.",
            },
            Field {
                name: "truncated",
                ty: "boolean",
                doc: "Whether stdout or stderr was cut at the 1 MiB cap and the rest dropped.",
            },
            Field {
                name: "stdout",
                ty: "string",
                doc: "Captured standard output.",
            },
            Field {
                name: "stderr",
                ty: "string",
                doc: "Captured standard error.",
            },
        ],
    },
    Class {
        name: "kage.HttpRequestOpts",
        doc: &[
            "Options accepted by `kage.http.get`, `kage.http.post`,",
            "`kage.http.delete`, and `kage.http.post_stream`.",
        ],
        fields: &[
            Field {
                name: "headers?",
                ty: "table<string, string>",
                doc: "Request headers.",
            },
            Field {
                name: "body?",
                ty: "string",
                doc: "Raw request body. Mutually exclusive with `json`.",
            },
            Field {
                name: "json?",
                ty: "table",
                doc: "Body encoded as JSON; sets Content-Type to application/json.",
            },
            Field {
                name: "max_bytes?",
                ty: "integer",
                doc: "Response body cap. Defaults: 2 MB simple, 32 MB streamed.",
            },
            Field {
                name: "timeout_secs?",
                ty: "integer",
                doc: "Whole-request budget in seconds. Default 30, at least 1. `post_stream` ignores it.",
            },
        ],
    },
    Class {
        name: "kage.AutocmdOpts",
        doc: &["Options for `kage.api.autocmd_create`."],
        fields: &[
            Field {
                name: "callback",
                ty: "fun(ev: kage.AutocmdEvent): any",
                doc: "Called when the event fires.",
            },
            Field {
                name: "group?",
                ty: "integer|string",
                doc: "Group id or name from `kage.api.augroup_create`.",
            },
            Field {
                name: "pattern?",
                ty: "string|string[]",
                doc: "Exact match values; `*` (the default) matches all.",
            },
            Field {
                name: "once?",
                ty: "boolean",
                doc: "Delete the autocmd before its first call.",
            },
            Field {
                name: "desc?",
                ty: "string",
                doc: "Shown in error messages.",
            },
        ],
    },
    Class {
        name: "kage.AutocmdEvent",
        doc: &[
            "What an autocmd callback receives. `match` is the tool name",
            "for `tool_call` and `tool_result`, the new value for",
            "`model_select` and `thinking_level_select`, the option name",
            "for `option_set`, the theme name for `color_scheme`, and the",
            "exec pattern for `user`.",
        ],
        fields: &[
            Field {
                name: "id",
                ty: "integer",
                doc: "Autocmd id.",
            },
            Field {
                name: "event",
                ty: "kage.Event",
                doc: "Event name.",
            },
            Field {
                name: "match?",
                ty: "string",
                doc: "Value the patterns matched against.",
            },
            Field {
                name: "group?",
                ty: "integer",
                doc: "Group id.",
            },
            Field {
                name: "data",
                ty: "kage.EventData",
                doc: "Event payload; its shape follows `event`.",
            },
        ],
    },
    Class {
        name: "kage.KeymapOpts",
        doc: &["Options for `kage.keymap.set` and `kage.api.keymap_set`."],
        fields: &[
            Field {
                name: "desc?",
                ty: "string",
                doc: "Shown in `?` help. Mappings without one are hidden there.",
            },
            Field {
                name: "group?",
                ty: "string",
                doc: "Help section. Defaults to `other`.",
            },
        ],
    },
    Class {
        name: "kage.HlSpec",
        doc: &[
            "A highlight group. Colors are `#rrggbb`, a color name",
            "(`red`, `lightblue`, ...) or a palette index `0` to `255`.",
        ],
        fields: &[
            Field {
                name: "fg?",
                ty: "string",
                doc: "Foreground color.",
            },
            Field {
                name: "bg?",
                ty: "string",
                doc: "Background color.",
            },
            Field {
                name: "bold?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "italic?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "underline?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "dim?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "reverse?",
                ty: "boolean",
                doc: "Swap foreground and background.",
            },
            Field {
                name: "link?",
                ty: "string",
                doc: "Group to follow. Wins over every other field.",
            },
        ],
    },
    Class {
        name: "kage.Span",
        doc: &[
            "A styled run of text in chrome rows and block renderers.",
            "Styles resolve when the row is painted, so they follow",
            "theme switches.",
        ],
        fields: &[
            Field {
                name: "text",
                ty: "string",
                doc: "",
            },
            Field {
                name: "hl?",
                ty: "string",
                doc: "Highlight group whose colors and attributes apply first.",
            },
            Field {
                name: "fg?",
                ty: "string",
                doc: "A group name (its fg), a theme role name, or a color.",
            },
            Field {
                name: "bg?",
                ty: "string",
                doc: "A group name (its bg), a theme role name, or a color.",
            },
            Field {
                name: "bold?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "dim?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "italic?",
                ty: "boolean",
                doc: "",
            },
            Field {
                name: "underline?",
                ty: "boolean",
                doc: "",
            },
        ],
    },
    Class {
        name: "kage.Block",
        doc: &[
            "Block payload a `kage.register_block_renderer` render receives.",
            "Every payload carries `kind`; `width` is injected fresh per",
            "render; the rest follow the",
            "kind: `user` has `text`; `assistant` has `text` and `live`",
            "(the block is still streaming); `thinking` has `text`, `folded`",
            "and `live`; `tool_call` has `name`, `input_summary`,",
            "`input_pretty` and `folded`; `tool_result` has `name`,",
            "`output`, `is_error`, `folded` and `duration_ms`; a custom",
            "kind has `text` and `folded`.",
        ],
        fields: &[
            Field {
                name: "kind",
                ty: "string",
                doc: "A namespaced custom kind or a reserved built-in name.",
            },
            Field {
                name: "width",
                ty: "integer",
                doc: "Terminal width in columns.",
            },
            Field {
                name: "text?",
                ty: "string",
                doc: "Block text: user, assistant, thinking and custom blocks.",
            },
            Field {
                name: "live?",
                ty: "boolean",
                doc: "Whether the assistant or thinking block is still streaming.",
            },
            Field {
                name: "folded?",
                ty: "boolean",
                doc: "Whether the block is collapsed: thinking, tool_call, tool_result and custom blocks.",
            },
            Field {
                name: "name?",
                ty: "string",
                doc: "Tool name, for tool_call and tool_result.",
            },
            Field {
                name: "input_summary?",
                ty: "string",
                doc: "One-line tool input summary, for tool_call.",
            },
            Field {
                name: "input_pretty?",
                ty: "string",
                doc: "Pretty-printed tool input, for tool_call.",
            },
            Field {
                name: "output?",
                ty: "string",
                doc: "Captured tool output, for tool_result.",
            },
            Field {
                name: "is_error?",
                ty: "boolean",
                doc: "Whether the tool call failed, for tool_result.",
            },
            Field {
                name: "duration_ms?",
                ty: "integer",
                doc: "Tool execution time in milliseconds, for tool_result.",
            },
        ],
    },
    Class {
        name: "kage.Action",
        doc: &["A Rust action from `kage.action`, used as a mapping rhs."],
        fields: &[],
    },
    Class {
        name: "kage.SlotSpec",
        doc: &[
            "The layout of a slot. `header`, `activity`, `input_pill`",
            "and `footer` take `left`, `right` and `sep`; `start` takes",
            "`lines`. The `header` and `activity` rows collapse while",
            "they paint nothing. An item is a built-in component name, a",
            "`kage.Span`, or a `kage.SlotComponent`.",
        ],
        fields: &[
            Field {
                name: "left?",
                ty: "(kage.Component|kage.Span|kage.SlotComponent)[]",
                doc: "Painted from the left edge.",
            },
            Field {
                name: "right?",
                ty: "(kage.Component|kage.Span|kage.SlotComponent)[]",
                doc: "Painted against the right edge.",
            },
            Field {
                name: "sep?",
                ty: "string",
                doc: "Painted between two items that both have output.",
            },
            Field {
                name: "lines?",
                ty: "(kage.Component|kage.Span|kage.SlotComponent)[]",
                doc: "One line per item, for `start`.",
            },
        ],
    },
    Class {
        name: "kage.SlotComponent",
        doc: &[
            "A slot component rendered by Lua. Its output is kept and",
            "recomputed only when a listed event fires, its interval",
            "elapses, `kage.api.redraw` is called, its slot is set, or",
            "the terminal width changes.",
        ],
        fields: &[
            Field {
                name: "render",
                ty: "fun(ctx: kage.SlotContext): any|nil",
                doc: "Returns the same shape as a block renderer.",
            },
            Field {
                name: "events?",
                ty: "string[]",
                doc: "Event names, each optionally followed by a space and a pattern.",
            },
            Field {
                name: "interval?",
                ty: "integer",
                doc: "Recompute every this many milliseconds (at least 50).",
            },
            Field {
                name: "hl?",
                ty: "string",
                doc: "Highlight group applied under the spans' own styles.",
            },
        ],
    },
    Class {
        name: "kage.SlotContext",
        doc: &["What a `kage.SlotComponent` render receives."],
        fields: &[
            Field {
                name: "width",
                ty: "integer",
                doc: "Terminal width in columns.",
            },
            Field {
                name: "model",
                ty: "string",
                doc: "Active `provider/model` id.",
            },
            Field {
                name: "thinking",
                ty: "string",
                doc: "Thinking level the next run sends, `off` when it sends none.",
            },
            Field {
                name: "thinking_auto",
                ty: "boolean",
                doc: "Whether the level is automatic rather than chosen.",
            },
            Field {
                name: "permission_mode?",
                ty: "string",
                doc: "Session permission override, if any.",
            },
            Field {
                name: "working",
                ty: "boolean",
                doc: "Whether a run is in flight.",
            },
            Field {
                name: "usage?",
                ty: "table",
                doc: "`{ total = { input, output, cache_read, cache_write }, context_used, context_window, cost }`.",
            },
            Field {
                name: "session",
                ty: "{ id: string, title?: string }",
                doc: "Active session.",
            },
            Field {
                name: "cwd",
                ty: "string",
                doc: "Working directory.",
            },
            Field {
                name: "mode",
                ty: "string",
                doc: "Editor mode: `normal`, `insert` or `visual`.",
            },
        ],
    },
    Class {
        name: "kage.EventUsage",
        doc: &["Token usage an event payload carries."],
        fields: &[
            Field {
                name: "input",
                ty: "integer",
                doc: "Input tokens charged.",
            },
            Field {
                name: "output",
                ty: "integer",
                doc: "Output tokens charged.",
            },
            Field {
                name: "cache_read",
                ty: "integer",
                doc: "Cache-read tokens charged.",
            },
            Field {
                name: "cache_write",
                ty: "integer",
                doc: "Cache-write tokens charged.",
            },
        ],
    },
    Class {
        name: "kage.BeforeAgentStartPayload",
        doc: &["Payload of the `before_agent_start` event."],
        fields: &[
            Field {
                name: "system_prompt",
                ty: "string",
                doc: "System prompt the run will send.",
            },
            Field {
                name: "first_user_message",
                ty: "string",
                doc: "Text of the user message that started the run.",
            },
        ],
    },
    Class {
        name: "kage.AgentStartPayload",
        doc: &["Payload of the `agent_start` event: an empty table."],
        fields: &[],
    },
    Class {
        name: "kage.AgentEndPayload",
        doc: &["Payload of the `agent_end` event."],
        fields: &[Field {
            name: "ok",
            ty: "boolean",
            doc: "Whether the run returned without a provider error.",
        }],
    },
    Class {
        name: "kage.TurnStartPayload",
        doc: &["Payload of the `turn_start` event."],
        fields: &[Field {
            name: "index",
            ty: "integer",
            doc: "Zero-based index of the turn inside the run.",
        }],
    },
    Class {
        name: "kage.TurnEndPayload",
        doc: &["Payload of the `turn_end` event."],
        fields: &[
            Field {
                name: "index",
                ty: "integer",
                doc: "Zero-based index of the turn inside the run.",
            },
            Field {
                name: "had_tool_calls",
                ty: "boolean",
                doc: "Whether the model requested any tool calls this turn.",
            },
        ],
    },
    Class {
        name: "kage.MessageStartPayload",
        doc: &["Payload of the `message_start` event."],
        fields: &[Field {
            name: "id",
            ty: "string",
            doc: "Id of the assistant message that began.",
        }],
    },
    Class {
        name: "kage.MessageUpdatePayload",
        doc: &["Payload of the `message_update` event, fired per text delta."],
        fields: &[
            Field {
                name: "id",
                ty: "string",
                doc: "Id of the assistant message being streamed.",
            },
            Field {
                name: "delta",
                ty: "string",
                doc: "Text emitted since the previous update.",
            },
        ],
    },
    Class {
        name: "kage.MessageEndPayload",
        doc: &[
            "Payload of the `message_end` and `after_provider_response`",
            "events.",
        ],
        fields: &[
            Field {
                name: "id",
                ty: "string",
                doc: "Id of the assistant message that finished.",
            },
            Field {
                name: "usage",
                ty: "kage.EventUsage",
                doc: "Tokens the turn charged.",
            },
        ],
    },
    Class {
        name: "kage.ToolCallPayload",
        doc: &["Payload of the `tool_call` event."],
        fields: &[
            Field {
                name: "id",
                ty: "string",
                doc: "Provider correlation id of the call.",
            },
            Field {
                name: "name",
                ty: "string",
                doc: "Tool being invoked; also the `match` key.",
            },
            Field {
                name: "input",
                ty: "table",
                doc: "Arguments the model passed, as sent so far.",
            },
        ],
    },
    Class {
        name: "kage.ToolUpdatePayload",
        doc: &["Payload of the `tool_update` event, fired mid-execution."],
        fields: &[
            Field {
                name: "id",
                ty: "string",
                doc: "Provider correlation id of the running call.",
            },
            Field {
                name: "content",
                ty: "string",
                doc: "Human-readable progress line.",
            },
            Field {
                name: "structured?",
                ty: "table",
                doc: "Machine-readable progress detail, when the tool sent any.",
            },
        ],
    },
    Class {
        name: "kage.ToolResultPayload",
        doc: &["Payload of the `tool_result` event."],
        fields: &[
            Field {
                name: "id",
                ty: "string",
                doc: "Provider correlation id of the finished call.",
            },
            Field {
                name: "name?",
                ty: "string",
                doc: "Tool that ran; also the `match` key.",
            },
            Field {
                name: "is_error",
                ty: "boolean",
                doc: "Whether the call failed.",
            },
            Field {
                name: "text",
                ty: "string",
                doc: "Output text returned to the model.",
            },
        ],
    },
    Class {
        name: "kage.ModelSelectPayload",
        doc: &["Payload of the `model_select` event. Fires in the TUI only."],
        fields: &[
            Field {
                name: "prev",
                ty: "string",
                doc: "Provider-qualified model id before the switch.",
            },
            Field {
                name: "next",
                ty: "string",
                doc: "Model id after the switch; also the `match` key.",
            },
            Field {
                name: "source",
                ty: "string",
                doc: "How the switch happened: `set` today.",
            },
        ],
    },
    Class {
        name: "kage.ThinkingLevelSelectPayload",
        doc: &["Payload of the `thinking_level_select` event."],
        fields: &[
            Field {
                name: "prev",
                ty: "string",
                doc: "Level before the switch, `default` for the automatic one.",
            },
            Field {
                name: "next",
                ty: "string",
                doc: "Level after the switch; also the `match` key.",
            },
            Field {
                name: "source",
                ty: "string",
                doc: "`cycle` or `settings`.",
            },
        ],
    },
    Class {
        name: "kage.UserShellPayload",
        doc: &["Payload of the `user_shell` event, fired when a `!cmd` ends."],
        fields: &[
            Field {
                name: "cmd",
                ty: "string",
                doc: "Command line that ran.",
            },
            Field {
                name: "exit_code?",
                ty: "integer",
                doc: "Exit code, nil when a signal or a cancel ended the command.",
            },
        ],
    },
    Class {
        name: "kage.PermissionModeSelectPayload",
        doc: &["Payload of the `permission_mode_select` event."],
        fields: &[
            Field {
                name: "prev",
                ty: "string",
                doc: "Mode before the switch: `default`, `ask` or `deny`.",
            },
            Field {
                name: "next",
                ty: "string",
                doc: "Mode after the switch.",
            },
            Field {
                name: "source",
                ty: "string",
                doc: "`command` today.",
            },
        ],
    },
    Class {
        name: "kage.OptionSetPayload",
        doc: &["Payload of the `option_set` event."],
        fields: &[
            Field {
                name: "name",
                ty: "string",
                doc: "Option that changed; also the `match` key.",
            },
            Field {
                name: "old",
                ty: "string|boolean|integer|number",
                doc: "Value before the set.",
            },
            Field {
                name: "new",
                ty: "string|boolean|integer|number",
                doc: "Value after the set.",
            },
            Field {
                name: "source",
                ty: "kage.OptionSource",
                doc: "Where the new value came from.",
            },
        ],
    },
    Class {
        name: "kage.ColorSchemePayload",
        doc: &["Payload of the `color_scheme` event, fired on a theme switch."],
        fields: &[Field {
            name: "name",
            ty: "string",
            doc: "New theme name; also the `match` key.",
        }],
    },
    Class {
        name: "kage.HistoryMessage",
        doc: &["One message of the history `transform_context` receives."],
        fields: &[
            Field {
                name: "role",
                ty: "string",
                doc: "`user`, `assistant`, `tool_result` or `system`.",
            },
            Field {
                name: "content",
                ty: "table[]",
                doc: "Content blocks, each tagged with its `kind`.",
            },
            Field {
                name: "id",
                ty: "string",
                doc: "Stable message id.",
            },
            Field {
                name: "parent?",
                ty: "string",
                doc: "Parent message id, when the message is a branch.",
            },
            Field {
                name: "ts",
                ty: "string",
                doc: "RFC 3339 creation timestamp.",
            },
        ],
    },
    Class {
        name: "kage.ProviderRequestPayload",
        doc: &["Outgoing request `before_provider_request` receives."],
        fields: &[
            Field {
                name: "model",
                ty: "string",
                doc: "Provider-qualified model id.",
            },
            Field {
                name: "messages",
                ty: "kage.HistoryMessage[]",
                doc: "Conversation history, ending with the latest user turn.",
            },
            Field {
                name: "system?",
                ty: "string",
                doc: "System prompt, when one is set.",
            },
            Field {
                name: "tools",
                ty: "table[]",
                doc: "Tool specs available to the model this turn.",
            },
            Field {
                name: "max_output_tokens?",
                ty: "integer",
                doc: "Output-token cap, when one is set.",
            },
            Field {
                name: "temperature?",
                ty: "number",
                doc: "Sampling temperature, when one is set.",
            },
            Field {
                name: "thinking?",
                ty: "{ budget_tokens: integer }",
                doc: "Explicit thinking budget, when one is set.",
            },
            Field {
                name: "level?",
                ty: "string",
                doc: "Thinking effort for this turn, when one is set.",
            },
            Field {
                name: "reasoning?",
                ty: "table",
                doc: "Thinking settings the model accepts, tagged with a `kind`.",
            },
        ],
    },
    Class {
        name: "kage.CompactPreparePayload",
        doc: &["Payload of the `compact_prepare` transform."],
        fields: &[
            Field {
                name: "transcript",
                ty: "string",
                doc: "Plain-text transcript of the turns being summarized.",
            },
            Field {
                name: "instruction",
                ty: "string",
                doc: "System instruction for the summarization call.",
            },
            Field {
                name: "prompt",
                ty: "string",
                doc: "User-role prompt the summarizer receives.",
            },
            Field {
                name: "model",
                ty: "string",
                doc: "Model id the summarization call uses.",
            },
            Field {
                name: "summarized",
                ty: "integer",
                doc: "Messages being summarized away.",
            },
            Field {
                name: "kept",
                ty: "integer",
                doc: "Recent messages kept verbatim after compaction.",
            },
        ],
    },
    Class {
        name: "kage.ShouldStopAfterTurnPayload",
        doc: &["Turn summary the `should_stop_after_turn` predicate receives."],
        fields: &[
            Field {
                name: "index",
                ty: "integer",
                doc: "Zero-based index of the finished turn inside the run.",
            },
            Field {
                name: "had_tool_calls",
                ty: "boolean",
                doc: "Whether the assistant requested any tool calls.",
            },
            Field {
                name: "usage",
                ty: "kage.EventUsage",
                doc: "Tokens the turn charged.",
            },
        ],
    },
];
