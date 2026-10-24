//! kage CLI binary.
//!
//! Layering: top of the workspace; depends on every library crate it uses.
//!
//! Without a subcommand kage opens the interactive TUI. Print mode (`-p`)
//! runs a single prompt through the agent loop, streams the reply to
//! stdout, and exits. Subcommands:
//!
//! - `list`, `resume`, `fork` and `search` work on the recorded sessions
//!   under `$XDG_DATA_HOME/kage/sessions/`.
//! - `auth` manages saved provider credentials, and `init` is the
//!   first-run setup wizard.
//! - `doctor` diagnoses the install, and `trust` trusts a project config.
//! - `models` manages the model catalog, and `plugin` the plugins
//!   directory and installs from kura, the plugin registry.
//! - `rpc` serves the Agent Client Protocol over stdio, and `serve`
//!   serves it over WebSocket behind a token, so editors and ACP
//!   clients drive kage locally or over the network; `serve` also
//!   serves the web client bundle at `/` from `--web-dir`. `mcp` serves
//!   the built-in tools over the Model Context Protocol and logs in to
//!   MCP servers.
//! - `completions` and the hidden `gen-manpage` generate shell and man
//!   page files.

/// mimalloc hands freed memory back to the system. glibc's allocator
/// keeps the peak of a long session's resume for the life of the
/// process, and musl's is slow.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod acp_glue;
mod agents;
mod auth;
mod doctor;
mod engine;
mod goal;
mod history;
mod init;
mod mcp;
mod mcp_auth;
mod opener;
mod paths;
mod permissions;
mod plugin_cmd;
mod plugins;
mod providers;
mod rpc;
mod runtime_env;
mod serve;
#[cfg(unix)]
mod serve_registry;
mod state;
mod title;
mod trust;
mod tui;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use clap::{Parser, Subcommand};
use kage_loop::AgentContext;
use kage_session::{EntryId, FORMAT_VERSION, Header, SessionId, SessionSummary, SessionWriter};
use kage_tools::builtin_registry;

pub(crate) use paths::{
    cache_root, config_dir, data_root, models_cache_path, plugins_dir, sessions_dir, state_root,
    themes_dir,
};
pub(crate) use providers::{
    NO_CREDENTIALS_MESSAGE, NO_MODEL_MESSAGE, build_provider_registry, configured_default_model,
    default_model, has_usable_provider, validate_providers,
};

use crate::plugins::setup_runtime;

/// The system prompt role when `--system` is not given.
pub(crate) const DEFAULT_SYSTEM: &str = "You are kage, a helpful coding agent.";

/// kage: a fast, hackable coding agent.
#[derive(Parser, Debug)]
#[command(
    name = "kage",
    version = env!("KAGE_VERSION"),
    about = "A fast, hackable coding agent for your terminal, editor and browser",
    long_about = None
)]
struct Cli {
    /// Subcommand. With no subcommand, `-p/--print` runs print mode;
    /// with neither, the interactive TUI opens.
    #[command(subcommand)]
    command: Option<Command>,

    /// Run a single prompt through the agent loop and stream the response
    /// to stdout.
    #[arg(short = 'p', long = "print")]
    print: Option<String>,

    /// Provider-qualified model id (`provider/model`). Defaults to
    /// `[provider] default_model` when its provider has credentials,
    /// then the last model you used, then the first available
    /// provider's preferred model.
    #[arg(short = 'm', long = "model", value_parser = model_arg)]
    model: Option<String>,

    /// System prompt to prepend.
    #[arg(long = "system", default_value = DEFAULT_SYSTEM)]
    system: String,

    /// Disable session recording. By default every run writes a JSONL
    /// session file under `$XDG_DATA_HOME/kage/sessions/<session-id>.jsonl`.
    #[arg(long = "no-session")]
    no_session: bool,

    /// Start the session in allow mode: every tool call runs without
    /// asking, MCP servers included. A configured deny still denies.
    /// Same effect as `/permission allow`, applied at startup.
    #[arg(long)]
    yolo: bool,

