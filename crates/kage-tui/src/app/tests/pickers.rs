//! Pickers, the settings dialog, the session tree and closing overlays.

use super::*;

#[test]
fn the_help_overlay_never_overlaps_the_input_rows() {
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    app.set_editor_modeless(true);
    let blank = snapshot_rows(&render_app(&mut app));
    app.handle_key(key('?'));
    assert!(app.help_overlay.is_some());
    let rows = snapshot_rows(&render_app(&mut app));
    // Without session usage the footer is the hint row alone, and it
    // names the overlay's keys.
    let (input_top, footer) = (blank.len() - 4, blank.len() - 1);
    assert_eq!(
        rows[input_top..footer],
        blank[input_top..footer],
        "{rows:?}"
    );
    assert_ne!(rows[..input_top], blank[..input_top], "{rows:?}");
}

#[test]
fn pickers_and_settings_never_overlap_the_input_rows() {
    for (width, height) in [(80, 24), (60, 20)] {
        let (tx, _rx) = mpsc::channel();
        let mut app = app_with_defaults(shared_buffer(), tx);
        app.set_editor_modeless(true);
        let check = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            app.render_into(&mut terminal).unwrap();
            let rows = snapshot_rows(&terminal);
            let bottom = rows.iter().rposition(|r| r.contains('\u{256F}'));
            let rule = rows
                .iter()
                .rposition(|r| r.starts_with('\u{2500}'))
                .unwrap();
            let above = rows[..rule].iter().rposition(|r| r.starts_with('\u{2500}'));
            assert!(bottom < above, "{rows:#?}");
        };
        app.dispatch_builtin("settings", "", &crate::command::ParsedArgs::new());
        assert!(app.settings_overlay.is_some());
        check(&mut app);
        app.settings_overlay = None;

        let items = (0..40)
            .map(|i| PickItem::simple(format!("session {i}")))
            .collect();
        app.picker = Some(OverlayPicker::new("sessions", items));
        check(&mut app);
    }
}

#[test]
fn tree_command_without_source_reports_unavailable() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    assert!(
        app.dispatch_builtin("tree", "", &crate::command::ParsedArgs::new())
            .is_none()
    );
    assert!(app.session_tree.is_none());
    let buf = buffer.lock().unwrap();
    assert!(matches!(
        buf.blocks().last().map(Arc::as_ref),
        Some(crate::buffer::Block::Custom { kind, .. }) if kind == "kage:error"
    ));
}

#[test]
fn tree_command_opens_and_enter_dispatches_resume() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_session_tree_source(Box::new(|| {
        vec![
            crate::overlay::SessionNode {
                id: "root".into(),
                path: "/s/root.jsonl".into(),
                parent: None,
                label: "root".into(),
                is_current: true,
            },
            crate::overlay::SessionNode {
                id: "child".into(),
                path: "/s/child.jsonl".into(),
                parent: Some("root".into()),
                label: "child".into(),
                is_current: false,
            },
        ]
    }));
    assert!(
        app.dispatch_builtin("tree", "", &crate::command::ParsedArgs::new())
            .is_none()
    );
    assert!(app.session_tree.is_some());
    // Selection starts on the current session (root); Enter resumes.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.session_tree.is_none());
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::ResumeSession(std::path::PathBuf::from(
            "/s/root.jsonl"
        )))
    );
}

/// Open `:tree` on a one-node fixture and press `d` on it. Returns
/// the app and the request receiver to assert against.
fn tree_delete_fixture() -> (App, mpsc::Receiver<RunRequest>) {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_session_tree_source(Box::new(|| {
        vec![crate::overlay::SessionNode {
            id: "only".into(),
            path: "/s/only.jsonl".into(),
            parent: None,
            label: "only".into(),
            is_current: false,
        }]
    }));
    app.dispatch_builtin("tree", "", &crate::command::ParsedArgs::new());
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    (app, rx)
}

