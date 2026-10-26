//! `_kage/config/test`: asks a provider for its model list, so a
//! settings form can test a connection and fill its model table through
//! the engine's network access instead of the client's, which a browser
//! does not have.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use kage_acp::acp::{
    ConfigTestResult, KeySource, ProbeModel, ProbeTool, ProviderKey, ProviderProbe,
};
use kage_core::config::{Config, CustomProviderKind};
use kage_core::thinking::Reasoning;
use kage_provider::catalog;
use kage_provider::compat::COMPAT_PROVIDERS;
use serde_json::Value;

use super::REDACTED;
use crate::auth::{self, AuthStore};
use crate::chatgpt;

/// How long the model list may take.
const TIMEOUT: Duration = Duration::from_secs(15);

/// The wire protocol a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    OpenAi,
    Anthropic,
    Gemini,
}

/// Where and how to ask for the model list.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    kind: Kind,
    base: String,
    models_url: Option<String>,
    key: String,
    headers: BTreeMap<String, String>,
}

/// Asks the provider `probe` describes for its models, with `config`
/// and `store` filling what the probe leaves out.
pub(crate) fn probe(probe: &ProviderProbe, config: &Config, store: &AuthStore) -> ConfigTestResult {
    let target = match resolve(probe, config, store) {
        Ok(target) => target,
        Err(message) => {
            return ConfigTestResult {
                message,
                ..ConfigTestResult::default()
            };
        }
    };
    list(&probe.id, &target)
}

/// The target `probe` names: its own fields, then the saved entry for
/// its id, then the provider's defaults. `openai` with no key typed or
/// exported and a `ChatGPT` login stored lists the backend's
/// models with that login, the way the registry would route it.
fn resolve(probe: &ProviderProbe, config: &Config, store: &AuthStore) -> Result<Target, String> {
    let id = probe.id.as_str();
    if id.is_empty() {
        return Err("name the provider to test".to_owned());
    }
    let typed = probe.api_key.as_deref().filter(|key| !key.is_empty());
    let env = probe
        .api_key_env
        .clone()
        .unwrap_or_else(|| key_env(id, config));
    let exported = (!env.is_empty())
        .then(|| std::env::var(&env).ok())
        .flatten()
        .filter(|value| !value.is_empty());
    let chatgpt = match (id, typed, &exported) {
        ("openai", None, None) => chatgpt_login(store),
        _ => None,
    };
    let custom = config.providers.custom.get(id);
    let builtin = config.providers.overrides.get(id);
    let compat = COMPAT_PROVIDERS.iter().find(|entry| entry.id == id);
    let kind = match probe.kind.as_deref() {
        Some("openai") => Kind::OpenAi,
        Some("anthropic") => Kind::Anthropic,
        Some("gemini") => Kind::Gemini,
        Some(other) => return Err(format!("unknown protocol `{other}`")),
        None => match custom.map(|custom| custom.kind) {
            Some(CustomProviderKind::Anthropic) => Kind::Anthropic,
            Some(CustomProviderKind::Gemini) => Kind::Gemini,
            None if id == "anthropic" => Kind::Anthropic,
            None if id == "google" => Kind::Gemini,
            Some(CustomProviderKind::OpenAi) | None => Kind::OpenAi,
        },
    };
    let default_base = match id {
        _ if chatgpt.is_some() => Some(chatgpt::BACKEND_BASE_URL),
        "anthropic" => Some("https://api.anthropic.com"),
        "google" => Some("https://generativelanguage.googleapis.com"),
        "openai" => Some("https://api.openai.com/v1"),
        _ => compat.map(|entry| entry.base_url),
    };
    let base = probe
        .base_url
        .clone()
        .filter(|base| !base.trim().is_empty())
        .or_else(|| custom.map(|custom| custom.base_url.clone()))
        .or_else(|| builtin.and_then(|o| o.base_url.clone()))
        .or_else(|| default_base.map(str::to_owned))
        .ok_or_else(|| "set a base URL".to_owned())?;
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(format!("{base} is not an http(s) URL"));
    }
    let key = if let Some(key) = typed {
        key.to_owned()
    } else if let Some(login) = &chatgpt {
        login.access_token.clone()
    } else if env.is_empty() {
        String::new()
    } else {
        exported.map_or_else(|| saved_key(id, &env, store), Ok)?
    };
    let saved = custom
        .map(|custom| &custom.headers)
        .or_else(|| builtin.map(|o| &o.headers));
    let mut headers = chatgpt
        .as_ref()
        .map(|login| chatgpt::backend_headers(login.account_id.as_deref()))
        .unwrap_or_default();
    headers.extend(probe.headers.iter().filter_map(|(name, value)| {
        if value == REDACTED {
            saved?.get(name).map(|value| (name.clone(), value.clone()))
        } else {
            Some((name.clone(), value.clone()))
        }
    }));
    let saved_models_url = custom
        .map(|custom| custom.models_url.clone())
        .unwrap_or_default();
    let models_url = probe
        .models_url
        .clone()
        .filter(|url| !url.trim().is_empty())
        .or(saved_models_url)
        .filter(|url| !url.trim().is_empty());
    if let Some(url) = &models_url
        && !(url.starts_with("http://") || url.starts_with("https://"))
    {
        return Err(format!("{url} is not an http(s) URL"));
    }
    let base = base.trim_end_matches('/').to_owned();
    let models_url = models_url.or_else(|| chatgpt.is_some().then(|| chatgpt::models_url(&base)));
    Ok(Target {
        kind,
        base,
        models_url,
        key,
        headers,
    })
}

