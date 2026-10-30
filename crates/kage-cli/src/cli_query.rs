//! Resume, search, and fork subcommand handlers.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use kage_core::{Content, Message, Role};
use kage_loop::AgentContext;
use kage_session::{SessionId, SessionWriter};

use crate::cli_loop_run::execute_print_run;
use crate::plugins::setup_runtime;
use crate::{
    DEFAULT_SYSTEM, NO_CREDENTIALS_MESSAGE, acp_glue, build_provider_registry, has_usable_provider,
    plugins, plugins_dir, runtime_env, sessions_dir, state, truncate_one_line, tui,
};

/// Implement `kage resume`: open an existing session in the TUI, or with
/// `print`, replay it and append a new user prompt before re-running the
/// loop.
#[expect(clippy::too_many_lines, reason = "one linear resume sequence")]
pub(crate) fn run_resume(
    id: Option<&str>,
    last: bool,
    print: Option<&str>,
    model_override: Option<&str>,
    json: bool,
) -> ExitCode {
    let dir = match sessions_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let path = match resolve_resume_target(&dir, id, last) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let Some(prompt) = print else {
        return tui::run_tui(model_override, DEFAULT_SYSTEM, Some(path), false);
    };

    let replay = match kage_session::replay(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kage: failed to replay session {}: {e}", path.display());
            return ExitCode::from(1);
        }
    };

    let mut registry = match build_provider_registry() {
        Ok(registry) => registry,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let provisional_model = kage_core::canonical_model(model_override.unwrap_or(&replay.model));
    let plugin_runtime = match plugins_dir() {
        Ok(dir) => match setup_runtime(
            &dir,
            &workdir,
            &provisional_model,
            &replay.header.system_prompt,
        ) {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("kage: {e}");
                None
            }
        },
        Err(e) => {
            eprintln!("kage: {e}");
            None
        }
    };

    if let Some(rt) = plugin_runtime.as_ref() {
        for id in plugins::merge_plugin_providers(rt, &mut registry) {
            eprintln!("kage: plugin provider `{id}` shadows the built-in registration");
        }
        acp_glue::set_runtime(rt);
    }

    let model = provisional_model;
    if !has_usable_provider(&registry) && registry.resolve(&model).is_err() {
        eprintln!("{NO_CREDENTIALS_MESSAGE}");
        return ExitCode::from(1);
    }
    let resolved = match registry.resolve(&model) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kage: cannot resolve model {model}: {e}");
            return ExitCode::from(1);
        }
    };

    let writer = match SessionWriter::open(&path) {
        Ok(w) => {
            eprintln!("kage: appending to session {}", w.path().display());
            w
        }
        Err(e) => {
            eprintln!("kage: failed to reopen session file: {e}");
            return ExitCode::from(1);
        }
    };
    let Ok((_layered, tools, mcp_manager)) = crate::run_tools(&workdir, plugin_runtime.as_deref())
    else {
        return ExitCode::from(1);
    };
    let mut cx = AgentContext::new(resolved.model.clone(), &replay.header.system_prompt)
        .with_workdir(&workdir);
    if let Some(window) = runtime_env::context_window_for(&registry, &model) {
        cx = cx.with_context_window(window);
    }
    if let Some(out) = runtime_env::max_output_tokens_for(&registry, &model) {
        cx = cx.with_max_output_tokens(out);
    }
    cx.history = replay.history.into_iter().map(Arc::new).collect();
    cx.budget.used_input = replay.usage_total.input;
    cx.budget.used_output = replay.usage_total.output;
    cx.budget.used_cache_read = replay.usage_total.cache_read;
    cx.budget.used_cache_write = replay.usage_total.cache_write;
    cx.budget.current_context = replay.usage_total.last_context;
    let exit = execute_print_run(
        Arc::new(registry),
        &model,
        tools,
        cx,
        prompt.to_owned(),
        Some(writer),
        plugin_runtime,
        Some(mcp_manager),
        json,
        false,
    );
    if let Err(err) = state::record_last_model(&model) {
        eprintln!("kage: {err}");
    }
    exit
}

/// Upper bound on `kage search` hits; keeps huge session dirs from
/// flooding the terminal.
const MAX_SEARCH_HITS: usize = 200;

