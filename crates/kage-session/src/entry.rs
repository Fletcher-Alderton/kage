//! Append-only JSONL session entry types.
//!
//! A session is a flat JSONL file. The first line is a [`SessionEntry::Header`];
//! subsequent lines are entries appended in order. Each entry has its own id
//! and timestamp so forks and rewinds can target any point. An undo or
//! rewind appends a [`SessionEntry::Branch`] marker instead of rewriting:
//! the conversation continues from the entry the marker names, and the
//! entries it abandons stay in the file. See [`crate::tree::SessionTree`].

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
pub use kage_core::SessionId;
use kage_core::{Message, TokenUsage};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Current on-disk session format version.
///
/// Bumped whenever the entry schema changes in a non-additive way. Readers
/// accept the versions whose entries they all interpret: version 2 added
/// the [`SessionEntry::Branch`] marker, which version 1 readers must not
/// see, and version 3 added `Content::Audio`, which version 2 readers
/// cannot decode.
pub const FORMAT_VERSION: u32 = 3;

/// Stable identifier for a single entry within a session.
///
/// Used as the cut point for [`fork`](crate::fork()): a forked child copies
/// every entry up to and including the named id.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(pub Ulid);

impl EntryId {
    /// Generate a fresh entry id.
    #[must_use]
    pub fn new() -> Self {
        Self(Ulid::generate())
    }
}

impl Default for EntryId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One JSONL line in a session file.
///
/// The `Header` variant must appear exactly once, on the first line. All
/// other variants may appear any number of times in any order. The reader
/// preserves order on disk and the writer never rewrites prior lines.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    /// First line of every session file. Carries identity and provenance.
    Header(Header),
    /// A reassembled conversation message (user, assistant, or tool result).
    Message(MessageEntry),
    /// The model's thinking budget was changed mid-session.
    ThinkingLevelChange(ThinkingLevelChange),
    /// The active model was switched mid-session.
    ModelChange(ModelChange),
    /// Older turns were summarized into a synthetic message.
    Compaction(Compaction),
    /// A user-supplied bookmark anchored at a specific entry.
    Label(Label),
    /// A short generated title for the session, written once after
    /// the first assistant response. Latest one wins on read.
    Title(SessionTitle),
    /// The conversation continues from entry `at`. Entries between
    /// that entry and the marker stay in the file as an abandoned
    /// branch; replay, fork and the session list follow the marker and
    /// ignore them. Written by an in-place undo or rewind.
    Branch(BranchEntry),
    /// A plugin-defined entry the core does not interpret.
    Custom(Custom),
}

impl SessionEntry {
    /// Id assigned to this entry at append time.
    #[must_use]
    pub fn id(&self) -> EntryId {
        match self {
            Self::Header(h) => h.id,
            Self::Message(m) => m.id,
            Self::ThinkingLevelChange(t) => t.id,
            Self::ModelChange(m) => m.id,
            Self::Compaction(c) => c.id,
            Self::Label(l) => l.id,
            Self::Title(t) => t.id,
            Self::Branch(b) => b.id,
            Self::Custom(c) => c.id,
        }
    }

    /// UTC timestamp at which this entry was appended.
    #[must_use]
    pub fn ts(&self) -> DateTime<Utc> {
        match self {
            Self::Header(h) => h.ts,
            Self::Message(m) => m.ts,
            Self::ThinkingLevelChange(t) => t.ts,
            Self::ModelChange(m) => m.ts,
            Self::Compaction(c) => c.ts,
            Self::Label(l) => l.ts,
            Self::Title(t) => t.ts,
            Self::Branch(b) => b.ts,
            Self::Custom(c) => c.ts,
        }
    }
}

/// Identity and provenance of a session file. Always the first entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Header {
    /// On-disk schema version. See [`FORMAT_VERSION`].
    pub version: u32,
    /// This session's unique id.
    pub session: SessionId,
    /// Entry id assigned to the header itself.
    pub id: EntryId,
    /// Creation timestamp.
    pub ts: DateTime<Utc>,
    /// Working directory at session creation.
    pub cwd: PathBuf,
    /// Provider-qualified model id (`provider/model`) at creation.
    pub model: String,
    /// System prompt at creation. Subsequent changes are not reflected here.
    pub system_prompt: String,
    /// The session this one was forked from or spawned by, if any. A
    /// spawned session marks itself with a `kage:agent` custom entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<SessionId>,
    /// Entry id within `parent_session` that this fork branched from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_entry: Option<EntryId>,
}

/// A reassembled conversation message persisted as a session entry.
///
/// Assistant messages may carry a [`TokenUsage`] snapshot from the
/// turn's `MessageEnd` event. User and tool-result messages set
/// `usage` to `None`. Older session files written before the field
/// existed deserialize cleanly: `serde(default)` falls back to
/// `None`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageEntry {
    /// Entry id.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// The full message including role, content blocks, and message id.
    /// Shared with the loop's history buffer; serializes identically to
    /// [`Message`].
    pub message: Arc<Message>,
    /// Provider-reported token usage for this turn, when available.
    /// Optional and defaulted to `None` so old sessions still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

/// Records a change to the model's thinking budget.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThinkingLevelChange {
    /// Entry id.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// New thinking budget level. Plugin-defined; the loop forwards it as is.
    pub level: String,
}

