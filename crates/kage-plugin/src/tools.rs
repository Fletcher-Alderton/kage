//! `kage.register_tool` and the `Tool` adapter that backs into Lua.
//!
//! Plugins call
//! ```lua
//! kage.register_tool({
//!     name = "echo",
//!     description = "echoes its input",
//!     schema = { type = "object" },
//!     risk = "read",
//!     execute = function(input) return { text = input.msg } end,
//! })
//! ```
//! and the runtime stores a [`LuaTool`] that the host can hand to a
//! `ToolRegistry`. A call runs its handler as a driven coroutine on the
//! runtime's Lua owner thread (see [`crate::tasks`]), so while it waits
//! on `kage.http` or `kage.sleep_ms` other calls and plugins run. The
//! calling thread waits for the result; once the call is cancelled it
//! gives up, and the handler is dropped where it was waiting.

use std::sync::{Arc, Mutex};

use crossbeam_channel::select_biased;
use kage_core::{Risk, ToolOutput, sync::lock};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};
use mlua::{Function, Lua, RegistryKey, Table, Value};

use crate::api::{LogLevel, SharedHostLog, json_to_lua, lua_to_json};
use crate::error::PluginError;
use crate::host::{self, LuaHost, WeakHost};
use crate::tasks::{self, Abort};

/// Shared collection of tools registered by plugins. Cloned into the Lua
/// callback so registrations made during `dofile` accumulate here.
pub type RegisteredTools = Arc<Mutex<Vec<Arc<dyn Tool>>>>;

/// Construct an empty registered-tools collection.
#[must_use]
pub fn registered_tools() -> RegisteredTools {
    Arc::new(Mutex::new(Vec::new()))
}

/// Real name to advertised name pairs registered by plugins via
/// `kage.rename_tool`. Collected during `dofile` like tools.
pub type RegisteredRenames = Arc<Mutex<Vec<(String, String)>>>;

/// Construct an empty registered-renames collection.
#[must_use]
pub fn registered_renames() -> RegisteredRenames {
    Arc::new(Mutex::new(Vec::new()))
}

/// A `Tool` whose `execute` runs inside the plugin runtime's Lua state.
pub struct LuaTool {
    name: String,
    description: String,
    schema: serde_json::Value,
    risk: Risk,
    parallel: bool,
    host: LuaHost,
    sink: SharedHostLog,
    handler_key: Arc<RegistryKey>,
}

impl std::fmt::Debug for LuaTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaTool")
            .field("name", &self.name)
            .field("risk", &self.risk)
            .finish_non_exhaustive()
    }
}

impl Tool for LuaTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    fn risk(&self) -> Risk {
        self.risk
    }

    fn execution_mode(&self) -> Option<ExecMode> {
        self.parallel.then_some(ExecMode::Parallel)
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        if cx.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let failed = |e: &dyn std::fmt::Display| {
            ToolError::Other(format!("plugin tool '{}': {e}", self.name))
        };
        let abort = Arc::new(Abort::default());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let call = Call {
            name: self.name.clone(),
            sink: Arc::clone(&self.sink),
            key: Arc::clone(&self.handler_key),
            abort: Arc::clone(&abort),
            tx,
        };
        match self
            .host
            .call_cancellable(cx.cancel_flag(), move |lua| call.start(lua, &input))
        {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(e))) => return Err(e),
            Ok(None) => return Err(ToolError::Cancelled),
            Err(e) => return Err(failed(&e)),
        }
        let watch = cx.cancel_flag().watch();
        select_biased! {
            recv(rx) -> output => output.map_err(|_| failed(&"the plugin runtime stopped")),
            recv(watch.receiver()) -> _ => {
                abort.abort();
                Err(ToolError::Cancelled)
            }
        }
    }
}

/// One call of a plugin tool, on its way to the owner thread.
struct Call {
    name: String,
    sink: SharedHostLog,
    key: Arc<RegistryKey>,
    abort: Arc<Abort>,
    tx: crossbeam_channel::Sender<ToolOutput>,
}