#[test]
fn tree_d_key_asks_before_deleting_and_n_declines() {
    let (mut app, rx) = tree_delete_fixture();
    // The tree closes and a confirmation opens; nothing is deleted yet.
    assert!(app.session_tree.is_none());
    assert!(app.plugin_overlay.is_some());
    assert!(app.pending_tree_delete.is_some());
    assert!(rx_idle(&rx));

    // `n` declines: the dialog closes and still nothing is deleted.
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(app.plugin_overlay.is_none());
    assert!(app.pending_tree_delete.is_none());
    assert!(rx_idle(&rx));
}

#[test]
fn tree_delete_confirm_yes_sends_delete_request() {
    let (mut app, rx) = tree_delete_fixture();
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(app.plugin_overlay.is_none());
    assert!(app.pending_tree_delete.is_none());
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::DeleteSession(std::path::PathBuf::from(
            "/s/only.jsonl"
        )))
    );
}

#[test]
fn tree_delete_confirm_esc_cancels_without_deleting() {
    let (mut app, rx) = tree_delete_fixture();
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.plugin_overlay.is_none());
    assert!(app.pending_tree_delete.is_none());
    assert!(rx_idle(&rx));
}

#[test]
fn settings_command_opens_overlay_and_esc_closes_it() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    // `:settings` opens the modal (reads config read-only; never
    // writes, so this is safe in a test).
    assert!(
        app.dispatch_builtin("settings", "", &crate::command::ParsedArgs::new())
            .is_none()
    );
    assert!(app.settings_overlay.is_some());
    // While open it owns the keyboard; Esc cancels without persist.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.settings_overlay.is_none());
    assert_eq!(app.input().text(), "", "esc went to the overlay, not input");
}

#[test]
fn settings_thinking_level_persists_and_sets_the_live_level() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.apply_settings_at(
        &serde_json::json!({ "thinking_level": "high" }),
        Some(path.clone()),
    );
    let cfg = kage_core::config::Config::load(&path).unwrap();
    assert_eq!(cfg.ui.thinking_level.as_deref(), Some("high"));
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::SetThinkingLevel("high".into()))
    );
}

#[test]
fn settings_thinking_level_ignores_unknown_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.apply_settings_at(
        &serde_json::json!({ "thinking_level": "maximum" }),
        Some(path.clone()),
    );
    let cfg = kage_core::config::Config::load(&path).unwrap();
    assert_eq!(cfg.ui.thinking_level, None);
    assert!(rx_idle(&rx));
}

#[test]
fn model_picker_renders_its_login_note() {
    let mut app = defaults_app();
    app.set_model_choices(vec![PickItem::simple("fake:m")]);
    let _ = app.apply(InputAction::OpenModelPicker);
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    assert!(
        rows.iter().any(|r| r.contains("/login to add a provider")),
        "{rows:#?}"
    );
}

#[test]
fn model_picker_marks_the_current_sessions_model() {
    use kage_core::protocol::{HostEvent, SessionState};

    let (mut app, _rx, events) = app_with_events();
    app.set_status_model(Arc::new(Mutex::new("fake/default".into())));
    let choices = vec![
        PickItem::simple("fake/default")
            .with_label("Default model")
            .with_badge('*'),
        PickItem::simple("fake:session")
            .with_label("Session model")
            .with_badge(' '),
    ];

    for (model, expected) in [
        ("fake/default", Some("Default model")),
        ("fake/session", Some("Session model")),
        ("fake/unlisted", None),
        ("fake:default", Some("Default model")),
    ] {
        let session = kage_core::SessionId::new();
        send_to(
            &mut app,
            &events,
            session,
            vec![
                HostEvent::SessionChanged {
                    path: format!("/tmp/{session}.jsonl").into(),
                    title: None,
                    timeline: Vec::new(),
                    compacted_agents: Vec::new(),
                }
                .into(),
                HostEvent::StateChanged {
                    state: SessionState {
                        model: model.into(),
                        ..SessionState::default()
                    },
                }
                .into(),
            ],
        );
        app.set_model_choices(choices.clone());
        let _ = app.apply(InputAction::OpenModelPicker);
        let rows = rendered(&mut app, 120, 24);
        let starred: Vec<_> = rows.iter().filter(|row| row.contains(" * ")).collect();
        if let Some(expected) = expected {
            assert_eq!(starred.len(), 1, "{model}: {rows:#?}");
            assert!(starred[0].contains(expected), "{model}: {rows:#?}");
        } else {
            assert!(starred.is_empty(), "{model}: {rows:#?}");
        }
        app.picker = None;
    }
}

