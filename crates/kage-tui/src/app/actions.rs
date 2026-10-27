//! Command validation, builtin dispatch, selection, and info screens.

use base64::Engine as _;
use kage_core::protocol::ImageAction;

use super::*;
use crate::command::{ArgValue, ParsedArgs};

impl App {
    /// Validated command dispatch. Parses the argument string against
    /// the matched spec's schema and returns a [`CommandResult`]
    /// instead of pushing errors to the buffer. On
    /// [`CommandResult::ValidationError`], the caller keeps the
    /// cmdline open so the user can fix the input.
    pub(crate) fn run_command_validated(
        &mut self,
        line: &str,
        registry: &[&CommandSpec],
    ) -> CommandResult {
        self.run_command_line(line, registry, '/')
    }

    /// [`Self::run_command_validated`] for a line typed after `prefix`,
    /// which a suggestion for an unknown command repeats.
    pub(crate) fn run_command_line(
        &mut self,
        line: &str,
        registry: &[&CommandSpec],
        prefix: char,
    ) -> CommandResult {
        let mut parts = line.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();

        if head.is_empty() {
            return CommandResult::Done(None);
        }

        // Resolve `head` to a plugin command (direct name or alias).
        let canonical = if self.plugin_commands.iter().any(|(n, _)| n == head) {
            Some(head.to_owned())
        } else {
            self.plugin_command_aliases
                .iter()
                .find(|(alias, _)| alias == head)
                .map(|(_, name)| name.clone())
        };

        // An `override_command` shadows a builtin: dispatch it ahead
        // of the builtin lookup.
        if let Some(name) = canonical
            .as_deref()
            .filter(|n| self.plugin_command_overrides.iter().any(|o| o == n))
        {
            let _ = self.send_request(RunRequest::InvokePluginCommand {
                name: name.to_owned(),
                args: rest.to_owned(),
            });
            return CommandResult::Done(None);
        }

        if let Some(spec) = crate::command::find_builtin_command(head) {
            let (target_spec, target_rest) = Self::resolve_subcommand_tree(spec, rest);
            let args = match crate::cmdparse::parse_input(target_spec, target_rest) {
                Ok(args) => args,
                Err(e) => return CommandResult::ValidationError(e.to_string()),
            };
            let exit = self.dispatch_builtin(spec.name, rest, &args);
            return CommandResult::Done(exit);
        }

        if let Some(name) = canonical {
            let _ = self.send_request(RunRequest::InvokePluginCommand {
                name,
                args: rest.to_owned(),
            });
            return CommandResult::Done(None);
        }

        if self.mcp_command_specs.iter().any(|spec| spec.name == head) {
            let text = if rest.is_empty() {
                format!("/{head}")
            } else {
                format!("/{head} {rest}")
            };
            self.send_prompt(text, Vec::new(), true, None);
            return CommandResult::Done(None);
        }

        // A skill expands to its body as the prompt, with the rest of
        // the line appended so the skill knows what to act on.
        if let Some(skill) = self.skills.iter().find(|skill| skill.name == head) {
            let mut prompt = skill.body.clone();
            if !rest.is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(rest);
            }
            self.send_prompt(prompt, Vec::new(), false, self.focus);
            return CommandResult::Done(None);
        }