impl Call {
    /// Start the handler on `input` as a driven coroutine, whose output
    /// goes to `tx` unless the call is aborted first.
    fn start(self, lua: &Lua, input: &serde_json::Value) -> Result<(), ToolError> {
        let Self {
            name,
            sink,
            key,
            abort,
            tx,
        } = self;
        let failed = |e: mlua::Error| ToolError::Other(format!("plugin tool '{name}': {e}"));
        let func: Function = lua.registry_value(&key).map_err(failed)?;
        let input = json_to_lua(lua, input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let run = tasks::drive::<Value>(lua, func, input).map_err(failed)?;
        tasks::spawn(lua, async move {
            let Some(returned) = tasks::abortable(run, abort).await else {
                return;
            };
            let output = match returned {
                Ok(value) => value_to_output(value),
                Err(err) => {
                    lock(&sink).log(
                        LogLevel::Error,
                        &format!("plugin tool '{name}' raised: {err}"),
                    );
                    ToolOutput {
                        is_error: true,
                        text: err.to_string(),
                        structured: None,
                        terminate: false,
                        media: Vec::new(),
                    }
                }
            };
            let _ = tx.send(output);
        });
        Ok(())
    }
}

fn value_to_output(value: Value) -> ToolOutput {
    match value {
        Value::Nil => ToolOutput {
            is_error: false,
            text: String::new(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        },
        Value::String(s) => ToolOutput {
            is_error: false,
            text: s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        },
        Value::Boolean(b) => ToolOutput {
            is_error: false,
            text: b.to_string(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        },
        Value::Integer(i) => ToolOutput {
            is_error: false,
            text: i.to_string(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        },
        Value::Number(n) => ToolOutput {
            is_error: false,
            text: n.to_string(),
            structured: None,
            terminate: false,
            media: Vec::new(),
        },
        Value::Table(t) => table_to_output(&t),
        other => {
            let json = lua_to_json(other).unwrap_or(serde_json::Value::Null);
            ToolOutput {
                is_error: false,
                text: json.to_string(),
                structured: Some(json),
                terminate: false,
                media: Vec::new(),
            }
        }
    }
}

fn table_to_output(table: &Table) -> ToolOutput {
    let is_error: bool = table.get("is_error").unwrap_or(false);
    let text: Option<String> = table.get("text").ok();
    let structured: Option<Value> = table.get("structured").ok();
    let structured_json = structured.and_then(|v| lua_to_json(v).ok());
    if let Some(text) = text {
        return ToolOutput {
            is_error,
            text,
            structured: structured_json,
            terminate: false,
            media: table_media(table),
        };
    }
    // No `text` field: serialize the table itself as JSON for the model.
    let as_json = lua_to_json(Value::Table(table.clone())).unwrap_or(serde_json::Value::Null);
    ToolOutput {
        is_error,
        text: as_json.to_string(),
        structured: Some(as_json),
        terminate: false,
        media: Vec::new(),
    }
}

/// The `media` list of a tool's result table: `{ mime, data }` entries,
/// `data` base64. Entries missing either are skipped.
fn table_media(table: &Table) -> Vec<kage_core::ToolMedia> {
    let Ok(list) = table.get::<Table>("media") else {
        return Vec::new();
    };
    list.sequence_values::<Table>()
        .flatten()
        .filter_map(|entry| {
            let mime: String = entry.get("mime").ok()?;
            let data: String = entry.get("data").ok()?;
            (!mime.is_empty() && !data.is_empty()).then_some(kage_core::ToolMedia { mime, data })
        })
        .collect()
}

/// Install `kage.register_tool` on the running Lua state. The closure
/// pushes each registered [`LuaTool`] into `registered`, which the host
/// later drains via [`crate::PluginRuntime::registered_tools`].
pub(crate) fn install_register_tool(
    lua: &Lua,
    host: WeakHost,
    sink: SharedHostLog,
    registered: &RegisteredTools,
) -> Result<(), PluginError> {
    install_tool_fn(lua, host, sink, registered, "register_tool")
}

/// Install `kage.override_tool` on the running Lua state. Semantically
/// identical to `register_tool` from the plugin's perspective; the host
/// distinguishes overrides via
/// [`crate::PluginRuntime::registered_tool_overrides`] and replaces
/// matching built-ins (logging a warning when no existing tool with the
/// supplied name is present).
pub(crate) fn install_override_tool(
    lua: &Lua,
    host: WeakHost,
    sink: SharedHostLog,
    overrides: &RegisteredTools,
) -> Result<(), PluginError> {
    install_tool_fn(lua, host, sink, overrides, "override_tool")
}

fn install_tool_fn(
    lua: &Lua,
    host: WeakHost,
    sink: SharedHostLog,
    bucket: &RegisteredTools,
    fn_name: &'static str,
) -> Result<(), PluginError> {
    let bucket = Arc::downgrade(bucket);
    let kage: Table = lua.globals().get("kage")?;
    kage.set(
        fn_name,
        lua.create_function(move |lua, spec: Table| {
            let name: String = spec.get("name")?;
            let description: String = spec.get("description")?;
            let risk_str: Option<String> = spec.get("risk")?;
            let risk = parse_risk(risk_str.as_deref())?;
            let parallel: Option<bool> = spec.get("parallel")?;
            let schema_value: Value = spec.get("schema").unwrap_or(Value::Nil);
            let schema = lua_to_json(schema_value).unwrap_or(serde_json::Value::Null);
            let execute: Function = spec.get("execute")?;
            let key = lua.create_registry_value(execute)?;
            let tool = LuaTool {
                name,
                description,
                schema,
                risk,
                parallel: parallel.unwrap_or(false),
                host: host.upgrade()?,
                sink: sink.clone(),
                handler_key: Arc::new(key),
            };
            host::upgrade(&bucket)?
                .lock()
                .map_err(|_| mlua::Error::external("plugin tools registry poisoned"))?
                .push(Arc::new(tool) as Arc<dyn Tool>);
            Ok(())
        })?,
    )?;
    Ok(())
}

/// Install `kage.rename_tool` on the running Lua state. Plugins call
/// it with `{ from = "shell", to = "run_command" }` so the model sees
/// only `to`; execution keeps running the `from` tool.
pub(crate) fn install_rename_tool(
    lua: &Lua,
    bucket: &RegisteredRenames,
) -> Result<(), PluginError> {
    let bucket = Arc::downgrade(bucket);
    let kage: Table = lua.globals().get("kage")?;
    kage.set(
        "rename_tool",
        lua.create_function(move |_lua, spec: Table| {
            let from: String = spec.get("from")?;
            let to: String = spec.get("to")?;
            if from.is_empty() || to.is_empty() {
                return Err(mlua::Error::runtime(
                    "rename_tool needs non-empty `from` and `to`",
                ));
            }
            host::upgrade(&bucket)?
                .lock()
                .map_err(|_| mlua::Error::external("plugin renames registry poisoned"))?
                .push((from, to));
            Ok(())
        })?,
    )?;
    Ok(())
}

fn parse_risk(raw: Option<&str>) -> Result<Risk, mlua::Error> {
    match raw {
        None | Some("read") => Ok(Risk::Read),
        Some("write") => Ok(Risk::Write),
        Some("exec") => Ok(Risk::Exec),
        Some("network") => Ok(Risk::Network),
        Some(other) => Err(mlua::Error::runtime(format!(
            "unknown risk \"{other}\" (expected read, write, exec, network)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use kage_core::CancelFlag;
    use serde_json::json;

    use super::*;
    use crate::PluginRuntime;

    #[test]
    fn register_tool_appends_to_registry() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_tool({
                name = 'echo',
                description = 'echo back',
                schema = { type = 'object' },
                risk = 'read',
                execute = function(input) return 'hi:' .. (input.msg or '?') end,
            })
            ",
        )
        .unwrap();
        let tools = rt.registered_tools();
        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        assert_eq!(tool.name(), "echo");
        assert_eq!(tool.risk(), Risk::Read);
    }

    #[test]
    fn a_tool_runs_in_parallel_only_when_it_says_so() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_tool({ name = 'one', description = '', execute = function() end })
            kage.register_tool({ name = 'many', description = '', parallel = true,
                execute = function() end })
            ",
        )
        .unwrap();
        let mode = |name: &str| {
            rt.registered_tools()
                .into_iter()
                .find(|tool| tool.name() == name)
                .unwrap()
                .execution_mode()
        };
        assert_eq!(mode("one"), None);
        assert_eq!(mode("many"), Some(ExecMode::Parallel));
    }

    #[test]
    fn lua_tool_execute_returns_text() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_tool({
                name = 'add',
                description = 'sum of a + b',
                schema = { type = 'object' },
                risk = 'read',
                execute = function(input) return tostring(input.a + input.b) end,
            })
            ",
        )
        .unwrap();
        let tool = rt.registered_tools().pop().unwrap();
        let cancel = CancelFlag::new();
        let workdir = PathBuf::from("/tmp");
        let cx = ToolContext::new(&workdir, &cancel);
        let out = tool.execute(json!({"a": 2, "b": 3}), &cx).unwrap();
        assert!(!out.is_error);
        assert_eq!(out.text, "5");
    }

    #[test]
    fn lua_tool_returns_table_with_structured_output() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_tool({
                name = 'meta',
                description = 'returns table with text and structured',
                schema = {},
                execute = function() return { text = 'ok', structured = { count = 7 } } end,
            })
            ",
        )
        .unwrap();
        let tool = rt.registered_tools().pop().unwrap();
        let cancel = CancelFlag::new();
        let workdir = PathBuf::from("/tmp");
        let cx = ToolContext::new(&workdir, &cancel);
        let out = tool.execute(json!({}), &cx).unwrap();
        assert_eq!(out.text, "ok");
        assert_eq!(out.structured.as_ref().unwrap()["count"], 7);
    }

