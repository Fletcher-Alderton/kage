//! OAuth 2.1 for remote MCP servers.
//!
//! The public client side of the MCP authorization spec (revisions
//! 2025-06-18 and 2025-11-25). [`discover`] probes the server without a
//! token and reads the `WWW-Authenticate` challenge, follows it (or the
//! well-known fallbacks) to the protected resource metadata (RFC 9728),
//! and loads the metadata of the authorization server it names
//! (RFC 8414, then `OpenID` Connect discovery). kage then uses a
//! configured client id or registers itself ([`register`], RFC 7591),
//! sends the user to [`authorize_url`] with PKCE (S256 only), a random
//! `state` and the `resource` indicator (RFC 8707), receives the
//! redirect on a one-shot [`Loopback`] listener or as a pasted URL
//! ([`parse_redirect`]), and trades the code for tokens ([`exchange`],
//! later [`refresh`]).
//!
//! Nothing here stores anything. The host owns token storage and hands
//! the HTTP transport a [`TokenSource`].
//!
//! Every endpoint of the authorization server must use HTTPS, except on
//! loopback hosts, and redirects are never followed. Every type holding
//! a secret (a verifier, a code, a token, the state) prints `***` in its
//! `Debug` output, and no error message carries one.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use kage_tools::ssrf;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::{Host, Url};

use crate::server::PROTOCOL_VERSION;

/// How long a login waits for the browser to come back.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Bound on one request to the MCP or authorization server.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on a metadata, registration or token response body.
const MAX_BODY: u64 = 1024 * 1024;

/// Cap on the request line and headers the loopback reads.
const MAX_REQUEST_HEAD: u64 = 16 * 1024;

/// How long the loopback waits for a connected browser to send its
/// request before moving on.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the loopback checks its deadline and cancel flag.
const POLL: Duration = Duration::from_millis(25);

/// What `Debug` prints in place of a secret.
const REDACTED: &str = "***";

/// Bearer tokens for HTTP servers. The host owns storage and policy.
pub trait TokenSource: Send + Sync {
    /// Token to send to `url`, refreshed first when it expires soon.
    fn bearer(&self, url: &str) -> Option<String>;
    /// `url` rejected `token`. Refresh and return the new token,
    /// unless the stored token already differs from `token` - another
    /// request or process refreshed while ours was in flight - in
    /// which case return that one, so one rejection rotates once no
    /// matter how many requests raced. `None` when the user has to
    /// log in again.
    fn rejected(&self, url: &str, token: &str) -> Option<String>;
}

/// A failed discovery, registration, redirect or token request. The
/// messages name URLs and error codes, never a secret.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// A configured or advertised URL does not parse as http(s).
    #[error("`{0}` is not a valid http(s) URL")]
    Url(String),
    /// An authorization endpoint uses plain http on a remote host.
    #[error("`{0}` must use https (plain http is allowed only on loopback hosts)")]
    Insecure(String),
    /// A request could not be sent or its answer not read.
    #[error("{url}: {detail}")]
    Http {
        /// The URL that failed.
        url: String,
        /// The transport error.
        detail: String,
    },
    /// The metadata chain is missing or inconsistent.
    #[error("authorization discovery: {0}")]
    Discovery(String),
    /// The authorization server does not offer PKCE with S256.
    #[error("authorization server `{0}` does not support PKCE with S256")]
    NoPkce(String),
    /// No client id is configured and the server cannot register one.
    #[error(
        "authorization server `{0}` offers no dynamic client registration. \
         Set `client_id` under `[mcp.servers.<name>.oauth]`"
    )]
    NoRegistration(String),
    /// A registration or token request was refused.
    #[error("{what} failed with status {status}: {error}")]
    Rejected {
        /// Which request failed.
        what: &'static str,
        /// The HTTP status.
        status: u16,
        /// The OAuth `error` code of the answer, if any.
        error: String,
    },
    /// A registration or token answer lacks a required field.
    #[error("unexpected response: {0}")]
    Response(&'static str),
    /// The redirect back from the browser is unusable.
    #[error("the authorization redirect {0}")]
    Redirect(&'static str),
    /// The redirect carried another `state` than this login sent.
    #[error("the authorization redirect carried the wrong state. Start the login again")]
    State,
    /// The user or the server refused the authorization.
    #[error("the authorization server refused: {0}")]
    Denied(String),
    /// The browser did not come back in time.
    #[error("timed out waiting for the browser to return")]
    Timeout,
    /// The host stopped the wait (for example after a pasted redirect).
    #[error("the login was cancelled")]
    Cancelled,
    /// A local failure: the listener or the random source.
    #[error("{0}")]
    Local(String),
}

/// What discovery learned about one MCP server and its authorization
/// server. Holds no secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
    /// Canonical URL of the MCP server, sent as the `resource`
    /// indicator and used to key stored tokens.
    pub resource: String,
    /// Issuer identifier of the authorization server.
    pub issuer: String,
    /// Where the browser goes to authorize kage.
    pub authorization_endpoint: String,
    /// Where codes and refresh tokens become access tokens.
    pub token_endpoint: String,
    /// Dynamic client registration endpoint, when offered.
    pub registration_endpoint: Option<String>,
    /// Scope to request: the challenge's `scope`, else the resource's
    /// `scopes_supported`, else none.
    pub scope: Option<String>,
    /// Whether the server promises `iss` on every redirect (RFC 9207).
    pub iss_required: bool,
}

/// The canonical form of an MCP server URL: lowercase scheme and host,
/// no default port, no user info, no fragment and no trailing slash.
///
/// # Errors
///
/// [`OAuthError::Url`] when `url` is not an http(s) URL with a host.
pub fn canonical_url(url: &str) -> Result<String, OAuthError> {
    let parsed = http_url(url)?;
    let mut out = parsed.origin().ascii_serialization();
    out.push_str(parsed.path().trim_end_matches('/'));
    if let Some(query) = parsed.query() {
        out.push('?');
        out.push_str(query);
    }
    Ok(out)
}

