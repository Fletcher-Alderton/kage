//! Commands: validation, the `:` line and `/`-prompt dispatch, and the
//! built-in commands.

use super::*;

#[test]
fn goal_command_sets_and_clears_the_goal() {
    let (mut app, rx, _events) = app_with_events();
    app.dispatch_builtin("goal", "", &crate::command::ParsedArgs::new());
    assert!(rx_idle(&rx), "no argument is a usage error");
    app.dispatch_builtin("goal", "tests pass", &crate::command::ParsedArgs::new());
    assert!(matches!(
        rx.try_recv(),
        Ok(RunRequest::SetGoal { goal: Some(goal) }) if goal == "tests pass"
    ));
    app.dispatch_builtin("goal", "clear", &crate::command::ParsedArgs::new());
    assert!(matches!(
        rx.try_recv(),
        Ok(RunRequest::SetGoal { goal: None })
    ));
}

#[test]
fn swarm_command_flips_the_mode_and_runs_one_shot_tasks() {
    let (mut app, rx, events) = app_with_events();

    app.dispatch_builtin("swarm", "", &crate::command::ParsedArgs::new());
    assert!(rx_idle(&rx), "no argument is a usage error");

    app.dispatch_builtin("swarm", "on", &crate::command::ParsedArgs::new());
    assert!(matches!(
        rx.try_recv(),
        Ok(RunRequest::SwarmMode { on: true })
    ));
    app.dispatch_builtin("swarm", "off", &crate::command::ParsedArgs::new());
    assert!(matches!(
        rx.try_recv(),
        Ok(RunRequest::SwarmMode { on: false })
    ));

    app.dispatch_builtin(
        "swarm",
        "fix all the crates",
        &crate::command::ParsedArgs::new(),
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(RunRequest::SwarmMode { on: true })
    ));
    match rx.try_recv() {
        Ok(RunRequest::Submit {
            text,
            session: None,
            queue: false,
            ..
        }) => assert_eq!(text, "fix all the crates"),
        other => panic!("expected the task prompt, got {other:?}"),
    }
    assert!(app.swarm_oneshot.is_some());

    feed(
        &mut app,
        &events,
        vec![run_ended(kage_core::protocol::RunOutcome::Completed)],
    );
    assert!(
        matches!(rx.try_recv(), Ok(RunRequest::SwarmMode { on: false })),
        "the one-shot swarm turns itself off"
    );
    assert!(app.swarm_oneshot.is_none());
}

#[test]
fn a_session_switch_clears_a_pending_oneshot_swarm() {
    let (mut app, rx, events) = app_with_events();
    app.dispatch_builtin("swarm", "fix it", &crate::command::ParsedArgs::new());
    let _ = rx.try_recv();
    let _ = rx.try_recv();

    feed(
        &mut app,
        &events,
        vec![kage_core::protocol::Event::Host(
            kage_core::protocol::HostEvent::SessionChanged {
                path: std::path::PathBuf::from("x.jsonl"),
                title: None,
                timeline: Vec::new(),
                compacted_agents: Vec::new(),
            },
        )],
    );
    assert!(app.swarm_oneshot.is_none());
    feed(
        &mut app,
        &events,
        vec![run_ended(kage_core::protocol::RunOutcome::Completed)],
    );
    assert!(rx_idle(&rx), "the cleared one-shot sends no exit note");
}

#[test]
fn events_command_lists_known_hooks_by_kind() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    app.push_events();
    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last().map(Arc::as_ref) {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
    };
    assert!(rendered.contains("tool_call"), "{rendered}");
    assert!(rendered.contains("transform_context"), "{rendered}");
    assert!(rendered.contains("should_stop_after_turn"), "{rendered}");
    assert!(rendered.contains("predicate:"), "{rendered}");
}

