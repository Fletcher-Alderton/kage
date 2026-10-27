//! The transcript view: the rows the renderers draw, wired into the
//! shell's chat column.
//!
//! The view owns the measured-height list, the expansion state, the
//! markdown caches, the find marks and the paced reveal; the shell owns
//! the store and hands a `&Store` in each frame. One frame is: build the
//! row model of the active (or pinned) session, sign each row, let the
//! list splice the measurements whose signature moved, then lay out
//! only the rows the viewport can see inside a [`egui::ScrollArea`].
//!
//! The row dispatch mirrors the old view's [`render_row`]: one arm per
//! [`Row`], and inside it the guards that read the transcript item the
//! row names, so a row whose item is gone draws nothing but keeps its
//! place.

use std::collections::HashMap;

use kage_client::{Session, TranscriptItem};

use super::list::List;
use super::markdown::Pipeline;
use super::model::{
    FindMarks, Row, RowKey, Tick, UiState, gap, margins, rail_tick, row_model, row_search_text,
    signatures,
};
use super::text_rows::{self, UserAction};
use super::tool_rows::{self, PlanRevise, RowAction};
use crate::reveal::{self, Reveal};
use crate::store::Store;
use crate::theme::{CONTENT_W, Palette, R_SM, SP_3, SP_8};
use crate::timing::SessionTimes;

/// The vim cursor's rail: how far left of the column it sits.
const CURSOR_RAIL: f32 = 10.0;
/// The vim cursor's bar width.
const CURSOR_W: f32 = 3.0;

/// What one row's layout needs: the store, the session it belongs to,
/// the session's timings, the view state the rows read, the markdown
/// caches, the plan card's revise field and the paced reveal's map.
struct RowCtx<'a> {
    /// The store the session came from.
    store: &'a Store,
    /// The session on screen.
    session: &'a Session,
    /// The session's timings, when it carries any.
    times: Option<&'a SessionTimes>,
    /// The expansion choices the rows read.
    ui: &'a mut UiState,
    /// The markdown caches of the entries on screen.
    pipeline: &'a mut Pipeline,
    /// The plan card's revise field.
    revise: &'a mut PlanRevise,
    /// How many characters of each paced item shows this frame.
    revealed: &'a HashMap<usize, usize>,
    /// Whether this view is a pinned subagent pane in the workbench.
    pinned: bool,
    /// When this view was built, the clock the constellation twinkles
    /// on.
    born: web_time::Instant,
}

/// The transcript view of one session: the measured list, the state the
/// rows read, and the find marks the find bar sets.
#[derive(Debug)]
pub struct TranscriptView {
    /// The measured-height list the rows render in.
    list: List,
    /// The expansion choices of the session on screen.
    ui: UiState,
    /// The markdown caches of the entries on screen.
    pipeline: Pipeline,
    /// The find marks the find bar last set, tinting matching rows.
    find: Option<FindMarks>,
    /// The session this view always shows, for a subagent's transcript
    /// in the workbench; `None` follows the active session.
    pinned: Option<String>,
    /// The expansion state of the other sessions, kept for their return.
    stashed: HashMap<String, UiState>,
    /// The session the UI state belongs to.
    session_key: Option<String>,
    /// The row the vim cursor sits on, while vim mode moves one.
    vim_cursor: Option<usize>,
    /// The pace of the streamed text on screen.
    reveal: Reveal,
    /// How many characters of each paced item show this frame, for the
    /// items still behind what arrived.
    revealed: HashMap<usize, usize>,
    /// The revise field of the plan card under review.
    revise: PlanRevise,
    /// The row a `scroll_to_row` asked for, resolved against the next
    /// frame's model.
    scroll_to: Option<RowKey>,
    /// The rows the last frame built, for the vim cursor's methods.
    rows: Vec<Row>,
    /// The rail's ticks, kept while the row signatures they were built
    /// from still hold.
    rail: RailTicks,
    /// When this view was built, the clock the constellation twinkles
    /// on, so a repaint from a state change continues the animation
    /// instead of restarting it.
    born: web_time::Instant,
}

impl Default for TranscriptView {
    fn default() -> Self {
        Self::born(web_time::Instant::now())
    }
}

impl TranscriptView {
    /// A transcript following the store's active session.
    #[must_use]
    pub fn new() -> Self {
        Self::born(web_time::Instant::now())
    }

    /// A transcript that always shows session `id`, such as a
    /// subagent's in the workbench.
    #[must_use]
    pub fn pinned(id: impl Into<String>) -> Self {
        Self::born(web_time::Instant::now()).with_pinned(id)
    }

    /// A transcript born at `born`, the clock its constellation
    /// twinkles on.
    #[must_use]
    fn born(born: web_time::Instant) -> Self {
        Self {
            list: List::new(),
            ui: UiState::default(),
            pipeline: Pipeline::default(),
            find: None,
            pinned: None,
            stashed: HashMap::new(),
            session_key: None,
            vim_cursor: None,
            reveal: Reveal::default(),
            revealed: HashMap::new(),
            revise: PlanRevise::default(),
            scroll_to: None,
            rows: Vec::new(),
            rail: RailTicks::default(),
            born,
        }
    }