/// Records a switch to a different provider-qualified model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelChange {
    /// Entry id.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// New provider-qualified model id (`provider/model`).
    pub model: String,
}

/// Records a history compaction performed by the loop.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    /// Entry id.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// Number of recent turns kept verbatim after compaction.
    pub kept: usize,
    /// Number of older turns replaced by the summary.
    pub summarized: usize,
    /// Synthetic summary text inserted in place of the summarized turns.
    pub summary: String,
    /// Context tokens in use right before the loop spliced the summary
    /// in. Optional and defaulted so old sessions still parse; the
    /// other half of a compaction card's token delta. The post-turn
    /// fill is not stored: replay reads it from the first usage
    /// recorded after this entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
}

/// User-supplied label/bookmark anchored to a specific entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Label {
    /// Entry id of the label itself.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// Free-form label text shown by `kage list` and search.
    pub text: String,
    /// The entry the label is attached to.
    pub anchor: EntryId,
}

/// A short generated title for the session. Written once after the
/// first assistant response (the host generates it); the most recent
/// `Title` entry wins when a session is summarized, so a later
/// regeneration can override an earlier one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionTitle {
    /// Entry id of the title itself.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// The generated title text (already trimmed to a short length
    /// by the host).
    pub title: String,
}

/// Plugin-defined entry. The core neither validates nor interprets `data`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Custom {
    /// Entry id.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// Plugin-defined kind tag, namespaced like `plugin:tps`.
    pub kind: String,
    /// Arbitrary JSON payload.
    pub data: serde_json::Value,
}

/// Marks where the conversation continues after an undo or rewind.
///
/// The marker itself is not part of the conversation: the entry
/// [`BranchEntry::at`] names takes its previous entry's place as the
/// parent of whatever is appended next, and the entries between that
/// entry and the marker are abandoned but kept.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchEntry {
    /// Entry id of the marker itself.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// The entry the conversation continues from, or the header id to
    /// restart from the top.
    pub at: EntryId,
}

#[cfg(test)]
mod tests {
    use kage_core::{Content, Role};

    use super::*;

    #[test]
    fn header_round_trips() {
        let header = SessionEntry::Header(Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/tmp/work"),
            model: "anthropic:claude-sonnet-4-6".into(),
            system_prompt: "You are kage.".into(),
            parent_session: None,
            parent_entry: None,
        });
        let line = serde_json::to_string(&header).unwrap();
        let back: SessionEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(header, back);
    }

    #[test]
    fn header_skips_none_parents() {
        let header = SessionEntry::Header(Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/tmp"),
            model: "x:y".into(),
            system_prompt: String::new(),
            parent_session: None,
            parent_entry: None,
        });
        let json = serde_json::to_value(&header).unwrap();
        assert!(json.get("parent_session").is_none());
        assert!(json.get("parent_entry").is_none());
    }

    #[test]
    fn message_entry_round_trips() {
        let entry = SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Arc::new(Message::new(
                Role::User,
                vec![Content::Text { text: "hi".into() }],
                None,
            )),
            usage: None,
        });
        let line = serde_json::to_string(&entry).unwrap();
        let back: SessionEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn entry_carries_type_tag() {
        let entry = SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "milestone".into(),
            anchor: EntryId::new(),
        });
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "label");
        assert_eq!(json["text"], "milestone");
    }

    #[test]
    fn compaction_round_trips() {
        let entry = SessionEntry::Compaction(Compaction {
            id: EntryId::new(),
            ts: Utc::now(),
            kept: 4,
            summarized: 12,
            summary: "[summary of 12 earlier turns]\nuser asked about X.".into(),
            before: Some(160_000),
        });
        let line = serde_json::to_string(&entry).unwrap();
        let back: SessionEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(entry, back);
    }

    /// A compaction written before the `before` count existed parses
    /// with it as `None`.
    #[test]
    fn compaction_without_before_reads_none() {
        let line = format!(
            "{{\"type\":\"compaction\",\"id\":\"{}\",\"ts\":\"2026-01-01T00:00:00Z\",\"kept\":2,\"summarized\":4,\"summary\":\"s\"}}",
            ulid::Ulid::generate()
        );
        let back: SessionEntry = serde_json::from_str(&line).unwrap();
        match back {
            SessionEntry::Compaction(c) => {
                assert_eq!(c.before, None);
                assert_eq!(c.kept, 2);
            }
            other => panic!("expected compaction, got {other:?}"),
        }
    }

    #[test]
    fn custom_round_trips() {
        let entry = SessionEntry::Custom(Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind: "plugin:notes".into(),
            data: serde_json::json!({ "note": "remember to test" }),
        });
        let line = serde_json::to_string(&entry).unwrap();
        let back: SessionEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn branch_round_trips() {
        let entry = SessionEntry::Branch(BranchEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            at: EntryId::new(),
        });
        let line = serde_json::to_string(&entry).unwrap();
        let back: SessionEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(entry, back);
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "branch");
    }

    #[test]
    fn entry_id_helpers() {
        let id = EntryId::new();
        let entry = SessionEntry::ThinkingLevelChange(ThinkingLevelChange {
            id,
            ts: Utc::now(),
            level: "high".into(),
        });
        assert_eq!(entry.id(), id);
    }

    #[test]
    fn format_version_is_three() {
        assert_eq!(FORMAT_VERSION, 3);
    }
}
