//! MCP adapter: tools discovered from an MCP server materialize in the same
//! [`ToolRegistry`] as the built-ins, via the [`RawTool`] registration path.
//!
//! The kit's side of the seam is small on purpose:
//!
//! - [`McpConnection`] owns one client connection to one MCP server (rmcp
//!   underneath — the official MCP Rust SDK). Hosts connect over any rmcp
//!   transport; [`McpConnection::connect_command`] covers the common
//!   child-process (stdio) case, and the opt-in `http` cargo feature adds
//!   `connect_http` for remote streamable-HTTP servers.
//! - [`McpConnection::register_tools`] lists the server's tools and registers
//!   each as a [`RawTool`]: the server's name/description/input schema pass
//!   through verbatim, and `run` forwards the model's raw JSON arguments to
//!   the server's `tools/call`.
//! - A run can start connection-free: [`McpConnection::export_catalog`]
//!   snapshots the discovered tools into serializable [`CachedToolSpec`]s a
//!   host persists, and [`register_cached`] registers them without dialing —
//!   the first call dials through a host-injected [`McpDialer`] and the live
//!   connection is memoized for the rest of the session.
//! - The opt-in `managed` module composes the standard control plane around
//!   those primitives: portable configuration, identity-bound catalog,
//!   credentials, probes, refresh/backfill, lazy mounting, and status. Hosts
//!   inject paths, process environment, and OAuth/RPC presentation policy.
//! - Failures are data, per AC doctrine: a transport error, a server-side
//!   `isError` result, or non-object arguments all come back as
//!   [`ToolOutput::error`] — never a panic, never a poisoned session.
//! - Cancellation composes: `run` races the remote call against
//!   `ctx.cancel`, so a cancelled turn is not held hostage by a slow server.
//!
//! Capability doctrine: MCP `ToolAnnotations` are *server-claimed hints* — the
//! spec itself says clients must not make trust decisions on them. So by
//! default every MCP tool registers as [`Capability::Mutating`], which keeps a
//! read-only permission mode safe against a server that lies about being
//! read-only. A host that trusts a server can opt in to honoring
//! `readOnlyHint` via [`RegisterOptions::trust_annotations`].

use std::io;
use std::sync::Arc;
use std::time::Duration;

use ac_tool::{Capability, RawTool, ToolCtx, ToolOutput, ToolRegistry};
use ac_types::ToolSpec;
use futures::future::BoxFuture;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotification,
    CancelledNotificationParam, ClientRequest, ContentBlock, PaginatedRequestParams,
    ResourceContents, ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions, RoleClient, RunningService};
use rmcp::transport::{IntoTransport, TokioChildProcess};
use rmcp::{ServiceError, ServiceExt};
use serde::Serialize;
use serde_json::Value;

/// Re-exported so hosts can reach rmcp's transports and model types without
/// declaring their own dependency (and without version skew).
pub use rmcp;

/// OAuth 2.1 protocol mechanics for remote MCP servers.
#[cfg(feature = "http")]
pub mod oauth;

/// Shared loopback callback state machine for interactive OAuth.
#[cfg(feature = "http")]
pub mod oauth_callback;

/// Managed MCP control plane: portable server configuration, offline catalog,
/// credential persistence, connection policy, and lifecycle orchestration.
///
/// This layer is opt-in with `managed`. It currently includes `http` because
/// the portable server registry supports remote OAuth servers.
#[cfg(feature = "managed")]
pub mod managed;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The host-chosen server name can't be used (see [`McpConnection::connect`]).
    #[error("invalid MCP server name '{server}': {reason}")]
    InvalidServerName { server: String, reason: String },
    /// Connecting or the MCP initialize handshake failed.
    #[error("MCP connect failed for server '{server}': {message}")]
    Connect { server: String, message: String },
    /// The server observably refused us as unauthorized (HTTP 401/403 or the
    /// transport's own auth-required signals). Distinguished so a host can
    /// route to (re)authentication instead of retrying blindly. Ambiguous
    /// failures stay [`Connect`](Self::Connect)/[`Service`](Self::Service) —
    /// only what the underlying error actually surfaces is classified.
    #[error("MCP server '{server}' requires authentication: {message}")]
    Auth { server: String, message: String },
    /// An RPC on an established connection failed (transport or protocol).
    ///
    /// The underlying transport error is rendered immediately rather than
    /// retained as a source: a configured header or environment value can be
    /// reflected by a remote server, and retaining the raw source would make
    /// both `Display` and derived `Debug` secret-bearing.
    #[error("MCP request failed for server '{server}': {message}")]
    Service { server: String, message: String },
}

pub const MAX_ERROR_BYTES: usize = 32 * 1024;

fn redact_secrets(mut message: String, secrets: &[String]) -> String {
    let mut secrets = secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .collect::<Vec<_>>();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    for secret in secrets {
        message = message.replace(secret, "[REDACTED]");
    }
    message
}

fn truncate_owned(mut message: String, limit: usize) -> (String, bool) {
    if message.len() <= limit {
        return (message, false);
    }
    let mut end = limit;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    (message, true)
}

pub(crate) fn redact_and_limit(
    message: String,
    secrets: &[String],
    limit: usize,
) -> (String, bool) {
    // Cap before replacement so a hostile transport string is not cloned in
    // full, then cap again because repeated short secrets expand to the
    // longer redaction marker.
    let (message, input_truncated) = truncate_owned(message, limit);
    let message = redact_truncated_secret_suffix(message, secrets, limit);
    let (message, redaction_truncated) = truncate_owned(message, limit);
    (message, input_truncated || redaction_truncated)
}

fn safe_error_message(message: String, secrets: &[String]) -> String {
    let (mut message, truncated) = redact_and_limit(message, secrets, MAX_ERROR_BYTES);
    if truncated {
        message.push_str("\n[truncated: MCP error exceeded 32 KiB]");
    }
    message
}

fn redact_mcp_error(error: McpError, secrets: &[String]) -> McpError {
    match error {
        McpError::InvalidServerName { server, reason } => McpError::InvalidServerName {
            server,
            reason: safe_error_message(reason, secrets),
        },
        McpError::Connect { server, message } => McpError::Connect {
            server,
            message: safe_error_message(message, secrets),
        },
        McpError::Auth { server, message } => McpError::Auth {
            server,
            message: safe_error_message(message, secrets),
        },
        McpError::Service { server, message } => McpError::Service {
            server,
            message: safe_error_message(message, secrets),
        },
    }
}

/// Signals the underlying stack actually emits on an unauthorized refusal:
/// rmcp's streamable-HTTP client renders 401-with-`WWW-Authenticate` as
/// "Auth required", 403-with-`WWW-Authenticate` as "Insufficient scope", and
/// header-less 401/403 responses as "HTTP <status>: <body>" where the status
/// carries the canonical reason phrase. Anything else is ambiguous and keeps
/// its existing class — a mislabeled connect error beats a guessed auth one.
fn indicates_unauthorized(message: &str) -> bool {
    message.contains("Auth required")
        || message.contains("Insufficient scope")
        || message.contains("HTTP 401")
        || message.contains("HTTP 403")
        || message.contains("401 Unauthorized")
        || message.contains("403 Forbidden")
}

fn connect_error(server: String, message: String) -> McpError {
    let needs_auth = indicates_unauthorized(&message);
    let message = safe_error_message(message, &[]);
    if needs_auth {
        McpError::Auth { server, message }
    } else {
        McpError::Connect { server, message }
    }
}

/// thiserror interpolates transitively, so rendering the [`ServiceError`]
/// exposes the transport error underneath it for classification.
fn service_error(server: &str, source: ServiceError, secrets: &[String]) -> McpError {
    let raw = source.to_string();
    let needs_auth = indicates_unauthorized(&raw);
    let message = safe_error_message(raw, secrets);
    if needs_auth {
        McpError::Auth {
            server: server.to_string(),
            message,
        }
    } else {
        McpError::Service {
            server: server.to_string(),
            message,
        }
    }
}