/// Discover how to authorize against the MCP server at `server_url`.
///
/// # Errors
///
/// [`OAuthError::Discovery`] when the metadata chain is missing or
/// inconsistent, [`OAuthError::NoPkce`] when the authorization server
/// lacks S256, [`OAuthError::Insecure`] for a plain http endpoint on a
/// remote host, and [`OAuthError::Http`] when a server is unreachable.
pub fn discover(server_url: &str) -> Result<Discovery, OAuthError> {
    let resource = canonical_url(server_url)?;
    let agent = agent();
    let challenge = probe(&agent, server_url)?;
    let metadata = protected_resource(&agent, &resource, challenge.get("resource_metadata"))?;
    let issuer = metadata
        .get("authorization_servers")
        .and_then(Value::as_array)
        .and_then(|servers| servers.first())
        .and_then(Value::as_str)
        .ok_or_else(|| {
            OAuthError::Discovery(format!(
                "the metadata of `{resource}` names no authorization server"
            ))
        })?
        .to_owned();
    let server = authorization_server(&agent, &issuer)?;
    let s256 = server
        .get("code_challenge_methods_supported")
        .and_then(Value::as_array)
        .is_some_and(|methods| methods.iter().any(|m| m == "S256"));
    if !s256 {
        return Err(OAuthError::NoPkce(issuer));
    }
    let registration_endpoint = match server.get("registration_endpoint") {
        Some(_) => Some(endpoint(&server, "registration_endpoint")?),
        None => None,
    };
    let scope = challenge
        .get("scope")
        .cloned()
        .or_else(|| {
            let scopes = metadata.get("scopes_supported")?.as_array()?;
            let names: Vec<&str> = scopes.iter().filter_map(Value::as_str).collect();
            Some(names.join(" "))
        })
        .filter(|scope| !scope.is_empty());
    Ok(Discovery {
        resource,
        authorization_endpoint: endpoint(&server, "authorization_endpoint")?,
        token_endpoint: endpoint(&server, "token_endpoint")?,
        registration_endpoint,
        scope,
        iss_required: server
            .get("authorization_response_iss_parameter_supported")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        issuer,
    })
}

/// Register kage as a public client (RFC 7591) that redirects to
/// `redirect_uri`, returning the issued client id.
///
/// # Errors
///
/// [`OAuthError::NoRegistration`] when the server offers no
/// registration endpoint, [`OAuthError::Rejected`] when it refuses, and
/// [`OAuthError::Response`] when the answer has no client id.
pub fn register(discovery: &Discovery, redirect_uri: &str) -> Result<String, OAuthError> {
    let Some(endpoint) = &discovery.registration_endpoint else {
        return Err(OAuthError::NoRegistration(discovery.issuer.clone()));
    };
    let url = secure_url(endpoint)?;
    vet(&url)?;
    let body = serde_json::json!({
        "client_name": "kage",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let mut response = agent()
        .post(url.as_str())
        .header("accept", "application/json")
        .send_json(&body)
        .map_err(|e| http_error(&url, &e))?;
    let status = response.status().as_u16();
    let answer = read_json(&mut response);
    if !(200..300).contains(&status) {
        return Err(rejected("client registration", status, &answer));
    }
    answer
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or(OAuthError::Response("the registration has no client_id"))
}

/// A PKCE verifier and its S256 challenge.
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    /// A fresh verifier of 43 unreserved characters (32 random bytes)
    /// and its challenge, `BASE64URL(SHA256(verifier))`.
    ///
    /// # Errors
    ///
    /// [`OAuthError::Local`] when the system has no random source.
    pub fn new() -> Result<Self, OAuthError> {
        let verifier = random_token()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Ok(Self {
            verifier,
            challenge,
        })
    }

    /// The S256 challenge the authorize URL carries.
    #[must_use]
    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    /// The verifier the code exchange proves possession with.
    #[must_use]
    pub fn verifier(&self) -> &str {
        &self.verifier
    }
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &REDACTED)
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// What the authorization redirect must carry to be accepted: this
/// login's random `state`, and the issuer's `iss` when present or
/// promised. An `Expected` without an issuer checks the state alone.
pub struct Expected {
    state: String,
    issuer: String,
    iss_required: bool,
}

impl Expected {
    /// A fresh random state for one login against `discovery`.
    ///
    /// # Errors
    ///
    /// [`OAuthError::Local`] when the system has no random source.
    pub fn new(discovery: &Discovery) -> Result<Self, OAuthError> {
        Ok(Self {
            state: random_token()?,
            issuer: discovery.issuer.clone(),
            iss_required: discovery.iss_required,
        })
    }

    /// A fresh random state for a login whose issuer is fixed by the
    /// host rather than discovered, so the redirect's `iss` is not
    /// checked.
    ///
    /// # Errors
    ///
    /// [`OAuthError::Local`] when the system has no random source.
    pub fn state_only() -> Result<Self, OAuthError> {
        Ok(Self {
            state: random_token()?,
            issuer: String::new(),
            iss_required: false,
        })
    }

    /// The `state` the authorize URL carries.
    #[must_use]
    pub fn state(&self) -> &str {
        &self.state
    }

    /// Check the query of a redirect and return its code.
    fn accept(&self, query: &str) -> Result<AuthCode, OAuthError> {
        let (mut state, mut code, mut iss, mut error) = (None, None, None, None);
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            let slot = match key.as_ref() {
                "state" => &mut state,
                "code" => &mut code,
                "iss" => &mut iss,
                "error" => &mut error,
                _ => continue,
            };
            *slot = Some(value.into_owned());
        }
        if state.as_deref() != Some(self.state.as_str()) {
            return Err(OAuthError::State);
        }
        match iss {
            _ if self.issuer.is_empty() => {}
            Some(iss) if iss != self.issuer => {
                return Err(OAuthError::Redirect("names another issuer"));
            }
            None if self.iss_required => return Err(OAuthError::Redirect("has no `iss`")),
            _ => {}
        }
        if let Some(error) = error {
            return Err(OAuthError::Denied(error_code(&error)));
        }
        code.filter(|code| !code.is_empty())
            .map(AuthCode)
            .ok_or(OAuthError::Redirect("has no code"))
    }
}

impl fmt::Debug for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Expected")
            .field("state", &REDACTED)
            .field("issuer", &self.issuer)
            .field("iss_required", &self.iss_required)
            .finish()
    }
}

/// An authorization code from the redirect, redeemed by [`exchange`].
pub struct AuthCode(String);

impl AuthCode {
    /// The code itself, for a host that redeems it at its own token
    /// endpoint.
    #[must_use]
    pub fn secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AuthCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AuthCode").field(&REDACTED).finish()
    }
}

/// The URL the browser opens to authorize kage: the authorization
/// endpoint with the S256 challenge, the state, the `resource`
/// indicator and, when given, `scope`.
///
/// # Errors
///
/// [`OAuthError::Url`] or [`OAuthError::Insecure`] for a bad
/// authorization endpoint.
pub fn authorize_url(
    discovery: &Discovery,
    client_id: &str,
    redirect_uri: &str,
    pkce: &Pkce,
    expected: &Expected,
    scope: Option<&str>,
) -> Result<String, OAuthError> {
    let mut url = secure_url(&discovery.authorization_endpoint)?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &expected.state)
            .append_pair("resource", &discovery.resource);
        if let Some(scope) = scope.filter(|scope| !scope.is_empty()) {
            query.append_pair("scope", scope);
        }
    }
    Ok(url.into())
}