        let mut msg = format!("unknown command: {head}");
        if let Some(suggestion) = crate::cmdparse::suggest_command(registry, head) {
            msg = format!("{msg} (did you mean {prefix}{suggestion}?)");
        }
        CommandResult::ValidationError(msg)
    }

    /// Walk the subcommand tree for commands like `theme set <name>`.
    /// Returns the leaf spec and the remaining argument substring
    /// after consuming subcommand names. If no subcommand matches,
    /// returns the parent spec with the full `rest`.
    pub(crate) fn resolve_subcommand_tree<'a, 'b>(
        spec: &'a CommandSpec,
        rest: &'b str,
    ) -> (&'a CommandSpec, &'b str) {
        if spec.subcommands.is_empty() {
            return (spec, rest);
        }
        let mut parts = rest.splitn(2, char::is_whitespace);
        let first = parts.next().unwrap_or("");
        if let Some(sub) = spec.subcommand(first) {
            let sub_rest = parts.next().unwrap_or("").trim();
            return Self::resolve_subcommand_tree(sub, sub_rest);
        }
        (spec, rest)
    }

    /// Execute a built-in command by canonical name with the
    /// remaining unparsed argument string and the arguments parsed
    /// against the matched spec. The match is on the primary name;
    /// aliases were already resolved by [`Self::run_command`].
    #[expect(
        clippy::too_many_lines,
        reason = "a flat dispatch table over every builtin command"
    )]
    pub(crate) fn dispatch_builtin(
        &mut self,
        name: &str,
        rest: &str,
        args: &ParsedArgs,
    ) -> Option<AppExit> {
        match name {
            "quit" => Some(AppExit::Quit),
            "cancel" => {
                self.trip_cancel();
                None
            }
            "model" => {
                if rest.is_empty() {
                    let _ = self.apply(InputAction::OpenModelPicker);
                } else {
                    let _ = self.send_request(RunRequest::SwitchModel(rest.to_owned()));
                }
                None
            }
            "fold" => {
                if rest == "all" {
                    self.set_all_folds(true);
                } else {
                    self.push_error("fold: usage `/fold all`");
                }
                None
            }
            "unfold" => {
                if rest == "all" {
                    self.set_all_folds(false);
                } else {
                    self.push_error("unfold: usage `/unfold all`");
                }
                None
            }
            "theme" => {
                self.run_theme_command(rest);
                None
            }
            "mouse" => {
                self.run_mouse_command(rest);
                None
            }
            "help" => {
                self.open_help();
                None
            }
            "keybindings" => {
                self.push_keybindings();
                None
            }
            "events" => {
                self.push_events();
                None
            }
            "attach" => {
                self.attach_image_path(rest);
                None
            }
            "compact" => {
                let _ = self.send_request(RunRequest::CompactNow);
                None
            }
            "undo" => {
                let _ = self.send_request(RunRequest::UndoSession);
                None
            }
            "rewind" => {
                self.begin_rewind();
                None
            }
            "settings" => {
                self.open_settings();
                None
            }
            "permission" => {
                self.run_permission_command(rest);
                None
            }
            "login" => {
                self.run_login_command(rest);
                None
            }
            "mcp" => {
                self.run_mcp_command(rest);
                None
            }
            "image" => {
                let path = match args.get("file") {
                    Some(ArgValue::Path(path)) => {
                        Some(kage_core::fsutil::expand_tilde(std::path::Path::new(path)))
                    }
                    _ => None,
                };
                self.run_image_command(rest, path);
                None
            }
            "media" => {
                let path = match args.get("file") {
                    Some(ArgValue::Path(path)) => {
                        Some(kage_core::fsutil::expand_tilde(std::path::Path::new(path)))
                    }
                    _ => None,
                };
                self.run_media_command(rest, path);
                None
            }
            "tree" => {
                self.open_session_tree();
                None
            }
            "agents" => {
                self.open_agents();
                None
            }
            "swarm" => {
                self.run_swarm_command(rest);
                None
            }
            "plan" => {
                self.run_plan_command(rest);
                None
            }
            "goal" => {
                self.run_goal_command(rest);
                None
            }
            "todo_list" => {
                self.open_todo_list();
                None
            }
            "usage" => {
                self.push_usage();
                None
            }
            "clone" => {
                let _ = self.send_request(RunRequest::CloneSession);
                None
            }
            "new" => {
                let _ = self.send_request(RunRequest::NewSession);
                None
            }
            "export" => {
                let dest = match args.get("file") {
                    Some(ArgValue::Path(path)) => {
                        Some(kage_core::fsutil::expand_tilde(std::path::Path::new(path)))
                    }
                    _ => None,
                };
                let _ = self.send_request(RunRequest::ExportSession(dest));
                None
            }
            "clear" => {
                let mut buf = lock(&self.buffer);
                buf.clear();
                None
            }
            "noh" => {
                self.search_pattern = None;
                None
            }
            "reload" => {
                let _ = self.send_request(RunRequest::ReloadPlugins);
                None
            }
            _ => None,
        }
    }

    /// Handle `:login [provider]`: queue the login flow for the run
    /// loop, which owns the terminal the flow suspends.
    pub(crate) fn run_login_command(&mut self, rest: &str) {
        let arg = rest.trim();
        if self.login_runner.is_none() {
            self.push_error("login: unavailable in this host");
            return;
        }
        self.pending_login = Some(if arg.is_empty() {
            PendingLogin::Picker
        } else {
            PendingLogin::Provider(arg.to_owned())
        });
    }

    /// Handle `/mcp`, `/mcp restart <server>` and `/mcp login <server>`.
    /// The argument schema already checked that a subcommand names its
    /// server.
    /// `/image open` and `/image save [file]`: hand the newest image the
    /// model produced to the engine, which holds its bytes. `path` is
    /// the parsed `file` of a save.
    pub(crate) fn run_image_command(&mut self, rest: &str, path: Option<std::path::PathBuf>) {
        let Some(action) = self.media_action("image", rest) else {
            return;
        };
        let _ = self.send_request(RunRequest::Image { action, path });
    }

    /// `/media open` and `/media save [file]`: like
    /// [`Self::run_image_command`] over audio clips as well. `path` is
    /// the parsed `file` of a save.
    pub(crate) fn run_media_command(&mut self, rest: &str, path: Option<std::path::PathBuf>) {
        let Some(action) = self.media_action("media", rest) else {
            return;
        };
        let _ = self.send_request(RunRequest::Media { action, path });
    }

    /// The action an `/image` or `/media` line asks for, or `None`
    /// after reporting an unknown subcommand.
    fn media_action(&mut self, command: &str, rest: &str) -> Option<ImageAction> {
        match rest.split_whitespace().next().unwrap_or("") {
            "open" | "" => Some(ImageAction::Open),
            "save" => Some(ImageAction::Save),
            other => {
                self.push_error(format!(
                    "{command}: unknown subcommand `{other}` (try open, save)"
                ));
                None
            }
        }
    }

    pub(crate) fn run_mcp_command(&mut self, rest: &str) {
        let (sub, server) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let server = server.trim();
        match sub {
            "" => self.open_mcp_picker(),
            "restart" => {
                let _ = self.send_request(RunRequest::RestartMcp(server.to_owned()));
            }
            "login" => self.mcp_login(server),
            other => self.push_error(format!(
                "mcp: unknown subcommand `{other}` (try restart, login)"
            )),
        }
    }

    /// Log in to MCP server `server`, from `/mcp login` or the picker:
    /// queue the flow for the run loop, which owns the terminal the
    /// flow suspends.
    pub(crate) fn mcp_login(&mut self, server: &str) {
        if self.mcp_login_runner.is_none() {
            self.push_error("mcp login: unavailable in this host");
            return;
        }
        self.pending_login = Some(PendingLogin::Mcp(server.to_owned()));
    }

    /// Whether the engine expands `text` through MCP: it starts with the
    /// prompt command of a live server, or mentions a resource of a
    /// configured server. Such a prompt is queued, never steered,
    /// because the engine expands only prompts it starts a run with.
    pub(crate) fn mcp_expands(&self, text: &str) -> bool {
        let command = text
            .trim_start()
            .strip_prefix('/')
            .and_then(|c| c.split_whitespace().next())
            .and_then(|token| token.split_once(':'));
        if let Some((server, prompt)) = command
            && self.mcp_servers.iter().any(|s| {
                s.name == server
                    && s.status == kage_core::protocol::McpServerStatus::Connected
                    && s.prompts.iter().any(|p| p.name == prompt)
            })
        {
            return true;
        }
        text.split_whitespace().any(|word| {
            word.strip_prefix('@')
                .and_then(|w| w.split_once(':'))
                .is_some_and(|(server, uri)| {
                    !uri.is_empty() && self.mcp_servers.iter().any(|s| s.name == server)
                })
        })
    }

    /// Handle `:permission [mode]`: dispatch the override request,
    /// or report the current mode when called with no argument.
    pub(crate) fn run_permission_command(&mut self, rest: &str) {
        let arg = rest.trim();
        let parsed = match arg {
            "" => None,
            "allow" => Some(Some(kage_core::permissions::PermissionAction::Allow)),
            "default" => Some(None),
            "ask" => Some(Some(kage_core::permissions::PermissionAction::Ask)),
            "deny" => Some(Some(kage_core::permissions::PermissionAction::Deny)),
            other => {
                self.push_error(format!(
                    "permission: unknown mode `{other}` (try allow, ask, deny or default)"
                ));
                return;
            }
        };
        let Some(mode) = parsed else {
            let current = self
                .session_usage
                .as_ref()
                .and_then(|u| lock(u).permission_mode);
            let label = match current {
                Some(kage_core::permissions::PermissionAction::Allow) => {
                    "allow (yolo: every call runs, configured denies still deny)".to_owned()
                }
                Some(kage_core::permissions::PermissionAction::Ask) => "ask".to_owned(),
                Some(kage_core::permissions::PermissionAction::Deny) => "deny".to_owned(),
                _ => "default (configured rules)".to_owned(),
            };
            self.push_info(format!("permission mode: {label}"));
            return;
        };
        let _ = self.send_request(RunRequest::SetPermissionMode(mode));
    }

    /// Handle `/swarm [on|off|task]`: flip the session's swarm mode,
    /// or run one task as a one-shot swarm that turns itself off when
    /// the run ends.
    pub(crate) fn run_swarm_command(&mut self, rest: &str) {
        let arg = rest.trim();
        match arg {
            "" => {
                self.push_error("swarm: usage `/swarm on`, `/swarm off` or `/swarm <task>`");
            }
            "on" | "off" => {
                self.swarm_oneshot = None;
                let _ = self.send_request(RunRequest::SwarmMode { on: arg == "on" });
            }
            task => {
                if self.session_running(None) {
                    self.push_error("swarm: stop the current run first");
                    return;
                }
                self.swarm_oneshot = Some(task.to_owned());
                let _ = self.send_request(RunRequest::SwarmMode { on: true });
                let _ = self.send_request(RunRequest::Submit {
                    text: task.to_owned(),
                    images: Vec::new(),
                    queue: false,
                    session: None,
                });
            }
        }
    }

    /// Handle `/goal <text>` and `/goal clear`: set the goal the session
    /// works toward, or clear it.
    pub(crate) fn run_goal_command(&mut self, rest: &str) {
        match rest.trim() {
            "" => self.push_error("goal: usage `/goal <what done looks like>` or `/goal clear`"),
            "clear" => {
                let _ = self.send_request(RunRequest::SetGoal { goal: None });
                self.push_info("goal cleared".to_owned());
            }
            goal => {
                let _ = self.send_request(RunRequest::SetGoal {
                    goal: Some(goal.to_owned()),
                });
                self.push_info(format!(
                    "goal set: {goal}. Sent to the agent; kage checks it after each turn and \
                     keeps going until it is met."
                ));
            }
        }
    }

    /// Handle `/plan [on|off|task]`: flip the session's plan mode, or
    /// turn it on and submit a task to plan. A bare `/plan` toggles it.
    pub(crate) fn run_plan_command(&mut self, rest: &str) {
        match rest.trim() {
            "" => {
                let on = !self.plan_mode();
                let _ = self.send_request(RunRequest::PlanMode { on });
            }
            arg @ ("on" | "off") => {
                let _ = self.send_request(RunRequest::PlanMode { on: arg == "on" });
            }
            task => {
                if self.session_running(None) {
                    self.push_error("plan: stop the current run first");
                    return;
                }
                let _ = self.send_request(RunRequest::PlanMode { on: true });
                let _ = self.send_request(RunRequest::Submit {
                    text: task.to_owned(),
                    images: Vec::new(),
                    queue: false,
                    session: None,
                });
            }
        }
    }

    /// Whether the session's plan mode is on, as the engine last said.
    pub(crate) fn plan_mode(&self) -> bool {
        self.session_usage.as_ref().is_some_and(|u| lock(u).plan)
    }

    /// Turn swarm mode back off after a one-shot `/swarm <task>` run.
    pub(crate) fn end_swarm_oneshot(&mut self) {
        if self.swarm_oneshot.take().is_some() {
            let _ = self.send_request(RunRequest::SwarmMode { on: false });
        }
    }

    pub(crate) fn push_error(&mut self, msg: impl Into<String>) {
        let mut buf = lock(&self.buffer);
        buf.push_custom("kage:error", msg, false);
    }

    /// Push `text` to the system clipboard: the OSC52 escape for the
    /// terminal, and `arboard` on a background thread for terminals
    /// that ignore OSC52. Returns the number of chars written (0 for
    /// empty input, which is a no-op). A copy is best-effort, so
    /// failures are silently dropped.
    pub(crate) fn copy_to_clipboard(text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
        let _ = stdout.flush();
        // Tests must not overwrite the developer's real clipboard.
        #[cfg(not(test))]
        set_os_clipboard(text.to_owned());
        text.chars().count()
    }

    /// Copy the active screen selection to the clipboard, or the
    /// focused block's source when nothing is selected, and clear the
    /// selection.
    pub(crate) fn yank_screen_selection(&mut self) {
        if self.screen_selection.is_none() {
            // No selection: `y` means "copy the focused response",
            // same raw-text path as `Y`.
            self.yank_focused_block();
            return;
        }
        let text = self.extract_selection_text();
        if text.is_empty() {
            self.clear_selection();
            return;
        }
        let n = Self::copy_to_clipboard(&text);
        self.clear_selection();
        self.notify(format!("copied {n} characters"));
    }

    pub(crate) fn clear_selection(&mut self) {
        self.screen_selection = None;
        self.captured_rows.clear();
    }

    /// Anchor a keyboard selection at the focused block's first
    /// visible row (or the viewport top if nothing's focused) and
    /// switch the mode to [`Mode::Visual`]. Subsequent
    /// [`InputAction::Visual*`] events move the cursor end.
    pub(crate) fn enter_visual_mode(&mut self) {
        let anchor = {
            let buf = lock(&self.buffer);
            let area_x = buf.last_area_x();
            let area_y = buf.last_area_y();
            let virtual_top = buf.last_virtual_top();
            let row = buf
                .effective_focus()
                .and_then(|idx| buf.screen_top_of(idx))
                .unwrap_or(area_y);
            let vrow = virtual_top.saturating_add(usize::from(row.saturating_sub(area_y)));
            (vrow, area_x)
        };
        self.captured_rows.clear();
        self.screen_selection = Some((anchor, anchor));
        self.input.switch_mode(Mode::Visual);
    }

    pub(crate) fn move_visual_cursor(&mut self, dvrow: i32, dcol: i32) {
        let Some((anchor, cursor)) = self.screen_selection else {
            return;
        };
        let (mut vrow, mut col) = cursor;
        if dvrow != 0 {
            let next = i64::try_from(vrow).unwrap_or(i64::MAX) + i64::from(dvrow);
            vrow = usize::try_from(next.max(0)).unwrap_or(0);
        }
        if dcol != 0 {
            let next = i32::from(col).saturating_add(dcol).max(0);
            col = u16::try_from(next).unwrap_or(u16::MAX);
        }
        self.screen_selection = Some((anchor, (vrow, col)));
        self.scroll_visual_cursor_into_view(vrow);
    }

    pub(crate) fn snap_visual_cursor_x(&mut self, target_col: i32) {
        let Some((anchor, cursor)) = self.screen_selection else {
            return;
        };
        let (vrow, _) = cursor;
        let col = if target_col <= 0 {
            0
        } else {
            let buf = lock(&self.buffer);
            buf.last_area_width().saturating_sub(1)
        };
        self.screen_selection = Some((anchor, (vrow, col)));
    }

    /// Keep the visual cursor on screen by adjusting buffer scroll.
    /// Cursor above the viewport top pins the viewport at the cursor
    /// row; below the bottom scrolls just past it. Otherwise no-op.
    pub(crate) fn scroll_visual_cursor_into_view(&mut self, cursor_vrow: usize) {
        let mut buf = lock(&self.buffer);
        let area_height = usize::from(buf.last_area_height());
        if area_height == 0 {
            return;
        }
        let visible_top = buf.last_virtual_top();
        let visible_bot = visible_top.saturating_add(area_height);
        if cursor_vrow < visible_top {
            buf.set_scroll(cursor_vrow);
        } else if cursor_vrow >= visible_bot {
            buf.set_scroll(visible_top + (cursor_vrow + 1 - visible_bot));
        }
    }

    /// Yank the entire content of the currently focused block by
    /// projecting its screen rows onto captured cells. Limitation:
    /// only rows that have been visible (and thus captured) since
    /// the last selection clear contribute text - tall blocks the
    /// user hasn't scrolled fully through return only the visible
    /// portion. Auto-scroll on entering visual covers the keyboard
    /// path; for `Y` we just use whatever cells we have right now.
    pub(crate) fn yank_focused_block(&mut self) {
        // Yank the block's raw source text from the model, not the
        // markdown-rendered screen cells: the user wants the original
        // assistant text (verbatim ```fences```, list bullets, etc.)
        // to paste elsewhere, not the syntect-styled reflow.
        let buf = lock(&self.buffer);
        let Some(idx) = buf.effective_focus() else {
            return;
        };
        let text = buf.block_text(idx).unwrap_or_default();
        drop(buf);
        let text = text.trim_end().to_owned();
        if text.is_empty() {
            return;
        }
        let n = Self::copy_to_clipboard(&text);
        self.notify(format!("copied {n} characters"));
    }

    /// Copy block `idx`'s raw source to the clipboard. Backs the
    /// context menu's Copy row; like `Y` but for an explicit block
    /// rather than whatever has focus.
    pub(crate) fn copy_block_raw(&mut self, idx: usize) {
        let text = {
            let buf = lock(&self.buffer);
            buf.block_text(idx).unwrap_or_default()
        };
        let text = text.trim_end();
        let n = Self::copy_to_clipboard(text);
        if n > 0 {
            self.notify(format!("copied {n} characters"));
        }
    }

    /// Run a context-menu action against the block it targeted.
    pub(crate) fn run_context_action(&mut self, action: ContextAction, block_idx: usize) {
        match action {
            ContextAction::Copy => self.copy_block_raw(block_idx),
        }
    }

    /// Right mouse press at screen `(col, row)`: open a context menu
    /// over the block under the cursor. A press over no block (or
    /// outside the buffer pane) dismisses any open menu instead.
    pub(crate) fn open_context_menu(&mut self, col: u16, row: u16) {
        let idx = {
            let buf = lock(&self.buffer);
            let area_y = buf.last_area_y();
            let area_h = buf.last_area_height();
            if row < area_y || row >= area_y.saturating_add(area_h) {
                None
            } else {
                buf.block_at_screen_row(row)
            }
        };
        self.context_menu = idx.map(|idx| ContextMenu::new(col, row, idx));
    }

    /// Left click while a context menu is open: a click on a row runs
    /// its action; a click anywhere else just dismisses. Either way
    /// the click is consumed here so it never also starts a drag
    /// selection on the buffer beneath.
    pub(crate) fn context_menu_click(&mut self, col: u16, row: u16) {
        if self.context_menu.is_none() {
            return;
        }
        let viewport = {
            let buf = lock(&self.buffer);
            ratatui::layout::Rect {
                x: buf.last_area_x(),
                y: buf.last_area_y(),
                width: buf.last_area_width(),
                height: buf.last_area_height(),
            }
        };
        let (hit, idx) = {
            let menu = self.context_menu.as_mut().expect("checked above");
            (menu.handle_click(viewport, col, row), menu.block_idx())
        };
        self.context_menu = None;
        if let Some(action) = hit {
            self.run_context_action(action, idx);
        }
    }

    /// Route a key into the open context menu. Modal while present:
    /// navigation and Esc are consumed, a stray key is swallowed, and
    /// activation runs the row's action then closes the menu.
    pub(crate) fn dispatch_context_menu_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let outcome = {
            let menu = self.context_menu.as_mut()?;
            menu.handle_key(key)
        };
        match outcome {
            ContextMenuOutcome::Navigated => {}
            ContextMenuOutcome::Dismissed => self.context_menu = None,
            ContextMenuOutcome::Activated(action) => {
                let idx = self.context_menu.as_ref().map(ContextMenu::block_idx);
                self.context_menu = None;
                if let Some(idx) = idx {
                    self.run_context_action(action, idx);
                }
            }
        }
        None
    }

    pub(crate) fn extract_selection_text(&self) -> String {
        let Some((anchor, cursor)) = self.screen_selection else {
            return String::new();
        };
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        // Copy exactly the captured cells under the selection: the
        // rendered text for the selected rows and columns, decoration
        // (chrome) cells dropped. Partial drags through any block -
        // assistant, thinking, tool - yield only what is highlighted,
        // never the whole block.
        let mut out = String::new();
        for vrow in start.0..=end.0 {
            let Some(grid_row) = self.captured_rows.get(&vrow) else {
                if !out.is_empty() {
                    out.push('\n');
                }
                continue;
            };
            let from_col = if vrow == start.0 {
                usize::from(start.1)
            } else {
                0
            };
            let to_col = if vrow == end.0 {
                usize::from(end.1).saturating_add(1)
            } else {
                grid_row.len()
            };
            let to_col = to_col.min(grid_row.len());
            if from_col >= to_col {
                if !out.is_empty() {
                    out.push('\n');
                }
                continue;
            }
            let slice: String = grid_row[from_col..to_col]
                .iter()
                .filter(|cell| !cell.decoration)
                .map(|cell| cell.ch)
                .collect();
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(slice.trim_end());
        }
        out
    }

    /// Render the live keymap per mode with the owner of each mapping,
    /// then the keys the editor grammar handles and the two hatches.
    pub(crate) fn push_keybindings(&mut self) {
        use kage_core::keymap::Mode as KeymapMode;
        let mut lines = vec!["key mappings (last set wins):".to_owned()];
        {
            let keymap = lock(&self.keymap);
            let entries = keymap.entries();
            for (mode, what) in [
                (KeymapMode::Global, "any editing state"),
                (KeymapMode::Insert, "insert and the modeless editor"),
                (KeymapMode::Normal, "vim normal mode"),
                (KeymapMode::Buffer, "vim normal mode, conversation pane"),
                (KeymapMode::Visual, "visual mode"),
            ] {
                let rows: Vec<String> = entries
                    .iter()
                    .filter(|e| e.mode == mode)
                    .map(|e| {
                        let rhs = match &e.mapping.rhs {
                            Rhs::Action {
                                name,
                                arg: Some(arg),
                            } => format!("action:{name}({arg})"),
                            Rhs::Action { name, arg: None } => format!("action:{name}"),
                            Rhs::Command(command) => format!(":{command}"),
                            Rhs::Lua(_) => "lua function".to_owned(),
                            Rhs::Nop => "<Nop>".to_owned(),
                        };
                        let lhs = crate::keymap::key_labels(e.lhs);
                        format!("  {lhs:<14} {rhs:<32} {}", e.mapping.owner)
                    })
                    .collect();
                if rows.is_empty() {
                    continue;
                }
                lines.push(String::new());
                lines.push(format!("{mode}: {what}"));
                lines.extend(rows);
            }
        }
        lines.push(String::new());
        lines.push(
            "built in (editor grammar; a mapping or <Nop> shadows these, del cannot remove them):"
                .to_owned(),
        );
        for row in [
            "vim motions, operators, counts, registers, r, undo, redo",
            "readline edits and the kill ring (ctrl+a/e/w/u/k/y, ctrl+/, alt+b/f/d, alt+backspace)",
            "enter submits (steers a running turn), shift+enter and alt+enter insert a newline, up and down walk history",
            "modeless esc interrupts the run, else esc esc rewinds",
            "vim esc, insert ctrl+o (expand a paste or fold), ctrl+g external editor",
            "modeless empty-prompt /, ! and ?, conversation pane i and a",
        ] {
            lines.push(format!("  {row}"));
        }
        lines.push(String::new());
        lines.push(
            "hatches (above every layer; they yield only to init.lua or config.toml):".to_owned(),
        );
        lines.push("  ctrl+q         quit".to_owned());
        lines.push("  ctrl+c         interrupt the run, else twice to quit".to_owned());

        let body = lines.join("\n");
        let mut buf = lock(&self.buffer);
        buf.push_custom("kage:help", body, false);
    }

    /// Render the `/usage` panel: session-wide token totals with the
    /// cache counters the footer hides, then the context-window bar.
    /// Reads the live [`SessionUsage`](crate::usage::SessionUsage)
    /// snapshot; an unwired host renders zeros.
    pub(crate) fn push_usage(&mut self) {
        let usage = self.session_usage_snapshot().unwrap_or_default();
        let body = usage_body(&usage);
        let mut buf = lock(&self.buffer);
        buf.push_custom("kage:usage", body, false);
    }

    /// Render every event a plugin can hook with `kage.on`, grouped
    /// by dispatch kind, sourced from the single
    /// [`kage_plugin::KNOWN_EVENTS`] catalog so it cannot drift from
    /// what the host actually fires.
    pub(crate) fn push_events(&mut self) {
        let mut lines = vec![
            "events (kage.on(name, fn)); kinds: notification | transform | predicate | veto"
                .to_owned(),
        ];
        for kind in ["notification", "transform", "predicate", "veto"] {
            lines.push(String::new());
            lines.push(format!("{kind}:"));
            for (name, _, desc) in kage_plugin::KNOWN_EVENTS
                .iter()
                .filter(|(_, k, _)| *k == kind)
            {
                lines.push(format!("  {name:<24} {desc}"));
            }
        }
        let body = lines.join("\n");
        let mut buf = lock(&self.buffer);
        buf.push_custom("kage:help", body, false);
    }
}