/// How discovered tool names appear in the registry.
///
/// Prefixing is the collision guard: two servers exporting `search`, or a
/// server exporting a name that shadows a built-in, must not silently replace
/// each other. The default namespaces by the host-chosen server name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ToolPrefix {
    /// `mcp__<server>__<tool>` — the default.
    #[default]
    ServerName,
    /// Register the server's tool names verbatim. Existing entries still win.
    None,
    /// A custom prefix, prepended verbatim.
    Custom(String),
}

#[derive(Debug, Clone)]
pub struct RegisterOptions {
    pub prefix: ToolPrefix,
    /// Honor the server's `readOnlyHint` annotation when classifying
    /// [`Capability`]. Off by default: annotations are unverified claims, and
    /// a read-only permission mode must not be bypassable by a server that
    /// mislabels a mutating tool. Enable only for servers the host trusts.
    pub trust_annotations: bool,
    /// Per-call deadline for `tools/call`. On expiry the call fails as error
    /// data and rmcp sends the server a `notifications/cancelled`. `None`
    /// waits forever — the host's cancel token becomes the only escape from a
    /// server that accepts a call and never responds. Default: 5 minutes.
    pub call_timeout: Option<Duration>,
}

impl Default for RegisterOptions {
    fn default() -> Self {
        Self {
            prefix: ToolPrefix::default(),
            trust_annotations: false,
            call_timeout: Some(Duration::from_secs(300)),
        }
    }
}

/// Auth for [`McpConnection::connect_http_with`], applied to every request
/// the transport makes (the streamable-HTTP transport is many requests, not
/// one connection).
#[cfg(feature = "http")]
#[derive(Clone, Default)]
pub struct HttpOptions {
    /// Sent as `Authorization: Bearer <token>`. The token only — no
    /// `Bearer ` prefix.
    pub bearer_token: Option<String>,
    /// Extra `(name, value)` headers. Invalid names or values fail the
    /// connect with a typed error, never a partial header set.
    pub headers: Vec<(String, String)>,
}

#[cfg(feature = "http")]
impl std::fmt::Debug for HttpOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpOptions")
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "header_names",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// What [`McpConnection::register_tools`] did — nothing is skipped silently.
#[derive(Debug, Clone, Default)]
pub struct RegisteredTools {
    /// Registry-visible names actually registered, in server order.
    pub registered: Vec<String>,
    /// Tools whose (prefixed) name can't survive a provider request, left out
    /// so one bad name can't poison every subsequent completion call.
    pub skipped: Vec<SkippedTool>,
}

#[derive(Debug, Clone)]
pub struct SkippedTool {
    /// The tool's name as the server declared it.
    pub remote_name: String,
    pub reason: String,
}

/// Provider APIs constrain tool names (OpenAI-routed models enforce
/// `^[a-zA-Z0-9_-]{1,64}$`; Anthropic allows 128). Names are resent with
/// every completion request, so one out-of-contract name would 400 every
/// remaining turn of the session — the kit enforces the strictest common
/// contract at registration instead.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Untrusted catalog ceilings. A server's `tools/list` response is durable
/// control-plane input, not an unbounded document store.
///
/// These limits are enforced on fully deserialized rmcp values. They bound
/// what AC retains and projects, but rmcp's transport framing must separately
/// bound bytes before deserialization to prevent hostile wire-size spikes.
pub const MAX_CATALOG_TOOLS: usize = 512;
pub const MAX_CATALOG_NAME_BYTES: usize = 256;
pub const MAX_CATALOG_DESCRIPTION_BYTES: usize = 16 * 1024;
pub const MAX_CATALOG_SCHEMA_BYTES: usize = 256 * 1024;
pub const MAX_CATALOG_SCHEMA_DEPTH: usize = 64;
pub const MAX_CATALOG_BYTES: usize = 4 * 1024 * 1024;
const MAX_CATALOG_PAGES: usize = MAX_CATALOG_TOOLS;
const MAX_CATALOG_CURSOR_BYTES: usize = 4 * 1024;

struct BoundedCounter {
    bytes: usize,
    limit: usize,
}

impl io::Write for BoundedCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("serialized byte count overflow"))?;
        if next > self.limit {
            return Err(io::Error::other("serialized value exceeds byte limit"));
        }
        self.bytes = next;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bounded_serialized_size(value: &impl Serialize, limit: usize) -> Result<usize, ()> {
    let mut counter = BoundedCounter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|_| ())?;
    Ok(counter.bytes)
}

fn bounded_json_size(value: &impl Serialize, limit: usize) -> Result<usize, String> {
    bounded_serialized_size(value, limit)
        .map_err(|()| format!("input schema exceeds {limit} serialized bytes"))
}

fn value_depth(value: &Value) -> usize {
    let mut maximum = 1;
    let mut stack = vec![(value, 1_usize)];
    while let Some((value, depth)) = stack.pop() {
        maximum = maximum.max(depth);
        match value {
            Value::Array(values) => {
                stack.extend(values.iter().map(|value| (value, depth.saturating_add(1))));
            }
            Value::Object(values) => {
                stack.extend(
                    values
                        .values()
                        .map(|value| (value, depth.saturating_add(1))),
                );
            }
            _ => {}
        }
        if maximum > MAX_CATALOG_SCHEMA_DEPTH {
            break;
        }
    }
    maximum
}

fn object_depth(value: &serde_json::Map<String, Value>) -> usize {
    value
        .values()
        .map(|value| value_depth(value).saturating_add(1))
        .max()
        .unwrap_or(1)
}

fn catalog_entry_size(
    name: &str,
    description: Option<&str>,
    schema: &impl Serialize,
    schema_depth: usize,
) -> Result<usize, String> {
    if name.len() > MAX_CATALOG_NAME_BYTES {
        return Err(format!(
            "tool name is {} bytes; catalog names are capped at {MAX_CATALOG_NAME_BYTES}",
            name.len()
        ));
    }
    let description_bytes = description.map_or(0, str::len);
    if description_bytes > MAX_CATALOG_DESCRIPTION_BYTES {
        return Err(format!(
            "tool description is {description_bytes} bytes; catalog descriptions are capped at \
             {MAX_CATALOG_DESCRIPTION_BYTES}"
        ));
    }
    if schema_depth > MAX_CATALOG_SCHEMA_DEPTH {
        return Err(format!(
            "tool input schema is {schema_depth} levels deep; catalog schemas are capped at \
             {MAX_CATALOG_SCHEMA_DEPTH}"
        ));
    }
    let schema_bytes = bounded_json_size(schema, MAX_CATALOG_SCHEMA_BYTES)?;
    name.len()
        .checked_add(description_bytes)
        .and_then(|bytes| bytes.checked_add(schema_bytes))
        .ok_or_else(|| "catalog entry byte count overflow".to_string())
}

fn add_catalog_bytes(total: &mut usize, entry: usize) -> Result<(), String> {
    *total = total
        .checked_add(entry)
        .ok_or_else(|| "catalog aggregate byte count overflow".to_string())?;
    if *total > MAX_CATALOG_BYTES {
        return Err(format!(
            "catalog exceeds the aggregate byte limit of {MAX_CATALOG_BYTES}"
        ));
    }
    Ok(())
}

#[cfg(feature = "http")]
fn validated_remote_url(configured: &str) -> Result<reqwest::Url, &'static str> {
    let parsed = reqwest::Url::parse(configured)
        .map_err(|_| "invalid configured URL: expected an absolute https:// URL")?;
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" => {
            let host = parsed.host_str().unwrap_or_default();
            let host = host
                .strip_prefix('[')
                .and_then(|host| host.strip_suffix(']'))
                .unwrap_or(host);
            let is_literal_loopback = host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback());
            if is_literal_loopback {
                Ok(parsed)
            } else {
                Err(
                    "insecure configured URL: remote MCP requires https://; http:// is allowed \
                     only for a literal loopback IP",
                )
            }
        }
        _ => Err("invalid configured URL: expected an absolute https:// URL"),
    }
}

