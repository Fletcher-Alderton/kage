use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::TcpListener;

use base64::Engine as _;

use super::*;

/// An ID3 tag's head, enough for the sniffer to call it MP3.
const MP3: &[u8] = b"ID3\x04\x00\x00\x00\x00\x00\x00song";

/// Answer one request per entry of `answers` on `listener`, in order,
/// each as a status, extra header lines and a body. Returns the
/// requests read.
fn serve(
    listener: TcpListener,
    answers: Vec<(&'static str, String, Vec<u8>)>,
) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, headers, body) in answers {
            let (mut conn, _) = listener.accept().unwrap();
            requests.push(crate::testing::read_request(&mut conn));
            let head = format!(
                "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            conn.write_all(head.as_bytes()).unwrap();
            conn.write_all(&body).unwrap();
        }
        requests
    })
}

fn json(body: &serde_json::Value) -> Vec<u8> {
    body.to_string().into_bytes()
}

fn endpoint(base_url: String) -> MediaEndpoint {
    MediaEndpoint {
        base_url,
        api_key: "music-key".to_owned(),
        headers: BTreeMap::from([("X-Team".to_owned(), "band".to_owned())]),
    }
}

fn request() -> MusicRequest {
    MusicRequest {
        model: "music".to_owned(),
        prompt: "an upbeat synth-pop song".to_owned(),
        ..MusicRequest::default()
    }
}

