//! Buffer read-side: scroll, focus, cache geometry, search, folding queries.

use super::*;

impl Buffer {
    /// Construct an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read-only view of the blocks. Each block is shared, so cloning
    /// the handle is cheap and the payload is read through its
    /// [`Deref`](std::ops::Deref) to [`Block`].
    #[must_use]
    pub fn blocks(&self) -> &[Arc<Block>] {
        &self.blocks
    }

    /// Total logical lines summed across all blocks.
    #[must_use]
    pub fn total_lines(&self) -> usize {
        self.blocks.iter().map(|b| b.line_count()).sum()
    }

    /// Absolute virtual row of the viewport's first visible row while
    /// scrolled up; `None` means pinned to the bottom (follow newest).
    #[must_use]
    pub fn scroll(&self) -> Option<usize> {
        self.scroll
    }

    /// True when the viewport is pinned to the bottom; auto-follows
    /// streaming content.
    #[must_use]
    pub fn is_following(&self) -> bool {
        self.scroll.is_none()
    }

    /// Pin the viewport so its first visible row is virtual row
    /// `scroll`, detaching from the bottom. No model-layer cap is
    /// applied because the model doesn't know about line wrapping: a
    /// logical line may render as multiple visual rows once Paragraph
    /// wraps it. The renderer holds the authoritative max each frame
    /// and clamps there.
    pub fn set_scroll(&mut self, scroll: usize) {
        if self.scroll != Some(scroll) {
            self.scroll = Some(scroll);
            self.bump_version();
        }
    }

    /// Re-arm bottom-following: the viewport tracks the newest row as
    /// content streams in.
    pub fn follow(&mut self) {
        if self.scroll.is_some() {
            self.scroll = None;
            self.bump_version();
        }
    }

    /// Currently focused foldable block index, if the user has
    /// explicitly selected one. Renderers that highlight a focused
    /// block should fall back to [`Self::effective_focus`] for
    /// "no selection but show something".
    #[must_use]
    pub fn focus(&self) -> Option<usize> {
        self.focus
    }

    /// Effective focus: the explicit user selection if any, otherwise
    /// the index of the last selectable block in the buffer. `None`
    /// when there are no selectable blocks at all.
    #[must_use]
    pub fn effective_focus(&self) -> Option<usize> {
        self.focus.or_else(|| self.last_selectable_index())
    }

    /// What focus value the renderer painted last frame. The renderer
    /// compares this to the focus it paints each frame; when they
    /// differ, it invalidates the moved blocks' caches so emphasis
    /// repaints.
    #[must_use]
    pub fn last_drawn_focus(&self) -> Option<usize> {
        self.last_drawn_focus
    }

    /// Renderer hook: stash the focus value used while painting this
    /// frame so the next frame can compare and react.
    pub fn set_last_drawn_focus(&mut self, value: Option<usize>) {
        self.last_drawn_focus = value;
    }

    /// The explicit focus the renderer last observed. Scroll-into-view
    /// keys on changes to this value, so streaming appends (which move
    /// the effective-focus fallback) never yank a pinned viewport.
    #[must_use]
    pub fn last_user_focus(&self) -> Option<usize> {
        self.last_user_focus
    }

    /// Renderer hook: record the explicit focus value seen this frame
    /// so the next frame can tell a user move from fallback drift.
    pub fn set_last_user_focus(&mut self, value: Option<usize>) {
        self.last_user_focus = value;
    }

    /// Whether the cached height/lines for block `idx` may be served
    /// even though the live last block has unparsed streaming edits.
    /// False in exactly one case: `idx` is the last block and a
    /// re-parse has been pending longer than
    /// [`STREAM_REPARSE_THROTTLE`], which forces the renderer to
    /// rebuild.
    fn stream_cache_usable(&self, idx: usize) -> bool {
        if idx + 1 != self.blocks.len() {
            return true;
        }
        self.stream_dirty_since
            .is_none_or(|t| t.elapsed() < STREAM_REPARSE_THROTTLE)
    }

    /// True while the live last block has edits whose render is still
    /// pending (inside the throttle window). The search-match cache
    /// uses this to skip its full-text rescan on streaming deltas.
    pub(crate) fn stream_edits_pending(&self) -> bool {
        self.stream_dirty_since.is_some()
    }

