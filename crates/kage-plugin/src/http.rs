//! `kage.http` - sandboxed HTTP fetch helpers.
//!
//! All requests share the SSRF guards used by the built-in `web_fetch`
//! tool: the URL must be `http(s)`, the host must resolve to a routable
//! address, and the response body is capped to keep one malicious link
//! from filling memory. When the operator's environment names an egress
//! proxy (`HTTPS_PROXY`), requests ride it instead of dialing directly:
//! the proxy resolves the target's name on its own side, so local
//! address vetting is replaced by the proxy's egress policy, with only
//! a literal-form check kept on the target.
//!
//! Available:
//!
//! ```lua
//! local res = kage.http.get(url)
//! local res = kage.http.post(url, { headers = {...}, body = "...", json = {...}, max_bytes = N, timeout_secs = S })
//! local res = kage.http.delete(url, { headers = {...} })
//! local res = kage.http.post_stream(url, opts, function(ev) ... end)
//! ```
//!
//! `get` / `post` / `delete` give up after `timeout_secs` seconds
//! (default 30); `post_stream` bounds only the connection phases so
//! the stream itself can run long.
//!
//! `get` / `post` / `delete` return `{ status, body, content_type, headers,
//! truncated }`, `headers` keyed by lowercase name with a repeated
//! header's values joined by `, `.
//! `post_stream` returns `{ status, content_type }` and dispatches each
//! decoded SSE frame to the callback as `{ event = name, data = payload }`
//! (multi-line `data:` lines are joined with `\n`). The callback is
//! invoked once per blank-line-terminated frame.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kage_core::sync::lock;
use kage_tools::ssrf;
use mlua::{Function, Lua, Table, Value};

use crate::api::lua_to_json;
use crate::capabilities::{Capability, CapabilityRegistry};
use crate::error::PluginError;
use crate::tasks;

/// Default cap on response body bytes for non-streaming requests.
const DEFAULT_MAX_BYTES: u64 = 2_000_000;

/// Default total-bytes cap for streamed responses. Larger than the
/// non-streaming cap since a streaming chat response can legitimately
/// run into the tens of megabytes over many tokens.
const DEFAULT_STREAM_MAX_BYTES: u64 = 32_000_000;

/// Default whole-request budget for non-streaming requests without an
/// explicit `timeout_secs`.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Connection-phase budget for streaming requests: resolve, connect,
/// and sending the request each get this long, but the response stream
/// itself stays unbounded so a long SSE chat is never cut off.
const STREAM_PHASE_TIMEOUT: Duration = Duration::from_secs(10);

/// Install the base `kage.http` placeholder table.
///
/// The request helpers themselves are gated behind the `net` capability
/// and attached per-plugin by `register`. The empty base table keeps
/// `kage.http` resolvable so an ungranted plugin sees `kage.http.get`
/// as `nil` rather than indexing a nil value, mirroring how
/// `kage.session` carries gated functions on top of a base table.
pub fn install_http(lua: &Lua) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    kage.set("http", lua.create_table()?)?;
    Ok(())
}

/// Register the `net` capability installer.
///
/// Run (via `request_capabilities`) against a granted plugin's `kage`
/// proxy, it shadows the empty base `kage.http` with a table carrying
/// `get` / `post` / `delete` / `post_stream`, so only a plugin the user
/// granted `net` can make outbound requests.
pub(crate) fn register(registry: &CapabilityRegistry) {
    let mut reg = lock(registry);
    reg.entry(Capability::Net)
        .or_default()
        .push(Box::new(|lua: &Lua, pkage: &Table| {
            pkage.set("http", build_http_table(lua)?)?;
            Ok(())
        }));
}