/// The key saved for `id`, or why there is none to read from `env`.
fn saved_key(id: &str, env: &str, store: &AuthStore) -> Result<String, String> {
    store
        .api_credential(id)
        .map(|credential| credential.raw_token().to_owned())
        .ok_or_else(|| format!("no key: {env} is not set and no key is saved for {id}"))
}

/// The stored `ChatGPT` login, renewed first when it is about to expire
/// (the renewal is saved, so the registry sees it too).
fn chatgpt_login(store: &AuthStore) -> Option<auth::OAuthCredential> {
    store
        .credential(chatgpt::PROVIDER_ID)?
        .is_oauth()
        .then_some(())?;
    let mut store = store.clone();
    if let Ok(path) = AuthStore::default_path() {
        let _ = chatgpt::ensure_fresh(&mut store, &path, chrono::Utc::now());
    }
    match store.credential(chatgpt::PROVIDER_ID) {
        Some(auth::Credential::Oauth(login)) => Some(login.clone()),
        _ => None,
    }
}

/// The ids of the providers kage registers itself.
const BUILTIN: [&str; 3] = ["anthropic", "google", "openai"];

/// The environment variable provider `id` reads its key from: the
/// config's, else the one kage knows for a registered provider, else
/// `<ID>_API_KEY`. Empty means the provider needs no key.
fn key_env(id: &str, config: &Config) -> String {
    let custom = config.providers.custom.get(id);
    let saved = custom.map_or_else(
        || {
            config
                .providers
                .overrides
                .get(id)
                .and_then(|o| o.api_key_env.clone())
        },
        |custom| custom.api_key_env.clone(),
    );
    let registered = BUILTIN.contains(&id) || COMPAT_PROVIDERS.iter().any(|entry| entry.id == id);
    saved.unwrap_or_else(|| {
        if custom.is_none() && registered {
            auth::env_var_for(id).to_owned()
        } else {
            format!("{}_API_KEY", id.to_uppercase())
        }
    })
}

/// Where one provider finds its API key.
pub(super) fn provider_keys(config: &Config, store: &AuthStore) -> BTreeMap<String, ProviderKey> {
    BUILTIN
        .iter()
        .copied()
        .chain(COMPAT_PROVIDERS.iter().map(|entry| entry.id))
        .chain(config.providers.custom.keys().map(String::as_str))
        .map(|id| {
            let env = key_env(id, config);
            let source = if env.is_empty() {
                KeySource::Unneeded
            } else if std::env::var(&env).is_ok_and(|value| !value.is_empty()) {
                KeySource::Env
            } else if store.credential(id).is_some() {
                KeySource::Auth
            } else {
                KeySource::Missing
            };
            let name = display_name(id, config);
            (id.to_owned(), ProviderKey { env, source, name })
        })
        .collect()
}

/// The display name a client should show for provider `id`: the
/// compat list's curated one, else the model catalog's, else the
/// config's own display name for a custom provider, else nothing and
/// the client keeps the id.
fn display_name(id: &str, config: &Config) -> Option<String> {
    if let Some(entry) = COMPAT_PROVIDERS.iter().find(|entry| entry.id == id) {
        return Some(entry.display_name.to_owned());
    }
    if let Some(known) = kage_provider::catalog::provider(id) {
        return Some(known.name.to_owned());
    }
    config
        .providers
        .custom
        .get(id)
        .and_then(|custom| custom.display_name.clone())
}