/// Hand `text` to the OS clipboard off the UI thread. On X11 and
/// Wayland the thread keeps serving the text until another client
/// takes the clipboard over, since the contents vanish with their
/// owner.
#[cfg(not(test))]
fn set_os_clipboard(text: String) {
    std::thread::spawn(move || {
        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            return;
        };
        #[cfg(all(
            unix,
            not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
        ))]
        let set = {
            use arboard::SetExtLinux;
            clipboard.set().wait()
        };
        #[cfg(not(all(
            unix,
            not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
        )))]
        let set = clipboard.set();
        let _ = set.text(text);
    });
}

/// Width of the `/usage` context bar in cells. Fixed: the panel is
/// static transcript text with no layout pass to measure against.
const USAGE_BAR_WIDTH: usize = 20;

use crate::usage::compact_tokens;

/// Cells of [`usage_bar`] that fit `used` of `total`, rounded to the
/// nearest cell and clamped to the width. Integer math: token counts
/// never touch `f64`.
fn usage_bar_fill(used: u64, total: u64, width: usize) -> usize {
    if total == 0 || used == 0 {
        return 0;
    }
    let width_u64 = width as u64;
    let filled = (used.saturating_mul(width_u64) + total / 2) / total;
    usize::try_from(filled.min(width_u64)).unwrap_or(width)
}

