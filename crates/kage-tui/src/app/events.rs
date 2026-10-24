//! Submit, mouse, scroll/fold, and render entry points.

use super::*;

impl App {
    /// Resolve a submitted draft: send the prompt and its images to the
    /// session on screen, steered into the run in flight or, with
    /// `queue`, held until it ends. A prompt the engine expands through
    /// MCP is always queued. An agent of a resumed session takes no
    /// prompts, so the text goes back into the draft. A draft starting
    /// with `/` is a command instead, run through the command
    /// executor; an unknown one goes back into the draft with the
    /// reason shown.
    pub(crate) fn handle_submit(&mut self, text: String, queue: bool) {
        if let Some(line) = text.strip_prefix('/') {
            if line.is_empty() {
                return;
            }
            let registry = self.command_registry();
            match self.run_command_validated(line, &registry) {
                CommandResult::Done(_) => return,
                CommandResult::ValidationError(msg) => {
                    self.input.splice(0, 0, &text);
                    self.notify(msg);
                    return;
                }
            }
        }
        if self.focused_read_only() {
            self.input.splice(0, 0, &text);
            let agent = self.focused_agent().unwrap_or("the agent");
            let text = format!("{agent} has finished; only a running agent can be steered");
            self.notify(text);
            return;
        }
        let images = self.input.take_attached();
        let queue = queue || self.mcp_expands(&text);
        self.send_prompt(text, images, queue, self.focus);
    }

    /// Resolve an `InputAction::QueuePrompt`: send the draft to run
    /// after the run in flight. Idle it does nothing, so a stray Tab
    /// never sends a prompt.
    fn queue_prompt(&mut self) {
        if self.is_run_in_flight()
            && let Some(text) = self.input.take_prompt()
        {
            self.handle_submit(text, true);
        }
    }

    /// Resolve an `InputAction::RecallPrompt`: take the newest pending
    /// prompt of the focused session back out of the engine queue. The
    /// row leaves when the engine confirms what it popped.
    fn recall_prompt(&mut self) {
        let session = self.focus;
        let Some(at) = self.pending.iter().rposition(|(s, _)| *s == session) else {
            return;
        };
        let delivery = if self.pending[at].1.queued {
            kage_core::protocol::Delivery::Queue
        } else {
            kage_core::protocol::Delivery::Steer
        };
        if self
            .send_request(RunRequest::RecallPrompt { session, delivery })
            .is_err()
        {
            self.push_error("recall failed: agent worker has stopped");
        }
    }

    /// Send a prompt to the engine and follow the conversation to its
    /// end. The user block appears when the engine delivers it. During
    /// a run that happens later, so until then it shows as a pending
    /// row above the input. Steers are listed before queued prompts,
    /// the order the engine delivers them in. A prompt for an agent
    /// `session` goes to that agent, and its row shows in that agent's
    /// view.
    pub(crate) fn send_prompt(
        &mut self,
        text: String,
        images: Vec<crate::image::AttachedImage>,
        queue: bool,
        session: Option<kage_core::SessionId>,
    ) {
        let pending = self.session_running(session).then(|| view::PendingPrompt {
            text: text
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("[image]")
                .to_owned(),
            queued: queue,
        });
        let submit = RunRequest::Submit {
            text,
            images,
            queue,
            session,
        };
        if self.send_request(submit).is_err() {
            self.push_error("submit failed: agent worker has stopped");
            return;
        }
        self.follow();
        if let Some(pending) = pending {
            let at = if pending.queued {
                self.pending.len()
            } else {
                self.pending.partition_point(|(_, p)| !p.queued)
            };
            self.pending.insert(at, (session, pending));
        }
    }

    /// Resolve an `InputAction::RunShell`: send the command to the
    /// worker, which paints its own shell block, so no user block is
    /// pushed here, and follow the conversation to its end.
    pub(crate) fn handle_shell(&mut self, text: String) {
        if self.send_request(RunRequest::RunShell(text)).is_err() {
            self.push_error("shell failed: agent worker has stopped");
        } else {
            self.follow();
        }
    }

