//! Hermetic end-to-end tests for the MCP adapter: a real rmcp server (served
//! in-process over a duplex pipe — real protocol, real framing, no network)
//! is discovered, registered, and driven through the same [`ToolRegistry`]
//! and runtime loop the built-ins use.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ac_mcp::{
    MAX_TOOL_NAME_LEN, McpConnection, McpDialer, McpError, RegisterOptions, ToolPrefix,
    register_cached,
};
use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::{AgentConfig, AgentEvent, Session};
use ac_tool::{Capability, SubtreePolicy, ToolCtx, ToolRegistry};
use ac_types::StopReason;
use futures::future::BoxFuture;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, CancelledNotificationParam, ContentBlock, ErrorData,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    ToolAnnotations,
};
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::{RoleServer, ServerHandler, ServiceExt};
use serde_json::json;
use tokio::sync::mpsc;

fn schema(value: serde_json::Value) -> JsonObject {
    value
        .as_object()
        .expect("schema literal is an object")
        .clone()
}

/// Fits the provider contract raw (61 <= 64) but overflows it once the
/// `mcp__test__` prefix (11 bytes) is prepended.
fn long_name() -> String {
    "x".repeat(61)
}

/// Observable server-side state, so tests can assert what actually reached
/// the server (e.g. a cancellation notification).
#[derive(Clone, Default)]
struct ServerState {
    cancelled: Arc<AtomicBool>,
}

#[derive(Clone, Default)]
struct TestServer {
    state: ServerState,
}

impl ServerHandler for TestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut echo = Tool::new(
            "echo",
            "Echoes text back.",
            schema(json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            })),
        );
        // The server *claims* echo is read-only; whether the client believes
        // it is exactly what trust_annotations decides.
        echo.annotations = Some(ToolAnnotations::new().read_only(true));
        let add = Tool::new(
            "add",
            "Adds two numbers.",
            schema(json!({
                "type": "object",
                "properties": { "a": { "type": "number" }, "b": { "type": "number" } },
                "required": ["a", "b"],
            })),
        );
        let fail = Tool::new(
            "always_fails",
            "Always fails.",
            schema(json!({ "type": "object" })),
        );
        let slow = Tool::new(
            "slow",
            "Sleeps forever.",
            schema(json!({ "type": "object" })),
        );
        // Hostile / sloppy names the adapter must refuse to register.
        let hostile = Tool::new(
            "bad name!",
            "Name violates the provider contract.",
            schema(json!({ "type": "object" })),
        );
        let long = Tool::new(
            long_name(),
            "Name only overflows once prefixed.",
            schema(json!({ "type": "object" })),
        );
        let empty = Tool::new(
            "",
            "Spec-violating empty name.",
            schema(json!({ "type": "object" })),
        );
        Ok(ListToolsResult::with_all_items(vec![
            echo, add, fail, slow, hostile, long, empty,
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        match request.name.as_ref() {
            "echo" => {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
            }
            "add" => {
                match (
                    args.get("a").and_then(|v| v.as_f64()),
                    args.get("b").and_then(|v| v.as_f64()),
                ) {
                    (Some(a), Some(b)) => Ok(CallToolResult::success(vec![ContentBlock::text(
                        format!("{}", a + b),
                    )])),
                    _ => Ok(CallToolResult::error(vec![ContentBlock::text(
                        "add requires numeric a and b",
                    )])),
                }
            }
            "always_fails" => Ok(CallToolResult::error(vec![ContentBlock::text(
                "deliberate failure",
            )])),
            "slow" => {
                tokio::time::sleep(Duration::from_secs(300)).await;
                Ok(CallToolResult::success(vec![ContentBlock::text("done")]))
            }
            other => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "unknown tool: {other}"
            ))])),
        }
    }

    async fn on_cancelled(
        &self,
        _notification: CancelledNotificationParam,
        _context: NotificationContext<RoleServer>,
    ) {
        self.state.cancelled.store(true, Ordering::SeqCst);
    }
}

/// A real client↔server MCP session over an in-memory duplex pipe.
async fn connect() -> (McpConnection, ServerState) {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let server = TestServer::default();
    let state = server.state.clone();
    tokio::spawn(async move {
        let service = server.serve(server_io).await.expect("server handshake");
        let _ = service.waiting().await;
    });
    let conn = McpConnection::connect("test", client_io)
        .await
        .expect("client handshake");
    (conn, state)
}

