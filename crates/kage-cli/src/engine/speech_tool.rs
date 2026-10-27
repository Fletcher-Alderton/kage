//! The `generate_speech` tool: one text read aloud, from the speech
//! endpoint `[tools.generate_speech]` names.
//!
//! The tool is registered only when that table resolves to an endpoint
//! and a model, so a config without one advertises the same tools as
//! before. Its clip is the result's audio media (see [`ToolMedia`])
//! and stays in the session, where `/media` and the desktop play and
//! save it. A call may also name a `path` in the workspace to save it
//! there at once.

use std::fmt::Write as _;
use std::time::Duration;

use base64::Engine as _;
use kage_core::config::Config;
use kage_core::{Risk, ToolMedia, ToolOutput, ToolUpdate};
use kage_provider::ProviderError;
use kage_provider::media::{MediaEndpoint, MediaError};
use kage_provider::speech::{self, SpeechRequest};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};
use serde::Deserialize;

use super::media_tool::{self, failure, workspace_path};
use crate::auth::AuthStore;

/// The tool's name, as the model calls it and permission rules name it.
pub(crate) const GENERATE_SPEECH_TOOL: &str = "generate_speech";

/// Input shape for the `generate_speech` tool.
#[derive(Debug, Deserialize)]
struct GenerateSpeechInput {
    text: String,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

/// Read one text aloud and hand the clip back as the result's media.
#[derive(Debug)]
pub(crate) struct GenerateSpeechTool {
    endpoint: MediaEndpoint,
    model: String,
    timeout: Duration,
    /// The voice asked when a call names none.
    voice: Option<String>,
}

impl GenerateSpeechTool {
    /// The tool `config`'s `[tools.generate_speech]` describes, with keys
    /// looked up in the environment and then in `store`.
    ///
    /// # Errors
    ///
    /// Why the table does not describe a usable tool, ready to follow
    /// `[tools.generate_speech]` in a message.
    pub(super) fn from_config(config: &Config, store: &AuthStore) -> Result<Self, String> {
        let cfg = &config.tools.generate_speech;
        let endpoint = media_tool::endpoint(GENERATE_SPEECH_TOOL, &cfg.endpoint, config, store)?;
        let model = media_tool::model(&cfg.endpoint)?;
        let timeout = media_tool::timeout(&cfg.endpoint)?;
        Ok(Self {
            endpoint,
            model,
            timeout,
            voice: cfg
                .voice
                .as_deref()
                .map(str::trim)
                .filter(|voice| !voice.is_empty())
                .map(str::to_owned),
        })
    }
}

impl Tool for GenerateSpeechTool {
    fn name(&self) -> &'static str {
        GENERATE_SPEECH_TOOL
    }

