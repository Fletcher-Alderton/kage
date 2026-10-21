//! Shared `ureq` plumbing used by every HTTP provider.
//!
//! `send`, `read_error_body` and the status-to-error mapping are
//! provider-agnostic, so each provider depends on this module rather
//! than on a sibling provider. Every request runs on a fresh agent
//! over the interruptible transport from [`crate::interrupt`], which
//! is what lets a cancel close the connection instead of leaving it
//! draining toward the idle deadline.

use std::sync::Arc;
use std::time::Duration;

use kage_core::CancelFlag;

use crate::ProviderError;
use crate::error::nonempty;
use crate::interrupt::{InterruptibleConnector, KillRegistry};

/// The configuration behind every provider request.
///
/// Timeout values, given ureq 3.x's chained deadlines: a phase's
/// deadline is the minimum over the phase itself, its preceding phases,
/// and `Global`/`PerCall` (`CallTimings::next_timeout`). An unset
/// timeout drops out of the chain - it is not "infinite for the phase".
///
/// `recv_response` is left unset on purpose. `RecvBody`'s chain
/// includes `RecvResponse`, whose deadline is anchored at the start of
/// response receipt, so a set `recv_response` also caps the body: a
/// generation streaming longer than it dies mid-stream with
/// `Timeout(RecvResponse)` while data flows nonstop. Time-to-first-
/// response is still bounded by the send deadlines, which precede
/// `RecvResponse` in its chain.
///
/// `recv_body` is an idle timeout, not a total cap: the *current*
/// phase is measured from "now" and recomputed every read, so 600s is
/// the max silence between bytes, not a ceiling on a long answer.
/// Providers emit deltas or pings well inside that; the loop's
/// auto-retry backstops a genuine stall, so the grace is generous on
/// purpose to avoid killing a slow-but-alive generation. `global`
/// stays unset for a stream, so an active generation is never capped
/// by total time. A buffered call passes `total` instead (see
/// [`send_within`]), which caps every phase from the first resolve to
/// the last byte of the body.
///
/// Every request names kage and its version as the `User-Agent`.
fn build_config(total: Option<Duration>) -> ureq::config::Config {
    ureq::Agent::config_builder()
        .timeout_global(total)
        .http_status_as_error(false)
        .user_agent(concat!("kage/", env!("CARGO_PKG_VERSION")))
        .timeout_resolve(Some(Duration::from_secs(15)))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_send_request(Some(Duration::from_secs(600)))
        .timeout_send_body(Some(Duration::from_secs(600)))
        .timeout_recv_body(Some(Duration::from_secs(600)))
        .build()
}

/// Build a fresh, single-request agent over the interruptible
/// transport, with its sockets registered under `kill`.
///
/// The agent is deliberately not shared or pooled: a pooled keep-alive
/// socket is handed to the next request without passing through any
/// connector, so a reused connection could never be registered for
/// shutdown. Dialing fresh per request costs a TLS handshake per call
/// and buys a guarantee in return: every connection kage opens can be
/// torn down on cancel.
fn build_agent(kill: &Arc<KillRegistry>, total: Option<Duration>) -> ureq::Agent {
    ureq::Agent::with_parts(
        build_config(total),
        InterruptibleConnector::new(Arc::clone(kill)),
        ureq::unversioned::resolver::DefaultResolver::default(),
    )
}