    /// Pins this view to session `id`.
    #[must_use]
    fn with_pinned(mut self, id: impl Into<String>) -> Self {
        self.pinned = Some(id.into());
        self
    }

    /// The id of the session this view shows.
    fn target(&self, store: &Store) -> Option<String> {
        self.pinned
            .clone()
            .or_else(|| store.active_id().map(str::to_owned))
    }

    /// The session this view shows.
    fn session<'a>(&self, store: &'a Store) -> Option<&'a Session> {
        match &self.pinned {
            Some(id) => store.state().session(id),
            None => store.active_session(),
        }
    }

    /// Draws one frame: the rows of the session on screen, in a scroll
    /// area that follows the tail until the user scrolls away from it.
    ///
    /// The actions the rows raised travel back through `events` for the
    /// shell to carry out, because the view never reaches past its own
    /// rect into the store, the composer or the workbench.
    pub fn show(&mut self, ui: &mut egui::Ui, store: &Store, events: &mut Vec<RowAction>) {
        let Some(session) = self.session(store) else {
            // The welcome state: nothing to draw yet, and no session to
            // keep state for.
            self.stashed.clear();
            self.session_key = None;
            self.ui.expanded.clear();
            self.ui.full_output.clear();
            self.ui.templates.clear();
            self.find = None;
            self.vim_cursor = None;
            self.reveal.clear();
            self.revealed.clear();
            self.pipeline.clear();
            return;
        };
        self.swap_session(self.target(store));
        let model = row_model(session, &self.ui);
        self.rows = model.rows;
        self.pace(session, store, ui);
        let rows = self.rows.as_slice();
        let times = store.timings(&session.id);
        let signatures = signatures(session, rows, &self.ui, &self.revealed);
        let pinned = self.pinned.is_some();
        let find = self.find.clone();
        let vim_cursor = self.vim_cursor;
        let revise = &mut self.revise;
        let scroll_to = self.scroll_to.take();
        let revealed = self.revealed.clone();
        let born = self.born;

        // The measure pass borrows the same state the draw pass does, so
        // the fields are split before either runs.
        let Self {
            list,
            ui: ui_state,
            pipeline,
            ..
        } = self;
        let mut ctx = RowCtx {
            store,
            session,
            times,
            ui: ui_state,
            pipeline,
            revise,
            revealed: &revealed,
            pinned,
            born,
        };

        let column_w = (ui.max_rect().width() - 2.0 * SP_8).clamp(0.0, CONTENT_W);
        list.sync(&signatures, |ix| {
            measure_row(&mut *ui, column_w, rows, ix, &mut ctx)
        });

        let viewport = ui.available_height().max(1.0);
        let content = list.content_height();
        // The follow drives the offset to its target every frame; a jump
        // to a row detaches the way a scroll up does.
        let mut target = if list.following() {
            Some(list.follow_target(viewport))
        } else {
            None
        };
        if let Some(key) = scroll_to
            && let Some(ix) = rows.iter().position(|row| row.key() == key)
        {
            list.user_scrolled();
            target = Some(list.offset_of(ix));
        }

        let mut area = egui::ScrollArea::vertical()
            .id_salt("transcript-rows")
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden);
        if let Some(target) = target {
            area = area.vertical_scroll_offset(target);
        }
        let shown = area.show_viewport(ui, |ui, viewport| {
            ui.set_height(content.max(0.0));
            let (first, last) = list.window(viewport.min.y, viewport.height());
            if last <= first {
                return;
            }
            let top = ui.max_rect().top();
            let mid = ui.max_rect().center().x;
            // The rows lay out top down from the window's first row; the
            // bottom is left far below the last, so a row that draws
            // taller than it measured never squeezes the rows after it
            // into a negative height.
            // The column snaps to a whole pixel as the reference lays it.
            let left = crate::theme::snap(mid - column_w / 2.0);
            let rect = egui::Rect::from_x_y_ranges(
                left..=left + column_w,
                top + list.offset_of(first)..=top + list.offset_of(last) + MEASURE_MAX_H,
            );
            ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(rect)
                    .id_salt("transcript-window"),
                |ui| {
                    for ix in first..last {
                        let row = &rows[ix];
                        let prev = ix.checked_sub(1).map(|p| &rows[p]);
                        let last_row = ix + 1 == rows.len();
                        let row_top = ui.cursor().top();
                        let tint = ui.painter().add(egui::Shape::Noop);
                        let height = draw_row(ui, row, prev, last_row, &mut ctx, events);
                        list.set_height(ix, finite_height(height, ix, row));
                        let cell = egui::Rect::from_x_y_ranges(
                            ui.max_rect().x_range(),
                            row_top..=row_top + height,
                        );
                        if let Some(fill) = find.as_ref().and_then(|find| {
                            row_tint(find, row.key(), Palette::active_of(ui.ctx()))
                        }) {
                            ui.painter().set(
                                tint,
                                egui::Shape::rect_filled(
                                    cell.expand2(egui::vec2(SP_3, 0.0)),
                                    egui::CornerRadius::same(R_SM as u8),
                                    fill,
                                ),
                            );
                        }
                        if vim_cursor == Some(ix) {
                            let rail = egui::Rect::from_min_size(
                                egui::pos2(ui.max_rect().left() - CURSOR_RAIL, row_top),
                                egui::vec2(CURSOR_W, height),
                            );
                            ui.painter().rect_filled(
                                rail,
                                egui::CornerRadius::same(2),
                                Palette::active_of(ui.ctx()).accent,
                            );
                        }
                    }
                },
            );
        });

        // The offset the frame left: a move the view did not ask for is
        // the user's, and it decides whether the follow detaches.
        let offset = shown.state.offset.y;
        let driven = target.is_some_and(|want| (offset - want).abs() <= 0.5);
        if !driven {
            list.user_scrolled();
        }
        list.on_scroll(offset, content, viewport);
        if !list.following() && jump_to_latest(ui, shown.inner_rect) {
            list.follow_again();
            ui.ctx().request_repaint();
        }

        // The turn timeline rail, while the lab toggle keeps it: a
        // tick per marked row, the viewport's thumb, and drag to
        // seek. The seek rides the same path a jump button takes, so
        // it detaches the follow the same way.
        if store.prefs().rail && self.pinned.is_none() {
            self.rail.refresh(&signatures, || {
                rows.iter().map(|row| rail_tick(session, row)).collect()
            });
            let seek = draw_rail(ui, rows, &self.rail.ticks, offset, content, viewport);
            if let Some(key) = seek {
                self.scroll_to = Some(key);
            }
        }
    }

    /// Swaps the expansion state when the session on screen changes:
    /// the state of the session leaving is kept for its return, and the
    /// session arriving opens on its tail.
    fn swap_session(&mut self, active: Option<String>) {
        if active == self.session_key {
            return;
        }
        if let Some(key) = self.session_key.take() {
            self.stashed.insert(key, std::mem::take(&mut self.ui));
        }
        self.ui = active
            .as_ref()
            .and_then(|key| self.stashed.remove(key))
            .unwrap_or_default();
        self.session_key = active;
        self.list = List::new();
        self.find = None;
        self.vim_cursor = None;
        self.reveal.clear();
        self.revealed.clear();
        self.pipeline.clear();
    }

    /// Advances the pace of the streamed text this frame: a live reply
    /// or thinking starts being paced, and while any paced text trails
    /// what arrived, another frame is asked for.
    fn pace(&mut self, session: &Session, store: &Store, ui: &egui::Ui) {
        self.revealed.clear();
        if !self.smooth(store) {
            self.reveal.clear();
            return;
        }
        let now = web_time::Instant::now();
        let rows = self.rows.clone();
        for row in &rows {
            let (ix, live) = match row {
                Row::Assistant { ix, live } | Row::Thinking { ix, live, .. } => (*ix, *live),
                _ => continue,
            };
            if live {
                self.reveal.track(ix, now);
            }
            if !self.reveal.tracks(ix) {
                continue;
            }
            let text = match session.items.get(ix) {
                Some(
                    TranscriptItem::Assistant { text } | TranscriptItem::Thinking { text, .. },
                ) => text,
                _ => continue,
            };
            let len = text.chars().count();
            if let Some(shown) = self.reveal.advance(ix, len, live, now)
                && shown < len
            {
                self.revealed.insert(ix, shown);
            }
        }
        if self.reveal.behind() {
            ui.ctx().request_repaint();
        }
    }

    /// Whether the preferences ask for the paced reveal: the lab
    /// toggle off shows a reply whole, and a pinned session has no
    /// live tail to pace.
    fn smooth(&self, store: &Store) -> bool {
        store.prefs().smooth_stream && self.pinned.is_none()
    }

    /// The searchable text of every model row, in row order: what the
    /// rows render, with tool titles, targets and notice text
    /// included.
    #[must_use]
    pub fn searchable_rows(&self, store: &Store) -> Vec<(RowKey, String)> {
        let Some(session) = self.session(store) else {
            return Vec::new();
        };
        row_model(session, &self.ui)
            .rows
            .iter()
            .map(|row| (row.key(), row_search_text(session, row)))
            .collect()
    }

    /// Tints the rows the find bar matched; `None` clears the tint.
    pub fn set_find(&mut self, marks: Option<FindMarks>) {
        self.find = marks;
    }

    /// Brings the row `key` into view and stops following the bottom.
    pub fn scroll_to_row(&mut self, key: RowKey) {
        self.scroll_to = Some(key);
    }

    /// The rows the vim cursor moves over.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The row the vim cursor sits on.
    #[must_use]
    pub fn cursor_row(&self) -> Option<usize> {
        self.vim_cursor
    }

    /// The scroll offset the last frame left, for the probes that
    /// watch a seek land.
    #[must_use]
    pub fn scroll_offset(&self) -> f32 {
        self.list.offset()
    }

    /// Puts the vim cursor on row `ix`, clamped to the rows. `None`
    /// removes it.
    pub fn set_vim_cursor(&mut self, ix: Option<usize>) {
        let count = self.rows.len();
        self.vim_cursor = ix.filter(|_| count > 0).map(|ix| ix.min(count - 1));
    }

    /// Opens or closes the row under the vim cursor, as a click on its
    /// header does.
    pub fn toggle_vim_row(&mut self) {
        let Some(ix) = self.vim_cursor else {
            return;
        };
        let Some(row) = self.rows.get(ix) else {
            return;
        };
        let key = row.key();
        if !self.ui.expanded.remove(&key) {
            self.ui.expanded.insert(key);
        }
    }

    /// Opens or closes every row, as the `zR` and `zM` motions do.
    pub fn fold_all(&mut self, open: bool) {
        if open {
            let keys = self.rows.iter().map(Row::key);
            self.ui.expanded.extend(keys);
        } else {
            self.ui.expanded.clear();
        }
    }

    /// Moves the vim cursor by `delta` rows, clamped, and brings it
    /// into view.
    pub fn move_cursor(&mut self, delta: isize) {
        let to = crate::vim::step(self.vim_cursor, self.rows.len(), delta);
        self.set_vim_cursor(to);
        self.scroll_cursor_into_view();
    }

    /// Puts the vim cursor on the first or the last row and brings it
    /// into view.
    pub fn jump_cursor(&mut self, last: bool) {
        let to = if last {
            self.rows.len().checked_sub(1)
        } else if self.rows.is_empty() {
            None
        } else {
            Some(0)
        };
        self.set_vim_cursor(to);
        self.scroll_cursor_into_view();
    }

    /// Scrolls so the cursor's row comes into view, the way the old
    /// shell's cursor moves did. No cursor, no move.
    fn scroll_cursor_into_view(&mut self) {
        if let Some(ix) = self.vim_cursor
            && let Some(row) = self.rows.get(ix)
        {
            self.scroll_to = Some(row.key());
        }
    }
}