fn make_ctx() -> (Arc<ToolCtx>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let policy = SubtreePolicy::new(dir.path()).unwrap();
    let ctx = Arc::new(ToolCtx::new(Arc::new(policy)));
    (ctx, dir)
}

/// A dialer over the same duplex harness, instrumented so tests can assert
/// exactly when dials happen and force them to fail.
fn counting_dialer(dials: Arc<AtomicUsize>, broken: Arc<AtomicBool>) -> McpDialer {
    Arc::new(move || {
        let dials = dials.clone();
        let broken = broken.clone();
        let fut: BoxFuture<'static, Result<McpConnection, McpError>> = Box::pin(async move {
            dials.fetch_add(1, Ordering::SeqCst);
            if broken.load(Ordering::SeqCst) {
                return Err(McpError::Connect {
                    server: "test".to_string(),
                    message: "dial refused (synthetic)".to_string(),
                });
            }
            let (conn, _state) = connect().await;
            Ok(conn)
        });
        fut
    })
}

/// Export a catalog from a live connection, shut it down, and round-trip the
/// specs through JSON — the persistence step the offline path exists for.
async fn exported_catalog() -> Vec<ac_mcp::CachedToolSpec> {
    let (conn, _state) = connect().await;
    let specs = conn.export_catalog().await.unwrap();
    conn.shutdown();
    let json = serde_json::to_string(&specs).unwrap();
    serde_json::from_str(&json).unwrap()
}