#[test]
fn usage_command_renders_totals_cache_and_context_bar() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    let usage = crate::usage::shared_session_usage();
    app.set_session_usage(usage.clone());
    {
        let mut u = lock(&usage);
        u.model = "p:m".to_owned();
        u.input_tokens = 650_200_000;
        u.output_tokens = 1_800_000;
        u.cache_read_tokens = 12_400_000;
        u.cache_write_tokens = 3_100_000;
        u.current_context = 190_000;
        u.context_window = 250_000;
        u.total_cost = 1.23;
    }
    app.push_usage();
    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last().map(Arc::as_ref) {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
    };
    assert!(rendered.contains("Session usage"), "{rendered}");
    assert!(rendered.contains("p:m"), "{rendered}");
    assert!(rendered.contains("input 650.2M"), "{rendered}");
    assert!(rendered.contains("output 1.8M"), "{rendered}");
    assert!(rendered.contains("cache read 12.4M"), "{rendered}");
    assert!(rendered.contains("cache write 3.1M"), "{rendered}");
    assert!(rendered.contains("total 652M"), "{rendered}");
    assert!(rendered.contains("($1.23)"), "{rendered}");
    assert!(rendered.contains("Context window"), "{rendered}");
    assert!(rendered.contains("76%"), "{rendered}");
    assert!(rendered.contains("(190k / 250k)"), "{rendered}");
    let bar = "\u{2588}".repeat(15) + &"\u{2591}".repeat(5);
    assert!(rendered.contains(&bar), "{rendered}");
}

#[test]
fn usage_command_without_window_or_cost() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    app.push_usage();
    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last().map(Arc::as_ref) {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
    };
    assert!(rendered.contains("input 0"), "{rendered}");
    assert!(rendered.contains("(unknown window)"), "{rendered}");
    assert!(!rendered.contains('$'), "{rendered}");
}

#[test]
fn ctrl_c_interrupts_over_an_open_cmdline() {
    let (mut app, rx, _events) = app_with_events();
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(key(':'));
    assert!(app.cmdline.is_some(), "cmdline should be open");
    lock(app.session_usage.as_ref().unwrap()).working = true;
    app.handle_key(ctrl('c'));
    assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel { session: None }));
    assert!(
        app.cmdline.is_some(),
        "interrupt must not close the cmdline"
    );
    lock(app.session_usage.as_ref().unwrap()).working = false;
    app.handle_key(ctrl('c'));
    assert!(app.cmdline.is_none(), "idle, ctrl+c closes the cmdline");
    assert!(rx_idle(&rx));
}

#[test]
fn cancel_command_sends_a_cancel_request() {
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();
    let result = app.run_command_validated("cancel", &registry);
    assert!(
        matches!(result, CommandResult::Done(None)),
        "expected Done(None), got {result:?}"
    );
    assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel { session: None }));
}

#[test]
fn permission_command_dispatches_mode_override() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();

    let result = app.run_command_validated("permission ask", &registry);
    assert!(matches!(result, CommandResult::Done(None)));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::SetPermissionMode(mode) => {
            assert_eq!(mode, Some(kage_core::permissions::PermissionAction::Ask));
        }
        other => panic!("expected SetPermissionMode, got {other:?}"),
    }

    let _ = app.run_command_validated("permission deny", &registry);
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::SetPermissionMode(mode) => {
            assert_eq!(mode, Some(kage_core::permissions::PermissionAction::Deny));
        }
        other => panic!("expected SetPermissionMode, got {other:?}"),
    }

    let _ = app.run_command_validated("permission default", &registry);
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::SetPermissionMode(mode) => assert_eq!(mode, None),
        other => panic!("expected SetPermissionMode, got {other:?}"),
    }
}

#[test]
fn permission_command_rejects_unknown_mode_without_request() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();

    let result = app.run_command_validated("permission bogus", &registry);
    assert!(
        matches!(result, CommandResult::ValidationError(_)),
        "arg validation should reject an unknown mode, got {result:?}"
    );
    assert!(
        rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "no request should be sent for an invalid mode"
    );
}

