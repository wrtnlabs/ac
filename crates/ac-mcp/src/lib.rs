//! MCP adapter: tools discovered from an MCP server materialize in the same
//! [`ToolRegistry`] as the built-ins, via the [`RawTool`] registration path.
//!
//! The kit's side of the seam is small on purpose:
//!
//! - [`McpConnection`] owns one client connection to one MCP server (rmcp
//!   underneath — the official MCP Rust SDK). Hosts connect over any rmcp
//!   transport; [`McpConnection::connect_command`] covers the common
//!   child-process (stdio) case.
//! - [`McpConnection::register_tools`] lists the server's tools and registers
//!   each as a [`RawTool`]: the server's name/description/input schema pass
//!   through verbatim, and `run` forwards the model's raw JSON arguments to
//!   the server's `tools/call`.
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
    /// An RPC on an established connection failed (transport or protocol).
    #[error("MCP request failed for server '{server}': {source}")]
    Service {
        server: String,
        #[source]
        source: ServiceError,
    },
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
        let service = ().serve(transport).await.map_err(|e| McpError::Connect {
            server: name.clone(),
            message: e.to_string(),
        })?;
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
            .map_err(|e| McpError::Service {
                server: self.name.clone(),
                source: e,
            })
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
        let prefix = match &options.prefix {
            ToolPrefix::ServerName => format!("mcp__{}__", self.name),
            ToolPrefix::None => String::new(),
            ToolPrefix::Custom(prefix) => prefix.clone(),
        };

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
                    .unwrap_or("(no description provided by the MCP server)")
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
            let arguments = match input {
                Value::Object(map) => Some(map),
                Value::Null => None,
                other => {
                    return ToolOutput::error(format!(
                        "invalid input for {}: expected a JSON object, got {}",
                        self.spec.name,
                        json_kind(&other)
                    ));
                }
            };

            let mut params = CallToolRequestParams::new(self.remote_name.clone());
            params.arguments = arguments;
            let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
            let options = match self.call_timeout {
                Some(timeout) => PeerRequestOptions::with_timeout(timeout),
                None => PeerRequestOptions::no_options(),
            };

            // A cancellable request, so a cancelled turn tells the server to
            // stop (notifications/cancelled) instead of silently abandoning a
            // possibly-mutating call to run to completion.
            let handle = match self.peer.send_cancellable_request(request, options).await {
                Ok(handle) => handle,
                Err(e) => {
                    return ToolOutput::error(format!(
                        "MCP call to '{}' on server '{}' failed: {e}",
                        self.remote_name, self.server
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
                    ToolOutput::error(format!("{} cancelled", self.spec.name))
                }
                result = handle.await_response() => match result {
                    Ok(ServerResult::CallToolResult(result)) => render_result(result),
                    Ok(_) => ToolOutput::error(format!(
                        "MCP call to '{}' on server '{}' returned an unexpected response type",
                        self.remote_name, self.server
                    )),
                    Err(ServiceError::Timeout { timeout }) => ToolOutput::error(format!(
                        "MCP call to '{}' on server '{}' timed out after {timeout:?}",
                        self.remote_name, self.server
                    )),
                    Err(e) => ToolOutput::error(format!(
                        "MCP call to '{}' on server '{}' failed: {e}",
                        self.remote_name, self.server
                    )),
                },
            }
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
