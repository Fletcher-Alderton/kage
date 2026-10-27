//! Agent conversation message and content block types.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Unique identifier for a message in a session.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(pub Ulid);

impl MessageId {
    /// Generate a fresh message id.
    #[must_use]
    pub fn new() -> Self {
        Self(Ulid::generate())
    }
}

impl Default for MessageId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifier for a single tool invocation issued by an assistant.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallId(pub String);

impl ToolCallId {
    /// Wrap an arbitrary id string emitted by a provider.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Source of a message in the conversation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Sent by the human end-user.
    User,
    /// Sent by the model.
    Assistant,
    /// Carries the result of a tool execution back to the model.
    ToolResult,
    /// System / instruction prompt anchored at the conversation root.
    System,
}

/// One block of content within a message.
///
/// Multiple blocks per message let an assistant turn carry, for example,
/// thinking + text + several tool calls without ambiguity in ordering.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Plain user-visible text.
    Text {
        /// The text body.
        text: String,
    },
    /// Hidden chain-of-thought emitted by the model.
    Thinking {
        /// The thinking body. Empty when the provider sent the reasoning
        /// encrypted only.
        text: String,
        /// Opaque provider data needed to send the block back natively.
        /// Absent for plain thinking and in sessions written before kage
        /// kept it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<ThinkingSignature>,
        /// How long the model spent on the block, in milliseconds. For
        /// display only and never sent to a provider. Absent for
        /// untimed blocks and in sessions written before kage kept it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// An image attached to the message.
    Image {
        /// Where the image bytes live: a URL, inline base64, or the
        /// session blob store.
        source: MediaSource,
        /// MIME type, for example `image/png`.
        mime: String,
    },
    /// An audio clip attached to the message.
    Audio {
        /// Where the clip's bytes live: a URL, inline base64, or the
        /// session blob store.
        source: MediaSource,
        /// MIME type, for example `audio/mpeg`.
        mime: String,
    },
    /// A request from the model to invoke a tool.
    ToolCall {
        /// Provider-issued correlation id.
        id: ToolCallId,
        /// Tool name as registered in the tool registry.
        name: String,
        /// Tool input arguments as JSON.
        input: serde_json::Value,
    },
    /// The result of a previously-issued tool call, fed back to the model.
    ToolResultBlock {
        /// The id of the tool call this result corresponds to.
        call_id: ToolCallId,
        /// Stringified output, the text the model sees. A tool's
        /// structured detail never enters history; it rides the
        /// [`crate::event::ToolOutput`] event for host rendering.
        output: String,
        /// Whether the tool failed.
        is_error: bool,
    },
    /// Plugin-provided content type, opaque to the core loop.
    Custom {
        /// Plugin-defined kind tag, namespaced like `plugin:tps`.
        kind: String,
        /// Arbitrary JSON payload.
        data: serde_json::Value,
    },
}

impl Content {
    /// The text an image or audio clip the model produced stands as
    /// when its own history goes back to a provider that is not sent
    /// the bytes.
    #[must_use]
    pub fn produced_media_note(mime: &str) -> String {
        if mime.starts_with("audio/") {
            format!("[you produced audio here: {mime}]")
        } else {
            format!("[you produced an image here: {mime}]")
        }
    }

    /// The text an audio clip stands as in a user or tool-result
    /// position: no model is sent audio yet, so the bytes stay behind.
    #[must_use]
    pub fn audio_note(mime: &str) -> String {
        format!("[an audio clip ({mime}) was here; it is not sent to the model]")
    }

    /// Thinking `text` framed as `<thinking>` text, the form kage sends
    /// to a provider that cannot take a thinking block natively. A
    /// literal close tag in the text is escaped so the frame cannot end
    /// early. `None` for blank thinking.
    #[must_use]
    pub fn flattened_thinking(text: &str) -> Option<String> {
        if text.trim().is_empty() {
            return None;
        }
        Some(format!(
            "<thinking>\n{}\n</thinking>",
            text.replace("</thinking", "<\\/thinking")
        ))
    }
}

/// Opaque data a provider attached to a thinking block: an Anthropic
/// signature or redacted thinking, a Gemini thought signature, `OpenAI`
/// encrypted reasoning, or `OpenRouter` reasoning details. Sending it
/// back lets the provider verify the block and continue its reasoning.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ThinkingSignature {
    /// Model that produced the block. Providers send the data back only
    /// to this model.
    pub model: String,
    /// The provider's opaque value.
    pub data: String,
    /// Whether `data` is a whole Anthropic redacted thinking block
    /// rather than the signature of the text.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
}

