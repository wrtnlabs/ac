//! Application-agnostic OAuth 2.1 protocol mechanics for remote MCP servers.
//!
//! This module owns the reusable wire behavior: loopback redirect validation,
//! RFC 9728 protected-resource discovery, RFC 8414 authorization-server
//! discovery, RFC 7591 dynamic client registration, PKCE S256, authorization
//! URL construction, and authorization-code exchange. The optional managed
//! control plane supplies portable config, stock credential persistence, and configured
//! enumeration; embeddings still choose storage locations, browser hand-off,
//! RPC framing, identity, and product copy.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::future::BoxFuture;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::oauth_callback::{self, BindOutcome, PageCopy};

pub const ERR_STATE_MISMATCH: &str = "OAuth state mismatch — potential CSRF attack";
pub const ERR_OAUTH_DISABLED: &str = "OAuth is disabled for this server";
pub const ERR_INVALID_URL: &str = "Invalid server URL";

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ERROR_BODY: usize = 300;

/// Discovery requests carry no credentials and intentionally follow redirects:
/// real deployments commonly normalize well-known URLs through redirects.
static DISCOVERY_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .expect("reqwest client builder (OAuth discovery)")
});

/// Credential-bearing DCR and token requests must not follow redirects.
///
/// A 307/308 preserves the POST body, which could disclose an authorization
/// code, PKCE verifier, or client secret to an attacker-chosen location.
static SECRET_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client builder (OAuth secrets, no redirects)")
});

/// The redirect-following client intended for metadata discovery only.
pub fn discovery_http_client() -> &'static reqwest::Client {
    &DISCOVERY_HTTP_CLIENT
}

/// The no-redirect client intended for DCR and token exchange.
pub fn secret_http_client() -> &'static reqwest::Client {
    &SECRET_HTTP_CLIENT
}

/// Host identity injected into MCP initialize and RFC 7591 client metadata.
///
/// AC supplies a neutral default, while an embedding application can preserve
/// its own name, URI, and version without putting product branding in AC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientMetadata {
    pub client_name: String,
    pub client_uri: Option<String>,
    pub client_version: String,
}

impl ClientMetadata {
    pub fn new(client_name: impl Into<String>, client_version: impl Into<String>) -> Self {
        Self {
            client_name: client_name.into(),
            client_uri: None,
            client_version: client_version.into(),
        }
    }

    pub fn with_uri(mut self, client_uri: impl Into<String>) -> Self {
        self.client_uri = Some(client_uri.into());
        self
    }
}

impl Default for ClientMetadata {
    fn default() -> Self {
        Self::new("ac-mcp", env!("CARGO_PKG_VERSION"))
    }
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n + 16);
    while out.len() < n {
        out.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    out.truncate(n);
    out
}

/// Lowercase hexadecimal encoding of `bytes` cryptographically random bytes.
pub fn random_hex(bytes: usize) -> String {
    random_bytes(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// True only for HTTP(S) URLs safe to hand to an HTTP client or browser.
pub fn is_http_url(raw: &str) -> bool {
    Url::parse(raw).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// A redirect URI must target the exact address the callback listener binds.
///
/// Accepting `localhost` or IPv6 loopback while listening only on
/// `127.0.0.1` creates a valid-looking flow that can never receive its code.
pub fn is_loopback_redirect(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    if url.scheme() != "http" {
        return false;
    }
    url.host_str() == Some("127.0.0.1")
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Debug, Clone, PartialEq)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Epoch seconds.
    pub expires_at: Option<f64>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub client_id_issued_at: Option<f64>,
    pub client_secret_expires_at: Option<f64>,
    /// True when issued by dynamic client registration.
    pub from_registration: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Endpoints {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    /// RFC 8707 resource, present only when protected-resource metadata was
    /// actually published.
    pub resource: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    authorization_servers: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct AuthServerMetadata {
    #[serde(default)]
    authorization_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint: Option<String>,
    #[serde(default)]
    registration_endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    client_id_issued_at: Option<f64>,
    #[serde(default)]
    client_secret_expires_at: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<f64>,
    #[serde(default)]
    scope: Option<String>,
}

fn truncate(mut text: String) -> String {
    if text.len() > MAX_ERROR_BODY {
        let mut end = MAX_ERROR_BODY;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

fn origin_of(url: &Url) -> String {
    let mut base = url.clone();
    base.set_path("");
    base.set_query(None);
    base.set_fragment(None);
    base.as_str().trim_end_matches('/').to_string()
}

/// Canonical RFC 8707 resource URI: no query or fragment.
pub fn canonical_resource(url: &Url) -> String {
    let mut base = url.clone();
    base.set_query(None);
    base.set_fragment(None);
    base.as_str().trim_end_matches('/').to_string()
}

/// RFC 8414 §3.1 / RFC 9728 well-known candidates for a possibly-pathful URL.
pub fn well_known_candidates(url: &Url, name: &str) -> Vec<String> {
    let origin = origin_of(url);
    let path = url.path().trim_end_matches('/');
    let mut out = Vec::new();
    if !path.is_empty() {
        out.push(format!("{origin}/.well-known/{name}{path}"));
    }
    out.push(format!("{origin}/.well-known/{name}"));
    out
}

async fn fetch_json(client: &reqwest::Client, url: &str) -> Option<Value> {
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .header("MCP-Protocol-Version", "2025-06-18")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<Value>().await.ok()
}

async fn resource_metadata_hint(
    client: &reqwest::Client,
    server_url: &Url,
    metadata: &ClientMetadata,
) -> Option<String> {
    let response = client
        .post(server_url.clone())
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .body(
            json!({
                "jsonrpc": "2.0",
                "id": "ac-mcp-auth-discovery",
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {
                        "name": metadata.client_name,
                        "version": metadata.client_version,
                    }
                }
            })
            .to_string(),
        )
        .send()
        .await
        .ok()?;
    let header = response.headers().get("www-authenticate")?.to_str().ok()?;
    let start = header.find("resource_metadata=")? + "resource_metadata=".len();
    let rest = &header[start..];
    let value = rest.strip_prefix('"').map_or_else(
        || rest.split(&[',', ' '][..]).next().unwrap_or(""),
        |quoted| quoted.split('"').next().unwrap_or(""),
    );
    (!value.is_empty()).then(|| value.to_string())
}

/// Discover OAuth endpoints using AC's neutral client identity.
pub async fn discover_endpoints(
    client: &reqwest::Client,
    server_url: &Url,
) -> Result<Endpoints, String> {
    discover_endpoints_with_metadata(client, server_url, &ClientMetadata::default()).await
}

/// Discover OAuth endpoints while injecting the embedding host's identity.
pub async fn discover_endpoints_with_metadata(
    client: &reqwest::Client,
    server_url: &Url,
    client_metadata: &ClientMetadata,
) -> Result<Endpoints, String> {
    let mut protected_resource_urls = Vec::new();
    if let Some(hint) = resource_metadata_hint(client, server_url, client_metadata).await
        && is_http_url(&hint)
    {
        protected_resource_urls.push(hint);
    }
    protected_resource_urls.extend(well_known_candidates(
        server_url,
        "oauth-protected-resource",
    ));

    let mut protected_resource: Option<ProtectedResourceMetadata> = None;
    for url in &protected_resource_urls {
        if let Some(value) = fetch_json(client, url).await
            && let Ok(parsed) = serde_json::from_value::<ProtectedResourceMetadata>(value)
        {
            protected_resource = Some(parsed);
            break;
        }
    }

    let issuer = protected_resource
        .as_ref()
        .and_then(|metadata| metadata.authorization_servers.as_ref())
        .and_then(|servers| servers.first())
        .filter(|server| is_http_url(server))
        .and_then(|server| Url::parse(server).ok())
        .unwrap_or_else(|| server_url.clone());

    let mut metadata_urls = well_known_candidates(&issuer, "oauth-authorization-server");
    metadata_urls.extend(well_known_candidates(&issuer, "openid-configuration"));
    let mut authorization_server: Option<AuthServerMetadata> = None;
    for url in &metadata_urls {
        if let Some(value) = fetch_json(client, url).await
            && let Ok(parsed) = serde_json::from_value::<AuthServerMetadata>(value)
            && parsed.authorization_endpoint.is_some()
        {
            authorization_server = Some(parsed);
            break;
        }
    }

    let issuer_origin = origin_of(&issuer);
    let metadata = authorization_server.unwrap_or(AuthServerMetadata {
        authorization_endpoint: None,
        token_endpoint: None,
        registration_endpoint: None,
    });
    let http_only = |value: Option<String>| value.filter(|endpoint| is_http_url(endpoint));
    Ok(Endpoints {
        authorization_endpoint: http_only(metadata.authorization_endpoint)
            .unwrap_or_else(|| format!("{issuer_origin}/authorize")),
        token_endpoint: http_only(metadata.token_endpoint)
            .unwrap_or_else(|| format!("{issuer_origin}/token")),
        registration_endpoint: http_only(metadata.registration_endpoint)
            .or_else(|| Some(format!("{issuer_origin}/register"))),
        resource: protected_resource.map(|metadata| {
            metadata
                .resource
                .unwrap_or_else(|| canonical_resource(server_url))
        }),
    })
}

/// Build RFC 7591 metadata with host-supplied identity.
pub fn client_metadata(
    host: &ClientMetadata,
    redirect_uri: &str,
    scope: Option<&str>,
    has_secret: bool,
) -> Value {
    let mut out = json!({
        "redirect_uris": [redirect_uri],
        "client_name": host.client_name,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": if has_secret { "client_secret_post" } else { "none" },
    });
    if let Some(client_uri) = host.client_uri.as_deref().filter(|uri| !uri.is_empty()) {
        out["client_uri"] = json!(client_uri);
    }
    if let Some(scope) = scope.filter(|scope| !scope.is_empty()) {
        out["scope"] = json!(scope);
    }
    out
}

/// Register an OAuth client. Callers should use [`secret_http_client`] unless
/// they intentionally supply an equivalently no-redirect client.
pub async fn register_client(
    client: &reqwest::Client,
    registration_endpoint: &str,
    metadata: &Value,
) -> Result<ClientCredentials, String> {
    let response = client
        .post(registration_endpoint)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(metadata)
        .send()
        .await
        .map_err(|error| format!("Dynamic client registration failed: {error}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "Dynamic client registration failed ({}): {}",
            status.as_u16(),
            truncate(body)
        ));
    }
    let parsed: RegistrationResponse = serde_json::from_str(&body).map_err(|error| {
        format!("Dynamic client registration returned an unreadable response: {error}")
    })?;
    Ok(ClientCredentials {
        client_id: parsed.client_id,
        client_secret: parsed.client_secret,
        client_id_issued_at: parsed.client_id_issued_at,
        client_secret_expires_at: parsed.client_secret_expires_at,
        from_registration: true,
    })
}

/// A PKCE pair. The verifier is 43 base64url characters, within RFC 7636's
/// required 43..128 range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn new_pkce() -> Pkce {
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes(32));
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

pub fn build_authorization_url(
    endpoints: &Endpoints,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    scope: Option<&str>,
) -> Result<String, String> {
    let mut url = Url::parse(&endpoints.authorization_endpoint)
        .map_err(|error| format!("Invalid authorization endpoint: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "Refused a non-http(s) authorization endpoint: {}",
            endpoints.authorization_endpoint
        ));
    }
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state);
        if let Some(scope) = scope.filter(|scope| !scope.is_empty()) {
            query.append_pair("scope", scope);
        }
        if let Some(resource) = &endpoints.resource {
            query.append_pair("resource", resource);
        }
    }
    Ok(url.to_string())
}