#[test]
fn model_picker_prefers_reported_usage_over_status_fallback() {
    use kage_core::protocol::{HostEvent, SessionState};

    let (mut app, _rx, events) = app_with_events();
    app.set_model_choices(vec![PickItem::simple("fake/m")]);
    feed(
        &mut app,
        &events,
        vec![
            HostEvent::StateChanged {
                state: SessionState {
                    model: "fake/m".into(),
                    ..SessionState::default()
                },
            }
            .into(),
        ],
    );
    let _ = app.apply(InputAction::OpenModelPicker);
    let rows = rendered(&mut app, 120, 24);
    assert!(
        rows.iter().any(|row| row.contains(" * fake/m")),
        "{rows:#?}"
    );
    for status in ["", "fake/stale"] {
        app.picker = None;
        app.set_status_model(Arc::new(Mutex::new(status.into())));
        let _ = app.apply(InputAction::OpenModelPicker);
        let rows = rendered(&mut app, 120, 24);
        assert!(
            rows.iter().any(|row| row.contains(" * fake/m")),
            "{status}: {rows:#?}"
        );
    }
}

#[test]
fn model_picker_keeps_host_badges_without_live_model_state() {
    let mut app = defaults_app();
    app.set_model_choices(vec![PickItem::simple("fake/m").with_badge('*')]);
    let _ = app.apply(InputAction::OpenModelPicker);
    let rows = rendered(&mut app, 120, 24);
    assert!(
        rows.iter().any(|row| row.contains(" * fake/m")),
        "{rows:#?}"
    );
}

#[test]
fn session_picker_defaults_to_cwd_and_ctrl_a_toggles_all() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    // cwd scope -> only "here"; all scope -> "here" + "elsewhere".
    app.set_session_lister(Box::new(|all| {
        if all {
            vec![PickItem::simple("here"), PickItem::simple("elsewhere")]
        } else {
            vec![PickItem::simple("here")]
        }
    }));

    app.session_scope_all = false;
    app.open_session_picker(false);
    assert_eq!(app.picker_kind, Some(PickerKind::Session));
    // cwd scope: Enter resolves the only cwd session.
    app.dispatch_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::ResumeSession(p) => assert_eq!(p, std::path::PathBuf::from("here")),
        other => panic!("expected ResumeSession(here), got {other:?}"),
    }

    // Reopen, toggle to all dirs with Ctrl+A, pick the 2nd row.
    app.session_scope_all = false;
    app.open_session_picker(false);
    assert!(app.dispatch_picker_key(ctrl('a')).is_none());
    assert!(app.session_scope_all, "Ctrl+A switched to all dirs");
    // Ungrouped rows sort alphabetically, so row 0 is now
    // "elsewhere" - which only exists in the all-dirs dataset,
    // proving the toggle re-listed with the wider scope.
    app.dispatch_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::ResumeSession(p) => {
            assert_eq!(p, std::path::PathBuf::from("elsewhere"));
        }
        other => panic!("expected ResumeSession(elsewhere), got {other:?}"),
    }
    // Toggle back to cwd-only.
    app.session_scope_all = false;
    app.open_session_picker(false);
    assert!(app.dispatch_picker_key(ctrl('a')).is_none());
    assert!(app.session_scope_all);
    assert!(app.dispatch_picker_key(ctrl('a')).is_none());
    assert!(!app.session_scope_all, "Ctrl+A toggles back");
}

