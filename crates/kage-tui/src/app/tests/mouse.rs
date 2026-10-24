//! Mouse selection, drags, clicks and the context menu.

use super::*;

#[test]
fn partial_selection_copies_only_highlighted_cells_not_whole_block() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    {
        let mut buf = buffer.lock().unwrap();
        buf.begin_thinking();
        buf.append_thinking_delta("alpha beta gamma delta epsilon");
    }
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();

    // Render once with a full-buffer selection so capture_and_overlay
    // populates captured_rows. The selection coords cover the entire
    // visible area.
    app.screen_selection = Some(((0, 0), (999, 59)));
    app.render_into(&mut terminal).unwrap();

    // Locate "gamma" in the captured grid by column (cell) index,
    // not byte offset, so a multi-byte chrome glyph on the row
    // cannot skew the mapping.
    let needle: Vec<char> = "gamma".chars().collect();
    let mut sel = None;
    for (&vrow, cells) in &app.captured_rows {
        let chars: Vec<char> = cells.iter().map(|c| c.ch).collect();
        if let Some(at) = chars
            .windows(needle.len())
            .position(|w| w == needle.as_slice())
        {
            let lo = u16::try_from(at).unwrap();
            let hi = u16::try_from(at + needle.len() - 1).unwrap();
            sel = Some(((vrow, lo), (vrow, hi)));
            break;
        }
    }
    let (anchor, cursor) = sel.expect("thinking text was never painted");
    app.screen_selection = Some((anchor, cursor));
    app.render_into(&mut terminal).unwrap();

    assert_eq!(app.extract_selection_text(), "gamma");
}

#[test]
fn right_click_opens_context_menu_on_the_block_then_esc_and_no_block_close_it() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    {
        let mut buf = buffer.lock().unwrap();
        buf.push_user("hello there");
    }
    let backend = TestBackend::new(40, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    app.render_into(&mut terminal).unwrap();

    // A screen row and the buffer-pane geometry that block 0
    // painted into.
    let (row, area_x, below) = {
        let buf = buffer.lock().unwrap();
        let y0 = buf.last_area_y();
        let y1 = y0.saturating_add(buf.last_area_height());
        let row = (y0..y1)
            .find(|&y| buf.block_at_screen_row(y) == Some(0))
            .expect("block 0 painted");
        (row, buf.last_area_x(), y1)
    };

    // Right press over the block opens a menu targeting it.
    app.open_context_menu(area_x + 1, row);
    assert_eq!(
        app.context_menu.as_ref().map(ContextMenu::block_idx),
        Some(0)
    );

    // Esc closes it.
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.dispatch_context_menu_key(esc), None);
    assert!(app.context_menu.is_none());

    // Reopened, a right press below the buffer pane (over no
    // block) dismisses instead of opening.
    app.open_context_menu(area_x + 1, row);
    assert!(app.context_menu.is_some());
    app.open_context_menu(area_x + 1, below);
    assert!(app.context_menu.is_none());
}

fn mouse_event(
    kind: ratatui::crossterm::event::MouseEventKind,
) -> ratatui::crossterm::event::MouseEvent {
    ratatui::crossterm::event::MouseEvent {
        kind,
        column: 5,
        row: 5,
        modifiers: KeyModifiers::NONE,
    }
}

/// An App over forty one-line replies, painted into a 40x12 terminal
/// and scrolled to the top, with a selection anchored on the first
/// buffer row.
fn drag_fixture() -> (App, SharedBuffer, Terminal<TestBackend>) {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    {
        let mut buf = buffer.lock().unwrap();
        for i in 0..40 {
            buf.append_assistant_delta(&format!("reply {i}"));
            buf.finish_streaming();
        }
        buf.set_scroll(0);
    }
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let area_y = buffer.lock().unwrap().last_area_y();
    app.mouse_down(area_y, 2);
    app.render_into(&mut terminal).unwrap();
    (app, buffer, terminal)
}

#[test]
fn a_drag_below_the_buffer_scrolls_one_line_and_extends_the_selection() {
    let (mut app, buffer, mut terminal) = drag_fixture();
    let (area_y, height) = {
        let buf = buffer.lock().unwrap();
        (buf.last_area_y(), buf.last_area_height())
    };
    let below = area_y + height + 1;
    app.mouse_drag(below, 5);
    assert_eq!(buffer.lock().unwrap().scroll(), Some(1));
    let (_, cursor) = app.screen_selection.unwrap();
    assert_eq!(cursor, (usize::from(height), 5));

    app.render_into(&mut terminal).unwrap();
    app.mouse_drag(below, 5);
    assert_eq!(buffer.lock().unwrap().scroll(), Some(2));
    let (anchor, cursor) = app.screen_selection.unwrap();
    assert_eq!(anchor, (0, 2));
    assert_eq!(cursor, (usize::from(height) + 1, 5));
}

