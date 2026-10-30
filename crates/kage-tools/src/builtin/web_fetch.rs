//! `web_fetch` tool: GET an HTTP(S) URL and return readable text.
//!
//! Defends against SSRF by resolving the host to its IP addresses *before*
//! issuing the request and rejecting any address that lands in a private,
//! loopback, link-local, or otherwise non-routable range. The same check
//! runs on every DNS resolution the HTTP agent performs, so a redirect or a
//! rebinding DNS answer cannot reach such an address either. Each fetch is
//! bounded by `[tools.web_fetch] timeout_ms` and at most
//! `max_redirects` redirects.

use std::fmt::Write as _;
use std::io::Read;
use std::time::Duration;

use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::ssrf;
use crate::{Tool, ToolContext, ToolError, schema_for};

/// Input shape for the `web_fetch` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct WebFetchInput {
    /// HTTP or HTTPS URL to fetch.
    url: String,
    /// Cap on response body bytes read. Defaults to 2,000,000 and is
    /// hard-capped at 5,000,000.
    #[serde(default)]
    max_bytes: Option<u64>,
}

/// Fetch an HTTP(S) URL and return readable text.
///
/// The body budget is `max_bytes` unless the call asks for another,
/// never above `max_bytes_limit`: the body lands in the conversation
/// context, so an unbounded model-controlled cap could pull arbitrary
/// amounts of data in. `timeout` is the whole-request budget:
/// resolving, connecting, redirects, and the body.
#[derive(Debug)]
pub struct WebFetchTool {
    max_bytes: u64,
    max_bytes_limit: u64,
    timeout: Duration,
    max_redirects: u32,
    description: String,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new(&kage_core::config::WebFetchToolConfig::default())
    }
}

impl WebFetchTool {
    /// A `web_fetch` tool with the limits `[tools.web_fetch]` sets.
    #[must_use]
    pub fn new(cfg: &kage_core::config::WebFetchToolConfig) -> Self {
        Self {
            max_bytes: cfg.max_bytes,
            max_bytes_limit: cfg.max_bytes_limit,
            timeout: Duration::from_millis(cfg.timeout_ms),
            max_redirects: cfg.max_redirects,
            description: format!(
                "Fetch an HTTP(S) URL and return its body as readable text. HTML is \
                 stripped to plain text. Refuses to fetch private, loopback, or \
                 link-local addresses. Body is capped at {} by default, {} at \
                 most.",
                super::human_bytes(cfg.max_bytes),
                super::human_bytes(cfg.max_bytes_limit)
            ),
        }
    }
}

/// `n` with thousands separators, as the schema doc writes it.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        let mut schema = schema_for::<WebFetchInput>();
        // The derived doc already states the defaults; keeping it leaves
        // the default schema byte-identical, so prompt caches survive.
        let defaults = kage_core::config::WebFetchToolConfig::default();
        if (self.max_bytes, self.max_bytes_limit) != (defaults.max_bytes, defaults.max_bytes_limit)
        {
            crate::schema::set_property_doc(
                &mut schema,
                "max_bytes",
                &format!(
                    "Cap on response body bytes read. Defaults to {} and is hard-capped at {}.",
                    grouped(self.max_bytes),
                    grouped(self.max_bytes_limit)
                ),
            );
        }
        schema
    }

    fn risk(&self) -> Risk {
        Risk::Network
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: WebFetchInput = serde_json::from_value(input)?;
        if cx.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let parsed = url::Url::parse(&input.url)
            .map_err(|e| ToolError::InvalidInput(format!("invalid url: {e}")))?;
        match parsed.scheme() {
            "http" | "https" => {}
            other => {
                return Err(ToolError::InvalidInput(format!(
                    "unsupported scheme: {other}"
                )));
            }
        }
        let max_bytes = effective_max_bytes(input.max_bytes, self.max_bytes, self.max_bytes_limit);
        let agent = ssrf::agent_for_url(agent_config(self.timeout, self.max_redirects), &parsed)?;
        fetch(&agent, &parsed, max_bytes, self.max_redirects)
    }
}

