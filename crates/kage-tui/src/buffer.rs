//! Conversation buffer model.
//!
//! [`Buffer`] is an append-only list of [`Block`]s plus a scroll offset.
//! It is the source of truth the TUI's render loop walks each frame: the
//! buffer maps a streamed [`kage_core::LoopEvent`] timeline (assembled
//! by the host's `Hooks` impl) into discrete blocks the renderer can
//! lay out.
//!
//! Folding state lives on each block so the user can collapse thinking
//! blocks and tool calls without losing their content.

pub(crate) use std::mem;
pub(crate) use std::sync::Arc;
pub(crate) use std::time::{Duration, Instant};

use std::collections::{HashMap, HashSet};

pub(crate) use ratatui::text::Line;

pub(crate) use crate::view::tool_view::{EditDiff, ToolPhase};

/// One renderable region of the conversation.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// User prompt.
    User {
        /// Raw user text. May contain newlines.
        text: String,
    },
    /// Assistant text response. The host appends streamed deltas with
    /// [`Buffer::append_assistant_delta`] until the turn ends.
    Assistant {
        /// Reassembled assistant text.
        text: String,
        /// Whether this block is still receiving deltas.
        live: bool,
    },
    /// Hidden chain-of-thought emitted by the model.
    Thinking {
        /// Reassembled thinking text.
        text: String,
        /// Whether the body is collapsed. A live folded block still
        /// shows its last lines.
        folded: bool,
        /// Whether this block is still receiving deltas.
        live: bool,
        /// When the block began.
        started_at: Instant,
        /// How long the model thought, recorded when the block
        /// finished. `None` for a block replayed from history without
        /// a stored duration.
        duration_ms: Option<u64>,
        /// Whether the user toggled the fold while the block was live.
        /// Finishing keeps a pinned block's fold state instead of
        /// collapsing it.
        pinned: bool,
    },
    /// One tool invocation by the assistant.
    ToolCall {
        /// Stable id from the provider; matches the corresponding
        /// [`Block::ToolResult`].
        call_id: String,
        /// Tool name as the model invoked it.
        name: String,
        /// One-line summary of the tool input shown in the folded
        /// header (e.g. `shell("ls -la")`).
        input_summary: String,
        /// Pretty-printed full input, for search and yank.
        input_pretty: String,
        /// Parsed input, partial while the model streams it. Shared, so
        /// the per-frame buffer snapshot does not copy it.
        input: Arc<serde_json::Value>,
        /// Whether the user has collapsed the body.
        folded: bool,
        /// Where the call is in its lifecycle.
        phase: ToolPhase,
        /// Latest progress text from the running tool. Each update
        /// replaces it.
        progress: String,
        /// When the call entered its current phase. Entering
        /// [`ToolPhase::Running`] resets it, so the duration shown
        /// once the result arrives excludes any approval wait.
        started_at: Instant,
        /// A finished edit's change as whole lines of its file, set by
        /// [`Buffer::annotate_edits`]. `None` shows the change its
        /// input describes.
        diff: Option<Arc<EditDiff>>,
    },
    /// Output of a previously-issued tool call.
    ToolResult {
        /// Correlation id matching the prior [`Block::ToolCall`].
        call_id: String,
        /// Tool name, copied for header rendering.
        name: String,
        /// Stringified output.
        output: String,
        /// Whether the tool reported failure.
        is_error: bool,
        /// Whether the user has collapsed the body.
        folded: bool,
        /// Milliseconds elapsed between the matching call's
        /// `started_at` and when this result was pushed. `None` when
        /// the call was missing (orphan result).
        duration_ms: Option<u64>,
    },
    /// Plugin-defined block the core does not interpret.
    Custom {
        /// Plugin-defined kind tag, namespaced like `plugin:tps`.
        kind: String,
        /// Human-readable text the renderer shows verbatim.
        text: String,
        /// Whether the user has collapsed the body.
        folded: bool,
    },
}