#[test]
fn permission_command_without_arg_reports_current_mode() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();

    let result = app.run_command_validated("permission", &registry);
    assert!(matches!(result, CommandResult::Done(None)));
    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last().map(Arc::as_ref) {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
    };
    assert!(rendered.contains("permission mode: default"), "{rendered}");
}

fn builtin_registry() -> Vec<&'static CommandSpec> {
    BUILTIN_COMMANDS.iter().collect()
}

#[test]
fn validated_unknown_command_returns_error_with_suggestion() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    let result = app.run_command_validated("quut", &registry);
    match result {
        CommandResult::ValidationError(msg) => {
            assert!(msg.contains("unknown command: quut"), "got {msg:?}");
            assert!(
                msg.contains("did you mean /quit?"),
                "should suggest closest match with the / sigil, got {msg:?}"
            );
        }
        other @ CommandResult::Done(_) => {
            panic!("expected ValidationError, got {other:?}");
        }
    }
}

#[test]
fn the_colon_line_suggests_with_its_own_prefix() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.handle_key(code(KeyCode::Esc));
    app.handle_key(key(':'));
    type_str(&mut app, "quut");
    app.handle_key(code(KeyCode::Enter));
    let error = app.cmdline.as_ref().and_then(|c| c.error()).unwrap_or("");
    assert_eq!(error, "unknown command: quut (did you mean :quit?)");
}

#[test]
fn validated_invalid_choice_returns_error() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    // "mouse mayb" is invalid: "mayb" is not in [on, off, toggle]
    let result = app.run_command_validated("mouse mayb", &registry);
    match result {
        CommandResult::ValidationError(msg) => {
            assert!(
                msg.contains("state"),
                "error should mention the arg name, got {msg:?}"
            );
        }
        other @ CommandResult::Done(_) => {
            panic!("expected ValidationError, got {other:?}");
        }
    }
}

#[test]
fn validated_missing_required_arg_returns_error() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    let result = app.run_command_validated("fold", &registry);
    match result {
        CommandResult::ValidationError(msg) => {
            assert!(
                msg.contains("missing"),
                "error should mention missing arg, got {msg:?}"
            );
        }
        other @ CommandResult::Done(_) => {
            panic!("expected ValidationError, got {other:?}");
        }
    }
}

#[test]
fn validated_valid_command_returns_done() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    let result = app.run_command_validated("help", &registry);
    assert!(
        matches!(result, CommandResult::Done(_)),
        "expected Done, got {result:?}"
    );
}

#[test]
fn validated_quit_returns_done_with_exit() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    let result = app.run_command_validated("quit", &registry);
    assert!(
        matches!(result, CommandResult::Done(Some(AppExit::Quit))),
        "expected Done(Some(Quit)), got {result:?}"
    );
}

#[test]
fn validated_subcommand_validates_against_leaf_spec() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    // "theme set" without required <name> arg should error
    let result = app.run_command_validated("theme set", &registry);
    match result {
        CommandResult::ValidationError(msg) => {
            assert!(
                msg.contains("missing"),
                "error should mention missing arg, got {msg:?}"
            );
        }
        other @ CommandResult::Done(_) => {
            panic!("expected ValidationError, got {other:?}");
        }
    }
}

#[test]
fn validated_empty_input_returns_done_none() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    let result = app.run_command_validated("", &registry);
    assert!(
        matches!(result, CommandResult::Done(None)),
        "expected Done(None), got {result:?}"
    );
}

#[test]
fn validated_optional_arg_missing_is_ok() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    // "mouse" without arg is valid (optional arg)
    let result = app.run_command_validated("mouse", &registry);
    assert!(
        matches!(result, CommandResult::Done(_)),
        "mouse with no arg should be valid, got {result:?}"
    );
}

