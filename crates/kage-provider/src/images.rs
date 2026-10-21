//! Image generation over the `OpenAI` Images API:
//! `POST {base_url}/images/generations`, one image a call, answered as
//! base64.
//!
//! Many providers and self-hosted servers speak this shape. The answer
//! arrives whole rather than streamed, so a call carries a ceiling on
//! its whole duration instead of the idle deadline a chat stream gets,
//! and the body read is capped.
//!
//! The image's bytes are asked for (`response_format` is `b64_json`).
//! `OpenAI`'s GPT image models always answer base64 and refuse that
//! parameter, so a refusal that names it is asked again without it. A
//! server that answers a `data:` URI in `url` is read as well. A server
//! that answers an `http(s)` link is refused rather than followed:
//! fetching it would be a second request to a host nobody configured.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::time::{Duration, Instant};

use base64::Engine as _;
use kage_core::CancelFlag;

use crate::ProviderError;
use crate::error::nonempty;

/// The largest answer read. A 1.5 MB PNG is about 2 MB of base64, so
/// this leaves room for a large image and still refuses a runaway body.
pub const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

/// Where images are generated. Its `Debug` form leaves the key out.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ImageEndpoint {
    /// The base URL, without `/images/generations`. A trailing `/` is
    /// allowed.
    pub base_url: String,
    /// Sent as a bearer token. Empty sends no credential, for a server
    /// that needs none.
    pub api_key: String,
    /// Extra headers sent on the request, after the protocol's own.
    pub headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for ImageEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageEndpoint")
            .field("base_url", &self.base_url)
            .field("has_key", &!self.api_key.is_empty())
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// One image to generate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageRequest {
    /// The model the server is asked for.
    pub model: String,
    /// What to draw.
    pub prompt: String,
    /// The size as `WxH`, which a server may round or refuse. `None`
    /// leaves it to the server.
    pub size: Option<String>,
}

/// The image a server answered with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedImage {
    /// The image file's bytes.
    pub bytes: Vec<u8>,
    /// The type the bytes themselves declare. The answer's own claim is
    /// never used.
    pub mime: &'static str,
    /// The server's `revised_prompt`, trimmed, when it sent one. This is
    /// free text: servers put the prompt they rewrote here, and some put
    /// other notices, such as a fallback to another model.
    pub revised_prompt: Option<String>,
}

/// Why a generation failed.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{error}")]
pub struct ImageError {
    /// What failed. An HTTP error carries the server's own message
    /// rather than its whole body.
    pub error: ProviderError,
    /// The wait the server asked for in `Retry-After`, on any refusal
    /// that carried one. A busy server answers 503 with one as well as
    /// 429, and [`ProviderError`] keeps it only for 429.
    pub retry_after: Option<Duration>,
}

impl From<ProviderError> for ImageError {
    fn from(error: ProviderError) -> Self {
        Self {
            error,
            retry_after: None,
        }
    }
}

/// Generate one image, waiting at most `timeout` for the whole call.
///
/// Setting `cancel` ends the call at once, closing its connection.
///
/// # Errors
///
/// - [`ProviderError::Cancelled`] when `cancel` is set first.
/// - [`ProviderError::Transport`] when the server cannot be reached or
///   does not answer within `timeout`.
/// - [`ProviderError::RateLimited`], [`ProviderError::Auth`] or
///   [`ProviderError::Http`] for a refusal, carrying the server's
///   message.
/// - [`ProviderError::Decode`] for an answer that holds no image this
///   client reads: no `data`, a link instead of bytes, bad base64, a
///   body over [`MAX_RESPONSE_BYTES`], or bytes that are not a PNG,
///   JPEG, GIF or WebP image.
pub fn generate(
    endpoint: &ImageEndpoint,
    request: &ImageRequest,
    timeout: Duration,
    cancel: &CancelFlag,
) -> Result<GeneratedImage, ImageError> {
    let deadline = Instant::now() + timeout;
    let mut result = post(endpoint, request, deadline, timeout, cancel, true);
    // A 400 is answered before any image is made, so asking again costs
    // a round trip and no generation. The retry shares the deadline.
    if let Err(ImageError {
        error: ProviderError::Http { status: 400, body },
        ..
    }) = &result
        && body.contains("response_format")
    {
        result = post(endpoint, request, deadline, timeout, cancel, false);
    }
    result.map_err(|refused| ImageError {
        error: server_message(refused.error),
        ..refused
    })
}

