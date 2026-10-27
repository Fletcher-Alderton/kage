//! The in-file branch tree of a session.
//!
//! A session file is append-only JSONL. Besides the conversation
//! entries the loop writes, an undo or rewind appends a
//! [`SessionEntry::Branch`] marker: the conversation now continues
//! from the entry the marker names, and the entries between that entry
//! and the marker stay in the file as an abandoned branch.
//!
//! [`SessionTree`] turns one pass over the file into that tree. Every
//! non-marker entry gets a parent: the previous entry in the file, or
//! the target of a `Branch` marker sitting between it and its textual
//! predecessor, or the header for the first entry. The marker itself
//! is not a node. The active chain is the ancestry of the head, the
//! last node in the file; replay, fork and the session list follow it
//! and ignore the abandoned branches.

use std::collections::HashMap;
use std::path::Path;

use crate::entry::{EntryId, FORMAT_VERSION, Header, SessionEntry};
use crate::error::SessionError;
use crate::reader::SessionReader;

/// One non-marker entry of a session file with its tree position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeNode {
    /// The entry's own id.
    pub id: EntryId,
    /// The entry the conversation continued from: the previous entry
    /// in the file, or the target of a `Branch` marker between them,
    /// or the header for the first entry.
    pub parent: EntryId,
    /// Entries above this one on its branch: `0` for a direct child of
    /// the header.
    pub depth: u32,
    /// Whether the parent edge is a `Branch` marker redirect, the
    /// mark of a rewind: the conversation did not simply continue
    /// from the previous entry, it forked back to an earlier one.
    pub branched: bool,
}

/// One node of [`SessionTree::walk`]: a node with its entry, in
/// depth-first order, children in file order.
#[derive(Clone, Debug)]
pub struct TreeWalk<'a> {
    /// The node's tree position.
    pub node: TreeNode,
    /// Whether the node sits on the active chain, the ancestry of the
    /// head.
    pub active: bool,
    /// The entry itself.
    pub entry: &'a SessionEntry,
}

/// The branch tree of one session file.
///
/// Read with [`SessionTree::read`], which validates the header and
/// every branch marker. The whole file is decoded once and kept, so a
/// fork can copy an ancestry's entries without a second read.
#[derive(Debug)]
pub struct SessionTree {
    header: Header,
    nodes: Vec<(TreeNode, SessionEntry)>,
    index: HashMap<EntryId, usize>,
    active: Vec<bool>,
    head: EntryId,
    truncated: bool,
}

impl SessionTree {
    /// Read `path` into its branch tree.
    ///
    /// # Errors
    ///
    /// Errors like [`crate::replay`] does on a file this build cannot
    /// read: empty, headerless, a second header, an unsupported format
    /// version, a decode failure, or a `Branch` marker naming an entry
    /// the file does not hold at or before the marker.
    pub fn read(path: &Path) -> Result<Self, SessionError> {
        let mut reader = SessionReader::iter(path)?;
        let Some(first) = reader.next() else {
            if reader.torn_trailing() {
                return Err(SessionError::MissingHeader {
                    path: path.to_path_buf(),
                });
            }
            return Err(SessionError::Empty {
                path: path.to_path_buf(),
            });
        };
        let first = first?;
        let SessionEntry::Header(header) = first else {
            return Err(SessionError::MissingHeader {
                path: path.to_path_buf(),
            });
        };
        ensure_supported_version(path, header.version)?;
        let header_id = header.id;

        // `pending` carries the next `Branch` marker's target: the
        // parent of the next node. `prev` is the last node seen, the
        // parent when no marker intervened. Markers only ever point
        // backwards, so parent chains terminate at the header.
        let mut pending: Option<EntryId> = None;
        let mut prev: Option<EntryId> = None;
        let mut nodes: Vec<(TreeNode, SessionEntry)> = Vec::new();
        let mut index: HashMap<EntryId, usize> = HashMap::new();
        for item in reader.by_ref() {
            let entry = item?;
            match entry {
                SessionEntry::Header(_) => {
                    return Err(SessionError::SecondHeader {
                        path: path.to_path_buf(),
                    });
                }
                SessionEntry::Branch(marker) => {
                    if marker.at != header_id && !index.contains_key(&marker.at) {
                        return Err(SessionError::BranchTarget {
                            path: path.to_path_buf(),
                            at: marker.at,
                        });
                    }
                    pending = Some(marker.at);
                }
                other => {
                    let id = other.id();
                    let branched = pending.is_some();
                    let parent = pending.take().unwrap_or_else(|| prev.unwrap_or(header_id));
                    let depth = index
                        .get(&parent)
                        .map_or(0, |&i| nodes[i].0.depth.saturating_add(1));
                    index.insert(id, nodes.len());
                    nodes.push((
                        TreeNode {
                            id,
                            parent,
                            depth,
                            branched,
                        },
                        other,
                    ));
                    prev = Some(id);
                }
            }
        }
        let truncated = reader.torn_trailing();
        // A file ending on a marker has already moved on: its head is
        // the entry the marker names, even though that entry sits
        // earlier in the file.
        let head = pending.unwrap_or_else(|| nodes.last().map_or(header_id, |(node, _)| node.id));

        // The active chain is the ancestry of the head.
        let mut active = vec![false; nodes.len()];
        let mut cursor = head;
        while let Some(&i) = index.get(&cursor) {
            active[i] = true;
            cursor = nodes[i].0.parent;
        }
        Ok(Self {
            header,
            nodes,
            index,
            active,
            head,
            truncated,
        })
    }