    /// Emit one JSON object per event on stdout instead of plain text.
    /// Only meaningful with `-p/--print`. Each line carries the event's
    /// `type` (such as `text_delta`, `tool_call_start`, `message_appended`,
    /// or `run_ended`) plus the `session` it belongs to and a per-session
    /// `seq` number, so external tools can parse the agent's output
    /// without screen-scraping.
    #[arg(long = "json", requires = "print")]
    json: bool,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// List recorded sessions in `$XDG_DATA_HOME/kage/sessions/`.
    List,
    /// Resume a recorded session, appending new entries to the same file.
    Resume {
        /// Session id or unique prefix. Mutually exclusive with --last.
        id: Option<String>,
        /// Resume the most recently created session of the current
        /// directory.
        #[arg(long = "last", conflicts_with = "id")]
        last: bool,
        /// New user prompt to append in print mode. Without it the
        /// session opens in the interactive TUI.
        #[arg(short = 'p', long = "print")]
        print: Option<String>,
        /// Override the recorded model. Defaults to the model the session
        /// was last using.
        #[arg(short = 'm', long = "model", value_parser = model_arg)]
        model: Option<String>,
        /// Emit one JSON object per event on stdout instead of plain
        /// text. Same format as the top-level `--json` flag.
        #[arg(long = "json", requires = "print")]
        json: bool,
    },
    /// Fork a recorded session at a specific entry into a new session file.
    Fork {
        /// Source session id or unique prefix.
        id: String,
        /// Entry id (or unique prefix) to fork at; everything up through
        /// this entry is copied into the new session.
        #[arg(long = "at")]
        at: String,
    },
    /// Regex-search recorded sessions.
    Search {
        /// Regex query, ripgrep-style.
        query: String,
    },
    /// Manage saved provider API credentials.
    Auth {
        /// Auth subcommand.
        #[command(subcommand)]
        action: AuthAction,
    },
    /// First-run setup wizard. Creates `~/.config/kage/config.toml`,
    /// scaffolds the data directories, and offers to save a provider
    /// API key. Idempotent: rerunning without `--force` keeps any
    /// existing config in place.
    Init {
        /// Overwrite an existing `~/.config/kage/config.toml` instead of
        /// keeping it.
        #[arg(long = "force")]
        force: bool,
        /// Skip every prompt. Useful for scripted bootstrap; will
        /// still write a fresh config when one is missing but never
        /// asks for an API key.
        #[arg(long = "non-interactive")]
        non_interactive: bool,
    },
    /// Diagnose the kage install: parses config, lists available
    /// providers, validates plugins, probes each MCP server.
    /// Exit code is `0` when no check fails, `1` otherwise.
    Doctor,
    /// Render the `kage(1)` manpage from the clap CLI definition and
    /// write it to `--out` (default `man/kage.1`). Hidden because it
    /// is a developer / packager command: end users read the
    /// committed file at `man/kage.1`.
    #[command(hide = true)]
    GenManpage {
        /// Destination path for the generated manpage.
        #[arg(long = "out", default_value = "man/kage.1")]
        out: PathBuf,
        /// Compare the rendered page against the file at `--out` and
        /// fail when they differ, instead of writing. The CI man-page
        /// gate runs this so a CLI change without a page refresh is
        /// caught before it ships.
        #[arg(long = "check")]
        check: bool,
    },
    /// Print a shell completion script for `kage` to stdout.
    ///
    /// Pipe it to `source` (bash, zsh) or redirect it into your
    /// shell's completion directory (fish, elvish), as in these
    /// examples:
    ///
    ///   `source <(kage completions bash)`
    ///
    ///   `kage completions fish > ~/.config/fish/completions/kage.fish`
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Agent Client Protocol server: speak JSON-RPC over stdio so an
    /// editor (Zed, Neovim, ...) can drive kage. Messages are
    /// newline-delimited JSON. Each request is answered, and loop
    /// progress is streamed back as ACP `session/update` notifications.
    Rpc {
        /// Provider-qualified model id (`provider/model`). Defaults
        /// the same way as the top-level `-m`.
        #[arg(short = 'm', long = "model", value_parser = model_arg)]
        model: Option<String>,
        /// System-prompt role override forwarded to the agent loop.
        #[arg(long = "system", default_value = "")]
        system: String,
        /// A file whose contents are appended to the system prompt.
        ///
        /// For a prompt too long or too private to spell out on the
        /// command line, which `/proc/<pid>/cmdline` publishes to every
        /// user on the host. Read once, before any session starts; a
        /// file that cannot be read is an error rather than a session
        /// that starts without its instructions.
        #[arg(long = "system-file", value_name = "PATH")]
        system_file: Option<PathBuf>,
        /// Start every session in allow mode: every tool call runs
        /// without asking, MCP servers included. A configured deny
        /// still denies, and a client may still change the mode.
        #[arg(long)]
        yolo: bool,
    },
    /// Serve the Agent Client Protocol over WebSocket so remote editors
    /// and ACP clients can drive kage over the network. One endpoint
    /// (`/acp`) behind a bearer token; every connection shares one
    /// engine. The same listener serves the web client bundle at `/`,
    /// so a browser needs only the page URL and the token. The startup output on stderr carries the connect
    /// URL, the only place the token is ever printed.
    Serve {
        /// Provider-qualified model id (`provider/model`). Defaults
        /// the same way as the top-level `-m`.
        #[arg(short = 'm', long = "model", value_parser = model_arg)]
        model: Option<String>,
        /// System-prompt role override forwarded to the agent loop.
        #[arg(long = "system", default_value = "")]
        system: String,
        /// Address to bind. Loopback by default; a non-loopback
        /// address prints a warning because kage itself has no TLS.
        #[arg(long = "host", default_value = "127.0.0.1")]
        host: String,
        /// TCP port to bind. `0` picks a free port and the connect
        /// URL shows it.
        #[arg(long = "port", default_value_t = serve::DEFAULT_PORT)]
        port: u16,
        /// Replace the stored token with a fresh one before serving,
        /// so every client holding the old connect URL must be sent
        /// the new one.
        #[arg(long = "rotate-token")]
        rotate_token: bool,
        /// Directory with the web client bundle to serve at `/` (built
        /// from `crates/kage-desktop`). Defaults to
        /// `$XDG_DATA_HOME/kage/web`. A missing or empty directory only
        /// disables the web UI; `/acp` keeps working.
        #[arg(long = "web-dir")]
        web_dir: Option<PathBuf>,
        /// Download the web client bundle matching this kage into the
        /// web directory, replacing what is there, then serve.
        #[arg(long = "fetch-web")]
        fetch_web: bool,
    },
    /// Trust the current directory's `.kage/config.toml`. Until a
    /// project is trusted, its `mcp`, `permissions` and
    /// `plugins.capabilities` settings are ignored. Trust covers the
    /// values as they are now, so editing any of them asks again.
    Trust {
        /// Forget the trust recorded for this directory.
        #[arg(long)]
        revoke: bool,
    },
    /// Manage the model catalog. kage ships a snapshot of the curated
    /// catalog; `kage models refresh` fetches a newer one into the model
    /// cache, which later runs use in place of the snapshot's model
    /// entries.
    Models {
        /// Models sub-action.
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Manage plugins: list them, install and update packages from the
    /// plugin registry (kura unless `[plugins] registry` names
    /// another), grant capabilities, and turn plugins on or off.
    Plugin {
        /// Plugin sub-action.
        #[command(subcommand)]
        action: PluginAction,
    },
    /// Model Context Protocol: expose kage's built-in tools to another
    /// agent over stdio (newline-delimited JSON-RPC) with `kage mcp
    /// serve`, or log in to remote MCP servers that need OAuth.
    Mcp {
        /// MCP sub-action.
        #[command(subcommand)]
        action: McpAction,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ModelsAction {
    /// Download the curated model catalog into
    /// `$XDG_CACHE_HOME/kage/models.json`. It can add models and update
    /// their metadata, never provider endpoints or credentials. Only
    /// runs when asked; kage never refreshes on its own.
    Refresh {
        /// Read the catalog from here instead of the published feed. A
        /// `file://` URL reads a local file, which is how an offline run
        /// or a test pins a catalog. The `KAGE_CATALOG_URL`
        /// environment variable sets the same thing.
        #[arg(long)]
        source: Option<String>,
    },
    /// List the models a provider endpoint serves. Names a saved
    /// provider to test its saved settings; the flags fill what the
    /// config leaves out, or probe an endpoint that is not configured.
    Fetch {
        /// Provider id: a `[providers.<id>]` entry, an override, or a
        /// builtin such as `openai` or `anthropic`.
        provider: String,
        /// Endpoint base URL, when the provider has none saved.
        #[arg(long)]
        base_url: Option<String>,
        /// Wire protocol: openai, anthropic or gemini. Guessed from
        /// the provider when omitted.
        #[arg(long)]
        kind: Option<String>,
        /// API key, used for this request only.
        #[arg(long)]
        api_key: Option<String>,
        /// Environment variable that holds the API key.
        #[arg(long)]
        api_key_env: Option<String>,
        /// The URL the model list is read from, when the endpoint's
        /// own will not do - a models.dev-style directory document,
        /// say. Overrides the provider's saved models URL. A key and
        /// headers are only sent when it shares the base URL's host
        /// and port.
        #[arg(long)]
        models_url: Option<String>,
        /// Save the fetched models into the saved
        /// `[providers.custom.<provider>]` entry: ids it does not list
        /// yet are appended as fresh `[[...models]]` tables, listed
        /// ones and their comments stay untouched. Needs the entry to
        /// exist; registered providers keep their models in the
        /// catalog instead.
        #[arg(long)]
        save: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum PluginAction {
    /// List installed plugins: kind, version, where each came from, and
    /// whether it is disabled, edited or has an update.
    List,
    /// Search the registry by name, description and tags. Without a
    /// query, list every package.
    Search {
        /// Words that must all match.
        query: Vec<String>,
        /// Fetch the index again instead of using the cached one.
        #[arg(long)]
        refresh: bool,
    },
    /// Show a registry package: its releases, capabilities and links.
    Info {
        /// Package name.
        name: String,
        /// Fetch the index again instead of using the cached one.
        #[arg(long)]
        refresh: bool,
    },
    /// Install a package from the registry, checking every file against
    /// the index. Lists the capabilities it asks for and offers to grant
    /// them.
    Install {
        /// `name`, or `name@version` for a given release.
        package: String,
        /// Replace a script or local package of the same name, or
        /// discard local changes to an installed one.
        #[arg(long)]
        force: bool,
        /// Grant what the package asks for without asking.
        #[arg(long)]
        grant: bool,
    },
    /// Update packages installed from the registry to their newest
    /// release this kage can load.
    Update {
        /// Packages to update; every one when none is named.
        names: Vec<String>,
        /// Discard local changes to a package being updated.
        #[arg(long)]
        force: bool,
        /// Grant newly asked-for capabilities without asking.
        #[arg(long)]
        grant: bool,
    },
    /// Remove a plugin, script or package. A linked package loses only
    /// its link.
    Remove {
        /// Plugin name.
        name: String,
    },
    /// Grant a plugin capabilities in `[plugins.capabilities]`. Without
    /// names, grants everything a package's plugin.toml declares.
    Grant {
        /// Plugin name.
        name: String,
        /// Capabilities to grant.
        capabilities: Vec<String>,
    },
    /// Take capabilities back from a plugin; all of them without names.
    Revoke {
        /// Plugin name.
        name: String,
        /// Capabilities to revoke.
        capabilities: Vec<String>,
    },
    /// Let a disabled plugin load again.
    Enable {
        /// Plugin name.
        name: String,
    },
    /// Keep a plugin from loading without removing it, through
    /// `[plugins] disabled`.
    Disable {
        /// Plugin name.
        name: String,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum McpAction {
    /// Serve kage's built-in tools as an MCP server over stdio. Calls
    /// are checked against `[permissions]`, and a tool whose verdict is
    /// `ask` is refused because there is no one to ask.
    Serve {
        /// Comma-separated built-in tools to expose. Unknown names are
        /// an error.
        #[arg(long, value_delimiter = ',', default_value = "read,grep,find,ls")]
        tools: Vec<String>,
    },
    /// Log in to a remote MCP server with OAuth. Prints the
    /// authorization URL, opens it in a browser when one is available,
    /// and waits for the browser to return, or for the redirected URL to
    /// be pasted on a machine without one. The token is stored in
    /// `$XDG_DATA_HOME/kage/mcp-auth.json` (mode 0600).
    Login {
        /// Server name from `[mcp.servers.<name>]`.
        server: String,
    },
    /// Forget the stored OAuth token of a remote MCP server.
    Logout {
        /// Server name from `[mcp.servers.<name>]`.
        server: String,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum AuthAction {
    /// Save an API key for a provider, prompting for it without echo.
    Login {
        /// Provider id, or `generate_image` for the key the image tool
        /// sends to its `base_url`. Omit to pick from a list.
        provider: Option<String>,
    },
    /// Remove a saved API key.
    Logout {
        /// Provider id.
        provider: String,
    },
    /// Show which known and custom providers have credentials available.
    List,
}

/// Dispatch a parsed subcommand. Lives outside [`main`] so its body
/// doesn't push the entry point past clippy's `too_many_lines`
/// threshold; the print-mode path below still runs from [`main`]
/// directly because it needs every local in scope.
pub(crate) fn run_subcommand(command: Command) -> ExitCode {
    match command {
        Command::List => run_list(),
        Command::Resume {
            id,
            last,
            print,
            model,
            json,
        } => config_error().unwrap_or_else(|| {
            run_resume(
                id.as_deref(),
                last,
                print.as_deref(),
                model.as_deref(),
                json,
            )
        }),
        Command::Fork { id, at } => run_fork(&id, &at),
        Command::Search { query } => run_search(&query),
        Command::Auth { action } => match action {
            AuthAction::Login { provider } => {
                let config = match kage_core::config::Config::load_default() {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("kage: config: {e}; custom providers unavailable for login");
                        kage_core::config::Config::default()
                    }
                };
                auth::run_login(provider.as_deref(), &config)
            }
            AuthAction::Logout { provider } => auth::run_logout(&provider),
            AuthAction::List => auth::run_list(),
        },
        Command::Init {
            force,
            non_interactive,
        } => init::run(force, non_interactive),
        Command::Doctor => doctor::run(),
        Command::GenManpage { out, check } => run_gen_manpage(&out, check),
        Command::Completions { shell } => run_completions(shell),
        Command::Rpc {
            model,
            system,
            system_file,
            yolo,
        } => config_error()
            .unwrap_or_else(|| rpc::run(model.as_deref(), &system, system_file.as_deref(), yolo)),
        Command::Serve {
            model,
            system,
            host,
            port,
            rotate_token,
            web_dir,
            fetch_web,
        } => config_error().unwrap_or_else(|| {
            serve::run(
                model.as_deref(),
                &system,
                &host,
                port,
                rotate_token,
                web_dir.as_deref(),
                fetch_web,
            )
        }),
        Command::Trust { revoke } => trust::run(revoke),
        Command::Models { action } => match action {
            ModelsAction::Refresh { source } => run_models_refresh(source.as_deref()),
            ModelsAction::Fetch {
                provider,
                base_url,
                kind,
                api_key,
                api_key_env,
                models_url,
                save,
            } => run_models_fetch(
                &provider,
                base_url.as_deref(),
                kind.as_deref(),
                api_key.as_deref(),
                api_key_env.as_deref(),
                models_url.as_deref(),
                save,
            ),
        },
        Command::Plugin { action } => plugin_cmd::run(action),
        Command::Mcp { action } => match action {
            McpAction::Serve { tools } => mcp::run_serve(&tools),
            McpAction::Login { server } => mcp_auth::run_login(&server),
            McpAction::Logout { server } => mcp_auth::run_logout(&server),
        },
    }
}

/// Implement `kage models refresh`: download the catalog into the model
/// cache. `source` overrides the published feed for this run only.
fn run_models_refresh(source: Option<&str>) -> ExitCode {
    let dest = match models_cache_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let url = source.map_or_else(kage_provider::catalog::source::catalog_url, str::to_owned);
    match kage_provider::catalog::refresh(&url, &dest) {
        Ok(count) => {
            println!("wrote {count} models to {}", dest.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kage: models refresh: {e}");
            ExitCode::from(1)
        }
    }
}

/// Implement `kage models fetch`: list the models a provider serves.
/// The user config and saved keys fill what the flags leave out, the
/// same way the settings screen's connection test resolves a provider.
/// With `--save`, the fetched models are merged into the saved custom
/// provider's entry, the way the form's Fetch fills its models table
/// before Save.
fn run_models_fetch(
    provider: &str,
    base_url: Option<&str>,
    kind: Option<&str>,
    api_key: Option<&str>,
    api_key_env: Option<&str>,
    models_url: Option<&str>,
    save: bool,
) -> ExitCode {
    let config = match kage_core::config::Config::load_default_raw() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("kage: models fetch: {e}");
            return ExitCode::from(1);
        }
    };
    let store = crate::auth::AuthStore::load().unwrap_or_else(|_| crate::auth::AuthStore::empty());
    let probe = kage_acp::acp::ProviderProbe {
        id: provider.to_owned(),
        kind: kind
            .filter(|kind| !kind.trim().is_empty())
            .map(str::to_owned),
        base_url: base_url
            .filter(|base| !base.trim().is_empty())
            .map(str::to_owned),
        api_key_env: api_key_env
            .filter(|env| !env.trim().is_empty())
            .map(str::to_owned),
        api_key: api_key.filter(|key| !key.is_empty()).map(str::to_owned),
        models_url: models_url
            .filter(|url| !url.trim().is_empty())
            .map(str::to_owned),
        ..kage_acp::acp::ProviderProbe::default()
    };
    let result = crate::rpc::probe::probe(&probe, &config, &store);
    println!("{}", result.message);
    if !result.ok {
        return ExitCode::from(1);
    }
    for model in &result.models {
        let name = match &model.name {
            Some(name) if !name.is_empty() && name != &model.id => format!(" ({name})"),
            _ => String::new(),
        };
        let context = model
            .context
            .map_or_else(|| "?".to_owned(), |n| n.to_string());
        let out = model
            .max_output
            .map_or_else(|| "?".to_owned(), |n| n.to_string());
        let input = model.input.join(",");
        let input = if input.is_empty() {
            String::new()
        } else {
            format!("  in {input}")
        };
        println!("{}{name}  ctx {context}  out {out}{input}", model.id);
    }
    if save {
        let saved = kage_core::config::Config::default_path().map_or_else(
            || Err("no home directory for the config".to_owned()),
            |path| save_fetched_models(&path, provider, &result.models),
        );
        match saved {
            Ok((0, listed)) => {
                println!("nothing to save: all {listed} fetched models are already listed");
            }
            Ok((added, listed)) => {
                println!(
                    "saved {added} new models to [providers.custom.{provider}] ({listed} already listed)"
                );
            }
            Err(e) => {
                eprintln!("kage: models fetch: {e}");
                return ExitCode::from(1);
            }
        }
    }
    ExitCode::SUCCESS
}

/// Merges the fetched models into the saved custom provider's entry:
/// new ids are appended, listed ones stay exactly as written. The
/// config's own editor merges the `models` array positionally, so the
/// comments inside listed tables survive. Returns the ids added and
/// the count already listed.
fn save_fetched_models(
    path: &Path,
    provider: &str,
    fetched: &[kage_acp::acp::ProbeModel],
) -> Result<(usize, usize), String> {
    use kage_core::modality::Modality;
    use kage_core::thinking::Effort;
    use serde_json::{Value, json};
    let keys = ["providers", "custom", provider];
    let mut entry = kage_core::config_edit::current(path, &keys)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "--save edits [providers.custom.{provider}], and the config lists no such \
                 custom provider; registered providers keep their models in the catalog"
            )
        })?;
    let listed = entry
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let held: Vec<String> = listed
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let fresh: Vec<Value> = fetched
        .iter()
        .filter(|model| !held.iter().any(|id| id == &model.id))
        .map(|model| {
            let mut row = json!({
                "id": model.id,
                "name": model.name.clone().unwrap_or_else(|| model.id.clone()),
            });
            if let Some(context) = model.context {
                row["context"] = json!(context);
            }
            if let Some(out) = model.max_output.and_then(|out| u32::try_from(out).ok()) {
                row["max_output"] = json!(out);
            }
            // Only values the config's own types re-parse make it in;
            // an endpoint naming an unknown kind would fail the next
            // load.
            let input: Vec<&str> = model
                .input
                .iter()
                .map(String::as_str)
                .filter(|kind| Modality::parse(kind).is_some())
                .collect();
            if !input.is_empty() {
                row["input"] = json!(input);
            }
            let produces: Vec<&str> = model
                .produces
                .iter()
                .map(String::as_str)
                .filter(|kind| Modality::parse(kind).is_some())
                .collect();
            if produces.iter().any(|kind| *kind != "text") {
                row["produces"] = json!(produces);
            }
            let efforts: Vec<&str> = model
                .efforts
                .iter()
                .map(String::as_str)
                .filter(|level| Effort::parse(level).is_some())
                .collect();
            if !efforts.is_empty() {
                row["efforts"] = json!(efforts);
            }
            if let Some(reasoning) = model.reasoning {
                row["reasoning"] = json!(reasoning);
            }
            row
        })
        .collect();
    let added = fresh.len();
    if added == 0 {
        return Ok((0, listed.len()));
    }
    let mut merged = listed;
    merged.extend(fresh);
    entry["models"] = Value::Array(merged);
    let text =
        kage_core::config_edit::edited(path, &keys, Some(&entry)).map_err(|e| e.to_string())?;
    kage_core::fsutil::atomic_write(path, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok((added, held.len()))
}

/// Print a shell completion script for `kage` to stdout. The script
/// is generated fresh from the clap definition every invocation, so
/// adding or renaming a subcommand requires no extra checked-in
/// artifacts.
pub(crate) fn run_completions(shell: clap_complete::Shell) -> ExitCode {
    use clap::CommandFactory as _;
    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_owned();
    let mut stdout = io::stdout().lock();
    clap_complete::generate(shell, &mut cmd, bin_name, &mut stdout);
    ExitCode::SUCCESS
}

/// The CLI definition the man page renders. The page names the release
/// alone: a commit would make the committed page stale on every commit.
fn manpage_command() -> clap::Command {
    use clap::CommandFactory as _;
    Cli::command().version(env!("CARGO_PKG_VERSION"))
}

/// Render the manpage via `clap_mangen` and write it to `out`, or with
/// `check`, compare the rendered page against the file at `out` and
/// fail when they differ. Creates the parent directory when missing so
/// a fresh checkout can run `kage gen-manpage --out man/kage.1`
/// without a prior `mkdir`.
pub(crate) fn run_gen_manpage(out: &std::path::Path, check: bool) -> ExitCode {
    let buffer = match render_manpage(&manpage_command()) {
        Ok(buffer) => buffer,
        Err(err) => {
            eprintln!("kage: render manpage: {err}");
            return ExitCode::from(1);
        }
    };
    if check {
        return match std::fs::read_to_string(out) {
            Ok(stored) if stored == buffer => {
                eprintln!("kage: gen-manpage --check: {} is current", out.display());
                ExitCode::SUCCESS
            }
            Ok(_) => {
                eprintln!(
                    "kage: gen-manpage --check: {} is stale; run `kage gen-manpage --out {}` and commit the result",
                    out.display(),
                    out.display()
                );
                ExitCode::from(1)
            }
            Err(err) => {
                eprintln!(
                    "kage: gen-manpage --check: cannot read {}: {err}; run `kage gen-manpage --out {}` and commit the result",
                    out.display(),
                    out.display()
                );
                ExitCode::from(1)
            }
        };
    }
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!("kage: mkdir {}: {err}", parent.display());
        return ExitCode::from(1);
    }
    if let Err(err) = std::fs::write(out, &buffer) {
        eprintln!("kage: write {}: {err}", out.display());
        return ExitCode::from(1);
    }
    eprintln!("kage: wrote {}", out.display());
    ExitCode::SUCCESS
}

/// Paths listed in the manpage's FILES section, with what each holds.
const MANPAGE_FILES: &[(&str, &str)] = &[
    (
        "~/.config/kage/config.toml",
        "User config, written by kage init.",
    ),
    (
        "~/.config/kage/init.lua",
        "Lua config the TUI runs after config.toml.",
    ),
    (
        "<workdir>/.kage/config.toml",
        "Project config, merged over the user config. Its mcp, permissions and plugins.capabilities tables apply only after kage trust.",
    ),
    (
        "~/.config/kage/{plugins,themes,agents,skills,templates}/",
        "Lua plugins, user themes, agent definitions, skills and prompt templates.",
    ),
    (
        "~/.local/share/kage/sessions/",
        "Recorded sessions, one JSONL file each.",
    ),
    (
        "~/.local/share/kage/auth.json",
        "Saved provider credentials (mode 0600).",
    ),
    (
        "~/.local/state/kage/",
        "Session state, input history and trusted projects (trust.json).",
    ),
    (
        "~/.cache/kage/models.json",
        "Model catalog written by kage models refresh.",
    ),
];

/// Render the manpage for `cmd`. The NAME, SYNOPSIS, OPTIONS and
/// VERSION sections come from `clap_mangen`; DESCRIPTION, COMMANDS and
/// FILES are written here so the page describes the TUI, lists the
/// subcommands as `kage <command>` rather than as separate pages that
/// are not installed, and names the files kage reads and writes.
fn render_manpage(cmd: &clap::Command) -> io::Result<String> {
    use clap_mangen::Man;
    use clap_mangen::roff::{Roff, bold, italic, roman};

    type Section = fn(&Man, &mut dyn Write) -> io::Result<()>;
    let mut built = cmd.clone();
    built.build();
    let cmd = &built;
    let man = Man::new(cmd.clone());
    let preamble = Roff::new().render();
    let mut page = preamble.clone();
    let mut push = |section: String| {
        page.push_str(section.strip_prefix(&preamble).unwrap_or(&section));
    };
    let render = |section: Section| -> io::Result<String> {
        let mut buffer = Vec::new();
        section(&man, &mut buffer)?;
        Ok(String::from_utf8_lossy(&buffer).into_owned())
    };
    push(render(Man::render_title)?);
    push(render(Man::render_name_section)?);
    push(render(Man::render_synopsis_section)?);

    let mut roff = Roff::new();
    roff.control("SH", ["DESCRIPTION"]);
    roff.text([
        roman("With no command and no "),
        bold("-p"),
        roman(", kage opens its interactive TUI in the current directory. Type a prompt and press enter to send it. On an empty prompt, "),
        bold("?"),
        roman(" shows the keys and a prompt starting with "),
        bold("/"),
        roman(" runs a command. "),
        bold("ctrl+q"),
        roman(" quits."),
    ]);
    roff.control("PP", []);
    roff.text([
        roman("With "),
        bold("-p"),
        roman(
            ", kage runs one prompt through the agent loop, streams the reply to stdout and exits.",
        ),
    ]);
    push(roff.render());
    push(render(Man::render_options_section)?);

    let mut roff = Roff::new();
    roff.control("SH", ["COMMANDS"]);
    let listed = |c: &&clap::Command| !c.is_hide_set() && c.get_name() != "help";
    for sub in cmd.get_subcommands().filter(listed) {
        let nested = sub
            .get_subcommands()
            .filter(listed)
            .map(|n| (format!("{} {}", sub.get_name(), n.get_name()), n));
        for (name, page) in std::iter::once((sub.get_name().to_owned(), sub)).chain(nested) {
            roff.control("TP", []);
            roff.text([bold(format!("kage {name}"))]);
            if let Some(about) = page.get_about() {
                roff.text([roman(about.to_string())]);
            }
            // The option blocks belong to this command's `.TP` inside
            // COMMANDS; pushing them to the page here would land them
            // in the still-open OPTIONS section above.
            for (tag, help) in subcommand_args(page) {
                roff.control("TP", []);
                roff.text(tag);
                if !help.is_empty() {
                    roff.text(help);
                }
            }
        }
    }
    roff.control("PP", []);
    roff.text([
        roman("Run "),
        bold("kage "),
        italic("command"),
        bold(" --help"),
        roman(" for the options of a command."),
    ]);

    roff.control("SH", ["FILES"]);
    for (path, what) in MANPAGE_FILES {
        roff.control("TP", []);
        roff.text([italic(*path)]);
        roff.text([roman(*what)]);
    }
    roff.control("PP", []);
    roff.text([roman(
        "XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME and XDG_CACHE_HOME replace the ~/.config, ~/.local/share, ~/.local/state and ~/.cache roots.",
    )]);
    push(roff.render());
    push(render(Man::render_version_section)?);
    Ok(page)
}

/// Option and positional lines for one command in the COMMANDS
/// section, styled like the OPTIONS section: bold flags, italic
/// `<VALUE>`, roman help. Each entry is a (`tag`, `help`) pair emitted
/// as one `.TP` block. Hidden arguments are skipped;
/// `--help`/`--version` are documented once in OPTIONS.
fn subcommand_args(
    cmd: &clap::Command,
) -> Vec<(
    Vec<clap_mangen::roff::Inline>,
    Vec<clap_mangen::roff::Inline>,
)> {
    use clap_mangen::roff::{bold, italic, roman};
    let mut blocks = Vec::new();
    for arg in cmd.get_arguments() {
        if arg.is_hide_set() || matches!(arg.get_id().as_str(), "help" | "version") {
            continue;
        }
        let mut tag = Vec::new();
        if arg.is_positional() {
            let name = arg
                .get_value_names()
                .map_or_else(|| arg.get_id().as_str().to_owned(), |names| names.join(" "));
            tag.push(italic(name));
        } else {
            match (arg.get_short(), arg.get_long()) {
                (Some(short), Some(long)) => {
                    tag.push(bold(format!("-{short}")));
                    tag.push(roman(", "));
                    tag.push(bold(format!("--{long}")));
                }
                (Some(short), None) => tag.push(bold(format!("-{short}"))),
                (None, Some(long)) => tag.push(bold(format!("--{long}"))),
                (None, None) => continue,
            }
            let takes_values = arg.get_num_args().is_some_and(|range| range.takes_values());
            if takes_values && let Some(names) = arg.get_value_names() {
                for name in names {
                    tag.push(italic(format!(" <{name}>")));
                }
            }
        }
        let help = arg
            .get_help()
            .or_else(|| arg.get_long_help())
            .map(|help| vec![roman(help.to_string())])
            .unwrap_or_default();
        blocks.push((tag, help));
    }
    blocks
}

/// `-p/--print` belongs to print mode, which only runs without a
/// subcommand; a `-p` before a subcommand is always a usage error.
fn subcommand_print_conflict(cli: &Cli) -> Option<ExitCode> {
    (cli.command.is_some() && cli.print.is_some()).then(|| {
        eprintln!(
            "kage: -p/--print cannot be combined with a subcommand; \
             pass it after the subcommand (e.g. `kage resume -p ...`) or drop it"
        );
        ExitCode::from(2)
    })
}

fn main() -> ExitCode {
    kage_core::build::set_version(env!("KAGE_VERSION"));
    let cli = Cli::parse();

    if let Some(code) = subcommand_print_conflict(&cli) {
        return code;
    }
    if let Ok(path) = models_cache_path() {
        // A missing cache just means `models refresh` has not run
        // yet; the snapshot serves. One that exists but does not
        // load is drift the user should hear about.
        if path.exists() {
            match kage_provider::catalog::use_cache(&path) {
                Ok(count) => eprintln!("kage: model cache loaded ({count} models)"),
                Err(reason) => {
                    eprintln!("kage: ignoring model cache {}: {reason}", path.display());
                }
            }
        }
    }

    if let Some(command) = cli.command {
        return run_subcommand(command);
    }
    if let Some(code) = config_error() {
        return code;
    }

    if cli.print.is_some() {
        return run_print_mode(cli);
    }

    // No subcommand and no `-p`: drop into the interactive TUI.
    tui::run_tui(cli.model.as_deref(), &cli.system, None, cli.yolo)
}

/// Load the layered config for the current directory before a run
/// starts. A config that does not load stops kage with the error and
/// the returned exit status, because running on defaults would drop
/// custom providers and could start the first-run wizard.
fn config_error() -> Option<ExitCode> {
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let err = kage_core::config::Config::load_layered(&workdir).err()?;
    eprintln!("kage: {err}");
    Some(ExitCode::from(1))
}

/// Open the print-mode session recorder, unless `no_session`. A failed
/// open is an error for the caller to turn into an exit status:
/// an unrecorded `-p` turn loses its transcript.
fn open_session_writer(
    no_session: bool,
    model: &str,
    system_prompt: &str,
) -> Result<Option<kage_session::SessionWriter>, String> {
    if no_session {
        return Ok(None);
    }
    match open_session(model, system_prompt) {
        Ok(w) => {
            eprintln!("kage: recording session to {}", w.path().display());
            Ok(Some(w))
        }
        Err(e) => Err(format!("failed to open session file: {e}")),
    }
}

/// The configured shell program for `workdir` (`[shell] program`),
/// when the layered config loads.
fn configured_shell(workdir: &std::path::Path) -> Option<String> {
    kage_core::config::Config::load_layered(workdir)
        .ok()
        .and_then(|c| c.shell.program)
}

/// One `-p` print-mode run: provider and tool setup, permission gate,
/// session recording, and the exit code.
fn run_print_mode(cli: Cli) -> ExitCode {
    let Some(prompt) = cli.print else {
        return tui::run_tui(cli.model.as_deref(), &cli.system, None, cli.yolo);
    };
    let mut registry = match build_provider_registry() {
        Ok(registry) => registry,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };

    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let shell = configured_shell(&workdir);
    let provisional_model = cli
        .model
        .clone()
        .unwrap_or_else(|| default_model(&registry));
    let bare_prompt = runtime_env::build_system_prompt(
        &cli.system,
        &workdir,
        &provisional_model,
        &[],
        shell.as_deref(),
    );
    let plugin_runtime = match plugins_dir() {
        Ok(dir) => match setup_runtime(&dir, &workdir, &provisional_model, &bare_prompt) {
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

    let model = cli.model.unwrap_or_else(|| default_model(&registry));
    if !has_usable_provider(&registry) && registry.resolve(&model).is_err() {
        eprintln!("{NO_CREDENTIALS_MESSAGE}");
        return ExitCode::from(1);
    }
    if model.is_empty() {
        eprintln!("{NO_MODEL_MESSAGE}");
        return ExitCode::from(1);
    }
    let resolved = match registry.resolve(&model) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kage: cannot resolve model {model}: {e}");
            return ExitCode::from(1);
        }
    };
    let skills = load_skills(&workdir, plugin_runtime.as_deref());
    let system_prompt =
        runtime_env::build_system_prompt(&cli.system, &workdir, &model, &skills, shell.as_deref());

    let layered = kage_core::config::Config::load_layered(&workdir).unwrap_or_default();
    let mut tools = builtin_registry().with_renames(&layered.tools.rename);
    let (mcp_manager, mcp_errors) =
        mcp::spawn_and_register(&mut tools, &workdir, plugin_runtime.as_deref());
    for (server, err) in mcp_errors {
        eprintln!("kage: mcp `{server}`: {err}");
    }
    let mut cx = AgentContext::new(resolved.model.clone(), &system_prompt).with_workdir(&workdir);
    if let Some(window) = runtime_env::context_window_for(&registry, &model) {
        cx = cx.with_context_window(window);
    }
    if let Some(out) = runtime_env::max_output_tokens_for(&registry, &model) {
        cx = cx.with_max_output_tokens(out);
    }
    let writer = match open_session_writer(cli.no_session, &model, &system_prompt) {
        Ok(writer) => writer,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };

    let exit = execute_print_run(
        Arc::new(registry),
        &model,
        tools,
        cx,
        prompt,
        writer,
        plugin_runtime,
        Some(mcp_manager),
        cli.json,
        cli.yolo,
    );
    if let Err(err) = state::record_last_model(&model) {
        eprintln!("kage: {err}");
    }
    exit
}

mod cli_loop_run;
mod cli_printing;
mod cli_query;
mod sigint;

pub(crate) use cli_loop_run::execute_print_run;
pub(crate) use cli_query::{run_fork, run_resume, run_search};

/// Discover and load every SKILL.md under the user config dir
/// (`$XDG_CONFIG_HOME/kage/skills/<name>/`), the project-local
/// `./.kage/skills/<name>/` and `./.agents/skills/<name>/`, and any
/// directory contributed by a plugin's `resources_discover` handler.
/// Later entries shadow earlier ones with the same skill name. Failing
/// skills are logged to stderr and skipped.
pub(crate) fn load_skills(
    workdir: &std::path::Path,
    plugin_runtime: Option<&kage_plugin::PluginRuntime>,
) -> Vec<kage_core::Skill> {
    let mut out: std::collections::BTreeMap<String, kage_core::Skill> =
        std::collections::BTreeMap::new();
    let mut search: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(p) = config_dir() {
        search.push(p.join("skills"));
    }
    // Project skills are instructions headed for the system prompt, so
    // they wait for the same first-run trust decision as project agents.
    if kage_core::trust::project_extensions_trusted(workdir) {
        search.push(workdir.join(".kage").join("skills"));
        search.push(workdir.join(".agents").join("skills"));
    }
    if let Some(rt) = plugin_runtime {
        match rt.discover_resources() {
            Ok(entries) => search.extend(entries.skills),
            Err(err) => eprintln!("kage: resources_discover dispatch failed: {err}"),
        }
    }
    for dir in &search {
        for result in kage_core::load_skills_dir(dir) {
            match result {
                Ok(skill) => {
                    out.insert(skill.name.clone(), skill);
                }
                Err(err) => eprintln!("kage: skill load error: {err}"),
            }
        }
    }
    out.into_values().collect()
}

/// Implement `kage list`: print one row per recorded session.
pub(crate) fn run_list() -> ExitCode {
    let dir = match sessions_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let mut summaries = match kage_session::list(&dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kage: failed to list sessions: {e}");
            return ExitCode::from(1);
        }
    };
    summaries.retain(|s| s.agent.is_none());
    if summaries.is_empty() {
        eprintln!("kage: no sessions found in {}", dir.display());
        return ExitCode::SUCCESS;
    }
    print_session_table(&mut io::stdout().lock(), &summaries);
    ExitCode::SUCCESS
}

/// Print one row per session: short id, local creation time, model and
/// the title, or the last prompt for a session without one.
pub(crate) fn print_session_table<W: Write>(out: &mut W, summaries: &[SessionSummary]) {
    let id_h = "ID";
    let created_h = "CREATED";
    let model_h = "MODEL";
    let title_h = "TITLE";
    let _ = writeln!(out, "{id_h:<8}  {created_h:<16}  {model_h:<32}  {title_h}");
    for s in summaries {
        let id = s.id.to_string();
        // Eight chars, matching the prefixes engine notices and the
        // TUI show, so the id can be pasted where those are expected.
        let id_short: String = id.chars().take(8).collect();
        let created = s
            .created_at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        let model = &s.model;
        let title = s
            .title
            .as_deref()
            .or(s.last_user_prompt.as_deref())
            .map_or_else(
                || "(untitled session)".to_owned(),
                |text| truncate_one_line(text, 60),
            );
        let _ = writeln!(out, "{id_short:<8}  {created:<16}  {model:<32}  {title}");
    }
}

pub(crate) fn truncate_one_line(text: &str, max: usize) -> String {
    let single_line = text.lines().next().unwrap_or("").trim();
    if single_line.chars().count() <= max {
        return single_line.to_owned();
    }
    let head: String = single_line.chars().take(max - 3).collect();
    format!("{head}...")
}

/// Create a fresh session file under [`sessions_dir`].
pub(crate) fn open_session(model: &str, system_prompt: &str) -> Result<SessionWriter, String> {
    let (path, header) = plan_session(model, system_prompt)?;
    SessionWriter::create(path, header).map_err(|e| e.to_string())
}

/// Plan a fresh session: build the path and header without touching
/// the filesystem. The TUI uses this to defer file creation until the
/// first real prompt actually lands, so launching the TUI and
/// quitting (or resuming a different session) doesn't litter the
/// sessions directory with empty header-only stubs.
pub(crate) fn plan_session(model: &str, system_prompt: &str) -> Result<(PathBuf, Header), String> {
    Ok(plan_session_in(&sessions_dir()?, model, system_prompt))
}

/// [`plan_session`] in the sessions directory `dir`.
pub(crate) fn plan_session_in(dir: &Path, model: &str, system_prompt: &str) -> (PathBuf, Header) {
    let session = SessionId::new();
    let path = build_session_path(dir, session);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let header = Header {
        version: FORMAT_VERSION,
        session,
        id: EntryId::new(),
        ts: Utc::now(),
        cwd,
        model: model.to_owned(),
        system_prompt: system_prompt.to_owned(),
        parent_session: None,
        parent_entry: None,
    };
    (path, header)
}

/// A `--model` value in the `provider/model` form, an older
/// `provider/model` value rewritten.
#[expect(
    clippy::unnecessary_wraps,
    reason = "clap value parsers return a Result"
)]
fn model_arg(value: &str) -> Result<String, String> {
    Ok(kage_core::canonical_model(value))
}

pub(crate) fn build_session_path(dir: &std::path::Path, session: SessionId) -> PathBuf {
    dir.join(format!("{session}.jsonl"))
}

#[cfg(test)]
#[expect(
    clippy::result_large_err,
    reason = "figment::Jail closures must return figment::Error"
)]
mod tests {
    use super::*;

