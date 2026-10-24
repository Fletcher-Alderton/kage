//! Conversation buffer rendering and cell capture.

use kage_core::sync::read;

use crate::buffer::gap_between;

use super::*;

#[expect(
    clippy::too_many_lines,
    reason = "measure, scroll and paint the visible blocks in one pass"
)]
pub(super) fn render_buffer(
    frame: &mut Frame,
    regions: Regions,
    buffer: &mut Buffer,
    search_pattern: Option<&str>,
    search_match_set: Option<&[usize]>,
) {
    let width = regions.buffer.width;
    let visible = usize::from(regions.buffer.height);
    if width == 0 || visible == 0 {
        return;
    }

    // The opaque base canvas is painted once for the whole frame in
    // `render`; blocks tint their own rows on top of it.

    // The call/result pairing and read groups are cached on the
    // buffer and rebuilt only when the block list or a fold changes,
    // so steady-state frames share one `Arc` instead of cloning every
    // call id twice per frame.
    let topology = buffer.tool_topology();
    let n = buffer.blocks().len();
    // Display-space match indices, precomputed once per frame so the
    // per-block lookup is a binary search instead of a full scan of
    // the match set.
    let mut display_matches: Vec<usize> = search_match_set
        .unwrap_or(&[])
        .iter()
        .map(|&m| topology.display_idx(m))
        .collect();
    display_matches.sort_unstable();
    display_matches.dedup();

    let registry = read(registry::global());
    // Emphasis follows the explicit focus only: the fallback target of
    // fold gestures paints no rule.
    let explicit = buffer.focus();
    let focus = explicit.map(|f| topology.display_idx(f));
    let user_moved = explicit != buffer.last_user_focus();
    buffer.set_last_user_focus(explicit);

    let mut heights: Vec<usize> = Vec::with_capacity(n);
    let mut tops: Vec<usize> = Vec::with_capacity(n);
    let mut total_rows = 0usize;
    let mut above: Option<usize> = None;
    for idx in 0..n {
        if topology.is_hidden(idx) {
            heights.push(0);
            tops.push(total_rows);
            continue;
        }
        if let Some(prev) = above {
            let blocks = buffer.blocks();
            total_rows = total_rows.saturating_add(gap_between(&blocks[prev], &blocks[idx]));
        }
        above = Some(idx);
        let h = if let Some(cached) = buffer.cached_height(idx, width) {
            usize::from(cached)
        } else {
            let block_lines = build_block_lines(
                buffer,
                idx,
                width,
                &topology,
                Emphasis::None,
                &registry,
                Some(0),
            );
            let measured = block_lines.len();
            let stored = u16::try_from(measured).unwrap_or(u16::MAX);
            buffer.set_cached_height(idx, width, stored);
            measured
        };
        heights.push(h);
        tops.push(total_rows);
        total_rows = total_rows.saturating_add(h);
    }

    let max_scroll_back = total_rows.saturating_sub(visible);

    if focus != buffer.last_drawn_focus() {
        if let Some(old) = buffer.last_drawn_focus() {
            buffer.invalidate_height(old);
        }
        if let Some(new) = focus {
            buffer.invalidate_height(new);
        }
    }

    // Scroll a just-moved focus into view. Keyed on the *explicit*
    // focus: the effective fallback changes every time a block lands,
    // and following that drift would yank a pinned viewport on each
    // streaming append. `user_moved` makes the scroll one-shot instead
    // of re-firing every frame while focus rests on a block the user
    // deliberately scrolled away.
    if user_moved
        && let Some(di) = focus
        && let (Some(&rendered_height), Some(&rendered_start)) = (heights.get(di), tops.get(di))
    {
        // A focus index past `heights` (host reset shrank the
        // buffer between frames) skips the follow-scroll
        // instead of panicking the render.
        let rendered_end = rendered_start.saturating_add(rendered_height);
        // Absolute anchors: the viewport can show the block's top
        // row at the viewport top (`rendered_start`) or its bottom
        // row at the viewport bottom (`end - visible`); anything
        // between keeps the whole block on screen.
        let top_align = rendered_start;
        let bottom_align = rendered_end.saturating_sub(visible);
        let current = buffer
            .scroll()
            .map_or(max_scroll_back, |top| top.min(max_scroll_back));
        let in_view = current >= bottom_align && current <= top_align;
        if !in_view {
            // Tall blocks always align their top; otherwise bring
            // the nearest edge into view.
            let target = if rendered_height > visible || current < bottom_align {
                top_align
            } else {
                bottom_align
            };
            buffer.set_scroll(target.min(max_scroll_back));
        }
    }

    // Apply the scroll anchor. `None` follows the bottom; `Some(top)`
    // pins the viewport at absolute virtual row `top`, clamped to the
    // real total (only the renderer knows wrapped row counts). A pin
    // that clamps to the bottom row re-arms follow, so scrolling down
    // to the bottom resumes tracking new content.
    let visible_top = match buffer.scroll() {
        None => total_rows.saturating_sub(visible),
        Some(top) => {
            let clamped = top.min(max_scroll_back);
            if clamped < top {
                buffer.set_scroll(clamped);
            }
            if clamped == max_scroll_back {
                buffer.follow();
                total_rows.saturating_sub(visible)
            } else {
                clamped
            }
        }
    };
    let visible_bot = visible_top.saturating_add(visible);

    let mut emitted_lines: Vec<Line<'static>> = Vec::new();
    let mut next_row = visible_top;
    let mut paragraph_scroll = 0u16;
    let mut emitted_rows = 0usize;
    // Stop once we've covered the viewport plus a small margin for
    // wrap surprises. Skipping ahead saves the per-line clone cost
    // for huge unfolded blocks during fast scrolling.
    let row_budget = visible.saturating_add(32);
    // (block_idx, virtual_top, virtual_bottom) collected during this
    // pass; converted to absolute terminal rows below so mouse
    // handlers can translate a click row into a block.
    let mut block_layout: Vec<(usize, usize, usize)> = Vec::new();
    for (idx, h) in heights.iter().copied().enumerate() {
        if topology.is_hidden(idx) {
            continue;
        }
        let block_top = tops[idx];
        let block_bot = block_top.saturating_add(h);

        if block_bot <= visible_top {
            continue;
        }
        if block_top >= visible_bot {
            break;
        }
        if emitted_rows >= row_budget {
            break;
        }
        for _ in next_row..block_top {
            emitted_lines.push(Line::raw(""));
            emitted_rows = emitted_rows.saturating_add(1);
        }
        let intra_block_skip = next_row.saturating_sub(block_top);
        let emp = emphasis_for(idx, focus, &display_matches);
        let cached_owner;
        let built_owner;
        let take_rows = row_budget.saturating_sub(emitted_rows);
        let block_lines: &[Line<'static>] = if let Some(cached) =
            buffer.cached_render_lines(idx, width)
        {
            cached_owner = cached;
            cached_owner.as_slice()
        } else {
            let budget = Some(intra_block_skip.saturating_add(take_rows));
            let built = build_block_lines(buffer, idx, width, &topology, emp, &registry, budget);
            let measured = built.len();
            let stored = u16::try_from(measured).unwrap_or(u16::MAX);
            buffer.set_cached_height(idx, width, stored);
            buffer.set_cached_render_lines(idx, width, std::sync::Arc::new(built.clone()));
            built_owner = built;
            built_owner.as_slice()
        };
        let (sliced, slice_offset) =
            slice_lines_for_window(block_lines, width, intra_block_skip, take_rows);
        // The first emitted block sets the paragraph-level scroll;
        // subsequent blocks always slice from row 0 so no further
        // adjustment is needed.
        if emitted_lines.is_empty() {
            paragraph_scroll = slice_offset;
        }
        let sliced_rows = sliced.len();
        emitted_lines.extend(sliced);
        emitted_rows = emitted_rows.saturating_add(sliced_rows);
        block_layout.push((idx, block_top, block_bot));
        next_row = block_bot;
    }

    if let Some(pattern) = search_pattern {
        highlight_matches_in_lines(&mut emitted_lines, pattern);
    }

    // Translate the virtual layout into absolute terminal rows, clamped
    // to the buffer area. Skip blocks fully outside the area (defensive;
    // pass 2 already filtered them, but the screen clamp is what
    // matters for mouse hit-testing).
    let area = regions.buffer;
    let area_y = area.y;
    let area_bottom = area.y.saturating_add(area.height);
    let screen_rows: Vec<(usize, u16, u16)> = block_layout
        .iter()
        .filter_map(|&(idx, vtop, vbot)| {
            let virt_view_top = vtop.saturating_sub(visible_top);
            let virt_view_bot = vbot.saturating_sub(visible_top);
            let top = area_y.saturating_add(u16::try_from(virt_view_top).unwrap_or(u16::MAX));
            let bot = area_y.saturating_add(u16::try_from(virt_view_bot).unwrap_or(u16::MAX));
            let top = top.min(area_bottom);
            let bot = bot.min(area_bottom);
            (top < bot).then_some((idx, top, bot))
        })
        .collect();
    buffer.set_last_block_screen_rows(screen_rows);
    // Unclamped virtual spans (same coordinate space as mouse
    // selection rows) so yank can map a selected row to its source
    // line even when the block is scrolled past its own top/bottom.
    buffer.set_last_block_virtual_rows(block_layout);
    buffer.set_last_area_geometry(area.x, area.y, area.width, area.height, visible_top);

    let paragraph = Paragraph::new(emitted_lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph.scroll((paragraph_scroll, 0)), regions.buffer);
    buffer.set_last_drawn_focus(focus);
    if buffer.has_unseen_output() {
        render_new_output_mark(frame, buffer, regions.buffer);
    } else {
        buffer.set_last_mark_hit(None);
    }
}

/// Paint a `new output below` mark on the last row of `area`, against
/// the right edge, and record its screen rectangle for mouse
/// hit-testing.
fn render_new_output_mark(frame: &mut Frame, buffer: &mut Buffer, area: Rect) {
    const MARK: &str = " \u{2193} new output below ";
    let width = u16::try_from(MARK.width()).unwrap_or(u16::MAX);
    if area.height == 0 || area.width < width + 2 {
        buffer.set_last_mark_hit(None);
        return;
    }
    let theme = crate::theme::current();
    let style = Style::default()
        .fg(theme.input_pill_insert_fg)
        .bg(theme.input_pill_insert_bg)
        .add_modifier(DECORATION_MARKER);
    let x = area.right() - width - 2;
    let y = area.bottom() - 1;
    frame.buffer_mut().set_string(x, y, MARK, style);
    buffer.set_last_mark_hit(Some((x, y, width)));
}

/// Compute the emphasis for the displayed block at `idx`. `focus` is
/// already mapped to its displayed block; `display_matches` holds the
/// search hits mapped to the blocks that paint them, sorted.
fn emphasis_for(idx: usize, focus: Option<usize>, display_matches: &[usize]) -> Emphasis {
    if focus == Some(idx) {
        Emphasis::Focused
    } else if display_matches.binary_search(&idx).is_ok() {
        Emphasis::Match
    } else {
        Emphasis::None
    }
}

/// One captured cell from the rendered frame: the char that was
/// painted plus whether the renderer marked it as decoration. The
/// host's yank path filters out non-selectable cells using this
/// flag so clipboard text matches what the user logically owns.
#[derive(Clone, Copy, Debug)]
pub struct CapturedCell {
    /// First `char` of the cell's symbol. Multi-`char` graphemes
    /// collapse to their first; OK for clipboard-style yank where
    /// exact display width doesn't matter.
    pub ch: char,
    /// `true` if the renderer tagged this cell as chrome via
    /// `DECORATION_MARKER`.
    pub decoration: bool,
}

/// Walk the buffer area's cell rows once: store each visible row's
/// captured cells into `captured_rows` keyed by virtual row, and
/// apply the selection bg overlay for rows in the selection range.
/// Skips cells the renderer flagged with [`DECORATION_MARKER`] so
/// the overlay doesn't bleed onto borders/padding and yank doesn't
/// pull chrome into the clipboard. Stays in virtual-row space so
/// selection survives subsequent scrolls.
pub(crate) fn capture_and_overlay(
    frame: &mut Frame,
    regions: Regions,
    buffer: &Buffer,
    selection: Option<((usize, u16), (usize, u16))>,
    captured_rows: &mut std::collections::BTreeMap<usize, Vec<CapturedCell>>,
) {
    let area = regions.buffer;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let virtual_top = buffer.last_virtual_top();
    let buf = frame.buffer_mut();
    let (start, end) = match selection {
        Some((a, c)) if a <= c => (Some(a), Some(c)),
        Some((a, c)) => (Some(c), Some(a)),
        None => (None, None),
    };

    let has_selection = start.is_some() && end.is_some();
    if has_selection {
        let theme = crate::theme::current();
        let highlight_bg = theme.selection_color;
        let on_select_fg = theme.selection_fg;
        let last_col = area.x.saturating_add(area.width).saturating_sub(1);
        let (s, e) = (start.unwrap(), end.unwrap());
        for screen_row in area.y..area.y.saturating_add(area.height) {
            let vrow = virtual_top.saturating_add(usize::from(screen_row - area.y));
            let mut row_cells: Vec<CapturedCell> = Vec::with_capacity(usize::from(area.width));
            for col in area.x..area.x.saturating_add(area.width) {
                let cell = &mut buf[(col, screen_row)];
                let ch = cell.symbol().chars().next().unwrap_or(' ');
                let decoration = cell_is_decoration(cell.modifier);
                row_cells.push(CapturedCell { ch, decoration });
            }
            if vrow >= s.0 && vrow <= e.0 {
                let last_real_local = row_cells
                    .iter()
                    .rposition(|cell| !cell.decoration && cell.ch != ' ');
                if let Some(last_real_idx) = last_real_local {
                    let last_real_col = area
                        .x
                        .saturating_add(u16::try_from(last_real_idx).unwrap_or(u16::MAX));
                    let from_col = if vrow == s.0 { s.1 } else { area.x };
                    let to_col = if vrow == e.0 { e.1 } else { last_col };
                    let lo = from_col.max(area.x);
                    let hi = to_col.min(last_col).min(last_real_col);
                    if hi >= lo {
                        for col in lo..=hi {
                            let local_idx = usize::from(col - area.x);
                            if let Some(cell_meta) = row_cells.get(local_idx)
                                && cell_meta.decoration
                            {
                                continue;
                            }
                            let cell = &mut buf[(col, screen_row)];
                            cell.set_bg(highlight_bg);
                            cell.set_fg(on_select_fg);
                        }
                    }
                }
            }
            captured_rows.insert(vrow, row_cells);
        }
    }
}

/// Approximate wrapped-row count for one [`Line`] at `width`, used
/// by [`slice_lines_for_window`] to walk a cached vector of lines and
/// land on the slice that intersects the visible window. Sums display
/// width, so the estimate tracks how ratatui actually lays cells out;
/// only word-wrap break points make it approximate.
fn wrap_rows(line: &Line<'_>, width: usize) -> usize {
    let cols: usize = line.spans.iter().map(|s| s.content.width()).sum();
    if cols == 0 {
        1
    } else {
        cols.div_ceil(width).max(1)
    }
}

/// Walk `lines` accumulating wrap rows; produce the smallest
/// contiguous prefix that covers `[skip_rows, skip_rows + take_rows]`
/// and a Paragraph-level scroll offset to land the viewport on the
/// right row of the first emitted line. Used in pass 2 to avoid
/// cloning every line of a giant unfolded block when only the
/// viewport is actually visible.
fn slice_lines_for_window(
    lines: &[Line<'static>],
    width: u16,
    skip_rows: usize,
    take_rows: usize,
) -> (Vec<Line<'static>>, u16) {
    let usable = usize::from(width).max(1);
    let mut rows_seen = 0usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut paragraph_offset = 0u16;
    let mut started = false;
    for line in lines {
        let lh = wrap_rows(line, usable);
        if started {
            out.push(line.clone());
        } else if rows_seen.saturating_add(lh) > skip_rows {
            started = true;
            paragraph_offset =
                u16::try_from(skip_rows.saturating_sub(rows_seen)).unwrap_or(u16::MAX);
            out.push(line.clone());
        }
        rows_seen = rows_seen.saturating_add(lh);
        if rows_seen >= skip_rows.saturating_add(take_rows).saturating_add(16) {
            break;
        }
    }
    (out, paragraph_offset)
}

/// Build the rendered lines for the block at `idx`, automatically
/// merging a `ToolCall` with its paired `ToolResult` when one exists
/// and painting an `Explored` group from its head. Callers pass the
/// cached topology (so lookups stay cheap inside the render loop) and
/// the emphasis state for this idx.
///
/// Routes through the [`registry::BlockRenderer`] so block rendering
/// goes through the same widget dispatch plugins hook into via
/// `set_builtin` / `set_custom`.
pub(crate) fn build_block_lines(
    buffer: &Buffer,
    idx: usize,
    width: u16,
    topology: &crate::buffer::ToolTopology,
    emphasis: Emphasis,
    registry: &registry::BlockRenderer,
    row_budget: Option<usize>,
) -> Vec<Line<'static>> {
    let blocks = buffer.blocks();
    let cur = &blocks[idx];
    let theme = crate::theme::current();
    let ctx = widget::RenderCtx {
        theme: &theme,
        focused: emphasis == Emphasis::Focused,
        emphasis,
        selection: None,
        search_pattern: None,
        row_budget,
    };
    if let Some(members) = topology.groups.get(&idx) {
        let calls: Vec<&Block> = members.iter().map(|&m| blocks[m].as_ref()).collect();
        return tool_group_lines(&calls, width, emphasis);
    }
    if let Block::ToolCall { .. } = cur.as_ref()
        && let Some(&result_idx) = topology.result_of_call.get(&idx)
        && let Some(w) = registry.pair_widget_for(cur, &blocks[result_idx])
    {
        return w.lines(width, &ctx);
    }
    if let Some(w) = registry.widget_for(cur) {
        return w.lines(width, &ctx);
    }
    Vec::new()
}