/// How long an MCP server or ACP agent may take to answer.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// Connects to MCP server `name` as `spec` describes and lists its
/// tools. The worker owns the connection and closes it when done; a
/// server that never answers leaves only that worker behind.
pub(super) fn probe_mcp(name: &str, spec: &kage_core::config::McpServer) -> ConfigTestResult {
    let (tx, rx) = std::sync::mpsc::channel();
    let server = name.to_owned();
    let spec = spec.clone();
    let started = Instant::now();
    std::thread::spawn(move || {
        let listed = kage_mcp::McpServerHandle::spawn(&server, &spec, &[], None)
            .map_err(|e| e.to_string())
            .and_then(|handle| handle.connection().list_tools().map_err(|e| e.to_string()));
        let _ = tx.send(listed);
    });
    let outcome = rx
        .recv_timeout(START_TIMEOUT)
        .unwrap_or_else(|_| Err(format!("no answer within {}s", START_TIMEOUT.as_secs())));
    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match outcome {
        Ok(tools) => ConfigTestResult {
            ok: true,
            message: format!("connected \u{b7} {} tools \u{b7} {millis} ms", tools.len()),
            millis,
            tools: tools
                .into_iter()
                .map(|tool| ProbeTool {
                    name: tool.name,
                    description: tool.description,
                })
                .collect(),
            ..ConfigTestResult::default()
        },
        Err(why) => ConfigTestResult {
            message: why,
            millis,
            ..ConfigTestResult::default()
        },
    }
}

/// Starts ACP agent `agent`, sends `initialize` and waits for the
/// answer, then stops it.
pub(super) fn probe_acp(agent: &kage_core::config::AcpAgent) -> ConfigTestResult {
    use std::io::{BufRead as _, Write as _};

    let started = Instant::now();
    let failed = |message: String| ConfigTestResult {
        message,
        millis: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        ..ConfigTestResult::default()
    };
    let mut child = match std::process::Command::new(&agent.command)
        .args(&agent.args)
        .envs(&agent.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return failed(format!("cannot start {}: {e}", agent.command)),
    };
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": 1, "clientCapabilities": {} },
    });
    // Held open until the answer: closing stdin ends some agents.
    let mut stdin = child.stdin.take();
    if let Some(stdin) = &mut stdin {
        let _ = writeln!(stdin, "{request}");
    }
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if value["id"] == 1 {
                    let _ = tx.send(value);
                    return;
                }
            }
        });
    }
    let answer = rx.recv_timeout(START_TIMEOUT);
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let Ok(answer) = answer else {
        return failed(format!(
            "no initialize answer within {}s",
            START_TIMEOUT.as_secs()
        ));
    };
    if let Some(error) = answer.get("error") {
        return failed(format!(
            "initialize failed: {}",
            error["message"].as_str().unwrap_or("error")
        ));
    }
    let info = &answer["result"]["agentInfo"];
    let who = [info["name"].as_str(), info["version"].as_str()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    ConfigTestResult {
        ok: true,
        message: format!(
            "initialize \u{b7} {} \u{b7} {millis} ms",
            if who.is_empty() {
                "agent answered"
            } else {
                who.as_str()
            }
        ),
        millis,
        agent: (!who.is_empty()).then_some(who),
        ..ConfigTestResult::default()
    }
}

/// Lists the models of provider `id` at `target`, warning when the key
/// rides a cleartext request.
fn list(id: &str, target: &Target) -> ConfigTestResult {
    let mut result = list_models(id, target);
    if let Some(warning) = cleartext_warning(target) {
        result.message = format!("{warning}\n{}", result.message);
    }
    result
}

/// The warning when a non-empty key would be sent over `http://`, which
/// any network peer can read. `None` for https and for providers that
/// need no key, where `http://` is the normal local case.
fn cleartext_warning(target: &Target) -> Option<&'static str> {
    let url = models_url(target);
    let same_origin = origin(&url) == origin(&target.base);
    let cleartext = url.starts_with("http://") && same_origin && !target.key.is_empty();
    cleartext.then_some("warning: the key is sent over cleartext http; use https")
}

/// The URL the model list is read from: the models URL when one is
/// set, else the wire protocol's own list path on the base.
fn models_url(target: &Target) -> String {
    match &target.models_url {
        Some(url) => url.clone(),
        None => match target.kind {
            Kind::OpenAi => format!("{}/models", target.base),
            Kind::Anthropic => format!("{}/v1/models", target.base),
            Kind::Gemini => format!("{}/v1beta/models", target.base),
        },
    }
}

/// The `host:port` authority of an http(s) URL, for the same-origin
/// check that guards the key and headers.
fn origin(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next()?;
    (!authority.is_empty()).then_some(authority)
}