/// A rewindable prompt with entry id `at` and one text block, on the
/// original thread.
fn past_prompt(at: &str, text: &str) -> kage_core::protocol::PastPrompt {
    branch_prompt(at, text, 0, false)
}

/// A rewindable prompt on a branch: `depth` prompts above it, and
/// whether it is the active head.
fn branch_prompt(
    at: &str,
    text: &str,
    depth: u32,
    active: bool,
) -> kage_core::protocol::PastPrompt {
    kage_core::protocol::PastPrompt {
        at: at.to_owned(),
        prompt: vec![kage_core::Content::Text {
            text: text.to_owned(),
        }],
        depth,
        active,
    }
}

#[test]
fn the_rewind_picker_lists_oldest_first_and_starts_on_the_newest() {
    let (mut app, rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![
            kage_core::protocol::HostEvent::PromptList {
                prompts: vec![
                    past_prompt("11111111111111111111111111", "round 0"),
                    branch_prompt(
                        "22222222222222222222222222",
                        "round 1\nmore detail",
                        0,
                        true,
                    ),
                ],
            }
            .into(),
        ],
    );
    assert_eq!(app.picker_kind, Some(PickerKind::Rewind));
    // Oldest row first, and a multi-line prompt renders as its first
    // line only.
    let rows = rendered(&mut app, 60, 30);
    let first = rows.iter().position(|r| r.contains("round 0"));
    let second = rows.iter().position(|r| r.contains("round 1"));
    assert!(
        first.is_some_and(|f| second.is_some_and(|s| f < s)),
        "oldest first: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("round 1") && !r.contains("more detail")),
        "{rows:?}"
    );
    // The selection starts on the active head, so enter rewinds there.
    app.dispatch_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.picker_kind, None);
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::Rewind { at } => {
            assert_eq!(at, "22222222222222222222222222");
        }
        other => panic!("expected Rewind, got {other:?}"),
    }
}

#[test]
fn the_rewind_picker_indents_branches_and_badges_the_active_head() {
    let (mut app, rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![
            kage_core::protocol::HostEvent::PromptList {
                prompts: vec![
                    branch_prompt("11111111111111111111111111", "round 0", 0, true),
                    branch_prompt("22222222222222222222222222", "round 1", 1, false),
                    branch_prompt("33333333333333333333333333", "redo", 1, false),
                ],
            }
            .into(),
        ],
    );
    assert_eq!(app.picker_kind, Some(PickerKind::Rewind));
    let rows = rendered(&mut app, 80, 30);
    // Depth-first order: a branch's descendants before its next
    // sibling.
    let round0 = rows.iter().position(|r| r.contains("round 0"));
    let round1 = rows.iter().position(|r| r.contains("round 1"));
    let redo = rows.iter().position(|r| r.contains("redo"));
    assert!(
        round0.is_some_and(|a| round1.is_some_and(|b| redo.is_some_and(|c| a < b && b < c))),
        "depth-first order: {rows:?}"
    );
    // Branch rows sit one indent under their fork point.
    assert!(
        rows.iter().any(|r| r.contains("  round 1")),
        "branch rows indent two spaces per level: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains("  round 0")),
        "top-level rows do not indent: {rows:?}"
    );
    // The active head carries the badge and starts selected.
    assert!(
        rows.iter()
            .any(|r| r.contains('*') && r.contains("round 0")),
        "the active head is badged: {rows:?}"
    );
    app.dispatch_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::Rewind { at } => assert_eq!(at, "11111111111111111111111111"),
        other => panic!("expected Rewind, got {other:?}"),
    }
}