fn drain(mut rx: mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

#[tokio::test]
async fn discovers_and_registers_with_server_prefix() {
    let (conn, _state) = connect().await;
    let mut registry = ToolRegistry::new();
    let result = conn
        .register_tools(&mut registry, &RegisterOptions::default())
        .await
        .unwrap();

    assert_eq!(
        result.registered,
        vec![
            "mcp__test__echo",
            "mcp__test__add",
            "mcp__test__always_fails",
            "mcp__test__slow",
        ]
    );

    // Contract-violating names are skipped and reported, never registered —
    // one bad name in a request would 400 every remaining completion call.
    assert_eq!(result.skipped.len(), 3);
    assert_eq!(result.skipped[0].remote_name, "bad name!");
    assert!(result.skipped[0].reason.contains("' '"));
    assert_eq!(result.skipped[1].remote_name, long_name());
    assert!(
        result.skipped[1].reason.contains("64"),
        "got: {}",
        result.skipped[1].reason
    );
    // An empty remote name must not hide behind the prefix as "mcp__test__".
    assert_eq!(result.skipped[2].remote_name, "");
    assert!(!registry.contains("mcp__test__bad name!"));
    assert!(!registry.contains("mcp__test__"));

    // The server's spec passes through verbatim — description and schema.
    let specs = registry.specs();
    let echo = specs.iter().find(|s| s.name == "mcp__test__echo").unwrap();
    assert_eq!(echo.description, "Echoes text back.");
    assert_eq!(echo.input_schema["properties"]["text"]["type"], "string");
    assert_eq!(echo.input_schema["required"][0], "text");

    // Annotations are untrusted by default: even the read-only-hinted tool
    // registers as Mutating, so a read-only permission mode stays safe.
    assert_eq!(
        registry.capability("mcp__test__echo"),
        Some(Capability::Mutating)
    );
}

#[tokio::test]
async fn prefix_modes_and_trusted_annotations() {
    let (conn, _state) = connect().await;

    let mut registry = ToolRegistry::new();
    let options = RegisterOptions {
        prefix: ToolPrefix::None,
        trust_annotations: true,
        ..RegisterOptions::default()
    };
    let result = conn.register_tools(&mut registry, &options).await.unwrap();
    assert_eq!(result.registered[0], "echo");
    // Unprefixed, the 61-byte name fits the provider contract again; only
    // the genuinely hostile names stay out.
    assert!(result.registered.contains(&long_name()));
    assert_eq!(result.skipped.len(), 2);
    assert_eq!(result.skipped[0].remote_name, "bad name!");
    assert_eq!(result.skipped[1].remote_name, "");
    // Opted in: the server's readOnlyHint is honored…
    assert_eq!(registry.capability("echo"), Some(Capability::ReadOnly));
    // …but only an explicit true hint upgrades; unannotated stays Mutating.
    assert_eq!(registry.capability("add"), Some(Capability::Mutating));

    let mut registry = ToolRegistry::new();
    let options = RegisterOptions {
        prefix: ToolPrefix::Custom("x_".into()),
        ..RegisterOptions::default()
    };
    let result = conn.register_tools(&mut registry, &options).await.unwrap();
    assert_eq!(result.registered[0], "x_echo");
    assert!(registry.contains("x_add"));
}

#[tokio::test]
async fn server_names_that_break_the_prefix_scheme_are_rejected() {
    for bad in ["", "a__b", "has space", "mcp__", "a_"] {
        let (client_io, _server_io) = tokio::io::duplex(1 << 10);
        let err = McpConnection::connect(bad, client_io)
            .await
            .expect_err("must reject");
        assert!(
            matches!(err, McpError::InvalidServerName { .. }),
            "{bad:?} → {err}"
        );
    }
}

#[tokio::test]
async fn call_roundtrip_and_errors_are_data() {
    let (conn, _state) = connect().await;
    let mut registry = ToolRegistry::new();
    conn.register_tools(&mut registry, &RegisterOptions::default())
        .await
        .unwrap();
    let (ctx, _dir) = make_ctx();

    // Happy path: arguments reach the server, text content comes back.
    let out = registry
        .run("mcp__test__echo", json!({ "text": "ping" }), ctx.clone())
        .await;
    assert!(!out.is_error, "echo failed: {}", out.content);
    assert_eq!(out.content, "ping");

    // A server-side isError result is model-visible error data.
    let out = registry
        .run("mcp__test__always_fails", json!({}), ctx.clone())
        .await;
    assert!(out.is_error);
    assert_eq!(out.content, "deliberate failure");

    // Server-side validation failures come back as error data too.
    let out = registry
        .run("mcp__test__add", json!({ "a": 2 }), ctx.clone())
        .await;
    assert!(out.is_error);
    assert!(out.content.contains("add requires numeric a and b"));

    // Non-object arguments are rejected client-side as error data.
    let out = registry
        .run("mcp__test__echo", json!("not an object"), ctx.clone())
        .await;
    assert!(out.is_error);
    assert!(out.content.contains("expected a JSON object"));
}

#[tokio::test]
async fn cancel_interrupts_a_slow_call_and_notifies_the_server() {
    let (conn, state) = connect().await;
    let mut registry = ToolRegistry::new();
    conn.register_tools(&mut registry, &RegisterOptions::default())
        .await
        .unwrap();
    let (ctx, _dir) = make_ctx();

    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });

    let start = Instant::now();
    let out = registry.run("mcp__test__slow", json!({}), ctx).await;
    assert!(out.is_error);
    assert!(out.content.contains("cancelled"), "got: {}", out.content);
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "cancel must not wait for the server"
    );

    // The abandoned call is not silently orphaned: the server receives
    // notifications/cancelled for it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !state.cancelled.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "server never saw notifications/cancelled"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn per_call_timeout_fails_as_error_data() {
    let (conn, _state) = connect().await;
    let mut registry = ToolRegistry::new();
    let options = RegisterOptions {
        call_timeout: Some(Duration::from_millis(200)),
        ..RegisterOptions::default()
    };
    conn.register_tools(&mut registry, &options).await.unwrap();
    let (ctx, _dir) = make_ctx();

    let start = Instant::now();
    let out = registry.run("mcp__test__slow", json!({}), ctx).await;
    assert!(out.is_error);
    assert!(out.content.contains("timed out"), "got: {}", out.content);
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn registered_tools_keep_the_connection_alive() {
    let (conn, _state) = connect().await;
    let mut registry = ToolRegistry::new();
    conn.register_tools(&mut registry, &RegisterOptions::default())
        .await
        .unwrap();
    drop(conn);

    let (ctx, _dir) = make_ctx();
    let out = registry
        .run("mcp__test__echo", json!({ "text": "still here" }), ctx)
        .await;
    assert!(
        !out.is_error,
        "connection died with the handle: {}",
        out.content
    );
    assert_eq!(out.content, "still here");
}

