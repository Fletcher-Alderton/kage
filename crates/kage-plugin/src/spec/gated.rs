//! Capability-gated function bindings.

use super::{Field, Func, GatedFunc};

pub(super) const GATED: &[GatedFunc] = &[
    GatedFunc {
        cap: "session_write",
        func: Func {
            doc: &[
                "Metadata for every entry in the current session, in",
                "order, each `{ id, kind, role?, ts }`. Use it to find",
                "a rewind point. Requires the `session_write`",
                "capability.",
            ],
            path: "kage.session.entries",
            since: 1,
            params: &[],
            ret: Some("{ id: string, kind: string, role: string?, ts: string }[]"),
        },
    },
    GatedFunc {
        cap: "session_write",
        func: Func {
            doc: &[
                "Fork the current session at entry-id prefix `at` (or",
                "the latest entry when omitted) and reseat the live",
                "conversation onto the new fork between turns. This is",
                "the rewind move: base `fork` branches and stays;",
                "`fork_to` branches and goes there. Requires",
                "`session_write`.",
            ],
            path: "kage.session.fork_to",
            since: 1,
            params: &[Field {
                name: "at?",
                ty: "string",
                doc: "",
            }],
            ret: None,
        },
    },
    GatedFunc {
        cap: "session_write",
        func: Func {
            doc: &[
                "Reseat the live conversation onto an existing session",
                "(an id or path from `kage.session.list()`). The host",
                "validates and applies it between turns, consulting the",
                "`session_before_switch` veto. Requires `session_write`.",
            ],
            path: "kage.session.switch",
            since: 1,
            params: &[Field {
                name: "target",
                ty: "string",
                doc: "",
            }],
            ret: None,
        },
    },
    GatedFunc {
        cap: "exec",
        func: Func {
            doc: &[
                "Run a subprocess rooted at the workdir, no shell.",
                "Captures stdout/stderr and blocks until the process",
                "exits. `cwd` may not escape the workdir. The grant is",
                "coarse: any binary on the `PATH` may run with any args,",
                "with no command allowlist. Requires the `exec`",
                "capability.",
            ],
            path: "kage.exec",
            since: 1,
            params: &[Field {
                name: "spec",
                ty: "kage.ExecSpec",
                doc: "",
            }],
            ret: Some("kage.ExecResult"),
        },
    },
    GatedFunc {
        cap: "exec",
        func: Func {
            doc: &[
                "Declare an upstream ACP agent at runtime, mirroring",
                "`[acp.agents.<name>]` in config.toml. Core spawns the",
                "command, so this requires the `exec` capability.",
            ],
            path: "kage.acp.add_agent",
            since: 1,
            params: &[Field {
                name: "spec",
                ty: "kage.AcpAgentSpec",
                doc: "",
            }],
            ret: None,
        },
    },
    GatedFunc {
        cap: "exec",
        func: Func {
            doc: &[
                "Declare an MCP server at runtime, mirroring",
                "`[mcp.servers.<name>]` in config.toml. Core spawns the",
                "command, so this requires the `exec` capability.",
            ],
            path: "kage.mcp.add_server",
            since: 1,
            params: &[Field {
                name: "spec",
                ty: "kage.McpServerSpec",
                doc: "",
            }],
            ret: None,
        },
    },
    GatedFunc {
        cap: "exec",
        func: Func {
            doc: &[
                "Ask the host to restart a configured MCP server,",
                "including one that failed to start. Applied at the next",
                "run start. Requires the `exec` capability.",
            ],
            path: "kage.mcp.restart",
            since: 1,
            params: &[Field {
                name: "name",
                ty: "string",
                doc: "",
            }],
            ret: None,
        },
    },
    GatedFunc {
        cap: "env",
        func: Func {
            doc: &[
                "Read a process environment variable. Returns the value",
                "or `nil` when unset. The grant is coarse: any variable",
                "can be read (including secrets) and there is no setter.",
                "Requires the `env` capability.",
            ],
            path: "kage.env",
            since: 1,
            params: &[Field {
                name: "name",
                ty: "string",
                doc: "",
            }],
            ret: Some("string?"),
        },
    },
    GatedFunc {
        cap: "env",
        func: Func {
            doc: &[
                "Token the host holds for a provider id, or `nil`",
                "when nothing is stored. Same store the login flow",
                "writes. Requires the `env` capability.",
            ],
            path: "kage.credential",
            since: 2,
            params: &[Field {
                name: "provider",
                ty: "string",
                doc: "",
            }],
            ret: Some("string?"),
        },
    },
    GatedFunc {
        cap: "net",
        func: Func {
            doc: &[
                "HTTP GET. `opts` may carry headers and a body cap. Only",
                "SSRF filtering applies: the scheme must be http(s) and",
                "the host must resolve to a routable address; there is no",
                "host allow-list. Requires the `net` capability.",
            ],
            path: "kage.http.get",
            since: 1,
            params: &[
                Field {
                    name: "url",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "opts?",
                    ty: "kage.HttpRequestOpts",
                    doc: "",
                },
            ],
            ret: Some(
                "{ status: integer, body: string, content_type: string, headers: table<string, string>, truncated: boolean }",
            ),
        },
    },
    GatedFunc {
        cap: "net",
        func: Func {
            doc: &[
                "HTTP POST. `opts` carries headers and either `body`",
                "(string) or `json` (table; auto-serialized with",
                "`Content-Type: application/json`). The two are mutually",
                "exclusive. Same SSRF rules as GET. Requires `net`.",
            ],
            path: "kage.http.post",
            since: 1,
            params: &[
                Field {
                    name: "url",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "opts?",
                    ty: "kage.HttpRequestOpts",
                    doc: "",
                },
            ],
            ret: Some(
                "{ status: integer, body: string, content_type: string, headers: table<string, string>, truncated: boolean }",
            ),
        },
    },
    GatedFunc {
        cap: "net",
        func: Func {
            doc: &[
                "HTTP DELETE. `opts` carries headers (and optionally",
                "body, though most servers ignore it). Same SSRF rules as",
                "GET. Requires `net`.",
            ],
            path: "kage.http.delete",
            since: 1,
            params: &[
                Field {
                    name: "url",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "opts?",
                    ty: "kage.HttpRequestOpts",
                    doc: "",
                },
            ],
            ret: Some(
                "{ status: integer, body: string, content_type: string, headers: table<string, string>, truncated: boolean }",
            ),
        },
    },
    GatedFunc {
        cap: "net",
        func: Func {
            doc: &[
                "Streaming HTTP POST. The response is read frame-by-frame",
                "as Server-Sent Events and `on_event({event, data})` is",
                "called once per blank-line-terminated frame. Multi-line",
                "`data:` lines join with `\\n`. Returns when the stream",
                "ends. Same SSRF rules as GET. Requires `net`.",
            ],
            path: "kage.http.post_stream",
            since: 1,
            params: &[
                Field {
                    name: "url",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "opts?",
                    ty: "kage.HttpRequestOpts",
                    doc: "",
                },
                Field {
                    name: "on_event",
                    ty: "fun(ev: { event: string, data: string })",
                    doc: "",
                },
            ],
            ret: Some("{ status: integer, content_type: string }"),
        },
    },
    GatedFunc {
        cap: "crypto",
        func: Func {
            doc: &[
                "N cryptographically random bytes as a raw string.",
                "Requires the `crypto` capability.",
            ],
            path: "kage.crypto.random_bytes",
            since: 2,
            params: &[Field {
                name: "count",
                ty: "integer",
                doc: "",
            }],
            ret: Some("string"),
        },
    },
    GatedFunc {
        cap: "crypto",
        func: Func {
            doc: &[
                "SHA-256 of a byte string, as 32 raw bytes.",
                "Requires the `crypto` capability.",
            ],
            path: "kage.crypto.sha256",
            since: 2,
            params: &[Field {
                name: "data",
                ty: "string",
                doc: "",
            }],
            ret: Some("string"),
        },
    },
    GatedFunc {
        cap: "crypto",
        func: Func {
            doc: &[
                "SHA-512 of a byte string, as 64 raw bytes.",
                "Requires the `crypto` capability.",
            ],
            path: "kage.crypto.sha512",
            since: 2,
            params: &[Field {
                name: "data",
                ty: "string",
                doc: "",
            }],
            ret: Some("string"),
        },
    },
    GatedFunc {
        cap: "crypto",
        func: Func {
            doc: &[
                "HMAC-SHA256 of data under key, as 32 raw bytes.",
                "Requires the `crypto` capability.",
            ],
            path: "kage.crypto.hmac_sha256",
            since: 2,
            params: &[
                Field {
                    name: "key",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "data",
                    ty: "string",
                    doc: "",
                },
            ],
            ret: Some("string"),
        },
    },
    GatedFunc {
        cap: "crypto",
        func: Func {
            doc: &[
                "HKDF-SHA256 over the input keying material, as length",
                "raw bytes. An empty salt behaves as a zero salt.",
                "Requires the `crypto` capability.",
            ],
            path: "kage.crypto.hkdf_sha256",
            since: 2,
            params: &[
                Field {
                    name: "ikm",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "salt",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "info",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "length",
                    ty: "integer",
                    doc: "",
                },
            ],
            ret: Some("string"),
        },
    },
    GatedFunc {
        cap: "crypto",
        func: Func {
            doc: &[
                "AES-256-GCM decrypt. The key is 32 bytes, the iv 12",
                "bytes. Fails when authentication fails, without saying",
                "why. Requires the `crypto` capability.",
            ],
            path: "kage.crypto.aes256gcm_decrypt",
            since: 2,
            params: &[
                Field {
                    name: "key",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "iv",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "aad",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "ciphertext",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "tag",
                    ty: "string",
                    doc: "",
                },
            ],
            ret: Some("string"),
        },
    },
    GatedFunc {
        cap: "crypto",
        func: Func {
            doc: &[
                "Ed25519 signature of a message, as 64 raw bytes. The",
                "key is a PKCS#8 DER private key. Requires the `crypto`",
                "capability.",
            ],
            path: "kage.crypto.ed25519_sign",
            since: 2,
            params: &[
                Field {
                    name: "private_key_pkcs8",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "message",
                    ty: "string",
                    doc: "",
                },
            ],
            ret: Some("string"),
        },
    },
    GatedFunc {
        cap: "crypto",
        func: Func {
            doc: &[
                "Standard base64 of a byte string. Requires the `crypto`",
                "capability.",
            ],
            path: "kage.crypto.to_base64",
            since: 2,
            params: &[Field {
                name: "data",
                ty: "string",
                doc: "",
            }],
            ret: Some("string"),
        },
    },
    GatedFunc {
        cap: "crypto",
        func: Func {
            doc: &[
                "Bytes of standard base64. Requires the `crypto`",
                "capability.",
            ],
            path: "kage.crypto.from_base64",
            since: 2,
            params: &[Field {
                name: "text",
                ty: "string",
                doc: "",
            }],
            ret: Some("string"),
        },
    },
    GatedFunc {
        cap: "crypto",
        func: Func {
            doc: &[
                "Lower-case hex of a byte string. Requires the `crypto`",
                "capability.",
            ],
            path: "kage.crypto.to_hex",
            since: 2,
            params: &[Field {
                name: "data",
                ty: "string",
                doc: "",
            }],
            ret: Some("string"),
        },
    },
    GatedFunc {
        cap: "crypto",
        func: Func {
            doc: &[
                "Bytes of lower-case hex. Requires the `crypto`",
                "capability.",
            ],
            path: "kage.crypto.from_hex",
            since: 2,
            params: &[Field {
                name: "text",
                ty: "string",
                doc: "",
            }],
            ret: Some("string"),
        },
    },
    GatedFunc {
        cap: "session_write",
        func: Func {
            doc: &[
                "Append a custom entry to the session JSONL. `kind` is a",
                "namespaced string; `data` is any table (defaults to",
                "{}). Requires `session_write`: session content is not",
                "writable from the base surface.",
            ],
            path: "kage.session.append_entry",
            since: 1,
            params: &[
                Field {
                    name: "kind",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "data?",
                    ty: "table",
                    doc: "",
                },
            ],
            ret: None,
        },
    },
    GatedFunc {
        cap: "session_write",
        func: Func {
            doc: &[
                "Queue a synthetic message delivered between turns as a",
                "real user turn. Requires `session_write`: the queued",
                "text enters the conversation and the session file.",
            ],
            path: "kage.send_message",
            since: 1,
            params: &[
                Field {
                    name: "text",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "opts?",
                    ty: "kage.SendOpts",
                    doc: "",
                },
            ],
            ret: None,
        },
    },
    GatedFunc {
        cap: "provider",
        func: Func {
            doc: &[
                "Register a new LLM provider implementation. The handler",
                "sees the full outgoing request and produces the response",
                "stream. Requires the `provider` capability; reaching the",
                "network or reading credentials still needs `net`/`env`.",
            ],
            path: "kage.register_provider",
            since: 1,
            params: &[Field {
                name: "spec",
                ty: "kage.ProviderSpec",
                doc: "",
            }],
            ret: None,
        },
    },
    GatedFunc {
        cap: "provider",
        func: Func {
            doc: &[
                "Raise a typed provider error, so the agent loop retries",
                "the failures worth retrying. Never returns. A bare",
                "`error(\"...\")` is a permanent decode",
                "error; this is how a plugin says the failure was",
                "the pipe's fault. Kind is one of transport,",
                "rate_limited, http, auth, unknown_model, decode.",
                "transport and rate_limited are retried; http is",
                "retried on 5xx, 408 and 429; the rest are permanent.",
                "Pass a status for the http kind. An unknown kind",
                "raises, naming the known ones.",
            ],
            path: "kage.provider_error",
            since: 2,
            params: &[
                Field {
                    name: "kind",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "message",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "status",
                    ty: "integer?",
                    doc: "HTTP status, for the http kind.",
                },
            ],
            ret: None,
        },
    },
    GatedFunc {
        cap: "fs_write",
        func: Func {
            doc: &[
                "Write a file under the workdir. Same restriction as",
                "read. Requires the `fs_write` capability; `kage.fs.read`",
                "stays on the base surface.",
            ],
            path: "kage.fs.write",
            since: 1,
            params: &[
                Field {
                    name: "path",
                    ty: "string",
                    doc: "",
                },
                Field {
                    name: "content",
                    ty: "string",
                    doc: "",
                },
            ],
            ret: None,
        },
    },
];