/// Build the populated `kage.http` table. The helpers are stateless
/// (SSRF guard plus a blocking `ureq` request), so the same table shape
/// is attached onto each granted plugin's proxy.
fn build_http_table(lua: &Lua) -> mlua::Result<Table> {
    let http = lua.create_table()?;
    http.set("get", simple_function(lua, "GET", "http.get")?)?;
    http.set("post", simple_function(lua, "POST", "http.post")?)?;
    http.set("delete", simple_function(lua, "DELETE", "http.delete")?)?;
    http.set(
        "post_stream",
        lua.create_async_function(
            |lua, (url, opts, on_event): (String, Option<Table>, Function)| async move {
                let fail =
                    |err: String| mlua::Error::external(format!("http.post_stream {url}: {err}"));
                let request = build_request(opts.as_ref()).map_err(fail)?;
                let max_bytes = request.max_bytes.unwrap_or(DEFAULT_STREAM_MAX_BYTES);
                if tasks::is_driven(&lua) {
                    stream_driven(lua.clone(), url.clone(), request, max_bytes, on_event)
                        .await
                        .map_err(fail)
                } else {
                    stream_request(&lua, &url, &request, max_bytes, &on_event).map_err(fail)
                }
            },
        )?,
    )?;
    Ok(http)
}

/// One of `get` / `post` / `delete`. Inside a driven coroutine the
/// request runs on a worker thread while the coroutine yields; anywhere
/// else it blocks.
fn simple_function(lua: &Lua, method: &'static str, name: &'static str) -> mlua::Result<Function> {
    lua.create_async_function(
        move |lua, (url, opts): (String, Option<Table>)| async move {
            let fail = |err: String| mlua::Error::external(format!("{name} {url}: {err}"));
            let request = build_request(opts.as_ref()).map_err(fail)?;
            let max_bytes = request.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
            let result = if tasks::is_driven(&lua) {
                let target = url.clone();
                tasks::off_thread_stoppable(&lua, move |stop| {
                    simple_request(method, &target, Some(&request), max_bytes, stop)
                })
                .await
                .unwrap_or_else(|| Err("the request thread panicked".to_owned()))
            } else {
                simple_request(
                    method,
                    &url,
                    Some(&request),
                    max_bytes,
                    &AtomicBool::new(false),
                )
            };
            simple_result_to_table(&lua, &result.map_err(fail)?)
        },
    )
}

/// Caller-supplied request details parsed out of the Lua `opts` table.
#[derive(Debug, Default)]
struct RequestSpec {
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    content_type: Option<String>,
    max_bytes: Option<u64>,
    timeout: Option<Duration>,
}

fn build_request(opts: Option<&Table>) -> Result<RequestSpec, String> {
    let mut spec = RequestSpec::default();
    let Some(opts) = opts else {
        return Ok(spec);
    };

    if let Ok(headers) = opts.get::<Table>("headers") {
        for pair in headers.pairs::<String, String>() {
            let (k, v) = pair.map_err(|e| format!("headers: {e}"))?;
            spec.headers.push((k, v));
        }
    }

    let body: Option<String> = opts.get("body").ok();
    let json: Option<Value> = opts.get("json").ok();
    if body.is_some() && matches!(json, Some(Value::Table(_))) {
        return Err("opts.body and opts.json are mutually exclusive".to_owned());
    }
    if let Some(b) = body {
        spec.body = Some(b.into_bytes());
    } else if let Some(Value::Table(t)) = json {
        let encoded = serde_json::to_vec(
            &lua_to_json(Value::Table(t)).map_err(|e| format!("json encode: {e}"))?,
        )
        .map_err(|e| format!("json serialize: {e}"))?;
        spec.body = Some(encoded);
        spec.content_type = Some("application/json".to_owned());
    }

    spec.max_bytes = opt_u64(opts, "max_bytes")?;

    match opt_u64(opts, "timeout_secs")? {
        None => {}
        Some(0) => return Err("opts.timeout_secs must be at least 1".to_owned()),
        Some(secs) => spec.timeout = Some(Duration::from_secs(secs)),
    }

    Ok(spec)
}