fn run(
    request: &MusicRequest,
    answers: impl FnOnce(&str) -> Vec<(&'static str, String, Vec<u8>)>,
) -> (Result<GeneratedSong, MediaError>, Vec<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = serve(listener, answers(&origin));
    let result = generate(
        &endpoint(format!("{origin}/v1/")),
        request,
        Duration::from_secs(10),
        &CancelFlag::new(),
    );
    (result, server.join().unwrap())
}

fn sent_body(request: &str) -> serde_json::Value {
    serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap()
}

#[test]
fn a_base64_song_is_read_with_its_seed_and_notes() {
    let answer = serde_json::json!({
        "created": 1,
        "data": [{
            "b64_json": base64::engine::general_purpose::STANDARD.encode(MP3),
            "format": "mp3",
            "seed": 826_527_417,
            "notice": "music-max was busy; music-xhigh served",
            "note": "every song is about 160 s",
        }],
    });
    let (result, requests) = run(&request(), |_| {
        vec![("200 OK", String::new(), json(&answer))]
    });
    let song = result.unwrap();
    assert_eq!(song.audio, Ok((MP3.to_vec(), "audio/mpeg")));
    assert_eq!(song.link, None);
    assert_eq!(song.seed, Some(826_527_417));
    assert_eq!(
        song.note.as_deref(),
        Some("music-max was busy; music-xhigh served; every song is about 160 s")
    );

    let lower = requests[0].to_lowercase();
    assert!(
        lower.starts_with("post /v1/music/generations "),
        "{requests:?}"
    );
    assert!(lower.contains("\r\nauthorization: bearer music-key\r\n"));
    assert!(lower.contains("\r\nx-team: band\r\n"));
    assert_eq!(
        sent_body(&requests[0]),
        serde_json::json!({"model": "music", "prompt": "an upbeat synth-pop song"}),
        "nothing the call left out is sent"
    );
}

#[test]
fn lyrics_instrumental_and_seed_are_sent_when_named() {
    let answer = serde_json::json!({"data": [{"b64_json": base64::engine::general_purpose::STANDARD.encode(MP3)}]});
    let request = MusicRequest {
        lyrics: Some("[verse]\nCity lights".to_owned()),
        instrumental: Some(false),
        seed: Some(7),
        ..request()
    };
    let (result, requests) = run(&request, |_| vec![("200 OK", String::new(), json(&answer))]);
    result.unwrap();
    assert_eq!(
        sent_body(&requests[0]),
        serde_json::json!({
            "model": "music",
            "prompt": "an upbeat synth-pop song",
            "lyrics": "[verse]\nCity lights",
            "instrumental": false,
            "seed": 7,
        })
    );
}

#[test]
fn a_link_on_the_origin_is_fetched_without_the_key() {
    let (result, requests) = run(&request(), |origin| {
        vec![
            (
                "200 OK",
                String::new(),
                json(
                    &serde_json::json!({"data": [{"url": format!("{origin}/f/a.mp3"), "seed": 3}]}),
                ),
            ),
            (
                "200 OK",
                "content-type: audio/mpeg\r\n".to_owned(),
                MP3.to_vec(),
            ),
        ]
    });
    let song = result.unwrap();
    assert_eq!(song.audio, Ok((MP3.to_vec(), "audio/mpeg")));
    assert!(song.link.unwrap().ends_with("/f/a.mp3"));
    let fetch = requests[1].to_lowercase();
    assert!(fetch.starts_with("get /f/a.mp3 "), "{fetch}");
    assert!(!fetch.contains("authorization:"), "{fetch}");
    assert!(!fetch.contains("x-team:"), "{fetch}");
}

#[test]
fn a_song_that_cannot_be_read_keeps_its_link_and_is_not_an_error() {
    // Off the origin: not fetched at all.
    let (result, requests) = run(&request(), |_| {
        vec![(
            "200 OK",
            String::new(),
            json(&serde_json::json!({"data": [{"url": "https://elsewhere.test/f/a.mp3"}]})),
        )]
    });
    let song = result.unwrap();
    assert!(
        matches!(&song.audio, Err(why) if why.starts_with("the link is on another host")),
        "{:?}",
        song.audio
    );
    assert_eq!(song.link.as_deref(), Some("https://elsewhere.test/f/a.mp3"));
    assert_eq!(requests.len(), 1);

    // On the origin, but not audio.
    let (result, _) = run(&request(), |origin| {
        vec![
            (
                "200 OK",
                String::new(),
                json(&serde_json::json!({"data": [{"url": format!("{origin}/f/a.mp3")}]})),
            ),
            ("200 OK", String::new(), b"<html>gone</html>".to_vec()),
        ]
    });
    let song = result.unwrap();
    assert!(
        matches!(&song.audio, Err(why) if why.contains("not MP3 or WAV")),
        "{:?}",
        song.audio
    );
    assert!(song.link.is_some());
}

#[test]
fn a_redirect_from_the_linked_song_is_not_followed() {
    let (result, requests) = run(&request(), |origin| {
        vec![
            (
                "200 OK",
                String::new(),
                json(&serde_json::json!({"data": [{"url": format!("{origin}/f/a.mp3")}]})),
            ),
            (
                "302 Found",
                "location: http://elsewhere.test/a.mp3\r\n".to_owned(),
                Vec::new(),
            ),
        ]
    });
    let song = result.unwrap();
    assert!(song.audio.is_err(), "{:?}", song.audio);
    assert_eq!(requests.len(), 2);
}

#[test]
fn a_refusal_carries_the_servers_message() {
    let (result, _) = run(&request(), |_| {
        vec![(
            "400 Bad Request",
            String::new(),
            json(&serde_json::json!({"error": {"message": "the prompt was refused"}})),
        )]
    });
    let err = result.unwrap_err();
    assert!(
        matches!(&err.error, ProviderError::Http { status: 400, body } if body == "the prompt was refused"),
        "{err:?}"
    );
}

#[test]
fn an_answer_without_a_song_is_an_error() {
    let no_fetch = |url: &str| -> Result<Vec<u8>, ProviderError> { panic!("fetched {url}") };
    for body in [
        &br#"{"data":[]}"#[..],
        br#"{"data":[{"seed":1}]}"#,
        br#"{"data":[{"b64_json":"!!!"}]}"#,
        b"not json",
    ] {
        let err = parse_answer(body, no_fetch).unwrap_err();
        assert!(matches!(err, ProviderError::Decode(_)), "{err:?}");
    }
}
