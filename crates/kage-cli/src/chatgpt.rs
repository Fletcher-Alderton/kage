//! `ChatGPT` sign-in for the `OpenAI` provider: the browser OAuth flow the
//! Codex CLI uses, pointed at the same public client.
//!
//! `kage auth login openai` offers this instead of an API key. The
//! browser authorizes kage against `auth.openai.com` (PKCE S256, a
//! random `state`), the code comes back to a one-shot listener on
//! `127.0.0.1:1455` (paste the redirect URL when no browser can reach
//! this machine), and the tokens land in `auth.json` as an OAuth
//! credential carrying the `chatgpt_account_id` parsed from the
//! `id_token`. The provider registry then routes `openai` models
//! through the `ChatGPT` backend at
//! `https://chatgpt.com/backend-api/codex`. PKCE, the state check, the
//! listener and the paste prompt are the ones MCP logins use.
//!
//! Access tokens expire, so two paths renew them: [`ensure_fresh`] at
//! registry build, and the provider's 401 handler through
//! [`ChatGptBearer`]. Both persist the renewal under a lock file beside
//! `auth.json`, because a refresh token rotates only once and several
//! kage processes may share the login.
//!
//! Every constant here matches the flow Codex CLI performs; they are
//! collected at the top so a backend change is a one-place edit.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use kage_mcp::oauth::{self, AuthCode, Expected, Loopback, OAuthError, Pkce};
use serde_json::Value;

use crate::auth::{AuthStore, OAuthCredential};

/// The public OAuth client id of the Codex CLI, reused for `ChatGPT`
/// sign-in. Not a secret: it rides every authorize URL.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The issuer of `ChatGPT` OAuth tokens.
pub const ISSUER: &str = "https://auth.openai.com";

/// The Responses API of the `ChatGPT` backend that a `ChatGPT` login can
/// call. Requests append `/responses`.
pub const BACKEND_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Scopes of the login. `offline_access` is what makes the refresh
/// token exist.
pub const SCOPES: &str = "openid profile email offline_access";

/// The loopback port the Codex CLI registered its redirect URI on.
/// The auth server only redirects there (or to [`CALLBACK_PORT_FALLBACK`]).
pub const CALLBACK_PORT: u16 = 1455;

/// The fallback port when 1455 is taken, as the Codex CLI falls back.
pub const CALLBACK_PORT_FALLBACK: u16 = 1457;

/// The redirect path of the registered callback URIs.
const CALLBACK_PATH: &str = "/auth/callback";

/// The `originator` header and authorize parameter the backend expects
/// from first-party CLI clients.
pub const ORIGINATOR: &str = "codex_cli_rs";

/// The `OpenAI-Beta` value the `ChatGPT` Responses endpoint routes on.
const OPENAI_BETA: &str = "responses=experimental";

/// The Codex CLI release whose requests kage's match. The backend's
/// model list answers with the models this version may use.
const CODEX_CLIENT_VERSION: &str = "0.164.0";

/// Refresh at registry build when the stored token expires within this
/// window, the window the Codex CLI renews in. Renewing earlier only
/// spends refresh-token rotations.
const REFRESH_SLACK: chrono::Duration = chrono::Duration::minutes(5);

/// Which provider id the `ChatGPT` credential is stored under.
pub const PROVIDER_ID: &str = "openai";

/// Cap on a token endpoint answer body.
const MAX_BODY: u64 = 1024 * 1024;

/// What `Debug` prints in place of a secret.
const REDACTED: &str = "***";

/// A failed login or refresh. Messages never carry a token.
#[derive(Debug, thiserror::Error)]
pub enum ChatGptError {
    /// The browser leg failed: the listener, the redirect, a timeout
    /// or Ctrl+C.
    #[error(transparent)]
    OAuth(#[from] OAuthError),
    /// The token endpoint refused.
    #[error("the authorization server refused: {0}")]
    Denied(String),
    /// A token answer lacks a required field or a token is malformed.
    #[error("{0}")]
    Response(String),
    /// A request could not be sent or its answer not read.
    #[error("{0}")]
    Http(String),
}

/// The URL the browser opens: the `ChatGPT` authorize endpoint with the
/// S256 challenge, this login's `state`, and the parameters the Codex
/// CLI sends so the account picker behaves the same.
fn authorize_url(pkce: &Pkce, expected: &Expected, redirect_uri: &str) -> String {
    let mut url = url::Url::parse(&format!("{ISSUER}/oauth/authorize")).expect("constant URL");
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("code_challenge", pkce.challenge())
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", expected.state())
            .append_pair("scope", SCOPES)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", ORIGINATOR);
    }
    url.into()
}