/// Read an unsigned integer option from the `opts` table.
///
/// A present but non-integer value is a misconfiguration and errors
/// instead of falling back to the default. Numeric strings do not
/// coerce: a quoted number is a type error.
fn opt_u64(opts: &Table, name: &str) -> Result<Option<u64>, String> {
    let value: Value = opts.get(name).map_err(|e| format!("opts.{name}: {e}"))?;
    match value {
        Value::Nil => Ok(None),
        Value::Integer(i) => u64::try_from(i)
            .map(Some)
            .map_err(|_| format!("opts.{name} must be a positive integer")),
        // Guard bounds n to whole values in [0.0, 2^64]; `as` is the
        // exact saturating conversion for that range (2^64 -> u64::MAX).
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        Value::Number(n) if n.fract() == 0.0 && (0.0..=u64::MAX as f64).contains(&n) => {
            Ok(Some(n as u64))
        }
        _ => Err(format!("opts.{name} must be a positive integer")),
    }
}

struct SimpleResult {
    status: u16,
    body: String,
    content_type: String,
    headers: BTreeMap<String, String>,
    truncated: bool,
}

fn simple_result_to_table(lua: &Lua, r: &SimpleResult) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("status", r.status)?;
    table.set("body", r.body.clone())?;
    table.set("content_type", r.content_type.clone())?;
    table.set("headers", lua.create_table_from(r.headers.clone())?)?;
    table.set("truncated", r.truncated)?;
    Ok(table)
}

/// `headers` by lowercase name, a repeated header's values joined by
/// `, ` in the order they came. A value that is not UTF-8 is read
/// lossily.
fn header_map(headers: &ureq::http::HeaderMap) -> BTreeMap<String, String> {
    let mut map = BTreeMap::<String, String>::new();
    for (name, value) in headers {
        let value = String::from_utf8_lossy(value.as_bytes());
        map.entry(name.as_str().to_owned())
            .and_modify(|joined| {
                joined.push_str(", ");
                joined.push_str(&value);
            })
            .or_insert_with(|| value.into_owned());
    }
    map
}

/// A reader that ends with an error once `stop` is set.
struct Stoppable<'a, R> {
    inner: R,
    stop: &'a AtomicBool,
}

impl<R: Read> Read for Stoppable<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.stop.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("stopped"));
        }
        self.inner.read(buf)
    }
}

/// Reads `reader` up to `max_bytes` plus one lookahead byte, reporting
/// whether the body was cut at `max_bytes`. The lookahead is what makes
/// `truncated` exact; it saturates so a maximal `max_bytes` means "no
/// cap" instead of wrapping.
fn read_capped(reader: impl Read, max_bytes: u64) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut taken = reader.take(max_bytes.saturating_add(1));
    let _ = taken.read_to_end(&mut buf);
    let truncated = u64::try_from(buf.len()).unwrap_or(u64::MAX) > max_bytes;
    if truncated {
        buf.truncate(usize::try_from(max_bytes).unwrap_or(buf.len()));
    }
    (buf, truncated)
}

/// Make one buffered request. Once `stop` is set the body is no longer
/// read, which closes the connection.
fn simple_request(
    method: &str,
    url: &str,
    spec: Option<&RequestSpec>,
    max_bytes: u64,
    stop: &AtomicBool,
) -> Result<SimpleResult, String> {
    let timeout = spec.and_then(|s| s.timeout).unwrap_or(DEFAULT_TIMEOUT);
    let (parsed, agent) = prepare(url, Some(timeout))?;
    let response = dispatch(method, &agent, parsed.as_str(), spec)?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let headers = header_map(response.headers());
    let reader = Stoppable {
        inner: response.into_body().into_reader(),
        stop,
    };
    let (buf, truncated) = read_capped(reader, max_bytes);
    Ok(SimpleResult {
        status,
        body: String::from_utf8_lossy(&buf).into_owned(),
        content_type,
        headers,
        truncated,
    })
}

fn stream_request(
    lua: &Lua,
    url: &str,
    spec: &RequestSpec,
    max_bytes: u64,
    on_event: &Function,
) -> Result<Table, String> {
    // `None` = no global deadline: only the connection phases are
    // bounded so a long-lived SSE stream survives.
    let (parsed, agent) = prepare(url, None)?;
    let response = dispatch("POST", &agent, parsed.as_str(), Some(spec))?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();

    let raw_reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
    let mut capped = raw_reader.take(max_bytes);

    let table = lua.create_table().map_err(|e| format!("lua table: {e}"))?;
    table
        .set("status", status)
        .map_err(|e| format!("lua set status: {e}"))?;
    table
        .set("content_type", content_type)
        .map_err(|e| format!("lua set content_type: {e}"))?;

    if !(200..300).contains(&status) {
        let mut buf = Vec::new();
        let _ = capped.read_to_end(&mut buf);
        table
            .set("body", String::from_utf8_lossy(&buf).into_owned())
            .map_err(|e| format!("lua set body: {e}"))?;
        return Ok(table);
    }

    let mut reader = BufReader::new(capped);
    run_sse_loop(lua, &mut reader, on_event)?;
    Ok(table)
}