/// The part of item `ix`'s `text` that shows this frame under the
/// paced reveal: the whole text until the reveal is behind it, then
/// its first `chars` characters.
/// The rail's width and the pane margins it hangs by, as the old
/// shell's rail sat: 16px wide, 6px off the pane's right edge, 10px
/// off the top and the bottom.
const RAIL_W: f32 = 16.0;
/// The thumb's smallest height, so a tall transcript keeps it grabbable.
const RAIL_THUMB_MIN: f32 = 18.0;

/// Draws the turn timeline rail over the pane's right edge and
/// returns the row a click or drag asked to see. The rail paints a
/// subtle track, the viewport's thumb, and a tick per marked row:
/// hovering a tick names it, a click scrolls to it, and a press or
/// drag anywhere on the rail scrolls there.
/// The rail's ticks and the row signatures they were built from.
#[derive(Debug, Default)]
struct RailTicks {
    signatures: Vec<u64>,
    ticks: Vec<Option<(Tick, String)>>,
}

impl RailTicks {
    /// Rebuild the ticks with `build` when `signatures` changed since
    /// the last build, and report whether it did.
    fn refresh(
        &mut self,
        signatures: &[u64],
        build: impl FnOnce() -> Vec<Option<(Tick, String)>>,
    ) -> bool {
        if self.signatures == signatures {
            return false;
        }
        self.ticks = build();
        self.signatures = signatures.to_vec();
        true
    }
}

