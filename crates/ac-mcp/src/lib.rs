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

use std::sync::Arc;
use std::time::Duration;

use ac_tool::{Capability, RawTool, ToolCtx, ToolOutput, ToolRegistry};
use ac_types::ToolSpec;
use futures::future::BoxFuture;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotification,
    CancelledNotificationParam, ClientRequest, ContentBlock, ResourceContents, ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions, RoleClient, RunningService};
use rmcp::transport::{IntoTransport, TokioChildProcess};
use rmcp::{ServiceError, ServiceExt};
use serde_json::Value;

/// Re-exported so hosts can reach rmcp's transports and model types without
/// declaring their own dependency (and without version skew).
pub use rmcp;

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
    #[error("MCP request failed for server '{server}': {source}")]
    Service {
        server: String,
        #[source]
        source: ServiceError,
    },
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
    if indicates_unauthorized(&message) {
        McpError::Auth { server, message }
    } else {
        McpError::Connect { server, message }
    }
}

/// thiserror interpolates transitively, so rendering the [`ServiceError`]
/// exposes the transport error underneath it for classification.
fn service_error(server: &str, source: ServiceError) -> McpError {
    let message = source.to_string();
    if indicates_unauthorized(&message) {
        McpError::Auth {
            server: server.to_string(),
            message,
        }
    } else {
        McpError::Service {
            server: server.to_string(),
            source,
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
    /// Register the server's tool names verbatim. Collisions replace.
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
#[derive(Debug, Clone, Default)]
pub struct HttpOptions {
    /// Sent as `Authorization: Bearer <token>`. The token only — no
    /// `Bearer ` prefix.
    pub bearer_token: Option<String>,
    /// Extra `(name, value)` headers. Invalid names or values fail the
    /// connect with a typed error, never a partial header set.
    pub headers: Vec<(String, String)>,
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
        })
    }

    /// Connect to a stdio MCP server spawned as a child process — the common
    /// local-server case (`npx some-server`, a bundled binary, …).
    pub async fn connect_command(
        name: impl Into<String>,
        command: tokio::process::Command,
    ) -> Result<Self, McpError> {
        let name = name.into();
        let transport = TokioChildProcess::new(command).map_err(|e| McpError::Connect {
            server: name.clone(),
            message: format!("spawn failed: {e}"),
        })?;
        Self::connect(name, transport).await
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
        // Checked before the transport spawns its worker: a bad URL should be
        // one crisp error here, not a confusing request failure at handshake.
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(McpError::Connect {
                server: name,
                message: format!("invalid URL '{url}': expected an http:// or https:// URL"),
            });
        }
        let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
        if let Some(token) = options.bearer_token {
            config = config.auth_header(token);
        }
        if !options.headers.is_empty() {
            let mut headers = std::collections::HashMap::new();
            for (key, value) in &options.headers {
                let key = http::HeaderName::from_bytes(key.as_bytes()).map_err(|e| {
                    McpError::Connect {
                        server: name.clone(),
                        message: format!("invalid header name '{key}': {e}"),
                    }
                })?;
                let value = http::HeaderValue::from_str(value).map_err(|e| McpError::Connect {
                    server: name.clone(),
                    message: format!("invalid value for header '{key}': {e}"),
                })?;
                headers.insert(key, value);
            }
            config = config.custom_headers(headers);
        }
        Self::connect(name, StreamableHttpClientTransport::from_config(config)).await
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

    /// The raw discovered tool list, for hosts that want to inspect or filter
    /// before registering.
    pub async fn tools(&self) -> Result<Vec<rmcp::model::Tool>, McpError> {
        self.service
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| service_error(&self.name, e))
    }

    /// Snapshot the server's discovered tools into serializable specs a host
    /// can persist and later feed to [`register_cached`] — starting a run
    /// with no live connection at all.
    pub async fn export_catalog(&self) -> Result<Vec<CachedToolSpec>, McpError> {
        Ok(self
            .tools()
            .await?
            .iter()
            .map(|tool| CachedToolSpec {
                name: tool.name.to_string(),
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
    /// completion request. Within one server, a duplicated tool name replaces
    /// the earlier entry — same semantics as every other registration path.
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
                self.peer.clone(),
                &self.server,
                &self.remote_name,
                &self.spec.name,
                self.call_timeout,
                input,
                &ctx,
            )
            .await
        })
    }
}