/// A one-shot redirect listener on 127.0.0.1, serving one callback
/// path.
#[derive(Debug)]
pub struct Loopback {
    listener: TcpListener,
    port: u16,
    path: &'static str,
}

impl Loopback {
    /// Bind 127.0.0.1 on a port the system picks, serving `/callback`.
    ///
    /// # Errors
    ///
    /// [`OAuthError::Local`] when the socket cannot be bound.
    pub fn bind() -> Result<Self, OAuthError> {
        Self::bind_on(&[0], "/callback")
    }

    /// Bind 127.0.0.1 on the first free port of `ports`, serving
    /// `path`, for a client whose redirect URIs are registered on fixed
    /// ports. The standard listener sets `SO_REUSEADDR` on unix, so a
    /// second login rebinds while the last one's connections sit in
    /// `TIME_WAIT`.
    ///
    /// # Errors
    ///
    /// [`OAuthError::Local`] when no port of `ports` can be bound.
    pub fn bind_on(ports: &[u16], path: &'static str) -> Result<Self, OAuthError> {
        let mut last = None;
        for &port in ports {
            match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).and_then(|listener| {
                let port = listener.local_addr()?.port();
                Ok(Self {
                    listener,
                    port,
                    path,
                })
            }) {
                Ok(loopback) => return Ok(loopback),
                Err(e) => last = Some(e),
            }
        }
        let ports: Vec<String> = ports.iter().map(u16::to_string).collect();
        Err(OAuthError::Local(format!(
            "loopback listener on port {}: {}",
            ports.join(" or "),
            last.map_or_else(|| "no port to bind".to_owned(), |e| e.to_string())
        )))
    }

    /// The redirect URI to register and send: the callback path on this
    /// listener.
    #[must_use]
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.path)
    }

    /// Serve until the first request for the callback path and return
    /// its code, answering any other path with 404. The listener closes
    /// when this returns.
    ///
    /// # Errors
    ///
    /// [`OAuthError::State`] and the other redirect errors of
    /// [`parse_redirect`] for a bad callback, [`OAuthError::Timeout`]
    /// after `timeout`, and [`OAuthError::Cancelled`] once `cancel` is
    /// set.
    pub fn wait(
        self,
        expected: &Expected,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<AuthCode, OAuthError> {
        let local = |e: std::io::Error| OAuthError::Local(format!("loopback listener: {e}"));
        self.listener.set_nonblocking(true).map_err(local)?;
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(OAuthError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(OAuthError::Timeout);
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(result) = serve_callback(&stream, self.path, expected) {
                        return result;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
                Err(e) => return Err(local(e)),
            }
        }
    }
}

/// Answer one loopback connection. `None` means it was not the
/// callback (another path, or no readable request), so the wait goes on.
fn serve_callback(
    mut stream: &TcpStream,
    callback: &str,
    expected: &Expected,
) -> Option<Result<AuthCode, OAuthError>> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok()?;
    let target = read_target(stream)?;
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    if path != callback {
        respond(&mut stream, "404 Not Found", "Not found.");
        return None;
    }
    let result = expected.accept(query);
    if result.is_ok() {
        respond(
            &mut stream,
            "200 OK",
            "kage is authorized. You can close this tab.",
        );
    } else {
        respond(
            &mut stream,
            "400 Bad Request",
            "kage could not finish the login. Return to the terminal.",
        );
    }
    Some(result)
}

/// Read the request line and headers, returning the request target.
fn read_target(stream: &TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.take(MAX_REQUEST_HEAD));
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let target = line.split_whitespace().nth(1)?.to_owned();
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => break,
            Ok(_) if header.trim_end().is_empty() => break,
            Ok(_) => {}
        }
    }
    Some(target)
}

/// Write a small HTML page and close.
fn respond(stream: &mut &TcpStream, status: &str, message: &str) {
    let body = format!("<!doctype html><title>kage</title><p>{message}</p>\n");
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\n\
         content-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// Read the code from a redirect URL the user pasted (the address the
/// browser was sent to), for machines whose browser cannot reach the
/// loopback listener.
///
/// # Errors
///
/// [`OAuthError::Redirect`] for input that is not a redirect URL,
/// [`OAuthError::State`] for another login's state, and
/// [`OAuthError::Denied`] when the redirect carries an error.
pub fn parse_redirect(pasted: &str, expected: &Expected) -> Result<AuthCode, OAuthError> {
    let url = Url::parse(pasted.trim()).map_err(|_| OAuthError::Redirect("is not a URL"))?;
    expected.accept(url.query().unwrap_or_default())
}

/// Tokens from the token endpoint.
#[derive(Clone, PartialEq, Eq)]
pub struct Tokens {
    /// Sent as `Authorization: Bearer`.
    pub access_token: String,
    /// Trades for a new access token, when issued.
    pub refresh_token: Option<String>,
    /// When the access token stops working, when the server said.
    pub expires_at: Option<SystemTime>,
    /// The scope granted, when the server said.
    pub scope: Option<String>,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &REDACTED)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| REDACTED),
            )
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Redeem an authorization code with its PKCE verifier.
///
/// # Errors
///
/// [`OAuthError::Rejected`] when the token endpoint refuses, and
/// [`OAuthError::Response`] for an answer without a bearer token.
pub fn exchange(
    discovery: &Discovery,
    client_id: &str,
    redirect_uri: &str,
    code: &AuthCode,
    pkce: &Pkce,
) -> Result<Tokens, OAuthError> {
    token_request(
        &discovery.token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code.0),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", &pkce.verifier),
            ("resource", &discovery.resource),
        ],
    )
}

/// Trade `refresh_token` for new tokens. An answer without a new
/// refresh token keeps the old one.
///
/// # Errors
///
/// As [`exchange`]. A refused refresh means the user logs in again.
pub fn refresh(
    token_endpoint: &str,
    client_id: &str,
    resource: &str,
    refresh_token: &str,
) -> Result<Tokens, OAuthError> {
    let mut tokens = token_request(
        token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("resource", resource),
        ],
    )?;
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = Some(refresh_token.to_owned());
    }
    Ok(tokens)
}