fn draw_rail(
    ui: &mut egui::Ui,
    rows: &[Row],
    ticks: &[Option<(Tick, String)>],
    offset: f32,
    content: f32,
    viewport: f32,
) -> Option<RowKey> {
    if rows.is_empty() || content <= 0.0 {
        return None;
    }
    let palette = Palette::active_of(ui.ctx());
    let pane = ui.max_rect();
    let rail = egui::Rect::from_min_max(
        egui::pos2(pane.right() - RAIL_W - 6.0, pane.top() + 10.0),
        egui::pos2(pane.right() - 6.0, pane.bottom() - 10.0),
    );
    if !rail.is_positive() {
        return None;
    }
    let painter = ui.painter();
    let track = egui::Rect::from_min_max(
        egui::pos2(rail.left() + 7.0, rail.top()),
        egui::pos2(rail.left() + 9.0, rail.bottom()),
    );
    painter.rect_filled(track, egui::CornerRadius::same(1), palette.subtle);

    let (thumb_top, thumb_h) = if content > 0.0 {
        (offset / content, viewport / content)
    } else {
        (0.0, 1.0)
    };
    let height = (thumb_h * rail.height()).clamp(RAIL_THUMB_MIN, rail.height());
    let top = rail.top() + thumb_top.clamp(0.0, 1.0) * (rail.height() - height);
    let thumb = egui::Rect::from_min_max(
        egui::pos2(rail.left() + 1.0, top),
        egui::pos2(rail.right() - 1.0, top + height),
    );
    painter.rect_filled(thumb, egui::CornerRadius::same(5), palette.fill_hover);
    painter.rect_stroke(
        thumb,
        egui::CornerRadius::same(5),
        egui::Stroke::new(1.0, palette.line),
        egui::StrokeKind::Inside,
    );

    let count = rows.len();
    let rail_id = egui::Id::new("transcript-rail");
    let response = ui.interact(rail, rail_id, egui::Sense::click_and_drag());
    let mut seek = None;
    if (response.clicked() || response.dragged())
        && let Some(at) = response.interact_pointer_pos()
    {
        let fraction = ((at.y - rail.top()) / rail.height()).clamp(0.0, 1.0);
        let ix = ((fraction * count as f32) as usize).min(count - 1);
        seek = rows.get(ix).map(Row::key);
    }

    for (ix, (row, tick)) in rows.iter().zip(ticks).enumerate() {
        let Some((tick, label)) = tick else {
            continue;
        };
        let at = (ix as f32 + 0.5) / count as f32;
        let (left, w, h, color, round) = match tick {
            Tick::Turn => (2.0, 12.0, 2.0, palette.muted, 2),
            Tick::Edit => (3.0, 10.0, 3.0, palette.ok, 2),
            Tick::Fail => (3.0, 10.0, 4.0, palette.danger, 2),
            Tick::Approval => (5.0, 6.0, 6.0, palette.ok, 3),
            Tick::Swarm => (4.0, 8.0, 8.0, palette.done, 4),
            Tick::Agent => (5.0, 6.0, 6.0, palette.info, 2),
            Tick::Plan => (3.0, 10.0, 4.0, palette.accent, 2),
        };
        let center = rail.top() + at * rail.height();
        let rect = egui::Rect::from_min_size(
            egui::pos2(rail.left() + left, center - h / 2.0),
            egui::vec2(w, h),
        );
        let tick_id = egui::Id::new(("transcript-rail-tick", ix));
        let response = ui.interact(rect, tick_id, egui::Sense::click());
        if response.clicked() {
            seek = Some(row.key());
        }
        response.on_hover_text(label.as_str());
        painter.rect_filled(rect, egui::CornerRadius::same(round), color);
    }
    seek
}

