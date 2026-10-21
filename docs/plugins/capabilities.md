# capabilities

The plugin sandbox is closed by default: no subprocesses, no
filesystem outside the workdir, no writing files at all, no rewriting
the live session, no conversation text, no environment variables and
no network. A few
plugins genuinely need more. Those powers are **capabilities**: opt-in,
per-plugin, and only ever attached to the one plugin that was granted
them.

## two-sided opt-in

A capability is active only when both sides agree:

1. The user grants it to a named plugin in `config.toml`. The name is
   the plugin file's stem (`rewind.lua` -> `rewind`), or a package's
   directory name.

   ```toml
   [plugins.capabilities]
   rewind = ["session_write", "exec"]
   ```

2. The plugin asks for it at load and adapts to the answer:

   ```lua
   local caps = kage.request_capabilities({ "session_write", "exec" })
   if not caps.session_write then
     kage.notify("rewind: disabled (grant session_write)")
     return
   end
   local files = caps.exec  -- optional extra, degrade if absent
   ```

`request_capabilities` returns a truthful `{ name = granted }` table:
a capability is `true` only if that exact plugin was granted it. The
elevated API is then attached onto that plugin's `kage` alone. Another
plugin cannot see it, even if it asks. An unknown capability name
raises rather than silently resolving to `false`, so a config typo is
loud.

A [package](/plugins/packages) adds a third side: its `plugin.toml`
lists every capability it may hold. Asking for one it does not list
raises, whatever the config grants, so what a package declares is all
it can ever get.

## the capabilities

### `session_write`

Inspect and reseat the live conversation.

| call | effect |
| --- | --- |
| `kage.session.entries()` | metadata for every entry in the current session, in order: `{ id, kind, role?, ts }`. `kind` is one of `header`, `message`, `thinking_level_change`, `model_change`, `compaction`, `label`, `title`, `custom`; `role` rides only on `message` entries and is one of `user`, `assistant`, `tool_result`, `system`. The navigation index a checkpoint picker orders its rows by. |
| `kage.session.switch(target)` | reseat onto an existing session (an id or a path from `kage.session.list()`). |
| `kage.session.fork_to(at?)` | fork the current session at entry-id prefix `at` (latest if omitted) and land on the new branch. The rewind move: base `kage.session.fork` branches and stays, while `fork_to` branches and goes there. |
| `kage.session.append_entry(kind, data?)` | append a custom entry: a namespaced `kind` such as `"my-plugin:bookmark"` plus any JSON-serializable `data` table (`{}` by default). The host writes it between turns. |
| `kage.send_message(text, opts?)` | queue a synthetic user message the host delivers between turns. `opts` takes `trigger_turn` (default `true`) and `deliver_as` (only `"user"` is wired). |

`entries()` returns metadata only (ids, kinds and timestamps, no
message text) because it is a navigation index, not a transcript
reader. `switch` and `fork_to` only *request* a reseat. The host
applies it between turns, after consulting the `session_before_switch`
veto, so a plugin can confirm or block its own reseat.

### `exec`

```lua
local r = kage.exec({ cmd = "git", args = { "stash", "create" } })
-- r = { code = 0, timed_out = false, truncated = false, stdout = "...", stderr = "" }
```

Spawns a subprocess **directly, with no shell**, so there is no quoting
or injection surface. `cmd` resolves through `PATH`. The working
directory defaults to the host workdir, and `cwd` may name a directory
under it but never escape it (the same check `kage.fs` uses). The call
blocks until the process exits and returns its captured output, the
way `kage.http.get` blocks. A process still running after
`timeout_secs` seconds (30 by default, at least 1) is killed and the
result has `timed_out = true`. `truncated` is `true` when stdout or
stderr passed the 1 MiB cap per stream and the rest of that stream
was dropped. `code` is `-1` when a signal ended the
process.

The grant is coarse: a granted plugin may run any program with any
arguments.