    #[test]
    fn lua_tool_lua_error_becomes_is_error_output() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_tool({
                name = 'oops',
                description = 'always fails',
                schema = {},
                execute = function() error('boom') end,
            })
            ",
        )
        .unwrap();
        let tool = rt.registered_tools().pop().unwrap();
        let cancel = CancelFlag::new();
        let workdir = PathBuf::from("/tmp");
        let cx = ToolContext::new(&workdir, &cancel);
        let out = tool.execute(json!({}), &cx).unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("boom"));
    }

    #[test]
    fn lua_tool_respects_cancel() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_tool({
                name = 't',
                description = 'noop',
                schema = {},
                execute = function() return 'never' end,
            })
            ",
        )
        .unwrap();
        let tool = rt.registered_tools().pop().unwrap();
        let cancel = CancelFlag::new();
        cancel.cancel();
        let workdir = PathBuf::from("/tmp");
        let cx = ToolContext::new(&workdir, &cancel);
        let err = tool.execute(json!({}), &cx).unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[test]
    fn a_result_table_can_carry_media() {
        let lua = Lua::new();
        let table: Table = lua
            .load(
                "return { text = 'drawn', media = {
                    { mime = 'image/png', data = 'iVBO' },
                    { mime = 'image/png' },
                } }",
            )
            .eval()
            .unwrap();
        let output = table_to_output(&table);
        assert_eq!(output.text, "drawn");
        assert_eq!(
            output.media,
            [kage_core::ToolMedia {
                mime: "image/png".into(),
                data: "iVBO".into(),
            }],
            "an entry without data is skipped"
        );
    }

    #[test]
    fn risk_parses_known_strings() {
        assert_eq!(parse_risk(Some("read")).unwrap(), Risk::Read);
        assert_eq!(parse_risk(Some("write")).unwrap(), Risk::Write);
        assert_eq!(parse_risk(Some("exec")).unwrap(), Risk::Exec);
        assert_eq!(parse_risk(Some("network")).unwrap(), Risk::Network);
        assert_eq!(parse_risk(None).unwrap(), Risk::Read);
    }

    #[test]
    fn unknown_risk_raises_with_expected_values() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt
            .eval(
                r"
                kage.register_tool({
                    name = 'bad',
                    description = '',
                    schema = {},
                    risk = 'danger',
                    execute = function() return '' end,
                })
                ",
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains(r#"unknown risk "danger" (expected read, write, exec, network)"#),
            "{err}"
        );
        assert!(rt.registered_tools().is_empty());
    }

    #[test]
    fn exec_risk_registers_as_exec() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_tool({
                name = 'run',
                description = '',
                schema = {},
                risk = 'exec',
                execute = function() return '' end,
            })
            ",
        )
        .unwrap();
        assert_eq!(rt.registered_tools()[0].risk(), Risk::Exec);
    }

    #[test]
    fn override_tool_appends_to_overrides_registry_not_register_one() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.override_tool({
                name = 'shell',
                description = 'filtered bash',
                schema = { type = 'object' },
                risk = 'write',
                execute = function() return 'noop' end,
            })
            ",
        )
        .unwrap();
        assert!(
            rt.registered_tools().is_empty(),
            "override_tool should not populate registered_tools"
        );
        let overrides = rt.registered_tool_overrides();
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].name(), "shell");
        assert_eq!(overrides[0].risk(), Risk::Write);
    }

    #[test]
    fn override_tool_and_register_tool_are_independent() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_tool({
                name = 'new_tool',
                description = '',
                schema = {},
                execute = function() return '' end,
            })
            kage.override_tool({
                name = 'shell',
                description = '',
                schema = {},
                execute = function() return '' end,
            })
            ",
        )
        .unwrap();
        assert_eq!(rt.registered_tools().len(), 1);
        assert_eq!(rt.registered_tool_overrides().len(), 1);
    }

    #[test]
    fn rename_tool_collects_real_to_advertised_pairs() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.rename_tool({ from = 'shell', to = 'run_command' })
            kage.rename_tool({ from = 'web_fetch', to = 'browse' })
            ",
        )
        .unwrap();
        assert_eq!(
            rt.registered_renames(),
            vec![
                ("shell".to_owned(), "run_command".to_owned()),
                ("web_fetch".to_owned(), "browse".to_owned()),
            ]
        );
    }

    #[test]
    fn rename_tool_rejects_empty_names() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt
            .eval("kage.rename_tool({ from = '', to = 'x' })")
            .unwrap_err();
        assert!(err.to_string().contains("non-empty"), "{err}");
        assert!(rt.registered_renames().is_empty());
    }
}