/// Fixed-width context fill bar: full blocks per used cell, light
/// shade per free cell (escapes: the source tree is ASCII-only).
fn usage_bar(used: u64, total: u64) -> String {
    let filled = usage_bar_fill(used, total, USAGE_BAR_WIDTH);
    let mut bar = String::with_capacity(USAGE_BAR_WIDTH + 2);
    bar.push('[');
    for _ in 0..filled {
        bar.push('\u{2588}');
    }
    for _ in filled..USAGE_BAR_WIDTH {
        bar.push('\u{2591}');
    }
    bar.push(']');
    bar
}

/// The `/usage` panel body for a [`SessionUsage`](crate::usage::SessionUsage)
/// snapshot. Pure over the snapshot so tests pin the text.
fn usage_body(usage: &crate::usage::SessionUsage) -> String {
    let mut lines = vec!["Session usage".to_owned()];
    lines.push(format!(
        "  {}",
        if usage.model.is_empty() {
            "(no model)"
        } else {
            usage.model.as_str()
        }
    ));
    let cost = if usage.total_cost > 0.0 {
        format!("  (${:.2})", usage.total_cost)
    } else {
        String::new()
    };
    lines.push(format!(
        "    input {}  output {}  cache read {}  cache write {}  total {}{cost}",
        compact_tokens(usage.input_tokens),
        compact_tokens(usage.output_tokens),
        compact_tokens(usage.cache_read_tokens),
        compact_tokens(usage.cache_write_tokens),
        compact_tokens(usage.total_tokens()),
    ));
    lines.push("Context window".to_owned());
    if usage.context_window == 0 {
        lines.push("  (unknown window)".to_owned());
    } else {
        let percent = usage
            .current_context
            .saturating_mul(100)
            .checked_div(usage.context_window)
            .unwrap_or(100);
        let about = if usage.context_estimated { "~" } else { "" };
        lines.push(format!(
            "  {}  {about}{percent}%  ({about}{} / {})",
            usage_bar(usage.current_context, usage.context_window),
            compact_tokens(usage.current_context),
            compact_tokens(usage.context_window),
        ));
    }
    lines.join("\n")
}
