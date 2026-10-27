//! Speech synthesis over the `OpenAI` speech shape:
//! `POST {base_url}/audio/speech`, JSON in, the clip's bytes out.
//!
//! Many providers and self-hosted servers speak this shape. The answer
//! arrives whole rather than streamed, so a call carries a ceiling on
//! its whole duration instead of the idle deadline a chat stream gets,
//! and the body read is capped.
//!
//! The clip's type is read from the bytes themselves; the answer's
//! `Content-Type` is never used. `response_format` asks for `mp3`, so a
//! server whose default is another format does not make a clip this
//! client then refuses. `language` is not an `OpenAI` field. A refusal
//! naming either is asked again without it. `speed` and `instructions`
//! are never sent.

use std::time::{Duration, Instant};

use kage_core::CancelFlag;

use crate::ProviderError;
use crate::media::{self, MediaEndpoint, MediaError, RequestBody, names_field, server_message};

/// One clip to synthesize.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpeechRequest {
    /// The model the server is asked for.
    pub model: String,
    /// The text to read aloud.
    pub input: String,
    /// The voice to read with. `None` leaves it to the server.
    pub voice: Option<String>,
    /// The text's language as an ISO 639-1 code, a hint some servers
    /// take. `None` leaves it out.
    pub language: Option<String>,
}

/// The clip a server answered with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedAudio {
    /// The audio file's bytes.
    pub bytes: Vec<u8>,
    /// The type the bytes themselves declare. The answer's own claim is
    /// never used.
    pub mime: &'static str,
}

/// Speak one text, waiting at most `timeout` for the whole call.
///
/// The clip is asked for as MP3, and `language`, when the request
/// names one, is sent as a hint. A refusal naming either field is asked
/// again without the fields it names, within the same deadline; a
/// server that refuses them one at a time is asked at most three times.
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
/// - [`ProviderError::Decode`] for a body over
///   [`media::MAX_RESPONSE_BYTES`] or bytes that are neither MP3 nor
///   WAV audio.
pub fn speak(
    endpoint: &MediaEndpoint,
    request: &SpeechRequest,
    timeout: Duration,
    cancel: &CancelFlag,
) -> Result<GeneratedAudio, MediaError> {
    let deadline = Instant::now() + timeout;
    let mut language = request.language.is_some();
    let mut format = true;
    let mut result = post(
        endpoint, request, deadline, timeout, cancel, language, format,
    );
    // A 400 is answered before any audio is made, so asking again
    // costs a round trip and no synthesis. Each retry shares the
    // deadline and drops only fields that were sent and are named.
    for _ in 0..2 {
        let Err(error) = &result else { break };
        let drop_language = language && names_field(error, "language");
        let drop_format = format && names_field(error, "response_format");
        if !drop_language && !drop_format {
            break;
        }
        language &= !drop_language;
        format &= !drop_format;
        result = post(
            endpoint, request, deadline, timeout, cancel, language, format,
        );
    }
    result.map_err(|refused| MediaError {
        error: server_message(refused.error),
        ..refused
    })
}