/// The agent configuration web fetches share: one whole-request budget
/// and the redirect cap. The proxy decision and SSRF vet ride in
/// [`ssrf::agent_for_url`].
fn agent_config(
    timeout: Duration,
    max_redirects: u32,
) -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(max_redirects)
}

/// The body budget in effect: the model's request, else `default`,
/// never above `limit`.
fn effective_max_bytes(requested: Option<u64>, default: u64, limit: u64) -> u64 {
    requested.unwrap_or(default).min(limit)
}

fn fetch(
    agent: &ureq::Agent,
    parsed: &url::Url,
    max_bytes: u64,
    max_redirects: u32,
) -> Result<ToolOutput, ToolError> {
    let response = agent
        .get(parsed.as_str())
        .header("user-agent", "kage/0.1 (+https://github.com/QaidVoid/kage)")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(code) => {
                ToolError::Other(format!("{} returned http {code}", parsed.as_str()))
            }
            ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied => {
                ToolError::InvalidInput(io.to_string())
            }
            ureq::Error::Timeout(_) => ToolError::Other(format!("{} timed out", parsed.as_str())),
            ureq::Error::TooManyRedirects => ToolError::Other(format!(
                "{} redirected more than {max_redirects} times",
                parsed.as_str()
            )),
            other => ToolError::Other(format!("transport error: {other}")),
        })?;

    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let mut reader = response.into_body().into_reader();
    let mut buf = Vec::new();
    let mut taken = (&mut reader).take(max_bytes.saturating_add(1));
    // A read error here means the body arrived incomplete; surface
    // it rather than hand the model a silently-truncated page as a
    // success. (Size-capping is the `take` above, not an error.)
    taken
        .read_to_end(&mut buf)
        .map_err(|e| ToolError::Other(format!("read body from {}: {e}", parsed.as_str())))?;
    let truncated = u64::try_from(buf.len()).unwrap_or(u64::MAX) > max_bytes;
    if truncated {
        buf.truncate(usize::try_from(max_bytes).unwrap_or(buf.len()));
    }

    let text = if content_type.contains("html") {
        html2text::from_read(&buf[..], 100)
            .unwrap_or_else(|_| String::from_utf8_lossy(&buf).into_owned())
    } else {
        String::from_utf8_lossy(&buf).into_owned()
    };

    let mut output = text;
    if truncated {
        let _ = write!(output, "\n\n[... truncated at {max_bytes} bytes ...]");
    }

    Ok(ToolOutput {
        is_error: false,
        text: output,
        structured: Some(serde_json::json!({
            "url": parsed.as_str(),
            "status": status,
            "content_type": content_type,
            "truncated": truncated,
        })),
        terminate: false,
        media: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    /// The default `[tools.web_fetch]` limits.
    const DEFAULT_MAX_BYTES: u64 = 2_000_000;
    const ABSOLUTE_MAX_BYTES: u64 = 5_000_000;
    const MAX_REDIRECTS: u32 = 5;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::time::Instant;

    use super::*;

    #[test]
    fn rejects_non_http_scheme() {
        let err = WebFetchTool::default()
            .execute(
                serde_json::json!({"url": "file:///etc/passwd"}),
                &ToolContext::new(std::path::Path::new("/tmp"), &kage_core::CancelFlag::new()),
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn rejects_private_url() {
        let err = WebFetchTool::default()
            .execute(
                serde_json::json!({"url": "http://127.0.0.1:8080/x"}),
                &ToolContext::new(std::path::Path::new("/tmp"), &kage_core::CancelFlag::new()),
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "got {err:?}");
    }

    /// Serve every connection on a loopback listener from a thread,
    /// answering each request line's path with `respond(port, path)`.
    fn serve(respond: impl Fn(u16, &str) -> String + Send + 'static) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut header = String::new();
                while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                    header.clear();
                }
                let path = request_line.split(' ').nth(1).unwrap_or("/");
                let _ = stream.write_all(respond(addr.port(), path).as_bytes());
            }
        });
        addr
    }

    fn redirect(location: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\
             Connection: close\r\n\r\n"
        )
    }

    fn ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn fetch_from(
        addr: SocketAddr,
        path: &str,
        timeout: Duration,
    ) -> Result<ToolOutput, ToolError> {
        fetch_capped(addr, path, timeout, DEFAULT_MAX_BYTES)
    }

    fn fetch_capped(
        addr: SocketAddr,
        path: &str,
        timeout: Duration,
        max_bytes: u64,
    ) -> Result<ToolOutput, ToolError> {
        let agent =
            ssrf::guarded_agent_allowing(agent_config(timeout, MAX_REDIRECTS).build(), addr);
        let url = url::Url::parse(&format!("http://{addr}{path}")).unwrap();
        fetch(&agent, &url, max_bytes, MAX_REDIRECTS)
    }

    #[test]
    fn follows_redirects_to_allowed_addresses() {
        let addr = serve(|_, path| match path {
            "/start" => redirect("/final"),
            _ => ok("fetched"),
        });
        let out = fetch_from(addr, "/start", Duration::from_secs(5)).unwrap();
        assert_eq!(out.text, "fetched");
        assert_eq!(out.structured.unwrap()["status"], 200);
    }

    #[test]
    fn refuses_redirects_to_non_routable_addresses() {
        let addr = serve(|port, path| match path {
            "/metadata" => redirect("http://169.254.169.254/latest/meta-data/"),
            "/loopback" => redirect(&format!("http://127.0.0.1:{}/", port ^ 1)),
            _ => redirect("http://[::1]/"),
        });
        for path in ["/metadata", "/loopback", "/v6"] {
            let err = fetch_from(addr, path, Duration::from_secs(5)).unwrap_err();
            let ToolError::InvalidInput(msg) = &err else {
                panic!("{path}: {err:?}");
            };
            assert!(msg.contains("non-routable address"), "{path}: {msg}");
        }
    }

    #[test]
    fn stops_after_too_many_redirects() {
        let addr = serve(|_, _| redirect("/again"));
        let err = fetch_from(addr, "/", Duration::from_secs(5)).unwrap_err();
        assert!(err.to_string().contains("redirected more than"), "{err}");
    }

    #[test]
    fn a_stalled_server_is_cut_off_by_the_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0; 1024];
            while stream.read(&mut buf).is_ok_and(|n| n > 0) {}
        });
        let started = Instant::now();
        let err = fetch_from(addr, "/", Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn requested_caps_clamp_to_the_absolute_ceiling() {
        assert_eq!(
            effective_max_bytes(None, DEFAULT_MAX_BYTES, ABSOLUTE_MAX_BYTES),
            DEFAULT_MAX_BYTES
        );
        assert_eq!(
            effective_max_bytes(Some(1_000), DEFAULT_MAX_BYTES, ABSOLUTE_MAX_BYTES),
            1_000
        );
        assert_eq!(
            effective_max_bytes(
                Some(ABSOLUTE_MAX_BYTES),
                DEFAULT_MAX_BYTES,
                ABSOLUTE_MAX_BYTES
            ),
            ABSOLUTE_MAX_BYTES
        );
        assert_eq!(
            effective_max_bytes(Some(u64::MAX), DEFAULT_MAX_BYTES, ABSOLUTE_MAX_BYTES),
            ABSOLUTE_MAX_BYTES
        );
    }

    #[test]
    fn a_body_over_the_ceiling_is_cut_even_when_asked_for_more() {
        let capped_len = usize::try_from(ABSOLUTE_MAX_BYTES).unwrap();
        let body = "x".repeat(capped_len + 1024);
        let addr = serve(move |_, _| ok(&body));
        let cap = effective_max_bytes(Some(u64::MAX), DEFAULT_MAX_BYTES, ABSOLUTE_MAX_BYTES);
        let out = fetch_capped(addr, "/big", Duration::from_secs(30), cap).unwrap();
        assert_eq!(
            out.structured.unwrap()["truncated"],
            serde_json::json!(true)
        );
        let marker = format!("\n\n[... truncated at {cap} bytes ...]");
        assert_eq!(out.text.len(), capped_len + marker.len());
    }
}