    pub(crate) fn apply(&mut self, action: InputAction) -> Option<AppExit> {
        match action {
            InputAction::Submit(text) => self.handle_submit(text, false),
            InputAction::QueuePrompt => self.queue_prompt(),
            InputAction::RecallPrompt => self.recall_prompt(),
            InputAction::Escape => return self.escalate(keys::Trigger::Esc),
            InputAction::RunShell(text) => self.handle_shell(text),
            InputAction::DroppedStaleAttach => {
                self.notify("dropped stale image attach (the prompt was empty)");
            }
            InputAction::Scroll(delta) => self.scroll_by(delta),
            InputAction::ScrollToTop => self.set_scroll(0),
            InputAction::ScrollToBottom => self.follow(),
            InputAction::ToggleFold => self.toggle_last_fold(),
            InputAction::UnfoldAll => self.set_all_folds(false),
            InputAction::FoldAll => self.set_all_folds(true),
            InputAction::Cancel => {
                self.trip_cancel();
            }
            InputAction::OpenModelPicker => {
                if self.model_choices.is_empty() {
                    self.notify("no models available. Run /login to connect a provider");
                } else {
                    self.picker = Some(
                        OverlayPicker::new("switch model", self.model_choices.clone())
                            .with_note("/login to add a provider"),
                    );
                    self.picker_kind = Some(PickerKind::Model);
                }
            }
            InputAction::FocusPrev => {
                let mut buf = lock(&self.buffer);
                buf.focus_prev_any();
            }
            InputAction::FocusNext => {
                let mut buf = lock(&self.buffer);
                buf.focus_next_any();
            }
            InputAction::OpenSessionPicker => {
                // Always this directory's sessions first. A directory
                // with none still opens the empty picker when other
                // directories have some, so Ctrl+A is one key away
                // instead of the picker silently listing every project.
                if let Some(lister) = self.session_lister.as_ref() {
                    let items = lister(false);
                    let elsewhere = items.is_empty() && !lister(true).is_empty();
                    self.session_scope_all = false;
                    self.show_session_picker(items, elsewhere);
                }
            }
            InputAction::BeginCommand => {
                self.cmdline = Some(CommandLine::new());
            }
            InputAction::EnterMode(_) => {}
            InputAction::Yank => self.yank_screen_selection(),
            InputAction::ClearSelection => self.clear_selection(),
            InputAction::EnterVisual => self.enter_visual_mode(),
            InputAction::VisualLeft => self.move_visual_cursor(0, -1),
            InputAction::VisualRight => self.move_visual_cursor(0, 1),
            InputAction::VisualUp => self.move_visual_cursor(-1, 0),
            InputAction::VisualDown => self.move_visual_cursor(1, 0),
            InputAction::VisualLineStart => self.snap_visual_cursor_x(0),
            InputAction::VisualLineEnd => self.snap_visual_cursor_x(i32::MAX),
            InputAction::YankFocusedBlock => self.yank_focused_block(),
            InputAction::BeginSearch => self.begin_search(),
            InputAction::SearchNext => self.jump_to_search_match(true),
            InputAction::SearchPrev => self.jump_to_search_match(false),
            InputAction::CyclePane => {
                self.input.toggle_focused_pane();
            }
            InputAction::FocusPane(pane) => {
                self.input.set_focused_pane(pane);
            }
            InputAction::CycleThinkingLevel => {
                let _ = self.send_request(RunRequest::CycleThinkingLevel);
            }
            InputAction::TogglePlanMode => {
                let on = !self.plan_mode();
                let _ = self.send_request(RunRequest::PlanMode { on });
            }
            InputAction::OpenHelp => self.open_help(),
            InputAction::OpenJumpPicker => self.open_jump_picker(),
            InputAction::OpenAgents => self.open_agents(),
            InputAction::AttachClipboardImage => self.request_clipboard_attach(),
        }
        None
    }