/// What the stream thread of a driven `post_stream` hands over.
enum Streamed {
    Head { status: u16, content_type: String },
    Body(String),
    Frame(SseFrame),
}

/// `post_stream` inside a driven coroutine: a worker thread makes the
/// request and reads the frames, and the coroutine yields between them.
/// Each frame's callback runs as a driven coroutine of its own, so an
/// `emit` inside it can yield too.
async fn stream_driven(
    lua: Lua,
    url: String,
    spec: RequestSpec,
    max_bytes: u64,
    on_event: Function,
) -> Result<Table, String> {
    let (tx, mut rx) = tasks::channel::<Result<Streamed, String>>();
    std::thread::Builder::new()
        .name("kage-lua-io".to_owned())
        .spawn(move || {
            if let Err(err) = read_stream(&url, &spec, max_bytes, &tx) {
                tx.send(Err(err));
            }
        })
        .map_err(|e| format!("cannot start the stream thread: {e}"))?;
    let table = lua.create_table().map_err(|e| format!("lua table: {e}"))?;
    while let Some(item) = rx.recv().await {
        match item? {
            Streamed::Head {
                status,
                content_type,
            } => {
                table
                    .set("status", status)
                    .map_err(|e| format!("lua set status: {e}"))?;
                table
                    .set("content_type", content_type)
                    .map_err(|e| format!("lua set content_type: {e}"))?;
            }
            Streamed::Body(body) => table
                .set("body", body)
                .map_err(|e| format!("lua set body: {e}"))?,
            Streamed::Frame(frame) => {
                let payload = frame_table(&lua, frame)?;
                tasks::drive::<()>(&lua, on_event.clone(), payload)
                    .map_err(|e| format!("on_event: {e}"))?
                    .await
                    .map_err(|e| format!("on_event raised: {e}"))?;
            }
        }
    }
    Ok(table)
}

/// The stream thread of a driven `post_stream`: make the request and
/// hand over its head, then its body or each SSE frame. Stops early
/// once the coroutine stops listening.
fn read_stream(
    url: &str,
    spec: &RequestSpec,
    max_bytes: u64,
    tx: &tasks::Sender<Result<Streamed, String>>,
) -> Result<(), String> {
    let (parsed, agent) = prepare(url, None)?;
    let response = dispatch("POST", &agent, parsed.as_str(), Some(spec))?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if !tx.send(Ok(Streamed::Head {
        status,
        content_type,
    })) {
        return Ok(());
    }
    let raw_reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
    let mut capped = raw_reader.take(max_bytes);
    if !(200..300).contains(&status) {
        let mut buf = Vec::new();
        let _ = capped.read_to_end(&mut buf);
        tx.send(Ok(Streamed::Body(
            String::from_utf8_lossy(&buf).into_owned(),
        )));
        return Ok(());
    }
    let mut reader = BufReader::new(capped);
    while let Some(frame) = read_sse_frame(&mut reader)? {
        if !tx.send(Ok(Streamed::Frame(frame))) {
            break;
        }
    }
    Ok(())
}

/// The `{ event, data }` table a frame's callback receives.
fn frame_table(lua: &Lua, frame: SseFrame) -> Result<Table, String> {
    let payload = lua.create_table().map_err(|e| format!("lua table: {e}"))?;
    payload
        .set("event", frame.event)
        .map_err(|e| format!("lua set event: {e}"))?;
    payload
        .set("data", frame.data)
        .map_err(|e| format!("lua set data: {e}"))?;
    Ok(payload)
}