impl Block {
    /// Byte length of the owned text this block stores. Small
    /// constant fields (ids, names, kinds) are not counted: the
    /// quantity exists for [`Buffer`]'s scrollback byte cap, which
    /// bounds the payload that can grow without bound.
    #[must_use]
    pub(crate) fn text_bytes(&self) -> usize {
        match self {
            Self::User { text }
            | Self::Assistant { text, .. }
            | Self::Thinking { text, .. }
            | Self::Custom { text, .. } => text.len(),
            Self::ToolCall {
                input_summary,
                input_pretty,
                progress,
                diff,
                ..
            } => {
                input_summary.len()
                    + input_pretty.len()
                    + progress.len()
                    + diff
                        .as_ref()
                        .map_or(0, |d| d.lines.iter().map(|l| l.text.len()).sum::<usize>())
            }
            Self::ToolResult { output, .. } => output.len(),
        }
    }

    /// Count of logical (newline-separated) lines this block contributes
    /// when rendered. Folded blocks always contribute 1 (the header).
    /// Width-aware wrapping happens in the renderer.
    #[must_use]
    pub fn line_count(&self) -> usize {
        match self {
            Self::User { text } | Self::Assistant { text, .. } => count_lines(text),
            Self::Thinking { text, folded, .. } => {
                if *folded {
                    1
                } else {
                    1 + count_lines(text)
                }
            }
            Self::ToolCall {
                input_pretty,
                folded,
                ..
            } => {
                if *folded {
                    1
                } else {
                    1 + count_lines(input_pretty)
                }
            }
            Self::ToolResult { output, folded, .. } => {
                if *folded {
                    1
                } else {
                    1 + count_lines(output)
                }
            }
            Self::Custom { text, folded, .. } => {
                if *folded {
                    1
                } else {
                    count_lines(text)
                }
            }
        }
    }

    /// True if the block is collapsible (has a folded/unfolded toggle).
    #[must_use]
    pub fn is_foldable(&self) -> bool {
        matches!(
            self,
            Self::Thinking { .. }
                | Self::ToolCall { .. }
                | Self::ToolResult { .. }
                | Self::Custom { .. }
        )
    }

    /// Toggle the fold state. No-op for non-foldable blocks. Toggling a
    /// live thinking block pins its fold state.
    pub fn toggle_fold(&mut self) {
        match self {
            Self::Thinking {
                folded,
                live,
                pinned,
                ..
            } => {
                *folded = !*folded;
                *pinned |= *live;
            }
            Self::ToolCall { folded, .. }
            | Self::ToolResult { folded, .. }
            | Self::Custom { folded, .. } => *folded = !*folded,
            _ => {}
        }
    }

    /// Mark a streaming block as no longer accepting deltas. A live
    /// thinking block records how long it ran and folds unless pinned.
    pub fn finish(&mut self) {
        match self {
            Self::Assistant { live, .. } => *live = false,
            Self::Thinking {
                live: live @ true,
                folded,
                started_at,
                duration_ms,
                pinned,
                ..
            } => {
                *live = false;
                *duration_ms = Some(elapsed_ms(*started_at));
                if !*pinned {
                    *folded = true;
                }
            }
            _ => {}
        }
    }

    /// Whether the block paints a ticking timer: live thinking or a
    /// running tool call.
    #[must_use]
    pub fn is_timed(&self) -> bool {
        matches!(
            self,
            Self::Thinking { live: true, .. }
                | Self::ToolCall {
                    phase: ToolPhase::Running,
                    ..
                }
        )
    }
}