#[tokio::test]
async fn is_closed_observes_a_server_that_dies_on_its_own() {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let server = TestServer::default();
    let server_task = tokio::spawn(async move {
        let service = server.serve(server_io).await.expect("server handshake");
        let _ = service.waiting().await;
    });
    let conn = McpConnection::connect("test", client_io)
        .await
        .expect("client handshake");
    assert!(!conn.is_closed());

    // The server side dies without the client ever calling shutdown() — the
    // connection must still read as closed once the transport drops.
    server_task.abort();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !conn.is_closed() {
        assert!(
            Instant::now() < deadline,
            "is_closed never observed the dead transport"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn shutdown_turns_calls_into_error_data() {
    let (conn, _state) = connect().await;
    let mut registry = ToolRegistry::new();
    conn.register_tools(&mut registry, &RegisterOptions::default())
        .await
        .unwrap();

    assert!(!conn.is_closed());
    conn.shutdown();
    assert!(conn.is_closed());

    let (ctx, _dir) = make_ctx();
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        registry.run("mcp__test__echo", json!({ "text": "hi" }), ctx),
    )
    .await
    .expect("a call after shutdown must fail promptly, not hang");
    assert!(out.is_error);
    assert!(out.content.contains("failed"), "got: {}", out.content);
}

/// The point of the whole adapter: an MCP tool is indistinguishable from a
/// built-in to the runtime loop — dispatched by name, result fed back into
/// the next request, events emitted in order.
#[tokio::test]
async fn mcp_tool_rides_the_runtime_loop() {
    let (conn, _state) = connect().await;
    let mut registry = ToolRegistry::new();
    conn.register_tools(&mut registry, &RegisterOptions::default())
        .await
        .unwrap();

    let provider = MockProvider::new(vec![
        vec![
            tool_use("c1", "mcp__test__add", json!({ "a": 2, "b": 3 })),
            stop_tool_use(),
        ],
        vec![text("the sum is 5"), stop_end()],
    ]);
    let (ctx, _dir) = make_ctx();
    let mut session = Session::new(
        Arc::new(provider.clone()),
        Arc::new(registry),
        ctx,
        AgentConfig::default(),
    );

    let (tx, rx) = mpsc::unbounded_channel();
    let stop = session.run_turn("add 2 and 3".into(), tx).await.unwrap();
    assert!(matches!(stop, StopReason::EndTurn));
    assert_eq!(provider.call_count(), 2);

    // The second request must carry the MCP tool's result back to the model.
    let reqs = provider.requests();
    let fed_back = reqs[1].messages.iter().any(|m| {
        m.content.iter().any(|p| {
            matches!(
                p,
                ac_types::ContentPart::ToolResult(tr)
                    if tr.tool_use_id == "c1" && tr.content == "5" && !tr.is_error
            )
        })
    });
    assert!(
        fed_back,
        "MCP tool result must be fed back into the next request"
    );

    let events = drain(rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { id, is_error: false, .. } if id == "c1"))
    );
}

#[tokio::test]
async fn cached_specs_register_without_a_single_dial() {
    let specs = exported_catalog().await;

    let dials = Arc::new(AtomicUsize::new(0));
    let dialer = counting_dialer(dials.clone(), Arc::new(AtomicBool::new(false)));
    let mut registry = ToolRegistry::new();
    let result = register_cached(
        &mut registry,
        "test",
        &specs,
        dialer,
        &RegisterOptions::default(),
    )
    .unwrap();

    assert_eq!(
        dials.load(Ordering::SeqCst),
        0,
        "registration must not dial"
    );

    // Same names, same prefixing, same skip accounting as the live path.
    assert_eq!(
        result.registered,
        vec![
            "mcp__test__echo",
            "mcp__test__add",
            "mcp__test__always_fails",
            "mcp__test__slow",
        ]
    );
    assert_eq!(result.skipped.len(), 3);
    assert_eq!(result.skipped[0].remote_name, "bad name!");
    assert_eq!(result.skipped[1].remote_name, long_name());
    assert_eq!(result.skipped[2].remote_name, "");

    // Spec passthrough survives the serde round-trip…
    let registered = registry.specs();
    let echo = registered
        .iter()
        .find(|s| s.name == "mcp__test__echo")
        .unwrap();
    assert_eq!(echo.description, "Echoes text back.");
    assert_eq!(echo.input_schema["properties"]["text"]["type"], "string");
    // …and default distrust holds even though the cache carries the hint.
    assert_eq!(
        registry.capability("mcp__test__echo"),
        Some(Capability::Mutating)
    );
}

