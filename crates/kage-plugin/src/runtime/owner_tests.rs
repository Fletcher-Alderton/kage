//! Tests for the Lua owner thread: render paths never block on it,
//! input hooks are bounded, dispatch stays ordered, reload runs there.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use kage_core::CancelFlag;
use kage_tools::{ToolContext, ToolError};
use serde_json::json;

use super::*;
use crate::api::LogLevel;
use crate::terminal_input::INPUT_DEADLINE;
use crate::test_support::{Gate, TIMEOUT, occupy, wait_until};
use crate::testing::runtime_with_recording;

fn tool_named(rt: &PluginRuntime, name: &str) -> Arc<dyn kage_tools::Tool> {
    rt.registered_tools()
        .into_iter()
        .find(|t| t.name() == name)
        .unwrap()
}

fn run_tool(tool: &Arc<dyn kage_tools::Tool>, cancel: &CancelFlag) -> Result<String, ToolError> {
    let workdir = PathBuf::from(".");
    tool.execute(json!({}), &ToolContext::new(&workdir, cancel))
        .map(|out| out.text)
}

#[test]
fn widget_render_returns_retained_text_while_the_owner_is_busy() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "kage.register_widget({ key = 'w', render = function(width) return 'w' .. width end })",
    )
    .unwrap();
    let widget = rt.registered_widgets().pop().unwrap();
    assert_eq!(widget.render(10), "w10");
    let redraw = rt.redraw_flag();
    redraw.store(false, Ordering::SeqCst);

    let busy = occupy(&rt.host);
    assert_eq!(widget.render(20), "w10");
    busy.assert_held();
    busy.open();

    wait_until(|| redraw.swap(false, Ordering::SeqCst));
    assert_eq!(widget.render(20), "w20");
}

#[test]
fn block_render_never_waits_on_a_busy_owner() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval("kage.register_block_renderer('k', function(b) return b.text end)")
        .unwrap();
    let renderer = rt.registered_block_renderers().pop().unwrap();
    let block = |text: &str| json!({ "kind": "k", "text": text, "width": 40 });
    assert_eq!(renderer.render(&block("a")).unwrap()[0].spans[0].text, "a");
    let redraw = rt.redraw_flag();
    redraw.store(false, Ordering::SeqCst);

    let busy = occupy(&rt.host);
    assert_eq!(renderer.render(&block("a")).unwrap()[0].spans[0].text, "a");
    assert!(renderer.render(&block("b")).is_none());
    busy.assert_held();
    busy.open();

    wait_until(|| redraw.swap(false, Ordering::SeqCst));
    assert_eq!(renderer.render(&block("b")).unwrap()[0].spans[0].text, "b");
}

#[test]
fn slow_terminal_hook_lets_the_key_through_and_warns_once() {
    let (rec, rt) = runtime_with_recording(PathBuf::from("."));
    let gate = Arc::new(Gate::default());
    let lua_gate = Arc::clone(&gate);
    rt.with_lua(move |lua| {
        let hold = lua
            .create_function(move |_, ()| {
                lua_gate.hold();
                Ok(())
            })
            .unwrap();
        lua.globals().set("hold", hold).unwrap();
    })
    .unwrap();
    rt.eval(
        r"
        kage.on_terminal_input(function(ev)
            if ev.code == 'slow' then hold() end
            return true
        end)
        ",
    )
    .unwrap();
    let hook = rt.registered_terminal_hooks().pop().unwrap();
    assert!(hook.handle(&json!({ "code": "enter" }), TIMEOUT));

    for _ in 0..2 {
        assert!(!hook.handle(&json!({ "code": "slow" }), INPUT_DEADLINE));
    }
    gate.assert_held();
    let warnings: Vec<_> = rec
        .snapshot()
        .logs
        .into_iter()
        .filter(|(level, msg)| *level == LogLevel::Warn && msg.contains("on_terminal_input"))
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");

    gate.open();
    wait_until(|| rt.host.is_idle());
    assert!(hook.handle(&json!({ "code": "enter" }), TIMEOUT));
}

