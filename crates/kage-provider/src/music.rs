//! Song generation: `POST {base_url}/music/generations`, JSON in, one
//! song out.
//!
//! There is no common music API; this follows the images shape the
//! servers that offer one copy. The answer carries the song in its first
//! `data` entry: as base64 in `b64_json`, as a `data:` URI in `url`, or as
//! a link in `url`. A link on the endpoint's own origin is fetched without
//! the key and without following a redirect; a link anywhere else is not.
//! A song the client cannot read was still made and paid for, so that is
//! not an error: the answer says why and keeps the link.
//!
//! `n`, `response_format` and `duration` are never sent.

use std::time::{Duration, Instant};

use kage_core::CancelFlag;

use crate::ProviderError;
use crate::media::{self, MediaEndpoint, MediaError, RequestBody, server_message};

/// One song to make.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MusicRequest {
    /// The model the server is asked for.
    pub model: String,
    /// The style of the song: genre, mood, instruments, vocals.
    pub prompt: String,
    /// The words to sing. `None` leaves them to the server.
    pub lyrics: Option<String>,
    /// `Some(true)` asks for a song with no vocals. `None` leaves it out.
    pub instrumental: Option<bool>,
    /// The seed to make the song with. `None` leaves it to the server.
    pub seed: Option<u64>,
}

/// The song a server answered with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedSong {
    /// The song's bytes and the type they declare, or why they could
    /// not be read.
    pub audio: Result<(Vec<u8>, &'static str), String>,
    /// The link the server answered with, when it answered one.
    pub link: Option<String>,
    /// The seed the song was made with, when the server said.
    pub seed: Option<u64>,
    /// What the server said about the song, such as a fallback to
    /// another tier.
    pub note: Option<String>,
}

/// Make one song, waiting at most `timeout` for the whole call, the
/// fetch of a linked song included.
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
/// - [`ProviderError::Decode`] for an answer that is not JSON or holds
///   no song. A song that is made but cannot be read is not an error.
pub fn generate(
    endpoint: &MediaEndpoint,
    request: &MusicRequest,
    timeout: Duration,
    cancel: &CancelFlag,
) -> Result<GeneratedSong, MediaError> {
    let deadline = Instant::now() + timeout;
    let mut body = serde_json::json!({
        "model": request.model,
        "prompt": request.prompt,
    });
    if let Some(lyrics) = &request.lyrics {
        body["lyrics"] = serde_json::Value::String(lyrics.clone());
    }
    if let Some(instrumental) = request.instrumental {
        body["instrumental"] = serde_json::Value::Bool(instrumental);
    }
    if let Some(seed) = request.seed {
        body["seed"] = serde_json::Value::from(seed);
    }
    let answer = media::call(
        endpoint,
        "music/generations",
        RequestBody::Json(body),
        "the song answer",
        deadline,
        timeout,
        cancel,
    )
    .map_err(|refused| MediaError {
        error: server_message(refused.error),
        ..refused
    })?;
    Ok(parse_answer(&answer, |url| {
        fetch_on_origin(endpoint, url, deadline, timeout, cancel)
    })?)
}

/// The song a server linked at `url`, when the link is on the
/// endpoint's own origin, read within the call's deadline.
fn fetch_on_origin(
    endpoint: &MediaEndpoint,
    url: &str,
    deadline: Instant,
    timeout: Duration,
    cancel: &CancelFlag,
) -> Result<Vec<u8>, ProviderError> {
    let origin = media::origin(endpoint.base_url.trim());
    if !url.starts_with(&format!("{origin}/")) {
        return Err(ProviderError::Decode(
            "the link is on another host, and kage does not fetch it".to_owned(),
        ));
    }
    media::fetch_link(url, "the song", deadline, timeout, cancel)
}

/// The song in an answer's first `data` entry. A `url` that is not a
/// `data:` URI is handed to `fetch_link`; a cancel while it runs is an
/// error, and any other failure to read the song is kept in the answer.
fn parse_answer(
    body: &[u8],
    fetch_link: impl FnOnce(&str) -> Result<Vec<u8>, ProviderError>,
) -> Result<GeneratedSong, ProviderError> {
    let json: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| ProviderError::Decode(format!("the song answer is not JSON: {e}")))?;
    let first = json
        .pointer("/data/0")
        .ok_or_else(|| ProviderError::Decode("the song answer holds no song".to_owned()))?;
    let field = |key: &str| first.get(key).and_then(serde_json::Value::as_str);
    let mut link = None;
    let bytes = if let Some(encoded) = field("b64_json") {
        Ok(media::decode_base64(encoded, "the song")?)
    } else if let Some(url) = field("url") {
        if let Some(uri) = url.strip_prefix("data:") {
            Ok(media::decode_data_uri(uri, "the song")?)
        } else {
            link = Some(url.to_owned());
            match fetch_link(url) {
                Err(ProviderError::Cancelled) => return Err(ProviderError::Cancelled),
                Err(ProviderError::Decode(why)) => Err(why),
                fetched => fetched.map_err(|error| error.to_string()),
            }
        }
    } else {
        return Err(ProviderError::Decode(
            "the song answer holds neither `b64_json` nor `url`".to_owned(),
        ));
    };
    let audio = bytes.and_then(|bytes| match kage_core::audio::sniff_mime(&bytes) {
        Some(mime) => Ok((bytes, mime)),
        None => Err("the song is not MP3 or WAV audio".to_owned()),
    });
    let note = ["notice", "note"]
        .into_iter()
        .filter_map(field)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    Ok(GeneratedSong {
        audio,
        link,
        seed: first.get("seed").and_then(serde_json::Value::as_u64),
        note: (!note.is_empty()).then_some(note),
    })
}

#[cfg(test)]
mod tests;