/// The one remote-call path both tool shapes share: shape-only argument
/// validation, a cancellable `tools/call`, and the cancel/timeout race.
async fn call_remote(
    peer: Peer<RoleClient>,
    server: &str,
    remote_name: &str,
    registry_name: &str,
    call_timeout: Option<Duration>,
    input: Value,
    ctx: &ToolCtx,
) -> ToolOutput {
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
                "MCP call to '{remote_name}' on server '{server}' failed: {e}"
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
            Ok(ServerResult::CallToolResult(result)) => render_result(result),
            Ok(_) => ToolOutput::error(format!(
                "MCP call to '{remote_name}' on server '{server}' returned an unexpected response type"
            )),
            Err(ServiceError::Timeout { timeout }) => ToolOutput::error(format!(
                "MCP call to '{remote_name}' on server '{server}' timed out after {timeout:?}"
            )),
            Err(e) => ToolOutput::error(format!(
                "MCP call to '{remote_name}' on server '{server}' failed: {e}"
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

/// Register a server's cached tools without dialing it. The registered tools
/// share one lazily-dialed connection: the first call dials through `dialer`
/// and memoizes the result for every subsequent call on any of them. A failed
/// dial is one failed tool result — nothing is poisoned, the next call
/// retries.
///
/// Name validation, prefixing, capability classification, and skip accounting
/// are exactly [`McpConnection::register_tools`]'s: `server_name` is held to
/// the same rules as [`McpConnection::connect`]'s name, contract-violating
/// tool names are skipped and reported, and every tool defaults to
/// [`Capability::Mutating`] unless the host opted into trusting annotations.
// The pre-existing `McpError::Service` variant embeds rmcp's `ServiceError`
// (large by clippy's threshold); this is just the crate's first non-async fn
// returning it, and boxing the variant would churn the public error contract.
#[allow(clippy::result_large_err)]
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
        let registry_name = format!("{prefix}{}", cached.name);
        if let Some(reason) = tool_name_violation(&registry_name) {
            result.skipped.push(SkippedTool {
                remote_name: cached.name.clone(),
                reason,
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
    async fn service(&self) -> Result<Arc<RunningService<RoleClient, ()>>, McpError> {
        let mut slot = self.slot.lock().await;
        if let Some(conn) = slot.as_ref()
            && !conn.is_closed()
        {
            return Ok(conn.service.clone());
        }
        let conn = (self.dialer)().await?;
        let service = conn.service.clone();
        *slot = Some(conn);
        Ok(service)
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
            let service = tokio::select! {
                _ = ctx.cancel.cancelled() => {
                    return ToolOutput::error(format!("{} cancelled", self.spec.name));
                }
                dialed = self.service() => match dialed {
                    Ok(service) => service,
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
                service.peer().clone(),
                &self.server,
                &self.remote_name,
                &self.spec.name,
                self.call_timeout,
                input,
                &ctx,
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

/// Flatten a `CallToolResult` into the single text block a tool result is.
/// Text content passes through; text resources contribute their text;
/// non-text content is noted, not dropped silently. `isError` maps straight
/// onto [`ToolOutput::error`].
fn render_result(result: CallToolResult) -> ToolOutput {
    let is_error = result.is_error.unwrap_or(false);
    let mut parts: Vec<String> = Vec::with_capacity(result.content.len());
    for block in &result.content {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::Resource(resource) => match &resource.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    parts.push(format!("[resource {uri}]\n{text}"));
                }
                ResourceContents::BlobResourceContents { uri, .. } => {
                    parts.push(format!("[binary resource {uri} omitted]"));
                }
                _ => parts.push("[resource content omitted]".to_string()),
            },
            ContentBlock::Image(image) => {
                parts.push(format!("[image {} omitted]", image.mime_type));
            }
            ContentBlock::Audio(audio) => {
                parts.push(format!("[audio {} omitted]", audio.mime_type));
            }
            ContentBlock::ResourceLink(link) => {
                parts.push(format!("[resource link: {}]", link.uri));
            }
            _ => parts.push("[unsupported content block omitted]".to_string()),
        }
    }

    let mut content = parts.join("\n\n");
    if content.is_empty()
        && let Some(structured) = &result.structured_content
    {
        content =
            serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string());
    }
    if content.is_empty() {
        content = "(the MCP server returned no content)".to_string();
    }
    if content.len() > MAX_RESULT_BYTES {
        let mut end = MAX_RESULT_BYTES;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
        content.push_str("\n[truncated: the MCP server's result exceeded 256 KiB]");
    }

    if is_error {
        ToolOutput::error(content)
    } else {
        ToolOutput::ok(content)
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
    use rmcp::model::CallToolResult;

    #[test]
    fn render_flattens_text_and_notes_non_text() {
        let result = CallToolResult::success(vec![
            ContentBlock::text("hello"),
            ContentBlock::image("aGk=", "image/png"),
            ContentBlock::text("world"),
        ]);
        let out = render_result(result);
        assert!(!out.is_error);
        assert_eq!(out.content, "hello\n\n[image image/png omitted]\n\nworld");
    }

    #[test]
    fn render_maps_is_error_and_empty_content() {
        let out = render_result(CallToolResult::error(vec![ContentBlock::text("boom")]));
        assert!(out.is_error);
        assert_eq!(out.content, "boom");

        let out = render_result(CallToolResult::success(vec![]));
        assert!(!out.is_error);
        assert!(out.content.contains("no content"));
    }

    #[test]
    fn render_falls_back_to_structured_content() {
        let mut result = CallToolResult::success(vec![]);
        result.structured_content = Some(serde_json::json!({ "answer": 42 }));
        let out = render_result(result);
        assert!(!out.is_error);
        assert!(out.content.contains("\"answer\": 42"));
    }

    #[test]
    fn render_caps_oversized_results() {
        let big = "y".repeat(MAX_RESULT_BYTES + 1000);
        let out = render_result(CallToolResult::success(vec![ContentBlock::text(big)]));
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
        let out = render_result(CallToolResult::success(vec![ContentBlock::text(big)]));
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
            service_error("test", source),
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
            service_error("test", source),
            McpError::Service { .. }
        ));
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn connect_http_rejects_non_http_urls_without_dialing() {
        let err = McpConnection::connect_http("test", "ftp://example.invalid/mcp")
            .await
            .expect_err("must reject");
        assert!(matches!(err, McpError::Connect { .. }), "{err}");
        assert!(err.to_string().contains("http://"), "{err}");
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn connect_http_rejects_malformed_headers_without_dialing() {
        let options = HttpOptions {
            bearer_token: None,
            headers: vec![("bad header name".into(), "v".into())],
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
}