#[test]
fn a_drag_above_the_top_clamps_and_a_drag_inside_does_not_scroll() {
    let (mut app, buffer, _terminal) = drag_fixture();
    let area_y = buffer.lock().unwrap().last_area_y();
    app.mouse_drag(area_y.saturating_sub(1), 3);
    assert_eq!(buffer.lock().unwrap().scroll(), Some(0));
    assert_eq!(app.screen_selection.unwrap().1, (0, 3));
    app.mouse_drag(area_y + 2, 3);
    assert_eq!(buffer.lock().unwrap().scroll(), Some(0));
    assert_eq!(app.screen_selection.unwrap().1, (2, 3));
}

#[test]
fn clicking_a_folded_header_unfolds_it_in_place() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    {
        let mut buf = lock(&buffer);
        for i in 0..20 {
            buf.append_assistant_delta(&format!("reply {i}"));
            buf.finish_streaming();
        }
        buf.push_tool_call("c1", "shell", serde_json::json!({"command": "seq 30"}));
        let out: Vec<String> = (1..=30).map(|i| format!("out {i}")).collect();
        buf.push_tool_result("c1", format!("stdout:\n{}\nexit: 0", out.join("\n")), false);
        buf.append_assistant_delta("after");
        buf.finish_streaming();
    }
    let mut terminal = Terminal::new(TestBackend::new(50, 20)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let header = |terminal: &Terminal<TestBackend>| {
        snapshot_rows(terminal)
            .iter()
            .position(|r| r.contains("Ran seq 30"))
    };
    let row = header(&terminal).expect("header on screen");
    let row = u16::try_from(row).unwrap();
    app.mouse_down(row, 5);
    app.mouse_up(row);
    app.render_into(&mut terminal).unwrap();
    assert!(
        snapshot_rows(&terminal)
            .iter()
            .any(|r| r.ends_with("out 1")),
        "the call unfolded"
    );
    assert_eq!(header(&terminal), Some(usize::from(row)));
}

#[test]
fn a_drag_release_toasts_the_copied_characters() {
    let (mut app, buffer, mut terminal) = drag_fixture();
    app.set_toasts(crate::toast::shared_toasts());
    let area_y = buffer.lock().unwrap().last_area_y();
    app.mouse_drag(area_y, 30);
    app.render_into(&mut terminal).unwrap();
    app.mouse_up(area_y);
    assert!(app.screen_selection.is_none());
    let toasts = app.live_toasts();
    assert!(
        toasts
            .iter()
            .any(|t| t.text.starts_with("copied ") && t.text.ends_with(" characters")),
        "{toasts:?}"
    );
}

#[test]
fn modal_open_reflects_every_modal_field() {
    // One toggled field per arm of `App::modal_open`; extend both
    // together when a modal field joins the App.
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    assert!(!app.modal_open());

    app.cmdline = Some(CommandLine::new());
    assert!(app.modal_open());
    app.cmdline = None;

    app.search_line = Some(CommandLine::new());
    assert!(app.modal_open());
    app.search_line = None;

    app.picker = Some(OverlayPicker::new("pick", Vec::new()));
    assert!(app.modal_open());
    app.picker = None;

    app.session_tree = Some(SessionTreeOverlay::new(Vec::new()));
    assert!(app.modal_open());
    app.session_tree = None;

    app.settings_overlay = Some(SettingsOverlay::new(
        &kage_core::options::OptionStore::default(),
        Vec::new(),
    ));
    assert!(app.modal_open());
    app.settings_overlay = None;

    app.plugin_overlay = Some(Box::new(crate::overlay::widget::EmptyOverlayWidget));
    assert!(app.modal_open());
    app.plugin_overlay = None;

    app.open_help();
    assert!(app.modal_open());
    app.help_overlay = None;

    app.agents_overlay = Some(crate::overlay::AgentsOverlay::new(Vec::new(), None));
    assert!(app.modal_open());
    app.agents_overlay = None;

    app.todo_list_overlay = Some(crate::overlay::TodoListOverlay::new(
        &crate::view::todo::TodoStrip::default(),
    ));
    assert!(app.modal_open());
    app.todo_list_overlay = None;

    app.approval_panel = Some(crate::overlay::ApprovalPanel::new(
        "shell",
        &serde_json::json!({ "command": "ls" }),
        None,
        Instant::now(),
    ));
    assert!(app.modal_open());
    app.approval_panel = None;

    assert!(!app.modal_open());
}