#[test]
fn a_long_rewind_list_opens_scrolled_to_the_newest() {
    let (mut app, _rx, events) = app_with_events();
    let prompts: Vec<_> = (0..30)
        .map(|i| {
            branch_prompt(
                &format!("{i:026}"),
                &format!("turn {i} of the long session"),
                0,
                i == 29,
            )
        })
        .collect();
    feed(
        &mut app,
        &events,
        vec![kage_core::protocol::HostEvent::PromptList { prompts }.into()],
    );
    assert_eq!(app.picker_kind, Some(PickerKind::Rewind));
    let rows = rendered(&mut app, 60, 30);
    assert!(
        rows.iter().any(|r| r.contains("turn 29")),
        "the newest row is on screen: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("more above")),
        "older rows sit above the window: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains("turn 0 of")),
        "the oldest row is scrolled off: {rows:?}"
    );
}

#[test]
fn an_empty_prompt_list_never_opens_the_picker() {
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![
            kage_core::protocol::HostEvent::PromptList {
                prompts: Vec::new(),
            }
            .into(),
        ],
    );
    assert_eq!(app.picker_kind, None);
}

#[test]
fn a_directory_without_sessions_opens_its_own_empty_scope() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_session_lister(Box::new(|all| {
        if all {
            vec![PickItem::simple("elsewhere")]
        } else {
            Vec::new()
        }
    }));
    let _ = app.apply(InputAction::OpenSessionPicker);
    assert_eq!(app.picker_kind, Some(PickerKind::Session));
    assert!(
        !app.session_scope_all,
        "other projects stay out of this list"
    );
    assert!(app.dispatch_picker_key(ctrl('a')).is_none());
    assert!(app.session_scope_all, "Ctrl+A still reaches them");

    app.picker = None;
    app.picker_kind = None;
    app.set_session_lister(Box::new(|_| Vec::new()));
    let _ = app.apply(InputAction::OpenSessionPicker);
    assert_eq!(app.picker_kind, None, "nothing anywhere opens nothing");
}

#[test]
fn paste_routes_to_the_active_overlay() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);

    // Cmdline open: the paste lands in the command line.
    app.cmdline = Some(CommandLine::new());
    app.handle_paste("theme ");
    assert_eq!(app.cmdline.as_ref().unwrap().text(), "theme ");
    app.cmdline = None;

    // A modal without a text field swallows the paste instead of
    // letting it fall through to the hidden main input.
    app.picker = Some(OverlayPicker::new("pick", Vec::new()));
    app.handle_paste("leak");
    assert_eq!(app.input().text(), "");
    app.picker = None;

    // Search line open: the paste lands there.
    let (tx2, _rx2) = mpsc::channel();
    let mut app2 = app_with_defaults(shared_buffer(), tx2);
    app2.search_line = Some(CommandLine::new());
    app2.handle_paste("pat");
    assert_eq!(app2.search_line.as_ref().unwrap().text(), "pat");

    // Nothing open: the main input receives it verbatim.
    app.handle_paste("plain");
    assert_eq!(app.input().text(), "plain");
}

#[test]
fn f3_opens_the_jump_picker_and_resolve_focuses_the_block() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    {
        let mut buf = app.buffer.lock().unwrap();
        buf.push_user("find me");
        buf.begin_assistant();
        buf.append_assistant_delta("answer");
    }

    app.dispatch_key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE));
    assert_eq!(app.picker_kind, Some(crate::app::PickerKind::Jump));

    // Rows are newest-first: row 0 is the latest block (the
    // assistant reply at buffer index 1).
    app.dispatch_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.picker.is_none(), "picker closed on resolve");
    assert_eq!(app.buffer.lock().unwrap().effective_focus(), Some(1));
}

#[test]
fn jump_picker_lists_the_newest_target_first() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    {
        let mut buf = app.buffer.lock().unwrap();
        buf.push_user("aaa oldest");
        buf.begin_assistant();
        buf.append_assistant_delta("middle");
        buf.push_user("zzz newest");
    }
    app.open_jump_picker();
    app.dispatch_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.buffer.lock().unwrap().effective_focus(), Some(2));
}