    fn description(&self) -> &'static str {
        "Read a text aloud through the configured speech server. The clip comes back with \
         this result and plays where the client plays audio; it stays in the session, and \
         the user can play or save it from there. Name a `path` only when the user wants \
         the file in the workspace."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "What to read aloud, written as it should be spoken. \
                                    Keep it short: servers cap the length.",
                },
                "voice": {
                    "type": "string",
                    "description": "A voice the server names, such as alloy. Leave it out \
                                    for the configured voice.",
                },
                "language": {
                    "type": "string",
                    "description": "The text's language as an ISO 639-1 code, such as en \
                                    or es. A hint some servers take; leave it out for \
                                    English.",
                },
                "path": {
                    "type": "string",
                    "description": "Also save the clip here, relative to the workspace, \
                                    such as audio/intro.mp3. The extension follows the \
                                    clip's type, and an existing file is never replaced.",
                },
            },
            "required": ["text"],
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
        let input: GenerateSpeechInput = serde_json::from_value(input)?;
        let text = input.text.trim();
        if text.is_empty() {
            return Err(ToolError::InvalidInput("text is empty".to_owned()));
        }
        let voice = input
            .voice
            .as_deref()
            .map(str::trim)
            .filter(|voice| !voice.is_empty());
        let language = input
            .language
            .as_deref()
            .map(str::trim)
            .filter(|language| !language.is_empty());
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
        let count = text.chars().count();
        cx.update(ToolUpdate {
            content: format!("reading {count} characters aloud with {}", self.model),
            structured: None,
        });
        let request = SpeechRequest {
            model: self.model.clone(),
            input: text.to_owned(),
            voice: voice.map(str::to_owned).or_else(|| self.voice.clone()),
            language: language.map(str::to_owned),
        };
        let clip = match speech::speak(&self.endpoint, &request, self.timeout, cx.cancel_flag()) {
            Ok(clip) => clip,
            Err(MediaError {
                error: ProviderError::Cancelled,
                ..
            }) => return Err(ToolError::Cancelled),
            Err(error) => {
                return Ok(failure(
                    &error,
                    self.timeout,
                    "Speech failed",
                    "the text, voice or language",
                ));
            }
        };
        // A failed save keeps the clip in this result: it was made and
        // paid for, and the session holds it even when no file does.
        let saved = path
            .map(|path| media_tool::save_in_workspace(cx.workdir(), path, clip.mime, &clip.bytes));
        let mut text = format!(
            "Read {count} characters aloud ({}, {}). The clip plays where the client plays \
             audio, and it is kept in the session.",
            clip.mime,
            kage_core::image::human_bytes(clip.bytes.len()),
        );
        let saved = match saved {
            Some(Ok(saved)) => {
                let shown = media_tool::shown_path(cx.workdir(), &saved);
                let _ = write!(text, " Saved as {}.", shown.display());
                Some(saved)
            }
            Some(Err(why)) => {
                let _ = write!(text, " It was not saved: {why}.");
                None
            }
            None => None,
        };
        Ok(ToolOutput {
            is_error: false,
            text,
            structured: Some(serde_json::json!({
                "model": self.model,
                "voice": request.voice,
                "path": saved.as_ref().map(|path| path.display().to_string()),
            })),
            terminate: false,
            media: vec![ToolMedia {
                mime: clip.mime.to_owned(),
                data: base64::engine::general_purpose::STANDARD.encode(&clip.bytes),
            }],
        })
    }
}

#[cfg(test)]
mod tests {

    use kage_core::CancelFlag;
    use kage_core::config::{MediaToolConfig, SpeechToolConfig};

    use super::super::edit_image_tool::EDIT_IMAGE_TOOL;
    use super::super::image_tool::GENERATE_IMAGE_TOOL;
    use super::super::media_tool::test_server::{self, Seen};
    use super::super::media_tool::{DEFAULT_TIMEOUT, WithMediaTools};
    use super::*;

    /// A small MP3: an `ID3` tag followed by an MPEG frame header.
    const MP3: &[u8] = &[
        b'I', b'D', b'3', 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFB, 0x90, 0x64,
    ];

    /// Answers every request on a fresh port with one MP3.
    fn speech_server() -> (String, Seen) {
        test_server::serve("application/octet-stream", MP3.to_vec())
    }

    fn tool(base_url: &str) -> GenerateSpeechTool {
        GenerateSpeechTool {
            endpoint: MediaEndpoint {
                base_url: base_url.to_owned(),
                ..MediaEndpoint::default()
            },
            model: "speech".to_owned(),
            timeout: Duration::from_secs(10),
            voice: None,
        }
    }

    fn config(cfg: SpeechToolConfig) -> Config {
        let mut config = Config::default();
        config.tools.generate_speech = cfg;
        config
    }