/// With `__` and a trailing `_` both rejected, `mcp__<server>__<tool>`
/// decomposes uniquely: a longer server name matching the same string would
/// have to be `<server>_` (trailing underscore — rejected) or contain `__`
/// (rejected). Without the trailing-underscore rule, server `a` + tool `_x`
/// and server `a_` + tool `x` would both register as `mcp__a___x` and could
/// silently replace each other.
fn server_name_violation(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("name is empty".to_string());
    }
    if name.contains("__") {
        return Some(
            "name contains \"__\", the prefix delimiter — prefixed tool names would be \
             ambiguous across servers"
                .to_string(),
        );
    }
    if name.ends_with('_') {
        return Some(
            "name ends with '_' — prefixed tool names would be ambiguous across servers"
                .to_string(),
        );
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-')
    {
        return Some(format!("name contains {bad:?}; allowed: [A-Za-z0-9_-]"));
    }
    None
}

fn registry_prefix(prefix: &ToolPrefix, server: &str) -> String {
    match prefix {
        ToolPrefix::ServerName => format!("mcp__{server}__"),
        ToolPrefix::None => String::new(),
        ToolPrefix::Custom(prefix) => prefix.clone(),
    }
}

fn tool_name_violation(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("name is empty".to_string());
    }
    if name.len() > MAX_TOOL_NAME_LEN {
        return Some(format!(
            "name is {} bytes; provider tool names are capped at {MAX_TOOL_NAME_LEN}",
            name.len()
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-')
    {
        return Some(format!(
            "name contains {bad:?}; provider tool names allow only [A-Za-z0-9_-]"
        ));
    }
    None
}

/// One client connection to one MCP server.
///
/// The connection stays alive as long as this handle *or any registered tool*
/// exists (tools hold it, so a registry never contains dangling tools). After
/// [`shutdown`](Self::shutdown), in-flight and future calls fail as error
/// data.
pub struct McpConnection {
    service: Arc<RunningService<RoleClient, ()>>,
    name: String,
    redaction_secrets: Arc<Vec<String>>,
}

impl std::fmt::Debug for McpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnection")
            .field("name", &self.name)
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl McpConnection {
    /// Connect over any rmcp client transport. `name` is the host-chosen
    /// server name used for tool-name prefixing and diagnostics. It must be
    /// non-empty `[A-Za-z0-9_-]`, contain no double underscore, and not end
    /// with `_` — `__` is the prefix delimiter, and those rules make the
    /// `mcp__<server>__<tool>` decomposition unique so no two servers'
    /// tools can collide into one registry name.
    pub async fn connect<T, E, A>(name: impl Into<String>, transport: T) -> Result<Self, McpError>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let name = name.into();
        if let Some(reason) = server_name_violation(&name) {
            return Err(McpError::InvalidServerName {
                server: name,
                reason,
            });
        }
        let service =
            ().serve(transport)
                .await
                .map_err(|e| connect_error(name.clone(), e.to_string()))?;
        Ok(Self {
            service: Arc::new(service),
            name,
            redaction_secrets: Arc::new(Vec::new()),
        })
    }

    fn with_redaction_secrets(mut self, secrets: Vec<String>) -> Self {
        self.redaction_secrets = Arc::new(secrets);
        self
    }

    /// Connect to a stdio MCP server spawned as a child process — the common
    /// local-server case (`npx some-server`, a bundled binary, …).
    pub async fn connect_command(
        name: impl Into<String>,
        command: tokio::process::Command,
    ) -> Result<Self, McpError> {
        Self::connect_command_with_redaction(name, command, Vec::new()).await
    }

    /// Managed embeddings know which explicitly configured environment values
    /// are credentials. Base process environment (PATH, HOME, LANG, USER, …)
    /// must never be inferred as secret: doing so would corrupt ordinary MCP
    /// results containing those ubiquitous strings.
    pub(crate) async fn connect_command_with_redaction(
        name: impl Into<String>,
        command: tokio::process::Command,
        secrets: Vec<String>,
    ) -> Result<Self, McpError> {
        let name = name.into();
        let transport = TokioChildProcess::new(command)
            .map_err(|e| McpError::Connect {
                server: name.clone(),
                message: format!("spawn failed: {e}"),
            })
            .map_err(|error| redact_mcp_error(error, &secrets))?;
        Self::connect(name, transport)
            .await
            .map(|connection| connection.with_redaction_secrets(secrets.clone()))
            .map_err(|error| redact_mcp_error(error, &secrets))
    }

    /// Connect to a remote MCP server over the streamable-HTTP transport.
    /// Requires the `http` cargo feature.
    #[cfg(feature = "http")]
    pub async fn connect_http(
        name: impl Into<String>,
        url: impl AsRef<str>,
    ) -> Result<Self, McpError> {
        Self::connect_http_with(name, url, HttpOptions::default()).await
    }

    /// [`connect_http`](Self::connect_http) with per-request auth: a bearer
    /// token and/or extra headers, sent on every request the transport makes.
    #[cfg(feature = "http")]
    pub async fn connect_http_with(
        name: impl Into<String>,
        url: impl AsRef<str>,
        options: HttpOptions,
    ) -> Result<Self, McpError> {
        use rmcp::transport::StreamableHttpClientTransport;
        use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

        let name = name.into();
        let url = url.as_ref();
        // Validate before constructing the transport worker or moving any
        // credentials into it. Plaintext remote dials must never carry a
        // bearer token or configured header off-host; literal loopback IPs
        // are the sole HTTP exception.
        let parsed = validated_remote_url(url).map_err(|message| McpError::Connect {
            server: name.clone(),
            message: message.to_string(),
        })?;
        let mut secrets = options
            .bearer_token
            .iter()
            .chain(options.headers.iter().map(|(_, value)| value))
            .cloned()
            .collect::<Vec<_>>();
        // The URL itself can carry credentials (userinfo, query, fragment),
        // and transport/handshake errors often echo it. Capture this exact
        // dial snapshot alongside headers so lazy calls, probes, and live
        // enumeration all redact identically.
        if !url.is_empty() {
            secrets.push(url.to_string());
        }
        secrets.push(parsed.as_str().to_string());
        if !parsed.username().is_empty() {
            secrets.push(parsed.username().to_string());
        }
        if let Some(password) = parsed.password() {
            secrets.push(password.to_string());
        }
        secrets.extend(parsed.query_pairs().map(|(_, value)| value.into_owned()));
        if let Some(fragment) = parsed.fragment() {
            secrets.push(fragment.to_string());
        }
        let mut config = StreamableHttpClientTransportConfig::with_uri(parsed.as_str().to_string());
        if let Some(token) = options.bearer_token {
            config = config.auth_header(token);
        }
        if !options.headers.is_empty() {
            let mut headers = std::collections::HashMap::new();
            for (key, value) in &options.headers {
                let key = http::HeaderName::from_bytes(key.as_bytes())
                    .map_err(|e| McpError::Connect {
                        server: name.clone(),
                        message: format!("invalid header name '{key}': {e}"),
                    })
                    .map_err(|error| redact_mcp_error(error, &secrets))?;
                let value = http::HeaderValue::from_str(value)
                    .map_err(|e| McpError::Connect {
                        server: name.clone(),
                        message: format!("invalid value for header '{key}': {e}"),
                    })
                    .map_err(|error| redact_mcp_error(error, &secrets))?;
                headers.insert(key, value);
            }
            config = config.custom_headers(headers);
        }
        Self::connect(name, StreamableHttpClientTransport::from_config(config))
            .await
            .map(|connection| connection.with_redaction_secrets(secrets.clone()))
            .map_err(|error| redact_mcp_error(error, &secrets))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// True once the connection is gone — whether by [`shutdown`](Self::shutdown)
    /// or because the server side died on its own (child crash, stdin EOF).
    pub fn is_closed(&self) -> bool {
        // `RunningService::is_closed` only observes OUR cancellation; the
        // peer's tx closes when the serve task ends for any reason.
        self.service.is_closed() || self.service.peer().is_transport_closed()
    }

    /// The discovered tool list, bounded and validated page-by-page before it
    /// is retained, for hosts that want to inspect or filter before
    /// registering.
    pub async fn tools(&self) -> Result<Vec<rmcp::model::Tool>, McpError> {
        let peer = self.service.peer();
        let mut tools = Vec::new();
        let mut total_bytes = 0;
        let mut cursor: Option<String> = None;
        let mut pages = 0;

        loop {
            if pages == MAX_CATALOG_PAGES {
                return Err(McpError::Service {
                    server: self.name.clone(),
                    message: format!(
                        "server tool pagination exceeded the page limit of {MAX_CATALOG_PAGES}"
                    ),
                });
            }
            pages += 1;
            let requested_cursor = cursor.take();
            let result = peer
                .list_tools(Some(
                    PaginatedRequestParams::default().with_cursor(requested_cursor.clone()),
                ))
                .await
                .map_err(|error| service_error(&self.name, error, &self.redaction_secrets))?;

            let declared =
                tools
                    .len()
                    .checked_add(result.tools.len())
                    .ok_or_else(|| McpError::Service {
                        server: self.name.clone(),
                        message: "server tool count overflowed".to_string(),
                    })?;
            if declared > MAX_CATALOG_TOOLS {
                return Err(McpError::Service {
                    server: self.name.clone(),
                    message: format!(
                        "server declared at least {declared} tools; catalogs are capped at \
                         {MAX_CATALOG_TOOLS}"
                    ),
                });
            }

            for tool in &result.tools {
                // Keep the field-specific diagnostics, then charge the full
                // protocol object to the aggregate budget. `Tool` also
                // carries title, outputSchema, icons, execution, annotations,
                // and _meta; ignored fields must not bypass the catalog cap.
                catalog_entry_size(
                    &tool.name,
                    tool.description.as_deref(),
                    tool.input_schema.as_ref(),
                    object_depth(tool.input_schema.as_ref()),
                )
                .map_err(|reason| McpError::Service {
                    server: self.name.clone(),
                    message: format!("refused tool catalog entry '{}': {reason}", tool.name),
                })?;
                let entry_bytes =
                    bounded_serialized_size(tool, MAX_CATALOG_BYTES).map_err(|()| {
                        McpError::Service {
                            server: self.name.clone(),
                            message: format!(
                                "refused tool catalog entry '{}': full tool definition exceeds \
                                 {MAX_CATALOG_BYTES} serialized bytes",
                                tool.name
                            ),
                        }
                    })?;
                add_catalog_bytes(&mut total_bytes, entry_bytes).map_err(|reason| {
                    McpError::Service {
                        server: self.name.clone(),
                        message: reason,
                    }
                })?;
            }
            tools.extend(result.tools);

            let Some(next_cursor) = result.next_cursor else {
                break;
            };
            if tools.len() == MAX_CATALOG_TOOLS {
                return Err(McpError::Service {
                    server: self.name.clone(),
                    message: format!(
                        "server tool pagination continued after reaching the catalog limit of \
                         {MAX_CATALOG_TOOLS}"
                    ),
                });
            }
            if next_cursor.len() > MAX_CATALOG_CURSOR_BYTES {
                return Err(McpError::Service {
                    server: self.name.clone(),
                    message: format!(
                        "server returned a pagination cursor larger than \
                         {MAX_CATALOG_CURSOR_BYTES} bytes"
                    ),
                });
            }
            if requested_cursor.as_deref() == Some(next_cursor.as_str()) {
                return Err(McpError::Service {
                    server: self.name.clone(),
                    message: "server repeated the same tool pagination cursor".to_string(),
                });
            }
            cursor = Some(next_cursor);
        }
        Ok(tools)
    }

    /// Snapshot the server's discovered tools into serializable specs a host
    /// can persist and later feed to [`register_cached`] — starting a run
    /// with no live connection at all.
    pub async fn export_catalog(&self) -> Result<Vec<CachedToolSpec>, McpError> {
        let tools = self.tools().await?;
        Ok(tools
            .iter()
            .map(|tool| CachedToolSpec {
                name: tool.name.to_string(),
                registry_name: None,
                description: tool.description.as_deref().map(str::to_string),
                input_schema: Value::Object((*tool.input_schema).clone()),
                read_only_hint: tool.annotations.as_ref().and_then(|a| a.read_only_hint),
            })
            .collect())
    }

    /// Discover the server's tools and register each into `registry`. Tools
    /// whose (prefixed) name would violate the provider tool-name contract
    /// are skipped and reported in [`RegisteredTools::skipped`], never
    /// registered — one hostile or sloppy name must not 400 every subsequent
    /// completion request. Dynamic tools never replace an existing registry
    /// entry: built-ins and earlier MCP tools win, while the collision is
    /// reported in [`RegisteredTools::skipped`].
    pub async fn register_tools(
        &self,
        registry: &mut ToolRegistry,
        options: &RegisterOptions,
    ) -> Result<RegisteredTools, McpError> {
        let tools = self.tools().await?;
        let prefix = registry_prefix(&options.prefix, &self.name);

        let mut result = RegisteredTools::default();
        for tool in tools {
            let remote_name = tool.name.to_string();
            // Checked bare: under a non-empty prefix an empty remote name
            // would otherwise slip through as a delimiter-only registry name.
            if remote_name.is_empty() {
                result.skipped.push(SkippedTool {
                    remote_name,
                    reason: "server declared an empty tool name".to_string(),
                });
                continue;
            }
            let registry_name = format!("{prefix}{remote_name}");
            if let Some(reason) = tool_name_violation(&registry_name) {
                result.skipped.push(SkippedTool {
                    remote_name,
                    reason,
                });
                continue;
            }
            if registry.contains(&registry_name) {
                result.skipped.push(SkippedTool {
                    remote_name,
                    reason: format!("registry already contains a tool named '{registry_name}'"),
                });
                continue;
            }
            let capability = if options.trust_annotations
                && let Some(annotations) = &tool.annotations
                && annotations.read_only_hint == Some(true)
            {
                Capability::ReadOnly
            } else {
                Capability::Mutating
            };
            let spec = ToolSpec {
                name: registry_name.clone(),
                description: tool
                    .description
                    .as_deref()
                    .unwrap_or(NO_DESCRIPTION)
                    .to_string(),
                input_schema: Value::Object((*tool.input_schema).clone()),
            };
            registry.register_raw(McpTool {
                peer: self.service.peer().clone(),
                _keepalive: self.service.clone(),
                remote_name,
                server: self.name.clone(),
                redaction_secrets: self.redaction_secrets.clone(),
                spec,
                capability,
                call_timeout: options.call_timeout,
            });
            result.registered.push(registry_name);
        }
        Ok(result)
    }

    /// Cancel the connection. Registered tools remain in the registry but
    /// every subsequent call returns error data — the model sees a failed
    /// tool, the session survives.
    ///
    /// Cleanup (transport close; for child-process transports, waiting out
    /// and killing the child) runs on the connection's detached background
    /// task with bounded waits — this method does not await it. A host that
    /// shuts down and immediately tears down its async runtime may leave a
    /// child that ignores stdin-EOF running; keep the runtime alive briefly
    /// after shutdown if that matters.
    pub fn shutdown(&self) {
        self.service.cancellation_token().cancel();
    }
}

/// A discovered MCP tool bridged into the registry. Not constructed directly —
/// [`McpConnection::register_tools`] is the entry point.
struct McpTool {
    peer: Peer<RoleClient>,
    /// Keeps the connection's background task alive while the tool is
    /// registered, so a registry can outlive the [`McpConnection`] handle.
    _keepalive: Arc<RunningService<RoleClient, ()>>,
    remote_name: String,
    server: String,
    redaction_secrets: Arc<Vec<String>>,
    spec: ToolSpec,
    capability: Capability,
    call_timeout: Option<Duration>,
}

impl RawTool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn capability(&self) -> Capability {
        self.capability
    }

    fn run(self: Arc<Self>, input: Value, ctx: Arc<ToolCtx>) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            call_remote(
                RemoteCall {
                    peer: self.peer.clone(),
                    server: &self.server,
                    remote_name: &self.remote_name,
                    registry_name: &self.spec.name,
                    call_timeout: self.call_timeout,
                    redaction_secrets: self.redaction_secrets.clone(),
                    ctx: &ctx,
                },
                input,
            )
            .await
        })
    }
}