/// The claims of a JWT payload: base64url-decoded JSON. The signature
/// is not checked, matching the Codex CLI: the token arrived over TLS
/// straight from the authorization server.
fn jwt_claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The `ChatGPT` workspace/account id inside an `id_token` or access
/// token, under the `https://api.openai.com/auth` claim.
fn account_id_from_jwt(jwt: &str) -> Option<String> {
    let claims = jwt_claims(jwt)?;
    let auth = claims.get("https://api.openai.com/auth")?;
    auth.get("chatgpt_account_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// When a JWT access token stops working, from its `exp` claim.
fn expiry_from_jwt(jwt: &str) -> Option<DateTime<Utc>> {
    let exp = jwt_claims(jwt)?.get("exp").and_then(Value::as_i64)?;
    DateTime::from_timestamp(exp, 0)
}

/// The raw token-endpoint answer, before it becomes a credential.
#[derive(Clone, Default)]
struct TokenAnswer {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
}

impl fmt::Debug for TokenAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenAnswer")
            .field("access_token", &REDACTED)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| REDACTED),
            )
            .field("id_token", &self.id_token.as_ref().map(|_| REDACTED))
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish()
    }
}

/// An agent for the token endpoint: statuses are read rather than
/// raised, and redirects are not followed.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .new_agent()
}

/// Parse a token-endpoint answer body.
fn parse_answer(status: u16, body: &str) -> Result<TokenAnswer, ChatGptError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| ChatGptError::Response("the token answer is not JSON".to_owned()))?;
    if !(200..300).contains(&status) {
        let code = value
            .get("error")
            .and_then(Value::as_str)
            .map_or_else(|| "no error code".to_owned(), oauth::error_code);
        return Err(ChatGptError::Denied(format!("status {status}: {code}")));
    }
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    Ok(TokenAnswer {
        access_token: text("access_token").ok_or_else(|| {
            ChatGptError::Response("the token answer has no access_token".to_owned())
        })?,
        refresh_token: text("refresh_token"),
        id_token: text("id_token"),
        expires_in: value.get("expires_in").and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        }),
        scope: text("scope"),
    })
}

/// POST the authorization-code grant (form-encoded, as the endpoint
/// requires) and parse the answer.
fn exchange_code(
    code: &AuthCode,
    pkce: &Pkce,
    redirect_uri: &str,
) -> Result<TokenAnswer, ChatGptError> {
    let endpoint = format!("{ISSUER}/oauth/token");
    let form: &[(&str, &str)] = &[
        ("grant_type", "authorization_code"),
        ("code", code.secret()),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", pkce.verifier()),
    ];
    let mut response = agent()
        .post(&endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .send_form(form.iter().copied())
        .map_err(|e| ChatGptError::Http(format!("token request: {e}")))?;
    let body = read_body(&mut response);
    parse_answer(response.status().as_u16(), &body)
}

/// POST the refresh-token grant. `ChatGPT` renewal is JSON-encoded, and
/// an answer without a new refresh token keeps the old one.
fn refresh_tokens(refresh_token: &str) -> Result<TokenAnswer, ChatGptError> {
    let endpoint = format!("{ISSUER}/oauth/token");
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh_token,
    });
    let mut response = agent()
        .post(&endpoint)
        .header("content-type", "application/json")
        .send_json(&body)
        .map_err(|e| ChatGptError::Http(format!("token renewal: {e}")))?;
    let text = read_body(&mut response);
    let mut answer = parse_answer(response.status().as_u16(), &text)?;
    if answer.refresh_token.is_none() {
        answer.refresh_token = Some(refresh_token.to_owned());
    }
    Ok(answer)
}

/// Read a response body, capped.
fn read_body(response: &mut ureq::http::Response<ureq::Body>) -> String {
    response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .unwrap_or_default()
}

