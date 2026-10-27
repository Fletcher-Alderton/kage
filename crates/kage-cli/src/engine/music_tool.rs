//! The `generate_music` tool: one song from a prompt and optional
//! lyrics, from the music endpoint `[tools.generate_music]` names.
//!
//! The tool is registered only when that table resolves to an endpoint
//! and a model, so a config without one advertises the same tools as
//! before. Its song is the result's audio media (see [`ToolMedia`]) and
//! stays in the session, where `/media` and the desktop play and save
//! it. A call may also name a `path` in the workspace to save it there at
//! once.
//!
//! A song the server made but kage could not read, such as one linked on
//! another host, is not a failure: the result says why and gives the
//! link, which anyone holding it can fetch until the server drops it.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use kage_core::config::Config;
use kage_core::{Risk, ToolMedia, ToolOutput, ToolUpdate};
use kage_provider::ProviderError;
use kage_provider::media::{MediaEndpoint, MediaError};
use kage_provider::music::{self, GeneratedSong, MusicRequest};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};
use serde::Deserialize;

use super::media_tool::{self, failure, workspace_path};
use crate::auth::AuthStore;

/// The tool's name, as the model calls it and permission rules name it.
pub(crate) const GENERATE_MUSIC_TOOL: &str = "generate_music";

/// Input shape for the `generate_music` tool.
#[derive(Debug, Deserialize)]
struct GenerateMusicInput {
    prompt: String,
    #[serde(default)]
    lyrics: Option<String>,
    #[serde(default)]
    instrumental: Option<bool>,
    #[serde(default)]
    seed: Option<u64>,
    #[serde(default)]
    path: Option<String>,
}

/// Make one song and hand it back as the result's media.
#[derive(Debug)]
pub(crate) struct GenerateMusicTool {
    endpoint: MediaEndpoint,
    model: String,
    timeout: Duration,
}

impl GenerateMusicTool {
    /// The tool `config`'s `[tools.generate_music]` describes, with keys
    /// looked up in the environment and then in `store`.
    ///
    /// # Errors
    ///
    /// Why the table does not describe a usable tool, ready to follow
    /// `[tools.generate_music]` in a message.
    pub(super) fn from_config(config: &Config, store: &AuthStore) -> Result<Self, String> {
        let cfg = &config.tools.generate_music;
        Ok(Self {
            endpoint: media_tool::endpoint(GENERATE_MUSIC_TOOL, cfg, config, store)?,
            model: media_tool::model(cfg)?,
            timeout: media_tool::timeout(cfg)?,
        })
    }

    /// The result text for `song`: what was made, where it was saved or
    /// why not, and what the server said. A song that could not be read
    /// says so and gives its link, so it does not read as a file in hand.
    fn text(
        cx: &ToolContext<'_>,
        song: &GeneratedSong,
        saved: Option<&Result<PathBuf, String>>,
    ) -> String {
        let mut text = match &song.audio {
            Ok((bytes, mime)) => format!(
                "Made a song ({mime}, {}). It comes with this result, and the user can play \
                 it.",
                kage_core::image::human_bytes(bytes.len())
            ),
            Err(why) => {
                let mut text = format!("Made a song, but it did not come back: {why}.");
                if let Some(link) = &song.link {
                    let _ = write!(
                        text,
                        " The server keeps it at {link}, which kage did not fetch; give the \
                         user the link."
                    );
                }
                text
            }
        };
        match saved {
            Some(Ok(saved)) => {
                let shown = media_tool::shown_path(cx.workdir(), saved);
                let _ = write!(text, " Saved as {}.", shown.display());
            }
            Some(Err(why)) => {
                let _ = write!(text, " It was not saved: {why}.");
            }
            None => {}
        }
        if let Some(seed) = song.seed {
            let _ = write!(
                text,
                " Seed {seed}: the same prompt, lyrics and seed make it again."
            );
        }
        if let Some(note) = &song.note {
            let _ = write!(text, "\nServer note: {note}");
        }
        text
    }
}