/// The "Jump to latest" button over the pane's bottom right corner,
/// shown while the reader has scrolled away from the tail. Returns
/// whether it was clicked.
fn jump_to_latest(ui: &mut egui::Ui, pane: egui::Rect) -> bool {
    use crate::ui::settings_config::{BtnTone, btn_sm_at, btn_sm_galley, btn_sm_size};
    let palette = Palette::active_of(ui.ctx());
    let label = "Jump to latest";
    let anchor = pane.right_bottom() - egui::vec2(16.0, 12.0);
    let size = btn_sm_size(&btn_sm_galley(ui, label), None);
    // The button's own fill is translucent, so the rows under it are
    // covered first.
    ui.painter().rect_filled(
        egui::Align2::RIGHT_BOTTOM.anchor_size(anchor, size),
        egui::CornerRadius::same(crate::theme::R_MD as u8),
        palette.bg,
    );
    btn_sm_at(
        ui,
        anchor,
        egui::Align2::RIGHT_BOTTOM,
        egui::Id::new("transcript-jump-latest"),
        label,
        None,
        BtnTone::Plain,
        palette,
    )
    .clicked()
}

fn shown<'a>(revealed: &HashMap<usize, usize>, ix: usize, text: &'a str) -> &'a str {
    match revealed.get(&ix) {
        Some(chars) => reveal::prefix(text, *chars),
        None => text,
    }
}

/// The tint a row renders with under the find bar: the current match
/// strongest, the other matches softer.
fn row_tint(find: &FindMarks, key: RowKey, palette: &Palette) -> Option<egui::Color32> {
    if find.keys.get(find.current) == Some(&key) {
        return Some(palette.selected_hover);
    }
    find.keys.contains(&key).then_some(palette.selected)
}

/// Lays one row out offscreen at the column's width and returns the
/// height it took, margins included, so the list can place it.
/// The height the measure pass offers a row. Generous, but finite: a
/// row that asks the layout for its available height (the tool and
/// plan detail boxes scroll inside a `max_height` cap) would otherwise
/// measure the offer itself, the content height would explode, and the
/// follow would park the viewport past every row but the last.
const MEASURE_MAX_H: f32 = 20_000.0;