/// Build the stored credential from a token answer. Expiry prefers the
/// `expires_in` the server stated, falling back to the access token's
/// own `exp` claim. The account id comes from the `id_token`, else the
/// access token, else the previous credential.
fn credential_from(answer: &TokenAnswer, previous: Option<&OAuthCredential>) -> OAuthCredential {
    let expires_at = answer
        .expires_in
        .and_then(|secs| chrono::Duration::try_seconds(i64::try_from(secs).ok()?))
        .and_then(|d| Utc::now().checked_add_signed(d))
        .or_else(|| expiry_from_jwt(&answer.access_token));
    let account_id = answer
        .id_token
        .as_deref()
        .and_then(account_id_from_jwt)
        .or_else(|| account_id_from_jwt(&answer.access_token))
        .or_else(|| previous.and_then(|p| p.account_id.clone()));
    OAuthCredential {
        access_token: answer.access_token.clone(),
        refresh_token: answer
            .refresh_token
            .clone()
            .or_else(|| previous.and_then(|p| p.refresh_token.clone())),
        expires_at,
        scope: Some(answer.scope.clone().unwrap_or_else(|| SCOPES.to_owned())),
        account_id,
    }
}

/// Run the whole login: authorize in the browser, wait for the
/// callback or a pasted redirect URL, trade the code for tokens, and
/// return the credential to store. The signal hook routes Ctrl+C into
/// `cancel` for the length of the login, so an interrupted login does
/// not kill the host process. The terminal reader stops when the login
/// ends, so a TUI that suspended itself for `:login` gets its input
/// back.
///
/// # Errors
///
/// A message for every way a login can fail, ready for a `kage: ` line.
pub fn login(cancel: &Arc<AtomicBool>) -> Result<OAuthCredential, String> {
    let sigint = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(cancel)).ok();
    let stop = AtomicBool::new(false);
    let result = thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        let stop = &stop;
        scope.spawn(move || kage_tui::terminal::forward_typed_lines(&tx, stop));
        let result = login_inner(&rx, cancel).map_err(|e| e.to_string());
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        result
    });
    if let Some(id) = sigint {
        signal_hook::low_level::unregister(id);
    }
    result
}

/// The login steps, with the typed error kept for tests.
fn login_inner(
    pasted: &Receiver<String>,
    cancel: &AtomicBool,
) -> Result<OAuthCredential, ChatGptError> {
    let pkce = Pkce::new()?;
    let expected = Expected::state_only()?;
    let loopback = Loopback::bind_on(&[CALLBACK_PORT, CALLBACK_PORT_FALLBACK], CALLBACK_PATH)?;
    let redirect = loopback.redirect_uri();
    let url = authorize_url(&pkce, &expected, &redirect);
    eprintln!("Sign in with ChatGPT. Open this URL in your browser:\n\n{url}\n");
    if crate::opener::open(std::ffi::OsStr::new(&url)).is_err() {
        eprintln!("kage: no browser detected; open the URL above manually");
    }
    crate::mcp_auth::prompt_for_redirect();
    let code = crate::mcp_auth::wait_for_code(loopback, &expected, pasted, cancel)?;
    let answer = exchange_code(&code, &pkce, &redirect)?;
    let credential = credential_from(&answer, None);
    if credential.account_id.is_none() {
        eprintln!("kage: the login carries no ChatGPT account id; requests may be refused");
    }
    Ok(credential)
}

/// Renew `credential` at the token endpoint. The refresh token is kept
/// when the answer does not rotate it; the account id never changes.
///
/// # Errors
///
/// When the endpoint refuses or the answer is unusable; the caller
/// should ask for a fresh login.
pub fn refresh_credential(credential: &OAuthCredential) -> Result<OAuthCredential, String> {
    let Some(refresh_token) = credential.refresh_token.as_deref() else {
        return Err(
            "the ChatGPT login has no refresh token; run `kage auth login openai`".to_owned(),
        );
    };
    let answer = refresh_tokens(refresh_token).map_err(|e| e.to_string())?;
    Ok(credential_from(&answer, Some(credential)))
}

/// Take the exclusive lock that serializes renewals of the store at
/// `path` across kage processes: `<path>.lock`, held until the returned
/// file drops.
fn renewal_lock(path: &Path) -> Result<File, String> {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let lock_path = PathBuf::from(name);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("open {}: {e}", lock_path.display()))?;
    file.lock()
        .map_err(|e| format!("lock {}: {e}", lock_path.display()))?;
    Ok(file)
}

/// The `ChatGPT` login in `store`, if one is stored.
fn stored_login(store: &AuthStore) -> Option<&OAuthCredential> {
    match store.credential(PROVIDER_ID) {
        Some(crate::auth::Credential::Oauth(oauth)) => Some(oauth),
        _ => None,
    }
}