// These tests drive the modal state machine with raw `KeyEvent`s
// and confirm that `:` reaches `run_command_validated` through
// `dispatch_key` and that a `/` draft reaches it through submit.

#[test]
fn colon_keystrokes_dispatch_quit_handler() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    // Default mode is Insert; switch to Normal so `:` is bound.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let exit = app.handle_key(key(':'));
    assert!(exit.is_none());
    assert!(app.cmdline.is_some(), "':' should open the command line");
    type_str(&mut app, "quit");
    let exit = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(exit, Some(AppExit::Quit));
    assert!(app.cmdline.is_none(), "successful submit closes cmdline");
}

/// Two plugin commands whose names share the prefix `zz-`, so the tab
/// tests do not depend on which builtins start alike.
fn lcp_commands(app: &mut App) {
    let command = |name: &str| PluginCommand {
        name: name.into(),
        aliases: Vec::new(),
        is_override: false,
        description: String::new(),
        args: Vec::new(),
    };
    app.set_plugin_commands(vec![command("zz-one"), command("zz-two")]);
}

#[test]
fn colon_tab_completes_to_lcp_and_opens_popup() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    lcp_commands(&mut app);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(key(':'));
    app.handle_key(key('z'));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let cl = app.cmdline.as_ref().expect("cmdline open");
    assert_eq!(cl.text(), "zz-", "tab should extend to the LCP");
    assert!(cl.popup_open(), "popup should be visible after LCP step");
    assert_eq!(cl.selected(), None, "LCP step does not pre-select a row");
}

#[test]
fn slash_draft_completes_commands_on_the_prompt() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    lcp_commands(&mut app);
    type_str(&mut app, "/z");
    let completion = app.input_completion.as_ref().expect("popup open");
    assert!(completion.items().iter().any(|i| i.value == "zz-one"));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.input().text(), "/zz-one");
}

#[test]
fn colon_bad_arg_keeps_cmdline_open_with_error() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(key(':'));
    type_str(&mut app, "mouse mayb");
    let exit = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(exit.is_none());
    let cl = app.cmdline.as_ref().expect("cmdline stays open on bad arg");
    assert!(cl.error().is_some(), "validation error should be set");
}

#[test]
fn slash_reload_sends_reload_plugins() {
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    type_str(&mut app, "/reload");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(rx.try_recv(), Ok(RunRequest::ReloadPlugins));
}

#[test]
fn login_command_defers_to_the_run_loop_via_pending_login() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_login_runner(std::sync::Arc::new(|_| true));
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();

    let result = app.run_command_validated("login", &registry);
    assert!(matches!(result, CommandResult::Done(None)));
    assert_eq!(app.pending_login, Some(crate::app::PendingLogin::Picker));

    let result = app.run_command_validated("login anthropic", &registry);
    assert!(matches!(result, CommandResult::Done(None)));
    assert_eq!(
        app.pending_login,
        Some(crate::app::PendingLogin::Provider("anthropic".to_owned()))
    );
}

#[test]
fn login_command_errors_without_a_runner() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();

    let result = app.run_command_validated("login", &registry);
    assert!(matches!(result, CommandResult::Done(None)));
    assert_eq!(app.pending_login, None, "no flow queued without a runner");
    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last().map(Arc::as_ref) {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
    };
    assert!(rendered.contains("login: unavailable"), "{rendered}");
}

#[test]
fn export_uses_the_unquoted_parsed_path() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let res = app.run_command_validated("export \"a b.md\"", &builtin_registry());
    assert!(matches!(res, CommandResult::Done(None)));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::ExportSession(dest) => {
            assert_eq!(dest, Some(std::path::PathBuf::from("a b.md")));
        }
        other => panic!("expected ExportSession, got {other:?}"),
    }
}