#[test]
fn mouse_events_are_swallowed_while_modal_is_open() {
    use ratatui::crossterm::event::MouseButton;
    use ratatui::crossterm::event::MouseEventKind;

    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    app.set_scroll(4);
    assert_eq!(buffer.lock().unwrap().scroll(), Some(4));

    // Picker open: the wheel must not move the hidden buffer.
    app.picker = Some(OverlayPicker::new("pick", Vec::new()));
    app.handle_mouse_event(mouse_event(MouseEventKind::ScrollDown));
    assert_eq!(buffer.lock().unwrap().scroll(), Some(4));

    // Same while the `:` cmdline is open, and a right-click must not
    // stack a context menu on top of the modal.
    app.picker = None;
    app.cmdline = Some(CommandLine::new());
    app.handle_mouse_event(mouse_event(MouseEventKind::ScrollDown));
    assert_eq!(buffer.lock().unwrap().scroll(), Some(4));
    app.handle_mouse_event(mouse_event(MouseEventKind::Down(MouseButton::Right)));
    assert!(app.context_menu.is_none());

    // No modal: the same wheel event scrolls the buffer again.
    app.cmdline = None;
    app.handle_mouse_event(mouse_event(MouseEventKind::ScrollDown));
    let expected = 4 + MOUSE_SCROLL_LINES as usize;
    assert_eq!(buffer.lock().unwrap().scroll(), Some(expected));
}

#[test]
fn a_click_in_the_buffer_keeps_modeless_focus_on_the_input() {
    for (modeless, pane) in [(true, Pane::Input), (false, Pane::Buffer)] {
        let (mut app, buffer, mut terminal) = drag_fixture();
        app.set_editor_modeless(modeless);
        app.input.set_focused_pane(Pane::Input);
        let area_y = buffer.lock().unwrap().last_area_y();
        app.mouse_down(area_y, 2);
        app.render_into(&mut terminal).unwrap();
        assert_eq!(app.input.focused_pane(), pane, "modeless = {modeless}");
    }
}

#[test]
fn clicking_the_new_output_mark_snaps_back_to_the_bottom() {
    use ratatui::crossterm::event::MouseButton;
    use ratatui::crossterm::event::MouseEventKind;

    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    {
        let mut buf = lock(&buffer);
        for i in 0..40 {
            buf.append_assistant_delta(&format!("reply {i}"));
            buf.finish_streaming();
        }
    }
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    app.render_into(&mut terminal).unwrap();
    buffer.lock().unwrap().set_scroll(0);
    app.render_into(&mut terminal).unwrap();

    // First screen cell of the `new output below` mark, if one is
    // painted on the buffer's last row.
    let mark = |buffer: &SharedBuffer| {
        let buf = buffer.lock().unwrap();
        let y = buf.last_area_y() + buf.last_area_height() - 1;
        (0..buf.last_area_width())
            .find(|&c| buf.mark_hit_at(y, c))
            .map(|c| (y, c))
    };
    let click = |row: u16, col: u16| ratatui::crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    };

    // Detaching alone paints no mark; output below the viewport does.
    assert!(mark(&buffer).is_none(), "no mark before new output");
    {
        let mut buf = lock(&buffer);
        buf.append_assistant_delta("streamed");
        buf.finish_streaming();
    }
    app.render_into(&mut terminal).unwrap();
    let (row, col) = mark(&buffer).expect("mark painted while scrolled up");

    // A click beside the mark stays pinned and starts a selection.
    app.handle_mouse_event(click(row, col - 1));
    app.handle_mouse_event(ratatui::crossterm::event::MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: col - 1,
        row,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(buffer.lock().unwrap().scroll(), Some(0));

    // A click on the mark jumps to the bottom and re-arms follow.
    app.handle_mouse_event(click(row, col));
    assert!(buffer.lock().unwrap().is_following());
    app.render_into(&mut terminal).unwrap();
    assert!(mark(&buffer).is_none(), "following again, mark gone");
}