The grant also covers the declarative surfaces that make the *host*
spawn a process: `kage.mcp.add_server(spec)` and
`kage.mcp.restart(name)` (see [mcp](/guide/mcp#declaring-servers-from-a-plugin))
and `kage.acp.add_agent(spec)` (see
[acp](/editors/acp-client#configure-from-a-plugin)). Declaring a
server or agent is naming a command for kage to run, so it needs the
same grant as running one.

### `fs_write`

```lua
kage.fs.write("notes/todo.txt", "ship it\n")
```

Attaches `kage.fs.write` alongside the always-available
`kage.fs.read`: a plugin can always look, but writing takes the
explicit grant. Paths resolve under the workdir and the write is
confined to it, including through symlinks; missing parent
directories are created and then re-verified against the workdir.

### `context`

Subscribe to the events that carry conversation text or tool payloads,
and read the system prompt.

| surface | effect |
| --- | --- |
| `transform_context` | see and rewrite the message history each turn |
| `before_provider_request` | see and rewrite the serialized provider request |
| `compact_prepare` | steer or replace the compaction summary |
| `before_agent_start` | see the system prompt and first user message |
| `message_start` / `message_update` / `message_end` / `after_provider_response` | observe the message stream |
| `tool_call` / `tool_result` | observe tool calls and their full outputs |
| `user` | receive custom `user` events |
| `kage.config().system_prompt` | the full system prompt; without the grant the key is absent |

A `kage.on` (or `kage.api.autocmd_create`) registration for one of
these from an ungranted plugin is dropped with a warning naming the
capability to grant. The rest of the event table (`agent_start`,
`turn_start`, option and theme changes, ...) stays open to every
plugin, because those payloads carry no conversation text.

### `provider`

```lua
kage.register_provider({ id = "mine", stream = function(req) ... end })
```

Attaches `kage.register_provider`, which teaches kage a new LLM
provider implementation. This is the widest power in the tier: the
plugin's `stream` function sees every request body (system prompt,
full history, tools) and produces whatever the model is supposed to
say. A streaming provider usually makes outbound requests via
`kage.http.post_stream`, so grant `net` alongside it. See
[providers](/plugins/api#providers).

The same grant attaches `kage.provider_error`, which raises a
typed error so the loop retries the failures worth retrying. A bare
`error("...")` is a permanent decode error, so a provider that
fails on a connect timeout would otherwise never be retried. See
[`provider_error`](/plugins/api#kageprovider_errorkind-message-status).

### `env`

```lua
local token = kage.env("GITHUB_TOKEN")  -- string, or nil when unset
```

Reads one variable from the host process environment. It returns `nil`
when the variable is unset and raises when the value is not valid
UTF-8. Access is read-only, with no setter. There is no
per-variable allowlist, so a granted plugin can read every variable,
including secrets such as provider API keys. Grant it only to plugins
you trust.

`kage.credential(provider)` returns the token the host holds for a
provider id, the same store the login flow writes, or `nil` when
nothing is stored. It is attached by the same `env` grant: stored
tokens are secrets of the same class as environment secrets.

### `net`

```lua
local res = kage.http.get("https://example.com/status")
-- res = { status = 200, body = "...", content_type = "...", headers = { ... }, truncated = false }
```

Attaches the request helpers to `kage.http`. Without the grant,
`kage.http` is an empty table and `kage.http.get` is `nil`.

| call | effect |
| --- | --- |
| `kage.http.get(url, opts?)` | GET request. |
| `kage.http.post(url, opts?)` | POST request. |
| `kage.http.delete(url, opts?)` | DELETE request. |
| `kage.http.post_stream(url, opts, fn)` | POST that calls `fn` with `{ event, data }` for each server-sent event frame. Returns `{ status, content_type }`. |

`opts` may carry `headers`, `body`, `json` (a table sent as JSON,
instead of `body`), `max_bytes` and `timeout_secs`. `get`, `post` and
`delete` return `{ status, body, content_type, headers, truncated }`
and give up after `timeout_secs` seconds (30 by default). `headers`
maps each lowercase header name to its value, with the values of a
repeated header joined by `, `. A `set-cookie` value can hold commas of
its own, so it cannot be split back apart. The body is capped at
`max_bytes` (2 MB by default, 32 MB for `post_stream`), and `truncated`
is `true` when the cap was hit. `post_stream` bounds only connecting
and sending, so a long stream is never cut off.

Every request passes the same SSRF check as the built-in `web_fetch`
tool: the scheme must be `http` or `https` and the host must resolve to
a routable address. There is no host allowlist beyond that.

### `crypto`

```lua
local digest = kage.crypto.sha256("abc")
-- digest is 32 raw bytes; kage.crypto.to_hex(digest) is ba7816...
```

Attaches synchronous primitives to `kage.crypto`. Without the grant,
`kage.crypto` is an empty table and `kage.crypto.sha256` is `nil`.

| call | effect |
| --- | --- |
| `kage.crypto.random_bytes(n)` | `n` cryptographically random bytes. `n` is 1 to 1048576. |
| `kage.crypto.sha256(data)` | SHA-256 digest, 32 raw bytes. |
| `kage.crypto.sha512(data)` | SHA-512 digest, 64 raw bytes. |
| `kage.crypto.hmac_sha256(key, data)` | HMAC-SHA256, 32 raw bytes. |
| `kage.crypto.hkdf_sha256(ikm, salt, info, length)` | HKDF-SHA256, `length` raw bytes (1 to 8160). An empty salt behaves as a zero salt. |
| `kage.crypto.aes256gcm_decrypt(key, iv, aad, ciphertext, tag)` | AES-256-GCM plaintext. The key is 32 bytes, the iv 12 bytes. Fails when authentication fails, without saying why. |
| `kage.crypto.ed25519_sign(private_key_pkcs8, message)` | Ed25519 signature, 64 raw bytes. The key is a PKCS#8 DER private key. |
| `kage.crypto.to_base64(data)` / `from_base64(text)` | standard base64 encode and decode. |
| `kage.crypto.to_hex(data)` / `from_hex(text)` | lower-case hex encode and decode. |

All inputs and outputs are byte strings. Lengths are validated and
failures are generic: an error never echoes the input that caused it.
The grant is coarse: a granted plugin may call any primitive with any
inputs. The work runs on the host, so long loops over these calls
never consume the Lua instruction budget the way a pure Lua loop
would.

## why this is safe enough

- Closed by default. A plugin you never granted anything to is
  exactly as confined as before this tier existed.
- Per-plugin attachment. A grant to `rewind` does nothing for any
  other plugin in the same runtime.
- No shell in `exec`, workdir-scoped `cwd` and `fs` writes.
- `net` requests are SSRF-checked and size-capped.
- Conversation text (message stream, history transforms, the system
  prompt) needs `context`; session reseats are host-applied between
  turns and pass through the `session_before_switch` veto.

## see it in use

`plugins/examples/rewind.lua` combines `session_write` and `exec`: it
uses `entries()` to order its checkpoints against the conversation and
`exec` to git-snapshot tracked files every `turn_end`, so
`/checkpoint-restore` can bring the working tree along when the
built-in `/undo` or `/rewind` moves the conversation back. See
[examples](/plugins/examples#conversation-and-file-rewind).