#[test]
fn a_waiting_tool_leaves_the_owner_free_for_other_calls() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
        kage.register_tool({ name = 'slow', description = '', schema = {},
            execute = function() kage.sleep_ms(400) kage.sleep_ms(400) return 'slow' end })
        kage.register_tool({ name = 'quick', description = '', schema = {},
            execute = function() return 'quick' end })
        ",
    )
    .unwrap();
    let slow = tool_named(&rt, "slow");
    let waiting = thread::spawn(move || run_tool(&slow, &CancelFlag::new()));
    thread::sleep(Duration::from_millis(50));
    let start = std::time::Instant::now();
    assert_eq!(
        run_tool(&tool_named(&rt, "quick"), &CancelFlag::new()).unwrap(),
        "quick"
    );
    assert!(
        start.elapsed() < Duration::from_millis(400),
        "the quick call waited {:?} behind the sleeping one",
        start.elapsed()
    );
    assert_eq!(waiting.join().unwrap().unwrap(), "slow");
}

#[test]
fn a_cancelled_tool_stops_where_it_waits() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
        ticks = 0
        kage.register_tool({ name = 'spin', description = '', schema = {},
            execute = function()
                while true do ticks = ticks + 1 kage.sleep_ms(10) end
            end })
        ",
    )
    .unwrap();
    let cancel = CancelFlag::new();
    let flag = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        flag.cancel();
    });
    let result = run_tool(&tool_named(&rt, "spin"), &cancel);
    assert!(matches!(result, Err(ToolError::Cancelled)), "{result:?}");
    let ticks = || rt.eval("return ticks").unwrap().as_i64().unwrap();
    thread::sleep(Duration::from_millis(30));
    let after_cancel = ticks();
    assert!(after_cancel > 0);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(ticks(), after_cancel, "the handler kept running");
}

#[test]
fn lua_tool_honors_cancel_while_waiting_for_a_busy_owner() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
        kage.register_tool({
            name = 'fast',
            description = '',
            schema = {},
            execute = function() fast_ran = true return 'ok' end,
        })
        ",
    )
    .unwrap();
    let busy = occupy(&rt.host);
    let cancel = CancelFlag::new();
    let flag = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        flag.cancel();
    });
    let result = run_tool(&tool_named(&rt, "fast"), &cancel);
    assert!(matches!(result, Err(ToolError::Cancelled)), "{result:?}");
    busy.assert_held();
    busy.open();
    assert!(
        rt.eval("return fast_ran").unwrap().is_nil(),
        "a cancelled tool call must not run later"
    );
}

#[test]
fn dispatch_event_preserves_call_order_and_side_effects() {
    let (rec, rt) = runtime_with_recording(PathBuf::from("."));
    rt.eval(
        r"
        seen = {}
        kage.on('turn_start', function(p) table.insert(seen, p.index) end)
        kage.on('turn_end', function(p) kage.notify('end ' .. p.index) end)
        ",
    )
    .unwrap();
    for index in 0..200 {
        rt.dispatch_event("turn_start", &json!({ "index": index }))
            .unwrap();
    }
    let seen = rt
        .with_lua(|lua| lua.load("return seen").eval::<Vec<i64>>())
        .unwrap()
        .unwrap();
    assert_eq!(seen, (0..200).collect::<Vec<_>>());

    rt.dispatch_event("turn_end", &json!({ "index": 7 }))
        .unwrap();
    assert_eq!(rec.snapshot().notifications, ["end 7"]);
}

#[test]
fn reload_dir_replaces_handlers_and_render_surfaces() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = |tag: &str| {
        format!(
            "kage.register_widget({{ key = 'w', render = function() return '{tag}' end }})
             kage.on('turn_start', function() kage.notify('{tag}') end)"
        )
    };
    std::fs::write(dir.path().join("a.lua"), plugin("v1")).unwrap();
    let (rec, rt) = runtime_with_recording(PathBuf::from("."));
    crate::loader::load_dir(dir.path(), &rt).unwrap();
    assert_eq!(rt.registered_widgets()[0].render(80), "v1");

    std::fs::write(dir.path().join("a.lua"), plugin("v2")).unwrap();
    let report = rt.reload_dir(dir.path()).unwrap();
    assert_eq!(report.loaded.len(), 1);
    assert_eq!(rt.handler_count("turn_start"), 1);
    let widgets = rt.registered_widgets();
    assert_eq!(widgets.len(), 1);
    assert_eq!(widgets[0].render(80), "v2");
    rt.dispatch_event("turn_start", &json!({})).unwrap();
    assert_eq!(rec.snapshot().notifications, ["v2"]);
}