    #[test]
    fn the_clip_rides_the_result_and_no_file_is_written() {
        let (url, seen) = speech_server();
        let workdir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let output = tool(&url)
            .execute(serde_json::json!({ "text": "a red kite" }), &cx)
            .unwrap();
        assert!(!output.is_error, "{}", output.text);
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].mime, "audio/mpeg");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&output.media[0].data)
                .unwrap(),
            MP3
        );
        assert!(
            output
                .text
                .starts_with("Read 10 characters aloud (audio/mpeg, 14 B)."),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("it is kept in the session"),
            "{}",
            output.text
        );
        assert!(!output.text.contains("Saved"), "{}", output.text);
        assert!(output.structured.unwrap()["path"].is_null());
        assert_eq!(
            std::fs::read_dir(workdir.path()).unwrap().count(),
            0,
            "nothing is written without a path"
        );
        assert_eq!(seen.count(), 1);
        assert_eq!(seen.json_body(0)["input"], "a red kite");
    }

    #[test]
    fn a_path_saves_the_clip_there_with_the_mp3_extension() {
        for (path, saved_as) in [
            ("audio/intro.mp3", "audio/intro.mp3"),
            ("audio/intro.png", "audio/intro.mp3"),
            ("audio/intro", "audio/intro.mp3"),
        ] {
            let (url, seen) = speech_server();
            let workdir = tempfile::tempdir().unwrap();
            let cancel = CancelFlag::new();
            let cx = ToolContext::new(workdir.path(), &cancel);
            let output = tool(&url)
                .execute(serde_json::json!({ "text": "hello", "path": path }), &cx)
                .unwrap();
            assert!(!output.is_error, "{path}: {}", output.text);
            assert_eq!(
                std::fs::read(workdir.path().join(saved_as)).unwrap(),
                MP3,
                "{path}"
            );
            assert!(
                output.text.contains(&format!("Saved as {saved_as}.")),
                "{path}: {}",
                output.text
            );
            assert_eq!(seen.count(), 1, "{path}");
            if path != saved_as {
                assert!(!workdir.path().join(path).exists(), "{path}");
            }
        }
    }

    #[test]
    fn voice_and_language_follow_the_table_and_the_call() {
        let (url, seen) = speech_server();
        let workdir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let voiced = GenerateSpeechTool {
            voice: Some("alloy".to_owned()),
            ..tool(&url)
        };
        let first = voiced
            .execute(serde_json::json!({ "text": "one" }), &cx)
            .unwrap();
        assert_eq!(seen.json_body(0)["voice"], "alloy");
        assert!(seen.json_body(0).get("language").is_none());
        assert_eq!(first.structured.unwrap()["voice"], "alloy");

        voiced
            .execute(serde_json::json!({ "text": "two", "voice": " nova " }), &cx)
            .unwrap();
        assert_eq!(seen.json_body(1)["voice"], "nova", "the call's voice wins");

        let third = tool(&url)
            .execute(serde_json::json!({ "text": "three" }), &cx)
            .unwrap();
        assert!(seen.json_body(2).get("voice").is_none(), "no voice is sent");
        assert!(third.structured.unwrap()["voice"].is_null());

        tool(&url)
            .execute(serde_json::json!({ "text": "four", "language": "es" }), &cx)
            .unwrap();
        assert_eq!(seen.json_body(3)["language"], "es");
        assert_eq!(seen.count(), 4);
    }

    #[test]
    fn a_refused_call_costs_no_request() {
        let (url, seen) = speech_server();
        let workdir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        for input in [
            serde_json::json!({ "text": "  " }),
            serde_json::json!({ "text": "hello", "path": "../out.mp3" }),
            serde_json::json!({ "text": "hello", "path": "." }),
            serde_json::json!({ "text": "hello", "path": "./" }),
        ] {
            let err = tool(&url).execute(input, &cx).unwrap_err();
            assert!(
                matches!(err, ToolError::InvalidInput(_) | ToolError::Path { .. }),
                "{err:?}"
            );
        }
        assert_eq!(seen.count(), 0, "no request is made for a refused call");
        let beside = workdir.path().with_extension("mp3");
        assert!(!beside.exists(), "{} was written", beside.display());
    }

    #[test]
    fn a_clip_that_cannot_be_saved_still_rides_the_result() {
        let (url, seen) = speech_server();
        let workdir = tempfile::tempdir().unwrap();
        std::fs::write(workdir.path().join("audio"), b"a file, not a directory").unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let output = tool(&url)
            .execute(
                serde_json::json!({ "text": "hello", "path": "audio/hello.mp3" }),
                &cx,
            )
            .unwrap();
        assert!(!output.is_error);
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].mime, "audio/mpeg");
        assert!(
            output.text.contains("It was not saved: "),
            "{}",
            output.text
        );
        assert!(output.structured.unwrap()["path"].is_null());
        assert_eq!(seen.count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_path_through_a_symlink_out_of_the_workspace_costs_no_request() {
        let (url, seen) = speech_server();
        let outside = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), workdir.path().join("audio")).unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir.path(), &cancel);
        let err = tool(&url)
            .execute(
                serde_json::json!({ "text": "hello", "path": "audio/hello.mp3" }),
                &cx,
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }), "{err:?}");
        assert_eq!(seen.count(), 0);
    }

    #[test]
    fn a_table_resolves_or_says_why_not() {
        let store = AuthStore::empty();
        let from = |cfg| GenerateSpeechTool::from_config(&config(cfg), &store);

        let named = from(SpeechToolConfig {
            endpoint: MediaToolConfig {
                base_url: Some("https://speech.example/v1".into()),
                model: Some("speech".into()),
                ..MediaToolConfig::default()
            },
            voice: Some("alloy".into()),
        })
        .unwrap();
        assert_eq!(named.endpoint.base_url, "https://speech.example/v1");
        assert!(named.endpoint.api_key.is_empty());
        assert_eq!(named.timeout, DEFAULT_TIMEOUT);
        assert_eq!(named.voice.as_deref(), Some("alloy"));

        let cases: [(MediaToolConfig, &str); 4] = [
            (
                MediaToolConfig {
                    base_url: Some("https://speech.example/v1".into()),
                    ..MediaToolConfig::default()
                },
                "names no `model`",
            ),
            (
                MediaToolConfig {
                    model: Some("speech".into()),
                    ..MediaToolConfig::default()
                },
                "names neither",
            ),
            (
                MediaToolConfig {
                    provider: Some("anthropic".into()),
                    model: Some("speech".into()),
                    ..MediaToolConfig::default()
                },
                "cannot serve this tool: it calls an OpenAI API",
            ),
            (
                MediaToolConfig {
                    base_url: Some("https://speech.example/v1".into()),
                    model: Some("speech".into()),
                    timeout_secs: Some(0),
                    ..MediaToolConfig::default()
                },
                "timeout_secs",
            ),
        ];
        for (endpoint, want) in cases {
            let err = from(SpeechToolConfig {
                endpoint,
                voice: None,
            })
            .unwrap_err();
            assert!(err.contains(want), "{err}");
        }

        let blank = from(SpeechToolConfig {
            endpoint: MediaToolConfig {
                base_url: Some("https://speech.example/v1".into()),
                model: Some("speech".into()),
                ..MediaToolConfig::default()
            },
            voice: Some("  ".into()),
        })
        .unwrap();
        assert_eq!(blank.voice, None, "a blank voice is no voice");
    }

    #[test]
    fn only_generate_speech_configured_registers_generate_speech_alone() {
        let mut config = Config::default();
        config.tools.generate_speech = SpeechToolConfig {
            endpoint: MediaToolConfig {
                base_url: Some("https://speech.example/v1".into()),
                model: Some("speech".into()),
                ..MediaToolConfig::default()
            },
            voice: None,
        };
        let tools = kage_tools::builtin_registry().with_media_tools(&config);
        assert!(tools.get(GENERATE_SPEECH_TOOL).is_some());
        assert!(tools.get(GENERATE_IMAGE_TOOL).is_none());
        assert!(tools.get(EDIT_IMAGE_TOOL).is_none());
    }
}