/// One request, sending `language` when `ask_language` is set and
/// `response_format` when `ask_format` is, given what is left until
/// `deadline` of the call's `timeout`. An HTTP error keeps its body as
/// sent.
fn post(
    endpoint: &MediaEndpoint,
    request: &SpeechRequest,
    deadline: Instant,
    timeout: Duration,
    cancel: &CancelFlag,
    ask_language: bool,
    ask_format: bool,
) -> Result<GeneratedAudio, MediaError> {
    let mut body = serde_json::json!({
        "model": request.model,
        "input": request.input,
    });
    if let Some(voice) = &request.voice {
        body["voice"] = serde_json::Value::String(voice.clone());
    }
    if ask_language && let Some(language) = &request.language {
        body["language"] = serde_json::Value::String(language.clone());
    }
    if ask_format {
        body["response_format"] = serde_json::Value::String("mp3".to_owned());
    }
    let bytes = media::call(
        endpoint,
        "audio/speech",
        RequestBody::Json(body),
        "the speech answer",
        deadline,
        timeout,
        cancel,
    )?;
    let mime = kage_core::audio::sniff_mime(&bytes)
        .ok_or_else(|| ProviderError::Decode("the answer is not MP3 or WAV audio".to_owned()))?;
    Ok(GeneratedAudio { bytes, mime })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write as _;
    use std::net::TcpListener;

    use super::*;

    /// An MP3: an ID3 tag followed by an MPEG-1 Layer III frame header.
    const MP3: &[u8] = &[
        b'I', b'D', b'3', 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFB, 0x90, 0x64,
    ];

    /// A WAV: the `RIFF` header and a `fmt ` chunk start.
    const WAV: &[u8] = b"RIFF\0\0\0\0WAVEfmt \x10\x00\x00\x00";

    /// Read one request off `listener` and answer it with `status`,
    /// `headers`, `content_type` and `body`. Returns the raw request.
    fn serve_one(
        listener: TcpListener,
        status: &'static str,
        headers: &'static str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let raw = crate::testing::read_request(&mut conn);
            let answer = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n{headers}\
                 content-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            conn.write_all(answer.as_bytes()).unwrap();
            conn.write_all(&body).unwrap();
            raw
        })
    }

    /// Answer one request per entry of `answers`, in order, and return
    /// the requests read.
    fn serve_each(
        listener: TcpListener,
        answers: Vec<(&'static str, Vec<u8>)>,
    ) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in answers {
                let (mut conn, _) = listener.accept().unwrap();
                requests.push(crate::testing::read_request(&mut conn));
                let answer = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                conn.write_all(answer.as_bytes()).unwrap();
                conn.write_all(&body).unwrap();
            }
            requests
        })
    }

    fn endpoint(base_url: String) -> MediaEndpoint {
        MediaEndpoint {
            base_url,
            api_key: "tts-key".to_owned(),
            headers: BTreeMap::from([("X-Team".to_owned(), "audio".to_owned())]),
        }
    }

    fn request() -> SpeechRequest {
        SpeechRequest {
            model: "tts:max".to_owned(),
            input: "a red kite".to_owned(),
            voice: Some("alloy".to_owned()),
            language: Some("en".to_owned()),
        }
    }

    /// Serve one request on a fresh server reached at a base with a
    /// trailing `/`, answer it, and return the call's result and the
    /// raw request read.
    fn run(
        status: &'static str,
        headers: &'static str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> (Result<GeneratedAudio, MediaError>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_one(listener, status, headers, content_type, body);
        let result = speak(
            &endpoint(format!("http://{addr}/v1/")),
            &request(),
            Duration::from_secs(10),
            &CancelFlag::new(),
        );
        (result, server.join().unwrap())
    }

    fn sent_body(request: &str) -> serde_json::Value {
        serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    #[test]
    fn posts_the_speech_request_with_the_key_and_headers() {
        let (result, request) = run("200 OK", "", "application/octet-stream", MP3.to_vec());
        let clip = result.unwrap();
        assert_eq!(clip.bytes, MP3);
        assert_eq!(clip.mime, "audio/mpeg");

        let lower = request.to_lowercase();
        assert!(
            lower.starts_with("post /v1/audio/speech "),
            "a trailing slash on the base must not double: {request}"
        );
        assert!(
            lower.contains("\r\nauthorization: bearer tts-key\r\n"),
            "{request}"
        );
        assert!(lower.contains("\r\nx-team: audio\r\n"), "{request}");
        assert!(lower.contains("\r\nuser-agent: kage/"), "{request}");
        let sent = sent_body(&request);
        assert_eq!(
            sent,
            serde_json::json!({
                "model": "tts:max",
                "input": "a red kite",
                "voice": "alloy",
                "language": "en",
                "response_format": "mp3",
            })
        );
        for never in ["speed", "instructions"] {
            assert!(sent.get(never).is_none(), "{sent}");
        }
    }

    #[test]
    fn no_voice_and_no_language_send_none_and_a_bare_base_still_routes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_one(listener, "200 OK", "", "audio/mpeg", MP3.to_vec());
        let bare = SpeechRequest {
            voice: None,
            language: None,
            ..request()
        };
        let clip = speak(
            &endpoint(format!("http://{addr}/v1")),
            &bare,
            Duration::from_secs(10),
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(clip.mime, "audio/mpeg");
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /v1/audio/speech "), "{request}");
        let sent = sent_body(&request);
        assert!(sent.get("voice").is_none(), "{sent}");
        assert!(sent.get("language").is_none(), "{sent}");
        assert_eq!(sent["response_format"], "mp3");
        for never in ["speed", "instructions"] {
            assert!(sent.get(never).is_none(), "{sent}");
        }
    }

    #[test]
    fn answers_are_sniffed_from_their_bytes() {
        let (result, _) = run("200 OK", "", "application/octet-stream", MP3.to_vec());
        assert_eq!(result.unwrap().mime, "audio/mpeg");

        let (result, _) = run("200 OK", "", "audio/mpeg", WAV.to_vec());
        assert_eq!(result.unwrap().mime, "audio/wav");

        let (result, _) = run("200 OK", "", "audio/mpeg", b"just text".to_vec());
        let err = result.unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::Decode(m) if m == "the answer is not MP3 or WAV audio"),
            "{err:?}"
        );
    }

    #[test]
    fn a_refusal_naming_language_is_asked_again_without_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let unknown = r#"{"error":{"message":"Unknown parameter: 'language'.","type":"invalid_request_error","param":"language","code":"unknown_parameter"}}"#;
        let server = serve_each(
            listener,
            vec![
                ("400 Bad Request", unknown.as_bytes().to_vec()),
                ("200 OK", MP3.to_vec()),
            ],
        );
        let clip = speak(
            &endpoint(format!("http://{addr}/v1")),
            &request(),
            Duration::from_secs(10),
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(clip.bytes, MP3);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(sent_body(&requests[0])["language"], "en");
        assert!(sent_body(&requests[1]).get("language").is_none());
        assert_eq!(sent_body(&requests[1])["response_format"], "mp3");
        assert_eq!(sent_body(&requests[1])["model"], "tts:max");
    }

    #[test]
    fn fields_refused_one_at_a_time_are_each_dropped() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let refuse = |field: &str| {
            format!(
                r#"{{"error":{{"message":"Unknown parameter: '{field}'.","param":"{field}"}}}}"#
            )
            .into_bytes()
        };
        let server = serve_each(
            listener,
            vec![
                ("400 Bad Request", refuse("response_format")),
                ("400 Bad Request", refuse("language")),
                ("200 OK", MP3.to_vec()),
            ],
        );
        let clip = speak(
            &endpoint(format!("http://{addr}/v1")),
            &request(),
            Duration::from_secs(10),
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(clip.bytes, MP3);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        let second = sent_body(&requests[1]);
        assert!(second.get("response_format").is_none(), "{second}");
        assert_eq!(second["language"], "en");
        let third = sent_body(&requests[2]);
        assert!(third.get("response_format").is_none(), "{third}");
        assert!(third.get("language").is_none(), "{third}");
    }

    #[test]
    fn another_bad_request_is_not_asked_again() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_each(
            listener,
            vec![(
                "400 Bad Request",
                br#"{"error":{"message":"the text was refused"}}"#.to_vec(),
            )],
        );
        let err = speak(
            &endpoint(format!("http://{addr}/v1")),
            &request(),
            Duration::from_secs(10),
            &CancelFlag::new(),
        )
        .unwrap_err();
        assert!(
            matches!(&err.error, ProviderError::Http { status: 400, body } if body == "the text was refused"),
            "{err:?}"
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn a_call_without_language_is_not_asked_again() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_each(
            listener,
            vec![(
                "400 Bad Request",
                br#"{"error":{"message":"Unknown parameter: 'language'."}}"#.to_vec(),
            )],
        );
        let bare = SpeechRequest {
            language: None,
            ..request()
        };
        let err = speak(
            &endpoint(format!("http://{addr}/v1")),
            &bare,
            Duration::from_secs(10),
            &CancelFlag::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err.error, ProviderError::Http { status: 400, .. }),
            "{err:?}"
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn a_refusal_carries_the_server_message_and_its_wait() {
        let (result, _) = run(
            "401 Unauthorized",
            "",
            "application/json",
            br#"{"error":{"message":"invalid key"}}"#.to_vec(),
        );
        assert!(matches!(result.unwrap_err().error, ProviderError::Auth(m) if m == "invalid key"));

        let (result, _) = run(
            "429 Too Many Requests",
            "retry-after: 7\r\n",
            "application/json",
            br#"{"error":{"message":"slow down"}}"#.to_vec(),
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
            "application/json",
            br#"{"error":{"message":"every engine is busy"}}"#.to_vec(),
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