/// The model list request of `list`, without the cleartext warning.
fn list_models(id: &str, target: &Target) -> ConfigTestResult {
    let url = models_url(target);
    // The key and the provider's headers belong to the endpoint; a
    // models URL elsewhere reads them anonymously.
    let same_origin = origin(&url) == origin(&target.base);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .build()
        .new_agent();
    let mut request = agent.get(url.as_str());
    if same_origin && !target.key.is_empty() {
        request = match target.kind {
            Kind::OpenAi => request.header("authorization", format!("Bearer {}", target.key)),
            Kind::Anthropic => request
                .header("x-api-key", target.key.as_str())
                .header("anthropic-version", "2023-06-01"),
            Kind::Gemini => request.header("x-goog-api-key", target.key.as_str()),
        };
    }
    for (name, value) in &target.headers {
        if same_origin {
            request = request.header(name.as_str(), value.as_str());
        }
    }
    let started = Instant::now();
    let response = request.call();
    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut response = match response {
        Ok(response) => response,
        Err(error) => {
            return ConfigTestResult {
                message: format!("GET {url} failed: {error}"),
                millis,
                ..ConfigTestResult::default()
            };
        }
    };
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        let detail: String = body.trim().chars().take(200).collect();
        return ConfigTestResult {
            status: Some(status),
            message: format!("GET {url} \u{b7} {status} \u{b7} {detail}"),
            millis,
            ..ConfigTestResult::default()
        };
    }
    let Some(models) = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|json| parse_models(target.kind, &json, &target.base))
    else {
        return ConfigTestResult {
            status: Some(status),
            message: format!("GET {url} \u{b7} {status} \u{b7} the answer is not a model list"),
            millis,
            ..ConfigTestResult::default()
        };
    };
    let models: Vec<ProbeModel> = models
        .into_iter()
        .map(|model| with_catalog(id, model))
        .collect();
    ConfigTestResult {
        ok: true,
        status: Some(status),
        message: format!(
            "GET {url} \u{b7} {status} \u{b7} {} models \u{b7} {millis} ms",
            models.len()
        ),
        millis,
        models,
        ..ConfigTestResult::default()
    }
}

/// The models a model list answer names, sorted by id. `None` when the
/// answer has no list. Beyond the wire protocol's own shape, the
/// models.dev directory shape is understood: an object keyed by
/// provider, holding `models` maps with limits, modalities, reasoning
/// and effort levels. The provider entry whose `api` names the asked
/// base wins; with no match, a sole entry is taken.
fn parse_models(kind: Kind, json: &Value, base: &str) -> Option<Vec<ProbeModel>> {
    let mut models = match kind {
        Kind::OpenAi => parse_envelope(json)
            .or_else(|| parse_codex(json))
            .or_else(|| parse_directory(json, base)),
        Kind::Anthropic => parse_envelope(json).or_else(|| parse_directory(json, base)),
        Kind::Gemini => parse_gemini(json).or_else(|| parse_directory(json, base)),
    }?;
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Some(models)
}

/// The `data` list the `OpenAI` and Anthropic protocols answer with.
/// `None` when the answer carries no such list; an empty one reads as
/// no models.
fn parse_envelope(json: &Value) -> Option<Vec<ProbeModel>> {
    json["data"].as_array()?;
    Some(
        json["data"]
            .as_array()?
            .iter()
            .filter_map(|entry| {
                let mut input = string_list(&entry["architecture"]["input_modalities"]);
                if input.is_empty() {
                    input = string_list(&entry["input_modalities"]);
                }
                let mut produces = string_list(&entry["architecture"]["output_modalities"]);
                if produces.is_empty() {
                    produces = string_list(&entry["output_modalities"]);
                }
                let mut efforts = Vec::new();
                if let Some(effort) = entry["effort"].as_str() {
                    efforts.push(effort.to_owned());
                }
                if efforts.is_empty() {
                    efforts = string_list(&entry["support_efforts"]);
                }
                Some(ProbeModel {
                    id: entry["id"].as_str()?.to_owned(),
                    name: entry["display_name"]
                        .as_str()
                        .or_else(|| entry["name"].as_str())
                        .map(str::to_owned),
                    context: number_of(&entry["context_length"])
                        .or_else(|| number_of(&entry["context_window"])),
                    max_output: number_of(&entry["top_provider"]["max_completion_tokens"])
                        .or_else(|| number_of(&entry["max_output_tokens"])),
                    input,
                    produces,
                    efforts,
                    ..ProbeModel::default()
                })
            })
            .collect(),
    )
}