#[tokio::test]
async fn dynamic_registration_never_replaces_an_existing_tool() {
    let specs = exported_catalog().await;
    let dialer = counting_dialer(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    let mut registry = ToolRegistry::new();
    register_cached(
        &mut registry,
        "test",
        &specs,
        dialer.clone(),
        &RegisterOptions::default(),
    )
    .unwrap();
    let before = serde_json::to_value(registry.specs()).unwrap();
    let duplicate = register_cached(
        &mut registry,
        "test",
        &specs,
        dialer,
        &RegisterOptions::default(),
    )
    .unwrap();
    assert!(duplicate.registered.is_empty());
    assert!(
        duplicate
            .skipped
            .iter()
            .any(|tool| tool.reason.contains("already contains"))
    );
    assert_eq!(serde_json::to_value(registry.specs()).unwrap(), before);

    let (connection, _state) = connect().await;
    let live = connection
        .register_tools(&mut registry, &RegisterOptions::default())
        .await
        .unwrap();
    assert!(live.registered.is_empty());
    assert!(
        live.skipped
            .iter()
            .any(|tool| tool.reason.contains("already contains"))
    );
    assert_eq!(serde_json::to_value(registry.specs()).unwrap(), before);
}

#[tokio::test]
async fn cached_capability_honors_trust_annotations_opt_in_only() {
    let specs = exported_catalog().await;
    let dialer = counting_dialer(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
    );

    let mut registry = ToolRegistry::new();
    let options = RegisterOptions {
        trust_annotations: true,
        ..RegisterOptions::default()
    };
    register_cached(&mut registry, "test", &specs, dialer, &options).unwrap();
    // The cached readOnlyHint upgrades under the opt-in; unannotated stays.
    assert_eq!(
        registry.capability("mcp__test__echo"),
        Some(Capability::ReadOnly)
    );
    assert_eq!(
        registry.capability("mcp__test__add"),
        Some(Capability::Mutating)
    );
}

#[tokio::test]
async fn first_call_dials_and_the_second_reuses_the_connection() {
    let specs = exported_catalog().await;

    let dials = Arc::new(AtomicUsize::new(0));
    let dialer = counting_dialer(dials.clone(), Arc::new(AtomicBool::new(false)));
    let mut registry = ToolRegistry::new();
    register_cached(
        &mut registry,
        "test",
        &specs,
        dialer,
        &RegisterOptions::default(),
    )
    .unwrap();
    let (ctx, _dir) = make_ctx();

    let out = registry
        .run("mcp__test__echo", json!({ "text": "lazy" }), ctx.clone())
        .await;
    assert!(!out.is_error, "first call failed: {}", out.content);
    assert_eq!(out.content, "lazy");
    assert_eq!(dials.load(Ordering::SeqCst), 1, "first call dials once");

    // A sibling tool from the same batch shares the memoized connection.
    let out = registry
        .run("mcp__test__add", json!({ "a": 2, "b": 3 }), ctx)
        .await;
    assert!(!out.is_error, "second call failed: {}", out.content);
    assert_eq!(out.content, "5");
    assert_eq!(
        dials.load(Ordering::SeqCst),
        1,
        "second call must not re-dial"
    );
}

#[tokio::test]
async fn failed_dial_is_error_data_and_the_next_call_retries() {
    let specs = exported_catalog().await;

    let dials = Arc::new(AtomicUsize::new(0));
    let broken = Arc::new(AtomicBool::new(true));
    let dialer = counting_dialer(dials.clone(), broken.clone());
    let mut registry = ToolRegistry::new();
    register_cached(
        &mut registry,
        "test",
        &specs,
        dialer,
        &RegisterOptions::default(),
    )
    .unwrap();
    let (ctx, _dir) = make_ctx();

    let out = registry
        .run("mcp__test__echo", json!({ "text": "hi" }), ctx.clone())
        .await;
    assert!(out.is_error, "a failed dial must be error data");
    assert!(out.content.contains("dial refused"), "got: {}", out.content);
    assert_eq!(dials.load(Ordering::SeqCst), 1);

    // Not poisoned: fix the dialer and the same tool works.
    broken.store(false, Ordering::SeqCst);
    let out = registry
        .run("mcp__test__echo", json!({ "text": "recovered" }), ctx)
        .await;
    assert!(!out.is_error, "retry after a fixed dialer: {}", out.content);
    assert_eq!(out.content, "recovered");
    assert_eq!(dials.load(Ordering::SeqCst), 2, "the retry re-dials");
}

#[test]
fn register_cached_holds_the_server_name_to_the_prefix_rules() {
    let dialer = counting_dialer(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    let mut registry = ToolRegistry::new();
    let err = register_cached(
        &mut registry,
        "a__b",
        &[],
        dialer,
        &RegisterOptions::default(),
    )
    .expect_err("must reject");
    assert!(matches!(err, McpError::InvalidServerName { .. }), "{err}");
}

#[test]
fn tool_name_contract_is_exactly_the_provider_floor() {
    // MAX_TOOL_NAME_LEN is the strictest common provider limit.
    assert_eq!(MAX_TOOL_NAME_LEN, 64);
}