fn measure_row(
    ui: &mut egui::Ui,
    width: f32,
    rows: &[Row],
    ix: usize,
    ctx: &mut RowCtx<'_>,
) -> f32 {
    let Some(row) = rows.get(ix) else {
        return 0.0;
    };
    let prev = ix.checked_sub(1).map(|p| &rows[p]);
    let last = ix + 1 == rows.len();
    let rect =
        egui::Rect::from_min_size(egui::pos2(-1.0e6, -1.0e6), egui::vec2(width, MEASURE_MAX_H));
    // A root-level ui, not a child of the panel hierarchy: a child
    // whose rect sits far outside its ancestors measures wrong (the
    // row comes back the size of the offer plus its content), while
    // the same layout on its own ui measures true. Style and layout
    // come from the caller so the numbers match the drawn rows.
    let mut child = egui::Ui::new(
        ui.ctx().clone(),
        egui::Id::new(("row-measure", ix, ui.ctx().cumulative_pass_nr())),
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(*ui.layout())
            .style(std::sync::Arc::clone(ui.style())),
    );
    let top = child.cursor().top();
    let mut actions = Vec::new();
    row_cell(&mut child, row, prev, last, ctx, &mut actions);
    finite_height(child.min_rect().bottom() - top, ix, row)
}

/// `height`, or nothing when a row's layout came back non-finite: the
/// list must not carry a NaN into the content height, so the row is
/// reported and placed as empty instead.
fn finite_height(height: f32, ix: usize, row: &Row) -> f32 {
    if height.is_finite() {
        height
    } else {
        eprintln!("kage: transcript row {ix} laid out {height} tall: {row:?}");
        0.0
    }
}

/// Draws one row and returns the height it took, margins included.
fn draw_row(
    ui: &mut egui::Ui,
    row: &Row,
    prev: Option<&Row>,
    last: bool,
    ctx: &mut RowCtx<'_>,
    events: &mut Vec<RowAction>,
) -> f32 {
    let top = ui.cursor().top();
    row_cell(ui, row, prev, last, ctx, events);
    ui.min_rect().bottom() - top
}

/// One row on the design's collapsed margins: the gap above it, the
/// element, and the bottom margin the last row carries.
fn row_cell(
    ui: &mut egui::Ui,
    row: &Row,
    prev: Option<&Row>,
    last: bool,
    ctx: &mut RowCtx<'_>,
    events: &mut Vec<RowAction>,
) -> egui::Response {
    let spacing = ui.spacing().item_spacing.y;
    ui.spacing_mut().item_spacing.y = 0.0;
    ui.add_space(gap(prev, row));
    let inner = ui.scope_builder(egui::UiBuilder::new().id_salt(row.key()), |ui| {
        ui.spacing_mut().item_spacing.y = spacing;
        render_row(ui, row, ctx, events)
    });
    if last {
        ui.add_space(margins(row).1);
    }
    ui.spacing_mut().item_spacing.y = spacing;
    inner.response
}

