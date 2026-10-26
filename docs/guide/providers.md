# providers

kage talks to LLM providers through ids addressed as
`provider-id/model-id`, e.g. `kage -m anthropic/claude-sonnet-4-6` or
`kage -m deepseek/deepseek-v4-pro`. The older `provider-id:model-id`
form still works anywhere a model is named. Every provider kage knows is either
built in, part of the OpenAI-compatible catalog, or declared by you
under `[providers.custom.*]`.

## built-in providers

| id          | provider            | credential env      |
| ----------- | ------------------- | ------------------- |
| `anthropic` | Anthropic           | `ANTHROPIC_API_KEY` |
| `openai`    | OpenAI              | `OPENAI_API_KEY`    |
| `google`    | Google Gemini       | `GEMINI_API_KEY`    |
| `acp`       | external ACP agents | -                   |

`openai` speaks OpenAI's Responses API. `kage auth login openai` offers
two ways in: an API key, or a ChatGPT sign-in that serves the same
models from your ChatGPT plan instead of API billing. An exported
`OPENAI_API_KEY` wins over a saved sign-in.

With the ChatGPT sign-in, `openai` offers the models your plan may use,
as ChatGPT's own model list reports them, with the largest context
window it accepts and no per-token cost. kage saves that list in
`~/.cache/kage/chatgpt-models.json` when you sign in, and
`kage models refresh` updates it. Without it, the catalog's OpenAI
models are offered instead.

A model draws by calling OpenAI's hosted image tool, which runs at
OpenAI and is billed on its own. kage offers it only when asked:

```toml
[providers.openai]
image_generation = true
```
`acp` is not an HTTP provider and cannot be overridden here. Its agents
are configured under `[acp.*]`.

The Gemini provider's id is `google`; `gemini` names only the wire
protocol a custom provider selects with `kind = "gemini"` below.

## openai-compatible providers

Each of these is an OpenAI-compatible endpoint, except
`commandcode-claude` and `kenari-claude`, which speak the Anthropic
Messages format. kage
registers one when a credential is available, and its models become
addressable as `<id>:<model>`:

| id                      | provider                      | credential env       | default base URL                              |
| ----------------------- | ----------------------------- | -------------------- | --------------------------------------------- |
| `zai`                   | Z.AI                          | `ZAI_API_KEY`        | `https://api.z.ai/api/paas/v4`                |
| `zai-coding-plan`       | Z.AI Coding Plan              | `ZAI_CODING_API_KEY` | `https://api.z.ai/api/coding/paas/v4`         |
| `zhipuai-coding-plan`   | Zhipu AI Coding Plan          | `ZAI_CODING_API_KEY` | `https://open.bigmodel.cn/api/coding/paas/v4` |
| `deepseek`              | DeepSeek                      | `DEEPSEEK_API_KEY`   | `https://api.deepseek.com/v1`                 |
| `groq`                  | Groq                          | `GROQ_API_KEY`       | `https://api.groq.com/openai/v1`              |
| `mistral`               | Mistral                       | `MISTRAL_API_KEY`    | `https://api.mistral.ai/v1`                   |
| `cerebras`              | Cerebras                      | `CEREBRAS_API_KEY`   | `https://api.cerebras.ai/v1`                  |
| `xai`                   | xAI                           | `XAI_API_KEY`        | `https://api.x.ai/v1`                         |
| `openrouter`            | OpenRouter                    | `OPENROUTER_API_KEY` | `https://openrouter.ai/api/v1`                |
| `fireworks-ai`          | Fireworks AI                  | `FIREWORKS_API_KEY`  | `https://api.fireworks.ai/inference/v1`       |
| `moonshotai`            | Moonshot AI                   | `MOONSHOT_API_KEY`   | `https://api.moonshot.ai/v1`                  |
| `kimi-for-coding`       | Kimi for Coding               | `KIMI_API_KEY`       | `https://api.kimi.com/coding/v1`              |
| `xiaomi`                | Xiaomi                        | `XIAOMI_API_KEY`     | `https://api.xiaomimimo.com/v1`               |
| `xiaomi-token-plan-ams` | Xiaomi Token Plan (Europe)    | `XIAOMI_API_KEY`     | `https://token-plan-ams.xiaomimimo.com/v1`    |
| `xiaomi-token-plan-cn`  | Xiaomi Token Plan (China)     | `XIAOMI_API_KEY`     | `https://token-plan-cn.xiaomimimo.com/v1`     |
| `xiaomi-token-plan-sgp` | Xiaomi Token Plan (Singapore) | `XIAOMI_API_KEY`     | `https://token-plan-sgp.xiaomimimo.com/v1`    |
| `commandcode`           | Command Code                  | `CMD_API_KEY`        | `https://api.commandcode.ai/provider/v1`      |
| `commandcode-claude`    | Command Code (Claude)         | `CMD_API_KEY`        | `https://api.commandcode.ai/provider`         |
| `kenari`                | Kenari                        | `KENARI_API_KEY`     | `https://kenari.id/v1`                        |
| `kenari-claude`         | Kenari (Claude)               | `KENARI_API_KEY`     | `https://kenari.id`                           |
| `opencode-go`           | OpenCode Go                   | `OPENCODE_API_KEY`   | `https://opencode.ai/zen/go/v1`               |

