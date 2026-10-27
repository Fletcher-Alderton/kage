//! Shared endpoints and bounded calls for media APIs.
//!
//! A call carries one deadline through sending and reading the answer.
//! Refusals keep their body and `Retry-After` for the caller's retry policy.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::time::{Duration, Instant};

use kage_core::CancelFlag;

use crate::ProviderError;
use crate::error::nonempty;

/// The largest answer read. A 1.5 MB PNG is about 2 MB of base64, so
/// this leaves room for a large image and still refuses a runaway body.
pub const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

/// Where a media API is reached. Its `Debug` form leaves the key out.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct MediaEndpoint {
    /// The base URL, without the API route. A trailing `/` is allowed.
    pub base_url: String,
    /// Sent as a bearer token. Empty sends no credential, for a server
    /// that needs none.
    pub api_key: String,
    /// Extra headers sent on the request, after the protocol's own.
    pub headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for MediaEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaEndpoint")
            .field("base_url", &self.base_url)
            .field("has_key", &!self.api_key.is_empty())
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Why a media call failed.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{error}")]
pub struct MediaError {
    /// What failed. An HTTP error carries the server's own message
    /// rather than its whole body.
    pub error: ProviderError,
    /// The wait the server asked for in `Retry-After`, on any refusal
    /// that carried one. A busy server answers 503 with one as well as
    /// 429, and [`ProviderError`] keeps it only for 429.
    pub retry_after: Option<Duration>,
}

impl From<ProviderError> for MediaError {
    fn from(error: ProviderError) -> Self {
        Self {
            error,
            retry_after: None,
        }
    }
}

/// The body sent to a media endpoint.
pub(crate) enum RequestBody {
    /// A JSON request with the protocol's content type.
    Json(serde_json::Value),
    /// A `multipart/form-data` request, as an image edit is sent. The
    /// content type names the boundary the bytes are built with.
    Multipart {
        /// The whole `multipart/form-data; boundary=...` header value.
        content_type: String,
        /// The body: every part and the closing boundary included.
        bytes: Vec<u8>,
    },
}

/// Send one request within the call's remaining deadline. A refusal
/// keeps its body as sent; `noun` names the answer in read failures.
pub(crate) fn call(
    endpoint: &MediaEndpoint,
    route: &str,
    body: RequestBody,
    noun: &'static str,
    deadline: Instant,
    timeout: Duration,
    cancel: &CancelFlag,
) -> Result<Vec<u8>, MediaError> {
    if cancel.is_cancelled() {
        return Err(ProviderError::Cancelled.into());
    }
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(
            ProviderError::Transport(format!("no answer within {}s", timeout.as_secs())).into(),
        );
    }
    let url = format!("{}/{route}", endpoint.base_url.trim_end_matches('/'));
    let credential = (!endpoint.api_key.is_empty())
        .then(|| ("authorization", format!("Bearer {}", endpoint.api_key)));
    let content_type = match &body {
        RequestBody::Json(_) => "application/json",
        RequestBody::Multipart { content_type, .. } => content_type,
    };
    let headers = crate::headers::request_headers(
        &[("content-type", content_type.to_owned())],
        credential,
        &endpoint.headers,
    );
    let late = |error: ProviderError| over_time(error, deadline, timeout);
    let (response, kill) = crate::http::send_within(cancel, url, left, move |agent, url| {
        let mut post = agent.post(url);
        for (name, value) in &headers {
            post = post.header(name.as_str(), value.as_str());
        }
        match body {
            RequestBody::Json(json) => post.send_json(&json),
            RequestBody::Multipart { bytes, .. } => post.send(bytes),
        }
    })
    .map_err(late)?;

    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let retry_after = crate::http::parse_retry_after(
            response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            chrono::Utc::now(),
        );
        return Err(MediaError {
            error: crate::http::read_error_body(status, response),
            retry_after,
        });
    }
    let body = crate::cancelable::cancellable_call(cancel, &kill, move || {
        read_capped(response.into_body().into_reader(), MAX_RESPONSE_BYTES, noun)
    })
    .map_err(late)?;
    Ok(body)
}

/// Whether a bad-request response names an unsupported field.
pub(crate) fn names_field(error: &MediaError, field: &str) -> bool {
    matches!(&error.error, ProviderError::Http { status: 400, body } if body.contains(field))
}

/// A transport failure once `deadline` has passed, said as such: the
/// cap surfaces from the HTTP client as a bare timeout.
fn over_time(error: ProviderError, deadline: Instant, timeout: Duration) -> ProviderError {
    match error {
        ProviderError::Transport(detail) if Instant::now() >= deadline => ProviderError::Transport(
            format!("no answer within {}s ({detail})", timeout.as_secs()),
        ),
        other => other,
    }
}

/// Read all of `reader`, refusing a body longer than `cap`.
pub(crate) fn read_capped(
    reader: impl std::io::Read,
    cap: u64,
    noun: &str,
) -> Result<Vec<u8>, ProviderError> {
    let mut body = Vec::new();
    reader
        .take(cap + 1)
        .read_to_end(&mut body)
        .map_err(|e| ProviderError::Transport(format!("reading {noun}: {e}")))?;
    if body.len() as u64 > cap {
        return Err(ProviderError::Decode(format!(
            "{noun} is over {} MiB",
            cap / (1024 * 1024)
        )));
    }
    Ok(body)
}

/// `error` with the server's own message in place of an error body:
/// the `OpenAI` shape `error.message`, a top-level `message`, or
/// `errors.message`, which some image servers send. A body in none of
/// these shapes is kept as sent.
pub(crate) fn server_message(error: ProviderError) -> ProviderError {
    let message = |body: &str| {
        let json = serde_json::from_str::<serde_json::Value>(body).ok();
        json.as_ref()
            .and_then(|json| {
                json.pointer("/error/message")
                    .or_else(|| json.get("message"))
                    .or_else(|| json.pointer("/errors/message"))
                    .and_then(serde_json::Value::as_str)
            })
            .map_or_else(|| body.trim().to_owned(), |text| text.trim().to_owned())
    };
    match error {
        ProviderError::Http { status, body } => ProviderError::Http {
            status,
            body: message(&body),
        },
        ProviderError::RateLimited { retry_after, body } => ProviderError::RateLimited {
            retry_after,
            body: body.as_deref().map(message).as_deref().and_then(nonempty),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_over_the_cap_is_refused() {
        assert_eq!(
            read_capped(&b"12345"[..], 5, "the image answer").unwrap(),
            b"12345"
        );
        let err = read_capped(&b"123456"[..], 5, "the image answer").unwrap_err();
        assert!(matches!(err, ProviderError::Decode(_)), "{err:?}");
    }
}