/// POST `form` to the token endpoint and parse the answer.
fn token_request(endpoint: &str, form: &[(&str, &str)]) -> Result<Tokens, OAuthError> {
    let url = secure_url(endpoint)?;
    vet(&url)?;
    let mut response = agent()
        .post(url.as_str())
        .header("accept", "application/json")
        .send_form(form.iter().copied())
        .map_err(|e| http_error(&url, &e))?;
    let status = response.status().as_u16();
    let answer = read_json(&mut response);
    if !(200..300).contains(&status) {
        return Err(rejected("token request", status, &answer));
    }
    let text = |key: &str| {
        answer
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    if text("token_type").is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer")) {
        return Err(OAuthError::Response("the token is not a bearer token"));
    }
    let access_token =
        text("access_token").ok_or(OAuthError::Response("the answer has no access_token"))?;
    let expires_in = answer.get("expires_in").and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    });
    Ok(Tokens {
        access_token,
        refresh_token: text("refresh_token"),
        expires_at: expires_in
            .and_then(|secs| SystemTime::now().checked_add(Duration::from_secs(secs))),
        scope: text("scope"),
    })
}

/// The agent for discovery, registration and token requests: statuses
/// are read rather than raised, and redirects are not followed, so an
/// HTTPS endpoint cannot bounce a request to plain http.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .new_agent()
}

/// POST an unauthenticated `initialize` and return the parameters of
/// the Bearer challenge of a 401 (empty for any other answer).
fn probe(agent: &ureq::Agent, server_url: &str) -> Result<BTreeMap<String, String>, OAuthError> {
    let url = http_url(server_url)?;
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "kage", "version": kage_core::build::version() },
        },
    });
    let response = agent
        .post(url.as_str())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .send(body.to_string().as_bytes())
        .map_err(|e| http_error(&url, &e))?;
    if response.status().as_u16() != 401 {
        return Ok(BTreeMap::new());
    }
    Ok(response
        .headers()
        .get_all("www-authenticate")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(bearer_params)
        .collect())
}

/// The auth-params of the `Bearer` challenges in one `WWW-Authenticate`
/// value (RFC 9110 section 11.6.1), with lowercase names.
fn bearer_params(header: &str) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    let mut bearer = false;
    let mut rest = header;
    loop {
        rest = rest.trim_start_matches([' ', '\t', ',']);
        if rest.is_empty() {
            return params;
        }
        let end = rest
            .find(|c: char| c == '=' || c == ',' || c.is_ascii_whitespace())
            .unwrap_or(rest.len());
        if end == 0 {
            rest = &rest[1..];
            continue;
        }
        let token = &rest[..end];
        let after = rest[end..].trim_start();
        let Some(value_start) = after.strip_prefix('=') else {
            bearer = token.eq_ignore_ascii_case("bearer");
            rest = after;
            continue;
        };
        let value_start = value_start.trim_start();
        let (value, tail) = if let Some(quoted) = value_start.strip_prefix('"') {
            let mut value = String::new();
            let mut chars = quoted.char_indices();
            let mut tail = "";
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => value.extend(chars.next().map(|(_, c)| c)),
                    '"' => {
                        tail = &quoted[i + 1..];
                        break;
                    }
                    c => value.push(c),
                }
            }
            (value, tail)
        } else {
            let end = value_start
                .find(|c: char| c == ',' || c.is_ascii_whitespace())
                .unwrap_or(value_start.len());
            (value_start[..end].to_owned(), &value_start[end..])
        };
        if bearer {
            params.insert(token.to_ascii_lowercase(), value);
        }
        rest = tail;
    }
}

/// Load the protected resource metadata of `resource`: from the
/// challenge's `resource_metadata` when given, else from the
/// path-aware well-known URL and then the root one.
fn protected_resource(
    agent: &ureq::Agent,
    resource: &str,
    from_challenge: Option<&String>,
) -> Result<Value, OAuthError> {
    let candidates = match from_challenge {
        Some(url) => vec![url.clone()],
        None => well_known(resource, "oauth-protected-resource")?,
    };
    for candidate in &candidates {
        let url = secure_url(candidate)?;
        let Some(metadata) = get_json(agent, &url)? else {
            continue;
        };
        if let Some(named) = metadata.get("resource").and_then(Value::as_str)
            && !covers(named, resource)
        {
            return Err(OAuthError::Discovery(format!(
                "the metadata at `{url}` is for `{named}`, not `{resource}`"
            )));
        }
        return Ok(metadata);
    }
    Err(OAuthError::Discovery(format!(
        "`{resource}` publishes no protected resource metadata"
    )))
}

/// Whether the metadata `resource` names `resource` or a path prefix of
/// it on the same origin.
fn covers(named: &str, resource: &str) -> bool {
    let Ok(named) = canonical_url(named) else {
        return false;
    };
    resource == named
        || resource
            .strip_prefix(&named)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `<origin>/.well-known/<suffix><path>` when `url` has a path, then
/// `<origin>/.well-known/<suffix>`.
fn well_known(url: &str, suffix: &str) -> Result<Vec<String>, OAuthError> {
    let parsed = http_url(url)?;
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path().trim_end_matches('/');
    let mut out = Vec::new();
    if !path.is_empty() {
        out.push(format!("{origin}/.well-known/{suffix}{path}"));
    }
    out.push(format!("{origin}/.well-known/{suffix}"));
    Ok(out)
}

/// Load the metadata of the authorization server `issuer`, trying
/// RFC 8414 before `OpenID` Connect discovery as the MCP spec orders
/// them, and check that it names the same issuer.
fn authorization_server(agent: &ureq::Agent, issuer: &str) -> Result<Value, OAuthError> {
    let parsed = secure_url(issuer)?;
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path().trim_end_matches('/');
    let candidates = if path.is_empty() {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server"),
            format!("{origin}/.well-known/openid-configuration"),
        ]
    } else {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
            format!("{origin}/.well-known/openid-configuration{path}"),
            format!("{origin}{path}/.well-known/openid-configuration"),
        ]
    };
    for candidate in &candidates {
        let url = secure_url(candidate)?;
        let Some(metadata) = get_json(agent, &url)? else {
            continue;
        };
        let named = metadata
            .get("issuer")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if named.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err(OAuthError::Discovery(format!(
                "the metadata at `{url}` names issuer `{named}`, not `{issuer}`"
            )));
        }
        return Ok(metadata);
    }
    Err(OAuthError::Discovery(format!(
        "authorization server `{issuer}` publishes no metadata"
    )))
}

/// The endpoint `key` of authorization server metadata, which must be
/// HTTPS or loopback.
fn endpoint(metadata: &Value, key: &str) -> Result<String, OAuthError> {
    let value = metadata
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| OAuthError::Discovery(format!("the server metadata has no `{key}`")))?;
    let url = secure_url(value)?;
    vet(&url)?;
    Ok(url.into())
}

/// Resolve `url` and refuse a host that answers on a non-routable
/// address: the metadata chain is served by the remote server, so a
/// hostile one could otherwise point the fetches at internal services.
/// Loopback stays allowed so local dev servers keep working.
fn vet(url: &Url) -> Result<(), OAuthError> {
    ssrf::check_allowing(url, std::net::IpAddr::is_loopback)
        .map_err(|e| OAuthError::Discovery(e.to_string()))
}