    /// The file's header.
    #[must_use]
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The header's entry id, the root every branch descends from.
    #[must_use]
    pub fn root(&self) -> EntryId {
        self.header.id
    }

    /// The last node in the file, the tip of the active chain; the
    /// header id when the file holds no entries.
    #[must_use]
    pub fn head(&self) -> EntryId {
        self.head
    }

    /// Whether the file's tail was torn: entries after the head may
    /// have been lost to a crashed appender.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Whether `id` names the header or any entry of the file,
    /// abandoned branches included.
    #[must_use]
    pub fn contains(&self, id: EntryId) -> bool {
        id == self.header.id || self.index.contains_key(&id)
    }

    /// The parent of `id`: `None` for the header and for unknown ids.
    #[must_use]
    pub fn parent(&self, id: EntryId) -> Option<EntryId> {
        self.index.get(&id).map(|&i| self.nodes[i].0.parent)
    }

    /// Whether `id`'s parent edge is a `Branch` marker redirect, the
    /// mark of a rewind. The header and unknown ids are not.
    #[must_use]
    pub fn branched(&self, id: EntryId) -> bool {
        self.index
            .get(&id)
            .is_some_and(|&i| self.nodes[i].0.branched)
    }

    /// Whether `id` sits on the active chain, the ancestry of the
    /// head. The header is always active; unknown ids never are.
    #[must_use]
    pub fn is_active(&self, id: EntryId) -> bool {
        match self.index.get(&id) {
            Some(&i) => self.active[i],
            None => id == self.header.id,
        }
    }

    /// The entries of the active chain from the header down to `at`,
    /// in conversation order, header excluded. The empty slice's
    /// equivalent for `at == self.root()`; an unknown `at` yields
    /// nothing, which callers reject through [`Self::contains`] first.
    #[must_use]
    pub fn ancestry(&self, at: EntryId) -> Vec<&SessionEntry> {
        let mut chain = Vec::new();
        let mut cursor = at;
        while let Some(&i) = self.index.get(&cursor) {
            chain.push(&self.nodes[i].1);
            cursor = self.nodes[i].0.parent;
        }
        chain.reverse();
        chain
    }

    /// The entry of `id`, markers excluded: `None` for the header and
    /// unknown ids.
    #[must_use]
    pub fn entry(&self, id: EntryId) -> Option<&SessionEntry> {
        self.index.get(&id).map(|&i| &self.nodes[i].1)
    }

    /// Every node with its entry, in file order, markers excluded.
    pub fn iter(&self) -> impl Iterator<Item = (&TreeNode, &SessionEntry)> {
        self.nodes.iter().map(|(node, entry)| (node, entry))
    }

    /// Every node with its entry, in depth-first order: a branch's
    /// descendants come before the next sibling, and siblings keep
    /// file order. Markers are not nodes and never appear.
    #[must_use]
    pub fn walk(&self) -> Vec<TreeWalk<'_>> {
        let mut children: HashMap<EntryId, Vec<usize>> = HashMap::new();
        for (i, (node, _)) in self.nodes.iter().enumerate() {
            children.entry(node.parent).or_default().push(i);
        }
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut stack: Vec<usize> = children
            .get(&self.header.id)
            .map(|kids| kids.iter().rev().copied().collect())
            .unwrap_or_default();
        while let Some(i) = stack.pop() {
            let (node, entry) = &self.nodes[i];
            out.push(TreeWalk {
                node: *node,
                active: self.active[i],
                entry,
            });
            if let Some(kids) = children.get(&node.id) {
                stack.extend(kids.iter().rev().copied());
            }
        }
        out
    }
}