struct RemoteCall<'a> {
    peer: Peer<RoleClient>,
    server: &'a str,
    remote_name: &'a str,
    registry_name: &'a str,
    call_timeout: Option<Duration>,
    redaction_secrets: Arc<Vec<String>>,
    ctx: &'a ToolCtx,
}

/// The one remote-call path both tool shapes share: shape-only argument
/// validation, a cancellable `tools/call`, and the cancel/timeout race.
async fn call_remote(call: RemoteCall<'_>, input: Value) -> ToolOutput {
    let RemoteCall {
        peer,
        server,
        remote_name,
        registry_name,
        call_timeout,
        redaction_secrets,
        ctx,
    } = call;
    let arguments = match input {
        Value::Object(map) => Some(map),
        Value::Null => None,
        other => {
            return ToolOutput::error(format!(
                "invalid input for {registry_name}: expected a JSON object, got {}",
                json_kind(&other)
            ));
        }
    };

    let mut params = CallToolRequestParams::new(remote_name.to_string());
    params.arguments = arguments;
    let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
    let options = match call_timeout {
        Some(timeout) => PeerRequestOptions::with_timeout(timeout),
        None => PeerRequestOptions::no_options(),
    };

    // A cancellable request, so a cancelled turn tells the server to
    // stop (notifications/cancelled) instead of silently abandoning a
    // possibly-mutating call to run to completion.
    let handle = match peer.send_cancellable_request(request, options).await {
        Ok(handle) => handle,
        Err(e) => {
            return ToolOutput::error(format!(
                "MCP call to '{remote_name}' on server '{server}' failed: {}",
                safe_error_message(e.to_string(), &redaction_secrets)
            ));
        }
    };
    let request_id = handle.id.clone();
    let peer = handle.peer.clone();

    tokio::select! {
        _ = ctx.cancel.cancelled() => {
            let notification = CancelledNotification::new(CancelledNotificationParam::new(
                Some(request_id),
                Some("cancelled by host".to_string()),
            ));
            // Best effort, bounded: a transport wedged mid-write must
            // not hang the very turn cancellation exists to escape.
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                peer.send_notification(notification.into()),
            )
            .await;
            ToolOutput::error(format!("{registry_name} cancelled"))
        }
        result = handle.await_response() => match result {
            Ok(ServerResult::CallToolResult(result)) => {
                render_result(result, &redaction_secrets)
            },
            Ok(_) => ToolOutput::error(format!(
                "MCP call to '{remote_name}' on server '{server}' returned an unexpected response type"
            )),
            Err(ServiceError::Timeout { timeout }) => ToolOutput::error(format!(
                "MCP call to '{remote_name}' on server '{server}' timed out after {timeout:?}"
            )),
            Err(e) => ToolOutput::error(format!(
                "MCP call to '{remote_name}' on server '{server}' failed: {}",
                safe_error_message(e.to_string(), &redaction_secrets)
            )),
        },
    }
}