/// Exchange an authorization code for tokens. Callers should use
/// [`secret_http_client`] unless they intentionally inject an equivalently
/// no-redirect client.
pub async fn exchange_code(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    credentials: &ClientCredentials,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthTokens, String> {
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
        ("client_id", &credentials.client_id),
    ];
    if let Some(secret) = &credentials.client_secret {
        form.push(("client_secret", secret));
    }
    if let Some(resource) = &endpoints.resource {
        form.push(("resource", resource));
    }
    let response = client
        .post(&endpoints.token_endpoint)
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|error| format!("Token exchange failed: {error}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "Token exchange failed ({}): {}",
            status.as_u16(),
            truncate(body)
        ));
    }
    let parsed: TokenResponse = serde_json::from_str(&body)
        .map_err(|error| format!("Token endpoint returned an unreadable response: {error}"))?;
    Ok(OAuthTokens {
        expires_at: parsed.expires_in.map(|seconds| now_secs() + seconds),
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token.filter(|token| !token.is_empty()),
        scope: parsed.scope.filter(|scope| !scope.is_empty()),
    })
}

// --- interactive authorization coordinator --------------------------------

/// Host policy and identity for one interactive OAuth authorization.
///
/// AC owns the state machine; an embedding application maps its config into
/// this neutral shape and supplies persistence, enumeration, and browser
/// hand-off through the traits below.
#[derive(Debug, Clone)]
pub struct InteractiveOAuthConfig {
    pub enabled: bool,
    pub server_url: String,
    pub redirect_uri: String,
    pub scope: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub discovery_metadata: ClientMetadata,
    pub registration_metadata: ClientMetadata,
    pub page_copy: PageCopy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthEnumerateError {
    pub message: String,
    /// True only when the server observably rejected the current credentials.
    /// Connection/configuration failures must not open a browser.
    pub needs_auth: bool,
}

/// Connect, enumerate the server's capabilities, then disconnect.
///
/// `T` is intentionally host-chosen: AC coordinates authentication without
/// owning the application's cached-catalog row or result envelope.
pub trait OAuthEnumerator<T>: Send + Sync {
    fn enumerate(
        &self,
        bearer: Option<String>,
    ) -> BoxFuture<'_, Result<Vec<T>, OAuthEnumerateError>>;
}

/// Whether a semantic credential-store operation failed or was invalidated by
/// a newer owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthStoreErrorKind {
    /// An I/O, serialization, keystore, or database failure.
    Failed,
    /// A newer authorization generation or explicit removal owns this entry.
    ///
    /// Cleanup from the stale flow must not overwrite that newer state.
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthStoreError {
    pub kind: OAuthStoreErrorKind,
    pub message: String,
}

impl OAuthStoreError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: OAuthStoreErrorKind::Failed,
            message: message.into(),
        }
    }

    pub fn superseded(message: impl Into<String>) -> Self {
        Self {
            kind: OAuthStoreErrorKind::Superseded,
            message: message.into(),
        }
    }

    pub fn is_superseded(&self) -> bool {
        self.kind == OAuthStoreErrorKind::Superseded
    }
}

impl fmt::Display for OAuthStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for OAuthStoreError {}

/// Persistence seam for the generic interactive flow.
///
/// Implementations own their storage location and locking. Every operation is
/// async so a file-backed host can move disk I/O off the runtime. The
/// coordinator names semantic operations instead of assuming a JSON schema,
/// keystore, or database. `clear_pending` must remove both the CSRF state and
/// PKCE verifier in one mutation.
pub trait OAuthFlowStore: Send + Sync {
    fn bearer<'a>(
        &'a self,
        server_url: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, OAuthStoreError>>;
    fn state(&self) -> BoxFuture<'_, Result<Option<String>, OAuthStoreError>>;
    fn persist_state<'a>(&'a self, state: &'a str) -> BoxFuture<'a, Result<(), OAuthStoreError>>;
    fn persist_verifier<'a>(
        &'a self,
        verifier: &'a str,
    ) -> BoxFuture<'a, Result<(), OAuthStoreError>>;
    fn clear_pending(&self) -> BoxFuture<'_, Result<(), OAuthStoreError>>;
    fn commit<'a>(
        &'a self,
        server_url: &'a str,
        tokens: &'a OAuthTokens,
        credentials: &'a ClientCredentials,
    ) -> BoxFuture<'a, Result<(), OAuthStoreError>>;
}