/// Refresh the `ChatGPT` login in `store` (loaded from `path`) when it
/// expires within [`REFRESH_SLACK`] of `now`, persisting the renewal. A
/// no-op for any other credential shape. Under the renewal lock the
/// file is read again, and `store` becomes that reading, so a renewal
/// another process made in the meantime is adopted instead of repeated.
///
/// # Errors
///
/// When the lock, the reload, the renewal or the save fails. The 401
/// path retries, so a caller that only builds providers may ignore it.
pub fn ensure_fresh(store: &mut AuthStore, path: &Path, now: DateTime<Utc>) -> Result<(), String> {
    if !stored_login(store).is_some_and(|oauth| oauth.expires_within(REFRESH_SLACK, now)) {
        return Ok(());
    }
    let _lock = renewal_lock(path)?;
    let mut current = AuthStore::load_from(path)?;
    if let Some(oauth) = stored_login(&current)
        && oauth.expires_within(REFRESH_SLACK, now)
    {
        let renewed = refresh_credential(oauth)?;
        current.set(PROVIDER_ID, crate::auth::Credential::Oauth(renewed));
        current.save_to(path)?;
    }
    *store = current;
    Ok(())
}

/// Headers every `ChatGPT` backend request carries beside the bearer:
/// the experimental Responses opt-in, the originator the public client
/// is known by, and the account id when the `id_token` carried one.
#[must_use]
pub fn backend_headers(account_id: Option<&str>) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    headers.insert("openai-beta".to_owned(), OPENAI_BETA.to_owned());
    headers.insert("originator".to_owned(), ORIGINATOR.to_owned());
    if let Some(id) = account_id {
        headers.insert("chatgpt-account-id".to_owned(), id.to_owned());
    }
    headers
}

/// The backend's model list under `base`, a [`BACKEND_BASE_URL`] or a
/// proxy of it, asked for the models [`CODEX_CLIENT_VERSION`] may use.
#[must_use]
pub fn models_url(base: &str) -> String {
    format!(
        "{}/models?client_version={CODEX_CLIENT_VERSION}",
        base.trim_end_matches('/')
    )
}

/// Cap on the backend's model list, which carries each model's prompt.
const MAX_MODELS_BODY: u64 = 16 * 1024 * 1024;

/// The fields of a backend model entry kage reads. The rest, prompts
/// and tool settings among them, is dropped before caching.
const MODEL_FIELDS: [&str; 6] = [
    "slug",
    "display_name",
    "context_window",
    "max_context_window",
    "supported_reasoning_levels",
    "input_modalities",
];

/// Where the `ChatGPT` plan's model list is kept between runs:
/// `$XDG_CACHE_HOME/kage/chatgpt-models.json`.
pub fn models_cache_path() -> Result<PathBuf, String> {
    Ok(crate::paths::cache_root()?.join("chatgpt-models.json"))
}

/// Fetch the models the plan behind `login` may use and cache them at
/// `path`, leaving out the ones the backend hides from pickers. Returns
/// how many were cached.
///
/// # Errors
///
/// When the request fails, the backend refuses, or the cache cannot be
/// written.
pub fn refresh_models(login: &OAuthCredential, path: &Path) -> Result<usize, String> {
    let mut request = agent()
        .get(&models_url(BACKEND_BASE_URL))
        .header("authorization", format!("Bearer {}", login.access_token));
    for (name, value) in backend_headers(login.account_id.as_deref()) {
        request = request.header(name, value);
    }
    let mut response = request.call().map_err(|e| format!("model list: {e}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("model list: status {status}"));
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_MODELS_BODY)
        .read_to_string()
        .map_err(|e| format!("model list: {e}"))?;
    let models = trim_models(&body)?;
    let count = models.len();
    let json = serde_json::to_string_pretty(&serde_json::json!({ "models": models }))
        .map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, json).map_err(|e| format!("{}: {e}", partial.display()))?;
    std::fs::rename(&partial, path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(count)
}

/// The visible entries of the backend's model list `body`, cut down to
/// [`MODEL_FIELDS`].
fn trim_models(body: &str) -> Result<Vec<Value>, String> {
    let json: Value =
        serde_json::from_str(body).map_err(|_| "the model list is not JSON".to_owned())?;
    let entries = json["models"]
        .as_array()
        .ok_or_else(|| "the model list has no models".to_owned())?;
    Ok(entries
        .iter()
        .filter(|entry| entry["visibility"].as_str() != Some("hide"))
        .map(|entry| {
            MODEL_FIELDS
                .iter()
                .filter_map(|key| Some(((*key).to_owned(), entry.get(*key)?.clone())))
                .collect::<serde_json::Map<_, _>>()
                .into()
        })
        .collect())
}

/// The models cached at `path`, as `openai` offers them on the
/// `ChatGPT` backend: free, since the plan pays, with the largest
/// context the backend accepts. Empty without a cache, which leaves the
/// catalog to drive the picker.
#[must_use]
pub fn cached_models(path: &Path) -> Vec<kage_provider::ProviderModel> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .map(|json| parse_models(&json))
        .unwrap_or_default()
}