/// Where the bytes of an image (or other media) live.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaSource {
    /// Remote URL.
    Url {
        /// HTTP(S) URL.
        url: String,
    },
    /// Inline base64-encoded bytes.
    Base64 {
        /// Base64 payload, no data: prefix.
        data: String,
    },
    /// Bytes kept in the session blob store, by content hash. A session
    /// file holds this instead of large inline bytes; reading the
    /// session turns it back into [`Self::Base64`]. Left as is only
    /// when the blob is missing, so consumers treat it as unavailable.
    Blob {
        /// The bytes' sha256, lowercase hex.
        sha256: String,
        /// The byte count.
        size: u64,
    },
}

impl MediaSource {
    /// The text that stands in for media whose bytes are gone: a
    /// [`Self::Blob`] the blob store no longer holds. `None` for a
    /// source that still names its bytes.
    #[must_use]
    pub fn missing_note(&self, mime: &str) -> Option<String> {
        match self {
            Self::Blob { size, .. } => Some(format!("[{mime}, {size} bytes, no longer stored]")),
            Self::Url { .. } | Self::Base64 { .. } => None,
        }
    }
}

/// One tool result of a tool-result message, with the media the tool
/// produced, which follow its result block in the message.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResultParts<'a> {
    /// The call the result answers.
    pub call_id: &'a ToolCallId,
    /// The result text.
    pub output: &'a str,
    /// Whether the tool failed.
    pub is_error: bool,
    /// The images the tool produced, as `(source, mime)`.
    pub media: Vec<(&'a MediaSource, &'a str)>,
    /// The mimes of the audio clips the tool produced.
    pub audio: Vec<&'a str>,
}

impl ToolResultParts<'_> {
    /// The result text, with a note for each image and clip after it,
    /// for a provider that cannot take media in a tool result.
    #[must_use]
    pub fn text_with_media_notes(&self) -> String {
        use std::fmt::Write as _;
        let mut text = self.output.to_owned();
        for (_, mime) in &self.media {
            let _ = write!(
                text,
                "\n[the tool produced an image ({mime}), not shown here]"
            );
        }
        for mime in &self.audio {
            let _ = write!(text, "\n{}", Content::audio_note(mime));
        }
        text
    }
}

/// The tool results in `blocks`, each with the images and clips that
/// follow it.
#[must_use]
pub fn tool_results(blocks: &[Content]) -> Vec<ToolResultParts<'_>> {
    let mut out: Vec<ToolResultParts<'_>> = Vec::new();
    for block in blocks {
        match block {
            Content::ToolResultBlock {
                call_id,
                output,
                is_error,
            } => out.push(ToolResultParts {
                call_id,
                output,
                is_error: *is_error,
                media: Vec::new(),
                audio: Vec::new(),
            }),
            Content::Image { source, mime } => {
                if let Some(last) = out.last_mut() {
                    last.media.push((source, mime));
                }
            }
            Content::Audio { mime, .. } => {
                if let Some(last) = out.last_mut() {
                    last.audio.push(mime);
                }
            }
            _ => {}
        }
    }
    out
}

/// One turn in the conversation.
///
/// A `Message` is the persisted unit of conversation history. Streaming
/// deltas are reassembled into a single `Message` before being appended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who or what produced this message.
    pub role: Role,
    /// Ordered content blocks.
    pub content: Vec<Content>,
    /// Stable identifier within a session.
    pub id: MessageId,
    /// Parent message id, used for branching and tree navigation.
    pub parent: Option<MessageId>,
    /// UTC timestamp at message creation.
    pub ts: DateTime<Utc>,
}

impl Message {
    /// Construct a message with a fresh id and the current timestamp.
    #[must_use]
    pub fn new(role: Role, content: Vec<Content>, parent: Option<MessageId>) -> Self {
        Self {
            role,
            content,
            id: MessageId::new(),
            parent,
            ts: Utc::now(),
        }
    }
}

/// Framing wrapper for the synthetic summary message that replaces the
/// drained history. The labelled block keeps providers seeing a clear
/// context summary rather than a rogue assistant turn. Exposed so the
/// resume path can detect the same framing in replayed history and route
/// it back through the compaction widget instead of rendering it as a
/// plain assistant block.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
/// Closing framing for the synthetic compaction summary message. See
/// [`COMPACTION_SUMMARY_PREFIX`].
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

