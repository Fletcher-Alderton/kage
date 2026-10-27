//! Image generation and editing over the `OpenAI` Images API.
//! Generation is `POST {base_url}/images/generations`, one image a
//! call, answered as base64. Editing is `POST {base_url}/images/edits`,
//! sent as `multipart/form-data` carrying one image's bytes, and
//! answered the same way.
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

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use kage_core::CancelFlag;

use crate::ProviderError;
use crate::error::nonempty;
use crate::media::{self, MediaEndpoint, MediaError, RequestBody, names_field, server_message};

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

/// One image to edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageEdit {
    /// The model the server is asked for.
    pub model: String,
    /// What to change.
    pub prompt: String,
    /// The size as `WxH`, which a server may round or refuse. `None`
    /// leaves it to the server.
    pub size: Option<String>,
    /// The image file's bytes, as read from the workspace.
    pub image: Vec<u8>,
    /// The type the bytes themselves declare, as
    /// [`kage_core::image::sniff_mime`] answers it.
    pub mime: &'static str,
}

/// Generate one image, waiting at most `timeout` for the whole call.
///
/// When `ask_base64` is set the answer is asked for as base64; a
/// refusal naming that field stores `false` in it and the request is
/// asked once more without it, within the same deadline, so the field
/// stays off for later calls.
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
///   body over [`media::MAX_RESPONSE_BYTES`], or bytes that are not a PNG,
///   JPEG, GIF or WebP image.
pub fn generate(
    endpoint: &MediaEndpoint,
    request: &ImageRequest,
    timeout: Duration,
    cancel: &CancelFlag,
    ask_base64: &AtomicBool,
) -> Result<GeneratedImage, MediaError> {
    let deadline = Instant::now() + timeout;
    let ask = ask_base64.load(Ordering::Relaxed);
    let mut result = post(endpoint, request, deadline, timeout, cancel, ask);
    // A 400 is answered before any image is made, so asking again costs
    // a round trip and no generation. The retry shares the deadline,
    // and the refusal is remembered for later calls.
    if ask
        && result
            .as_ref()
            .is_err_and(|error| names_field(error, "response_format"))
    {
        ask_base64.store(false, Ordering::Relaxed);
        result = post(endpoint, request, deadline, timeout, cancel, false);
    }
    result.map_err(|refused| MediaError {
        error: server_message(refused.error),
        ..refused
    })
}

/// One request, asking for base64 when `ask_base64` is set, given what
/// is left until `deadline` of the call's `timeout`. An HTTP error keeps
/// its body as sent.
fn post(
    endpoint: &MediaEndpoint,
    request: &ImageRequest,
    deadline: Instant,
    timeout: Duration,
    cancel: &CancelFlag,
    ask_base64: bool,
) -> Result<GeneratedImage, MediaError> {
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
    let body = media::call(
        endpoint,
        "images/generations",
        RequestBody::Json(body),
        "the image answer",
        deadline,
        timeout,
        cancel,
    )?;
    Ok(parse_answer(&body)?)
}

/// Edit one image from a prompt, waiting at most `timeout` for the
/// whole call.
///
/// The request is `multipart/form-data` carrying the model, the prompt
/// and the image's bytes. When `ask_base64` is set the answer is asked
/// for as base64; a refusal naming that field stores `false` in it and
/// the request is asked once more without it, within the same deadline,
/// so the field stays off for later calls.
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
///   body over [`media::MAX_RESPONSE_BYTES`], or bytes that are not a
///   PNG, JPEG, GIF or WebP image.
pub fn edit(
    endpoint: &MediaEndpoint,
    edit: &ImageEdit,
    timeout: Duration,
    cancel: &CancelFlag,
    ask_base64: &AtomicBool,
) -> Result<GeneratedImage, MediaError> {
    let deadline = Instant::now() + timeout;
    let ask = ask_base64.load(Ordering::Relaxed);
    let mut result = post_edit(endpoint, edit, deadline, timeout, cancel, ask);
    // A 400 is answered before any image is written, so asking again
    // costs a re-upload and no generation. The retry shares the
    // deadline, and the refusal is remembered for later calls.
    if ask
        && result
            .as_ref()
            .is_err_and(|error| names_field(error, "response_format"))
    {
        ask_base64.store(false, Ordering::Relaxed);
        result = post_edit(endpoint, edit, deadline, timeout, cancel, false);
    }
    result.map_err(|refused| MediaError {
        error: server_message(refused.error),
        ..refused
    })
}