    fn summary(title: Option<&str>, prompt: Option<&str>) -> SessionSummary {
        let created_at = Utc::now();
        SessionSummary {
            id: SessionId::new(),
            path: PathBuf::from("/s.jsonl"),
            created_at,
            updated_at: created_at,
            cwd: PathBuf::from("/p"),
            model: "mock:m".to_owned(),
            parent_session: None,
            last_user_prompt: prompt.map(str::to_owned),
            title: title.map(str::to_owned),
            agent: None,
        }
    }

    #[test]
    fn session_table_shows_local_time_and_prefers_the_title() {
        let rows = [
            summary(Some("Fix the parser"), Some("last prompt")),
            summary(None, Some("only a prompt\nsecond line")),
            summary(None, None),
        ];
        let mut out = Vec::new();
        print_session_table(&mut out, &rows);
        let text = String::from_utf8(out).expect("utf-8");
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].ends_with("TITLE"), "{text}");
        let id = rows[0].id.to_string();
        let prefix8: String = id.chars().take(8).collect();
        assert!(
            lines[1].starts_with(&format!("{prefix8}  ")),
            "the id column shows the 8-char prefix the engine notices and the TUI use: {text}"
        );
        let prefix10: String = id.chars().take(10).collect();
        assert!(!lines[1].contains(&prefix10), "{text}");
        let local = rows[0]
            .created_at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        assert!(lines[1].contains(&local), "{text}");
        assert!(lines[1].ends_with("Fix the parser"), "{text}");
        assert!(lines[2].ends_with("only a prompt"), "{text}");
        assert!(lines[3].ends_with("(untitled session)"), "{text}");
    }

    #[test]
    fn manpage_lists_commands_inline_and_has_files() {
        use clap::CommandFactory as _;
        let page = render_manpage(&Cli::command()).unwrap();
        assert_eq!(page.matches(".ds Aq").count(), 2, "one preamble");
        assert!(!page.contains("kage \\- kage"), "NAME repeats the name");
        assert!(!page.contains("(1)"), "no references to missing pages");
        assert!(!page.contains("For example:"));
        assert!(page.contains("\\fBkage auth list\\fR"));
        assert!(page.contains(".SH FILES"));
    }

    #[test]
    fn manpage_lists_per_command_options_and_positionals() {
        use clap::CommandFactory as _;
        let page = render_manpage(&Cli::command()).unwrap();
        assert!(
            page.contains("\\fB\\-\\-non\\-interactive\\fR"),
            "per-command options missing"
        );
        assert!(
            page.contains("\\fISESSION\\fR") || page.contains("\\fIID\\fR"),
            "positional arguments missing"
        );
    }

    /// Subcommand-only flags must sit after `.SH COMMANDS`: roff
    /// sections extend to the next `.SH`, so a block pushed earlier
    /// documents a subcommand flag as a top-level one.
    #[test]
    fn manpage_keeps_subcommand_options_out_of_the_options_section() {
        use clap::CommandFactory as _;
        let page = render_manpage(&Cli::command()).unwrap();
        let commands = page.find(".SH COMMANDS").expect("a COMMANDS section");
        for flag in ["\\-\\-last", "\\-\\-port", "\\-\\-tools", "\\-\\-revoke"] {
            assert!(
                !page[..commands].contains(flag),
                "{flag} is documented before .SH COMMANDS"
            );
        }
    }

    #[test]
    fn gen_manpage_check_fails_on_drift_and_passes_on_current() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("kage.1");
        assert_eq!(run_gen_manpage(&out, true), ExitCode::from(1), "missing");
        std::fs::write(&out, "stale\n").unwrap();
        assert_eq!(run_gen_manpage(&out, true), ExitCode::from(1), "stale");
        let page = render_manpage(&manpage_command()).unwrap();
        std::fs::write(&out, &page).unwrap();
        assert_eq!(run_gen_manpage(&out, true), ExitCode::SUCCESS, "current");
    }

    #[test]
    fn project_skills_wait_for_trust_then_dot_agents_shadows_dot_kage() {
        figment::Jail::expect_with(|jail| {
            let root = jail.directory().to_path_buf();
            for (key, dir) in [
                ("HOME", "home"),
                ("XDG_CONFIG_HOME", "config"),
                ("XDG_STATE_HOME", "state"),
            ] {
                jail.set_env(key, root.join(dir).to_string_lossy().as_ref());
            }
            let project = root.join("project");
            let kage_skills = project.join(".kage").join("skills").join("lint");
            let agents_skills = project.join(".agents").join("skills").join("lint");
            for dir in [&kage_skills, &agents_skills] {
                std::fs::create_dir_all(dir).unwrap();
            }
            std::fs::write(
                kage_skills.join("SKILL.md"),
                "---\ndescription: Kage lint\n---\nKage body.",
            )
            .unwrap();
            std::fs::write(
                agents_skills.join("SKILL.md"),
                "---\ndescription: Shared lint\n---\nShared body.",
            )
            .unwrap();

            // Cloned project: skill instructions stay out of the prompt
            // until the first-run trust decision.
            assert!(load_skills(&project, None).is_empty());
            let presented =
                kage_core::trust::untrusted_project(&project).expect("shared skill asks for trust");
            kage_core::trust::trust_project(&project, &presented).unwrap();

            let skills = load_skills(&project, None);
            let lint = skills
                .iter()
                .find(|s| s.name == "lint")
                .expect("lint skill");
            assert_eq!(lint.body, "Shared body.");
            assert_eq!(lint.description, "Shared lint");
            Ok(())
        });
    }

    #[test]
    fn save_fetched_models_appends_new_ids_and_keeps_comments() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[providers.custom.lab]