/// The explicit stand-in for a server that declared no description — a
/// placeholder the model can read, never an empty string.
const NO_DESCRIPTION: &str = "(no description provided by the MCP server)";

/// A serializable snapshot of one discovered tool: what a host persists so a
/// later run can register the server's tools before any connection exists.
/// Produced by [`McpConnection::export_catalog`]; consumed by
/// [`register_cached`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CachedToolSpec {
    /// The tool's name as the server declared it (unprefixed).
    pub name: String,
    /// Optional host-chosen provider-safe registry name. The raw [`Self::name`]
    /// is still sent to the MCP server on `tools/call`.
    ///
    /// This lets a host persist one stable public name even when the remote
    /// name contains characters provider APIs reject (for example `.`), or
    /// when it applies collision-safe truncation. `None` preserves
    /// [`RegisterOptions::prefix`] composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_name: Option<String>,
    pub description: Option<String>,
    /// The server's declared input schema, verbatim.
    pub input_schema: Value,
    /// The server's `readOnlyHint` annotation, if it declared one. Still an
    /// unverified claim — honored only under
    /// [`RegisterOptions::trust_annotations`], exactly like the live path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
}

/// Host-injected connection factory for [`register_cached`]. Called on the
/// first tool call (never at registration), and again only if a dial failed
/// or a memoized connection has since died.
pub type McpDialer =
    Arc<dyn Fn() -> BoxFuture<'static, Result<McpConnection, McpError>> + Send + Sync>;

/// Validate a durable/offline catalog before it crosses a persistence or
/// registration boundary. Custom enumerators must pass through the same
/// limits as [`McpConnection::export_catalog`].
pub fn validate_cached_catalog(
    server_name: &str,
    specs: &[CachedToolSpec],
) -> Result<(), McpError> {
    if specs.len() > MAX_CATALOG_TOOLS {
        return Err(McpError::Service {
            server: server_name.to_string(),
            message: format!(
                "cached catalog contains {} tools; catalogs are capped at {MAX_CATALOG_TOOLS}",
                specs.len()
            ),
        });
    }
    let mut catalog_bytes = 0;
    for cached in specs {
        let entry_bytes = catalog_entry_size(
            &cached.name,
            cached.description.as_deref(),
            &cached.input_schema,
            value_depth(&cached.input_schema),
        )
        .map_err(|reason| McpError::Service {
            server: server_name.to_string(),
            message: format!("refused cached tool '{}': {reason}", cached.name),
        })?;
        add_catalog_bytes(&mut catalog_bytes, entry_bytes).map_err(|reason| McpError::Service {
            server: server_name.to_string(),
            message: reason,
        })?;
    }
    Ok(())
}

/// Register a server's cached tools without dialing it. The registered tools
/// share one lazily-dialed connection: the first call dials through `dialer`
/// and memoizes the result for every subsequent call on any of them. A failed
/// dial is one failed tool result — nothing is poisoned, the next call
/// retries.
///
/// Name validation, capability classification, and skip accounting match
/// [`McpConnection::register_tools`]. By default names use the configured
/// prefix; a cached spec may instead carry a stable host-chosen
/// [`CachedToolSpec::registry_name`] while preserving its raw remote call
/// name. `server_name` is held to [`McpConnection::connect`]'s rules, invalid
/// public names are skipped and reported, and every tool defaults to
/// [`Capability::Mutating`] unless the host opted into trusting annotations.
// The pre-existing `McpError::Service` variant embeds rmcp's `ServiceError`
pub fn register_cached(
    registry: &mut ToolRegistry,
    server_name: &str,
    specs: &[CachedToolSpec],
    dialer: McpDialer,
    options: &RegisterOptions,
) -> Result<RegisteredTools, McpError> {
    if let Some(reason) = server_name_violation(server_name) {
        return Err(McpError::InvalidServerName {
            server: server_name.to_string(),
            reason,
        });
    }
    validate_cached_catalog(server_name, specs)?;
    let prefix = registry_prefix(&options.prefix, server_name);
    let slot = Arc::new(tokio::sync::Mutex::new(None));

    let mut result = RegisteredTools::default();
    for cached in specs {
        if cached.name.is_empty() {
            result.skipped.push(SkippedTool {
                remote_name: String::new(),
                reason: "server declared an empty tool name".to_string(),
            });
            continue;
        }
        let registry_name = cached
            .registry_name
            .clone()
            .unwrap_or_else(|| format!("{prefix}{}", cached.name));
        if let Some(reason) = tool_name_violation(&registry_name) {
            result.skipped.push(SkippedTool {
                remote_name: cached.name.clone(),
                reason,
            });
            continue;
        }
        if registry.contains(&registry_name) {
            result.skipped.push(SkippedTool {
                remote_name: cached.name.clone(),
                reason: format!("registry already contains a tool named '{registry_name}'"),
            });
            continue;
        }
        let capability = if options.trust_annotations && cached.read_only_hint == Some(true) {
            Capability::ReadOnly
        } else {
            Capability::Mutating
        };
        let spec = ToolSpec {
            name: registry_name.clone(),
            description: cached
                .description
                .clone()
                .unwrap_or_else(|| NO_DESCRIPTION.to_string()),
            input_schema: cached.input_schema.clone(),
        };
        registry.register_raw(CachedMcpTool {
            server: server_name.to_string(),
            remote_name: cached.name.clone(),
            spec,
            capability,
            call_timeout: options.call_timeout,
            dialer: dialer.clone(),
            slot: slot.clone(),
        });
        result.registered.push(registry_name);
    }
    Ok(result)
}

