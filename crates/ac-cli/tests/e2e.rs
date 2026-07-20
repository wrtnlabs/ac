//! Offline end-to-end proof of the whole AC stack: a scripted `MockProvider`
//! drives the REAL built-in tool registry over the REAL runtime loop against a
//! REAL temp directory. No network, no API key.
//!
//! These tests are the definition of done — if they pass, the generic agent
//! loop with built-in tools genuinely works, tools mutate disk, results feed
//! back to the model, and the path policy actually contains writes.

use std::sync::Arc;

use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::{AgentEvent, Session};
use ac_types::{ContentPart, Role, StopReason};
use serde_json::json;

/// Assemble through the SHIPPED wiring path (`ac_cli::build_host`) — the same
/// function `main.rs` uses — so this test breaks if the real host wiring
/// (built-in registration, system prompt, path policy) is gutted. Only the
/// provider is swapped for the scripted mock.
fn built_session(provider: MockProvider, dir: &std::path::Path) -> (Session, MockProvider) {
    let handle = provider.clone();
    let host = ac_cli::build_host(
        Arc::new(provider),
        dir,
        "mock/model".to_string(),
        ac_cli::HostOptions::default(),
    )
    .expect("build_host");
    (host.session, handle)
}

/// The shipped generic host must register every built-in and carry a system
/// prompt. Deleting `register_builtins` or emptying `SYSTEM_PROMPT` in the
/// library fails here — the standing-proof host cannot silently rot.
#[test]
fn shipped_host_registers_all_builtins_and_a_prompt() {
    let specs = ac_cli::generic_registry().specs();
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    for expected in [
        "read_file",
        "write_file",
        "edit_file",
        "list_files",
        "glob",
        "grep",
        "shell",
        "fetch",
    ] {
        assert!(names.contains(&expected), "missing built-in: {expected}");
    }
    assert_eq!(specs.len(), 8, "unexpected built-in tool count");
    assert!(
        !ac_cli::SYSTEM_PROMPT.trim().is_empty(),
        "the generic host must ship a system prompt"
    );
}

/// Drain the event sink into a flat vec, running the turn to completion.
async fn run(mut session: Session, prompt: &str) -> (Result<StopReason, String>, Vec<AgentEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let prompt = prompt.to_string();
    let driver = tokio::spawn(async move { session.run_turn(prompt, tx).await });

    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    let result = driver.await.expect("join").map_err(|e| e.to_string());
    (result, events)
}

#[tokio::test]
async fn agent_reads_seed_then_writes_new_file_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "hello").unwrap();

    // Three scripted turns: read the seed, write a NEW file, then answer.
    let provider = MockProvider::new(vec![
        vec![
            tool_use("call-read", "read_file", json!({ "path": "notes.txt" })),
            stop_tool_use(),
        ],
        vec![
            tool_use(
                "call-write",
                "write_file",
                json!({ "path": "out.txt", "content": "world" }),
            ),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);

    let (session, handle) = built_session(provider, dir.path());
    let (result, events) = run(session, "read notes then write out").await;

    // --- ground truth on disk ---
    let out = dir.path().join("out.txt");
    assert!(out.exists(), "out.txt should have been created on disk");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "world");

    // --- the read tool actually returned the seed content ---
    let read_result = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                name, output, id, ..
            } if name == "read_file" && id == "call-read" => Some(output.clone()),
            _ => None,
        })
        .expect("expected a read_file tool result");
    assert!(
        read_result.contains("hello"),
        "read_file result should carry the seed: {read_result}"
    );

    // --- write reported success (not an error) ---
    let write_ok = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolResult { name, is_error, .. } if name == "write_file" && !is_error
        )
    });
    assert!(write_ok, "write_file should have succeeded");

    // --- final stop reason is EndTurn ---
    assert_eq!(result.expect("turn ok"), StopReason::EndTurn);

    // --- the provider was called 3 times ---
    assert_eq!(handle.call_count(), 3, "expected three model round-trips");

    // --- later requests carried ToolResult content back to the model ---
    let requests = handle.requests();
    let tool_results_fed_back: usize = requests
        .iter()
        .flat_map(|r| r.messages.iter())
        .flat_map(|m| m.content.iter())
        .filter(|p| matches!(p, ContentPart::ToolResult(_)))
        .count();
    assert!(
        tool_results_fed_back >= 2,
        "read+write tool results should have been fed back into later requests, saw {tool_results_fed_back}"
    );

    // The last request must contain the read_file result echoed as a User
    // ToolResult with the seed content — proving the loop closes.
    let last = requests.last().expect("a request");
    let fed_hello = last.messages.iter().any(|m| {
        m.role == Role::User
            && m.content
                .iter()
                .any(|p| matches!(p, ContentPart::ToolResult(tr) if tr.content.contains("hello")))
    });
    assert!(
        fed_hello,
        "the seed content should be visible to the model in the final request"
    );
}

#[tokio::test]
async fn policy_refuses_write_escaping_root_end_to_end() {
    let dir = tempfile::tempdir().unwrap();

    // Point the escape at a sibling of the sandbox root so we can assert it was
    // never created anywhere outside.
    let outside = dir.path().parent().unwrap().join("escape.txt");
    let _ = std::fs::remove_file(&outside);

    let provider = MockProvider::new(vec![
        vec![
            tool_use(
                "call-escape",
                "write_file",
                json!({ "path": "../escape.txt", "content": "pwned" }),
            ),
            stop_tool_use(),
        ],
        vec![text("blocked"), stop_end()],
    ]);

    let (session, handle) = built_session(provider, dir.path());
    let (result, events) = run(session, "try to escape").await;

    // The tool result the model saw must be an error (policy refusal as DATA).
    let refused = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolResult { name, is_error, .. } if name == "write_file" && *is_error
        )
    });
    assert!(refused, "escaping write must come back as an error");

    // Ground truth: no file materialized outside the sandbox root.
    assert!(
        !outside.exists(),
        "policy must have prevented any write outside the root"
    );
    assert!(
        !dir.path().join("../escape.txt").exists(),
        "no escape.txt outside root"
    );

    // The loop still completed cleanly and the error was fed back to the model.
    assert_eq!(result.expect("turn ok"), StopReason::EndTurn);
    let saw_error_result = handle
        .requests()
        .iter()
        .flat_map(|r| r.messages.iter())
        .flat_map(|m| m.content.iter())
        .any(|p| matches!(p, ContentPart::ToolResult(tr) if tr.is_error));
    assert!(
        saw_error_result,
        "the policy-refusal error should have been fed back to the model"
    );
}