    /// Send a request to the worker. The channel is unbounded, so the
    /// only failure is a dropped receiver - the worker thread has
    /// stopped - which surfaces as a toast here: every caller discards
    /// the error, and an action that silently vanishes reads as a
    /// program hang.
    pub(crate) fn send_request(&mut self, req: RunRequest) -> Result<(), TrySendError<RunRequest>> {
        match self.requests.send(req) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.notify("agent worker stopped - request not delivered");
                Err(TrySendError::Disconnected(err.0))
            }
        }
    }

    /// True while a modal overlay owns the screen. While one is open,
    /// mouse events must not reach the buffer underneath: scrolling
    /// would move invisible content and clicks would change focus
    /// under a dialog. The context menu is deliberately excluded: it
    /// is itself driven by mouse events.
    pub(crate) fn modal_open(&self) -> bool {
        self.plugin_overlay.is_some()
            || self.picker.is_some()
            || self.settings_overlay.is_some()
            || self.session_tree.is_some()
            || self.agents_overlay.is_some()
            || self.cmdline.is_some()
            || self.search_line.is_some()
            || self.help_overlay.is_some()
            || self.todo_list_overlay.is_some()
            || self.approval_panel.is_some()
    }

    /// Dispatch one crossterm mouse event. While a modal overlay is
    /// open every event is swallowed so scrolling or clicking cannot
    /// act on the hidden buffer (the context menu is the exception:
    /// it is mouse-driven and stays live). Otherwise scroll moves the
    /// buffer (and closes any context menu so it cannot hang in
    /// mid-air); a right press opens the context menu; a left press
    /// either feeds the open menu or starts the normal selection
    /// gesture.
    pub(crate) fn handle_mouse_event(&mut self, mouse: ratatui::crossterm::event::MouseEvent) {
        use ratatui::crossterm::event::MouseButton;
        // The agents overlay scrolls with the wheel; nothing else under
        // it sees the mouse.
        if let Some(overlay) = self.agents_overlay.as_mut() {
            match mouse.kind {
                MouseEventKind::ScrollUp => overlay.wheel(false),
                MouseEventKind::ScrollDown => overlay.wheel(true),
                _ => {}
            }
            return;
        }
        if self.context_menu.is_none() && self.modal_open() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.context_menu = None;
                self.scroll_by(-MOUSE_SCROLL_LINES);
            }
            MouseEventKind::ScrollDown => {
                self.context_menu = None;
                self.scroll_by(MOUSE_SCROLL_LINES);
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.open_context_menu(mouse.column, mouse.row);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.context_menu.is_some() {
                    self.context_menu_click(mouse.column, mouse.row);
                } else {
                    self.mouse_down(mouse.row, mouse.column);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.mouse_drag(mouse.row, mouse.column);
            }
            MouseEventKind::Up(MouseButton::Left) => self.mouse_up(mouse.row),
            _ => {}
        }
    }

    /// Mouse left-button press: anchor a virtual-row selection at
    /// the click position. Any prior selection (and its captured
    /// text) is dropped. Focus snaps to whichever block sits under
    /// the click so subsequent keyboard gestures act on it. A press on a
    /// pinned agent row focuses that agent instead.
    pub(crate) fn mouse_down(&mut self, row: u16, col: u16) {
        if let Some(&(_, session)) = self.pinned_hits.iter().find(|(r, _)| *r == row) {
            self.focus_agent(session);
            return;
        }
        if self.pinned_summary_hit == Some(row) {
            self.open_agents();
            return;
        }
        // A press on the `new output below` mark snaps back to the
        // bottom and re-arms follow instead of starting a selection.
        if lock(&self.buffer).mark_hit_at(row, col) {
            self.follow();
            return;
        }
        self.captured_rows.clear();
        let mut buf = lock(&self.buffer);
        let area_y = buf.last_area_y();
        let area_height = buf.last_area_height();
        if row < area_y || row >= area_y.saturating_add(area_height) {
            // Click landed outside the buffer rectangle. Anything
            // below the buffer is the input card or modeline; the
            // top status row is above. Clicks below the buffer
            // focus the input pane (vim-style window focus); top
            // status clicks leave focus alone.
            self.screen_selection = None;
            self.mouse_drag_anchor = None;
            if row >= area_y.saturating_add(area_height) {
                self.input.set_focused_pane(Pane::Input);
            }
            return;
        }
        // Modeless typing always goes to the input, so only vim mode
        // moves keyboard focus to the buffer on a click.
        if !self.input.is_modeless() {
            self.input.set_focused_pane(Pane::Buffer);
        }
        let vrow = buf
            .last_virtual_top()
            .saturating_add(usize::from(row - area_y));
        self.screen_selection = Some(((vrow, col), (vrow, col)));
        if let Some(idx) = buf.block_at_screen_row(row) {
            buf.focus_in_place(idx);
            self.mouse_drag_anchor = Some((row, idx, false));
        } else {
            self.mouse_drag_anchor = None;
        }
    }

    /// Mouse drag while left-button is held: extend the selection
    /// cursor to the virtual-row under `(row, col)`. A drag above or
    /// below the buffer area scrolls one line toward it and extends
    /// the selection to the row that scrolled in. At either end of the
    /// conversation it clamps to the closest visible row instead.
    pub(crate) fn mouse_drag(&mut self, row: u16, col: u16) {
        let Some((anchor, _)) = self.screen_selection else {
            return;
        };
        let mut buf = lock(&self.buffer);
        let area_y = buf.last_area_y();
        let area_height = buf.last_area_height();
        if area_height == 0 {
            return;
        }
        let last_visible_row = area_y.saturating_add(area_height).saturating_sub(1);
        let top = buf.scroll().unwrap_or_else(|| buf.last_virtual_top());
        let vrow = if row < area_y && top > 0 {
            buf.set_scroll(top - 1);
            top - 1
        } else if row > last_visible_row && !buf.is_following() {
            buf.set_scroll(top + 1);
            top + usize::from(area_height)
        } else {
            top + usize::from(row.clamp(area_y, last_visible_row) - area_y)
        };
        self.screen_selection = Some((anchor, (vrow, col)));
        if let Some((_, _, ref mut dragged)) = self.mouse_drag_anchor {
            *dragged = true;
        }
    }

    /// Mouse left-button release: a non-dragged release on a block's
    /// header row toggles fold and clears the just-anchored
    /// zero-width selection; a dragged release copies the highlighted
    /// selection straight to the clipboard without waiting for `y`.
    /// The toggle pins the viewport, so the header stays under the
    /// pointer instead of the bottom-anchored view pushing it up.
    pub(crate) fn mouse_up(&mut self, row: u16) {
        let Some((_down_row, anchor_idx, dragged)) = self.mouse_drag_anchor.take() else {
            return;
        };
        if dragged {
            // The render after the final drag event already captured
            // every selected row into `captured_rows`, so this sees
            // the same state a `y` press would: extract, copy, clear.
            self.yank_screen_selection();
            return;
        }
        // Plain click: clear the zero-width selection we anchored on
        // press, then maybe toggle a fold on the header row.
        self.clear_selection();
        let mut buf = lock(&self.buffer);
        if buf.screen_top_of(anchor_idx) == Some(row) {
            if buf.is_following() {
                let top = buf.last_virtual_top();
                buf.set_scroll(top);
            }
            buf.toggle_fold(anchor_idx);
        }
    }

    pub(crate) fn scroll_by(&mut self, delta: i32) {
        let mut buf = lock(&self.buffer);
        // Positive delta = move toward newest (increment the absolute
        // anchor); negative = toward oldest (decrement). Anchor on the
        // last painted viewport top so a pinned viewport stays exactly
        // where the user is reading while content streams in below;
        // while following, the anchor is the previous frame's top
        // (bottom row), so a scroll wheel tick detaches from there.
        let anchor = buf.scroll().unwrap_or_else(|| buf.last_virtual_top());
        let current = i64::try_from(anchor).unwrap_or(i64::MAX);
        let target = (current + i64::from(delta)).max(0);
        let clamped = usize::try_from(target).unwrap_or(0);
        buf.set_scroll(clamped);
    }

    pub(crate) fn set_scroll(&mut self, scroll: usize) {
        let mut buf = lock(&self.buffer);
        buf.set_scroll(scroll);
    }

    pub(crate) fn follow(&mut self) {
        let mut buf = lock(&self.buffer);
        buf.follow();
    }

    pub(crate) fn toggle_last_fold(&mut self) {
        let mut buf = lock(&self.buffer);
        if let Some(idx) = buf.fold_target() {
            buf.toggle_fold(idx);
        }
    }

    pub(crate) fn set_all_folds(&mut self, folded: bool) {
        let mut buf = lock(&self.buffer);
        buf.set_all_folded(folded);
    }

    /// Read-only borrow of the input state. Tests use this to assert
    /// mode transitions without driving a real terminal.
    #[must_use]
    pub fn input(&self) -> &InputState {
        &self.input
    }

    /// Apply a key directly without going through crossterm. Used by
    /// tests and by external command handlers that want to
    /// drive the modal state machine programmatically.
    pub fn handle_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Option<AppExit> {
        self.dispatch_key(key)
    }

    /// Force a redraw onto an arbitrary terminal. Tests use this with
    /// [`ratatui::backend::TestBackend`] to capture the rendered frame.
    pub fn render_into<B>(&mut self, terminal: &mut ratatui::Terminal<B>) -> Result<(), TuiError>
    where
        B: ratatui::backend::Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        self.paint(terminal)
    }
}