/// The `{"models": [...]}` listing of the `ChatGPT` backend, keyed by
/// `slug`, without the models it hides from pickers. `None` when the
/// answer has no such list.
fn parse_codex(json: &Value) -> Option<Vec<ProbeModel>> {
    Some(
        json["models"]
            .as_array()?
            .iter()
            .filter(|entry| entry["visibility"].as_str() != Some("hide"))
            .filter_map(|entry| {
                Some(ProbeModel {
                    id: entry["slug"].as_str()?.to_owned(),
                    name: entry["display_name"].as_str().map(str::to_owned),
                    context: number_of(&entry["max_context_window"])
                        .or_else(|| number_of(&entry["context_window"])),
                    efforts: entry["supported_reasoning_levels"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|level| level["effort"].as_str().map(str::to_owned))
                        .collect(),
                    ..ProbeModel::default()
                })
            })
            .collect(),
    )
}

/// The `{"models": [...]}` listing the Gemini protocol answers with.
/// `None` when the answer carries no `models` list.
fn parse_gemini(json: &Value) -> Option<Vec<ProbeModel>> {
    json["models"].as_array()?;
    Some(
        json["models"]
            .as_array()?
            .iter()
            .filter(|entry| {
                entry["supportedGenerationMethods"]
                    .as_array()
                    .is_none_or(|methods| methods.iter().any(|m| m == "generateContent"))
            })
            .filter_map(|entry| {
                let name = entry["name"].as_str()?;
                Some(ProbeModel {
                    id: name.strip_prefix("models/").unwrap_or(name).to_owned(),
                    name: entry["displayName"].as_str().map(str::to_owned),
                    context: number_of(&entry["inputTokenLimit"]),
                    max_output: number_of(&entry["outputTokenLimit"]),
                    ..ProbeModel::default()
                })
            })
            .collect(),
    )
}

/// The models of a models.dev directory answer: an object keyed by
/// provider id, each holding a `models` map of rich entries. The entry
/// whose `api` names `base`, else the only entry, if there is just one.
fn parse_directory(json: &Value, base: &str) -> Option<Vec<ProbeModel>> {
    let providers = json.as_object()?;
    let entry = if providers.len() == 1 {
        providers.values().next()
    } else {
        providers.values().find(|provider| {
            provider["api"]
                .as_str()
                .is_some_and(|api| api.trim_end_matches('/') == base.trim_end_matches('/'))
        })
    }?;
    let models = entry["models"].as_object()?;
    Some(
        models
            .iter()
            .map(|(key, model)| ProbeModel {
                id: model["id"].as_str().unwrap_or(key).to_owned(),
                name: model["name"].as_str().map(str::to_owned),
                context: number_of(&model["limit"]["context"]),
                max_output: number_of(&model["limit"]["output"]),
                reasoning: model["reasoning"].as_bool(),
                input: string_list(&model["modalities"]["input"]),
                produces: string_list(&model["modalities"]["output"]),
                efforts: string_list(&model["support_efforts"]),
            })
            .collect(),
    )
}

/// The value as a list of strings, empty when it is not one.
fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The value as a number, `None` when it is not one.
fn number_of(value: &Value) -> Option<u64> {
    value.as_u64()
}