/// One request, asking for base64 when `ask_base64` is set, given what
/// is left until `deadline` of the call's `timeout`. An HTTP error keeps
/// its body as sent.
fn post(
    endpoint: &ImageEndpoint,
    request: &ImageRequest,
    deadline: Instant,
    timeout: Duration,
    cancel: &CancelFlag,
    ask_base64: bool,
) -> Result<GeneratedImage, ImageError> {
    if cancel.is_cancelled() {
        return Err(ProviderError::Cancelled.into());
    }
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(
            ProviderError::Transport(format!("no answer within {}s", timeout.as_secs())).into(),
        );
    }
    let url = format!(
        "{}/images/generations",
        endpoint.base_url.trim_end_matches('/')
    );
    let mut body = serde_json::json!({
        "model": request.model,
        "prompt": request.prompt,
        "n": 1,
    });
    if ask_base64 {
        body["response_format"] = serde_json::Value::String("b64_json".to_owned());
    }
    if let Some(size) = &request.size {
        body["size"] = serde_json::Value::String(size.clone());
    }
    let credential = (!endpoint.api_key.is_empty())
        .then(|| ("authorization", format!("Bearer {}", endpoint.api_key)));
    let headers = crate::headers::request_headers(
        &[("content-type", "application/json".to_owned())],
        credential,
        &endpoint.headers,
    );
    let late = |error: ProviderError| over_time(error, deadline, timeout);
    let (response, kill) = crate::http::send_within(cancel, url, left, move |agent, url| {
        let mut post = agent.post(url);
        for (name, value) in &headers {
            post = post.header(name.as_str(), value.as_str());
        }
        post.send_json(&body)
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
        return Err(ImageError {
            error: crate::http::read_error_body(status, response),
            retry_after,
        });
    }
    let body = crate::cancelable::cancellable_call(cancel, &kill, move || {
        read_capped(response.into_body().into_reader(), MAX_RESPONSE_BYTES)
    })
    .map_err(late)?;
    Ok(parse_answer(&body)?)
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
fn read_capped(reader: impl std::io::Read, cap: u64) -> Result<Vec<u8>, ProviderError> {
    let mut body = Vec::new();
    reader
        .take(cap + 1)
        .read_to_end(&mut body)
        .map_err(|e| ProviderError::Transport(format!("reading the image answer: {e}")))?;
    if body.len() as u64 > cap {
        return Err(ProviderError::Decode(format!(
            "the image answer is over {} MiB",
            cap / (1024 * 1024)
        )));
    }
    Ok(body)
}

/// The image in an answer's first `data` entry.
fn parse_answer(body: &[u8]) -> Result<GeneratedImage, ProviderError> {
    let json: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| ProviderError::Decode(format!("the image answer is not JSON: {e}")))?;
    let first = json
        .pointer("/data/0")
        .ok_or_else(|| ProviderError::Decode("the image answer holds no image".to_owned()))?;
    let bytes = if let Some(encoded) = first.get("b64_json").and_then(serde_json::Value::as_str) {
        decode_base64(encoded)?
    } else if let Some(url) = first.get("url").and_then(serde_json::Value::as_str) {
        match url.strip_prefix("data:") {
            Some(uri) => decode_data_uri(uri)?,
            None => {
                return Err(ProviderError::Decode(
                    "the server answered a link to the image instead of its bytes, \
                     and kage does not fetch it"
                        .to_owned(),
                ));
            }
        }
    } else {
        return Err(ProviderError::Decode(
            "the image answer holds neither `b64_json` nor `url`".to_owned(),
        ));
    };
    let mime = kage_core::image::sniff_mime(&bytes).ok_or_else(|| {
        ProviderError::Decode("the answer is not a PNG, JPEG, GIF or WebP image".to_owned())
    })?;
    let revised_prompt = first
        .get("revised_prompt")
        .and_then(serde_json::Value::as_str)
        .and_then(nonempty)
        .map(|note| note.trim().to_owned());
    Ok(GeneratedImage {
        bytes,
        mime,
        revised_prompt,
    })
}