#[test]
fn image_opens_by_default_and_saves_to_a_parsed_path() {
    use kage_core::protocol::ImageAction;
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    for (line, want_action, want_path) in [
        ("image", ImageAction::Open, None),
        ("image open", ImageAction::Open, None),
        ("image save", ImageAction::Save, None),
        (
            "image save \"art/a cat.png\"",
            ImageAction::Save,
            Some(std::path::PathBuf::from("art/a cat.png")),
        ),
    ] {
        let res = app.run_command_validated(line, &builtin_registry());
        assert!(matches!(res, CommandResult::Done(None)), "{line}");
        match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
            RunRequest::Image { action, path } => {
                assert_eq!((action, path), (want_action, want_path), "{line}");
            }
            other => panic!("{line}: expected Image, got {other:?}"),
        }
    }
}

#[test]
fn media_opens_by_default_and_saves_to_a_parsed_path() {
    use kage_core::protocol::ImageAction;
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    for (line, want_action, want_path) in [
        ("media", ImageAction::Open, None),
        ("media open", ImageAction::Open, None),
        ("media save", ImageAction::Save, None),
        (
            "media save \"a b/c.mp3\"",
            ImageAction::Save,
            Some(std::path::PathBuf::from("a b/c.mp3")),
        ),
    ] {
        let res = app.run_command_validated(line, &builtin_registry());
        assert!(matches!(res, CommandResult::Done(None)), "{line}");
        match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
            RunRequest::Media { action, path } => {
                assert_eq!((action, path), (want_action, want_path), "{line}");
            }
            other => panic!("{line}: expected Media, got {other:?}"),
        }
    }
}

#[test]
fn slash_missing_argument_restores_the_draft_with_the_reason() {
    let mut app = defaults_app();
    app.set_toasts(crate::toast::shared_toasts());
    type_str(&mut app, "/theme set");
    app.handle_key(code(KeyCode::Enter));
    assert_eq!(app.input().text(), "/theme set", "the draft is restored");
    let toasts = app.live_toasts();
    assert!(
        toasts
            .iter()
            .any(|t| t.text.contains("missing required argument `name`")),
        "{toasts:?}"
    );
}

#[test]
fn slash_descriptions_line_up_after_the_argument_hints() {
    let mut app = defaults_app();
    type_str(&mut app, "/m");
    let completion = app.input_completion.as_ref().expect("popup open");
    let detail = |value: &str| match completion.items().iter().find(|i| i.value == value) {
        Some(i) => i.detail.clone().unwrap_or_default(),
        None => panic!("{value}: {completion:?}"),
    };
    assert!(detail("model").starts_with("[id] \u{b7} switch to provider/model"));
    assert!(detail("mouse").starts_with("[on|off|toggle] \u{b7} toggle mouse capture"));
    assert!(detail("mcp").starts_with("[restart|login] \u{b7} list MCP servers"));
}

#[test]
fn answers_to_commands_land_in_the_conversation() {
    let (mut app, _rx, _events) = app_with_events();
    let registry = builtin_registry();
    for (line, want) in [
        ("theme current", "theme: "),
        ("mcp", "Add one under [mcp.servers.<name>] in config.toml"),
        ("agents", "no agents in this session yet"),
        ("permission", "permission mode: default"),
    ] {
        let result = app.run_command_validated(line, &registry);
        assert!(matches!(result, CommandResult::Done(None)), "{line}");
        let text = last_block_text(&app.buffer);
        assert!(text.contains(want), "{line}: {text}");
    }
}

#[test]
fn a_slash_submission_runs_the_command() {
    let (mut app, rx, _events) = app_with_events();
    app.handle_submit("/swarm whatever".into(), false);
    assert!(matches!(
        rx.try_recv(),
        Ok(RunRequest::SwarmMode { on: true })
    ));
    match rx.try_recv() {
        Ok(RunRequest::Submit {
            text,
            session: None,
            queue: false,
            ..
        }) => assert_eq!(text, "whatever"),
        other => panic!("expected the task prompt, got {other:?}"),
    }
    assert!(rx_idle(&rx), "the literal text must not submit");
}