#[test]
fn model_picker_with_no_models_explains_how_to_connect() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_toasts(crate::toast::shared_toasts());
    let _ = app.apply(InputAction::OpenModelPicker);
    assert!(app.picker.is_none());
    assert!(
        app.live_toasts()
            .iter()
            .any(|t| t.text.contains("Run /login")),
        "empty model list must explain itself"
    );
}

/// Opens one overlay on an App.
type OpenOverlay = fn(&mut App);

#[test]
fn idle_ctrl_c_closes_each_overlay_and_the_footer_names_its_keys() {
    let (mut app, rx, _events) = app_with_events();
    app.set_editor_modeless(true);
    app.set_model_choices(vec![PickItem::simple("fake:m").with_label("Fake")]);
    app.set_session_lister(Box::new(|_| vec![PickItem::simple("/tmp/s.jsonl")]));
    app.set_session_tree_source(Box::new(|| {
        vec![crate::overlay::SessionNode {
            id: "s".into(),
            path: "/tmp/s.jsonl".into(),
            label: "work".into(),
            ..Default::default()
        }]
    }));
    let open: [(OpenOverlay, &str); 5] = [
        (
            |app| {
                app.handle_key(ctrl('p'));
            },
            "enter to pick \u{b7} esc to close",
        ),
        (
            |app| {
                app.handle_key(ctrl('s'));
            },
            "enter to pick \u{b7} esc to close",
        ),
        (App::open_settings, "enter to save \u{b7} esc to cancel"),
        (
            App::open_session_tree,
            "enter to resume \u{b7} f to fork \u{b7} esc to close",
        ),
        (App::open_help, "up/down to scroll \u{b7} esc to close"),
    ];
    for (open, hint) in open {
        open(&mut app);
        assert!(app.keyboard_modal_open(), "{hint}");
        assert_eq!(app.footer_hint(), hint);
        app.handle_key(ctrl('c'));
        assert!(!app.keyboard_modal_open(), "ctrl+c closes: {hint}");
    }
    assert!(app.escalation.is_none(), "closing never arms quit");
    assert!(rx_idle(&rx), "idle, nothing is cancelled");
}

#[test]
fn the_session_picker_and_tree_mark_the_session_on_screen() {
    let (mut app, _rx, events) = app_with_events();
    let current = kage_core::SessionId::new();
    let other = kage_core::SessionId::new();
    send_to(
        &mut app,
        &events,
        current,
        vec![
            kage_core::protocol::HostEvent::SessionChanged {
                path: format!("/tmp/{current}.jsonl").into(),
                title: None,
                timeline: Vec::new(),
                compacted_agents: Vec::new(),
            }
            .into(),
        ],
    );
    let paths = [current, other].map(|id| format!("/tmp/{id}.jsonl"));
    let listed = paths.clone();
    app.set_session_lister(Box::new(move |_| {
        listed.iter().map(|p| PickItem::simple(p.clone())).collect()
    }));
    app.set_session_tree_source(Box::new(move || {
        [current, other]
            .map(|id| crate::overlay::SessionNode {
                id: id.to_string(),
                path: format!("/tmp/{id}.jsonl"),
                label: id.to_string(),
                ..Default::default()
            })
            .to_vec()
    }));
    app.open_session_picker(false);
    let rows = rendered(&mut app, 120, 24);
    let marked: Vec<&String> = rows.iter().filter(|r| r.contains(" * ")).collect();
    assert_eq!(marked.len(), 1, "{rows:#?}");
    assert!(marked[0].contains(&paths[0]), "{rows:#?}");
    app.picker = None;
    app.open_session_tree();
    let rows = rendered(&mut app, 120, 24);
    let marked: Vec<&String> = rows.iter().filter(|r| r.contains("* ")).collect();
    assert_eq!(marked.len(), 1, "{rows:#?}");
    assert!(marked[0].contains(&current.to_string()), "{rows:#?}");
}