const SHELL_PREFIX: &str = "[shell] ran `";
const SHELL_MIDDLE: &str = "` in the session working directory; exit code ";

/// A user `!` command and its output, shared with the model as the text
/// of a user message. Exposed so a replayed history can show the message
/// as the shell block it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellRun {
    /// Command line that ran.
    pub command: String,
    /// Exit code, or `None` when a signal ended the command.
    pub exit_code: Option<i32>,
    /// Combined stdout and stderr.
    pub output: String,
}

impl ShellRun {
    /// The text of the user message that carries the run.
    #[must_use]
    pub fn to_text(&self) -> String {
        let exit = self
            .exit_code
            .map_or_else(|| "signal".to_owned(), |c| c.to_string());
        format!(
            "{SHELL_PREFIX}{}{SHELL_MIDDLE}{exit}:\n{}",
            self.command,
            self.output.trim_end()
        )
    }

    /// Read a run back from text [`Self::to_text`] wrote.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix(SHELL_PREFIX)?;
        let (command, rest) = rest.split_once(SHELL_MIDDLE)?;
        let (exit, output) = rest.split_once(":\n")?;
        let exit_code = match exit {
            "signal" => None,
            code => Some(code.parse().ok()?),
        };
        Some(Self {
            command: command.to_owned(),
            exit_code,
            output: output.to_owned(),
        })
    }
}

/// The most bytes of output one tool result keeps in a conversation,
/// about 25k tokens. Every turn resends the whole history, so one
/// unbounded result (a grep over minified bundles, a read of a
/// generated file) costs its size on every request, in memory and in
/// the session file.
pub const MAX_TOOL_RESULT_BYTES: usize = 100_000;

/// Whether `output` is an `agent` call's `<agent>` element or a `swarm`
/// call's aggregate of `<swarm>` elements. Their tools bound their size
/// themselves, and resuming a session reads every child's header line
/// back out of them, so they are never cut. Both shapes are matched at
/// a line start after leading padding, so a padded or blank-prefixed
/// result is still detected.
#[must_use]
pub fn is_agent_result(output: &str) -> bool {
    let trimmed = output.trim_start();
    trimmed.starts_with("<agent ") || trimmed.contains("\n<swarm description=\"")
}