    /// When the pending streaming edits are due to be re-parsed, so a
    /// frame drawn from the stale render can be followed by one that
    /// shows them. `None` with nothing pending.
    pub(crate) fn stream_reparse_at(&self) -> Option<Instant> {
        self.stream_dirty_since
            .map(|since| since + STREAM_REPARSE_THROTTLE)
    }

    /// Cached call/result block pairing and `Explored` grouping for
    /// the current block list, rebuilt here when the block count, the
    /// structural epoch or the fold generation changed since it was
    /// last built. A rebuild drops the render caches of every group
    /// head whose members changed. The returned handle is shared, so
    /// this runs at most once per change rather than per frame.
    pub(crate) fn tool_topology(&mut self) -> Arc<ToolTopology> {
        let key = self.topology_key();
        if let Some((built, topo)) = &self.tool_topology
            && *built == key
        {
            return Arc::clone(topo);
        }
        let topo = Arc::new(ToolTopology::build(&self.blocks));
        let stale_heads: Vec<usize> = match &self.tool_topology {
            Some(((epoch, ..), old)) if *epoch == self.epoch => old
                .groups
                .keys()
                .chain(topo.groups.keys())
                .filter(|head| old.groups.get(head) != topo.groups.get(head))
                .copied()
                .collect(),
            _ => topo.groups.keys().copied().collect(),
        };
        for head in stale_heads {
            self.clear_caches_at(head);
        }
        self.tool_topology = Some((key, Arc::clone(&topo)));
        topo
    }

    fn topology_key(&self) -> TopologyKey {
        (self.epoch, self.blocks.len(), self.fold_generation)
    }

    /// Whether block `idx` is a call folded into an `Explored` group
    /// under another head, per the cached topology. A stale cache
    /// answers `false`.
    fn is_grouped_member(&self, idx: usize) -> bool {
        self.tool_topology.as_ref().is_some_and(|(key, topo)| {
            *key == self.topology_key() && topo.head_of_member.contains_key(&idx)
        })
    }

    /// Whether block `idx` paints a ticking timer and so must be
    /// rendered fresh each frame. The render caches never store such a
    /// block; every change that starts a timer drops its caches.
    #[must_use]
    pub fn is_timed(&self, idx: usize) -> bool {
        self.blocks.get(idx).is_some_and(|b| b.is_timed())
    }

    /// Whether any tool call is still in flight: streaming its
    /// arguments, queued, waiting for approval, or running.
    #[must_use]
    pub fn has_running_tool_call(&self) -> bool {
        self.blocks.iter().any(|b| {
            matches!(
                b.as_ref(),
                Block::ToolCall {
                    phase: ToolPhase::Streaming
                        | ToolPhase::Queued
                        | ToolPhase::Waiting
                        | ToolPhase::Approved
                        | ToolPhase::Running,
                    ..
                }
            )
        })
    }