base_url = "https://lab.example.com/v1"
kind = "anthropic"

[[providers.custom.lab.models]]
id = "held"
name = "Held"
# a kept note
efforts = ["max"]
"#,
        )
        .unwrap();
        let fetched = [
            kage_acp::acp::ProbeModel {
                id: "held".into(),
                ..kage_acp::acp::ProbeModel::default()
            },
            kage_acp::acp::ProbeModel {
                id: "fresh".into(),
                name: Some("Fresh".into()),
                context: Some(1024),
                max_output: Some(64),
                reasoning: Some(true),
                input: vec!["text".into(), "image".into(), "hologram".into()],
                produces: vec!["text".into(), "image".into()],
                efforts: vec!["low".into(), "xhigh".into(), "madeup".into()],
            },
        ];
        let (added, listed) = save_fetched_models(&path, "lab", &fetched).unwrap();
        assert_eq!((added, listed), (1, 1));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# a kept note"), "{text}");
        assert!(text.contains("id = \"held\""), "{text}");
        assert!(text.contains("id = \"fresh\""), "{text}");
        let row = &text[text.find("id = \"fresh\"").unwrap()..];
        let at = |needle: &str| row.find(needle).unwrap();
        assert!(at("name = \"Fresh\"") < at("context = 1024"), "{row}");
        assert!(at("context = 1024") < at("max_output = 64"), "{row}");
        assert!(at("max_output = 64") < at("input = ["), "{row}");
        assert!(at("input = [") < at("efforts = ["), "{row}");
        assert!(at("efforts = [") < at("reasoning = true"), "{row}");
        assert!(text.contains("input = [\"text\", \"image\"]"), "{text}");
        assert!(text.contains("produces = [\"text\", \"image\"]"), "{text}");
        assert!(text.contains("efforts = [\"low\", \"xhigh\"]"), "{text}");
        assert!(!text.contains("hologram"), "{text}");
        assert!(!text.contains("madeup"), "{text}");
    }

    #[test]
    fn save_fetched_models_writes_nothing_when_all_ids_are_listed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[providers.custom.lab]
base_url = "https://lab.example.com/v1"

[[providers.custom.lab.models]]
id = "held"
name = "Held"
"#,
        )
        .unwrap();
        let fetched = [kage_acp::acp::ProbeModel {
            id: "held".into(),
            ..kage_acp::acp::ProbeModel::default()
        }];
        let before = std::fs::read_to_string(&path).unwrap();
        let (added, listed) = save_fetched_models(&path, "lab", &fetched).unwrap();
        assert_eq!((added, listed), (0, 1));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn save_fetched_models_refuses_a_provider_the_config_lacks() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[providers.custom.other]\nbase_url = \"https://x\"\nmodels = []\n",
        )
        .unwrap();
        let err = save_fetched_models(&path, "lab", &[]).unwrap_err();
        assert!(err.contains("[providers.custom.lab]"), "{err}");
    }
}