The four `xiaomi*` ids share one key, and so do the two coding plans,
the two `commandcode` ids and the two `kenari`
ids. `zai` is billed apart from the coding
plans and needs its own key.

The model picker lists the models the [catalog](#model-catalog) knows
for each provider. Every provider in the table has catalog entries, so
pick a model from the picker or name one yourself, as in
`kage -m kimi-for-coding:k3`.

### Z.AI and Zhipu AI

Z.AI sells a GLM coding plan in two regions, and kage has a provider
for each:

| id                    | plan                         | endpoint                                      |
| --------------------- | ---------------------------- | --------------------------------------------- |
| `zai-coding-plan`     | Z.AI coding plan (global)    | `https://api.z.ai/api/coding/paas/v4`         |
| `zhipuai-coding-plan` | Zhipu AI coding plan (China) | `https://open.bigmodel.cn/api/coding/paas/v4` |

Both read the same key: `ZAI_CODING_API_KEY`, or the key saved with
`kage auth login zai-coding-plan`. A key saved with
`kage auth login zhipuai-coding-plan` is used for the China plan
instead, when there is one. Each plan lists the models the
[curated catalog](#model-catalog) publishes for it. `zai` is the
pay-as-you-go API with its own key.

If only one endpoint is reachable from your network, point the other
plan at it with a `base_url` override (see below), for example:

```toml
[providers.zhipuai-coding-plan]
base_url = "https://api.z.ai/api/coding/paas/v4"
```

kage shapes requests to `zai`, both coding plans and any custom
provider whose `base_url` is on `api.z.ai` or `open.bigmodel.cn` the
way Z.AI expects: `max_tokens` for the output limit, the system prompt as a
`system` message, `thinking` with `type` `enabled` or `disabled` (and
`clear_thinking: false` when enabled), `reasoning_effort` only for
models that list effort values, and `tool_stream: true` when tools
are sent (except to `glm-4.5` models). kage sends its own
`User-Agent`.

### Command Code

Command Code (commandcode.ai) fronts many model vendors behind one
key and two wire formats. kage registers a provider for each:

| id                   | format                  | endpoint                                 |
| -------------------- | ----------------------- | ---------------------------------------- |
| `commandcode`        | OpenAI Chat Completions | `https://api.commandcode.ai/provider/v1` |
| `commandcode-claude` | Anthropic Messages      | `https://api.commandcode.ai/provider`    |

Both read the same key: `CMD_API_KEY`, or the key saved with
`kage auth login commandcode`. The Claude models are served on the
Messages endpoint only and list under `commandcode-claude`, with
prompt caching and thinking following the Anthropic rules. Everything
else lists under `commandcode`. Models are addressed as
`commandcode/gpt-6-astra` or `commandcode-claude/claude-sonnet-5-5`.

Command Code is subscription-based, so its models carry no per-token
pricing: the footer and the agents overlay show no dollar amount for
them. The model list is rebuilt on a schedule from Command Code's
published model list (which models exist, their names, context
windows and wire placement) and the official `command-code` CLI
catalog (thinking levels), so new and retired models reach a catalog
refresh without anything being changed by hand.

Command Code retains request data by default, though most models
already route through zero-retention upstreams without asking. To
guarantee it, opt into their zero data retention flag on both
flavors:

```toml
[providers.commandcode.headers]
x-cmd-zdr = "1"

[providers.commandcode-claude.headers]
x-cmd-zdr = "1"
```

ZDR requests route only to ZDR-capable upstreams, fail with a 422
when none have capacity, and can cost more. See [Command Code's ZDR
docs](https://commandcode.ai/docs/resources/zdr).

### Kenari

Kenari (kenari.id) serves every model on both wire formats behind one
key. kage registers a provider for each:

| id              | format                  | endpoint               |
| --------------- | ----------------------- | ---------------------- |
| `kenari`        | OpenAI Chat Completions | `https://kenari.id/v1` |
| `kenari-claude` | Anthropic Messages      | `https://kenari.id`    |

Both read the same key: `KENARI_API_KEY`, or the key saved with
`kage auth login kenari`. The Claude models list under
`kenari-claude`, so prompt caching and thinking follow the Anthropic
rules. Everything else lists under `kenari`. Models are addressed as
`kenari/gpt-6-luna` or `kenari-claude/claude-sonnet-5-5`.

The bundled list carries only the models Kenari's
[subscription plans](https://kenari.id/en/docs/subscriptions) cover,
with no per-token pricing. Which of them your plan includes depends on
the tier. Other Kenari models still work when named directly, and are
billed to your balance. The list is rebuilt on a schedule from
Kenari's plans and published model list, taking names and release
dates from models.dev where it lists the model.

### OpenCode Go

[OpenCode](https://opencode.ai/docs/go) fronts a pooled model catalog
through its Go gateway, a cheaper relay with a smaller list:

| id            | endpoint                        |
| ------------- | ------------------------------- |
| `opencode-go` | `https://opencode.ai/zen/go/v1` |

It reads `OPENCODE_API_KEY`, or the key saved with
`kage auth login opencode-go`. Models are addressed as
`opencode-go/grok-4.7`.

Every request to an `opencode.ai` endpoint, including a custom provider
pointed there, carries the `x-opencode-session` and `x-opencode-client`
headers OpenCode routes by. The session header holds the kage session
id. A header of the same name under the provider's `headers` replaces
it.

## credentials

A provider registers when a key is reachable from either source, first
match wins:

1. the credential environment variable from the tables above;
2. the key saved by `kage auth login <provider-id>`
   (`~/.local/share/kage/auth.json`, mode `0600`).

`kage auth login` with no argument opens a picker that also lists your
custom providers. Rows marked `*` already have a stored credential.
`kage auth list` shows where each provider's key would come from,
custom providers included. Inside the TUI, `/login [provider]` runs
the same flow: the UI suspends, the credential prompt takes over the
terminal, and the model list refreshes in place when a key is saved.

## overriding a provider

`[providers.<provider-id>]` replaces the base URL or the key variable
of a built-in or catalog provider, or adds headers to every request.
`acp` cannot be overridden:

```toml
[providers.deepseek]
base_url = "https://relay.internal/v1"
api_key_env = "DEEPSEEK_RELAY_KEY"

[providers.deepseek.headers]
X-Team = "infra"
```

Fields left out keep the provider's default.

## request headers

`headers` adds HTTP headers to every request a provider sends. It
works on built-in and catalog providers under `[providers.<id>]` and
on custom providers under `[providers.custom.<id>]`, and the headers
go out after kage's own:

```toml
[providers.zhipuai-coding-plan.headers]
X-Team = "infra"

[providers.custom.relay.headers]
Authorization = "Basic c2VydmljZTpwdW5jdWF0aW9u"
```

The inline form works too:
`headers = { X-Team = "infra" }` inside the provider's table.

## custom providers

`[providers.custom.<id>]` registers a provider kage does not know. The
id must be lowercase letters, digits, and dashes. It becomes the
`<provider-id>` half of `provider-id/model-id`:

```toml
[providers.custom.llama-local]
kind = "openai"                      # openai (default), anthropic, or gemini
base_url = "http://localhost:8080/v1"
display_name = "Llama (local)"       # shown in the picker, defaults to the id
api_key_env = ""                     # no key needed. Omit to use LLAMA_LOCAL_API_KEY

# one [[...models]] table per model
[[providers.custom.llama-local.models]]
id = "llama-3-70b"
name = "Llama 3 70B"
context = 131072                     # optional, in tokens
max_output = 8192                    # optional, in tokens
efforts = ["low", "medium", "high"]  # optional, see the model keys below
input = ["text", "image"]            # optional: text, image, pdf, audio, video
produces = ["text"]                  # optional: what it outputs, text when unset
# optional, USD per million tokens. cache_read and cache_write are optional
cost = { input = 0.27, output = 1.10, cache_read = 0.07, cache_write = 0.0 }

[providers.custom.relay]
kind = "anthropic"
base_url = "https://relay.internal"
api_key_env = "RELAY_API_KEY"

[providers.custom.relay.headers]
Authorization = "Basic c2VydmljZTpwdW5jdWF0aW9u"

[[providers.custom.relay.models]]
id = "sonnet-relay"
name = "Sonnet via relay"
```

Models are addressed as `llama-local/llama-3-70b` and
`relay:sonnet-relay`, and appear in the model picker once their key is
available. A provider whose `api_key_env` is set to a non-empty
variable that is unset (and has no saved key) is skipped. With
`api_key_env = ""` it registers unconditionally, which is what
keyless local endpoints want.

A model whose `produces` (or catalog entry) includes `image` is asked
for images and can answer with them: Gemini models through
`responseModalities`, and `openai`-kind models through the
`modalities` field OpenRouter and compatible gateways read. The images
land in the reply and in the session like attached ones, without
bloating the session file. On later turns a Gemini model that draws
gets its own images back, so "make it blue" edits the last one; other
models read a short note where each image was. `/image save` writes
the newest one into the project.

A custom id that matches a catalog provider, such as `deepseek`,
replaces that provider. kage prints a warning at startup naming the
id, and the picker offers only the models the custom provider declares.
Cost tracking follows the same rule: prices come from the custom
models' `cost` keys, never from the catalog, even for a model id the
catalog prices.
Built-in ids are refused; override those with `[providers.<id>]`.

The provider keys `tool_use`, `thinking` and `caching` are accepted
but change nothing: kage always sends tool definitions, and thinking
goes back to the model as described under [thinking](#thinking).

Some `anthropic` gateways count cached tokens inside `input_tokens`
instead of beside it, which makes the context gauge and the cost read
about double. kage notices this by itself: Anthropic reports less input
than cached tokens on a cache hit, so a response whose `input_tokens`
covers its cached tokens has them taken back out. Set
`input_includes_cache = true` to do that for every response of a
provider. `openai` endpoints always count this way, so kage corrects
them without the key.

Five optional model keys describe what a model accepts and costs,
since kage has no catalog entry for it:

- `reasoning`: `false` for a model that does not think, which sends no
  thinking setting. `true` offers every level.
- `efforts`: the effort values the model takes (`none`, `minimal`,
  `low`, `medium`, `high`, `xhigh`, `max`, `ultra`). Implies `reasoning = true`
  and limits the levels to these.
- `input`: the kinds of input the model takes. Attaching an image to a
  model whose `input` leaves out `image` warns, and the image is not
  sent.
- `interleaved`: for `kind = "openai"`, the assistant message field
  the model reads its own reasoning back from during a tool loop,
  `reasoning_content` or `reasoning_details` (the models.dev
  `interleaved.field`). Left out, reasoning is sent as `<thinking>`
  text in the message content.
- `cost`: prices in USD per million tokens, with the same fields as
  the catalog: `input` and `output`, plus the optional `cache_read`
  and `cache_write`. Cache reads without a price are billed at the
  input rate and cache writes at the output rate. Every price must be
  a finite number and not negative, or kage refuses to start. A model
  without `cost` has no known price. Its cost counts as zero, so the
  footer and the agents overlay show no dollar amount for it.

A model with neither `reasoning` nor `efforts` sends a level you pick
unchanged, and no level while thinking is automatic.

## model catalog

kage ships a snapshot of a curated catalog for its providers: model
names, context and output limits, pricing, accepted inputs and thinking
options. The model picker shows each model's inputs on the right.
Attaching an image while the model's inputs leave out `image` warns,
and the image is not sent.

That catalog is built by [kage-models](https://github.com/QaidVoid/kage-models),
which merges [models.dev](https://models.dev) with the model lists and
subscription plans the other providers publish. models.dev alone
cannot describe Command Code, and its Kenari entry lists the wider
pay-as-you-go catalog rather than what the plans cover. kage reads the
merged document and never contacts those upstreams itself.

`kage models refresh` downloads the current catalog into
`~/.cache/kage/models.json` (`$XDG_CACHE_HOME/kage`). Later runs lay
it over the snapshot: it adds models to the providers kage knows and
updates their metadata. It never adds a provider or changes a
provider's endpoint or credentials. kage never refreshes on its own,
and a missing or unreadable cache falls back to the snapshot silently.
Delete the file to go back to the snapshot.

Because the catalog is curated upstream, a refresh can bring Command
Code and Kenari model changes too, not only the models.dev ones. Pass
`--source` to read a catalog from somewhere else, such as a local file
for an offline run, or set `KAGE_CATALOG_URL` to change it for every
run.

## thinking

A session's thinking level is one of `off`, `minimal`, `low`,
`medium`, `high`, `xhigh`, `max` and `ultra`, or automatic. Automatic is the default:
it sends `high`, or the nearest level the model accepts when it has no
`high` (the higher one on a tie). A level you choose with
`alt+t`, the settings dialog or `[ui] thinking_level` is fitted the
same way, and only an explicit `off` turns thinking off. On a model
that cannot switch thinking off, `off` becomes its lowest level.
`alt+t` only visits levels the model accepts. The start card shows an
automatic level as `high (auto)`, and the settings dialog and editors
name it `auto`.

The catalog lists how each model takes thinking, and kage maps the
level onto it:

| model takes        | kage sends |
| ------------------ | ---------- |
| effort values      | the effort named like the level. `none` is `off`. |
| a token budget     | the level's budget (`minimal` 1024 to `max` 65536 tokens), kept within the model's bounds |
| an on/off switch   | on for any level other than `off`, shown as `high` |
| nothing to set     | no thinking setting |

On the wire, Anthropic effort models get adaptive thinking with
`output_config.effort`, and budget models get `budget_tokens`. OpenAI
models get `reasoning_effort` (`reasoning.effort` on the Responses
API). Gemini models get `thinkingLevel` or `thinkingBudget`.
OpenAI-compatible models with an on/off switch get `thinking.type`
`enabled` or `disabled`. Z.AI endpoints get the shape described under
[Z.AI and Zhipu AI](#z-ai-and-zhipu-ai).

Reasoning goes back to the model that produced it, so thinking carries
across tool calls:

- Anthropic gets its signed thinking and redacted thinking blocks back
  unchanged, first in their assistant message. Budget thinking also
  sends the `interleaved-thinking-2025-05-14` beta header. A request
  that continues a tool loop whose assistant turn has no thinking
  signed by this model leaves budget thinking out, since the API
  would refuse it.
- Gemini gets each thought signature back on its function call.
- The Responses API returns encrypted reasoning, which goes back as the
  `encrypted_content` of its `reasoning` item.
- OpenAI-compatible models with a catalog or config `interleaved`
  field get the current turn's reasoning in that field. Earlier turns
  leave it out.

Anthropic-compatible gateways serving non-Claude models get thinking
another model produced back as an unsigned native block, which those
backends need on a tool-call turn. Claude models reject unsigned
blocks, so their unsigned thinking is dropped. Gemini cannot verify
thinking another model produced, so it gets it as `<thinking>` text.
Custom providers of `kind = "anthropic"` or `"gemini"` follow the
same rules.

## base urls

`base_url` replaces the default endpoint root, and kage appends the
protocol-specific path. What "root" means depends on `kind`:

| kind       | request path                                          | base_url must include |
| ---------- | ----------------------------------------------------- | --------------------- |
| `openai`   | `<base>/chat/completions`                             | the `/v1` segment, e.g. `http://localhost:8080/v1` |
| `anthropic`| `<base>/v1/messages`                                  | the host root, e.g. `https://relay.internal` |
| `gemini`   | `<base>/v1beta/models/<model>:streamGenerateContent`  | the host root, e.g. `https://generativelanguage.googleapis.com` |

The built-in `openai` provider follows the `openai` rule with
`<base>/responses`. API keys travel in request headers (`x-api-key`,
`Authorization`, `x-goog-api-key`), never in the URL.

## listing providers

`kage providers` lists every provider id this kage knows, built-in or
from the catalog, with the wire protocol it speaks and the tables that
may name it. The builtins come first, then the catalog, shortened
here:

```
PROVIDER               API               OVERRIDE  CUSTOM
acp                    acp               no        no
anthropic              anthropic         yes       no
google                 gemini            yes       no
openai                 openai-responses  yes       no
zai                    openai            yes       yes
zai-coding-plan        openai            yes       yes
...
```

`OVERRIDE` says whether `[providers.<id>]` may target the id, and
`CUSTOM` whether `[providers.custom.<id>]` may take it. An id that is
not listed takes only a custom table. Custom providers from your config
are not listed, since the command reports what this kage ships.

`kage providers --json` prints the same list as one object for
scripts, such as a tool that writes kage's config for it. Here it is
cut to its first entry:

```json
{"providers":[{"id":"acp","api":"acp","override":false,"custom":false}]}
```

Each entry carries `id`, `api`, `override` and `custom`. `api` is
`openai`, `anthropic` or `gemini` as a custom provider's `kind` names
them, `openai-responses` for the Responses API, or `acp`.

## validation

A `[providers]` section that kage would silently ignore is refused at
startup with exit status 1:

- A custom id must be non-empty lowercase letters, digits, or dashes.
- A custom id must not be a built-in id (`acp`, `anthropic`,
  `google`, `openai`). Override those providers
  with `[providers.<id>]` instead, and do the same for a catalog
  provider rather than reusing its id.
- A custom provider must declare at least one model.
- A `[providers.<id>]` override must name a provider that can be
  overridden (any built-in HTTP provider or catalog entry, not `acp`).

As with all config, environment variables override file values with
`KAGE_` plus `__` for nesting:
`KAGE_PROVIDERS__CUSTOM__MYLAMA__BASE_URL=http://10.0.0.2:8080/v1`
retargets `[providers.custom.mylama]`. Env var names cannot contain
dashes, so this form only reaches ids without one (`MYLAMA` maps to
`mylama`, never to `llama-local`). An env var whose nested name does
not match a declared id fails the whole config load, and kage stops
with the error.