    /// Message-level jump targets for the F3 picker: `(block_idx,
    /// label)` pairs, one per user prompt, assistant reply, tool
    /// call, and notice block. Labels are single-line summaries
    /// without markdown emphasis, truncated to `label_width`
    /// characters. Thinking blocks, standalone results, and help and
    /// status notices are skipped.
    #[must_use]
    pub fn jump_targets(&self, label_width: usize) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        for (idx, block) in self.blocks.iter().enumerate() {
            let label = match block.as_ref() {
                Block::User { text } => Some(format!("you: {}", plain_first_line(text))),
                Block::Custom { kind, .. }
                    if matches!(kind.as_str(), "kage:help" | "kage:notify") =>
                {
                    None
                }
                Block::Assistant { text, .. } | Block::Custom { text, .. } => {
                    Some(plain_first_line(text))
                }
                Block::ToolCall { name, input, .. } => {
                    let label = crate::view::tool_view::describe(name, input);
                    Some(format!("{} {}", label.verb_done, label.target))
                }
                Block::Thinking { .. } | Block::ToolResult { .. } => None,
            };
            if let Some(label) = label
                .as_deref()
                .and_then(|l| truncate_label(l, label_width))
            {
                out.push((idx, label));
            }
        }
        out
    }

    /// Cached rendered height (in wrapped rows) for the block at
    /// `idx`, but only if the cache entry was captured at the given
    /// `width`. Width-mismatched entries return `None` so the caller
    /// recomputes and stores a fresh value. Out-of-range indices and
    /// uncached blocks also return `None`.
    #[must_use]
    pub fn cached_height(&self, idx: usize, width: u16) -> Option<u16> {
        if !self.stream_cache_usable(idx) {
            return None;
        }
        self.block_heights
            .get(idx)
            .copied()
            .flatten()
            .and_then(|(w, h)| (w == width).then_some(h))
    }

    /// Renderer hook: store the wrapped-row height it just measured
    /// for the block at `idx` at the given `width`. Subsequent frames
    /// reuse this without rebuilding the block's [`Line`]s. A timed
    /// block is never stored, so it rebuilds every frame. Measuring the
    /// live last block consumes its pending streaming edits, so its
    /// cached lines, built before them, are dropped too.
    pub fn set_cached_height(&mut self, idx: usize, width: u16, height: u16) {
        if idx + 1 == self.blocks.len()
            && self.stream_dirty_since.take().is_some()
            && let Some(slot) = self.block_render_lines.get_mut(idx)
        {
            *slot = None;
        }
        if self.is_timed(idx) {
            return;
        }
        if let Some(slot) = self.block_heights.get_mut(idx) {
            *slot = Some((width, height));
        }
    }

    /// Drop every cached height and rendered line, for a width or
    /// palette change. Bumps the version so the renderer does not
    /// reuse a snapshot that still holds the old caches.
    pub fn invalidate_all_heights(&mut self) {
        for slot in &mut self.block_heights {
            *slot = None;
        }
        for slot in &mut self.block_render_lines {
            *slot = None;
        }
        self.bump_version();
    }

    /// Cached rendered lines for the block at `idx`, but only if the
    /// cache entry was captured at the given `width`. The lines were
    /// rendered with `Emphasis::None`; callers that need a focused or
    /// selection-emphasised render must rebuild.
    #[must_use]
    pub fn cached_render_lines(&self, idx: usize, width: u16) -> Option<Arc<Vec<Line<'static>>>> {
        if !self.stream_cache_usable(idx) {
            return None;
        }
        self.block_render_lines
            .get(idx)
            .and_then(Clone::clone)
            .and_then(|(w, lines)| (w == width).then_some(lines))
    }

    /// Renderer hook: store the rendered lines it just built for the
    /// block at `idx`, paired with the width used. Held behind `Arc`
    /// so the renderer's emit pass can clone the handle without
    /// duplicating the line vector. A timed block is never stored.
    pub fn set_cached_render_lines(
        &mut self,
        idx: usize,
        width: u16,
        lines: Arc<Vec<Line<'static>>>,
    ) {
        if idx + 1 == self.blocks.len() {
            self.stream_dirty_since = None;
        }
        if self.is_timed(idx) {
            return;
        }
        if let Some(slot) = self.block_render_lines.get_mut(idx) {
            *slot = Some((width, lines));
        }
    }

    /// Renderer hook: replace the absolute screen-row layout it just
    /// painted. The vec is sorted by `screen_top`; entries don't
    /// overlap. Used by mouse handlers to translate a click into a
    /// block index.
    pub fn set_last_block_screen_rows(&mut self, rows: Vec<(usize, u16, u16)>) {
        self.last_block_screen_rows = rows;
    }

    /// Renderer hook: stash each painted block's unclamped
    /// `(idx, virtual_top, virtual_bottom)` for this frame. See
    /// `Self::last_block_virtual_rows`.
    pub fn set_last_block_virtual_rows(&mut self, rows: Vec<(usize, usize, usize)>) {
        self.last_block_virtual_rows = rows;
    }

    /// Renderer hook: record where this frame painted the
    /// `new output below` mark, or `None` when it painted none. See
    /// [`Self::mark_hit_at`].
    pub fn set_last_mark_hit(&mut self, hit: Option<(u16, u16, u16)>) {
        self.last_mark_hit = hit;
    }

    /// Whether `(row, col)` lands on the `new output below` mark the
    /// last frame painted. `false` when the frame painted no mark.
    #[must_use]
    pub fn mark_hit_at(&self, row: u16, col: u16) -> bool {
        self.last_mark_hit
            .is_some_and(|(x, y, width)| row == y && col >= x && col < x.saturating_add(width))
    }

    /// Unclamped `(virtual_top, virtual_bottom)` of the block at
    /// `idx` from the last frame, `bottom` exclusive. `None` when the
    /// block was not painted. Used by yank to map a selected row to
    /// the block's source line correctly under any scroll.
    #[must_use]
    pub fn block_virtual_rows(&self, idx: usize) -> Option<(usize, usize)> {
        self.last_block_virtual_rows
            .iter()
            .find_map(|(i, top, bot)| (*i == idx).then_some((*top, *bot)))
    }

    /// Renderer hook: stash the buffer area's bounding box and the
    /// virtual-row index of its first visible row. Mouse handlers
    /// add `screen_row - area_y` to `last_virtual_top` to recover a
    /// stable virtual-row coordinate that survives subsequent
    /// scrolls; the renderer reverses that to project a virtual row
    /// back to a screen row when painting selection overlay.
    pub fn set_last_area_geometry(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        virtual_top: usize,
    ) {
        self.last_area_x = x;
        self.last_area_y = y;
        self.last_area_width = width;
        self.last_area_height = height;
        self.last_virtual_top = virtual_top;
    }

    /// Merge the renderer-owned state of a snapshot back into this
    /// buffer after an out-of-lock render: per-block height and line
    /// caches, the clamped scroll, the last-drawn and last-seen focus
    /// echoes, and the last-frame geometry tables mouse handlers read.
    ///
    /// Block content is never merged: blocks appended while the
    /// snapshot was being drawn stay in place with their (empty)
    /// cache entries intact. The merge is skipped entirely when the
    /// live buffer has fewer blocks than the snapshot or went through
    /// a structural change (clear, take, compaction) since the
    /// snapshot was taken, because stale cache indices would
    /// mislabel. (Structural mutations are UI-thread only, so they
    /// cannot overlap a draw on that same thread; the guard is
    /// defensive.)
    pub fn merge_render_state(&mut self, snapshot: &Self) {
        if snapshot.epoch != self.epoch || snapshot.blocks.len() > self.blocks.len() {
            return;
        }
        for (slot, entry) in self
            .block_heights
            .iter_mut()
            .zip(snapshot.block_heights.iter())
        {
            *slot = *entry;
        }
        for (slot, entry) in self
            .block_render_lines
            .iter_mut()
            .zip(snapshot.block_render_lines.iter())
        {
            slot.clone_from(entry);
        }
        self.scroll = snapshot.scroll;
        self.last_drawn_focus = snapshot.last_drawn_focus;
        self.last_user_focus = snapshot.last_user_focus;
        self.last_block_screen_rows
            .clone_from(&snapshot.last_block_screen_rows);
        self.last_block_virtual_rows
            .clone_from(&snapshot.last_block_virtual_rows);
        self.last_area_x = snapshot.last_area_x;
        self.last_area_y = snapshot.last_area_y;
        self.last_area_width = snapshot.last_area_width;
        self.last_area_height = snapshot.last_area_height;
        self.last_virtual_top = snapshot.last_virtual_top;
        self.last_mark_hit = snapshot.last_mark_hit;
        // The dirty flag is consumed by the rebuild that produced the
        // snapshot's fresh caches; copying it back keeps the live
        // buffer from re-serving a window that already rebuilt. A
        // delta landing mid-draw re-arms on the next frame at the
        // cost of one extra window of staleness, at worst.
        self.stream_dirty_since = snapshot.stream_dirty_since;
        self.detached_at = snapshot.detached_at;
        // The pairing cache is shared, not copied: the snapshot's
        // build key travels with it so a length mismatch here
        // triggers one rebuild on the next render.
        self.tool_topology.clone_from(&snapshot.tool_topology);
    }

    /// Renderer hook: whether blocks or deltas arrived since the view
    /// stopped following the bottom. Called once per frame.
    pub(crate) fn has_unseen_output(&mut self) -> bool {
        if self.is_following() {
            self.detached_at = None;
            return false;
        }
        self.output_serial > *self.detached_at.get_or_insert(self.output_serial)
    }

    /// Width of the last-painted buffer area, in cells.
    #[must_use]
    pub fn last_area_width(&self) -> u16 {
        self.last_area_width
    }

    /// X-origin of the last-painted buffer area.
    #[must_use]
    pub fn last_area_x(&self) -> u16 {
        self.last_area_x
    }

    /// Y-origin of the last-painted buffer area.
    #[must_use]
    pub fn last_area_y(&self) -> u16 {
        self.last_area_y
    }

    /// Height of the last-painted buffer area, in cells.
    #[must_use]
    pub fn last_area_height(&self) -> u16 {
        self.last_area_height
    }

    /// Virtual-row index of the first visible row in the last
    /// painted frame.
    #[must_use]
    pub fn last_virtual_top(&self) -> usize {
        self.last_virtual_top
    }

    /// Find the block painted under absolute terminal row `y` from
    /// the most recent frame. Returns `None` when the row is outside
    /// any block (separator gap, empty buffer, off-screen) or when
    /// the renderer hasn't painted a frame yet.
    #[must_use]
    pub fn block_at_screen_row(&self, y: u16) -> Option<usize> {
        self.last_block_screen_rows
            .iter()
            .find_map(|(idx, top, bot)| (y >= *top && y < *bot).then_some(*idx))
    }

    /// Top screen row of the block at `idx` from the most recent
    /// frame. Used by mouse handlers to detect "click on header row"
    /// (whose row matches this top).
    #[must_use]
    pub fn screen_top_of(&self, idx: usize) -> Option<u16> {
        self.last_block_screen_rows
            .iter()
            .find_map(|(i, top, _)| (*i == idx).then_some(*top))
    }

    /// `(top, bottom)` screen-row range of the block at `idx` from
    /// the most recent frame, with `bottom` exclusive. `None` when
    /// the block is currently off-screen.
    #[must_use]
    pub fn screen_rows_of(&self, idx: usize) -> Option<(u16, u16)> {
        self.last_block_screen_rows
            .iter()
            .find_map(|(i, top, bot)| (*i == idx).then_some((*top, *bot)))
    }

    /// Current mutation counter. Render loops compare consecutive
    /// reads to decide if anything has changed and a repaint is
    /// warranted.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    pub(crate) fn bump_version(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    pub(crate) fn push_block_caches(&mut self) {
        self.block_heights.push(None);
        self.block_render_lines.push(None);
        self.output_serial += 1;
        self.bump_version();
    }

    pub(crate) fn invalidate_last_block_caches(&mut self) {
        if let Some(slot) = self.block_heights.last_mut() {
            *slot = None;
        }
        if let Some(slot) = self.block_render_lines.last_mut() {
            *slot = None;
        }
        self.bump_version();
    }

    pub(crate) fn clear_block_caches(&mut self) {
        self.block_heights.clear();
        self.block_render_lines.clear();
        self.bump_epoch();
        self.bump_version();
    }

    pub(crate) fn bump_epoch(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    pub(crate) fn invalidate_height(&mut self, idx: usize) {
        self.clear_caches_at(idx);
        self.bump_version();
    }

    fn clear_caches_at(&mut self, idx: usize) {
        if let Some(slot) = self.block_heights.get_mut(idx) {
            *slot = None;
        }
        if let Some(slot) = self.block_render_lines.get_mut(idx) {
            *slot = None;
        }
    }

    pub(crate) fn invalidate_pair_height(&mut self, call_id: &str) {
        let pair: Vec<usize> = self
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                matches!(b.as_ref(), Block::ToolCall { call_id: cid, .. } | Block::ToolResult { call_id: cid, .. } if cid == call_id)
            })
            .map(|(i, _)| i)
            .collect();
        for i in pair {
            self.clear_caches_at(i);
        }
    }

    /// True if block `idx` contains `needle` (ASCII case-insensitive,
    /// fallback to case-sensitive for non-ASCII). Empty needles never
    /// match.
    ///
    /// Uses byte-level matching with no allocation so the renderer
    /// can call this for every block on every frame without
    /// `to_lowercase()` blowing up on multi-MB tool outputs.
    #[must_use]
    pub fn block_contains(&self, idx: usize, needle: &str) -> bool {
        let needle = needle.trim();
        if needle.is_empty() {
            return false;
        }
        let Some(block) = self.blocks.get(idx).map(Arc::as_ref) else {
            return false;
        };
        match block {
            Block::User { text } | Block::Assistant { text, .. } | Block::Thinking { text, .. } => {
                ascii_icontains(text, needle)
            }
            Block::ToolCall {
                name,
                input_summary,
                input_pretty,
                ..
            } => {
                ascii_icontains(name, needle)
                    || ascii_icontains(input_summary, needle)
                    || ascii_icontains(input_pretty, needle)
            }
            Block::ToolResult { name, output, .. } => {
                ascii_icontains(name, needle) || ascii_icontains(output, needle)
            }
            Block::Custom { kind, text, .. } => {
                ascii_icontains(kind, needle) || ascii_icontains(text, needle)
            }
        }
    }

    /// Verbatim source text of block `idx`, for clipboard yank: the
    /// raw assistant / user / thinking / custom text (the markdown
    /// *source*, not the syntect-rendered screen cells), a tool
    /// call's pretty-printed input, or a tool result's output.
    /// `None` for an out-of-range index.
    #[must_use]
    pub fn block_text(&self, idx: usize) -> Option<String> {
        let block = self.blocks.get(idx).map(Arc::as_ref)?;
        Some(match block {
            Block::User { text }
            | Block::Assistant { text, .. }
            | Block::Thinking { text, .. }
            | Block::Custom { text, .. } => text.clone(),
            Block::ToolCall { input_pretty, .. } => input_pretty.clone(),
            Block::ToolResult { output, .. } => output.clone(),
        })
    }

    /// All block indices whose content contains `needle`, in buffer
    /// order. Skips merged tool-result halves.
    #[must_use]
    pub fn match_indices(&self, needle: &str) -> Vec<usize> {
        (0..self.blocks.len())
            .filter(|i| self.is_selectable(*i) && self.block_contains(*i, needle))
            .collect()
    }

    /// Index of the next block after `from` (exclusive) whose content
    /// contains `needle`. Skips merged tool-result halves.
    #[must_use]
    pub fn next_match(&self, from: usize, needle: &str) -> Option<usize> {
        (from + 1..self.blocks.len())
            .find(|i| self.is_selectable(*i) && self.block_contains(*i, needle))
    }

    /// Index of the previous block before `from` (exclusive) whose
    /// content contains `needle`.
    #[must_use]
    pub fn prev_match(&self, from: usize, needle: &str) -> Option<usize> {
        (0..from)
            .rev()
            .find(|i| self.is_selectable(*i) && self.block_contains(*i, needle))
    }

    /// Replace the explicit focus. `None` clears it (renderer falls
    /// back to the last selectable block). Out-of-range indices are
    /// silently dropped.
    pub fn set_focus(&mut self, idx: Option<usize>) {
        let new = idx.filter(|i| self.blocks.get(*i).is_some());
        if self.focus != new {
            self.focus = new;
            self.bump_version();
        }
    }

    /// Focus block `idx` without scrolling to it, for a click on a
    /// block that is already on screen. Out-of-range indices are
    /// dropped.
    pub fn focus_in_place(&mut self, idx: usize) {
        self.set_focus(Some(idx));
        self.last_user_focus = self.focus;
    }

    /// The block a fold gesture acts on: the explicit focus, else the
    /// last foldable block, so the first fold works while the reply
    /// that ends the conversation (which cannot fold) is last.
    #[must_use]
    pub fn fold_target(&self) -> Option<usize> {
        self.focus.or_else(|| {
            (0..self.blocks.len())
                .rev()
                .find(|&i| self.is_selectable(i) && self.blocks[i].is_foldable())
        })
    }

    /// Move focus to the previous (older) foldable block, skipping
    /// non-foldable kinds (User/Assistant). Returns `true` if focus
    /// changed.
    pub fn focus_prev(&mut self) -> bool {
        let current = self.effective_focus();
        let Some(idx) = current else { return false };
        match self.foldable_index_before(idx) {
            Some(n) if Some(n) != current => {
                self.focus = Some(n);
                self.bump_version();
                true
            }
            _ => false,
        }
    }

    /// Move focus to the next (newer) foldable block. Returns
    /// `true` if focus changed.
    pub fn focus_next(&mut self) -> bool {
        let current = self.effective_focus();
        let Some(idx) = current else { return false };
        match self.foldable_index_after(idx) {
            Some(n) if Some(n) != current => {
                self.focus = Some(n);
                self.bump_version();
                true
            }
            _ => false,
        }
    }

    /// Move focus to the previous selectable block, walking *every*
    /// kind (used by visual-mode head extension). Returns `true` if
    /// focus changed.
    pub fn focus_prev_any(&mut self) -> bool {
        let current = self.effective_focus();
        let Some(idx) = current else { return false };
        match self.selectable_index_before(idx) {
            Some(n) if Some(n) != current => {
                self.focus = Some(n);
                self.bump_version();
                true
            }
            _ => false,
        }
    }

    /// Move focus to the next selectable block, walking *every*
    /// kind. Returns `true` if focus changed.
    pub fn focus_next_any(&mut self) -> bool {
        let current = self.effective_focus();
        let Some(idx) = current else { return false };
        match self.selectable_index_after(idx) {
            Some(n) if Some(n) != current => {
                self.focus = Some(n);
                self.bump_version();
                true
            }
            _ => false,
        }
    }

    pub(crate) fn foldable_index_before(&self, idx: usize) -> Option<usize> {
        (0..idx)
            .rev()
            .find(|i| self.is_selectable(*i) && self.blocks[*i].is_foldable())
    }

    pub(crate) fn foldable_index_after(&self, idx: usize) -> Option<usize> {
        (idx + 1..self.blocks.len())
            .find(|i| self.is_selectable(*i) && self.blocks[*i].is_foldable())
    }

    /// Whether `idx` is something `[` / `]` should land on. Every
    /// block kind is selectable except a `ToolResult` merged into its
    /// call (the renderer paints the pair as one composite, so
    /// landing on the result would look like a no-op visual) and a
    /// call grouped under another `Explored` head. Both are decided
    /// by the cached topology, so a call id reused across turns only
    /// hides the one result that actually paired.
    pub(crate) fn is_selectable(&self, idx: usize) -> bool {
        match self.blocks.get(idx).map(Arc::as_ref) {
            Some(Block::ToolResult { .. }) => !self.result_consumed(idx),
            Some(Block::ToolCall { .. }) => !self.is_grouped_member(idx),
            Some(_) => true,
            None => false,
        }
    }

    /// Whether the result at `idx` merged into its call, per the
    /// cached topology. Mirrors [`ToolTopology::build`]'s one-shot
    /// pairing when the cache has not been built for the current
    /// block list yet: only the newest unpaired earlier call of the
    /// id consumes this result.
    fn result_consumed(&self, idx: usize) -> bool {
        if let Some((key, topo)) = &self.tool_topology
            && *key == self.topology_key()
        {
            return topo.consumed_results.contains(&idx);
        }
        let Some(Block::ToolResult { call_id, .. }) = self.blocks.get(idx).map(Arc::as_ref) else {
            return false;
        };
        let mut open = false;
        for block in &self.blocks[..idx] {
            match block.as_ref() {
                Block::ToolCall { call_id: cid, .. } if cid == call_id => open = true,
                Block::ToolResult { call_id: cid, .. } if cid == call_id => open = false,
                _ => {}
            }
        }
        open
    }

    pub(crate) fn last_selectable_index(&self) -> Option<usize> {
        (0..self.blocks.len())
            .rev()
            .find(|i| self.is_selectable(*i))
    }

    pub(crate) fn selectable_index_before(&self, idx: usize) -> Option<usize> {
        (0..idx).rev().find(|i| self.is_selectable(*i))
    }

    pub(crate) fn selectable_index_after(&self, idx: usize) -> Option<usize> {
        (idx + 1..self.blocks.len()).find(|i| self.is_selectable(*i))
    }
}