#[test]
fn render_surfaces_outlive_the_runtime_handle() {
    let renderer = {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.register_block_renderer('k', function(b) return b.text end)")
            .unwrap();
        rt.registered_block_renderers().pop().unwrap()
    };
    let lines = renderer
        .render(&json!({ "kind": "k", "text": "still here" }))
        .unwrap();
    assert_eq!(lines[0].spans[0].text, "still here");
}

#[test]
fn handler_count_answers_while_the_owner_is_busy() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval("kage.on('turn_start', function() end)").unwrap();
    let busy = occupy(&rt.host);
    assert_eq!(rt.handler_count("turn_start"), 1);
    assert_eq!(rt.handler_count("turn_end"), 0);
    busy.assert_held();
    busy.open();
}

#[test]
fn dispatch_without_subscribers_skips_the_owner() {
    let rt = PluginRuntime::new().unwrap();
    let busy = occupy(&rt.host);
    rt.dispatch_event("turn_end", &json!({})).unwrap();
    let payload = rt
        .dispatch_transform("transform_context", json!({ "keep": 1 }))
        .unwrap();
    assert!(
        !rt.dispatch_predicate("should_stop_after_turn", &json!({}))
            .unwrap()
    );
    rt.notify_event("turn_end", &json!({})).unwrap();
    busy.assert_held();
    busy.open();
    assert_eq!(payload, json!({ "keep": 1 }));
}

#[test]
fn notify_event_returns_before_its_handler_runs() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval("hits = 0; kage.on('user_shell', function(p) hits = hits + p.n end)")
        .unwrap();
    let busy = occupy(&rt.host);
    rt.notify_event("user_shell", &json!({ "n": 2 })).unwrap();
    busy.assert_held();
    busy.open();
    assert_eq!(rt.eval("return hits").unwrap().as_integer(), Some(2));
}

#[test]
fn queued_repeats_of_one_event_coalesce_into_the_newest_payload() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "seen = {};\
         kage.on('turn_start', function(p) seen[#seen + 1] = 'a' .. p.n end);\
         kage.on('turn_end', function() seen[#seen + 1] = 'b' end)",
    )
    .unwrap();
    let busy = occupy(&rt.host);
    rt.notify_event("turn_start", &json!({ "n": 1 })).unwrap();
    rt.notify_event("turn_end", &json!({})).unwrap();
    rt.notify_event("turn_start", &json!({ "n": 2 })).unwrap();
    busy.assert_held();
    busy.open();
    let seen = match rt.eval("return table.concat(seen, ',')").unwrap() {
        mlua::Value::String(s) => s.to_str().unwrap().to_owned(),
        other => panic!("expected a string, got {other:?}"),
    };
    assert_eq!(seen, "a2,b");
}

#[test]
fn failed_plugin_eval_leaves_no_stale_current_plugin() {
    let rt = PluginRuntime::builder()
        .workdir(PathBuf::from("."))
        .capabilities(BTreeMap::from([("p".to_owned(), vec!["exec".to_owned()])]))
        .build()
        .unwrap();
    let eval = Arc::clone(&rt.eval);
    rt.with_lua(move |lua| {
        let env = eval.env(lua, "q").unwrap();
        let boom = lua
            .create_function(|_, ()| -> mlua::Result<()> { panic!("boom") })
            .unwrap();
        env.raw_set("boom", boom).unwrap();
        // mlua re-raises a callback panic at the Rust call boundary, so
        // eval_in unwinds instead of returning an error.
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            eval.eval_in(lua, "q", "=probe", env, "boom()")
        }));
        assert!(outcome.is_err());
        assert!(lock(&eval.current_plugin).is_none());
    })
    .unwrap();
    assert!(lock(&rt.eval.current_plugin).is_none());
    let granted = rt
        .eval_plugin(
            "p",
            "return kage.request_capabilities({'exec'}).exec == true",
        )
        .unwrap();
    assert_eq!(granted.as_boolean(), Some(true));
}