/// A cached tool bridged into the registry with no live connection behind it.
/// Not constructed directly — [`register_cached`] is the entry point.
struct CachedMcpTool {
    server: String,
    remote_name: String,
    spec: ToolSpec,
    capability: Capability,
    call_timeout: Option<Duration>,
    dialer: McpDialer,
    /// Shared by every tool of one [`register_cached`] batch: whichever tool
    /// is called first dials once for the whole server. Also the keepalive —
    /// the memoized connection lives as long as any of the batch's tools.
    slot: Arc<tokio::sync::Mutex<Option<McpConnection>>>,
}

impl CachedMcpTool {
    /// A dial failure leaves the slot as it was, so the next call retries; a
    /// memoized connection whose transport has since died is re-dialed.
    async fn connection(&self) -> Result<McpConnection, McpError> {
        let mut slot = self.slot.lock().await;
        if let Some(conn) = slot.as_ref()
            && !conn.is_closed()
        {
            return Ok(McpConnection {
                service: conn.service.clone(),
                name: conn.name.clone(),
                redaction_secrets: conn.redaction_secrets.clone(),
            });
        }
        let conn = (self.dialer)().await?;
        let result = McpConnection {
            service: conn.service.clone(),
            name: conn.name.clone(),
            redaction_secrets: conn.redaction_secrets.clone(),
        };
        *slot = Some(conn);
        Ok(result)
    }
}

impl RawTool for CachedMcpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn capability(&self) -> Capability {
        self.capability
    }

    fn run(self: Arc<Self>, input: Value, ctx: Arc<ToolCtx>) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let connection = tokio::select! {
                _ = ctx.cancel.cancelled() => {
                    return ToolOutput::error(format!("{} cancelled", self.spec.name));
                }
                dialed = self.connection() => match dialed {
                    Ok(connection) => connection,
                    // Errors-as-data: the dial failure is the tool result the
                    // model reads, and the session survives.
                    Err(e) => {
                        return ToolOutput::error(format!(
                            "MCP dial for server '{}' failed: {e}",
                            self.server
                        ));
                    }
                },
            };
            call_remote(
                RemoteCall {
                    peer: connection.service.peer().clone(),
                    server: &self.server,
                    remote_name: &self.remote_name,
                    registry_name: &self.spec.name,
                    call_timeout: self.call_timeout,
                    redaction_secrets: connection.redaction_secrets.clone(),
                    ctx: &ctx,
                },
                input,
            )
            .await
        })
    }
}

/// Rendered results are capped like every built-in caps its output (`fetch`
/// caps at 256 KiB): tool results live in the message history and are resent
/// to the provider on every remaining iteration, so an unbounded server
/// response would tax the whole session.
pub const MAX_RESULT_BYTES: usize = 256 * 1024;
/// Maximum serialized protocol envelope accepted for one tool result.
///
/// This is deliberately larger than [`MAX_RESULT_BYTES`]: a useful textual
/// prefix can still be rendered and truncated, while ignored structured
/// content, media payloads, and `_meta` cannot grow without a ceiling.
/// Like the catalog limits, this applies after rmcp deserialization.
pub const MAX_RESULT_SERIALIZED_BYTES: usize = 4 * 1024 * 1024;

/// Flatten a `CallToolResult` into the single text block a tool result is.
/// Text content passes through; text resources contribute their text;
/// non-text content is noted, not dropped silently. `isError` maps straight
/// onto [`ToolOutput::error`].
struct BoundedResultText {
    content: String,
    truncated: bool,
}

impl BoundedResultText {
    fn new() -> Self {
        Self {
            content: String::with_capacity(MAX_RESULT_BYTES.min(16 * 1024)),
            truncated: false,
        }
    }

    fn push(&mut self, text: &str) {
        if self.truncated || text.is_empty() {
            return;
        }
        let remaining = MAX_RESULT_BYTES.saturating_sub(self.content.len());
        if text.len() <= remaining {
            self.content.push_str(text);
            return;
        }
        let mut end = remaining.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        self.content.push_str(&text[..end]);
        self.truncated = true;
    }

    fn start_part(&mut self) {
        if !self.content.is_empty() {
            self.push("\n\n");
        }
    }
}

impl io::Write for BoundedResultText {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let text = std::str::from_utf8(buffer)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "JSON writer emitted UTF-8"))?;
        self.push(text);
        if self.truncated {
            Err(io::Error::other("MCP result exceeded byte limit"))
        } else {
            Ok(buffer.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn redact_truncated_secret_suffix(mut content: String, secrets: &[String], limit: usize) -> String {
    const MARKER: &str = "[REDACTED]";
    content = redact_secrets(content, secrets);
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        for end in (1..secret.len()).rev() {
            if secret.is_char_boundary(end) && content.ends_with(&secret[..end]) {
                let prefix_end = content.len() - end;
                let mut retained = prefix_end.min(limit.saturating_sub(MARKER.len()));
                while retained > 0 && !content.is_char_boundary(retained) {
                    retained -= 1;
                }
                content.truncate(retained);
                content.push_str(MARKER);
                break;
            }
        }
    }
    content
}

fn safe_image_media_type(media_type: &str) -> bool {
    fn token(bytes: &[u8]) -> bool {
        !bytes.is_empty()
            && bytes.iter().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'!' | b'#'
                            | b'$'
                            | b'%'
                            | b'&'
                            | b'\''
                            | b'*'
                            | b'+'
                            | b'-'
                            | b'.'
                            | b'^'
                            | b'_'
                            | b'`'
                            | b'|'
                            | b'~'
                    )
            })
    }

    let Some((kind, subtype)) = media_type.split_once('/') else {
        return false;
    };
    kind.eq_ignore_ascii_case("image") && token(kind.as_bytes()) && token(subtype.as_bytes())
}

fn valid_standard_base64(data: &str) -> bool {
    if data.is_empty() || data.len() % 4 == 1 {
        return false;
    }
    let bytes = data.as_bytes();
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 || (padding > 0 && !bytes.len().is_multiple_of(4)) {
        return false;
    }
    let body_end = bytes.len().saturating_sub(padding);
    bytes[..body_end]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
        && bytes[body_end..].iter().all(|byte| *byte == b'=')
}

fn image_is_safe_to_forward(media_type: &str, data: &str, redaction_secrets: &[String]) -> bool {
    safe_image_media_type(media_type)
        && valid_standard_base64(data)
        && !redaction_secrets.iter().any(|secret| {
            !secret.is_empty() && (media_type.contains(secret) || data.contains(secret))
        })
}