#[test]
fn a_bare_slash_submission_does_nothing() {
    let (mut app, rx, _events) = app_with_events();
    app.handle_submit("/".into(), false);
    assert!(rx_idle(&rx));
}

#[test]
fn an_unknown_slash_submission_returns_to_the_draft() {
    let (mut app, rx, _events) = app_with_events();
    app.handle_submit("/nope args".into(), false);
    assert!(rx_idle(&rx), "unknown commands send nothing");
    assert_eq!(app.input.text(), "/nope args");
}

#[test]
fn a_skill_submission_sends_its_body_and_args() {
    use kage_core::skills::Skill;
    let (mut app, rx, _events) = app_with_events();
    app.set_skills(vec![Skill {
        name: "review".into(),
        description: "review things".into(),
        body: "Review carefully.".into(),
        disable_model_invocation: false,
        path: std::path::PathBuf::from("/skills/review"),
    }]);
    assert!(
        app.command_registry()
            .iter()
            .any(|spec| spec.name == "review")
    );

    app.handle_submit("/review the input box".into(), false);
    match rx.try_recv() {
        Ok(RunRequest::Submit { text, session, .. }) => {
            assert_eq!(text, "Review carefully.\n\nthe input box");
            assert_eq!(session, None);
        }
        other => panic!("expected the skill prompt, got {other:?}"),
    }
    assert!(rx_idle(&rx), "the literal text must not submit");
}

#[test]
fn a_disabled_skill_takes_no_command() {
    use kage_core::skills::Skill;
    let (mut app, rx, _events) = app_with_events();
    app.set_skills(vec![Skill {
        name: "review".into(),
        description: "review things".into(),
        body: "Review carefully.".into(),
        disable_model_invocation: true,
        path: std::path::PathBuf::from("/skills/review"),
    }]);
    app.handle_submit("/review".into(), false);
    assert!(rx_idle(&rx));
    assert_eq!(app.input.text(), "/review");
}

#[test]
fn plan_command_toggles_sets_and_plans_one_task() {
    let (mut app, rx, _events) = app_with_events();
    let args = crate::command::ParsedArgs::new();

    app.dispatch_builtin("plan", "", &args);
    assert!(matches!(
        rx.try_recv(),
        Ok(RunRequest::PlanMode { on: true })
    ));
    if let Some(usage) = &app.session_usage {
        lock(usage).plan = true;
    }
    app.dispatch_builtin("plan", "", &args);
    assert!(
        matches!(rx.try_recv(), Ok(RunRequest::PlanMode { on: false })),
        "a bare /plan toggles from the engine's state"
    );
    app.dispatch_builtin("plan", "on", &args);
    assert!(matches!(
        rx.try_recv(),
        Ok(RunRequest::PlanMode { on: true })
    ));

    app.dispatch_builtin("plan", "map the auth flow", &args);
    assert!(matches!(
        rx.try_recv(),
        Ok(RunRequest::PlanMode { on: true })
    ));
    match rx.try_recv() {
        Ok(RunRequest::Submit {
            text, queue: false, ..
        }) => {
            assert_eq!(text, "map the auth flow");
        }
        other => panic!("expected the task prompt, got {other:?}"),
    }
}

#[test]
fn undo_command_sends_an_undo_request() {
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();
    let result = app.run_command_validated("undo", &registry);
    assert!(
        matches!(result, CommandResult::Done(None)),
        "expected Done(None), got {result:?}"
    );
    assert_eq!(rx.try_recv(), Ok(RunRequest::UndoSession));
}

#[test]
fn rewind_command_requests_the_prompt_list() {
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();
    let result = app.run_command_validated("rewind", &registry);
    assert!(
        matches!(result, CommandResult::Done(None)),
        "expected Done(None), got {result:?}"
    );
    assert_eq!(rx.try_recv(), Ok(RunRequest::ListPrompts));
}