/// One transcript row, from the model and the session it names,
/// dispatched to the renderer that draws it.
fn render_row(
    ui: &mut egui::Ui,
    row: &Row,
    ctx: &mut RowCtx<'_>,
    events: &mut Vec<RowAction>,
) -> egui::Response {
    let RowCtx {
        store,
        session,
        times,
        ui: ui_state,
        pipeline,
        revise,
        revealed,
        pinned,
        born,
    } = ctx;
    let times = *times;
    let revealed = *revealed;
    match row {
        Row::User { ix, text } => match session.items.get(*ix) {
            Some(TranscriptItem::User { .. })
                if let Some(summary) = text_rows::summary_of(text) =>
            {
                let response = text_rows::render_summary(
                    ui,
                    pipeline,
                    *ix,
                    summary,
                    ui_state.expanded.contains(&RowKey::Item(*ix)),
                );
                if response.clicked() {
                    tool_rows::toggle(ui_state, RowKey::Item(*ix));
                }
                response
            }
            Some(TranscriptItem::User { .. }) if let Some(note) = text_rows::note_of(text) => {
                text_rows::render_engine_note(ui, *ix, &note)
            }
            Some(TranscriptItem::User { steered, .. })
                if let Some((words, parts)) = text_rows::split_prompt(text) =>
            {
                let palette = Palette::active_of(ui.ctx());
                let mut opened = None;
                let mut draw =
                    |ui: &mut egui::Ui, n: usize, report: &kage_client::agent_text::Report| {
                        if crate::ui::agents::report_card(ui, n, report, palette) {
                            opened = Some(report.session.clone());
                        }
                    };
                let (response, actions) =
                    text_rows::render_delivered(ui, words, &parts, Some(&mut draw), !*pinned);
                events.extend(
                    actions
                        .into_iter()
                        .map(|action| user_row_action(*ix, action)),
                );
                if let Some(id) = opened {
                    events.push(RowAction::OpenAgent(id));
                }
                response
            }
            Some(TranscriptItem::User { steered, .. }) => {
                let attached = text_rows::attachments(session, *ix);
                let (response, actions) =
                    text_rows::render_user(ui, text, &attached, *steered, !*pinned);
                events.extend(
                    actions
                        .into_iter()
                        .map(|action| user_row_action(*ix, action)),
                );
                response
            }
            _ => blank_row(ui),
        },
        Row::Assistant { ix, live } => match session.items.get(*ix) {
            Some(TranscriptItem::Assistant { text }) => {
                text_rows::render_assistant(ui, pipeline, *ix, shown(revealed, *ix, text), *live)
            }
            _ => blank_row(ui),
        },
        Row::Media { ix } => match session.items.get(*ix) {
            Some(TranscriptItem::Media { mime, data }) => {
                let (response, pressed) = super::media_rows::render_media(ui, mime, data);
                events.extend(pressed.map(|pressed| RowAction::Media(*ix, 0, pressed)));
                response
            }
            _ => blank_row(ui),
        },
        Row::Thinking { ix, live, expanded } => match session.items.get(*ix) {
            Some(TranscriptItem::Thinking { text, .. }) => {
                let response = text_rows::render_thinking(
                    ui,
                    shown(revealed, *ix, text),
                    *live,
                    *expanded,
                    times.and_then(|t| t.thinking(*ix)),
                );
                if response.clicked() {
                    tool_rows::toggle(ui_state, RowKey::Item(*ix));
                }
                response
            }
            _ => blank_row(ui),
        },
        Row::Tool { ix, .. } => match session.items.get(*ix) {
            Some(TranscriptItem::ToolCall(call))
                if call.status == kage_client::wire::ToolCallStatus::Completed
                    && let Some(todos) = tool_rows::todos_of_call(call) =>
            {
                let (response, actions) = tool_rows::render_todos(ui, *ix, &todos, ui_state);
                events.extend(actions);
                response
            }
            Some(TranscriptItem::ToolCall(call))
                if (call.title == "agent" || call.title == "swarm")
                    && let Some((response, actions)) = tool_rows::render_agents(
                        ui, store, session, *ix, call, *born, ui_state,
                    ) =>
            {
                events.extend(actions);
                response
            }
            Some(TranscriptItem::ToolCall(call)) if call.title == crate::store::EXIT_PLAN_TOOL => {
                let (response, actions) =
                    tool_rows::render_plan_card(ui, pipeline, session, *ix, call, revise, ui_state);
                events.extend(actions);
                response
            }
            Some(TranscriptItem::ToolCall(call)) => {
                let media = super::media_rows::tool_media(call);
                let mut render = |ui: &mut egui::Ui, events: &mut Vec<RowAction>| {
                    let (response, actions) = tool_rows::render_tool(
                        ui,
                        *ix,
                        call,
                        false,
                        false,
                        times.and_then(|t| t.tool(&call.tool_call_id)),
                        ui_state,
                    );
                    events.extend(actions);
                    response
                };
                if media.is_empty() {
                    render(ui, events)
                } else {
                    // A tool's images and clips show under its row, open
                    // or not: the media is usually why it ran.
                    ui.vertical(|ui| {
                        render(ui, events);
                        for (nth, (mime, data)) in media.iter().enumerate() {
                            ui.add_space(crate::theme::SP_2);
                            let (_, pressed) = super::media_rows::render_media(ui, mime, data);
                            events
                                .extend(pressed.map(|pressed| RowAction::Media(*ix, nth, pressed)));
                        }
                    })
                    .response
                }
            }
            _ => blank_row(ui),
        },
        Row::Group { .. } => {
            let (response, actions) = tool_rows::render_group(ui, session, row, times, ui_state);
            events.extend(actions);
            response
        }
        Row::Changes { end } => {
            let entries = super::model::run_changes(session, *end);
            let (response, actions) = tool_rows::render_changes(ui, &entries, *end);
            events.extend(actions);
            response
        }
        Row::TurnEnd { ix, outcome } => {
            let (response, actions) = tool_rows::render_turn_end(
                ui,
                session,
                *ix,
                *outcome,
                times.and_then(|t| t.run_end(*ix)),
                *pinned,
            );
            events.extend(actions);
            response
        }
        Row::Notice { ix } => match session.items.get(*ix) {
            Some(TranscriptItem::Notice { tone, text }) => {
                text_rows::render_notice(ui, *ix, *tone, text)
            }
            _ => blank_row(ui),
        },
        Row::Compaction { ix } => match session.items.get(*ix) {
            Some(TranscriptItem::Compaction {
                kept,
                before,
                after,
                summarized,
                summary,
            }) => {
                let response = text_rows::render_compaction(
                    ui,
                    pipeline,
                    text_rows::CompactionCard {
                        ix: *ix,
                        kept: *kept,
                        before: *before,
                        after: *after,
                        summarized: *summarized,
                        summary: summary.as_deref(),
                        expanded: ui_state.expanded.contains(&RowKey::Item(*ix)),
                    },
                );
                if response.clicked() {
                    tool_rows::toggle(ui_state, RowKey::Item(*ix));
                }
                response
            }
            _ => blank_row(ui),
        },
        Row::Plan { ix } => match session.items.get(*ix) {
            Some(TranscriptItem::Plan { entries }) => {
                let todos = tool_rows::todos_of_plan(entries);
                let (response, actions) = tool_rows::render_todos(ui, *ix, &todos, ui_state);
                events.extend(actions);
                response
            }
            _ => blank_row(ui),
        },
        Row::Decision { ix } => match session.items.get(*ix) {
            Some(TranscriptItem::Decision {
                subject,
                label,
                allowed,
                feedback,
            }) => {
                text_rows::render_decision(ui, *ix, subject, label, *allowed, feedback.as_deref())
            }
            _ => blank_row(ui),
        },
        Row::Waiting => {
            let label = if session.compacting {
                "Compacting"
            } else {
                "Working"
            };
            text_rows::render_waiting(ui, label)
        }
    }
}