fn run_sse_loop<R: BufRead>(lua: &Lua, reader: &mut R, on_event: &Function) -> Result<(), String> {
    loop {
        match read_sse_frame(reader)? {
            Some(frame) => {
                let payload = frame_table(lua, frame)?;
                on_event
                    .call::<()>(payload)
                    .map_err(|e| format!("on_event raised: {e}"))?;
            }
            None => return Ok(()),
        }
    }
}

struct SseFrame {
    event: String,
    data: String,
}

fn read_sse_frame<R: BufRead>(reader: &mut R) -> Result<Option<SseFrame>, String> {
    let mut event = String::new();
    let mut data = String::new();
    let mut have_content = false;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("transport: {e}"))?;
        if n == 0 {
            if have_content {
                return Ok(Some(SseFrame { event, data }));
            }
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if have_content {
                return Ok(Some(SseFrame { event, data }));
            }
            continue;
        }
        if trimmed.starts_with(':') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("event:") {
            rest.trim_start().clone_into(&mut event);
            have_content = true;
        } else if let Some(rest) = trimmed.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
            have_content = true;
        }
    }
}

fn prepare(url: &str, timeout: Option<Duration>) -> Result<(url::Url, ureq::Agent), String> {
    prepare_with(url, timeout, ureq::Proxy::try_from_env())
}

/// [`prepare`], with the egress proxy supplied. `None` dials directly
/// under the SSRF guard; `Some` rides the operator's proxy, which
/// dials by name on its own side, so the target is not resolved (or
/// vetted) here. Hosts the operator exempted via `NO_PROXY` ride
/// straight past the vet as well.
fn prepare_with(
    url: &str,
    timeout: Option<Duration>,
    proxy: Option<ureq::Proxy>,
) -> Result<(url::Url, ureq::Agent), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(format!("unsupported scheme: {other}")),
    }
    let agent =
        ssrf::agent_for(agent_config(timeout), &parsed, proxy).map_err(|e| e.to_string())?;
    Ok((parsed, agent))
}

/// The plugin HTTP configuration: `SameHost` preserves the
/// `Authorization` header across apex-to-www redirects, and disabling
/// `http_status_as_error` lets the plugin read the response body on
/// non-2xx instead of seeing only a generic transport error.
///
/// `Some(global)` bounds the entire request (non-streaming calls);
/// `None` bounds only the connection phases, leaving the response
/// stream free to run (streaming calls). The proxy decision and the
/// SSRF vet ride in [`ssrf::agent_for`].
fn agent_config(
    global: Option<Duration>,
) -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    let config = ureq::Agent::config_builder()
        .redirect_auth_headers(ureq::config::RedirectAuthHeaders::SameHost)
        .http_status_as_error(false);
    match global {
        Some(global) => config.timeout_global(Some(global)),
        None => config
            .timeout_resolve(Some(STREAM_PHASE_TIMEOUT))
            .timeout_connect(Some(STREAM_PHASE_TIMEOUT))
            .timeout_send_request(Some(STREAM_PHASE_TIMEOUT)),
    }
}

/// Fallback user agent, sent only when the caller did not name its own.
/// A plugin speaking for an upstream client (a ported provider) must send
/// that client's identity verbatim. ureq appends headers rather than
/// replacing them, so an unconditional default would travel as a second
/// `user-agent` line and the server would read the wrong one.
const DEFAULT_USER_AGENT: &str = "kage-plugin/0.1";

/// True when the caller already sets `user-agent`, in any casing.
fn has_user_agent(spec: Option<&RequestSpec>) -> bool {
    spec.is_some_and(|s| {
        s.headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
    })
}

fn dispatch(
    method: &str,
    agent: &ureq::Agent,
    url: &str,
    spec: Option<&RequestSpec>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    match method {
        "GET" => dispatch_bodyless(agent.get(url), spec),
        "DELETE" => dispatch_bodyless(agent.delete(url), spec),
        "POST" => dispatch_with_body(agent.post(url), spec),
        other => Err(format!("unsupported method: {other}")),
    }
}