impl Tool for GenerateMusicTool {
    fn name(&self) -> &'static str {
        GENERATE_MUSIC_TOOL
    }

    fn description(&self) -> &'static str {
        "Make one song from a style prompt and optional lyrics through the configured music \
         server. The song comes back with this result and stays in the session, where the \
         user can play or save it. A song can take a minute or more. Name a `path` only \
         when the user wants the file in the workspace."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The style of the song: genre, mood, instruments, \
                                    vocals.",
                },
                "lyrics": {
                    "type": "string",
                    "description": "The words to sing, with section tags such as [verse] \
                                    and [chorus]. Leave it out to let the server choose.",
                },
                "instrumental": {
                    "type": "boolean",
                    "description": "true for a song with no vocals; the lyrics are then \
                                    ignored.",
                },
                "seed": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "The same prompt, lyrics and seed make the same song. \
                                    Leave it out for a new one.",
                },
                "path": {
                    "type": "string",
                    "description": "Also save the song here, relative to the workspace, \
                                    such as audio/theme.mp3. The extension follows the \
                                    song's type, and an existing file is never replaced.",
                },
            },
            "required": ["prompt"],
        })
    }

    /// A call may save a file in the workspace, so plan mode and a
    /// read-only session refuse it as they refuse `write`.
    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn execution_mode(&self) -> Option<ExecMode> {
        Some(ExecMode::Parallel)
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: GenerateMusicInput = serde_json::from_value(input)?;
        let prompt = input.prompt.trim();
        if prompt.is_empty() {
            return Err(ToolError::InvalidInput("prompt is empty".to_owned()));
        }
        let lyrics = input
            .lyrics
            .as_deref()
            .map(str::trim)
            .filter(|lyrics| !lyrics.is_empty());
        // Checked before the call, so a path outside the workspace
        // costs no request.
        let path = input
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty());
        if let Some(path) = path {
            workspace_path(cx.workdir(), path)?;
        }
        if cx.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        cx.update(ToolUpdate {
            content: format!("making a song with {}", self.model),
            structured: None,
        });
        let request = MusicRequest {
            model: self.model.clone(),
            prompt: prompt.to_owned(),
            lyrics: lyrics.map(str::to_owned),
            instrumental: input.instrumental,
            seed: input.seed,
        };
        let song = match music::generate(&self.endpoint, &request, self.timeout, cx.cancel_flag()) {
            Ok(song) => song,
            Err(MediaError {
                error: ProviderError::Cancelled,
                ..
            }) => return Err(ToolError::Cancelled),
            Err(error) => {
                return Ok(failure(
                    &error,
                    self.timeout,
                    "Song generation failed",
                    "the prompt or lyrics",
                ));
            }
        };
        // A failed save keeps the song in this result: it was made and
        // paid for, and the session holds it even when no file does.
        let saved = match (&song.audio, path) {
            (Ok((bytes, mime)), Some(path)) => Some(media_tool::save_in_workspace(
                cx.workdir(),
                path,
                mime,
                bytes,
            )),
            _ => None,
        };
        let text = Self::text(cx, &song, saved.as_ref());
        let media = match &song.audio {
            Ok((bytes, mime)) => vec![ToolMedia {
                mime: (*mime).to_owned(),
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            }],
            Err(_) => Vec::new(),
        };
        Ok(ToolOutput {
            is_error: false,
            text,
            structured: Some(serde_json::json!({
                "model": self.model,
                "seed": song.seed,
                "note": song.note,
                "link": song.link,
                "path": saved.and_then(Result::ok).map(|path| path.display().to_string()),
            })),
            terminate: false,
            media,
        })
    }
}

#[cfg(test)]
mod tests {
    use kage_core::CancelFlag;
    use kage_core::config::MediaToolConfig;

    use super::super::media_tool::WithMediaTools;
    use super::super::media_tool::test_server::{self, Seen};
    use super::*;

    /// An ID3 tag's head, enough for the sniffer to call it MP3.
    const MP3: &[u8] = b"ID3\x04\x00\x00\x00\x00\x00\x00song";

    /// Answers every request on a fresh port with one base64 song.
    fn music_server() -> (String, Seen) {
        let body = serde_json::json!({
            "data": [{
                "b64_json": base64::engine::general_purpose::STANDARD.encode(MP3),
                "seed": 42,
                "notice": "music-max was busy; music-xhigh served",
            }],
        });
        test_server::serve("application/json", body.to_string().into_bytes())
    }

