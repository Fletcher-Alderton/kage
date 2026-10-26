//! Theme, clipboard/paste, and overlay (picker/settings/tree) handling.

use super::*;
use kage_core::options::{OptionDef, option_from_json, save_options};

use crate::view::{UnicodeWidthStr as _, pad_to_width};

impl App {
    pub(crate) fn run_theme_command(&mut self, rest: &str) {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or("");
        let sub_rest = parts.next().unwrap_or("").trim();
        match sub {
            "" | "current" => {
                let cur = crate::theme::current().name.clone();
                self.push_info(format!("theme: {cur} (`/theme list` shows the others)"));
            }
            "list" => {
                let cur = crate::theme::current().name.clone();
                let names = crate::theme::Theme::available_names(self.themes_dir.as_deref())
                    .iter()
                    .map(|n| {
                        if *n == cur {
                            format!("* {n}")
                        } else {
                            format!("  {n}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut buf = lock(&self.buffer);
                buf.push_custom("kage:theme", format!("themes:\n{names}"), false);
            }
            "set" => {
                if sub_rest.is_empty() {
                    self.push_error("theme set: usage `/theme set <name>`");
                    return;
                }
                self.set_option("theme", OptionValue::Str(sub_rest.to_owned()));
            }
            other => {
                self.push_error(format!(
                    "theme: unknown subcommand `{other}` (try list, set, current)"
                ));
            }
        }
    }

    /// Set option `name` with source `runtime`, through the plugin
    /// runtime when one is wired so `option_set` fires. The change
    /// applies once it reaches the store. A rejected value surfaces
    /// inline.
    pub(crate) fn set_option(&mut self, name: &str, value: OptionValue) {
        let result = match &self.option_setter {
            Some(set) => set(name, value),
            None => lock(&self.options)
                .set(name, value, OptionSource::Runtime)
                .map(drop)
                .map_err(|e| e.to_string()),
        };
        if let Err(err) = result {
            self.push_error(err);
        }
    }

    /// Apply the option changes queued in the store since the last
    /// call. Returns whether anything was applied.
    pub(crate) fn apply_option_changes(&mut self) -> bool {
        let changes = lock(&self.options).take_changes();
        let mut applied = false;
        for change in changes.into_iter().filter(|c| c.old != c.new) {
            let announce = change.source == OptionSource::Runtime;
            applied |= self.apply_option(change.name, &change.new, announce);
        }
        applied
    }

    /// Apply one live option value. Options without a live effect
    /// return `false`. `announce` toasts a theme switch, which the
    /// palette picks up from the highlight table.
    pub(crate) fn apply_option(&mut self, name: &str, value: &OptionValue, announce: bool) -> bool {
        match (name, value) {
            ("theme", OptionValue::Str(theme)) => {
                if announce {
                    self.notify(format!("theme: {theme}"));
                }
            }
            ("mouse", OptionValue::Bool(on)) => self.pending_mouse_capture = Some(*on),
            ("editor", OptionValue::Str(editor)) => self.input.set_modeless(editor == "modeless"),
            ("timeoutlen", OptionValue::Int(ms)) => {
                let ms = u64::try_from(*ms).unwrap_or_default();
                self.sequencer.set_timeout(Duration::from_millis(ms));
            }
            ("input_min_lines" | "input_max_lines", _) => {
                let (min, max) = {
                    let store = lock(&self.options);
                    let rows = |name| {
                        store
                            .get(name)
                            .and_then(OptionValue::as_int)
                            .and_then(|n| u16::try_from(n).ok())
                            .unwrap_or(1)
                    };
                    (rows("input_min_lines"), rows("input_max_lines"))
                };
                crate::layout::set_input_bounds(min, max);
            }
            _ => return false,
        }
        true
    }

    /// Queue an async OS-clipboard image read (Ctrl+V, or `:attach`
    /// with no path). The arboard read can block for hundreds of ms
    /// on some compositors or a stalled clipboard owner, so it runs
    /// on a background thread; the result surfaces through
    /// [`Self::drain_clipboard_attach`] on a later loop pass.
    pub(crate) fn request_clipboard_attach(&mut self) {
        let tx = self.attach_tx.clone();
        std::thread::spawn(move || {
            let result = crate::image::clipboard_image()
                .and_then(|bytes| crate::image::from_bytes(&bytes, "clipboard"));
            let _ = tx.send(result);
        });
    }

    /// Drain completed async clipboard attaches. Success attaches the
    /// image and toasts; failure pushes the real reason inline so a
    /// setup that does not work is diagnosable, never a silent no-op.
    pub(crate) fn drain_clipboard_attach(&mut self) {
        while let Ok(result) = self.attach_rx.try_recv() {
            match result {
                Ok(att) => self.attach(att),
                Err(e) => self.push_error(format!("paste image: {e}")),
            }
        }
    }

    /// Handle a bracketed paste. If the pasted text is just a path to
    /// an existing image (a drag-drop, or a copied file), attach it
    /// instead of inserting the raw path as prompt text; otherwise
    /// route the text to the active surface: a plugin overlay's
    /// `handle_paste`, the `:` cmdline, or the `/` search line. A
    /// modal that does not accept text swallows
    /// the paste so it cannot land in the hidden main input and
    /// silently vanish. With no overlay open the main input receives
    /// it verbatim. A path that looks like an image but fails to load
    /// surfaces the error rather than silently pasting the path.
    pub(crate) fn handle_paste(&mut self, text: &str) {
        // A copied/dragged image *file* arrives as its path.
        if let Some(path) = crate::image::path_if_image(text) {
            match crate::image::load_path(&path) {
                Ok(att) => self.attach(att),
                Err(e) => self.push_error(format!("attach: {e}")),
            }
            return;
        }
        // A copied screenshot/image can't ride in the paste text; if
        // the terminal does deliver an empty bracketed paste for it,
        // treat that as an image-paste attempt (Ctrl+V is also
        // intercepted directly for terminals that send no event).
        if text.trim().is_empty() {
            self.request_clipboard_attach();
            return;
        }
        if let Some(overlay) = self.plugin_overlay.as_mut() {
            overlay.handle_paste(text);
            return;
        }
        if let Some(panel) = self.approval_panel.as_mut() {
            panel.paste(text);
            return;
        }
        if self.context_menu.is_some()
            || self.picker.is_some()
            || self.settings_overlay.is_some()
            || self.session_tree.is_some()
            || self.agents_overlay.is_some()
        {
            return;
        }
        let registry = self.command_registry();
        if let Some(cl) = self.cmdline.as_mut() {
            let resolver = AppResolver {
                models: &self.model_choices,
                plugin_commands: &self.plugin_commands,
                sessions: self.session_lister.as_ref(),
                themes_dir: self.themes_dir.as_deref(),
            };
            cl.paste_str(text, &registry, &resolver);
            return;
        }
        if let Some(line) = self.search_line.as_mut() {
            let empty: [&CommandSpec; 0] = [];
            line.paste_str(text, &empty, &EmptyResolver);
            self.preview_search();
            return;
        }
        self.input.paste(text);
    }

    /// `:attach [path]` - queue an image for the next prompt. With a
    /// `path`, load that file; with no argument, pull the image off
    /// the OS clipboard (off-thread, like Ctrl+V). Every failure path
    /// is explained inline (no path + nothing on the clipboard, no
    /// clipboard helper, bad file, unsupported format, too large) so
    /// a non-working setup is diagnosable rather than silent.
    pub(crate) fn attach_image_path(&mut self, rest: &str) {
        let path = rest.trim();
        if path.is_empty() {
            self.request_clipboard_attach();
            return;
        }
        match crate::image::load_path(&kage_core::fsutil::expand_tilde(std::path::Path::new(path)))
        {
            Ok(att) => self.attach(att),
            Err(e) => self.push_error(format!("attach: {e}")),
        }
    }

    /// Queue `att` for the next prompt and say so, warning when the
    /// active model is known not to accept images: the engine drops
    /// them from the prompt rather than let the provider fail.
    fn attach(&mut self, att: crate::image::AttachedImage) {
        let note = att.placeholder();
        self.input.attach_image(att);
        let text_only = self.session_usage.as_ref().and_then(|usage| {
            let usage = lock(usage);
            usage
                .input
                .lacks(kage_core::Modality::Image)
                .then(|| usage.model.clone())
        });
        let (text, kind) = match text_only {
            Some(model) => (
                format!("{model} does not accept images; it will not be sent"),
                ToastKind::Warning,
            ),
            None => (format!("attached {note}"), ToastKind::Info),
        };
        if let Some(toasts) = &self.toasts {
            toast::push_toast(
                toasts,
                Toast::with_kind(text, kind, toast::DEFAULT_TOAST_DURATION),
            );
        }
    }

    /// Show the answer to a command the user ran, such as the current
    /// theme or an empty list, as a notice in the conversation. A toast
    /// confirms a change instead.
    pub(crate) fn push_info(&mut self, msg: impl Into<String>) {
        lock(&self.buffer).push_custom("kage:help", msg, false);
    }

    pub(crate) fn notify(&mut self, msg: impl Into<String>) {
        let Some(toasts) = &self.toasts else {
            return;
        };
        toast::push_toast(
            toasts,
            Toast::with_kind(msg, ToastKind::Info, toast::DEFAULT_TOAST_DURATION),
        );
    }

    pub(crate) fn run_mouse_command(&mut self, rest: &str) {
        match rest {
            "off" | "disable" => {
                self.set_option("mouse", OptionValue::Bool(false));
                self.notify("mouse capture off \u{b7} the terminal selects and copies text");
            }
            "on" | "enable" => {
                self.set_option("mouse", OptionValue::Bool(true));
                self.notify("mouse capture on \u{b7} drag selects text inside kage");
            }
            "toggle" | "" => {
                let now_enabled =
                    lock(&self.options).get("mouse") != Some(&OptionValue::Bool(true));
                self.set_option("mouse", OptionValue::Bool(now_enabled));
                let state = if now_enabled { "on" } else { "off" };
                self.notify(format!("mouse capture {state}"));
            }
            other => {
                self.push_error(format!("mouse: unknown arg `{other}` (try on/off/toggle)"));
            }
        }
    }

    /// Open the model picker with the current session's model marked `*`.
    pub(crate) fn open_model_picker(&mut self) {
        if self.model_choices.is_empty() {
            self.notify("no models available. Run /login to connect a provider");
            return;
        }
        let active = self.model_id(self.main_usage().as_ref());
        let mut items = self.model_choices.clone();
        if let Some(active) = active {
            let active = kage_core::canonical_model(&active);
            for item in &mut items {
                item.badge = Some(if kage_core::canonical_model(&item.value) == active {
                    '*'
                } else {
                    ' '
                });
            }
        }
        self.picker =
            Some(OverlayPicker::new("switch model", items).with_note("/login to add a provider"));
        self.picker_kind = Some(PickerKind::Model);
    }

    /// Title for the session picker, encoding the active scope and
    /// the `ctrl+a` toggle so the binding is discoverable in-place.
    pub(crate) fn session_picker_title(all: bool) -> &'static str {
        if all {
            "resume session \u{b7} all dirs \u{b7} ctrl+a for this dir"
        } else {
            "resume session \u{b7} this dir \u{b7} ctrl+a for all dirs"
        }
    }

    /// (Re)build the session picker for the current
    /// [`Self::session_scope_all`] scope, with the session on screen
    /// marked `*`. `allow_empty` keeps the modal open with no rows
    /// (used by the toggle so the user can flip back); the initial
    /// open passes `false` so `Ctrl+S` with nothing to resume is a
    /// no-op rather than an empty dialog.
    pub(crate) fn open_session_picker(&mut self, allow_empty: bool) {
        let Some(lister) = self.session_lister.as_ref() else {
            return;
        };
        let items = lister(self.session_scope_all);
        self.show_session_picker(items, allow_empty);
    }

    /// Paint the session picker on `items`, with the session on screen
    /// marked `*`. `allow_empty` keeps the modal open with no rows
    /// (used by the toggle so the user can flip back); the initial
    /// open passes `false` so `Ctrl+S` with nothing to resume is a
    /// no-op rather than an empty dialog.
    pub(crate) fn show_session_picker(&mut self, items: Vec<PickItem>, allow_empty: bool) {
        let current = self.active_session.map(|id| id.to_string());
        let items: Vec<_> = items
            .into_iter()
            .map(|item| {
                let stem = std::path::Path::new(&item.value)
                    .file_stem()
                    .and_then(|s| s.to_str());
                if stem.is_some() && stem == current.as_deref() {
                    item.with_badge('*')
                } else {
                    item
                }
            })
            .collect();
        if items.is_empty() && !allow_empty {
            return;
        }
        let title = Self::session_picker_title(self.session_scope_all);
        self.picker = Some(OverlayPicker::new(title, items));
        self.picker_kind = Some(PickerKind::Session);
    }

    /// Open the F3 history-jump picker: one row per user prompt,
    /// assistant reply, tool call, and notice, newest first.
    /// Resolving scrolls the focused block into view.
    pub(crate) fn open_jump_picker(&mut self) {
        let items: Vec<crate::picker::PickItem> = lock(&self.buffer)
            .jump_targets(72)
            .into_iter()
            .rev()
            .map(|(idx, label)| crate::picker::PickItem {
                value: idx.to_string(),
                label,
                badge: None,
                group: None,
                right: None,
            })
            .collect();
        if items.is_empty() {
            return;
        }
        self.picker = Some(OverlayPicker::new_ordered("jump to message", items));
        self.picker_kind = Some(PickerKind::Jump);
    }

    /// Open the `/mcp` picker: one row per configured server of the
    /// main session with its status, and what it offers, why it failed
    /// or that it needs a login.
    pub(crate) fn open_mcp_picker(&mut self) {
        use kage_core::protocol::McpServerStatus;
        if self.mcp_servers.is_empty() {
            self.push_info(
                "no MCP servers configured. Add one under [mcp.servers.<name>] in \
                 config.toml, then restart kage.",
            );
            return;
        }
        let width = self
            .mcp_servers
            .iter()
            .map(|s| s.name.width())
            .max()
            .unwrap_or(0);
        let items = self
            .mcp_servers
            .iter()
            .map(|server| {
                let (status, detail) = match &server.status {
                    McpServerStatus::Connected => ("connected", mcp_offer(server)),
                    McpServerStatus::Starting => ("starting", "waiting for the server".to_owned()),
                    McpServerStatus::NeedsAuth => ("needs login", "enter to log in".to_owned()),
                    McpServerStatus::Failed { error } => {
                        ("failed", error.lines().next().unwrap_or("").to_owned())
                    }
                };
                let name = pad_to_width(&server.name, width);
                crate::picker::PickItem::simple(server.name.clone())
                    .with_label(format!("{name}  {status:<11}  {detail}"))
            })
            .collect();
        self.picker = Some(
            OverlayPicker::new_ordered("mcp servers", items)
                .with_note("enter restarts the server, or logs in when it needs a login"),
        );
        self.picker_kind = Some(PickerKind::Mcp);
    }

    /// Ask the engine for the prompts a rewind can target; the picker
    /// opens when [`kage_core::protocol::HostEvent::PromptList`]
    /// arrives.
    pub(crate) fn begin_rewind(&mut self) {
        let _ = self.send_request(RunRequest::ListPrompts);
    }

    /// Paint the rewind picker on the engine's prompt list: the
    /// branch tree oldest first, branches indented one level per
    /// rewind they were re-prompted through, the active head badged
    /// and selected. Picking one keeps that prompt's full turn, its
    /// reply included, and abandons what follows; the session
    /// continues from the turn's end.
    pub(crate) fn show_rewind_picker(&mut self, prompts: Vec<kage_core::protocol::PastPrompt>) {
        if prompts.is_empty() {
            self.notify("nothing to rewind");
            return;
        }
        let mut selected = prompts.len() - 1;
        let items = prompts
            .into_iter()
            .enumerate()
            .map(|(index, prompt)| {
                if prompt.active {
                    selected = index;
                }
                let indent = "  ".repeat(prompt.depth as usize);
                let width = REWIND_LABEL_WIDTH.saturating_sub(indent.len());
                let mut item = crate::picker::PickItem::simple(prompt.at)
                    .with_label(format!("{indent}{}", rewind_label(&prompt.prompt, width)));
                if prompt.active {
                    item = item.with_badge('*');
                }
                item
            })
            .collect();
        self.picker = Some(
            OverlayPicker::new_ordered("rewind to prompt", items)
                .with_selected(selected)
                .with_note("enter keeps the picked turn and drops what follows"),
        );
        self.picker_kind = Some(PickerKind::Rewind);
    }

    pub(crate) fn dispatch_picker_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        // Ctrl+A toggles the session picker between this-directory and
        // all-directories scope, rebuilding it in place. Intercepted
        // before the picker sees the key (its Char arm would
        // otherwise treat `a` as search input).
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        if self.picker_kind == Some(PickerKind::Session)
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('a'))
        {
            self.session_scope_all = !self.session_scope_all;
            self.open_session_picker(true);
            return None;
        }
        let picker = self.picker.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(picker, key) {
            OverlayAction::Stay | OverlayAction::PropagateKey => {}
            OverlayAction::Close => {
                self.picker = None;
                self.picker_kind = None;
            }
            OverlayAction::Resolve(value) => {
                let kind = self.picker_kind;
                self.picker = None;
                self.picker_kind = None;
                let serde_json::Value::String(value) = value else {
                    return None;
                };
                match kind {
                    Some(PickerKind::Model) => {
                        let _ = self.send_request(RunRequest::SwitchModel(value));
                    }
                    Some(PickerKind::Session) => {
                        let _ = self.send_request(RunRequest::ResumeSession(
                            std::path::PathBuf::from(value),
                        ));
                    }
                    Some(PickerKind::Mcp) => {
                        let needs_login = self.mcp_servers.iter().any(|s| {
                            s.name == value
                                && s.status == kage_core::protocol::McpServerStatus::NeedsAuth
                        });
                        if needs_login {
                            self.mcp_login(&value);
                        } else {
                            let _ = self.send_request(RunRequest::RestartMcp(value));
                        }
                    }
                    Some(PickerKind::Jump) => {
                        // Focus drives the renderer's scroll-into-view
                        // pass; the index always came from a live
                        // block list, but a stale pick must not panic.
                        if let Ok(idx) = value.parse::<usize>() {
                            lock(&self.buffer).set_focus(Some(idx));
                        }
                    }
                    Some(PickerKind::Rewind) => {
                        let _ = self.send_request(RunRequest::Rewind { at: value });
                    }
                    None => {}
                }
            }
        }
        None
    }

    /// Drive the settings dialog. Each edit applies live, `Esc`
    /// restores the values it opened with, and a resolve persists.
    pub(crate) fn dispatch_settings_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let overlay = self.settings_overlay.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(overlay, key) {
            OverlayAction::Stay | OverlayAction::PropagateKey => {
                if let Some((name, value)) = overlay.take_edit() {
                    self.set_option(name, value);
                }
            }
            OverlayAction::Close => {
                let reverts = overlay.reverts();
                self.settings_overlay = None;
                for (name, value) in reverts {
                    self.update_option(name, value);
                }
            }
            OverlayAction::Resolve(value) => {
                self.settings_overlay = None;
                self.apply_settings(&value);
            }
        }
        None
    }