/// Milliseconds since `start`, saturating.
fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// True if `haystack` contains `needle` ignoring ASCII case (a/A,
/// b/B, ...). Non-ASCII bytes are compared exactly. Allocates
/// nothing. Returns `false` for empty needles.
fn ascii_icontains(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || n.len() > h.len() {
        return false;
    }
    let limit = h.len() - n.len();
    'outer: for i in 0..=limit {
        for j in 0..n.len() {
            if !h[i + j].eq_ignore_ascii_case(&n[j]) {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.split('\n').count()
}

/// First line of `text` without markdown bold markers and backticks,
/// for single-line summaries.
fn plain_first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .replace("**", "")
        .replace('`', "")
}

/// Trim `label` to at most `label_width` characters, appending an
/// ellipsis when truncated. `None` for empty labels.
fn truncate_label(label: &str, label_width: usize) -> Option<String> {
    if label.is_empty() {
        return None;
    }
    if label.chars().count() <= label_width {
        return Some(label.to_owned());
    }
    let cut: String = label.chars().take(label_width.saturating_sub(3)).collect();
    Some(format!("{cut}..."))
}

/// Blank rows the conversation leaves between two displayed blocks.
/// Consecutive tool rows sit flush so a burst of calls stays compact;
/// every other neighbour pair gets one row.
pub(crate) fn gap_between(above: &Block, below: &Block) -> usize {
    let tool_row = |b: &Block| matches!(b, Block::ToolCall { .. } | Block::ToolResult { .. });
    usize::from(!(tool_row(above) && tool_row(below)))
}

/// Call/result pairing for [`Block::ToolCall`] and
/// [`Block::ToolResult`] blocks plus the `Explored` groups of
/// read-only calls, derived from the block list and cached between
/// frames. The renderer rebuilds it only when the block count, the
/// structural epoch or the fold generation changes; holding it behind
/// an [`Arc`] lets buffer snapshots share it instead of copying a map
/// of every call id per frame.
#[derive(Debug, Default)]
pub struct ToolTopology {
    /// Result block index for each call block index. A result pairs
    /// with the newest earlier call of its id that has no result yet,
    /// so a reused id never pairs with an older turn's call.
    pub(crate) result_of_call: HashMap<usize, usize>,
    /// Result block indexes already merged into their call block.
    pub(crate) consumed_results: HashSet<usize>,
    /// Result block index to its call block index.
    pub(crate) call_idx_for_result: HashMap<usize, usize>,
    /// `Explored` group head to all of its member calls, head first.
    pub(crate) groups: HashMap<usize, Vec<usize>>,
    /// Grouped call index to its group head, for every member but the
    /// head.
    pub(crate) head_of_member: HashMap<usize, usize>,
}

impl ToolTopology {
    /// Derive the pairing and grouping from an append-only block list.
    pub(crate) fn build(blocks: &[Arc<Block>]) -> Self {
        let mut topo = Self::default();
        let mut open: HashMap<&str, usize> = HashMap::new();
        for (i, block) in blocks.iter().enumerate() {
            match block.as_ref() {
                Block::ToolCall { call_id, .. } => {
                    open.insert(call_id, i);
                }
                Block::ToolResult { call_id, .. } => {
                    if let Some(call) = open.remove(call_id.as_str()) {
                        topo.result_of_call.insert(call, i);
                        topo.consumed_results.insert(i);
                        topo.call_idx_for_result.insert(i, call);
                    }
                }
                _ => {}
            }
        }
        topo.group_read_only_runs(blocks);
        topo
    }

    /// Group every maximal run of at least two displayed blocks that
    /// are finished, paired, read-only tool calls, unless the user
    /// unfolded the run's first call, which shows the run's calls
    /// individually.
    fn group_read_only_runs(&mut self, blocks: &[Arc<Block>]) {
        let mut run = Vec::new();
        for (i, block) in blocks.iter().enumerate() {
            if self.consumed_results.contains(&i) {
                continue;
            }
            if self.groupable(i, block.as_ref()) {
                run.push(i);
            } else {
                self.close_run(&mut run, blocks);
            }
        }
        self.close_run(&mut run, blocks);
    }

    fn groupable(&self, idx: usize, block: &Block) -> bool {
        matches!(
            block,
            Block::ToolCall {
                name,
                phase: ToolPhase::Done,
                ..
            } if crate::view::tool_view::is_read_only(name)
                && self.result_of_call.contains_key(&idx)
        )
    }

    fn close_run(&mut self, run: &mut Vec<usize>, blocks: &[Arc<Block>]) {
        let head_folded = run
            .first()
            .is_some_and(|&h| matches!(blocks[h].as_ref(), Block::ToolCall { folded: true, .. }));
        if run.len() >= 2 && head_folded {
            let head = run[0];
            for &member in &run[1..] {
                self.head_of_member.insert(member, head);
            }
            self.groups.insert(head, mem::take(run));
        }
        run.clear();
    }

    /// Whether block `idx` paints as part of another block: a result
    /// merged into its call, or a call grouped under an `Explored`
    /// head.
    pub(crate) fn is_hidden(&self, idx: usize) -> bool {
        self.consumed_results.contains(&idx) || self.head_of_member.contains_key(&idx)
    }

    /// The displayed block that paints block `idx`.
    pub(crate) fn display_idx(&self, idx: usize) -> usize {
        let call = self.call_idx_for_result.get(&idx).copied().unwrap_or(idx);
        self.head_of_member.get(&call).copied().unwrap_or(call)
    }
}

/// Append-only conversation history with a viewport anchor in
/// absolute virtual-row space. `scroll == None` means the viewport is
/// pinned to the latest content (auto-follow on streaming);
/// `Some(top)` means the viewport's first row is virtual row `top`,
/// so content arriving below never moves what the user is reading.
/// The model layer does not cap the anchor: only the renderer knows
/// how many visual rows the wrapped blocks occupy, so it clamps
/// against the real total each frame (and re-arms follow when the
/// clamp lands the viewport on the bottom row).
#[derive(Clone, Debug, Default)]
pub struct Buffer {
    /// Blocks, each shared behind an [`Arc`] so a [`Buffer::clone`] (the
    /// per-frame draw snapshot) copies pointers instead of every
    /// block's text. Mutators go through [`Arc::make_mut`], which
    /// clones a block's payload only while a snapshot still shares it.
    blocks: Vec<Arc<Block>>,
    /// Running sum of every block's [`Block::text_bytes`], maintained
    /// by block adds, text growth and in-place text replacement, and
    /// reduced by [`Self::compact_to`] when blocks are dropped. Keeps
    /// the [`MAX_BYTES`] enforcement in [`Self::trim_scrollback`] a
    /// comparison instead of a rescan of the block list.
    total_text_bytes: usize,
    scroll: Option<usize>,
    /// Index of the user-selected foldable block, if any. `None` means
    /// "no explicit selection"; the renderer falls back to the last
    /// foldable block in the buffer for fold-toggle gestures.
    focus: Option<usize>,
    /// The focus value the renderer last painted. The renderer
    /// compares this to the focus it paints each frame; when they
    /// differ, it invalidates the moved blocks' caches so emphasis
    /// repaints.
    last_drawn_focus: Option<usize>,
    /// The explicit focus the renderer last saw. Auto-scrolling a
    /// moved focus into view keys on this, not the effective focus:
    /// appended blocks change the effective fallback every time one
    /// lands, and a streaming append must never yank a pinned
    /// viewport back to the bottom.
    last_user_focus: Option<usize>,
    /// Per-block rendered-height cache, indexed parallel to
    /// [`Self::blocks`]. Each entry stores `(width, height_in_rows)`
    /// captured by the renderer's last successful layout pass for
    /// that block. The renderer reuses cached entries whose `width`
    /// matches the current viewport width and otherwise rebuilds.
    /// Mutators push or invalidate entries in lockstep with `blocks`
    /// to avoid stale data; this is what lets virtualized rendering
    /// skip building [`ratatui::text::Line`]s for off-screen blocks.
    block_heights: Vec<Option<(u16, u16)>>,
    /// Per-block rendered-line cache, indexed parallel to
    /// [`Self::blocks`]. Each entry stores
    /// `(width, Arc<Vec<Line<'static>>>)` captured at the same time
    /// as [`Self::block_heights`]. Renderers reuse the lines when
    /// the block is unfocused (no emphasis-driven rebuild), turning
    /// the per-frame cost of a re-render into a `Vec<Line>` clone.
    /// Stored behind `Arc` so the mutex isn't holding a clone of a
    /// possibly-huge vector while the renderer is still using it.
    block_render_lines: Vec<Option<(u16, Arc<Vec<Line<'static>>>)>>,
    /// Cached call/result block pairing together with the
    /// [`TopologyKey`] it was built at, shared behind an [`Arc`]. See
    /// [`ToolTopology`]. `None` until the first render; rebuilt
    /// whenever the key changed since.
    tool_topology: Option<(TopologyKey, Arc<ToolTopology>)>,
    /// Bumped by every fold change, which can form or split
    /// `Explored` groups. Streaming deltas leave it alone.
    fold_generation: u64,
    /// Structural generation, bumped by every change that is not a
    /// pure append (`clear`, `take`, compaction). Block indices from
    /// one epoch mean nothing in another, so index-keyed caches built
    /// against a different epoch are discarded. Appends leave it
    /// alone because existing indices stay valid.
    epoch: u64,
    /// Monotonically increasing counter bumped by every mutation
    /// (push, append, fold, focus, scroll). The render loop reads
    /// this to decide whether to repaint: an unchanged version means
    /// nothing user-visible has shifted, so the previous frame is
    /// still correct and we can sleep instead of redrawing at the
    /// full 30 Hz target. Wraps at `u64::MAX`, which won't happen in
    /// any realistic session lifetime.
    version: u64,
    /// Map of "what block currently sits under each screen row in
    /// the buffer area": `(block_idx, screen_top, screen_bottom)` in
    /// absolute terminal coordinates. The renderer rewrites this
    /// each frame; mouse handlers read it to translate a click row
    /// into a block. Cleared whenever the buffer is empty.
    last_block_screen_rows: Vec<(usize, u16, u16)>,
    /// `(idx, virtual_top, virtual_bottom)` per painted block in the
    /// last frame, in the unclamped 0..total virtual-row space (the
    /// same space mouse-selection rows live in). Unlike
    /// [`Self::last_block_screen_rows`] this is *not* clamped to the
    /// viewport, so a block scrolled past its own top still reports
    /// its true first row - yank uses this to map a selected row to
    /// the right source line regardless of scroll.
    last_block_virtual_rows: Vec<(usize, usize, usize)>,
    /// Width and X-origin of the buffer area in the last painted
    /// frame. Mouse handlers use this to translate a click column
    /// into a block-relative char column.
    last_area_x: u16,
    last_area_width: u16,
    /// First virtual row (0-indexed across the whole rendered buffer)
    /// that was visible in the last painted frame. Mouse handlers
    /// add `screen_row - area_y` to this to get a stable virtual-row
    /// coordinate that survives subsequent scrolls; the renderer uses
    /// it the other way to translate a virtual row back to a screen
    /// row when painting selection overlay.
    last_virtual_top: usize,
    last_area_y: u16,
    last_area_height: u16,
    /// When the live last block received a delta but its render caches
    /// were deliberately left stale. `Some(t)` means "a re-parse has
    /// been due since `t`"; the cache readers serve the stale lines
    /// until `t` ages past [`STREAM_REPARSE_THROTTLE`], then force a
    /// miss so the renderer rebuilds. Cleared when a rebuild stores
    /// fresh lines and when the stream finishes.
    stream_dirty_since: Option<Instant>,
    /// Test-only override of the scrollback caps, so cap tests drive
    /// compaction at kilobyte scale. `None` in production, where
    /// [`MAX_BYTES`] and [`MAX_BLOCKS`] always bind.
    #[cfg(test)]
    cap_overrides: Option<(usize, usize)>,
    /// Counts appended blocks and streamed deltas.
    output_serial: u64,
    /// [`Self::output_serial`] when the view stopped following the
    /// bottom. A higher serial since means output arrived out of
    /// sight. `None` while following.
    detached_at: Option<u64>,
    /// Screen rectangle of the `new output below` mark in the last
    /// painted frame as `(x, y, width)`; height is always one row.
    /// The renderer rewrites it each frame; mouse handlers hit-test
    /// clicks against it. `None` when no mark was painted.
    last_mark_hit: Option<(u16, u16, u16)>,
}

/// What a [`ToolTopology`] was built from: `(epoch, block count, fold
/// generation)`.
type TopologyKey = (u64, usize, u64);

/// Minimum spacing between full markdown re-parses of a streaming
/// block. Deltas inside the window update `text` but keep serving the
/// previous render from cache, bounding re-parse cost to one build
/// per window instead of one per delta (which was quadratic over the
/// stream length).
const STREAM_REPARSE_THROTTLE: Duration = Duration::from_millis(50);

/// Maximum blocks kept in the conversation buffer. Beyond this, the
/// oldest blocks are compacted away at draw time so an all-day
/// session can't grow memory (blocks, text, and render caches)
/// without bound. Chosen to stay far above what a focused work
/// session produces while keeping the per-frame block walk cheap.
pub(crate) const MAX_BLOCKS: usize = 512;

/// Maximum kept text across every block, in bytes. Beyond this,
/// [`Buffer::trim_scrollback`] compacts the oldest blocks so huge
/// streamed text and tool outputs cannot hold hundreds of megabytes
/// between paints. Chosen above the worst case of [`MAX_BLOCKS`]
/// blocks at their push-time output truncation (about 41 MB), so the
/// count cap stays the binding one in ordinary sessions and the byte
/// cap only binds when text genuinely piles up.
const MAX_BYTES: usize = 48 * 1024 * 1024;

mod edit;
mod view;

#[cfg(test)]
mod tests;