/// The entries of a cached model list. An effort kage does not know is
/// skipped rather than failing the model.
fn parse_models(json: &Value) -> Vec<kage_provider::ProviderModel> {
    let strings = |value: &Value, key: &str| -> Vec<String> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                if key.is_empty() {
                    item.as_str()
                } else {
                    item[key].as_str()
                }
                .map(str::to_owned)
            })
            .collect()
    };
    json["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry["slug"].as_str()?.to_owned();
            let efforts: kage_core::Efforts =
                strings(&entry["supported_reasoning_levels"], "effort")
                    .iter()
                    .filter_map(|effort| kage_core::Effort::parse(effort))
                    .collect();
            Some(kage_provider::ProviderModel {
                name: entry["display_name"].as_str().unwrap_or(&id).to_owned(),
                context: entry["max_context_window"]
                    .as_u64()
                    .or_else(|| entry["context_window"].as_u64()),
                reasoning: if efforts.is_empty() {
                    kage_core::Reasoning::Unknown
                } else {
                    kage_core::Reasoning::Effort {
                        efforts,
                        toggle: false,
                    }
                },
                input: strings(&entry["input_modalities"], "")
                    .iter()
                    .filter_map(|kind| kage_core::Modality::parse(kind))
                    .collect(),
                cost: Some(kage_core::ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: None,
                    cache_write: None,
                }),
                id,
                ..kage_provider::ProviderModel::default()
            })
        })
        .collect()
}

/// The provider-side renewable credential: hands the current token to
/// every request and renews through the auth store at `path` on a 401.
/// Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct ChatGptBearer {
    /// The token of the moment. Held across a renewal, so concurrent
    /// rejections in this process renew once.
    token: Arc<Mutex<String>>,
    path: PathBuf,
}

impl ChatGptBearer {
    /// A bearer source starting from `credential`, usually the one
    /// `providers.rs` just read from the auth store at `path`.
    #[must_use]
    pub fn new(credential: &OAuthCredential, path: PathBuf) -> Self {
        Self {
            token: Arc::new(Mutex::new(credential.access_token.clone())),
            path,
        }
    }
}

impl fmt::Debug for ChatGptBearer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatGptBearer")
            .field("token", &REDACTED)
            .field("path", &self.path)
            .finish()
    }
}

impl kage_provider::BearerSource for ChatGptBearer {
    fn token(&self) -> String {
        self.token
            .lock()
            .map_or_else(|_| String::new(), |token| token.clone())
    }

    fn refresh(&self, rejected: &str) -> Result<String, kage_provider::ProviderError> {
        let mut current = self
            .token
            .lock()
            .map_err(|_| renewal_failed("the token lock is poisoned"))?;
        let _lock = renewal_lock(&self.path).map_err(|e| renewal_failed(&e))?;
        let mut store = AuthStore::load_from(&self.path).map_err(|e| renewal_failed(&e))?;
        let Some(oauth) = stored_login(&store) else {
            return Err(kage_provider::ProviderError::Auth(
                "no ChatGPT login stored; run `kage auth login openai`".to_owned(),
            ));
        };
        // Single-flight: another request or process may have renewed
        // while the rejected token was in flight. Renewing again would
        // reuse a rotated refresh token, which the server refuses.
        if oauth.access_token != rejected {
            current.clone_from(&oauth.access_token);
            return Ok(oauth.access_token.clone());
        }
        let renewed = refresh_credential(oauth).map_err(|e| renewal_failed(&e))?;
        let token = renewed.access_token.clone();
        store.set(PROVIDER_ID, crate::auth::Credential::Oauth(renewed));
        store.save_to(&self.path).map_err(|e| renewal_failed(&e))?;
        current.clone_from(&token);
        Ok(token)
    }
}