    /// Open the `?` keyboard reference, built from the live keymap for
    /// the active editor style. Scroll-only: closing is the only
    /// outcome.
    pub(crate) fn open_help(&mut self) {
        let modeless = self.input.is_modeless();
        let editor = if modeless {
            kage_core::config::EditorMode::Modeless
        } else {
            kage_core::config::EditorMode::Vim
        };
        let groups = help_groups(&lock(&self.keymap), editor);
        self.help_overlay = Some(crate::overlay::HelpOverlay::new(&groups, modeless));
    }

    /// Drive the help reference. `Close` and `Resolve` both dismiss:
    /// there is nothing to resolve.
    pub(crate) fn dispatch_help_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        let Some(overlay) = self.help_overlay.as_mut() else {
            return;
        };
        match crate::overlay::OverlayWidget::handle_key(overlay, key) {
            crate::overlay::OverlayAction::Close | crate::overlay::OverlayAction::Resolve(_) => {
                self.help_overlay = None;
            }
            crate::overlay::OverlayAction::Stay | crate::overlay::OverlayAction::PropagateKey => {}
        }
    }

    /// Open the `/settings` dialog over the current option values.
    pub(crate) fn open_settings(&mut self) {
        let themes = crate::theme::Theme::available_names(self.themes_dir.as_deref());
        self.settings_overlay = Some(SettingsOverlay::new(&lock(&self.options), themes));
    }

    /// [`Self::set_option`], skipped when `name` already holds `value`.
    fn update_option(&mut self, name: &str, value: OptionValue) {
        if lock(&self.options).get(name) != Some(&value) {
            self.set_option(name, value);
        }
    }

    /// Apply the settings-dialog result `{ name: value }`: set every
    /// valid option not yet applied, persist them to the user config
    /// file (comment-preserving), and move the live session to a new
    /// thinking level once it is saved. An empty resolve means nothing
    /// changed.
    pub(crate) fn apply_settings(&mut self, value: &serde_json::Value) {
        self.apply_settings_at(value, kage_core::config::Config::default_path());
    }

    /// [`Self::apply_settings`] against an explicit config path
    /// (`None` skips persistence). Lets tests pin the file instead
    /// of the process environment.
    pub(crate) fn apply_settings_at(
        &mut self,
        value: &serde_json::Value,
        path: Option<std::path::PathBuf>,
    ) {
        let edits: Vec<(&OptionDef, OptionValue)> = value
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(name, value)| {
                let def = kage_core::options::find(name)?;
                Some((def, def.validate(option_from_json(value)?).ok()?))
            })
            .collect();
        if edits.is_empty() {
            self.notify("settings: nothing changed");
            return;
        }
        for (def, value) in &edits {
            self.update_option(def.name, value.clone());
        }
        let Some(path) = path else {
            self.push_error("settings: no home directory; not persisted");
            return;
        };
        if let Err(e) = save_options(&path, &edits) {
            self.push_error(format!("settings: save failed: {e}"));
            return;
        }
        self.notify("settings saved");
        let level = edits.iter().find_map(|(def, value)| match value {
            OptionValue::Str(level) if def.name == "thinking_level" && !level.is_empty() => {
                Some(level.clone())
            }
            _ => None,
        });
        if let Some(level) = level {
            let _ = self.send_request(RunRequest::SetThinkingLevel(level));
        }
    }

    /// Open the `:tree` session browser, querying the wired source,
    /// with the session on screen marked.
    pub(crate) fn open_session_tree(&mut self) {
        let Some(source) = self.session_tree_source.as_ref() else {
            self.push_error("tree: session browser unavailable");
            return;
        };
        let current = self.active_session.map(|id| id.to_string());
        let mut nodes = source();
        for node in &mut nodes {
            node.is_current |= current.as_deref() == Some(node.id.as_str());
        }
        if nodes.is_empty() {
            self.push_info("no sessions to browse yet");
            return;
        }
        self.session_tree = Some(SessionTreeOverlay::new(nodes));
    }

    pub(crate) fn dispatch_session_tree_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let overlay = self.session_tree.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(overlay, key) {
            OverlayAction::Stay | OverlayAction::PropagateKey => {}
            OverlayAction::Close => {
                self.session_tree = None;
            }
            OverlayAction::Resolve(value) => {
                self.session_tree = None;
                let action = value.get("action").and_then(|v| v.as_str()).unwrap_or("");
                let path = value.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    return None;
                }
                let path = std::path::PathBuf::from(path);
                match action {
                    "resume" => {
                        let _ = self.send_request(RunRequest::ResumeSession(path));
                    }
                    "fork" => {
                        let _ = self.send_request(RunRequest::ForkSessionFile(path));
                    }
                    "delete" => {
                        // Stage the deletion behind a confirmation;
                        // the request is only sent once the confirm
                        // overlay resolves Yes.
                        self.pending_tree_delete = Some(path);
                        self.plugin_overlay = Some(Box::new(crate::overlay::ConfirmOverlay::new(
                            "delete session",
                            "Delete this session? This cannot be undone.",
                        )));
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// Open the agents overlay over the main session's agents, with the
    /// agent on screen selected. A session without agents only says so.
    pub(crate) fn open_agents(&mut self) {
        let rows = self.agents_overlay_rows();
        if rows.len() < 2 {
            self.push_info("no agents in this session yet");
            return;
        }
        self.agents_overlay = Some(crate::overlay::AgentsOverlay::new(rows, self.focus));
    }

    /// Open the todo-list viewer over the transcript's latest
    /// `todo_list` write. A session that never tracked a plan only
    /// says so.
    pub(crate) fn open_todo_list(&mut self) {
        let strip = crate::view::todo::from_blocks(lock(&self.buffer).blocks());
        if !strip.tracked {
            self.push_info("no todo list in this session yet");
            return;
        }
        self.todo_list_overlay = Some(crate::overlay::TodoListOverlay::new(&strip));
    }

    /// Drive the todo-list viewer. A read-only surface: any key it
    /// does not scroll with closes it.
    pub(crate) fn dispatch_todo_list_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        let Some(overlay) = self.todo_list_overlay.as_mut() else {
            return;
        };
        match crate::overlay::OverlayWidget::handle_key(overlay, key) {
            crate::overlay::OverlayAction::Close | crate::overlay::OverlayAction::Resolve(_) => {
                self.todo_list_overlay = None;
            }
            crate::overlay::OverlayAction::Stay | crate::overlay::OverlayAction::PropagateKey => {}
        }
    }

    /// Drive the agents overlay. Enter closes it on the selected
    /// session's view, `x` stops the selected agent and `X` every live
    /// agent, keeping it open.
    /// Keys it does not use reach an approval panel under it.
    pub(crate) fn dispatch_agents_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let overlay = self.agents_overlay.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(overlay, key) {
            OverlayAction::Stay => {}
            OverlayAction::PropagateKey => {
                if self.approval_panel.is_some() {
                    return self.dispatch_permission_key(key);
                }
            }
            OverlayAction::Close => self.agents_overlay = None,
            OverlayAction::Resolve(action) => {
                let session = overlay.selected();
                match action.as_str() {
                    Some("open") => {
                        self.agents_overlay = None;
                        match session {
                            Some(session) => self.focus_agent(session),
                            None => self.set_focus(None),
                        }
                    }
                    Some("stop") => {
                        let _ = self.send_request(RunRequest::Cancel { session });
                    }
                    Some("stop-all") => {
                        let live: Vec<_> = self
                            .agents_overlay_rows()
                            .into_iter()
                            .filter(|row| row.state.is_live())
                            .filter_map(|row| row.session)
                            .collect();
                        for session in live {
                            let _ = self.send_request(RunRequest::Cancel {
                                session: Some(session),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// Drive the active plugin dialog overlay (`kage.ui.*`), or the
    /// `:tree` delete confirmation hosted in the same slot. The
    /// overlay owns its keys; on resolve/close the chosen value is
    /// sent back to the parked worker through [`Self::active_dialog`]
    /// (plugin dialogs) or completes the staged session deletion,
    /// then the overlay is dismissed.
    pub(crate) fn dispatch_plugin_overlay_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let overlay = self.plugin_overlay.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(overlay.as_mut(), key) {
            OverlayAction::Stay | OverlayAction::PropagateKey => {}
            OverlayAction::Close => {
                self.plugin_overlay = None;
                self.pending_tree_delete = None;
                if let Some(state) = self.active_dialog.take() {
                    let answer = state.cancelled();
                    let _ = state.reply().send(answer);
                }
            }
            OverlayAction::Resolve(value) => {
                self.plugin_overlay = None;
                if let Some(path) = self.pending_tree_delete.take() {
                    if value.as_bool() == Some(true) {
                        let _ = self.send_request(RunRequest::DeleteSession(path));
                        self.notify("session deleted");
                    }
                    return None;
                }
                if let Some(state) = self.active_dialog.take() {
                    let answer = state.resolved(&value);
                    let _ = state.reply().send(answer);
                }
            }
        }
        None
    }

    /// Drive the approval panel. Every answer resolves the waiting
    /// request, so the parked run always resumes.
    pub(crate) fn dispatch_permission_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        self.approval_key_at(key, Instant::now());
        None
    }

    /// Feed `key`, pressed at `now`, to the approval panel.
    pub(crate) fn approval_key_at(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
        now: Instant,
    ) {
        let Some(panel) = self.approval_panel.as_mut() else {
            return;
        };
        match panel.handle_key_at(key, now) {
            ApprovalOutcome::Stay => {}
            ApprovalOutcome::Decide(decision) => self.answer_permission(decision),
            ApprovalOutcome::Feedback(text) => self.answer_with_feedback(text),
            ApprovalOutcome::RejectPlan => {
                self.answer_permission(PermissionDecision::Deny);
                let _ = self.send_request(RunRequest::PlanMode { on: false });
            }
            ApprovalOutcome::Answer(answers) => self.answer_question(answers),
        }
    }
}

/// What a live MCP server offers, as the `/mcp` picker lists it: its
/// tools, then its prompts and resources when it has any.
fn mcp_offer(server: &kage_core::protocol::McpServerInfo) -> String {
    let count = |n: usize, what: &str| {
        let s = if n == 1 { "" } else { "s" };
        format!("{n} {what}{s}")
    };
    let mut parts = vec![count(
        usize::try_from(server.tools).unwrap_or(usize::MAX),
        "tool",
    )];
    if !server.prompts.is_empty() {
        parts.push(count(server.prompts.len(), "prompt"));
    }
    if !server.resources.is_empty() {
        parts.push(count(server.resources.len(), "resource"));
    }
    if !server.templates.is_empty() {
        parts.push(count(server.templates.len(), "template"));
    }
    parts.join(" \u{b7} ")
}

/// How wide a rewind picker row's label may grow before it is cut.
const REWIND_LABEL_WIDTH: usize = 72;

/// One-line label for a rewound prompt, cut to `max`: the first line
/// of its first text block, else a note for an image-only prompt.
fn rewind_label(prompt: &[kage_core::Content], max: usize) -> String {
    let line = prompt
        .iter()
        .find_map(|block| match block {
            kage_core::Content::Text { text } => {
                Some(text.lines().next().unwrap_or_default().to_owned())
            }
            kage_core::Content::Image { mime, .. } => Some(format!("[image {mime}]")),
            _ => None,
        })
        .unwrap_or_else(|| "(no text)".to_owned());
    if line.chars().count() <= max {
        return line;
    }
    let cut: String = line.chars().take(max).collect();
    format!("{cut}\u{2026}")
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn press(app: &mut App, code: KeyCode) {
        app.dispatch_settings_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn settings_app(name: &str) -> (App, mpsc::Receiver<RunRequest>) {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(crate::events::shared_buffer(), tx);
        app.open_settings();
        let target = kage_core::options::OPTIONS
            .iter()
            .position(|def| def.name == name)
            .unwrap();
        for _ in 0..target {
            press(&mut app, KeyCode::Down);
        }
        (app, rx)
    }

    #[test]
    fn cycling_editor_applies_live_and_esc_restores_it() {
        let (mut app, _rx) = settings_app("editor");
        app.input.set_modeless(true);
        press(&mut app, KeyCode::Right);
        assert!(app.apply_option_changes());
        assert!(!app.input.is_modeless());
        press(&mut app, KeyCode::Esc);
        assert!(app.settings_overlay.is_none());
        assert!(app.apply_option_changes());
        assert!(app.input.is_modeless());
    }

    #[test]
    fn enter_persists_the_edited_options() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\n[ui]\nmouse = true\n").unwrap();
        let (mut app, rx) = settings_app("thinking_level");
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Down);
        for c in "0.5".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let overlay = app.settings_overlay.as_mut().unwrap();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let OverlayAction::Resolve(value) =
            crate::overlay::OverlayWidget::handle_key(overlay, enter)
        else {
            panic!("enter resolves");
        };
        app.apply_settings_at(&value, Some(path.clone()));
        let cfg = kage_core::config::Config::load(&path).unwrap();
        assert_eq!(cfg.ui.thinking_level.as_deref(), Some("ultra"));
        assert!((cfg.loop_settings.compaction_threshold - 0.5).abs() < f32::EPSILON);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\n[ui]\nmouse = true\nthinking_level = \"ultra\"\n\n[loop]\ncompaction_threshold = 0.5\n"
        );
        assert_eq!(
            lock(&app.options).get("compaction_threshold"),
            Some(&OptionValue::Float(0.5))
        );
        assert_eq!(
            rx.try_recv(),
            Ok(RunRequest::SetThinkingLevel("ultra".into()))
        );
    }
}