/// GET a JSON object. `None` for a non-success status or a body that is
/// not a JSON object, so discovery can try the next candidate.
fn get_json(agent: &ureq::Agent, url: &Url) -> Result<Option<Value>, OAuthError> {
    vet(url)?;
    let mut response = agent
        .get(url.as_str())
        .header("accept", "application/json")
        .call()
        .map_err(|e| http_error(url, &e))?;
    if !response.status().is_success() {
        return Ok(None);
    }
    Ok(Some(read_json(&mut response)).filter(Value::is_object))
}

/// The body as JSON, or `Null` when it is not JSON or too large.
fn read_json(response: &mut ureq::http::Response<ureq::Body>) -> Value {
    response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

/// A refused request, with the OAuth `error` code of its answer.
fn rejected(what: &'static str, status: u16, answer: &Value) -> OAuthError {
    let error = answer
        .get("error")
        .and_then(Value::as_str)
        .map_or_else(|| "no error code".to_owned(), error_code);
    OAuthError::Rejected {
        what,
        status,
        error,
    }
}

/// An OAuth error code cut to its RFC 6749 character set and a short
/// length, so a server cannot smuggle arbitrary text into a message.
#[must_use]
pub fn error_code(error: &str) -> String {
    error
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .take(64)
        .collect()
}

fn http_error(url: &Url, error: &ureq::Error) -> OAuthError {
    OAuthError::Http {
        url: url.to_string(),
        detail: error.to_string(),
    }
}

/// Parse an http(s) URL with a host.
fn http_url(url: &str) -> Result<Url, OAuthError> {
    Url::parse(url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
        .ok_or_else(|| OAuthError::Url(url.to_owned()))
}

/// Parse an authorization server URL, which must be HTTPS unless it is
/// on a loopback host.
fn secure_url(url: &str) -> Result<Url, OAuthError> {
    let parsed = http_url(url)?;
    let loopback = match parsed.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if parsed.scheme() == "https" || loopback {
        Ok(parsed)
    } else {
        Err(OAuthError::Insecure(url.to_owned()))
    }
}

/// 32 random bytes as 43 unreserved characters.
fn random_token() -> Result<String, OAuthError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| OAuthError::Local(format!("random source: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::test_support::{FakeServer, Reply, StaticTokens, serve};

    /// RFC 8414 metadata for the issuer `base` with its endpoints on
    /// `base`, merged with `extra`.
    fn as_metadata(base: &str, extra: &Value) -> Value {
        let mut metadata = serde_json::json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "registration_endpoint": format!("{base}/register"),
            "code_challenge_methods_supported": ["S256"],
        });
        if let (Some(target), Some(extra)) = (metadata.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        metadata
    }

    /// The 401 a protected MCP endpoint answers without a valid token.
    fn challenge(base: &str) -> Reply {
        Reply::status(401).header(
            "www-authenticate",
            &format!(r#"Bearer resource_metadata="{base}/prm", scope="read""#),
        )
    }

    /// A resource server and authorization server in one: `/mcp`
    /// challenges, `/prm` names the issuer, both discovery documents
    /// exist (the `OpenID` one with another token endpoint), and
    /// `/register` and `/token` answer.
    fn full_server() -> FakeServer {
        serve(
            |request, base| match (request.method.as_str(), request.path.as_str()) {
                ("POST", "/mcp" | "/mcp/") => challenge(base),
                ("GET", "/prm") => Reply::json(
                    200,
                    &serde_json::json!({
                        "resource": format!("{base}/mcp"),
                        "authorization_servers": [base],
                        "scopes_supported": ["ignored"],
                    }),
                ),
                ("GET", "/.well-known/oauth-authorization-server") => {
                    Reply::json(200, &as_metadata(base, &serde_json::json!({})))
                }
                ("GET", "/.well-known/openid-configuration") => Reply::json(
                    200,
                    &as_metadata(
                        base,
                        &serde_json::json!({ "token_endpoint": format!("{base}/oidc-token") }),
                    ),
                ),
                ("POST", "/register") => {
                    Reply::json(201, &serde_json::json!({ "client_id": "kage-client" }))
                }
                ("POST", "/token") => Reply::json(
                    200,
                    &serde_json::json!({
                        "access_token": "access-secret-1",
                        "refresh_token": "refresh-secret-1",
                        "token_type": "Bearer",
                        "expires_in": 3600,
                        "scope": "read",
                    }),
                ),
                _ => Reply::status(404),
            },
        )
    }

    #[test]
    fn discovery_follows_the_challenge_and_prefers_rfc8414() {
        let server = full_server();
        let base = &server.base;
        let found = discover(&format!("{base}/mcp/")).unwrap();
        assert_eq!(
            found,
            Discovery {
                resource: format!("{base}/mcp"),
                issuer: base.clone(),
                authorization_endpoint: format!("{base}/authorize"),
                token_endpoint: format!("{base}/token"),
                registration_endpoint: Some(format!("{base}/register")),
                scope: Some("read".to_owned()),
                iss_required: false,
            }
        );
        assert_eq!(
            server.paths(),
            [
                "POST /mcp/",
                "GET /prm",
                "GET /.well-known/oauth-authorization-server"
            ]
        );
        let probe = server.find("POST", "/mcp/").unwrap();
        assert!(probe.header("authorization").is_none());
    }

    #[test]
    fn discovery_falls_back_to_the_well_known_paths() {
        let server = serve(|request, base| match request.path.as_str() {
            "/mcp" => Reply::status(401).header("www-authenticate", "Bearer"),
            "/.well-known/oauth-protected-resource" => Reply::json(
                200,
                &serde_json::json!({
                    "resource": base,
                    "authorization_servers": [format!("{base}/tenant")],
                    "scopes_supported": ["a", "b"],
                }),
            ),
            "/.well-known/openid-configuration/tenant" => Reply::json(
                200,
                &as_metadata(
                    &format!("{base}/tenant"),
                    &serde_json::json!({ "authorization_response_iss_parameter_supported": true }),
                ),
            ),
            _ => Reply::status(404),
        });
        let base = &server.base;
        let found = discover(&format!("{base}/mcp")).unwrap();
        assert_eq!(found.issuer, format!("{base}/tenant"));
        assert_eq!(found.scope.as_deref(), Some("a b"));
        assert!(found.iss_required);
        assert_eq!(
            server.paths(),
            [
                "POST /mcp",
                "GET /.well-known/oauth-protected-resource/mcp",
                "GET /.well-known/oauth-protected-resource",
                "GET /.well-known/oauth-authorization-server/tenant",
                "GET /.well-known/openid-configuration/tenant",
            ]
        );
    }

    /// A server whose authorization server metadata is `metadata(base)`.
    fn server_with(metadata: impl Fn(&str) -> Value + Send + Sync + 'static) -> FakeServer {
        serve(move |request, base| match request.path.as_str() {
            "/mcp" => challenge(base),
            "/prm" => Reply::json(
                200,
                &serde_json::json!({ "resource": format!("{base}/mcp"), "authorization_servers": [base] }),
            ),
            "/.well-known/oauth-authorization-server" => Reply::json(200, &metadata(base)),
            _ => Reply::status(404),
        })
    }

    #[test]
    fn discovery_refuses_a_server_without_s256() {
        for methods in [serde_json::json!(["plain"]), Value::Null] {
            let server = server_with(move |base| {
                let mut metadata = as_metadata(base, &serde_json::json!({}));
                metadata["code_challenge_methods_supported"] = methods.clone();
                metadata
            });
            let err = discover(&format!("{}/mcp", server.base)).unwrap_err();
            assert!(matches!(err, OAuthError::NoPkce(_)), "{err}");
        }
    }

    #[test]
    fn discovery_refuses_plain_http_on_a_remote_host() {
        let server = server_with(|base| {
            as_metadata(
                base,
                &serde_json::json!({ "token_endpoint": "http://auth.example.com/token" }),
            )
        });
        let err = discover(&format!("{}/mcp", server.base)).unwrap_err();
        assert!(
            matches!(&err, OAuthError::Insecure(url) if url == "http://auth.example.com/token"),
            "{err}"
        );
        assert!(secure_url("http://localhost:9/x").is_ok());
        assert!(secure_url("http://[::1]:9/x").is_ok());
        assert!(secure_url("https://auth.example.com/x").is_ok());
        assert!(matches!(
            secure_url("http://127.0.0.1.example.com/x"),
            Err(OAuthError::Insecure(_))
        ));
    }

    #[test]
    fn discovery_refuses_a_private_metadata_address_before_dialing() {
        let server = serve(|request, _| {
            match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/mcp") => Reply::status(401).header(
                "www-authenticate",
                r#"Bearer resource_metadata="https://10.255.255.1/.well-known/oauth-protected-resource""#,
            ),
            _ => Reply::status(404),
        }
        });
        let err = discover(&format!("{}/mcp", server.base)).unwrap_err();
        assert!(matches!(err, OAuthError::Discovery(_)), "{err}");
        assert!(err.to_string().contains("10.255.255.1"), "{err}");
    }

    #[test]
    fn discovery_refuses_metadata_for_another_resource_or_issuer() {
        let server = serve(|request, base| match request.path.as_str() {
            "/mcp" => challenge(base),
            "/prm" => Reply::json(
                200,
                &serde_json::json!({ "resource": format!("{base}/other"), "authorization_servers": [base] }),
            ),
            _ => Reply::status(404),
        });
        let err = discover(&format!("{}/mcp", server.base)).unwrap_err();
        assert!(matches!(err, OAuthError::Discovery(_)), "{err}");

        let server =
            server_with(|_| as_metadata("https://evil.example.com", &serde_json::json!({})));
        let err = discover(&format!("{}/mcp", server.base)).unwrap_err();
        assert!(err.to_string().contains("names issuer"), "{err}");
    }

    #[test]
    fn challenge_parameters_parse_from_mixed_challenges() {
        let params = bearer_params(
            r#"Basic realm="x", Bearer error="invalid_token", resource_metadata="https://a/b?c=\"d\"", Scope=read"#,
        );
        assert_eq!(
            params.get("error").map(String::as_str),
            Some("invalid_token")
        );
        assert_eq!(
            params.get("resource_metadata").map(String::as_str),
            Some(r#"https://a/b?c="d""#)
        );
        assert_eq!(params.get("scope").map(String::as_str), Some("read"));
        assert!(!params.contains_key("realm"));
        assert!(bearer_params("Bearer").is_empty());
        assert!(bearer_params("").is_empty());
    }

    #[test]
    fn canonical_urls_drop_case_default_ports_fragments_and_slashes() {
        assert_eq!(
            canonical_url("HTTPS://Mcp.Example.COM:443/mcp/#frag").unwrap(),
            "https://mcp.example.com/mcp"
        );
        assert_eq!(
            canonical_url("https://mcp.example.com/").unwrap(),
            "https://mcp.example.com"
        );
        assert_eq!(
            canonical_url("http://127.0.0.1:8080/a/b?x=1").unwrap(),
            "http://127.0.0.1:8080/a/b?x=1"
        );
        assert!(matches!(
            canonical_url("mcp.example.com"),
            Err(OAuthError::Url(_))
        ));
        assert!(canonical_url("file:///etc/passwd").is_err());
    }

    /// A discovery result pointing at `base`, with `registration`.
    fn discovered(base: &str, registration: bool) -> Discovery {
        Discovery {
            resource: "https://mcp.example.com/mcp".to_owned(),
            issuer: base.to_owned(),
            authorization_endpoint: format!("{base}/authorize?tenant=t"),
            token_endpoint: format!("{base}/token"),
            registration_endpoint: registration.then(|| format!("{base}/register")),
            scope: None,
            iss_required: false,
        }
    }

    #[test]
    fn registration_sends_the_redirect_and_returns_the_client_id() {
        let server = full_server();
        let redirect = "http://127.0.0.1:4242/callback";
        let client = register(&discovered(&server.base, true), redirect).unwrap();
        assert_eq!(client, "kage-client");
        let sent = server.find("POST", "/register").unwrap();
        let body: Value = serde_json::from_str(&sent.body).unwrap();
        assert_eq!(body["client_name"], "kage");
        assert_eq!(body["redirect_uris"], serde_json::json!([redirect]));
        assert_eq!(
            body["grant_types"],
            serde_json::json!(["authorization_code", "refresh_token"])
        );
        assert_eq!(body["token_endpoint_auth_method"], "none");

        let err = register(&discovered(&server.base, false), redirect).unwrap_err();
        assert!(matches!(err, OAuthError::NoRegistration(_)), "{err}");
        assert!(err.to_string().contains("client_id"), "{err}");
    }

    #[test]
    fn verifiers_are_unreserved_and_fresh() {
        let (a, b) = (Pkce::new().unwrap(), Pkce::new().unwrap());
        assert_eq!(a.verifier.len(), 43);
        assert!(
            a.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~'))
        );
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }

    #[test]
    fn the_authorization_url_carries_challenge_state_resource_and_scope() {
        let discovery = discovered("https://auth.example.com", false);
        let pkce = Pkce::new().unwrap();
        let expected = Expected::new(&discovery).unwrap();
        let redirect = "http://127.0.0.1:4242/callback";
        let url = authorize_url(
            &discovery,
            "cid",
            redirect,
            &pkce,
            &expected,
            Some("read write"),
        )
        .unwrap();
        let url = Url::parse(&url).unwrap();
        assert_eq!(url.path(), "/authorize");
        let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(query["code_challenge"], challenge);
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["state"], expected.state);
        assert_eq!(query["resource"], "https://mcp.example.com/mcp");
        assert_eq!(query["scope"], "read write");
        assert_eq!(query["client_id"], "cid");
        assert_eq!(query["redirect_uri"], redirect);
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["tenant"], "t");
        assert!(!query.contains_key("code_verifier"));

        let url = authorize_url(&discovery, "cid", redirect, &pkce, &expected, None).unwrap();
        assert!(!url.contains("scope="));
    }

    /// Send a browser-like GET for `target` to the listener at `port`
    /// and return the status line of the answer.
    fn browse(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nuser-agent: test\r\n\r\n"
        )
        .unwrap();
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        answer.lines().next().unwrap_or_default().to_owned()
    }

    fn expected_for(state: &str, iss_required: bool) -> Expected {
        Expected {
            state: state.to_owned(),
            issuer: "https://auth.example.com".to_owned(),
            iss_required,
        }
    }

    /// A listener on a host-chosen path answers only that path, and a
    /// state-only expectation ignores the redirect's `iss`.
    #[test]
    fn a_loopback_on_a_fixed_path_checks_the_state_alone() {
        let loopback = Loopback::bind_on(&[0], "/auth/callback").unwrap();
        let port = loopback.port;
        assert_eq!(
            loopback.redirect_uri(),
            format!("http://127.0.0.1:{port}/auth/callback")
        );
        let expected = Expected::state_only().unwrap();
        let state = expected.state().to_owned();
        let browser = std::thread::spawn(move || {
            [
                browse(port, "/callback?code=wrong-path"),
                browse(
                    port,
                    &format!("/auth/callback?code=c1&state={state}&iss=https://elsewhere"),
                ),
            ]
        });
        let code = loopback
            .wait(&expected, Duration::from_secs(10), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(code.secret(), "c1");
        let [wrong, ok] = browser.join().unwrap();
        assert!(wrong.contains("404"), "{wrong}");
        assert!(ok.contains("200"), "{ok}");

        let taken = Loopback::bind_on(&[0], "/x").unwrap();
        let err = Loopback::bind_on(&[taken.port], "/x").unwrap_err();
        assert!(matches!(err, OAuthError::Local(_)), "{err}");
    }

    #[test]
    fn the_loopback_returns_the_code_and_ignores_other_paths() {
        let loopback = Loopback::bind().unwrap();
        let port = loopback.port;
        assert_eq!(
            loopback.redirect_uri(),
            format!("http://127.0.0.1:{port}/callback")
        );
        let browser = std::thread::spawn(move || {
            [
                browse(port, "/favicon.ico"),
                browse(port, "/callback?code=the-code&state=s1"),
            ]
        });
        let code = loopback
            .wait(
                &expected_for("s1", false),
                Duration::from_secs(10),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(code.0, "the-code");
        let [favicon, callback] = browser.join().unwrap();
        assert!(favicon.contains("404"), "{favicon}");
        assert!(callback.contains("200"), "{callback}");
    }

    #[test]
    fn the_loopback_rejects_a_wrong_state() {
        let loopback = Loopback::bind().unwrap();
        let port = loopback.port;
        let browser = std::thread::spawn(move || browse(port, "/callback?code=c&state=forged"));
        let err = loopback
            .wait(
                &expected_for("s1", false),
                Duration::from_secs(10),
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert!(matches!(err, OAuthError::State), "{err}");
        assert!(browser.join().unwrap().contains("400"));
    }

    #[test]
    fn the_loopback_times_out_and_can_be_cancelled() {
        let start = Instant::now();
        let err = Loopback::bind()
            .unwrap()
            .wait(
                &expected_for("s1", false),
                Duration::from_millis(100),
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert!(matches!(err, OAuthError::Timeout), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
        let err = Loopback::bind()
            .unwrap()
            .wait(
                &expected_for("s1", false),
                Duration::from_secs(60),
                &AtomicBool::new(true),
            )
            .unwrap_err();
        assert!(matches!(err, OAuthError::Cancelled), "{err}");
    }

    #[test]
    fn a_pasted_redirect_yields_its_code() {
        let expected = expected_for("s1", false);
        let code = parse_redirect(
            "  http://127.0.0.1:4242/callback?code=abc%2Fd&state=s1\n",
            &expected,
        )
        .unwrap();
        assert_eq!(code.0, "abc/d");
        let wrong = parse_redirect("http://127.0.0.1/callback?code=abc&state=s2", &expected);
        assert!(matches!(wrong, Err(OAuthError::State)));
        let denied = parse_redirect(
            "http://127.0.0.1/callback?error=access_denied&state=s1",
            &expected,
        );
        assert!(
            matches!(&denied, Err(OAuthError::Denied(e)) if e == "access_denied"),
            "{denied:?}"
        );
        assert!(matches!(
            parse_redirect("not a url", &expected),
            Err(OAuthError::Redirect(_))
        ));
        assert!(matches!(
            parse_redirect("http://127.0.0.1/callback?state=s1", &expected),
            Err(OAuthError::Redirect(_))
        ));
    }

    #[test]
    fn the_issuer_is_checked_when_present_or_promised() {
        let url = |iss: &str| format!("http://127.0.0.1/callback?code=c&state=s1{iss}");
        let optional = expected_for("s1", false);
        let required = expected_for("s1", true);
        let ours = "&iss=https%3A%2F%2Fauth.example.com";
        assert!(parse_redirect(&url(""), &optional).is_ok());
        assert!(parse_redirect(&url(ours), &required).is_ok());
        assert!(parse_redirect(&url(""), &required).is_err());
        let other = "&iss=https%3A%2F%2Fevil.example.com";
        assert!(parse_redirect(&url(other), &optional).is_err());
    }

    #[test]
    fn exchange_and_refresh_send_the_resource_and_parse_expiry() {
        let server = full_server();
        let discovery = discovered(&server.base, false);
        let pkce = Pkce::new().unwrap();
        let code = AuthCode("code-secret".to_owned());
        let redirect = "http://127.0.0.1:4242/callback";
        let before = SystemTime::now();
        let tokens = exchange(&discovery, "cid", redirect, &code, &pkce).unwrap();
        assert_eq!(tokens.access_token, "access-secret-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-secret-1"));
        assert_eq!(tokens.scope.as_deref(), Some("read"));
        let expires = tokens.expires_at.unwrap();
        assert!(expires >= before + Duration::from_secs(3600));
        assert!(expires <= SystemTime::now() + Duration::from_secs(3600));
        let form = server.find("POST", "/token").unwrap().form();
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["code"], "code-secret");
        assert_eq!(form["code_verifier"], pkce.verifier);
        assert_eq!(form["redirect_uri"], redirect);
        assert_eq!(form["client_id"], "cid");
        assert_eq!(form["resource"], "https://mcp.example.com/mcp");

        server.seen.lock().unwrap().clear();
        let token_endpoint = format!("{}/token", server.base);
        let resource = "https://mcp.example.com/mcp";
        refresh(&token_endpoint, "cid", resource, "refresh-secret-0").unwrap();
        let form = server.find("POST", "/token").unwrap().form();
        assert_eq!(form["grant_type"], "refresh_token");
        assert_eq!(form["refresh_token"], "refresh-secret-0");
        assert_eq!(form["resource"], resource);
    }

    #[test]
    fn a_refresh_without_a_new_refresh_token_keeps_the_old_one() {
        let server = serve(|_, _| {
            Reply::json(
                200,
                &serde_json::json!({ "access_token": "a2", "expires_in": "60" }),
            )
        });
        let tokens = refresh(&format!("{}/token", server.base), "cid", "r", "keep-me").unwrap();
        assert_eq!(tokens.refresh_token.as_deref(), Some("keep-me"));
        assert!(tokens.expires_at.is_some());
    }

    #[test]
    fn refused_and_malformed_token_answers_fail_without_secrets() {
        let server = serve(|request, _| {
            if request.body.contains("refresh_token=refresh-secret") {
                Reply::json(
                    400,
                    &serde_json::json!({
                        "error": "invalid_grant",
                        "error_description": "refresh-secret is revoked",
                    }),
                )
            } else {
                Reply::json(
                    200,
                    &serde_json::json!({ "access_token": "x", "token_type": "mac" }),
                )
            }
        });
        let endpoint = format!("{}/token", server.base);
        let err = refresh(&endpoint, "cid", "r", "refresh-secret").unwrap_err();
        assert!(
            matches!(&err, OAuthError::Rejected { status: 400, error, .. } if error == "invalid_grant"),
            "{err}"
        );
        assert!(!err.to_string().contains("refresh-secret"), "{err}");
        assert!(!format!("{err:?}").contains("refresh-secret"), "{err:?}");
        let err = refresh(&endpoint, "cid", "r", "other").unwrap_err();
        assert!(matches!(err, OAuthError::Response(_)), "{err}");
    }

    #[test]
    fn debug_output_redacts_every_secret() {
        let pkce = Pkce::new().unwrap();
        let expected = expected_for("state-secret", false);
        let code = AuthCode("code-secret".to_owned());
        let tokens = Tokens {
            access_token: "access-secret".to_owned(),
            refresh_token: Some("refresh-secret".to_owned()),
            expires_at: None,
            scope: Some("read".to_owned()),
        };
        let printed = format!("{pkce:?} {expected:?} {code:?} {tokens:?}");
        for secret in [
            pkce.verifier.as_str(),
            "state-secret",
            "code-secret",
            "access-secret",
            "refresh-secret",
        ] {
            assert!(!printed.contains(secret), "{printed}");
        }
        assert!(printed.contains(REDACTED));
    }

    #[test]
    fn a_full_login_connects_the_server_with_its_token() {
        let server = serve(|request, base| {
            match (request.method.as_str(), request.path.as_str()) {
                ("POST", "/mcp")
                    if request.header("authorization") == Some("Bearer access-secret-1") =>
                {
                    let body: Value = serde_json::from_str(&request.body).unwrap_or_default();
                    if body["method"] == "initialize" {
                        Reply::json(
                            200,
                            &serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": body["id"],
                                "result": { "protocolVersion": PROTOCOL_VERSION, "capabilities": {} },
                            }),
                        )
                    } else {
                        Reply::status(202)
                    }
                }
                ("POST", "/mcp") => challenge(base),
                ("GET", "/mcp") => Reply::status(405),
                ("GET", "/prm") => Reply::json(
                    200,
                    &serde_json::json!({ "resource": format!("{base}/mcp"), "authorization_servers": [base] }),
                ),
                ("GET", "/.well-known/oauth-authorization-server") => {
                    Reply::json(200, &as_metadata(base, &serde_json::json!({})))
                }
                ("POST", "/register") => {
                    Reply::json(201, &serde_json::json!({ "client_id": "kage-client" }))
                }
                ("POST", "/token")
                    if request.form().get("code").map(String::as_str) == Some("c-1") =>
                {
                    Reply::json(
                        200,
                        &serde_json::json!({ "access_token": "access-secret-1", "token_type": "bearer" }),
                    )
                }
                _ => Reply::status(404),
            }
        });
        let url = format!("{}/mcp", server.base);
        let discovery = discover(&url).unwrap();
        let loopback = Loopback::bind().unwrap();
        let redirect = loopback.redirect_uri();
        let client = register(&discovery, &redirect).unwrap();
        let pkce = Pkce::new().unwrap();
        let expected = Expected::new(&discovery).unwrap();
        let authorize = authorize_url(
            &discovery,
            &client,
            &redirect,
            &pkce,
            &expected,
            discovery.scope.as_deref(),
        )
        .unwrap();
        let browser = std::thread::spawn(move || {
            let authorize = Url::parse(&authorize).unwrap();
            let query: HashMap<String, String> = authorize.query_pairs().into_owned().collect();
            let back = Url::parse(&query["redirect_uri"]).unwrap();
            let target = format!(
                "{}?code=c-1&state={}",
                back.path(),
                url::form_urlencoded::byte_serialize(query["state"].as_bytes()).collect::<String>()
            );
            browse(back.port().unwrap(), &target)
        });
        let code = loopback
            .wait(&expected, Duration::from_secs(10), &AtomicBool::new(false))
            .unwrap();
        assert!(browser.join().unwrap().contains("200"));
        let tokens = exchange(&discovery, &client, &redirect, &code, &pkce).unwrap();

        let cfg = kage_core::config::McpServer {
            startup_timeout_secs: None,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            url: Some(url),
            headers: BTreeMap::new(),
            disabled: false,
            oauth: None,
            disabled_tools: Vec::new(),
        };
        let source = StaticTokens::new(&tokens.access_token, None);
        let handle =
            crate::McpServerHandle::spawn_with("remote", &cfg, &[], None, Some(source)).unwrap();
        assert_eq!(handle.connection().protocol_version(), PROTOCOL_VERSION);
        let form = server.find("POST", "/token").unwrap().form();
        assert_eq!(form["client_id"], "kage-client");
        assert_eq!(form["redirect_uri"], redirect);
    }
}