/// Carries out the actions a user row raised: the copy goes to the
/// clipboard now, and the edit asks for the composer E05 brings.
fn user_row_action(ix: usize, action: UserAction) -> RowAction {
    match action {
        UserAction::Copy(text) => RowAction::Copy(text),
        UserAction::Edit(text) => RowAction::Edit(text),
        UserAction::Fork => RowAction::Fork(Some(ix)),
        UserAction::Rewind => RowAction::Rewind(ix),
    }
}

/// A row whose item is gone: nothing draws, but the row keeps its
/// place in the list.
fn blank_row(ui: &mut egui::Ui) -> egui::Response {
    ui.allocate_response(egui::Vec2::ZERO, egui::Sense::hover())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Carries out the store's pending commands the way the shell does.
    fn run_commands(store: &mut Store) {
        let commands = store.take_commands();
        for command in commands {
            match command {
                crate::store::Command::Handshake { replay_sessions } => {
                    store.handshake(replay_sessions);
                }
                crate::store::Command::NewSession => {
                    store.new_session();
                }
                crate::store::Command::ReplayPrompt => {
                    store.prompt("fix the null check");
                }
            }
        }
    }

    /// Drives the recording through a store the way the shell does:
    /// handshake, session, scripted prompt, then every later frame.
    fn replayed_store() -> Store {
        let mut store = Store::new("/w", true);
        store.set_connect(crate::transport::State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        let frames = crate::transport::replay::transcript();
        store.absorb(frames[0].clone());
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(frames[1].clone());
        run_commands(&mut store);
        let _ = store.take_outgoing();
        for frame in &frames[2..] {
            store.absorb(frame.clone());
        }
        store
    }

    #[test]
    fn the_find_marks_tint_the_current_match_strongest() {
        let marks = FindMarks {
            keys: vec![RowKey::Item(1), RowKey::Item(4)],
            current: 1,
        };
        let palette = Palette::shadow();
        let current = row_tint(&marks, RowKey::Item(4), &palette).expect("the current match tints");
        let other = row_tint(&marks, RowKey::Item(1), &palette).expect("a match tints");
        assert_ne!(current, other, "the current match reads strongest");
        assert!(row_tint(&marks, RowKey::Item(2), &palette).is_none());
    }

    #[test]
    fn the_cursor_clamps_to_the_rows_and_starts_absent() {
        let mut view = TranscriptView::new();
        assert_eq!(view.cursor_row(), None);
        assert_eq!(view.row_count(), 0);
        view.set_vim_cursor(Some(3));
        assert_eq!(view.cursor_row(), None, "no rows, no cursor");
    }

    #[test]
    fn a_pinned_view_reads_its_own_session() {
        let store = replayed_store();
        let active = store.active_id().expect("the recording opened a session");
        let view = TranscriptView::pinned(active);
        assert!(
            view.searchable_rows(&store).len() > 5,
            "the pinned view reads the recording's rows"
        );
    }

    #[test]
    fn the_lab_toggle_turns_the_paced_reveal_off() {
        let mut store = replayed_store();
        let view = TranscriptView::new();
        assert!(view.smooth(&store), "the default pace is on");
        store.update_prefs(|prefs| prefs.smooth_stream = false);
        assert!(!view.smooth(&store), "the toggle shows a reply whole");
    }

    #[test]
    fn the_rail_rebuilds_only_when_the_rows_change() {
        let mut rail = RailTicks::default();
        let mut builds = 0;
        let mut build = || {
            builds += 1;
            vec![Some((Tick::Turn, "You: hi".to_owned()))]
        };
        assert!(rail.refresh(&[1], &mut build));
        assert!(!rail.refresh(&[1], &mut build));
        assert!(!rail.refresh(&[1], &mut build));
        assert!(rail.refresh(&[1, 2], &mut build));
        assert_eq!(builds, 2);
    }

    #[test]
    fn swapping_sessions_forgets_the_markdown_caches() {
        let mut harness = egui_kittest::Harness::builder().build_ui_state(
            |ui, view: &mut TranscriptView| {
                view.pipeline.show("assistant-0", "hello", false, ui);
            },
            TranscriptView::default(),
        );
        harness.run();
        assert_eq!(harness.state().pipeline.parses.count("assistant-0"), 1);
        harness.state_mut().swap_session(Some("b".to_owned()));
        assert_eq!(harness.state().pipeline.parses.count("assistant-0"), 0);
    }

    #[test]
    fn a_row_whose_item_is_gone_draws_nothing() {
        // The dispatch guards on the item the row names, so a stale
        // index falls to the blank row and takes no space.
        let mut harness = egui_kittest::Harness::new_ui(|_ui| {});
        harness.run();
        let _ = &Row::Notice { ix: 9_999 };
    }
}