/// One request, asking for base64 when `ask_base64` is set, given what
/// is left until `deadline` of the call's `timeout`. An HTTP error
/// keeps its body as sent.
fn post_edit(
    endpoint: &MediaEndpoint,
    edit: &ImageEdit,
    deadline: Instant,
    timeout: Duration,
    cancel: &CancelFlag,
    ask_base64: bool,
) -> Result<GeneratedImage, MediaError> {
    let extension = kage_core::image::extension_for(edit.mime);
    debug_assert!(extension.is_some(), "{} is not a sniffed type", edit.mime);
    let extension = extension.unwrap_or("png");
    let mut form = crate::multipart::Form::new();
    form.text("model", &edit.model);
    form.text("prompt", &edit.prompt);
    form.text("n", "1");
    if ask_base64 {
        form.text("response_format", "b64_json");
    }
    if let Some(size) = &edit.size {
        form.text("size", size);
    }
    form.file(
        "image",
        &format!("image.{extension}"),
        edit.mime,
        &edit.image,
    );
    let (content_type, body) = form.finish();
    let body = media::call(
        endpoint,
        "images/edits",
        RequestBody::Multipart {
            content_type,
            bytes: body,
        },
        "the image answer",
        deadline,
        timeout,
        cancel,
    )?;
    Ok(parse_answer(&body)?)
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write as _;
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
            let raw = crate::testing::read_request(&mut conn);
            let answer = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{headers}\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            conn.write_all(answer.as_bytes()).unwrap();
            raw
        })
    }

    fn endpoint(base_url: String) -> MediaEndpoint {
        MediaEndpoint {
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
    ) -> (Result<GeneratedImage, MediaError>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_one(listener, status, headers, body);
        let result = generate(
            &endpoint(format!("http://{addr}/v1/")),
            &request(),
            Duration::from_secs(10),
            &CancelFlag::new(),
            &AtomicBool::new(true),
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
        let endpoint = MediaEndpoint {
            base_url: format!("http://{addr}/v1"),
            ..MediaEndpoint::default()
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
            &AtomicBool::new(true),
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
                requests.push(crate::testing::read_request(&mut conn));
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
        let flag = AtomicBool::new(true);
        let image = generate(
            &endpoint(format!("http://{addr}/v1")),
            &request(),
            Duration::from_secs(10),
            &CancelFlag::new(),
            &flag,
        )
        .unwrap();
        assert_eq!(image.bytes, PNG);
        assert!(!flag.load(Ordering::Relaxed));
        let requests = server.join().unwrap();
        assert_eq!(sent_body(&requests[0])["response_format"], "b64_json");
        assert!(sent_body(&requests[1]).get("response_format").is_none());
        assert_eq!(sent_body(&requests[1])["model"], "image:max");

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = format!(r#"{{"data":[{{"b64_json":"{}"}}]}}"#, png_base64());
        let server = serve_each(listener, vec![("200 OK", body)]);
        generate(
            &endpoint(format!("http://{addr}/v1")),
            &request(),
            Duration::from_secs(10),
            &CancelFlag::new(),
            &flag,
        )
        .unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(sent_body(&requests[0]).get("response_format").is_none());
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
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::Http { status: 400, body } if body == "the prompt was refused"),
            "{err:?}"
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    /// A server that reads the request and then answers nothing until
    /// `hold` has passed.
    fn silent_server(hold: Duration) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            crate::testing::read_request(&mut conn);
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
            &AtomicBool::new(true),
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
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::Transport(m) if m.contains("no answer within 1s")),
            "{err:?}"
        );
        assert!(err.error.is_transient());
    }

    #[test]
    fn cancel_ends_a_refusal_whose_body_stops_arriving() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            crate::testing::read_request(&mut conn);
            conn.write_all(
                b"HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\n\
                  content-length: 4096\r\n\r\n{\"error\":",
            )
            .unwrap();
            std::thread::sleep(Duration::from_secs(10));
        });
        let cancel = CancelFlag::new();
        let trigger = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            trigger.cancel();
        });
        let started = Instant::now();
        let err = generate(
            &endpoint(format!("http://{addr}/v1")),
            &request(),
            Duration::from_secs(30),
            &cancel,
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert!(matches!(err.error, ProviderError::Cancelled), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_body_that_stops_arriving_is_cut_off_at_the_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            crate::testing::read_request(&mut conn);
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
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::Transport(m) if m.contains("no answer within 1s")),
            "{err:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// Read one request off `listener` as raw bytes and answer it with
    /// `status`, `headers` and `body`. Returns the request read.
    /// Multipart bodies are not UTF-8, so the bytes are kept as read.
    fn serve_bytes(
        listener: TcpListener,
        status: &'static str,
        headers: &'static str,
        body: String,
    ) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let raw = crate::testing::read_request_bytes(&mut conn);
            let answer = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{headers}\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            conn.write_all(answer.as_bytes()).unwrap();
            raw
        })
    }

    /// Answer one request per entry of `answers`, in order, and return
    /// the requests read as raw bytes.
    fn serve_each_bytes(
        listener: TcpListener,
        answers: Vec<(&'static str, String)>,
    ) -> std::thread::JoinHandle<Vec<Vec<u8>>> {
        std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in answers {
                let (mut conn, _) = listener.accept().unwrap();
                requests.push(crate::testing::read_request_bytes(&mut conn));
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

    fn edit_request(size: Option<String>) -> ImageEdit {
        ImageEdit {
            model: "image:max".to_owned(),
            prompt: "make the kite blue".to_owned(),
            size,
            image: PNG.to_vec(),
            mime: "image/png",
        }
    }

    /// Serve one request for `image`, answer it, and return the call's
    /// result and the raw request read.
    fn run_edit(
        status: &'static str,
        headers: &'static str,
        body: String,
        ask_base64: &AtomicBool,
        image: &ImageEdit,
    ) -> (Result<GeneratedImage, MediaError>, Vec<u8>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_bytes(listener, status, headers, body);
        let result = edit(
            &endpoint(format!("http://{addr}/v1/")),
            image,
            Duration::from_secs(10),
            &CancelFlag::new(),
            ask_base64,
        );
        (result, server.join().unwrap())
    }

    /// `haystack` split at its first `needle`: what is before it and
    /// what follows it.
    fn split_once<'a>(haystack: &'a [u8], needle: &[u8]) -> Option<(&'a [u8], &'a [u8])> {
        let at = haystack.windows(needle.len()).position(|w| w == needle)?;
        Some((&haystack[..at], &haystack[at + needle.len()..]))
    }

    /// The multipart parts of `request` in order, as name, headers as
    /// sent (lowercased, for case-insensitive matching) and value.
    fn parts(request: &[u8]) -> Vec<(String, String, Vec<u8>)> {
        let head_end = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&request[..head_end]).to_lowercase();
        let boundary = head
            .lines()
            .find_map(|l| l.strip_prefix("content-type: multipart/form-data; boundary="))
            .unwrap()
            .trim()
            .to_owned();
        assert!(boundary.starts_with("kage-"), "{boundary}");
        let delimiter = format!("\r\n--{boundary}");
        let dash_boundary = format!("--{boundary}");
        let mut rest = request[head_end..]
            .strip_prefix(dash_boundary.as_bytes())
            .unwrap();
        let mut found = Vec::new();
        loop {
            rest = rest.strip_prefix(b"\r\n").unwrap();
            let (part_head, after) = split_once(rest, b"\r\n\r\n").unwrap();
            let part_head = String::from_utf8_lossy(part_head).to_lowercase();
            let name = part_head
                .lines()
                .find_map(|l| l.strip_prefix("content-disposition: form-data; name=\""))
                .unwrap()
                .split('"')
                .next()
                .unwrap()
                .to_owned();
            let (value, after) = split_once(after, delimiter.as_bytes()).unwrap();
            found.push((name, part_head, value.to_vec()));
            rest = after;
            if rest.starts_with(b"--") {
                assert_eq!(rest, b"--\r\n", "the closing boundary ends the body");
                return found;
            }
        }
    }

    fn part_names(sent: &[(String, String, Vec<u8>)]) -> Vec<&str> {
        sent.iter().map(|(name, ..)| name.as_str()).collect()
    }

    #[test]
    fn posts_the_edit_parts_with_the_key_and_headers() {
        let body = format!(r#"{{"data":[{{"b64_json":"{}"}}]}}"#, png_base64());
        let (result, request) = run_edit(
            "200 OK",
            "",
            body,
            &AtomicBool::new(true),
            &edit_request(Some("1024x1024".to_owned())),
        );
        let image = result.unwrap();
        assert_eq!(image.bytes, PNG);
        assert_eq!(image.mime, "image/png");

        let lower = String::from_utf8_lossy(&request).to_lowercase();
        assert!(
            lower.starts_with("post /v1/images/edits "),
            "a trailing slash on the base must not double: {lower}"
        );
        assert!(
            lower.contains("\r\nauthorization: bearer img-key\r\n"),
            "{lower}"
        );
        assert!(lower.contains("\r\nx-team: art\r\n"), "{lower}");
        assert!(lower.contains("\r\nuser-agent: kage/"), "{lower}");
        let sent = parts(&request);
        assert_eq!(
            part_names(&sent),
            ["model", "prompt", "n", "response_format", "size", "image"]
        );
        assert_eq!(sent[0].2.as_slice(), b"image:max".as_slice());
        assert_eq!(sent[1].2.as_slice(), b"make the kite blue".as_slice());
        assert_eq!(sent[2].2.as_slice(), b"1".as_slice());
        assert_eq!(sent[3].2.as_slice(), b"b64_json".as_slice());
        assert_eq!(sent[4].2.as_slice(), b"1024x1024".as_slice());
        let (_, head, value) = &sent[5];
        assert!(head.contains("filename=\"image.png\""), "{head}");
        assert!(head.contains("content-type: image/png"), "{head}");
        assert_eq!(value.as_slice(), PNG);
    }

    #[test]
    fn no_size_sends_no_size_part_and_a_bare_base_still_routes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = format!(r#"{{"data":[{{"b64_json":"{}"}}]}}"#, png_base64());
        let server = serve_bytes(listener, "200 OK", "", body);
        let image = edit(
            &endpoint(format!("http://{addr}/v1")),
            &edit_request(None),
            Duration::from_secs(10),
            &CancelFlag::new(),
            &AtomicBool::new(true),
        )
        .unwrap();
        assert_eq!(image.bytes, PNG);
        let request = server.join().unwrap();
        let lower = String::from_utf8_lossy(&request).to_lowercase();
        assert!(lower.starts_with("post /v1/images/edits "), "{lower}");
        assert_eq!(
            part_names(&parts(&request)),
            ["model", "prompt", "n", "response_format", "image"]
        );
    }

    #[test]
    fn an_edit_refusing_response_format_is_asked_again_without_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let unknown = r#"{"error":{"message":"Unknown parameter: 'response_format'.","type":"invalid_request_error","param":"response_format","code":"unknown_parameter"}}"#;
        let server = serve_each_bytes(
            listener,
            vec![
                ("400 Bad Request", unknown.to_owned()),
                (
                    "200 OK",
                    format!(r#"{{"data":[{{"b64_json":"{}"}}]}}"#, png_base64()),
                ),
            ],
        );
        let flag = AtomicBool::new(true);
        let image = edit(
            &endpoint(format!("http://{addr}/v1")),
            &edit_request(None),
            Duration::from_secs(10),
            &CancelFlag::new(),
            &flag,
        )
        .unwrap();
        assert_eq!(image.bytes, PNG);
        assert!(!flag.load(Ordering::Relaxed));
        let requests = server.join().unwrap();
        assert!(part_names(&parts(&requests[0])).contains(&"response_format"));
        assert!(!part_names(&parts(&requests[1])).contains(&"response_format"));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = format!(r#"{{"data":[{{"b64_json":"{}"}}]}}"#, png_base64());
        let server = serve_each_bytes(listener, vec![("200 OK", body)]);
        edit(
            &endpoint(format!("http://{addr}/v1")),
            &edit_request(None),
            Duration::from_secs(10),
            &CancelFlag::new(),
            &flag,
        )
        .unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!part_names(&parts(&requests[0])).contains(&"response_format"));
    }

    #[test]
    fn an_edit_bad_request_not_naming_the_field_is_not_asked_again() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_each_bytes(
            listener,
            vec![(
                "400 Bad Request",
                r#"{"error":{"message":"the prompt was refused"}}"#.to_owned(),
            )],
        );
        let flag = AtomicBool::new(true);
        let err = edit(
            &endpoint(format!("http://{addr}/v1")),
            &edit_request(None),
            Duration::from_secs(10),
            &CancelFlag::new(),
            &flag,
        )
        .unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::Http { status: 400, body } if body == "the prompt was refused"),
            "{err:?}"
        );
        assert_eq!(server.join().unwrap().len(), 1);
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn an_edit_refusal_carries_the_server_message_and_its_wait() {
        let (result, _) = run_edit(
            "401 Unauthorized",
            "",
            r#"{"error":{"message":"invalid key"}}"#.to_owned(),
            &AtomicBool::new(true),
            &edit_request(None),
        );
        assert!(matches!(result.unwrap_err().error, ProviderError::Auth(m) if m == "invalid key"));

        let (result, _) = run_edit(
            "413 Payload Too Large",
            "",
            r#"{"error":{"message":"the image is too large"}}"#.to_owned(),
            &AtomicBool::new(true),
            &edit_request(None),
        );
        let err = result.unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::Http { status: 413, body } if body == "the image is too large"),
            "{err:?}"
        );

        let (result, _) = run_edit(
            "429 Too Many Requests",
            "retry-after: 7\r\n",
            r#"{"error":{"message":"slow down"}}"#.to_owned(),
            &AtomicBool::new(true),
            &edit_request(None),
        );
        let err = result.unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::RateLimited { body: Some(b), .. } if b == "slow down"),
            "{err:?}"
        );
        assert_eq!(err.retry_after, Some(Duration::from_secs(7)));

        let (result, _) = run_edit(
            "503 Service Unavailable",
            "retry-after: 12\r\n",
            r#"{"error":{"message":"every engine is busy"}}"#.to_owned(),
            &AtomicBool::new(true),
            &edit_request(None),
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
}