/// The renewal error message, with the actionable suffix.
fn renewal_failed(e: &str) -> kage_provider::ProviderError {
    kage_provider::ProviderError::Auth(format!(
        "renewing the ChatGPT login failed: {e}. Run `kage auth login openai`"
    ))
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{Ipv4Addr, TcpStream};

    use super::*;

    /// A JWT with the claims of `json`, signed by nothing (the parser
    /// ignores signatures).
    fn jwt(json: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(json.to_string());
        format!("{header}.{payload}.c2ln")
    }

    #[test]
    fn the_account_id_comes_from_the_auth_claim() {
        let id_token = jwt(&serde_json::json!({
            "email": "dev@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acc-123",
                "chatgpt_plan_type": "plus",
            },
        }));
        assert_eq!(account_id_from_jwt(&id_token).as_deref(), Some("acc-123"));
        assert_eq!(account_id_from_jwt("not-a-jwt"), None);
        assert_eq!(account_id_from_jwt(""), None);
        let without = jwt(&serde_json::json!({ "email": "dev@example.com" }));
        assert_eq!(account_id_from_jwt(&without), None);
    }

    #[test]
    fn the_expiry_comes_from_the_exp_claim() {
        let token = jwt(&serde_json::json!({ "exp": 1_800_000_000i64 }));
        let at = expiry_from_jwt(&token).unwrap();
        assert_eq!(at.timestamp(), 1_800_000_000);
        assert_eq!(expiry_from_jwt("x.y.z"), None);
    }

    #[test]
    fn the_authorize_url_carries_every_codex_parameter() {
        let pkce = Pkce::new().unwrap();
        let expected = Expected::state_only().unwrap();
        let redirect = format!("http://127.0.0.1:{CALLBACK_PORT}{CALLBACK_PATH}");
        let url = url::Url::parse(&authorize_url(&pkce, &expected, &redirect)).unwrap();
        assert!(
            url.as_str()
                .starts_with("https://auth.openai.com/oauth/authorize?"),
            "{url}"
        );
        let query: std::collections::HashMap<String, String> =
            url.query_pairs().into_owned().collect();
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], CLIENT_ID);
        assert_eq!(query["scope"], SCOPES);
        assert_eq!(query["state"], expected.state());
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["code_challenge"], pkce.challenge());
        assert_eq!(query["redirect_uri"], redirect);
        assert_eq!(query["codex_cli_simplified_flow"], "true");
        assert_eq!(query["originator"], ORIGINATOR);
        assert_eq!(query["id_token_add_organizations"], "true");
        assert!(!query.contains_key("resource"), "no RFC 8707 resource");
        assert!(!query.contains_key("code_verifier"));
    }

    #[test]
    fn a_token_answer_becomes_a_stored_credential() {
        let answer = TokenAnswer {
            access_token: jwt(&serde_json::json!({ "exp": 1_800_000_000i64 })),
            refresh_token: Some("refresh-1".to_owned()),
            id_token: Some(jwt(&serde_json::json!({
                "https://api.openai.com/auth": { "chatgpt_account_id": "acc-9" },
            }))),
            expires_in: Some(3600),
            scope: Some("openid".to_owned()),
        };
        let credential = credential_from(&answer, None);
        assert_eq!(credential.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(credential.account_id.as_deref(), Some("acc-9"));
        assert!(credential.expires_at.is_some());
        assert_eq!(credential.scope.as_deref(), Some("openid"));

        let no_expiry = TokenAnswer {
            expires_in: None,
            access_token: answer.access_token.clone(),
            ..TokenAnswer::default()
        };
        let credential = credential_from(&no_expiry, None);
        assert_eq!(
            credential.expires_at.map(|t| t.timestamp()),
            Some(1_800_000_000),
            "expiry falls back to the token's own exp"
        );
        assert_eq!(credential.scope.as_deref(), Some(SCOPES));
    }

    #[test]
    fn the_account_id_falls_back_to_the_access_token() {
        let answer = TokenAnswer {
            access_token: jwt(&serde_json::json!({
                "https://api.openai.com/auth": { "chatgpt_account_id": "acc-at" },
            })),
            ..TokenAnswer::default()
        };
        assert_eq!(
            credential_from(&answer, None).account_id.as_deref(),
            Some("acc-at")
        );
    }

    #[test]
    fn a_refresh_keeps_the_old_refresh_token_and_account_id() {
        let previous = OAuthCredential {
            access_token: "old".to_owned(),
            refresh_token: Some("keep-me".to_owned()),
            expires_at: None,
            scope: None,
            account_id: Some("acc-1".to_owned()),
        };
        let answer = TokenAnswer {
            access_token: "new".to_owned(),
            refresh_token: None,
            id_token: None,
            expires_in: Some(60),
            scope: None,
        };
        let renewed = credential_from(&answer, Some(&previous));
        assert_eq!(renewed.access_token, "new");
        assert_eq!(renewed.refresh_token.as_deref(), Some("keep-me"));
        assert_eq!(renewed.account_id.as_deref(), Some("acc-1"));
    }

    #[test]
    fn a_refused_token_request_reports_the_oauth_error_without_secrets() {
        let body = r#"{"error":"invalid_grant","error_description":"refresh-secret is bad"}"#;
        let err = parse_answer(400, body).unwrap_err();
        assert!(
            matches!(err, ChatGptError::Denied(ref d) if d.contains("invalid_grant")),
            "{err}"
        );
        assert!(!err.to_string().contains("refresh-secret"), "{err}");
        let err = parse_answer(200, r#"{"access_token":""}"#).unwrap_err();
        assert!(matches!(err, ChatGptError::Response(_)), "{err}");
    }

    /// The `ChatGPT` callback path is served, and a pasted redirect
    /// carrying the login's state ends the wait.
    #[test]
    fn the_callback_and_the_paste_both_hand_over_the_code() {
        let expected = Expected::state_only().unwrap();
        let loopback = Loopback::bind_on(&[0], CALLBACK_PATH).unwrap();
        let redirect = loopback.redirect_uri();
        assert!(redirect.ends_with(CALLBACK_PATH), "{redirect}");
        let port = url::Url::parse(&redirect).unwrap().port().unwrap();
        let target = format!(
            "{CALLBACK_PATH}?code=from-browser&state={}",
            expected.state()
        );
        let browser = std::thread::spawn(move || {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            write!(stream, "GET {target} HTTP/1.1\r\nhost: x\r\n\r\n").unwrap();
            let mut out = String::new();
            let _ = stream.read_to_string(&mut out);
            out
        });
        let (_tx, rx) = mpsc::channel();
        let code =
            crate::mcp_auth::wait_for_code(loopback, &expected, &rx, &AtomicBool::new(false))
                .unwrap();
        assert_eq!(code.secret(), "from-browser");
        assert!(browser.join().unwrap().contains("200"));

        let (tx, rx) = mpsc::channel();
        tx.send(format!("{redirect}?code=pasted&state={}", expected.state()))
            .unwrap();
        let loopback = Loopback::bind_on(&[0], CALLBACK_PATH).unwrap();
        let code =
            crate::mcp_auth::wait_for_code(loopback, &expected, &rx, &AtomicBool::new(false))
                .unwrap();
        assert_eq!(code.secret(), "pasted");
    }

    fn stored(path: &Path, credential: OAuthCredential) {
        let mut store = AuthStore::empty();
        store.set_oauth(PROVIDER_ID, credential);
        store.save_to(path).unwrap();
    }

    #[test]
    fn ensure_fresh_leaves_everything_but_a_due_chatgpt_login_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut store = AuthStore::empty();
        store.set_api_key("openai", "sk-key");
        ensure_fresh(&mut store, &path, Utc::now()).unwrap();
        assert_eq!(store.access_token("openai"), Some("sk-key"));

        let mut store = AuthStore::empty();
        store.set_oauth(
            PROVIDER_ID,
            OAuthCredential {
                access_token: "live".to_owned(),
                expires_at: Some(Utc::now() + chrono::Duration::hours(5)),
                ..OAuthCredential::default()
            },
        );
        ensure_fresh(&mut store, &path, Utc::now()).unwrap();
        assert_eq!(store.access_token("openai"), Some("live"));
        assert!(!path.exists(), "nothing due, nothing locked or written");
    }

    /// A due login that another process renewed while this one waited
    /// for the lock is adopted from the file, not renewed again.
    #[test]
    fn ensure_fresh_adopts_a_renewal_made_by_another_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        stored(
            &path,
            OAuthCredential {
                access_token: "renewed-elsewhere".to_owned(),
                expires_at: Some(Utc::now() + chrono::Duration::hours(5)),
                ..OAuthCredential::default()
            },
        );
        let mut store = AuthStore::empty();
        store.set_oauth(
            PROVIDER_ID,
            OAuthCredential {
                access_token: "due".to_owned(),
                expires_at: Some(Utc::now()),
                ..OAuthCredential::default()
            },
        );
        ensure_fresh(&mut store, &path, Utc::now()).unwrap();
        assert_eq!(store.access_token("openai"), Some("renewed-elsewhere"));
    }

    /// A rejection of a token another request already replaced adopts
    /// the stored token instead of rotating the refresh token again.
    #[test]
    fn a_stale_rejection_adopts_the_stored_token_without_renewing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        stored(
            &path,
            OAuthCredential {
                access_token: "renewed-elsewhere".to_owned(),
                refresh_token: Some("rotated".to_owned()),
                ..OAuthCredential::default()
            },
        );
        let bearer = ChatGptBearer::new(
            &OAuthCredential {
                access_token: "stale".to_owned(),
                ..OAuthCredential::default()
            },
            path,
        );
        let token = kage_provider::BearerSource::refresh(&bearer, "stale").unwrap();
        assert_eq!(token, "renewed-elsewhere");
        assert_eq!(
            kage_provider::BearerSource::token(&bearer),
            "renewed-elsewhere"
        );
    }

    #[test]
    fn the_bearer_hands_out_its_token_and_redacts_itself() {
        let credential = OAuthCredential {
            access_token: "at-1".to_owned(),
            ..OAuthCredential::default()
        };
        let bearer = ChatGptBearer::new(&credential, PathBuf::from("auth.json"));
        assert_eq!(kage_provider::BearerSource::token(&bearer), "at-1");
        assert!(format!("{bearer:?}").contains(REDACTED));
        assert!(!format!("{bearer:?}").contains("at-1"));
    }

    #[test]
    fn the_models_url_asks_for_the_codex_version() {
        assert_eq!(
            models_url("https://chatgpt.com/backend-api/codex/"),
            format!(
                "https://chatgpt.com/backend-api/codex/models?client_version={CODEX_CLIENT_VERSION}"
            )
        );
    }

    /// The cache keeps the visible models and only the fields kage
    /// reads, and reads back as free models with the backend's largest
    /// context, the efforts kage knows, and their input kinds.
    #[test]
    fn the_plan_model_list_caches_visible_models_for_free() {
        let body = serde_json::json!({"models": [
            {
                "slug": "gpt-6-sol",
                "display_name": "GPT-6-Sol",
                "context_window": 272_000,
                "max_context_window": 872_000,
                "supported_reasoning_levels": [
                    {"effort": "low", "description": "fast"},
                    {"effort": "ultra"},
                    {"effort": "turbo"}
                ],
                "input_modalities": ["text", "image"],
                "visibility": "list",
                "base_instructions": "a long prompt"
            },
            {"slug": "gpt-reserve", "visibility": "hide"},
            {"slug": "plain", "context_window": 128_000}
        ]});
        let trimmed = trim_models(&body.to_string()).unwrap();
        assert_eq!(trimmed.len(), 2, "the hidden model is left out");
        assert!(trimmed[0].get("base_instructions").is_none());
        assert!(trimmed[0].get("visibility").is_none());

        let models = parse_models(&serde_json::json!({ "models": trimmed }));
        let sol = &models[0];
        assert_eq!(sol.id, "gpt-6-sol");
        assert_eq!(sol.name, "GPT-6-Sol");
        assert_eq!(sol.context, Some(872_000));
        assert_eq!(
            sol.reasoning,
            kage_core::Reasoning::Effort {
                efforts: kage_core::Efforts::of(&[
                    kage_core::Effort::Low,
                    kage_core::Effort::Ultra
                ]),
                toggle: false,
            }
        );
        assert_eq!(
            sol.input,
            kage_core::Modalities::of(&[kage_core::Modality::Text, kage_core::Modality::Image])
        );
        assert_eq!(sol.cost.map(|c| (c.input, c.output)), Some((0.0, 0.0)));

        let plain = &models[1];
        assert_eq!(plain.name, "plain");
        assert_eq!(plain.context, Some(128_000));
        assert_eq!(plain.reasoning, kage_core::Reasoning::Unknown);
    }

    #[test]
    fn a_missing_or_broken_cache_offers_no_models() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chatgpt-models.json");
        assert!(cached_models(&path).is_empty());
        std::fs::write(&path, "not json").unwrap();
        assert!(cached_models(&path).is_empty());
        assert!(trim_models("{}").is_err());
    }
}
