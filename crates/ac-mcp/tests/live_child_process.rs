//! Live probe for the child-process (stdio) transport path — the one seam the
//! hermetic duplex tests cannot cover. Spawns the MCP reference server
//! (`@modelcontextprotocol/server-everything`) via npx and drives it through
//! the exact shipped path: `connect_command` → `register_tools` → registry
//! dispatch.
//!
//! It needs node + network, so it is `#[ignore]`d — CI never runs it. Run it
//! explicitly:
//!
//!   cargo test -p ac-mcp --test live_child_process -- --ignored --nocapture

use std::sync::Arc;

use ac_mcp::{McpConnection, RegisterOptions};
use ac_tool::{SubtreePolicy, ToolCtx, ToolRegistry};
use serde_json::json;

#[tokio::test]
#[ignore = "spawns `npx -y @modelcontextprotocol/server-everything`; needs node + network"]
async fn child_process_stdio_roundtrip_live() {
    let mut command = tokio::process::Command::new("npx");
    command
        .arg("-y")
        .arg("@modelcontextprotocol/server-everything");

    let conn = McpConnection::connect_command("everything", command)
        .await
        .expect("spawn + MCP handshake against the reference server");

    let mut registry = ToolRegistry::new();
    let result = conn
        .register_tools(&mut registry, &RegisterOptions::default())
        .await
        .expect("tool discovery");
    let names = &result.registered;
    eprintln!(
        "live child-process probe: {} tool(s) ({} skipped): {names:?}",
        names.len(),
        result.skipped.len()
    );
    assert!(
        names.iter().any(|n| n == "mcp__everything__echo"),
        "reference server should export echo; got {names:?}"
    );

    let dir = tempfile::tempdir().unwrap();
    let policy = SubtreePolicy::new(dir.path()).unwrap();
    let ctx = Arc::new(ToolCtx::new(Arc::new(policy)));

    let out = registry
        .run(
            "mcp__everything__echo",
            json!({ "message": "hello from ac" }),
            ctx,
        )
        .await;
    eprintln!("live child-process probe: echo -> {}", out.content);
    assert!(!out.is_error, "echo failed: {}", out.content);
    assert!(out.content.contains("hello from ac"));

    conn.shutdown();
}