async fn fail_flow<T>(store: &dyn OAuthFlowStore, message: String) -> Result<Vec<T>, String> {
    match store.clear_pending().await {
        Ok(()) => Err(message),
        // A remove or a newer login owns the entry. The stale flow must not
        // "clean up" by recreating or changing it.
        Err(error) if error.is_superseded() => Err(message),
        Err(error) => Err(format!(
            "{message}; additionally, could not clear pending OAuth state: {error}"
        )),
    }
}

struct CoordinatorSlot {
    /// Exactly one flow per host-visible server name may own persistence and
    /// callback state. Other callers wait without becoming cleanup owners.
    gate: Arc<tokio::sync::Mutex<()>>,
    /// All attempts that started before `cancel_interactive_authentication`
    /// share this token. Cancelling swaps in a fresh token for later callers.
    generation_cancel: Mutex<CancellationToken>,
}

impl CoordinatorSlot {
    fn new() -> Self {
        Self {
            gate: Arc::new(tokio::sync::Mutex::new(())),
            generation_cancel: Mutex::new(CancellationToken::new()),
        }
    }

    fn attempt_token(&self) -> CancellationToken {
        self.generation_cancel
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn cancel_generation(&self) {
        let mut current = self
            .generation_cancel
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        current.cancel();
        *current = CancellationToken::new();
    }
}

struct OAuthCoordinatorInner {
    slots: Mutex<HashMap<String, Weak<CoordinatorSlot>>>,
    callback: oauth_callback::OAuthCallbackServer,
}

/// Instance-owned interactive OAuth orchestration.
///
/// Each embedding constructs and retains its own coordinator. Per-name
/// single-flight, cancellation generations, and callback listener state are
/// scoped to that value, so unrelated embeddings in the same process cannot
/// serialize or cancel one another merely because they chose the same server
/// name.
#[derive(Clone)]
pub struct OAuthCoordinator {
    inner: Arc<OAuthCoordinatorInner>,
}

impl Default for OAuthCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthCoordinator {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(OAuthCoordinatorInner {
                slots: Mutex::new(HashMap::new()),
                callback: oauth_callback::OAuthCallbackServer::new(),
            }),
        }
    }

    fn slot(&self, name: &str) -> Arc<CoordinatorSlot> {
        let mut slots = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        slots.retain(|_, slot| slot.strong_count() > 0);
        if let Some(slot) = slots.get(name).and_then(Weak::upgrade) {
            return slot;
        }
        let slot = Arc::new(CoordinatorSlot::new());
        slots.insert(name.to_string(), Arc::downgrade(&slot));
        slot
    }

    /// Run one complete interactive OAuth authorization.
    ///
    /// The sequence is deliberately centralized here: stored-token probe,
    /// callback bind, CSRF state, discovery, DCR, PKCE, browser hand-off,
    /// callback verification, code exchange, credential commit, and
    /// authenticated re-enumeration.
    pub async fn authenticate_interactive<T: Send>(
        &self,
        name: &str,
        config: &InteractiveOAuthConfig,
        store: &dyn OAuthFlowStore,
        enumerator: &dyn OAuthEnumerator<T>,
        on_open_url: &(dyn Fn(String) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<Vec<T>, String> {
        let slot = self.slot(name);
        let coordinator_cancel = slot.attempt_token();
        let gate = slot.gate.clone();

        // Waiting is not ownership. A caller that gives up here must not
        // clear the active flow's verifier/state or cancel its callback.
        let _single_flight = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(oauth_callback::ERR_CANCELLED.to_string()),
            _ = coordinator_cancel.cancelled() => {
                return Err(oauth_callback::ERR_CANCELLED.to_string());
            }
            guard = gate.lock_owned() => guard,
        };
        if is_cancelled(cancel, &coordinator_cancel) {
            return Err(oauth_callback::ERR_CANCELLED.to_string());
        }

        authenticate_interactive_inner(
            &self.inner.callback,
            name,
            config,
            store,
            enumerator,
            on_open_url,
            FlowCancellation {
                caller: cancel,
                coordinator: &coordinator_cancel,
            },
        )
        .await
    }

    /// Cancel every interactive authorization attempt for `name` that began
    /// before this call, whether it owns the flow or is waiting for
    /// single-flight. Later attempts receive a fresh cancellation generation.
    pub fn cancel_interactive_authentication(&self, name: &str) {
        self.slot(name).cancel_generation();
    }

    /// Cancel all current attempts and stop this coordinator's callback
    /// listener. Other coordinator instances are unaffected.
    pub async fn shutdown(&self) {
        let slots: Vec<_> = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for slot in slots {
            slot.cancel_generation();
        }
        self.inner.callback.stop().await;
    }

    /// Whether this coordinator currently owns a loopback callback listener.
    pub fn callback_is_running(&self) -> bool {
        self.inner.callback.is_running()
    }
}

fn is_cancelled(caller: &CancellationToken, coordinator: &CancellationToken) -> bool {
    caller.is_cancelled() || coordinator.is_cancelled()
}

async fn cancellation_requested(caller: &CancellationToken, coordinator: &CancellationToken) {
    tokio::select! {
        biased;
        _ = caller.cancelled() => {}
        _ = coordinator.cancelled() => {}
    }
}

async fn cancellable<F>(
    future: F,
    caller: &CancellationToken,
    coordinator: &CancellationToken,
) -> Result<F::Output, ()>
where
    F: std::future::Future,
{
    tokio::pin!(future);
    tokio::select! {
        biased;
        _ = cancellation_requested(caller, coordinator) => Err(()),
        output = &mut future => Ok(output),
    }
}

#[derive(Clone, Copy)]
struct FlowCancellation<'a> {
    caller: &'a CancellationToken,
    coordinator: &'a CancellationToken,
}