/// `model` with what the model catalog knows filling the gaps: the
/// entry under provider `id`, else the first provider listing the id.
fn with_catalog(id: &str, mut model: ProbeModel) -> ProbeModel {
    let known = catalog::model(id, &model.id).or_else(|| {
        catalog::providers()
            .iter()
            .find_map(|provider| provider.models.iter().find(|m| m.id == model.id))
    });
    if let Some(known) = known {
        model.name.get_or_insert_with(|| known.name.to_owned());
        if model.context.is_none() {
            model.context = known.context;
        }
        if model.max_output.is_none() {
            model.max_output = known.output;
        }
        if model.reasoning.is_none() {
            model.reasoning = match known.reasoning {
                Reasoning::Unknown => None,
                Reasoning::None => Some(false),
                Reasoning::Fixed
                | Reasoning::Toggle
                | Reasoning::Budget { .. }
                | Reasoning::Effort { .. } => Some(true),
            };
        }
        if model.input.is_empty() {
            model.input = known
                .input
                .iter()
                .map(|kind| kind.as_str().to_owned())
                .collect();
        }
        if model.efforts.is_empty()
            && let Reasoning::Effort { efforts, .. } = known.reasoning
        {
            model.efforts = efforts.iter().map(|e| e.as_str().to_owned()).collect();
        }
    }
    model
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    use kage_acp::acp::ProviderProbe;
    use kage_core::config::Config;

    use kage_acp::acp::KeySource;

    use super::{Kind, Target, cleartext_warning, probe, provider_keys, resolve};
    use crate::auth::AuthStore;

    /// Answers one request with `status` and `body`, handing back the
    /// request head it read.
    fn serve_once(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                head.push_str(&line);
            }
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            head
        });
        (base, thread)
    }

    fn draft(id: &str, base: &str) -> ProviderProbe {
        ProviderProbe {
            id: id.to_owned(),
            base_url: Some(base.to_owned()),
            api_key: Some("sk-typed".to_owned()),
            ..ProviderProbe::default()
        }
    }

    #[test]
    fn a_models_dev_directory_answer_maps_limits_modalities_and_efforts() {
        let json = serde_json::json!({
            "example-lab": {
                "id": "example-lab",
                "name": "Lab",
                "api": "https://lab.pg.example.dev/v1",
                "type": "openai",
                "models": {
                    "lab-big": {
                        "id": "lab-big",
                        "name": "Lab Big",
                        "limit": {"context": 200_000, "output": 32_000},
                        "tool_call": true,
                        "modalities": {"input": ["text", "image"], "output": ["text"]}
                    },
                    "lab-thinker": {
                        "id": "lab-thinker",
                        "name": "Lab Thinker",
                        "limit": {"context": 1_048_576, "output": 131_072},
                        "reasoning": true,
                        "support_efforts": ["minimal", "low", "medium", "high", "xhigh"],
                        "default_effort": "xhigh"
                    }
                }
            }
        });
        let models =
            super::parse_models(Kind::Anthropic, &json, "https://lab.pg.example.dev/v1").unwrap();
        assert_eq!(models.len(), 2);
        let big = models.iter().find(|m| m.id == "lab-big").unwrap();
        assert_eq!((big.context, big.max_output), (Some(200_000), Some(32_000)));
        assert_eq!(big.input, ["text", "image"]);
        assert_eq!(big.produces, ["text"]);
        let thinker = models.iter().find(|m| m.id == "lab-thinker").unwrap();
        assert_eq!(thinker.reasoning, Some(true));
        assert_eq!(
            thinker.efforts,
            ["minimal", "low", "medium", "high", "xhigh"]
        );
    }

    #[test]
    fn an_envelope_entry_effort_becomes_the_efforts_list() {
        let json = serde_json::json!({
            "data": [
                {"id": "m", "context_window": 1000, "max_output_tokens": 100, "effort": "xhigh"}
            ]
        });
        let models =
            super::parse_models(Kind::Anthropic, &json, "https://lab.example.com/v1").unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].efforts, ["xhigh"]);
    }

    #[test]
    fn a_directory_answer_picks_the_provider_whose_api_matches() {
        let json = serde_json::json!({
            "other": {
                "api": "https://other.example.com/v1",
                "models": {"decoy": {"id": "decoy", "name": "Decoy"}}
            },
            "lab": {
                "api": "https://lab.example.com/v1",
                "models": {"real": {"id": "real", "name": "Real"}}
            }
        });
        let models =
            super::parse_models(Kind::OpenAi, &json, "https://lab.example.com/v1").unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "real");
    }

    #[test]
    fn a_draft_lists_its_models_with_the_typed_key() {
        let (base, server) = serve_once(
            200,
            r#"{"data":[{"id":"zeta"},{"id":"alpha","context_length":32000}]}"#,
        );
        let result = probe(
            &draft("lab", &format!("{base}/v1")),
            &Config::default(),
            &AuthStore::empty(),
        );
        let head = server.join().unwrap();
        assert!(head.starts_with("GET /v1/models "), "{head}");
        assert!(
            head.to_lowercase()
                .contains("authorization: bearer sk-typed"),
            "{head}"
        );
        assert!(result.ok, "{}", result.message);
        assert_eq!(result.status, Some(200));
        let ids: Vec<_> = result.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["alpha", "zeta"]);
        assert_eq!(result.models[0].context, Some(32_000));
        assert!(result.message.contains("2 models"), "{}", result.message);
        assert!(result.message.contains("cleartext"), "{}", result.message);
    }

    #[test]
    fn a_models_url_on_the_endpoint_serves_the_list_with_the_key() {
        let body = r#"{"example-lab":{"api":"http://elsewhere","models":{"lab-one":{"id":"lab-one","name":"Lab One","limit":{"context":4096,"output":512},"modalities":{"input":["text","image"]}}}}}"#;
        let (base, server) = serve_once(200, body);
        let mut draft = draft("lab", &format!("{base}/v1"));
        draft.models_url = Some(format!("{base}/directory.json"));
        let result = probe(&draft, &Config::default(), &AuthStore::empty());
        let head = server.join().unwrap();
        assert!(head.starts_with("GET /directory.json "), "{head}");
        assert!(
            head.to_lowercase()
                .contains("authorization: bearer sk-typed"),
            "{head}"
        );
        assert!(result.ok, "{}", result.message);
        let one = &result.models[0];
        assert_eq!((one.context, one.max_output), (Some(4096), Some(512)));
        assert_eq!(one.input, ["text", "image"]);
    }

    #[test]
    fn a_models_url_on_another_host_is_read_anonymously() {
        let body = r#"{"lab":{"models":{"lab-one":{"id":"lab-one","name":"Lab One"}}}}"#;
        let (base, _endpoint) = serve_once(500, "{}");
        let (models_base, server) = serve_once(200, body);
        let mut draft = draft("lab", &base);
        draft.api_key = Some("sk-secret".into());
        draft.models_url = Some(format!("{models_base}/doc.json"));
        let result = probe(&draft, &Config::default(), &AuthStore::empty());
        assert!(result.ok, "{}", result.message);
        assert_eq!(result.models.len(), 1);
        let head = server.join().unwrap();
        assert!(!head.to_lowercase().contains("authorization"), "{head}");
    }

    #[test]
    fn only_an_http_base_with_a_key_warns_about_cleartext() {
        let warned = Target {
            kind: Kind::OpenAi,
            base: "http://localhost:11434/v1".to_owned(),
            models_url: None,
            key: "sk-typed".to_owned(),
            headers: BTreeMap::new(),
        };
        assert_eq!(
            cleartext_warning(&warned),
            Some("warning: the key is sent over cleartext http; use https")
        );
        let secure = Target {
            base: "https://api.example.com/v1".to_owned(),
            ..warned
        };
        assert_eq!(cleartext_warning(&secure), None);
        let keyless = Target {
            base: "http://localhost:11434/v1".to_owned(),
            key: String::new(),
            ..secure
        };
        assert_eq!(cleartext_warning(&keyless), None);
    }

    #[test]
    fn a_refusal_is_a_result_with_the_status() {
        let (base, server) = serve_once(401, r#"{"error":"bad key"}"#);
        let result = probe(
            &draft("lab", &base),
            &Config::default(),
            &AuthStore::empty(),
        );
        server.join().unwrap();
        assert!(!result.ok);
        assert_eq!(result.status, Some(401));
        assert!(
            result.message.contains("401") && result.message.contains("bad key"),
            "{}",
            result.message
        );
        assert!(!result.message.contains("sk-typed"));
    }

    #[test]
    fn a_saved_entry_fills_the_probe() {
        let config: Config = kage_core::config_edit::parse(
            "[providers.custom.lab]\nkind = \"anthropic\"\nbase_url = \"http://lab:1/\"\napi_key_env = \"\"\nheaders = { X-Team = \"red\" }\n[[providers.custom.lab.models]]\nid = \"m\"\nname = \"M\"\n",
        )
        .unwrap();
        let probe = ProviderProbe {
            id: "lab".to_owned(),
            headers: [("X-Team".to_owned(), "<redacted>".to_owned())].into(),
            ..ProviderProbe::default()
        };
        let target = resolve(&probe, &config, &AuthStore::empty()).unwrap();
        assert_eq!(target.kind, Kind::Anthropic);
        assert_eq!(target.base, "http://lab:1");
        assert_eq!(target.key, "");
        assert_eq!(target.headers["X-Team"], "red");
    }

    #[test]
    fn each_provider_says_where_its_key_is() {
        let config: Config = kage_core::config_edit::parse(
            "[providers.custom.local]\nbase_url = \"http://l\"\napi_key_env = \"\"\n[[providers.custom.local.models]]\nid = \"m\"\nname = \"M\"\n\n[providers.custom.lab]\nbase_url = \"http://lab\"\napi_key_env = \"KAGE_TEST_UNSET_LAB_KEY\"\n[[providers.custom.lab.models]]\nid = \"m\"\nname = \"M\"\n",
        )
        .unwrap();
        let mut store = AuthStore::empty();
        store.set_api_key("deepseek", "sk-saved");
        let keys = provider_keys(&config, &store);
        assert_eq!(keys["local"].source, KeySource::Unneeded);
        assert_eq!(keys["lab"].env, "KAGE_TEST_UNSET_LAB_KEY");
        assert_eq!(keys["lab"].source, KeySource::Missing);
        assert_eq!(keys["deepseek"].env, "DEEPSEEK_API_KEY");
        assert!(matches!(
            keys["deepseek"].source,
            KeySource::Auth | KeySource::Env
        ));
        assert!(keys.contains_key("anthropic"));
        assert_eq!(keys["deepseek"].name.as_deref(), Some("DeepSeek"));
        assert_eq!(keys["anthropic"].name.as_deref(), Some("Anthropic"));
        assert_eq!(keys["local"].name, None, "a bare custom id has no name");
    }

    #[test]
    fn an_acp_agent_answers_its_handshake() {
        let answer = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"fake","version":"1.0"}}}"#;
        let agent = kage_core::config::AcpAgent {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                format!("read line; printf '%s\\n' '{answer}'; sleep 5"),
            ],
            env: std::collections::BTreeMap::new(),
        };
        let result = super::probe_acp(&agent);
        assert!(result.ok, "{}", result.message);
        assert_eq!(result.agent.as_deref(), Some("fake 1.0"));
        let missing = kage_core::config::AcpAgent {
            command: "/nonexistent/agent".into(),
            ..agent
        };
        let result = super::probe_acp(&missing);
        assert!(!result.ok);
        assert!(
            result.message.contains("cannot start"),
            "{}",
            result.message
        );
    }

    #[test]
    fn an_mcp_server_that_cannot_start_says_why() {
        let spec = kage_core::config::McpServer {
            startup_timeout_secs: None,
            command: Some("/nonexistent/server".into()),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            disabled: false,
            oauth: None,
            disabled_tools: Vec::new(),
        };
        let result = super::probe_mcp("gone", &spec);
        assert!(!result.ok);
        assert!(!result.message.is_empty());
    }

    /// A store with only a `ChatGPT` login.
    fn chatgpt_store() -> AuthStore {
        let mut store = AuthStore::empty();
        store.set_oauth(
            "openai",
            crate::auth::OAuthCredential {
                access_token: "chatgpt-token".to_owned(),
                account_id: Some("acc-1".to_owned()),
                ..crate::auth::OAuthCredential::default()
            },
        );
        store
    }

    /// With no key typed or exported, `openai` lists the backend's
    /// models with the `ChatGPT` login and its headers.
    #[test]
    fn a_chatgpt_login_probes_the_backend_model_list() {
        let probe = ProviderProbe {
            id: "openai".to_owned(),
            api_key_env: Some("KAGE_TEST_UNSET_OPENAI_KEY".to_owned()),
            ..ProviderProbe::default()
        };
        let target = resolve(&probe, &Config::default(), &chatgpt_store()).unwrap();
        assert_eq!(target.base, crate::chatgpt::BACKEND_BASE_URL);
        assert_eq!(
            target.models_url.as_deref(),
            Some(crate::chatgpt::models_url(crate::chatgpt::BACKEND_BASE_URL).as_str())
        );
        assert_eq!(target.key, "chatgpt-token");
        assert_eq!(target.headers["chatgpt-account-id"], "acc-1");
        assert_eq!(target.headers["originator"], crate::chatgpt::ORIGINATOR);

        let typed = ProviderProbe {
            api_key: Some("sk-typed".to_owned()),
            ..probe
        };
        let target = resolve(&typed, &Config::default(), &chatgpt_store()).unwrap();
        assert_eq!(target.base, "https://api.openai.com/v1", "a typed key wins");

        let keys = provider_keys(&Config::default(), &chatgpt_store());
        assert!(matches!(
            keys["openai"].source,
            KeySource::Auth | KeySource::Env
        ));
    }

    #[test]
    fn the_backend_model_list_skips_hidden_models() {
        let json = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-6-sol",
                    "display_name": "GPT-6 Sol",
                    "visibility": "list",
                    "context_window": 400_000,
                    "supported_reasoning_levels": [
                        { "effort": "low", "description": "fast" },
                        { "effort": "high", "description": "deep" },
                    ],
                },
                { "slug": "internal", "visibility": "hide" },
            ],
        });
        let models = super::parse_models(Kind::OpenAi, &json, "https://x").unwrap();
        assert_eq!(models.len(), 1, "{models:?}");
        assert_eq!(models[0].id, "gpt-6-sol");
        assert_eq!(models[0].name.as_deref(), Some("GPT-6 Sol"));
        assert_eq!(models[0].context, Some(400_000));
        assert_eq!(models[0].efforts, ["low", "high"]);
    }

    #[test]
    fn a_missing_key_or_url_says_so() {
        let probe = ProviderProbe {
            id: "lab".to_owned(),
            base_url: Some("http://lab".to_owned()),
            api_key_env: Some("KAGE_TEST_UNSET_KEY_VAR".to_owned()),
            ..ProviderProbe::default()
        };
        let err = resolve(&probe, &Config::default(), &AuthStore::empty()).unwrap_err();
        assert!(err.contains("KAGE_TEST_UNSET_KEY_VAR"), "{err}");
        let nowhere = ProviderProbe {
            id: "lab".to_owned(),
            ..ProviderProbe::default()
        };
        assert_eq!(
            resolve(&nowhere, &Config::default(), &AuthStore::empty()).unwrap_err(),
            "set a base URL"
        );
    }
}