    fn tool(base_url: &str) -> GenerateMusicTool {
        GenerateMusicTool {
            endpoint: MediaEndpoint {
                base_url: base_url.to_owned(),
                ..MediaEndpoint::default()
            },
            model: "music".to_owned(),
            timeout: Duration::from_secs(10),
        }
    }

    #[test]
    fn the_song_rides_the_result_and_no_file_is_written() {
        let (url, seen) = music_server();
        let workdir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let output = tool(&url)
            .execute(
                serde_json::json!({
                    "prompt": "an upbeat synth-pop song",
                    "lyrics": "[verse]\nCity lights",
                    "instrumental": false,
                    "seed": 7,
                }),
                &cx,
            )
            .unwrap();
        assert!(!output.is_error, "{}", output.text);
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].mime, "audio/mpeg");
        assert!(
            output.text.starts_with("Made a song (audio/mpeg, "),
            "{}",
            output.text
        );
        assert!(output.text.contains("Seed 42"), "{}", output.text);
        assert!(
            output
                .text
                .contains("Server note: music-max was busy; music-xhigh served"),
            "{}",
            output.text
        );
        let structured = output.structured.unwrap();
        assert_eq!(structured["seed"], 42);
        assert!(structured["path"].is_null());
        assert_eq!(std::fs::read_dir(workdir.path()).unwrap().count(), 0);
        assert_eq!(seen.count(), 1);
        let sent = seen.json_body(0);
        assert_eq!(sent["prompt"], "an upbeat synth-pop song");
        assert_eq!(sent["lyrics"], "[verse]\nCity lights");
        assert_eq!(sent["instrumental"], false);
        assert_eq!(sent["seed"], 7);
    }

    #[test]
    fn a_path_saves_the_song_there() {
        let (url, _seen) = music_server();
        let workdir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let output = tool(&url)
            .execute(
                serde_json::json!({ "prompt": "a lullaby", "path": "audio/theme.mp3" }),
                &cx,
            )
            .unwrap();
        assert!(
            output.text.contains(&format!(
                "Saved as {}.",
                std::path::Path::new("audio").join("theme.mp3").display()
            )),
            "{}",
            output.text
        );
        assert_eq!(
            std::fs::read(workdir.path().join("audio/theme.mp3")).unwrap(),
            MP3
        );
    }

    #[test]
    fn a_song_that_did_not_come_back_gives_its_link_and_is_not_an_error() {
        let body = serde_json::json!({"data": [{"url": "https://elsewhere.test/f/a.mp3"}]});
        let (url, seen) = test_server::serve("application/json", body.to_string().into_bytes());
        let workdir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let output = tool(&url)
            .execute(
                serde_json::json!({ "prompt": "a lullaby", "path": "audio/theme.mp3" }),
                &cx,
            )
            .unwrap();
        assert!(!output.is_error);
        assert!(output.media.is_empty());
        assert!(
            output.text.contains("did not come back")
                && output.text.contains("https://elsewhere.test/f/a.mp3")
                && output.text.contains("did not fetch"),
            "{}",
            output.text
        );
        assert!(!workdir.path().join("audio").exists());
        assert_eq!(seen.count(), 1);
    }

    #[test]
    fn a_bad_call_costs_no_request() {
        let (url, seen) = music_server();
        let workdir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        for input in [
            serde_json::json!({ "prompt": "  " }),
            serde_json::json!({ "prompt": "a lullaby", "path": "../outside.mp3" }),
            serde_json::json!({ "prompt": "a lullaby", "seed": -1 }),
        ] {
            assert!(tool(&url).execute(input, &cx).is_err());
        }
        assert_eq!(seen.count(), 0);
    }

    #[test]
    fn only_generate_music_configured_registers_generate_music_alone() {
        let mut config = Config::default();
        config.tools.generate_music = MediaToolConfig {
            base_url: Some("https://music.example/v1".into()),
            model: Some("music".into()),
            ..MediaToolConfig::default()
        };
        let tools = kage_tools::builtin_registry().with_media_tools(&config);
        assert!(tools.get(GENERATE_MUSIC_TOOL).is_some());
        for other in [
            super::super::GENERATE_IMAGE_TOOL,
            super::super::EDIT_IMAGE_TOOL,
            super::super::GENERATE_SPEECH_TOOL,
        ] {
            assert!(tools.get(other).is_none(), "{other}");
        }
    }
}