async fn authenticate_interactive_inner<T: Send>(
    callback: &oauth_callback::OAuthCallbackServer,
    name: &str,
    config: &InteractiveOAuthConfig,
    store: &dyn OAuthFlowStore,
    enumerator: &dyn OAuthEnumerator<T>,
    on_open_url: &(dyn Fn(String) + Send + Sync),
    cancellation: FlowCancellation<'_>,
) -> Result<Vec<T>, String> {
    let FlowCancellation {
        caller: caller_cancel,
        coordinator: coordinator_cancel,
    } = cancellation;
    if !config.enabled {
        return Err(ERR_OAUTH_DISABLED.to_string());
    }
    let server_url = Url::parse(&config.server_url).map_err(|_| ERR_INVALID_URL.to_string())?;
    if !is_loopback_redirect(&config.redirect_uri) {
        return Err(format!(
            "The configured OAuth redirect URI must be a loopback http URL \
             (http://127.0.0.1:<port>/<path>) — got `{}`.",
            config.redirect_uri
        ));
    }

    let bearer_read = match cancellable(
        store.bearer(&config.server_url),
        caller_cancel,
        coordinator_cancel,
    )
    .await
    {
        Ok(result) => result,
        Err(()) => return Err(oauth_callback::ERR_CANCELLED.to_string()),
    };
    let bearer = match bearer_read {
        Ok(bearer) => bearer,
        Err(error) => {
            return Err(format!(
                "Could not read the stored OAuth credentials: {error}"
            ));
        }
    };
    if is_cancelled(caller_cancel, coordinator_cancel) {
        return Err(oauth_callback::ERR_CANCELLED.to_string());
    }
    let initial_enumeration = match cancellable(
        enumerator.enumerate(bearer),
        caller_cancel,
        coordinator_cancel,
    )
    .await
    {
        Ok(result) => result,
        Err(()) => return Err(oauth_callback::ERR_CANCELLED.to_string()),
    };
    match initial_enumeration {
        Ok(items) => return Ok(items),
        Err(error) if !error.needs_auth => return Err(error.message),
        Err(_) => {}
    }

    let binding = match cancellable(
        callback.acquire_binding(&config.redirect_uri, config.page_copy.clone()),
        caller_cancel,
        coordinator_cancel,
    )
    .await
    {
        Ok(result) => result,
        Err(()) => return Err(oauth_callback::ERR_CANCELLED.to_string()),
    };
    let (bind_outcome, _binding_lease) = match binding {
        Err(error) => {
            return Err(format!(
                "Could not start the OAuth callback server: {error}"
            ));
        }
        Ok(binding) => binding,
    };
    match bind_outcome {
        BindOutcome::PortHeldByForeignProcess { port } => {
            return Err(format!(
                "The OAuth callback port ({port}) is already in use by another process — close it \
                 and try again."
            ));
        }
        BindOutcome::Bound { .. } => {}
    }

    let state = random_hex(32);
    if let Err(error) = store.persist_state(&state).await {
        return fail_flow(store, format!("Could not persist the OAuth state: {error}")).await;
    }
    if is_cancelled(caller_cancel, coordinator_cancel) {
        return fail_flow(store, oauth_callback::ERR_CANCELLED.to_string()).await;
    }

    let discovery = match cancellable(
        discover_endpoints_with_metadata(
            discovery_http_client(),
            &server_url,
            &config.discovery_metadata,
        ),
        caller_cancel,
        coordinator_cancel,
    )
    .await
    {
        Ok(result) => result,
        Err(()) => {
            return fail_flow(store, oauth_callback::ERR_CANCELLED.to_string()).await;
        }
    };
    let endpoints = match discovery {
        Ok(endpoints) => endpoints,
        Err(error) => return fail_flow(store, error).await,
    };

    let credentials = if let Some(client_id) = config
        .client_id
        .clone()
        .filter(|client_id| !client_id.is_empty())
    {
        ClientCredentials {
            client_id,
            client_secret: config
                .client_secret
                .clone()
                .filter(|secret| !secret.is_empty()),
            client_id_issued_at: None,
            client_secret_expires_at: None,
            from_registration: false,
        }
    } else {
        let Some(endpoint) = endpoints.registration_endpoint.clone() else {
            return fail_flow(
                store,
                "This server has no client id configured and does not support dynamic client \
                 registration"
                    .to_string(),
            )
            .await;
        };
        let metadata = client_metadata(
            &config.registration_metadata,
            &config.redirect_uri,
            config.scope.as_deref(),
            false,
        );
        let registration = match cancellable(
            register_client(secret_http_client(), &endpoint, &metadata),
            caller_cancel,
            coordinator_cancel,
        )
        .await
        {
            Ok(result) => result,
            Err(()) => {
                return fail_flow(store, oauth_callback::ERR_CANCELLED.to_string()).await;
            }
        };
        match registration {
            Ok(credentials) => credentials,
            Err(error) => return fail_flow(store, error).await,
        }
    };

    let pkce = new_pkce();
    if let Err(error) = store.persist_verifier(&pkce.verifier).await {
        return fail_flow(
            store,
            format!("Could not persist the PKCE verifier: {error}"),
        )
        .await;
    }
    if is_cancelled(caller_cancel, coordinator_cancel) {
        return fail_flow(store, oauth_callback::ERR_CANCELLED.to_string()).await;
    }
    let authorization_url = match build_authorization_url(
        &endpoints,
        &credentials.client_id,
        &config.redirect_uri,
        &state,
        &pkce.challenge,
        config.scope.as_deref(),
    ) {
        Ok(url) => url,
        Err(error) => return fail_flow(store, error).await,
    };

    let pending = callback.begin(&state, name);
    on_open_url(authorization_url);
    if is_cancelled(caller_cancel, coordinator_cancel) {
        drop(pending);
        return fail_flow(store, oauth_callback::ERR_CANCELLED.to_string()).await;
    }
    let callback = match cancellable(pending.wait(), caller_cancel, coordinator_cancel).await {
        Ok(result) => result,
        Err(()) => {
            return fail_flow(store, oauth_callback::ERR_CANCELLED.to_string()).await;
        }
    };
    let code = match callback {
        Ok(code) => code,
        Err(error) => return fail_flow(store, error).await,
    };

    let state_read = match cancellable(store.state(), caller_cancel, coordinator_cancel).await {
        Ok(result) => result,
        Err(()) => {
            return fail_flow(store, oauth_callback::ERR_CANCELLED.to_string()).await;
        }
    };
    let persisted_state = match state_read {
        Ok(state) => state,
        Err(error) => {
            return fail_flow(
                store,
                format!("Could not read the persisted OAuth state: {error}"),
            )
            .await;
        }
    };
    if is_cancelled(caller_cancel, coordinator_cancel) {
        return fail_flow(store, oauth_callback::ERR_CANCELLED.to_string()).await;
    }
    if persisted_state.as_deref() != Some(state.as_str()) {
        return fail_flow(store, ERR_STATE_MISMATCH.to_string()).await;
    }

    let exchange = match cancellable(
        exchange_code(
            secret_http_client(),
            &endpoints,
            &credentials,
            &code,
            &pkce.verifier,
            &config.redirect_uri,
        ),
        caller_cancel,
        coordinator_cancel,
    )
    .await
    {
        Ok(result) => result,
        Err(()) => {
            return fail_flow(store, oauth_callback::ERR_CANCELLED.to_string()).await;
        }
    };
    let tokens = match exchange {
        Ok(tokens) => tokens,
        Err(error) => return fail_flow(store, error).await,
    };
    if let Err(error) = store
        .commit(&config.server_url, &tokens, &credentials)
        .await
    {
        return fail_flow(
            store,
            format!("Could not persist the OAuth tokens: {error}"),
        )
        .await;
    }
    if is_cancelled(caller_cancel, coordinator_cancel) {
        return Err(oauth_callback::ERR_CANCELLED.to_string());
    }

    match cancellable(
        enumerator.enumerate(Some(tokens.access_token)),
        caller_cancel,
        coordinator_cancel,
    )
    .await
    {
        Ok(result) => result.map_err(|error| error.message),
        Err(()) => Err(oauth_callback::ERR_CANCELLED.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::Notify;

    use super::*;

    const TEST_CALLBACK_PATH: &str = "/oauth/callback";

    #[test]
    fn random_values_and_pkce_are_well_formed() {
        let first = random_hex(32);
        let second = random_hex(32);
        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
        assert!(first.chars().all(|character| character.is_ascii_hexdigit()));

        let pkce = new_pkce();
        assert_eq!(pkce.verifier.len(), 43);
        assert!(
            pkce.verifier
                .chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || character == '-'
                    || character == '_')
        );
        assert_eq!(
            pkce.challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()))
        );
    }

    #[test]
    fn authorization_url_carries_pkce_state_scope_and_resource() {
        let endpoints = Endpoints {
            authorization_endpoint: "https://as.test/authorize?tenant=acme".to_string(),
            token_endpoint: "https://as.test/token".to_string(),
            registration_endpoint: None,
            resource: Some("https://mcp.test/mcp".to_string()),
        };
        let url = build_authorization_url(
            &endpoints,
            "client-1",
            "http://127.0.0.1:43123/oauth/callback",
            "st4te",
            "chal",
            Some("read write"),
        )
        .unwrap();
        let pairs: HashMap<_, _> = Url::parse(&url)
            .unwrap()
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(pairs["tenant"], "acme");
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["client_id"], "client-1");
        assert_eq!(pairs["code_challenge"], "chal");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["state"], "st4te");
        assert_eq!(pairs["scope"], "read write");
        assert_eq!(pairs["resource"], "https://mcp.test/mcp");
        assert_eq!(
            pairs["redirect_uri"],
            "http://127.0.0.1:43123/oauth/callback"
        );
    }

    #[test]
    fn well_known_candidates_cover_pathful_and_root_issuers() {
        let url = Url::parse("https://mcp.test/deep/mcp").unwrap();
        assert_eq!(
            well_known_candidates(&url, "oauth-authorization-server"),
            [
                "https://mcp.test/.well-known/oauth-authorization-server/deep/mcp",
                "https://mcp.test/.well-known/oauth-authorization-server",
            ]
        );
        let root = Url::parse("https://mcp.test/").unwrap();
        assert_eq!(
            well_known_candidates(&root, "oauth-protected-resource"),
            ["https://mcp.test/.well-known/oauth-protected-resource"]
        );
        assert_eq!(canonical_resource(&url), "https://mcp.test/deep/mcp");
    }

    #[test]
    fn dcr_metadata_uses_host_identity_without_core_branding() {
        let host = ClientMetadata::new("Host App", "1.2.3").with_uri("https://example.test/host");
        let metadata = client_metadata(&host, "http://127.0.0.1:43123/cb", Some("read"), false);
        assert_eq!(metadata["client_name"], "Host App");
        assert_eq!(metadata["client_uri"], "https://example.test/host");
        assert_eq!(
            metadata["redirect_uris"],
            json!(["http://127.0.0.1:43123/cb"])
        );
        assert_eq!(
            metadata["grant_types"],
            json!(["authorization_code", "refresh_token"])
        );
        assert_eq!(metadata["response_types"], json!(["code"]));
        assert_eq!(metadata["token_endpoint_auth_method"], "none");
        assert_eq!(metadata["scope"], "read");
    }

    #[test]
    fn only_http_schemes_and_http_loopback_redirects_are_accepted() {
        assert!(is_http_url("https://as.test/authorize"));
        assert!(is_http_url("http://127.0.0.1:9/authorize"));
        assert!(!is_http_url("file:///tmp/token"));
        assert!(!is_http_url("javascript:alert(1)"));

        for accepted in [
            "http://127.0.0.1:43123/oauth/callback",
            "http://127.0.0.1/cb",
        ] {
            assert!(is_loopback_redirect(accepted), "{accepted}");
        }
        for rejected in [
            "https://attacker.example:19999/cb",
            "http://attacker.example:19999/cb",
            "https://127.0.0.1:43123/cb",
            "http://localhost:9000/cb",
            "http://[::1]:9000/cb",
            "file:///tmp/cb",
            "not a url",
        ] {
            assert!(!is_loopback_redirect(rejected), "{rejected}");
        }
    }

    #[test]
    fn authorization_url_refuses_non_http_endpoints() {
        for endpoint in [
            "file:///tmp/token",
            "javascript:alert(document.domain)",
            "vscode://remote/x",
            "smb://attacker.example/share",
        ] {
            let endpoints = Endpoints {
                authorization_endpoint: endpoint.to_string(),
                token_endpoint: "https://as.test/token".to_string(),
                registration_endpoint: None,
                resource: None,
            };
            let error = build_authorization_url(
                &endpoints,
                "client",
                "http://127.0.0.1:1/cb",
                "state",
                "challenge",
                None,
            )
            .expect_err(endpoint);
            assert!(
                error.starts_with("Refused a non-http(s) authorization endpoint"),
                "{error}"
            );
        }
    }

    struct StubAuthorizationServer {
        base: String,
        requests: Arc<Mutex<Vec<(String, String, String)>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for StubAuthorizationServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    struct StallingServer {
        base: String,
        accepted: Arc<Notify>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for StallingServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl StallingServer {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let port = listener.local_addr().unwrap().port();
            let accepted = Arc::new(Notify::new());
            let signal = accepted.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    signal.notify_one();
                    tokio::spawn(async move {
                        let _stream = stream;
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    });
                }
            });
            Self {
                base: format!("http://127.0.0.1:{port}"),
                accepted,
                task,
            }
        }
    }

    impl StubAuthorizationServer {
        async fn start() -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let port = listener.local_addr().unwrap().port();
            let base = format!("http://127.0.0.1:{port}");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let request_log = requests.clone();
            let task_base = base.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let request_log = request_log.clone();
                    let base = task_base.clone();
                    tokio::spawn(async move {
                        let mut raw = Vec::new();
                        let mut buffer = [0_u8; 1024];
                        loop {
                            let Ok(read) = stream.read(&mut buffer).await else {
                                return;
                            };
                            if read == 0 {
                                break;
                            }
                            raw.extend_from_slice(&buffer[..read]);
                            let text = String::from_utf8_lossy(&raw);
                            let Some((head, body)) = text.split_once("\r\n\r\n") else {
                                continue;
                            };
                            let content_length = head
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .and_then(|value| value.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if body.len() >= content_length {
                                break;
                            }
                        }
                        let text = String::from_utf8_lossy(&raw);
                        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
                        let mut head_parts = head.split_whitespace();
                        let method = head_parts.next().unwrap_or("GET").to_string();
                        let target = head_parts.next().unwrap_or("/").to_string();
                        request_log.lock().unwrap().push((
                            method,
                            target.clone(),
                            body.to_string(),
                        ));

                        let (status, headers, payload) = if target.starts_with("/mcp") {
                            (
                                401,
                                format!(
                                    "WWW-Authenticate: Bearer resource_metadata=\"{base}/\
                                         .well-known/oauth-protected-resource\"\r\n"
                                ),
                                "{}".to_string(),
                            )
                        } else if target.starts_with("/.well-known/oauth-protected-resource") {
                            (
                                200,
                                String::new(),
                                json!({
                                    "resource": format!("{base}/mcp"),
                                    "authorization_servers": [base.clone()],
                                })
                                .to_string(),
                            )
                        } else if target.starts_with("/.well-known/oauth-authorization-server") {
                            (
                                200,
                                String::new(),
                                json!({
                                    "authorization_endpoint": format!("{base}/authorize"),
                                    "token_endpoint": format!("{base}/token"),
                                    "registration_endpoint": format!("{base}/register"),
                                })
                                .to_string(),
                            )
                        } else if target.starts_with("/redirect-register") {
                            (307, format!("Location: {base}/collect\r\n"), String::new())
                        } else if target.starts_with("/register") || target.starts_with("/collect")
                        {
                            (
                                200,
                                String::new(),
                                json!({
                                    "client_id": "registered-client",
                                    "client_secret": "registered-secret",
                                    "client_id_issued_at": 1,
                                    "client_secret_expires_at": 0,
                                })
                                .to_string(),
                            )
                        } else if target.starts_with("/token") {
                            (
                                200,
                                String::new(),
                                json!({
                                    "access_token": "access-1",
                                    "refresh_token": "refresh-1",
                                    "expires_in": 3600,
                                    "scope": "read",
                                })
                                .to_string(),
                            )
                        } else {
                            (404, String::new(), "{}".to_string())
                        };
                        let response = format!(
                            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\n{headers}Connection: close\r\n\r\n{payload}",
                            payload.len()
                        );
                        stream.write_all(response.as_bytes()).await.unwrap();
                    });
                }
            });
            Self {
                base,
                requests,
                task,
            }
        }

        fn requests_to(&self, path: &str) -> Vec<(String, String, String)> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, target, _)| target.starts_with(path))
                .cloned()
                .collect()
        }
    }

    #[derive(Default)]
    struct MemoryStoreState {
        bearer: Option<String>,
        state: Option<String>,
        verifier: Option<String>,
        clear_count: usize,
        commit_count: usize,
    }

    #[derive(Default)]
    struct MemoryStore {
        inner: Mutex<MemoryStoreState>,
        fail_clear: bool,
        fail_verifier: bool,
        supersede_clear: bool,
    }

    impl MemoryStore {
        fn with_clear_failure() -> Self {
            Self {
                fail_clear: true,
                ..Self::default()
            }
        }

        fn with_verifier_and_clear_failure() -> Self {
            Self {
                fail_clear: true,
                fail_verifier: true,
                ..Self::default()
            }
        }

        fn with_superseded_cleanup() -> Self {
            Self {
                supersede_clear: true,
                ..Self::default()
            }
        }

        fn snapshot(&self) -> (Option<String>, Option<String>, Option<String>, usize, usize) {
            let state = self.inner.lock().unwrap();
            (
                state.bearer.clone(),
                state.state.clone(),
                state.verifier.clone(),
                state.clear_count,
                state.commit_count,
            )
        }
    }

    impl OAuthFlowStore for MemoryStore {
        fn bearer<'a>(
            &'a self,
            _server_url: &'a str,
        ) -> BoxFuture<'a, Result<Option<String>, OAuthStoreError>> {
            Box::pin(async move { Ok(self.inner.lock().unwrap().bearer.clone()) })
        }

        fn state(&self) -> BoxFuture<'_, Result<Option<String>, OAuthStoreError>> {
            Box::pin(async move { Ok(self.inner.lock().unwrap().state.clone()) })
        }

        fn persist_state<'a>(
            &'a self,
            oauth_state: &'a str,
        ) -> BoxFuture<'a, Result<(), OAuthStoreError>> {
            Box::pin(async move {
                self.inner.lock().unwrap().state = Some(oauth_state.to_string());
                Ok(())
            })
        }

        fn persist_verifier<'a>(
            &'a self,
            verifier: &'a str,
        ) -> BoxFuture<'a, Result<(), OAuthStoreError>> {
            Box::pin(async move {
                if self.fail_verifier {
                    return Err(OAuthStoreError::failed("verifier write failed"));
                }
                self.inner.lock().unwrap().verifier = Some(verifier.to_string());
                Ok(())
            })
        }

        fn clear_pending(&self) -> BoxFuture<'_, Result<(), OAuthStoreError>> {
            Box::pin(async move {
                let mut state = self.inner.lock().unwrap();
                state.clear_count += 1;
                if self.supersede_clear {
                    return Err(OAuthStoreError::superseded("newer flow owns the entry"));
                }
                if self.fail_clear {
                    return Err(OAuthStoreError::failed("disk is read-only"));
                }
                state.state = None;
                state.verifier = None;
                Ok(())
            })
        }

        fn commit<'a>(
            &'a self,
            _server_url: &'a str,
            tokens: &'a OAuthTokens,
            _credentials: &'a ClientCredentials,
        ) -> BoxFuture<'a, Result<(), OAuthStoreError>> {
            Box::pin(async move {
                let mut state = self.inner.lock().unwrap();
                state.bearer = Some(tokens.access_token.clone());
                state.state = None;
                state.verifier = None;
                state.commit_count += 1;
                Ok(())
            })
        }
    }

    struct TokenEnumerator {
        accepted: String,
        calls: Mutex<Vec<Option<String>>>,
    }

    impl TokenEnumerator {
        fn new(accepted: &str) -> Self {
            Self {
                accepted: accepted.to_string(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl OAuthEnumerator<String> for TokenEnumerator {
        fn enumerate(
            &self,
            bearer: Option<String>,
        ) -> BoxFuture<'_, Result<Vec<String>, OAuthEnumerateError>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(bearer.clone());
                if bearer.as_deref() == Some(self.accepted.as_str()) {
                    Ok(vec!["tool".to_string()])
                } else {
                    Err(OAuthEnumerateError {
                        message: "authentication required".to_string(),
                        needs_auth: true,
                    })
                }
            })
        }
    }

    struct BlockingEnumerator {
        calls: AtomicUsize,
        entered: Notify,
        release: Notify,
    }

    impl BlockingEnumerator {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                entered: Notify::new(),
                release: Notify::new(),
            }
        }
    }

    impl OAuthEnumerator<String> for BlockingEnumerator {
        fn enumerate(
            &self,
            _bearer: Option<String>,
        ) -> BoxFuture<'_, Result<Vec<String>, OAuthEnumerateError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.notify_one();
                self.release.notified().await;
                Ok(vec!["tool".to_string()])
            })
        }
    }

    async fn free_callback_port() -> u16 {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        listener.local_addr().unwrap().port()
    }

    fn coordinator_config(
        stub: &StubAuthorizationServer,
        callback_port: u16,
    ) -> InteractiveOAuthConfig {
        let metadata =
            ClientMetadata::new("Test Host", "1.0").with_uri("https://example.test/host");
        InteractiveOAuthConfig {
            enabled: true,
            server_url: format!("{}/mcp", stub.base),
            redirect_uri: format!("http://127.0.0.1:{callback_port}{TEST_CALLBACK_PATH}"),
            scope: Some("read".to_string()),
            client_id: Some("configured-client".to_string()),
            client_secret: None,
            discovery_metadata: metadata.clone(),
            registration_metadata: metadata,
            page_copy: PageCopy::default(),
        }
    }

    fn deliver_callback(authorization_url: String) {
        tokio::spawn(async move {
            let authorization_url = Url::parse(&authorization_url).unwrap();
            let query: HashMap<_, _> = authorization_url
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            let state = query["state"].clone();
            let redirect = Url::parse(&query["redirect_uri"]).unwrap();
            let port = redirect.port().unwrap();
            let target = format!("{}?code=authorization-code&state={state}", redirect.path());

            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            stream
                .write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
        });
    }

    #[tokio::test]
    async fn interactive_coordinator_completes_and_commits_once() {
        let _serial = oauth_callback::TEST_SERIAL.lock().await;
        let coordinator = OAuthCoordinator::new();
        let stub = StubAuthorizationServer::start().await;
        let config = coordinator_config(&stub, free_callback_port().await);
        let store = MemoryStore::default();
        let enumerator = TokenEnumerator::new("access-1");

        let tools = coordinator
            .authenticate_interactive(
                "coordinator-success",
                &config,
                &store,
                &enumerator,
                &deliver_callback,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(tools, ["tool"]);
        assert_eq!(
            store.snapshot(),
            (Some("access-1".to_string()), None, None, 0, 1)
        );
        assert_eq!(
            *enumerator.calls.lock().unwrap(),
            [None, Some("access-1".to_string())]
        );
        assert!(!coordinator.callback_is_running());
        coordinator.shutdown().await;
    }

    #[tokio::test]
    async fn cleanup_failure_is_appended_to_the_primary_error() {
        let _serial = oauth_callback::TEST_SERIAL.lock().await;
        let coordinator = OAuthCoordinator::new();
        let stub = StubAuthorizationServer::start().await;
        let config = coordinator_config(&stub, free_callback_port().await);
        let store = MemoryStore::with_verifier_and_clear_failure();
        let enumerator = TokenEnumerator::new("never");

        let error = coordinator
            .authenticate_interactive(
                "coordinator-cleanup-failure",
                &config,
                &store,
                &enumerator,
                &|_| {},
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            error.contains("Could not persist the PKCE verifier: verifier write failed"),
            "{error}"
        );
        assert!(
            error.contains("additionally, could not clear pending OAuth state: disk is read-only"),
            "{error}"
        );
        let (_, state, _, clear_count, _) = store.snapshot();
        assert!(
            state.is_some(),
            "failed cleanup is visible, not silently erased"
        );
        assert_eq!(clear_count, 1);
        coordinator.shutdown().await;
    }

    #[tokio::test]
    async fn superseded_cleanup_does_not_replace_the_primary_error() {
        let store = MemoryStore::with_superseded_cleanup();
        let error = fail_flow::<String>(&store, "primary failure".to_string())
            .await
            .unwrap_err();
        assert_eq!(error, "primary failure");
        assert_eq!(store.snapshot().3, 1);
    }

    #[tokio::test]
    async fn caller_cancel_during_callback_surfaces_cleanup_failure() {
        let _serial = oauth_callback::TEST_SERIAL.lock().await;
        let coordinator = OAuthCoordinator::new();
        let stub = StubAuthorizationServer::start().await;
        let config = coordinator_config(&stub, free_callback_port().await);
        let store = MemoryStore::with_clear_failure();
        let enumerator = TokenEnumerator::new("access-1");
        let cancel = CancellationToken::new();
        let opened = Arc::new(Notify::new());
        let mark_opened = opened.clone();
        let on_open_url = move |_| mark_opened.notify_one();
        let flow = coordinator.authenticate_interactive(
            "coordinator-cancel-cleanup",
            &config,
            &store,
            &enumerator,
            &on_open_url,
            &cancel,
        );
        tokio::pin!(flow);
        tokio::select! {
            _ = opened.notified() => cancel.cancel(),
            result = &mut flow => panic!("flow ended before browser handoff: {result:?}"),
        }
        let error = flow.await.unwrap_err();
        assert!(error.starts_with(oauth_callback::ERR_CANCELLED), "{error}");
        assert!(error.contains("disk is read-only"), "{error}");
        assert_eq!(store.snapshot().3, 1);
        coordinator.shutdown().await;
    }

    #[tokio::test]
    async fn coordinator_cancel_reaches_the_callback_and_cleans_pending_state() {
        let _serial = oauth_callback::TEST_SERIAL.lock().await;
        let coordinator = OAuthCoordinator::new();
        let stub = StubAuthorizationServer::start().await;
        let config = coordinator_config(&stub, free_callback_port().await);
        let store = MemoryStore::default();
        let enumerator = TokenEnumerator::new("access-1");
        let opened = Arc::new(Notify::new());
        let mark_opened = opened.clone();
        let on_open_url = move |_| mark_opened.notify_one();
        let caller_cancel = CancellationToken::new();
        let flow = coordinator.authenticate_interactive(
            "coordinator-cancel-callback",
            &config,
            &store,
            &enumerator,
            &on_open_url,
            &caller_cancel,
        );
        tokio::pin!(flow);
        tokio::select! {
            _ = opened.notified() => {
                coordinator.cancel_interactive_authentication("coordinator-cancel-callback");
            }
            result = &mut flow => panic!("flow ended before browser handoff: {result:?}"),
        }
        assert_eq!(
            flow.await.unwrap_err(),
            oauth_callback::ERR_CANCELLED.to_string()
        );
        assert_eq!(store.snapshot(), (None, None, None, 1, 0));
        assert!(!coordinator.callback_is_running());
        coordinator.shutdown().await;
    }

    #[tokio::test]
    async fn coordinator_cancel_reaches_discovery_and_cleans_pending_state() {
        let _serial = oauth_callback::TEST_SERIAL.lock().await;
        let coordinator = OAuthCoordinator::new();
        let stall = StallingServer::start().await;
        let metadata = ClientMetadata::default();
        let config = InteractiveOAuthConfig {
            enabled: true,
            server_url: format!("{}/mcp", stall.base),
            redirect_uri: format!(
                "http://127.0.0.1:{}{TEST_CALLBACK_PATH}",
                free_callback_port().await
            ),
            scope: None,
            client_id: Some("client".to_string()),
            client_secret: None,
            discovery_metadata: metadata.clone(),
            registration_metadata: metadata,
            page_copy: PageCopy::default(),
        };
        let store = MemoryStore::default();
        let enumerator = TokenEnumerator::new("never");
        let caller_cancel = CancellationToken::new();
        let flow = coordinator.authenticate_interactive(
            "coordinator-cancel-discovery",
            &config,
            &store,
            &enumerator,
            &|_| {},
            &caller_cancel,
        );
        tokio::pin!(flow);
        tokio::select! {
            _ = stall.accepted.notified() => {
                coordinator.cancel_interactive_authentication("coordinator-cancel-discovery");
            }
            result = &mut flow => panic!("flow ended before stalled discovery: {result:?}"),
        }
        assert_eq!(
            flow.await.unwrap_err(),
            oauth_callback::ERR_CANCELLED.to_string()
        );
        assert_eq!(store.snapshot(), (None, None, None, 1, 0));
        assert!(!coordinator.callback_is_running());
        coordinator.shutdown().await;
    }

    #[tokio::test]
    async fn waiting_same_name_caller_cancel_does_not_touch_the_active_flow() {
        let coordinator = OAuthCoordinator::new();
        let config = InteractiveOAuthConfig {
            enabled: true,
            server_url: "https://mcp.test/mcp".to_string(),
            redirect_uri: "http://127.0.0.1:43123/callback".to_string(),
            scope: None,
            client_id: Some("client".to_string()),
            client_secret: None,
            discovery_metadata: ClientMetadata::default(),
            registration_metadata: ClientMetadata::default(),
            page_copy: PageCopy::default(),
        };
        let first_store = Arc::new(MemoryStore::default());
        let second_store = Arc::new(MemoryStore::default());
        let enumerator = Arc::new(BlockingEnumerator::new());

        let first = tokio::spawn({
            let coordinator = coordinator.clone();
            let config = config.clone();
            let store = first_store.clone();
            let enumerator = enumerator.clone();
            async move {
                coordinator
                    .authenticate_interactive(
                        "same-name-single-flight",
                        &config,
                        store.as_ref(),
                        enumerator.as_ref(),
                        &|_| {},
                        &CancellationToken::new(),
                    )
                    .await
            }
        });
        enumerator.entered.notified().await;

        let second_cancel = CancellationToken::new();
        let second = tokio::spawn({
            let coordinator = coordinator.clone();
            let config = config.clone();
            let store = second_store.clone();
            let enumerator = enumerator.clone();
            let cancel = second_cancel.clone();
            async move {
                coordinator
                    .authenticate_interactive(
                        "same-name-single-flight",
                        &config,
                        store.as_ref(),
                        enumerator.as_ref(),
                        &|_| {},
                        &cancel,
                    )
                    .await
            }
        });
        tokio::task::yield_now().await;
        second_cancel.cancel();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), second)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err(),
            oauth_callback::ERR_CANCELLED
        );
        assert_eq!(second_store.snapshot().3, 0);
        assert!(
            !first.is_finished(),
            "waiting caller cancelled the active flow"
        );
        assert_eq!(enumerator.calls.load(Ordering::SeqCst), 1);

        enumerator.release.notify_one();
        assert_eq!(first.await.unwrap().unwrap(), ["tool"]);
        assert_eq!(first_store.snapshot().3, 0);
    }

    #[tokio::test]
    async fn same_name_flows_execute_strictly_one_at_a_time() {
        let coordinator = OAuthCoordinator::new();
        let config = InteractiveOAuthConfig {
            enabled: true,
            server_url: "https://mcp.test/mcp".to_string(),
            redirect_uri: "http://127.0.0.1:43123/callback".to_string(),
            scope: None,
            client_id: Some("client".to_string()),
            client_secret: None,
            discovery_metadata: ClientMetadata::default(),
            registration_metadata: ClientMetadata::default(),
            page_copy: PageCopy::default(),
        };
        let first_store = Arc::new(MemoryStore::default());
        let second_store = Arc::new(MemoryStore::default());
        let enumerator = Arc::new(BlockingEnumerator::new());
        let spawn_flow = |store: Arc<MemoryStore>| {
            let coordinator = coordinator.clone();
            let config = config.clone();
            let enumerator = enumerator.clone();
            tokio::spawn(async move {
                coordinator
                    .authenticate_interactive(
                        "same-name-serialized",
                        &config,
                        store.as_ref(),
                        enumerator.as_ref(),
                        &|_| {},
                        &CancellationToken::new(),
                    )
                    .await
            })
        };

        let first = spawn_flow(first_store);
        enumerator.entered.notified().await;
        let second = spawn_flow(second_store);
        tokio::task::yield_now().await;
        assert_eq!(enumerator.calls.load(Ordering::SeqCst), 1);

        enumerator.release.notify_one();
        assert_eq!(first.await.unwrap().unwrap(), ["tool"]);
        enumerator.entered.notified().await;
        assert_eq!(enumerator.calls.load(Ordering::SeqCst), 2);
        enumerator.release.notify_one();
        assert_eq!(second.await.unwrap().unwrap(), ["tool"]);
    }

    #[tokio::test]
    async fn coordinator_cancel_reaches_the_preflight_enumerator_phase() {
        let coordinator = OAuthCoordinator::new();
        let config = InteractiveOAuthConfig {
            enabled: true,
            server_url: "https://mcp.test/mcp".to_string(),
            redirect_uri: "http://127.0.0.1:43123/callback".to_string(),
            scope: None,
            client_id: Some("client".to_string()),
            client_secret: None,
            discovery_metadata: ClientMetadata::default(),
            registration_metadata: ClientMetadata::default(),
            page_copy: PageCopy::default(),
        };
        let store = Arc::new(MemoryStore::default());
        let enumerator = Arc::new(BlockingEnumerator::new());
        let flow = tokio::spawn({
            let coordinator = coordinator.clone();
            let store = store.clone();
            let enumerator = enumerator.clone();
            async move {
                coordinator
                    .authenticate_interactive(
                        "coordinator-cancel-preflight",
                        &config,
                        store.as_ref(),
                        enumerator.as_ref(),
                        &|_| {},
                        &CancellationToken::new(),
                    )
                    .await
            }
        });
        enumerator.entered.notified().await;
        coordinator.cancel_interactive_authentication("coordinator-cancel-preflight");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), flow)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err(),
            oauth_callback::ERR_CANCELLED
        );
        assert_eq!(store.snapshot().3, 0);
    }

    #[tokio::test]
    async fn same_name_attempts_are_isolated_between_coordinator_instances() {
        let first_coordinator = OAuthCoordinator::new();
        let second_coordinator = OAuthCoordinator::new();
        let config = InteractiveOAuthConfig {
            enabled: true,
            server_url: "https://mcp.test/mcp".to_string(),
            redirect_uri: "http://127.0.0.1:43123/callback".to_string(),
            scope: None,
            client_id: Some("client".to_string()),
            client_secret: None,
            discovery_metadata: ClientMetadata::default(),
            registration_metadata: ClientMetadata::default(),
            page_copy: PageCopy::default(),
        };
        let first_store = Arc::new(MemoryStore::default());
        let second_store = Arc::new(MemoryStore::default());
        let first_enumerator = Arc::new(BlockingEnumerator::new());
        let second_enumerator = Arc::new(BlockingEnumerator::new());

        let first = tokio::spawn({
            let coordinator = first_coordinator.clone();
            let config = config.clone();
            let store = first_store.clone();
            let enumerator = first_enumerator.clone();
            async move {
                coordinator
                    .authenticate_interactive(
                        "shared-name",
                        &config,
                        store.as_ref(),
                        enumerator.as_ref(),
                        &|_| {},
                        &CancellationToken::new(),
                    )
                    .await
            }
        });
        let second = tokio::spawn({
            let coordinator = second_coordinator.clone();
            let config = config.clone();
            let store = second_store.clone();
            let enumerator = second_enumerator.clone();
            async move {
                coordinator
                    .authenticate_interactive(
                        "shared-name",
                        &config,
                        store.as_ref(),
                        enumerator.as_ref(),
                        &|_| {},
                        &CancellationToken::new(),
                    )
                    .await
            }
        });

        first_enumerator.entered.notified().await;
        second_enumerator.entered.notified().await;
        first_coordinator.cancel_interactive_authentication("shared-name");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), first)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err(),
            oauth_callback::ERR_CANCELLED
        );
        assert!(
            !second.is_finished(),
            "cancelling one embedding affected another embedding"
        );

        second_enumerator.release.notify_one();
        assert_eq!(second.await.unwrap().unwrap(), ["tool"]);
        assert_eq!(first_store.snapshot().3, 0);
        assert_eq!(second_store.snapshot().3, 0);
    }

    #[tokio::test]
    async fn discovery_registration_and_exchange_work_against_a_stub_server() {
        let stub = StubAuthorizationServer::start().await;
        let server_url = Url::parse(&format!("{}/mcp", stub.base)).unwrap();
        let host =
            ClientMetadata::new("Host App", "9.8.7").with_uri("https://example.test/application");

        let endpoints =
            discover_endpoints_with_metadata(discovery_http_client(), &server_url, &host)
                .await
                .unwrap();
        assert_eq!(
            endpoints.authorization_endpoint,
            format!("{}/authorize", stub.base)
        );
        assert_eq!(endpoints.token_endpoint, format!("{}/token", stub.base));
        assert_eq!(
            endpoints.registration_endpoint.as_deref(),
            Some(format!("{}/register", stub.base).as_str())
        );
        assert_eq!(endpoints.resource.as_deref(), Some(server_url.as_str()));

        let discovery_request = stub.requests_to("/mcp");
        assert_eq!(discovery_request.len(), 1);
        let discovery_body: Value = serde_json::from_str(&discovery_request[0].2).unwrap();
        assert_eq!(discovery_body["params"]["clientInfo"]["name"], "Host App");
        assert_eq!(discovery_body["params"]["clientInfo"]["version"], "9.8.7");

        let metadata = client_metadata(
            &host,
            "http://127.0.0.1:43123/oauth/callback",
            Some("read"),
            false,
        );
        let credentials = register_client(
            secret_http_client(),
            endpoints.registration_endpoint.as_deref().unwrap(),
            &metadata,
        )
        .await
        .unwrap();
        assert_eq!(credentials.client_id, "registered-client");
        assert_eq!(
            credentials.client_secret.as_deref(),
            Some("registered-secret")
        );
        assert!(credentials.from_registration);

        let before_exchange = now_secs();
        let tokens = exchange_code(
            secret_http_client(),
            &endpoints,
            &credentials,
            "authorization-code",
            "pkce-verifier",
            "http://127.0.0.1:43123/oauth/callback",
        )
        .await
        .unwrap();
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(tokens.scope.as_deref(), Some("read"));
        assert!(
            tokens
                .expires_at
                .is_some_and(|expires_at| expires_at >= before_exchange + 3599.0)
        );

        let registration_body: Value =
            serde_json::from_str(&stub.requests_to("/register")[0].2).unwrap();
        assert_eq!(registration_body["client_name"], "Host App");
        let token_form: HashMap<_, _> = Url::parse(&format!(
            "http://unused/?{}",
            stub.requests_to("/token")[0].2
        ))
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
        assert_eq!(token_form["code"], "authorization-code");
        assert_eq!(token_form["code_verifier"], "pkce-verifier");
        assert_eq!(token_form["client_id"], "registered-client");
        assert_eq!(token_form["client_secret"], "registered-secret");
        assert_eq!(token_form["resource"], server_url.as_str());
    }

    #[tokio::test]
    async fn credential_bearing_client_does_not_follow_redirects() {
        let stub = StubAuthorizationServer::start().await;
        let endpoint = format!("{}/redirect-register", stub.base);
        let metadata = json!({
            "client_name": "Host App",
            "client_secret": "must-not-be-forwarded",
        });

        let error = register_client(secret_http_client(), &endpoint, &metadata)
            .await
            .expect_err("the credential-bearing client must surface a 307");
        assert!(
            error.starts_with("Dynamic client registration failed (307)"),
            "{error}"
        );
        assert!(stub.requests_to("/collect").is_empty());

        let followed = register_client(discovery_http_client(), &endpoint, &metadata)
            .await
            .expect("the redirect-following discovery client is the negative control");
        assert_eq!(followed.client_id, "registered-client");
        assert_eq!(stub.requests_to("/collect").len(), 1);
    }
}