/// Run a provider's request-and-headers call on a fresh interruptible
/// agent, waiting on the response and a watch on `cancel` together so a
/// slow provider does not delay cancellation (see
/// [`crate::cancelable::cancellable_call`]).
///
/// Returns the response plus the [`KillRegistry`] holding that
/// request's sockets: pass it to
/// [`make_cancelable`](crate::cancelable::make_cancelable) so
/// cancelling the stream shuts the connection down instead of leaving
/// the worker reading toward the idle deadline. `build` receives the
/// per-request agent and `url` and issues the POST. A transport error
/// names the host and port of `url`, never its path or query.
pub(crate) fn send<F>(
    cancel: &CancelFlag,
    url: String,
    build: F,
) -> Result<(ureq::http::Response<ureq::Body>, Arc<KillRegistry>), ProviderError>
where
    F: FnOnce(&ureq::Agent, &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>
        + Send
        + 'static,
{
    send_capped(cancel, url, None, build)
}

/// [`send`] for a buffered call, whose whole duration is capped at
/// `total`: resolving, connecting, the wait for the response, and
/// reading its body. A call past it fails as a transport error.
pub(crate) fn send_within<F>(
    cancel: &CancelFlag,
    url: String,
    total: Duration,
    build: F,
) -> Result<(ureq::http::Response<ureq::Body>, Arc<KillRegistry>), ProviderError>
where
    F: FnOnce(&ureq::Agent, &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>
        + Send
        + 'static,
{
    send_capped(cancel, url, Some(total), build)
}

fn send_capped<F>(
    cancel: &CancelFlag,
    url: String,
    total: Option<Duration>,
    build: F,
) -> Result<(ureq::http::Response<ureq::Body>, Arc<KillRegistry>), ProviderError>
where
    F: FnOnce(&ureq::Agent, &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>
        + Send
        + 'static,
{
    let kill = Arc::new(KillRegistry::new());
    let agent = build_agent(&kill, total);
    let response = crate::cancelable::cancellable_call(cancel, &kill, move || {
        build(&agent, &url).map_err(|err| map_ureq_error(err, &url))
    })?;
    Ok((response, kill))
}

/// Read the body of a non-2xx response into a [`ProviderError`].
///
/// A 429 becomes [`ProviderError::RateLimited`], carrying the
/// provider's `Retry-After` hint (delta-seconds or HTTP-date) and the
/// response body verbatim, so quota details reach the user instead of
/// a canned phrase. A 401 means the credentials were rejected and
/// becomes [`ProviderError::Auth`] with a short detail pulled from the
/// body. A 403 means valid credentials that may not do this (a model
/// the key is not allowed to use), so it stays [`ProviderError::Http`]
/// with that same short detail instead of asking the user to log in
/// again. Every other status stays [`ProviderError::Http`] with the
/// body capped at 8 KiB so a misbehaving upstream cannot blow up our
/// error strings.
pub(crate) fn read_error_body(
    status: u16,
    response: ureq::http::Response<ureq::Body>,
) -> ProviderError {
    use std::io::Read as _;
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut buf = Vec::new();
    let _ = response
        .into_body()
        .into_reader()
        .take(8 * 1024)
        .read_to_end(&mut buf);
    let body = String::from_utf8_lossy(&buf).into_owned();
    classify_http_error(
        status,
        parse_retry_after(retry_after.as_deref(), chrono::Utc::now()),
        body,
    )
}

/// Pure status-to-error mapping, split out of [`read_error_body`] so
/// tests can pin it without a live response.
fn classify_http_error(status: u16, retry_after: Option<Duration>, body: String) -> ProviderError {
    match status {
        429 => ProviderError::RateLimited {
            retry_after,
            body: nonempty(&body),
        },
        401 => ProviderError::Auth(auth_detail(status, &body)),
        403 => ProviderError::Http {
            status,
            body: auth_detail(status, &body),
        },
        _ => ProviderError::Http { status, body },
    }
}

/// Maximum characters of a non-JSON auth error body kept as detail.
const AUTH_DETAIL_MAX_CHARS: usize = 200;

/// Human-readable detail for an auth failure: the JSON body's
/// `error.message` or top-level `message`, else the raw body cut to
/// [`AUTH_DETAIL_MAX_CHARS`], else the bare status.
fn auth_detail(status: u16, body: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let message = parsed.as_ref().and_then(|json| {
        json.pointer("/error/message")
            .or_else(|| json.get("message"))
            .and_then(serde_json::Value::as_str)
    });
    if let Some(message) = message.map(str::trim).filter(|m| !m.is_empty()) {
        return message.to_owned();
    }
    let body = body.trim();
    if body.is_empty() {
        return format!("status {status}");
    }
    if body.chars().count() <= AUTH_DETAIL_MAX_CHARS {
        return body.to_owned();
    }
    let cut: String = body.chars().take(AUTH_DETAIL_MAX_CHARS).collect();
    format!("{cut}...")
}

/// Parse an HTTP `Retry-After` value: a delta in seconds, or an
/// HTTP-date (IMF-fixdate). A date in the past, or anything
/// unparseable, yields `None` so the caller falls back to its own
/// backoff.
pub(crate) fn parse_retry_after(
    value: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<Duration> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    (date.to_utc() - now).to_std().ok()
}

/// Map a transport-time [`ureq::Error`] (from sending the request or
/// reading headers) onto a [`ProviderError`]. A bare status code keeps
/// an empty body; the caller reads the real body separately via
/// [`read_error_body`]. A transport error names the host of `url`.
fn map_ureq_error(err: ureq::Error, url: &str) -> ProviderError {
    let detail = match err {
        ureq::Error::StatusCode(code) => {
            return ProviderError::Http {
                status: code,
                body: String::new(),
            };
        }
        ureq::Error::Io(e) => e.to_string(),
        other => other.to_string(),
    };
    match url_host(url) {
        Some(host) => ProviderError::Transport(format!("{detail} (host {host})")),
        None => ProviderError::Transport(detail),
    }
}

/// The host of `url`, with its port when one is given. The scheme,
/// credentials, path and query are left out.
fn url_host(url: &str) -> Option<String> {
    let uri = url.parse::<ureq::http::Uri>().ok()?;
    let host = uri.host()?;
    Some(match uri.port_u16() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_error_names_the_host_but_not_the_path_or_credentials() {
        let url = "http://user:secret@localhost:11434/v1/chat?key=abc";
        let err = map_ureq_error(ureq::Error::ConnectionFailed, url).to_string();
        assert!(err.contains("(host localhost:11434)"), "got {err}");
        for hidden in ["user", "secret", "/v1", "chat", "key=abc"] {
            assert!(!err.contains(hidden), "{hidden} leaked into {err}");
        }
        assert_eq!(
            url_host("https://api.anthropic.com/v1/messages").as_deref(),
            Some("api.anthropic.com")
        );
        assert_eq!(url_host("not a url"), None);
    }

    #[test]
    fn retry_after_delta_seconds_parses() {
        let now = chrono::Utc::now();
        assert_eq!(
            parse_retry_after(Some("12"), now),
            Some(Duration::from_secs(12))
        );
        assert_eq!(
            parse_retry_after(Some(" 3 "), now),
            Some(Duration::from_secs(3))
        );
    }

    #[test]
    fn retry_after_http_date_parses() {
        let now = chrono::DateTime::parse_from_rfc2822("Sun, 06 Nov 1994 08:49:37 GMT")
            .expect("fixed date")
            .to_utc();
        let minute_later = parse_retry_after(Some("Sun, 06 Nov 1994 08:50:37 GMT"), now);
        assert_eq!(minute_later, Some(Duration::from_secs(60)));
    }

    #[test]
    fn retry_after_past_date_and_garbage_are_none() {
        let now = chrono::Utc::now();
        assert_eq!(parse_retry_after(None, now), None);
        assert_eq!(parse_retry_after(Some(""), now), None);
        assert_eq!(parse_retry_after(Some("   "), now), None);
        assert_eq!(
            parse_retry_after(Some("Sun, 06 Nov 1994 08:49:37 GMT"), now),
            None,
            "a date in the past must not produce a wait"
        );
        assert_eq!(parse_retry_after(Some("soon"), now), None);
    }

    #[test]
    fn classify_maps_429_to_rate_limited_and_keeps_other_statuses_http() {
        let json = "{\"error\":{\"message\":\"quota exceeded\"}}";
        assert!(matches!(
            classify_http_error(429, Some(Duration::from_secs(9)), json.to_owned()),
            ProviderError::RateLimited {
                retry_after: Some(d),
                body: Some(body),
            } if d == Duration::from_secs(9) && body == json
        ));
        assert!(matches!(
            classify_http_error(429, None, "   \n".into()),
            ProviderError::RateLimited {
                retry_after: None,
                body: None
            }
        ));
        assert!(matches!(
            classify_http_error(500, None, "boom".into()),
            ProviderError::Http {
                status: 500,
                body
            } if body == "boom"
        ));
    }

    #[test]
    fn classify_maps_401_to_auth_and_403_to_a_forbidden_detail() {
        let body = r#"{"error":{"message":"token expired"}}"#;
        assert!(matches!(
            classify_http_error(401, None, body.into()),
            ProviderError::Auth(detail) if detail == "token expired"
        ));
        let denied = r#"{"error":{"message":"This key may not use \"x\"."}}"#;
        assert!(matches!(
            classify_http_error(403, None, denied.into()),
            ProviderError::Http { status: 403, body } if body == r#"This key may not use "x"."#
        ));
        assert!(matches!(
            classify_http_error(401, None, r#"{"message":"bad key"}"#.into()),
            ProviderError::Auth(detail) if detail == "bad key"
        ));
        assert!(matches!(
            classify_http_error(400, None, body.into()),
            ProviderError::Http { status: 400, .. }
        ));
    }

    #[test]
    fn auth_detail_falls_back_to_a_truncated_body_or_status() {
        let long = "x".repeat(500);
        let detail = auth_detail(401, &long);
        assert_eq!(detail.chars().count(), AUTH_DETAIL_MAX_CHARS + 3);
        assert!(detail.ends_with("..."));
        assert_eq!(auth_detail(401, " denied \n"), "denied");
        assert_eq!(auth_detail(403, ""), "status 403");
    }

    #[test]
    fn read_error_body_carries_retry_after_header_into_rate_limit() {
        let response = ureq::http::Response::builder()
            .status(429)
            .header("retry-after", "7")
            .body(ureq::Body::builder().data("slow down"))
            .expect("static response");
        let err = read_error_body(429, response);
        assert!(
            matches!(
                &err,
                ProviderError::RateLimited {
                    retry_after: Some(d),
                    body: Some(body),
                } if *d == Duration::from_secs(7) && body == "slow down"
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn read_error_body_keeps_non_429_as_http() {
        let response = ureq::http::Response::builder()
            .status(503)
            .body(ureq::Body::builder().data("unavailable"))
            .expect("static response");
        let err = read_error_body(503, response);
        assert!(
            matches!(
                &err,
                ProviderError::Http { status: 503, body } if body == "unavailable"
            ),
            "got {err:?}"
        );
    }
}