/// Implement `kage search <query>`: regex-grep across the sessions dir and
/// render each hit as `<file>:<line>: <text>`.
pub(crate) fn run_search(query: &str) -> ExitCode {
    let dir = match sessions_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let hits = match kage_session::search(&dir, query, MAX_SEARCH_HITS) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("kage: search failed: {e}");
            return ExitCode::from(1);
        }
    };
    if hits.is_empty() {
        eprintln!("kage: no matches in {}", dir.display());
        return ExitCode::SUCCESS;
    }
    let mut stdout = io::stdout().lock();
    for hit in &hits {
        let name = hit
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| hit.path.to_str().unwrap_or("?"));
        let preview = render_hit_preview(hit);
        let _ = writeln!(stdout, "{name}:{}: {preview}", hit.line_no);
    }
    ExitCode::SUCCESS
}

/// One-line preview text for a search hit. Decodes the matched JSONL into
/// a `SessionEntry` when possible and surfaces only the human-readable
/// fragments; raw lines fall through unchanged.
pub(crate) fn render_hit_preview(hit: &kage_session::SearchHit) -> String {
    let Some(entry) = hit.entry() else {
        return hit.line.clone();
    };
    match entry {
        kage_session::SessionEntry::Header(h) => format!("[header] model={}", h.model),
        kage_session::SessionEntry::Message(m) => {
            let role = match m.message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::ToolResult => "tool",
                Role::System => "system",
            };
            let text = first_text_block(&m.message).unwrap_or_else(|| "(no text)".to_owned());
            format!("[{role}] {}", truncate_one_line(&text, 120))
        }
        kage_session::SessionEntry::Compaction(c) => {
            format!("[compaction] kept={} summarized={}", c.kept, c.summarized)
        }
        kage_session::SessionEntry::Label(l) => format!("[label] {}", l.text),
        kage_session::SessionEntry::Branch(_) => "[branch]".to_owned(),
        kage_session::SessionEntry::Title(t) => format!("[title] {}", t.title),
        kage_session::SessionEntry::ModelChange(m) => format!("[model_change] {}", m.model),
        kage_session::SessionEntry::ThinkingLevelChange(t) => {
            format!("[thinking_level] {}", t.level)
        }
        kage_session::SessionEntry::Custom(c) => format!("[custom:{}]", c.kind),
    }
}

pub(crate) fn first_text_block(message: &Message) -> Option<String> {
    for block in &message.content {
        if let Content::Text { text } = block {
            return Some(text.clone());
        }
        if let Content::ToolResultBlock { output, .. } = block {
            return Some(output.clone());
        }
    }
    None
}

/// Implement `kage fork`: copy a session up through a chosen entry into a
/// fresh file with a new session id, linked back to the source via the
/// header's `parent_session` and `parent_entry` fields.
pub(crate) fn run_fork(src_id_prefix: &str, at_prefix: &str) -> ExitCode {
    let dir = match sessions_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let src_path = match kage_session::find_by_prefix(&dir, src_id_prefix) {
        Ok(Some(p)) => p,
        Ok(None) => {
            eprintln!("kage: no session matches prefix '{src_id_prefix}'");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("kage: failed to resolve session id: {e}");
            return ExitCode::from(1);
        }
    };
    let at = match kage_session::resolve_entry_prefix(&src_path, at_prefix) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("kage: failed to resolve entry id: {e}");
            return ExitCode::from(1);
        }
    };
    let new_session = SessionId::new();
    let dst = dir.join(format!("{new_session}.jsonl"));
    if let Err(e) = kage_session::fork(&src_path, &dst, new_session, at) {
        eprintln!("kage: fork failed: {e}");
        return ExitCode::from(1);
    }
    println!("{new_session}");
    eprintln!("kage: forked {} into {}", src_path.display(), dst.display());
    ExitCode::SUCCESS
}

/// Resolve which session file to resume based on cli flags.
pub(crate) fn resolve_resume_target(
    dir: &std::path::Path,
    id: Option<&str>,
    last: bool,
) -> Result<PathBuf, String> {
    if last {
        let cwd = std::env::current_dir().map_err(|e| format!("current directory: {e}"))?;
        return kage_session::find_last(dir, &cwd)
            .map_err(|e| format!("failed to scan sessions: {e}"))?
            .ok_or_else(|| format!("no sessions recorded in {}", cwd.display()));
    }
    let prefix = id.ok_or_else(|| {
        "resume needs a session id or --last; to pick one, run `kage` and press ctrl+s".to_owned()
    })?;
    kage_session::find_by_prefix(dir, prefix)
        .map_err(|e| format!("failed to resolve session id: {e}"))?
        .ok_or_else(|| format!("no session matches prefix '{prefix}'"))
}