/// `text` cut to at most [`MAX_TOOL_RESULT_BYTES`]: the first two
/// thirds and the last third, joined by a line saying how much was left
/// out. Text within the limit comes back unchanged.
#[must_use]
pub fn cap_tool_result(text: String) -> String {
    if text.len() <= MAX_TOOL_RESULT_BYTES {
        return text;
    }
    let boundary = |mut at: usize| {
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    // Room is left for the note, so a capped result is within the limit
    // and capping it again changes nothing.
    let budget = MAX_TOOL_RESULT_BYTES - 128;
    let head = boundary(budget * 2 / 3);
    let tail = boundary(text.len() - budget / 3);
    let omitted = tail - head;
    format!(
        "{}\n\n[... {omitted} bytes of output omitted; narrow the request to see them ...]\n\n{}",
        &text[..head],
        &text[tail..]
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn tool_results_over_the_limit_keep_their_head_and_tail() {
        let short = "fine".to_owned();
        assert_eq!(super::cap_tool_result(short.clone()), short);
        let long = format!(
            "{}{}",
            "a".repeat(super::MAX_TOOL_RESULT_BYTES),
            "\u{e9}".repeat(50_000)
        );
        let capped = super::cap_tool_result(long.clone());
        assert!(
            capped.len() < super::MAX_TOOL_RESULT_BYTES + 200,
            "{}",
            capped.len()
        );
        assert!(capped.starts_with("aaa") && capped.ends_with("\u{e9}"));
        assert!(capped.contains("bytes of output omitted"));
        assert!(capped.len() <= super::MAX_TOOL_RESULT_BYTES);
        assert_eq!(
            super::cap_tool_result(capped.clone()),
            capped,
            "capping is idempotent"
        );
    }

    use super::*;

    #[test]
    fn only_agent_and_swarm_results_count_as_agent_results() {
        assert!(is_agent_result(
            "<agent name=\"general\" session=\"s\" state=\"completed\">\nhi\n</agent>"
        ));
        assert!(is_agent_result(
            "completed: 1, failed: 0, cancelled: 0\n<swarm description=\"d\" item=\"a\">"
        ));
        assert!(!is_agent_result("exit code 0:\nsome ordinary text"));
    }

    #[test]
    fn padded_agent_and_swarm_results_are_still_detected() {
        assert!(is_agent_result(
            "\n  <agent name=\"general\" session=\"s\" state=\"completed\">\nhi\n</agent>"
        ));
        assert!(is_agent_result(
            "  completed: 1, failed: 0, cancelled: 0\n<swarm description=\"d\" item=\"a\">"
        ));
        // A marker that does not start a line is ordinary output.
        assert!(!is_agent_result(
            "the log said <swarm description=\"d\" item=\"a\"> mid-sentence"
        ));
        assert!(!is_agent_result(
            "the log said <agent name=\"x\"> mid-sentence"
        ));
    }

    #[test]
    fn message_roundtrips_through_json() {
        let original = Message {
            role: Role::Assistant,
            content: vec![
                Content::Thinking {
                    text: "let me think".into(),
                    signature: Some(ThinkingSignature {
                        model: "claude-x".into(),
                        data: "sig".into(),
                        redacted: false,
                    }),
                    duration_ms: Some(2_400),
                },
                Content::Text {
                    text: "the answer is 42".into(),
                },
                Content::ToolCall {
                    id: ToolCallId::new("call_01"),
                    name: "calc".into(),
                    input: serde_json::json!({"expr": "6 * 7"}),
                },
            ],
            id: MessageId::new(),
            parent: Some(MessageId::new()),
            ts: Utc::now(),
        };

        let encoded = serde_json::to_string(&original).expect("serialize");
        let decoded: Message = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(original, decoded);
    }

    #[test]
    fn shell_runs_read_back_from_their_text() {
        for exit_code in [Some(0), Some(2), None] {
            let run = ShellRun {
                command: "echo `a`; ls".into(),
                exit_code,
                output: "a\nb".into(),
            };
            assert_eq!(ShellRun::parse(&run.to_text()), Some(run));
        }
        assert_eq!(ShellRun::parse("plain prompt"), None);
    }

    #[test]
    fn role_serializes_as_snake_case() {
        let json = serde_json::to_string(&Role::ToolResult).unwrap();
        assert_eq!(json, "\"tool_result\"");
    }

    #[test]
    fn thinking_without_a_signature_reads_and_writes_as_before() {
        let old = r#"{"type":"thinking","text":"hmm"}"#;
        let block: Content = serde_json::from_str(old).unwrap();
        assert_eq!(
            block,
            Content::Thinking {
                text: "hmm".into(),
                signature: None,
                duration_ms: None,
            }
        );
        assert_eq!(serde_json::to_string(&block).unwrap(), old);

        let redacted = Content::Thinking {
            text: String::new(),
            signature: Some(ThinkingSignature {
                model: "m".into(),
                data: "enc".into(),
                redacted: true,
            }),
            duration_ms: None,
        };
        let json = serde_json::to_value(&redacted).unwrap();
        assert_eq!(json["signature"]["redacted"], true);
        assert_eq!(serde_json::from_value::<Content>(json).unwrap(), redacted);
    }

    #[test]
    fn thinking_duration_is_written_only_when_timed() {
        let timed = Content::Thinking {
            text: "hmm".into(),
            signature: None,
            duration_ms: Some(11_000),
        };
        let json = serde_json::to_string(&timed).unwrap();
        assert_eq!(
            json,
            r#"{"type":"thinking","text":"hmm","duration_ms":11000}"#
        );
        assert_eq!(serde_json::from_str::<Content>(&json).unwrap(), timed);
    }

    #[test]
    fn flattened_thinking_frames_text_and_skips_blank() {
        assert_eq!(
            Content::flattened_thinking("why").as_deref(),
            Some("<thinking>\nwhy\n</thinking>")
        );
        assert_eq!(Content::flattened_thinking("  "), None);
    }

    #[test]
    fn media_notes_name_audio_and_images() {
        assert_eq!(
            Content::produced_media_note("image/png"),
            "[you produced an image here: image/png]"
        );
        assert_eq!(
            Content::produced_media_note("audio/mpeg"),
            "[you produced audio here: audio/mpeg]"
        );
        assert_eq!(
            Content::audio_note("audio/mpeg"),
            "[an audio clip (audio/mpeg) was here; it is not sent to the model]"
        );
    }

    #[test]
    fn content_carries_type_tag() {
        let block = Content::Text { text: "hi".into() };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hi");
    }
}