fn dispatch_bodyless(
    mut req: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    spec: Option<&RequestSpec>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    if !has_user_agent(spec) {
        req = req.header("user-agent", DEFAULT_USER_AGENT);
    }
    if let Some(spec) = spec {
        for (k, v) in &spec.headers {
            req = req.header(k.as_str(), v.as_str());
        }
    }
    req.call().map_err(|e| format!("transport error: {e}"))
}

fn dispatch_with_body(
    mut req: ureq::RequestBuilder<ureq::typestate::WithBody>,
    spec: Option<&RequestSpec>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    if !has_user_agent(spec) {
        req = req.header("user-agent", DEFAULT_USER_AGENT);
    }
    if let Some(spec) = spec {
        if let Some(ct) = &spec.content_type {
            req = req.header("content-type", ct);
        }
        for (k, v) in &spec.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(body) = &spec.body {
            return req
                .send(body.as_slice())
                .map_err(|e| format!("transport error: {e}"));
        }
    }
    req.send_empty()
        .map_err(|e| format!("transport error: {e}"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_tools::ssrf;
    use mlua::{Lua, Table};

    use super::header_map;
    use crate::PluginRuntime;

    /// A runtime that grants `net` to plugin `p`. `kage.http` is gated,
    /// so the request helpers only exist on a granted plugin's proxy.
    fn rt_with_net() -> PluginRuntime {
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("p".to_owned(), vec!["net".to_owned()]);
        PluginRuntime::builder().capabilities(caps).build().unwrap()
    }

    /// Evaluate `script` as the granted plugin `p` after requesting the
    /// `net` capability, so `kage.http` is attached on its proxy.
    fn eval_net(script: &str) -> Result<mlua::Value, crate::error::PluginError> {
        let rt = rt_with_net();
        rt.eval_plugin(
            "p",
            &format!("kage.request_capabilities({{'net'}}); {script}"),
        )
    }

    #[test]
    fn ungranted_plugin_has_no_http() {
        let rt = rt_with_net();
        let v = rt
            .eval_plugin("other", "return kage.http.get == nil")
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    fn rejects_private_url() {
        let res = eval_net("return kage.http.get('http://127.0.0.1:8080/x')");
        assert!(res.is_err());
    }

    #[test]
    fn rejects_non_http_scheme() {
        let res = eval_net("return kage.http.get('file:///etc/passwd')");
        assert!(res.is_err());
    }

    #[test]
    fn rejects_invalid_url() {
        let res = eval_net("return kage.http.get('not a url')");
        assert!(res.is_err());
    }

    #[test]
    fn post_rejects_private_url() {
        let res = eval_net(
            "return kage.http.post('http://127.0.0.1:8080/x', { json = { hello = 'world' } })",
        );
        assert!(res.is_err());
    }

    #[test]
    fn delete_rejects_private_url() {
        let res = eval_net("return kage.http.delete('http://127.0.0.1:8080/x')");
        assert!(res.is_err());
    }

    #[test]
    fn post_stream_rejects_private_url() {
        let res = eval_net(
            "return kage.http.post_stream('http://127.0.0.1:8080/x', \
             { json = { stream = true } }, function() end)",
        );
        assert!(res.is_err());
    }

    #[test]
    fn post_rejects_body_and_json_together() {
        let res = eval_net(
            "return kage.http.post('https://example.com', \
             { body = 'x', json = { a = 1 } })",
        );
        assert!(res.is_err());
    }

    #[test]
    fn sse_parser_joins_multi_line_data() {
        use std::io::BufReader;
        let bytes: &[u8] = b"event: msg\ndata: hello\ndata: world\n\n";
        let mut reader = BufReader::new(bytes);
        let frame = super::read_sse_frame(&mut reader).unwrap().unwrap();
        assert_eq!(frame.event, "msg");
        assert_eq!(frame.data, "hello\nworld");
        assert!(super::read_sse_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn sse_parser_skips_comments_and_blank_runs() {
        use std::io::BufReader;
        let bytes: &[u8] = b": comment\n\n\ndata: only\n\n";
        let mut reader = BufReader::new(bytes);
        let frame = super::read_sse_frame(&mut reader).unwrap().unwrap();
        assert_eq!(frame.event, "");
        assert_eq!(frame.data, "only");
    }

    #[test]
    fn sse_parser_flushes_trailing_frame_at_eof() {
        use std::io::BufReader;
        let bytes: &[u8] = b"data: trailing";
        let mut reader = BufReader::new(bytes);
        let frame = super::read_sse_frame(&mut reader).unwrap().unwrap();
        assert_eq!(frame.data, "trailing");
        assert!(super::read_sse_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn caller_user_agent_suppresses_default() {
        let lua = Lua::new();
        for key in ["User-Agent", "user-agent", "USER-AGENT"] {
            let opts: Table = lua
                .load(format!(
                    r#"{{ headers = {{ ["{key}"] = "ZCode/3.12.3" }} }}"#
                ))
                .eval()
                .unwrap();
            let spec = super::build_request(Some(&opts)).unwrap();
            assert!(super::has_user_agent(Some(&spec)), "key {key}");
        }
    }

    #[test]
    fn missing_user_agent_keeps_default() {
        let lua = Lua::new();
        let opts: Table = lua
            .load(r#"{ headers = { ["X-A"] = "b" } }"#)
            .eval()
            .unwrap();
        let spec = super::build_request(Some(&opts)).unwrap();
        assert!(!super::has_user_agent(Some(&spec)));
        assert!(!super::has_user_agent(None));
    }

    #[test]
    fn build_request_parses_timeout_secs() {
        let lua = Lua::new();
        let opts: Table = lua.load("{ timeout_secs = 5 }").eval().unwrap();
        let spec = super::build_request(Some(&opts)).unwrap();
        assert_eq!(spec.timeout, Some(Duration::from_secs(5)));

        let none: Table = lua.load("{}").eval().unwrap();
        let spec = super::build_request(Some(&none)).unwrap();
        assert_eq!(spec.timeout, None);

        let opts: Table = lua.load("{ timeout_secs = 0 }").eval().unwrap();
        assert!(super::build_request(Some(&opts)).is_err());
    }

    #[test]
    fn build_request_rejects_non_numeric_max_bytes() {
        let lua = Lua::new();
        let opts: Table = lua.load(r#"{ max_bytes = "huge" }"#).eval().unwrap();
        let err = super::build_request(Some(&opts)).unwrap_err();
        assert!(err.contains("max_bytes"), "{err}");
    }

    #[test]
    fn build_request_rejects_fractional_and_negative_max_bytes() {
        let lua = Lua::new();
        for bad in ["{ max_bytes = 10.5 }", "{ max_bytes = -1 }"] {
            let opts: Table = lua.load(bad).eval().unwrap();
            let err = super::build_request(Some(&opts)).unwrap_err();
            assert!(err.contains("max_bytes"), "{bad}: {err}");
        }
    }

    #[test]
    fn build_request_keeps_integer_max_bytes() {
        let lua = Lua::new();
        let opts: Table = lua.load("{ max_bytes = 10 }").eval().unwrap();
        let spec = super::build_request(Some(&opts)).unwrap();
        assert_eq!(spec.max_bytes, Some(10));
    }

    #[test]
    fn headers_come_by_lowercase_name_with_repeats_joined() {
        use ureq::http::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        headers.insert("Retry-After", HeaderValue::from_static("30"));
        headers.append("x-web-tier", HeaderValue::from_static("a"));
        headers.append("x-web-tier", HeaderValue::from_static("b"));
        headers.insert("x-raw", HeaderValue::from_bytes(b"caf\xe9").unwrap());
        let map = header_map(&headers);
        assert_eq!(map["retry-after"], "30");
        assert_eq!(map["x-web-tier"], "a, b");
        assert_eq!(map["x-raw"], "caf\u{fffd}");
    }

    #[test]
    fn read_capped_serves_the_whole_body_for_a_maximal_max_bytes() {
        let (buf, truncated) = super::read_capped("hello".as_bytes(), u64::MAX);
        assert_eq!(buf, b"hello");
        assert!(!truncated);
    }

    #[test]
    fn read_capped_cuts_the_body_at_max_bytes() {
        let (buf, truncated) = super::read_capped("hello".as_bytes(), 3);
        assert_eq!(buf, b"hel");
        assert!(truncated);
    }

    #[test]
    fn build_request_rejects_non_numeric_timeout_secs() {
        let lua = Lua::new();
        let opts: Table = lua.load(r#"{ timeout_secs = "soon" }"#).eval().unwrap();
        let err = super::build_request(Some(&opts)).unwrap_err();
        assert!(err.contains("timeout_secs"), "{err}");
    }

    /// A proxied dial cannot vet addresses, only literal forms: an IP
    /// naming a non-routable range, or the loopback name.
    #[test]
    fn proxied_target_vet_refuses_non_routable_literals() {
        let refused = [
            "http://127.0.0.1/x",
            "http://169.254.169.254/meta",
            "http://[::1]/x",
        ];
        for url in refused {
            let parsed = url::Url::parse(url).unwrap();
            assert!(ssrf::vet_proxied(&parsed).is_err(), "{url}");
        }
        let refused_names = ["http://localhost/x", "http://LOCALHOST:8080/x"];
        for url in refused_names {
            let parsed = url::Url::parse(url).unwrap();
            assert!(ssrf::vet_proxied(&parsed).is_err(), "{url}");
        }
        let allowed = [
            "https://zcode.z.ai/api/v1/agent/configs",
            "http://8.8.8.8/dns-query",
            "http://[2606:4700:4700::1111]/x",
        ];
        for url in allowed {
            let parsed = url::Url::parse(url).unwrap();
            assert!(ssrf::vet_proxied(&parsed).is_ok(), "{url}");
        }
    }

    /// Serves one CONNECT: reads the tunnel head, opens it, then reads
    /// the request inside and answers `200` with body `ok`. The target
    /// name is never looked up, which is the point: the caller picks a
    /// name that cannot resolve.
    fn one_connect_proxy() -> (u16, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                std::io::Read::read_exact(&mut sock, &mut byte).unwrap();
                head.push(byte[0]);
            }
            assert!(
                head.starts_with(b"CONNECT kage-proxy-test.invalid:80"),
                "{head:?}"
            );
            std::io::Write::write_all(&mut sock, b"HTTP/1.1 200 ok\r\n\r\n").unwrap();
            let mut inner = Vec::new();
            while let Ok(1) = std::io::Read::read(&mut sock, &mut byte) {
                inner.push(byte[0]);
                if inner.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            std::io::Write::write_all(
                &mut sock,
                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
            )
            .unwrap();
        });
        (port, handle)
    }

    #[test]
    fn a_proxy_resolves_the_target_on_its_own_side() {
        let (port, server) = one_connect_proxy();
        let proxy = ureq::Proxy::new(&format!("http://127.0.0.1:{port}")).unwrap();
        let url = "http://kage-proxy-test.invalid/x";
        let (parsed, agent) = super::prepare_with(url, None, Some(proxy)).unwrap();
        let response = super::dispatch("GET", &agent, parsed.as_str(), None).unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let mut body = String::new();
        std::io::Read::read_to_string(&mut response.into_body().into_reader(), &mut body).unwrap();
        server.join().unwrap();
        assert_eq!(body, "ok");
    }

    #[test]
    fn a_no_proxy_host_is_exempt_from_the_proxied_vet() {
        let proxy = ureq::Proxy::builder(ureq::ProxyProtocol::Http)
            .host("127.0.0.1")
            .port(9)
            .no_proxy("169.254.169.1")
            .build()
            .unwrap();
        // `NO_PROXY` names the operator's own broker address; the vet
        // must not second-guess the exemption.
        let res = super::prepare_with(
            "http://169.254.169.1:9/x",
            Some(Duration::from_secs(1)),
            Some(proxy),
        );
        assert!(res.is_ok(), "{res:?}");

        // Without the exemption the same target is refused up front.
        let proxy = ureq::Proxy::new("http://127.0.0.1:9").unwrap();
        let res = super::prepare_with(
            "http://169.254.169.1:9/x",
            Some(Duration::from_secs(1)),
            Some(proxy),
        );
        let err = res.unwrap_err();
        assert!(err.contains("must not name"), "{err}");
    }
}