/// Reject a header whose schema version this build cannot interpret.
fn ensure_supported_version(path: &Path, version: u32) -> Result<(), SessionError> {
    if version > 0 && version <= FORMAT_VERSION {
        return Ok(());
    }
    Err(SessionError::UnsupportedVersion {
        path: path.to_path_buf(),
        found: version,
        supported: FORMAT_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use chrono::Utc;
    use kage_core::{Content, Message, Role};
    use tempfile::tempdir;

    use super::*;
    use crate::entry::{BranchEntry, EntryId, Header, MessageEntry, SessionId};
    use crate::writer::SessionWriter;

    fn fresh_header() -> Header {
        Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "x:y".into(),
            system_prompt: "be helpful".into(),
            parent_session: None,
            parent_entry: None,
        }
    }

    fn message_entry(role: Role, text: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Arc::new(Message::new(
                role,
                vec![Content::Text {
                    text: text.to_owned(),
                }],
                None,
            )),
            usage: None,
        })
    }

    fn branch(at: EntryId) -> SessionEntry {
        SessionEntry::Branch(BranchEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            at,
        })
    }

    fn write(path: &Path, header: Header, entries: &[SessionEntry]) {
        let mut w = SessionWriter::create(path, header).unwrap();
        for e in entries {
            w.append(e).unwrap();
        }
    }

    /// header, A, B, C, D, branch(header), E: the file an undo of the
    /// last two exchanges followed by one new prompt produces.
    fn branched(path: &Path) -> (Header, Vec<SessionEntry>) {
        let header = fresh_header();
        let entries = vec![
            message_entry(Role::User, "a"),
            message_entry(Role::Assistant, "b"),
            message_entry(Role::User, "c"),
            message_entry(Role::Assistant, "d"),
            branch(header.id),
            message_entry(Role::User, "e"),
        ];
        write(path, header.clone(), &entries);
        (header, entries)
    }

    #[test]
    fn a_branch_marker_redirects_the_parent_of_the_next_node() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let (header, entries) = branched(&path);

        let tree = SessionTree::read(&path).unwrap();
        assert_eq!(tree.root(), header.id);
        assert_eq!(tree.head(), entries[5].id(), "the head is the last node");
        // a -> b -> c -> d on the first branch, e continues from the
        // header, and the marker is not a node.
        assert_eq!(tree.parent(entries[0].id()), Some(header.id));
        assert_eq!(tree.parent(entries[1].id()), Some(entries[0].id()));
        assert_eq!(tree.parent(entries[5].id()), Some(header.id));
        assert!(!tree.contains(entries[4].id()), "markers are not nodes");
        assert_eq!(tree.parent(header.id), None, "the header has no parent");
    }

    #[test]
    fn the_active_chain_is_the_ancestry_of_the_head() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let (_, entries) = branched(&path);

        let tree = SessionTree::read(&path).unwrap();
        assert!(!tree.is_active(entries[1].id()));
        assert!(!tree.is_active(entries[2].id()));
        assert!(!tree.is_active(entries[3].id()));
        assert!(
            !tree.is_active(entries[0].id()),
            "the branch restarts from the header, so a is abandoned too"
        );
        assert!(tree.is_active(entries[5].id()));
        assert!(tree.is_active(tree.root()));
        assert!(!tree.is_active(EntryId::new()));

        let active: Vec<&str> = tree
            .walk()
            .iter()
            .filter(|step| step.active)
            .map(|step| match step.entry {
                SessionEntry::Message(m) => m.message.content.first(),
                _ => None,
            })
            .filter_map(|block| match block {
                Some(Content::Text { text }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(active, ["e"], "the abandoned branch is skipped");
    }

    #[test]
    fn ancestry_of_a_node_is_its_parent_chain_root_first() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let (header, entries) = branched(&path);

        let tree = SessionTree::read(&path).unwrap();
        let texts: Vec<&str> = tree
            .ancestry(entries[3].id())
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message(m) => match &m.message.content[0] {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["a", "b", "c", "d"]);
        assert!(tree.ancestry(header.id).is_empty());
    }

    #[test]
    fn walk_lists_children_in_file_order_depth_first() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let (_, entries) = branched(&path);
        // A reply under e: the active chain deepens.
        let deep = message_entry(Role::Assistant, "f");
        let mut w = SessionWriter::open(&path).unwrap();
        w.append(&deep).unwrap();
        drop(w);

        let tree = SessionTree::read(&path).unwrap();
        let texts: Vec<&str> = tree
            .walk()
            .iter()
            .filter_map(|step| match step.entry {
                SessionEntry::Message(m) => match &m.message.content[0] {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["a", "b", "c", "d", "e", "f"]);
        let depths: Vec<u32> = tree.walk().iter().map(|step| step.node.depth).collect();
        assert_eq!(depths, [0, 1, 2, 3, 0, 1]);
        assert_eq!(tree.walk().len(), 6, "the marker is not walked");
        assert_eq!(tree.parent(deep.id()), Some(entries[5].id()));
        assert_eq!(tree.head(), deep.id());
    }

    #[test]
    fn branch_edges_mark_where_a_rewind_forked() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let (header, entries) = branched(&path);

        let tree = SessionTree::read(&path).unwrap();
        // a -> b -> c -> d are the original thread: natural edges
        // only. The marker redirects e to the header, a branch edge.
        for entry in &entries[..4] {
            assert!(!tree.branched(entry.id()), "{entry:?} is a natural edge");
        }
        assert!(tree.branched(entries[5].id()), "e forked via the marker");
        assert!(!tree.branched(header.id));

        // One branch edge on e's ancestry, none on a's.
        let count = |at| {
            let mut edges = 0;
            let mut cursor = at;
            while let Some(parent) = tree.parent(cursor) {
                if tree.branched(cursor) {
                    edges += 1;
                }
                cursor = parent;
            }
            edges
        };
        assert_eq!(count(entries[0].id()), 0);
        assert_eq!(count(entries[5].id()), 1);
    }

    #[test]
    fn a_trailing_marker_moves_the_head_to_its_target() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let header = fresh_header();
        let a = message_entry(Role::User, "a");
        write(&path, header.clone(), &[a.clone(), branch(a.id())]);

        let tree = SessionTree::read(&path).unwrap();
        assert_eq!(tree.head(), a.id(), "the file ends on a marker");
        assert!(tree.is_active(a.id()));
    }

    #[test]
    fn a_branch_pointing_at_an_unknown_entry_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("forward.jsonl");
        // The marker names an entry that only appears after it.
        let ghost = EntryId::new();
        write(
            &path,
            fresh_header(),
            &[branch(ghost), message_entry(Role::User, "a")],
        );
        let err = SessionTree::read(&path).unwrap_err();
        assert!(matches!(err, SessionError::BranchTarget { .. }), "{err:?}");
        assert!(err.to_string().contains("does not hold"));
    }

    #[test]
    fn a_branch_may_target_the_header() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("top.jsonl");
        let header = fresh_header();
        let a = message_entry(Role::User, "a");
        write(&path, header.clone(), &[a.clone(), branch(header.id)]);

        let tree = SessionTree::read(&path).unwrap();
        assert_eq!(tree.parent(a.id()), Some(header.id));
        assert!(
            !tree.is_active(a.id()),
            "a trailing marker abandons everything before it"
        );
        assert!(tree.is_active(tree.root()));
        assert_eq!(
            tree.head(),
            header.id,
            "the head follows the trailing marker"
        );
    }

    #[test]
    fn a_version_one_file_still_reads_as_a_linear_tree() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("old.jsonl");
        let mut header = fresh_header();
        header.version = 1;
        let a = message_entry(Role::User, "a");
        let b = message_entry(Role::Assistant, "b");
        write(&path, header.clone(), &[a.clone(), b.clone()]);

        let tree = SessionTree::read(&path).unwrap();
        assert!(tree.is_active(a.id()) && tree.is_active(b.id()));
        assert_eq!(tree.parent(a.id()), Some(header.id));
    }

    #[test]
    fn a_version_two_file_still_reads_as_a_linear_tree() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("v2.jsonl");
        let mut header = fresh_header();
        header.version = 2;
        let a = message_entry(Role::User, "a");
        let b = message_entry(Role::Assistant, "b");
        write(&path, header.clone(), &[a.clone(), b.clone()]);

        let tree = SessionTree::read(&path).unwrap();
        assert!(tree.is_active(a.id()) && tree.is_active(b.id()));
        assert_eq!(tree.parent(a.id()), Some(header.id));
    }

    #[test]
    fn structural_failures_match_replays() {
        let dir = tempdir().unwrap();
        let empty = dir.path().join("empty.jsonl");
        std::fs::write(&empty, b"").unwrap();
        assert!(matches!(
            SessionTree::read(&empty),
            Err(SessionError::Empty { .. })
        ));

        let headerless = dir.path().join("headerless.jsonl");
        // A decodable first entry: a bad *trailing* line would be a
        // torn write, so the line must parse to be refused as
        // headerless.
        let line = serde_json::to_string(&message_entry(Role::User, "hi")).unwrap();
        std::fs::write(&headerless, format!("{line}\n")).unwrap();
        assert!(matches!(
            SessionTree::read(&headerless),
            Err(SessionError::MissingHeader { .. })
        ));

        let path = dir.path().join("two.jsonl");
        write(
            &path,
            fresh_header(),
            &[SessionEntry::Header(fresh_header())],
        );
        assert!(matches!(
            SessionTree::read(&path),
            Err(SessionError::SecondHeader { .. })
        ));

        let future = dir.path().join("future.jsonl");
        let mut header = fresh_header();
        header.version = FORMAT_VERSION + 1;
        write(&future, header, &[message_entry(Role::User, "a")]);
        assert!(matches!(
            SessionTree::read(&future),
            Err(SessionError::UnsupportedVersion { .. })
        ));
    }
}