fn render_result(result: CallToolResult, redaction_secrets: &[String]) -> ToolOutput {
    // Check the complete protocol object before rendering. Looking only at
    // textual content misses structuredContent, media data, and `_meta`.
    // rmcp has already deserialized the message at this point; transport-level
    // framing limits remain a separate lower-layer responsibility.
    if bounded_serialized_size(&result, MAX_RESULT_SERIALIZED_BYTES).is_err() {
        return ToolOutput::error(format!(
            "MCP server result exceeded the serialized byte limit of \
             {MAX_RESULT_SERIALIZED_BYTES}"
        ));
    }
    let is_error = result.is_error.unwrap_or(false);
    let mut output = BoundedResultText::new();
    let mut transient_images: Vec<(String, Arc<str>)> = Vec::new();
    for block in &result.content {
        output.start_part();
        match block {
            ContentBlock::Text(text) => output.push(&text.text),
            ContentBlock::Resource(resource) => match &resource.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    output.push("[resource ");
                    output.push(uri);
                    output.push("]\n");
                    output.push(text);
                }
                ResourceContents::BlobResourceContents { uri, .. } => {
                    output.push("[binary resource ");
                    output.push(uri);
                    output.push(" omitted]");
                }
                _ => output.push("[resource content omitted]"),
            },
            ContentBlock::Image(image) => {
                output.push("[image ");
                output.push(&image.mime_type);
                output.push(" omitted]");
                if !is_error
                    && image_is_safe_to_forward(&image.mime_type, &image.data, redaction_secrets)
                {
                    transient_images.push((
                        image.mime_type.clone(),
                        Arc::<str>::from(image.data.as_str()),
                    ));
                }
            }
            ContentBlock::Audio(audio) => {
                output.push("[audio ");
                output.push(&audio.mime_type);
                output.push(" omitted]");
            }
            ContentBlock::ResourceLink(link) => {
                output.push("[resource link: ");
                output.push(&link.uri);
                output.push("]");
            }
            _ => output.push("[unsupported content block omitted]"),
        }
        if output.truncated {
            break;
        }
    }

    if output.content.is_empty()
        && let Some(structured) = &result.structured_content
    {
        let _ = serde_json::to_writer_pretty(&mut output, structured);
    }
    if output.content.is_empty() {
        output.push("(the MCP server returned no content)");
    }
    let (mut content, redaction_truncated) =
        redact_and_limit(output.content, redaction_secrets, MAX_RESULT_BYTES);
    if output.truncated || redaction_truncated {
        content.push_str("\n[truncated: the MCP server's result exceeded 256 KiB]");
    }

    if is_error {
        ToolOutput::error(content)
    } else {
        transient_images
            .into_iter()
            .fold(ToolOutput::ok(content), |output, (media_type, data)| {
                output.with_image(media_type, data)
            })
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, Meta};

    #[cfg(feature = "http")]
    #[test]
    fn http_options_debug_never_exposes_auth_values() {
        let options = HttpOptions {
            bearer_token: Some("bearer-secret".to_string()),
            headers: vec![("X-Api-Key".to_string(), "header-secret".to_string())],
        };
        let rendered = format!("{options:?}");
        assert!(rendered.contains("X-Api-Key"));
        assert!(!rendered.contains("bearer-secret"));
        assert!(!rendered.contains("header-secret"));
    }

    #[cfg(feature = "http")]
    #[test]
    fn remote_url_policy_requires_tls_except_literal_loopback() {
        for accepted in [
            "https://mcp.example.test/rpc",
            "https://127.0.0.1/rpc",
            "http://127.0.0.1:8123/rpc",
            "http://127.255.255.254/rpc",
            "http://[::1]:8123/rpc",
        ] {
            assert!(
                validated_remote_url(accepted).is_ok(),
                "expected {accepted:?} to be accepted"
            );
        }
        for rejected in [
            "http://mcp.example.test/rpc",
            "http://localhost:8123/rpc",
            "http://192.168.1.8/rpc",
            "ftp://127.0.0.1/rpc",
            "not a URL",
        ] {
            assert!(
                validated_remote_url(rejected).is_err(),
                "expected {rejected:?} to be rejected"
            );
        }
    }

    #[test]
    fn rendered_results_redact_connection_secrets() {
        let out = render_result(
            CallToolResult::success(vec![ContentBlock::text("server reflected header-secret")]),
            &["header-secret".to_string()],
        );
        assert_eq!(out.content, "server reflected [REDACTED]");
    }

    #[test]
    fn render_flattens_text_and_bridges_success_images_transiently() {
        let result = CallToolResult::success(vec![
            ContentBlock::text("hello"),
            ContentBlock::image("aGk=", "image/png"),
            ContentBlock::text("world"),
        ]);
        let out = render_result(result, &[]);
        assert!(!out.is_error);
        assert_eq!(out.content, "hello\n\n[image image/png omitted]\n\nworld");
        assert_eq!(out.durable_content(), out.content);
        assert!(!out.durable_content().contains("aGk="));
        assert_eq!(
            out.transient_parts,
            vec![ac_tool::ToolOutputPart::Image {
                media_type: "image/png".to_string(),
                data: Arc::from("aGk="),
            }]
        );
    }

    #[test]
    fn render_maps_is_error_and_empty_content() {
        let out = render_result(
            CallToolResult::error(vec![
                ContentBlock::text("boom"),
                ContentBlock::image("aGk=", "image/png"),
            ]),
            &[],
        );
        assert!(out.is_error);
        assert_eq!(out.content, "boom\n\n[image image/png omitted]");
        assert!(out.transient_parts.is_empty());
        assert!(!out.durable_content().contains("aGk="));

        let out = render_result(CallToolResult::success(vec![]), &[]);
        assert!(!out.is_error);
        assert!(out.content.contains("no content"));
    }

    #[test]
    fn render_falls_back_to_structured_content() {
        let mut result = CallToolResult::success(vec![]);
        result.structured_content = Some(serde_json::json!({ "answer": 42 }));
        let out = render_result(result, &[]);
        assert!(!out.is_error);
        assert!(out.content.contains("\"answer\": 42"));
    }

    #[test]
    fn complete_result_budget_covers_structured_content_and_meta() {
        let oversized = "x".repeat(MAX_RESULT_SERIALIZED_BYTES + 1);
        let mut structured = CallToolResult::success(vec![ContentBlock::text("small")]);
        structured.structured_content =
            Some(serde_json::json!({ "ignored_payload": oversized.clone() }));

        let mut meta = CallToolResult::success(vec![ContentBlock::text("small")]);
        meta.meta = Some(Meta(serde_json::Map::from_iter([(
            "ignored_payload".to_string(),
            Value::String(oversized),
        )])));

        for result in [structured, meta] {
            let out = render_result(result, &[]);
            assert!(out.is_error);
            assert!(
                out.content.contains("serialized byte limit"),
                "{}",
                out.content
            );
            assert!(out.content.len() < 256);
        }
    }

    #[test]
    fn oversized_image_result_fails_before_any_transient_payload_is_created() {
        let data = "x".repeat(MAX_RESULT_SERIALIZED_BYTES + 1);
        let out = render_result(
            CallToolResult::success(vec![ContentBlock::image(data, "image/png")]),
            &[],
        );
        assert!(out.is_error);
        assert!(out.content.contains("serialized byte limit"));
        assert!(out.content.len() < 256);
        assert!(out.transient_parts.is_empty());
    }

    #[test]
    fn malformed_or_secret_bearing_images_remain_durable_omissions() {
        for (data, media_type, secrets) in [
            ("not base64", "image/png", Vec::new()),
            ("aGk=", "image/png;name=bad", Vec::new()),
            (
                "header-secret",
                "image/png",
                vec!["header-secret".to_string()],
            ),
        ] {
            let out = render_result(
                CallToolResult::success(vec![ContentBlock::image(data, media_type)]),
                &secrets,
            );
            assert!(!out.is_error);
            assert!(out.transient_parts.is_empty());
            assert!(!out.durable_content().contains(data));
            assert!(!out.durable_content().contains("header-secret"));
        }
    }

    #[test]
    fn render_caps_oversized_results() {
        let big = "y".repeat(MAX_RESULT_BYTES + 1000);
        let out = render_result(CallToolResult::success(vec![ContentBlock::text(big)]), &[]);
        assert!(!out.is_error);
        assert!(out.content.len() < MAX_RESULT_BYTES + 100);
        assert!(out.content.ends_with("exceeded 256 KiB]"));
    }

    #[test]
    fn render_truncates_on_a_char_boundary() {
        // A 2-byte ASCII prefix puts a 4-byte char astride the byte cap, so
        // a naive `truncate(MAX_RESULT_BYTES)` would panic here.
        let mut big = String::from("ab");
        big.push_str(&"\u{1F980}".repeat(MAX_RESULT_BYTES / 4));
        let out = render_result(CallToolResult::success(vec![ContentBlock::text(big)]), &[]);
        assert!(!out.is_error);
        assert!(out.content.len() < MAX_RESULT_BYTES + 100);
        assert!(out.content.ends_with("exceeded 256 KiB]"));
    }

    #[test]
    fn tool_names_are_held_to_the_provider_contract() {
        assert!(tool_name_violation(&"a".repeat(64)).is_none());
        assert!(tool_name_violation(&"a".repeat(65)).is_some());
        assert!(tool_name_violation("").is_some());
        assert!(tool_name_violation("ok_name-123").is_none());
        assert!(tool_name_violation("bad name").is_some());
        assert!(tool_name_violation("bad.name").is_some());
        assert!(tool_name_violation("héllo").is_some());
    }

    #[test]
    fn auth_is_classified_only_on_observable_signals() {
        // What the HTTP stack actually emits on an unauthorized refusal:
        // rmcp's AuthRequired / InsufficientScope displays, and header-less
        // 401/403 responses rendered with their status line.
        assert!(indicates_unauthorized(
            "Transport [streamable http client] error: Auth required"
        ));
        assert!(indicates_unauthorized("Insufficient scope"));
        assert!(indicates_unauthorized(
            "HTTP 401 Unauthorized: missing bearer token"
        ));
        assert!(indicates_unauthorized(
            "HTTP status client error (403 Forbidden) for url (http://localhost/mcp)"
        ));
        // Ambiguity keeps the existing class — never guessed into Auth.
        assert!(!indicates_unauthorized("connection refused"));
        assert!(!indicates_unauthorized("spawn failed: no such file"));
        assert!(!indicates_unauthorized("request timeout after PT300S"));
    }

    #[test]
    fn connect_and_service_failures_route_to_the_auth_class() {
        let err = connect_error("test".into(), "HTTP 401 Unauthorized: nope".into());
        assert!(matches!(err, McpError::Auth { .. }), "{err}");
        let err = connect_error("test".into(), "connection refused".into());
        assert!(matches!(err, McpError::Connect { .. }), "{err}");

        // A synthetic transport error with the auth signal one level down:
        // thiserror renders transitively, so classification still sees it.
        let auth: Box<dyn std::error::Error + Send + Sync> = "Auth required".into();
        let source =
            ServiceError::TransportSend(rmcp::transport::DynamicTransportError::from_parts(
                "test-transport",
                std::any::TypeId::of::<()>(),
                auth,
            ));
        assert!(matches!(
            service_error("test", source, &[]),
            McpError::Auth { .. }
        ));

        let plain: Box<dyn std::error::Error + Send + Sync> = "broken pipe".into();
        let source =
            ServiceError::TransportSend(rmcp::transport::DynamicTransportError::from_parts(
                "test-transport",
                std::any::TypeId::of::<()>(),
                plain,
            ));
        assert!(matches!(
            service_error("test", source, &[]),
            McpError::Service { .. }
        ));
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn connect_http_rejects_non_http_urls_without_dialing() {
        let configured = "ftp://user:secret@example.invalid/mcp?token=private";
        let err = McpConnection::connect_http("test", configured)
            .await
            .expect_err("must reject");
        assert!(matches!(err, McpError::Connect { .. }), "{err}");
        assert!(err.to_string().contains("https://"), "{err}");
        assert!(!err.to_string().contains(configured), "{err}");
        assert!(!err.to_string().contains("secret"), "{err}");
        assert!(!err.to_string().contains("private"), "{err}");
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn connect_http_rejects_plaintext_remote_credentials_without_dialing() {
        let options = HttpOptions {
            bearer_token: Some("bearer-secret".into()),
            headers: vec![("X-Api-Key".into(), "header-secret".into())],
        };
        let err =
            McpConnection::connect_http_with("test", "http://mcp.example.invalid/rpc", options)
                .await
                .expect_err("plaintext remote URL must be rejected before dialing");
        assert!(matches!(err, McpError::Connect { .. }), "{err}");
        assert!(err.to_string().contains("requires https://"), "{err}");
        assert!(!err.to_string().contains("bearer-secret"), "{err}");
        assert!(!err.to_string().contains("header-secret"), "{err}");
        assert!(!err.to_string().contains("mcp.example.invalid"), "{err}");
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn connect_http_rejects_malformed_headers_without_dialing() {
        let options = HttpOptions {
            bearer_token: None,
            headers: vec![("bad header name".into(), "header-secret".into())],
        };
        let err = McpConnection::connect_http_with("test", "http://127.0.0.1:9/mcp", options)
            .await
            .expect_err("must reject");
        assert!(matches!(err, McpError::Connect { .. }), "{err}");
        assert!(err.to_string().contains("invalid header name"), "{err}");
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn connect_http_holds_the_server_name_to_the_prefix_rules() {
        let err = McpConnection::connect_http("a__b", "http://127.0.0.1:9/mcp")
            .await
            .expect_err("must reject");
        assert!(matches!(err, McpError::InvalidServerName { .. }), "{err}");
    }

    #[test]
    fn server_names_cannot_break_the_prefix_scheme() {
        assert!(server_name_violation("test").is_none());
        assert!(server_name_violation("a_b-2").is_none());
        assert!(server_name_violation("").is_some());
        assert!(server_name_violation("a__b").is_some());
        assert!(server_name_violation("has space").is_some());
        // Trailing underscore: server "a" + tool "_x" and server "a_" +
        // tool "x" would both be "mcp__a___x".
        assert!(server_name_violation("a_").is_some());
    }

    #[test]
    fn cached_tool_can_keep_a_raw_remote_name_behind_a_provider_safe_public_name() {
        let mut registry = ToolRegistry::new();
        let dialer: McpDialer = Arc::new(|| {
            Box::pin(async {
                Err(McpError::Connect {
                    server: "server".to_string(),
                    message: "not dialed by registration".to_string(),
                })
            })
        });
        let result = register_cached(
            &mut registry,
            "server",
            &[CachedToolSpec {
                name: "issues.list".to_string(),
                registry_name: Some("mcp__server__issues_list".to_string()),
                description: Some("List issues".to_string()),
                input_schema: serde_json::json!({ "type": "object" }),
                read_only_hint: None,
            }],
            dialer,
            &RegisterOptions::default(),
        )
        .unwrap();

        assert_eq!(result.registered, ["mcp__server__issues_list"]);
        assert!(result.skipped.is_empty());
        assert!(registry.contains("mcp__server__issues_list"));
        assert!(!registry.contains("mcp__server__issues.list"));
    }

    #[test]
    fn cached_catalog_rejects_oversized_entries_before_registration() {
        let mut registry = ToolRegistry::new();
        let dialer: McpDialer = Arc::new(|| {
            Box::pin(async {
                Err(McpError::Connect {
                    server: "server".to_string(),
                    message: "must not dial".to_string(),
                })
            })
        });
        let error = register_cached(
            &mut registry,
            "server",
            &[CachedToolSpec {
                name: "large".to_string(),
                registry_name: None,
                description: Some("x".repeat(MAX_CATALOG_DESCRIPTION_BYTES + 1)),
                input_schema: serde_json::json!({ "type": "object" }),
                read_only_hint: None,
            }],
            dialer,
            &RegisterOptions::default(),
        )
        .expect_err("oversized durable catalog input must fail closed");
        assert!(error.to_string().contains("description"));
        assert!(!registry.contains("mcp__server__large"));
    }

    #[test]
    fn result_rendering_stops_after_the_bounded_prefix() {
        let blocks = (0..8)
            .map(|_| ContentBlock::text("x".repeat(MAX_RESULT_BYTES)))
            .collect();
        let out = render_result(CallToolResult::success(blocks), &[]);
        assert!(out.content.len() < MAX_RESULT_BYTES + 100);
        assert!(out.content.ends_with("exceeded 256 KiB]"));
    }

    #[test]
    fn truncated_secret_prefix_is_not_exposed() {
        let secret = "secret-that-crosses-the-cap";
        let mut body = "x".repeat(MAX_RESULT_BYTES - 4);
        body.push_str(secret);
        let out = render_result(
            CallToolResult::success(vec![ContentBlock::text(body)]),
            &[secret.to_string()],
        );
        assert!(!out.content.ends_with("secr"));
        assert!(!out.content.contains("secret-that"));
        assert!(out.content.contains("[REDACTED]"));
    }

    #[test]
    fn redaction_growth_is_recapped() {
        let out = render_result(
            CallToolResult::success(vec![ContentBlock::text("x".repeat(MAX_RESULT_BYTES))]),
            &["x".to_string()],
        );
        assert!(out.content.len() < MAX_RESULT_BYTES + 100);
        assert!(out.content.ends_with("exceeded 256 KiB]"));
    }
}