/// The payload of a `data:` URI, given without its scheme. Only base64
/// payloads are read; an image is never sent percent-encoded.
fn decode_data_uri(uri: &str) -> Result<Vec<u8>, ProviderError> {
    match uri.split_once(',') {
        Some((meta, payload)) if meta.ends_with(";base64") => decode_base64(payload),
        _ => Err(ProviderError::Decode(
            "the image `data:` URI is not base64".to_owned(),
        )),
    }
}

/// Standard base64, ignoring the line breaks some servers wrap it with.
fn decode_base64(encoded: &str) -> Result<Vec<u8>, ProviderError> {
    let compact: String = encoded
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(compact)
        .map_err(|e| ProviderError::Decode(format!("the image is not valid base64: {e}")))
}

/// `error` with the server's own message in place of an error body:
/// the `OpenAI` shape `error.message`, a top-level `message`, or
/// `errors.message`, which some image servers send. A body in none of
/// these shapes is kept as sent.
fn server_message(error: ProviderError) -> ProviderError {
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
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    use super::*;

    /// A 1x1 PNG cut after its `IHDR` size fields, enough for the sniffer.
    const PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0, 0,
        0, 1, 0, 0, 0, 1,
    ];

    fn png_base64() -> String {
        base64::engine::general_purpose::STANDARD.encode(PNG)
    }

    /// Read one request off `listener` and answer it with `status`,
    /// `headers` and `body`. Returns the raw request.
    fn serve_one(
        listener: TcpListener,
        status: &'static str,
        headers: &'static str,
        body: String,
    ) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let raw = read_request(&mut conn);
            let answer = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{headers}\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            conn.write_all(answer.as_bytes()).unwrap();
            raw
        })
    }

    fn read_request(conn: &mut std::net::TcpStream) -> String {
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = conn.read(&mut buf).unwrap();
            raw.extend_from_slice(&buf[..n]);
        }
        let head = String::from_utf8_lossy(&raw).to_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map_or(0, |v| v.trim().parse().unwrap());
        let body_start = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        while raw.len() < body_start + length {
            let n = conn.read(&mut buf).unwrap();
            raw.extend_from_slice(&buf[..n]);
        }
        String::from_utf8(raw).unwrap()
    }

    fn endpoint(base_url: String) -> ImageEndpoint {
        ImageEndpoint {
            base_url,
            api_key: "img-key".to_owned(),
            headers: BTreeMap::from([("X-Team".to_owned(), "art".to_owned())]),
        }
    }

    fn request() -> ImageRequest {
        ImageRequest {
            model: "image:max".to_owned(),
            prompt: "a red kite".to_owned(),
            size: Some("1536x1024".to_owned()),
        }
    }

    fn run(
        status: &'static str,
        headers: &'static str,
        body: String,
    ) -> (Result<GeneratedImage, ImageError>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_one(listener, status, headers, body);
        let result = generate(
            &endpoint(format!("http://{addr}/v1/")),
            &request(),
            Duration::from_secs(10),
            &CancelFlag::new(),
        );
        (result, server.join().unwrap())
    }

    #[test]
    fn posts_one_b64_json_image_request_with_the_key_and_headers() {
        let body = format!(
            r#"{{"created":1,"data":[{{"b64_json":"{}","revised_prompt":"  image:max was rate-limited; this image was generated by image:xhigh "}}]}}"#,
            png_base64()
        );
        let (result, request) = run("200 OK", "", body);
        let image = result.unwrap();
        assert_eq!(image.bytes, PNG);
        assert_eq!(image.mime, "image/png");
        assert_eq!(
            image.revised_prompt.as_deref(),
            Some("image:max was rate-limited; this image was generated by image:xhigh")
        );

        let lower = request.to_lowercase();
        assert!(
            lower.starts_with("post /v1/images/generations "),
            "a trailing slash on the base must not double: {request}"
        );
        assert!(
            lower.contains("\r\nauthorization: bearer img-key\r\n"),
            "{request}"
        );
        assert!(lower.contains("\r\nx-team: art\r\n"), "{request}");
        assert!(lower.contains("\r\nuser-agent: kage/"), "{request}");
        let sent: serde_json::Value =
            serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            sent,
            serde_json::json!({
                "model": "image:max",
                "prompt": "a red kite",
                "n": 1,
                "response_format": "b64_json",
                "size": "1536x1024",
            })
        );
    }

    #[test]
    fn no_key_sends_no_credential_and_no_size_sends_none() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = format!(r#"{{"data":[{{"b64_json":"{}"}}]}}"#, png_base64());
        let server = serve_one(listener, "200 OK", "", body);
        let endpoint = ImageEndpoint {
            base_url: format!("http://{addr}/v1"),
            ..ImageEndpoint::default()
        };
        let request = ImageRequest {
            size: None,
            ..request()
        };
        let image = generate(
            &endpoint,
            &request,
            Duration::from_secs(10),
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(image.revised_prompt, None);
        let sent = server.join().unwrap();
        assert!(sent.starts_with("POST /v1/images/generations "), "{sent}");
        assert!(
            !sent.to_lowercase().contains("\r\nauthorization:"),
            "{sent}"
        );
        assert!(!sent.contains("\"size\""), "{sent}");
    }

    #[test]
    fn reads_a_data_uri_and_refuses_a_link() {
        let png = parse_answer(
            format!(
                r#"{{"data":[{{"url":"data:image/jpeg;base64,{}"}}]}}"#,
                png_base64()
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            png.mime, "image/png",
            "the bytes say PNG whatever the URI claims"
        );

        let link =
            parse_answer(br#"{"data":[{"url":"https://images.example.com/a.png"}]}"#).unwrap_err();
        assert!(
            matches!(&link, ProviderError::Decode(m) if m.contains("link")),
            "{link:?}"
        );
        let plain = parse_answer(br#"{"data":[{"url":"data:image/png,abc"}]}"#).unwrap_err();
        assert!(matches!(plain, ProviderError::Decode(_)), "{plain:?}");
    }

    #[test]
    fn refuses_answers_without_an_image() {
        for body in [
            &br#"{"data":[]}"#[..],
            br#"{"data":[{"revised_prompt":"x"}]}"#,
            br#"{"data":[{"b64_json":"!!!"}]}"#,
            b"not json",
        ] {
            let err = parse_answer(body).unwrap_err();
            assert!(matches!(err, ProviderError::Decode(_)), "{err:?}");
        }
        let text = base64::engine::general_purpose::STANDARD.encode("just text");
        let err = parse_answer(format!(r#"{{"data":[{{"b64_json":"{text}"}}]}}"#).as_bytes())
            .unwrap_err();
        assert!(
            matches!(&err, ProviderError::Decode(m) if m.contains("not a PNG")),
            "{err:?}"
        );
        let wrapped = format!(r#"{{"data":[{{"b64_json":"{}\n"}}]}}"#, png_base64());
        assert!(parse_answer(wrapped.as_bytes()).is_ok());
    }

    #[test]
    fn a_refusal_carries_the_server_message_and_its_wait() {
        let (result, _) = run(
            "400 Bad Request",
            "",
            r#"{"error":{"message":"size must be WxH, each side 256 to 2048","type":"invalid_request_error","code":"invalid_size"}}"#
                .to_owned(),
        );
        let err = result.unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::Http { status: 400, body } if body == "size must be WxH, each side 256 to 2048"),
            "{err:?}"
        );
        assert!(!err.error.is_transient());
        assert_eq!(err.retry_after, None);

        let (result, _) = run(
            "401 Unauthorized",
            "",
            r#"{"error":{"message":"invalid key"}}"#.to_owned(),
        );
        assert!(matches!(result.unwrap_err().error, ProviderError::Auth(m) if m == "invalid key"));

        let (result, _) = run(
            "429 Too Many Requests",
            "retry-after: 7\r\n",
            r#"{"error":{"message":"slow down"}}"#.to_owned(),
        );
        let err = result.unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::RateLimited { body: Some(b), .. } if b == "slow down"),
            "{err:?}"
        );
        assert_eq!(err.retry_after, Some(Duration::from_secs(7)));

        let (result, _) = run(
            "503 Service Unavailable",
            "retry-after: 12\r\n",
            r#"{"error":{"message":"every engine is busy"}}"#.to_owned(),
        );
        let err = result.unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::Http { status: 503, body } if body == "every engine is busy"),
            "{err:?}"
        );
        assert!(err.error.is_transient());
        assert_eq!(err.retry_after, Some(Duration::from_secs(12)));
        assert!(err.to_string().contains("every engine is busy"), "{err}");
    }

    /// Answer one request per entry of `answers`, in order, and return
    /// the requests read.
    fn serve_each(
        listener: TcpListener,
        answers: Vec<(&'static str, String)>,
    ) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in answers {
                let (mut conn, _) = listener.accept().unwrap();
                requests.push(read_request(&mut conn));
                let answer = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                conn.write_all(answer.as_bytes()).unwrap();
            }
            requests
        })
    }

    fn sent_body(request: &str) -> serde_json::Value {
        serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    #[test]
    fn a_server_refusing_response_format_is_asked_again_without_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let unknown = r#"{"error":{"message":"Unknown parameter: 'response_format'.","type":"invalid_request_error","param":"response_format","code":"unknown_parameter"}}"#;
        let server = serve_each(
            listener,
            vec![
                ("400 Bad Request", unknown.to_owned()),
                (
                    "200 OK",
                    format!(r#"{{"data":[{{"b64_json":"{}"}}]}}"#, png_base64()),
                ),
            ],
        );
        let image = generate(
            &endpoint(format!("http://{addr}/v1")),
            &request(),
            Duration::from_secs(10),
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(image.bytes, PNG);
        let requests = server.join().unwrap();
        assert_eq!(sent_body(&requests[0])["response_format"], "b64_json");
        assert!(sent_body(&requests[1]).get("response_format").is_none());
        assert_eq!(sent_body(&requests[1])["model"], "image:max");
    }

    #[test]
    fn another_bad_request_is_not_asked_again() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_each(
            listener,
            vec![(
                "400 Bad Request",
                r#"{"error":{"message":"the prompt was refused"}}"#.to_owned(),
            )],
        );
        let err = generate(
            &endpoint(format!("http://{addr}/v1")),
            &request(),
            Duration::from_secs(10),
            &CancelFlag::new(),
        )
        .unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::Http { status: 400, body } if body == "the prompt was refused"),
            "{err:?}"
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn a_body_over_the_cap_is_refused() {
        assert_eq!(read_capped(&b"12345"[..], 5).unwrap(), b"12345");
        let err = read_capped(&b"123456"[..], 5).unwrap_err();
        assert!(matches!(err, ProviderError::Decode(_)), "{err:?}");
    }

    /// A server that reads the request and then answers nothing until
    /// `hold` has passed.
    fn silent_server(hold: Duration) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            read_request(&mut conn);
            std::thread::sleep(hold);
        });
        addr
    }

    #[test]
    fn cancel_ends_a_call_the_server_has_not_answered() {
        let addr = silent_server(Duration::from_secs(10));
        let cancel = CancelFlag::new();
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flag.cancel();
        });
        let started = Instant::now();
        let err = generate(
            &endpoint(format!("http://{addr}/v1")),
            &request(),
            Duration::from_secs(30),
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err.error, ProviderError::Cancelled), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_call_past_its_timeout_fails_saying_so() {
        let addr = silent_server(Duration::from_secs(10));
        let err = generate(
            &endpoint(format!("http://{addr}/v1")),
            &request(),
            Duration::from_secs(1),
            &CancelFlag::new(),
        )
        .unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::Transport(m) if m.contains("no answer within 1s")),
            "{err:?}"
        );
        assert!(err.error.is_transient());
    }

    #[test]
    fn a_body_that_stops_arriving_is_cut_off_at_the_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            read_request(&mut conn);
            conn.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                  content-length: 4096\r\n\r\n{\"data\":[{\"b64_json\":\"",
            )
            .unwrap();
            std::thread::sleep(Duration::from_secs(10));
        });
        let started = Instant::now();
        let err = generate(
            &endpoint(format!("http://{addr}/v1")),
            &request(),
            Duration::from_secs(1),
            &CancelFlag::new(),
        )
        .unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::Transport(m) if m.contains("no answer within 1s")),
            "{err:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
