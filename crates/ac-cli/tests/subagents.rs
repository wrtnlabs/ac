//! Sub-agent delegation through the SHIPPED host wiring (`build_host` with
//! `HostOptions.subagents`). The offline test scripts a `MockProvider` so a
//! parent delegates to the `general` child, which runs a real agent loop and
//! writes a real file — proving the CLI's task-tool + reference-spawner wiring,
//! not just the seam. The live test does the same against real OpenRouter.

use std::sync::Arc;

use ac_cli::{HostOptions, build_host};
use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::{AgentEvent, Session};
use ac_types::{Effort, StopReason};
use serde_json::json;

fn subagent_options() -> HostOptions {
    HostOptions {
        subagents: true,
        ..Default::default()
    }
}

async fn run(mut session: Session, prompt: &str) -> (Result<StopReason, String>, Vec<AgentEvent>) {
    let prompt = prompt.to_string();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let driver = tokio::spawn(async move { session.run_turn(prompt, tx).await });
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    let result = driver.await.expect("join").map_err(|e| e.to_string());
    (result, events)
}

fn saw_task_call(events: &[AgentEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "task"))
}

#[tokio::test]
async fn parent_delegates_to_a_child_that_writes_a_real_file_offline() {
    let dir = tempfile::tempdir().unwrap();

    // One shared MockProvider drives BOTH the parent and the child (its cursor
    // advances across the nested run): parent delegates → child writes → child
    // finishes → parent finishes.
    let provider = MockProvider::new(vec![
        // Parent step 0: delegate to `general`.
        vec![
            tool_use(
                "c1",
                "task",
                json!({
                    "agent": "general",
                    "prompt": "create hello.txt containing 'from child'"
                }),
            ),
            stop_tool_use(),
        ],
        // Child step 0: the general worker writes the file.
        vec![
            tool_use(
                "g1",
                "write_file",
                json!({ "path": "hello.txt", "content": "from child" }),
            ),
            stop_tool_use(),
        ],
        // Child step 1: the child finishes.
        vec![text("wrote hello.txt"), stop_end()],
        // Parent step 1: the parent finishes.
        vec![text("done — the sub-agent wrote the file"), stop_end()],
    ]);
    let handle = provider.clone();

    let host = build_host(
        Arc::new(provider),
        dir.path(),
        "mock/model".to_string(),
        subagent_options(),
    )
    .expect("build_host with subagents");

    let (result, events) = run(host.session, "make the file via a sub-agent").await;
    assert_eq!(result.unwrap(), StopReason::EndTurn);

    // The delegation actually happened, and the child's work landed on disk.
    assert!(
        saw_task_call(&events),
        "the parent must have called the task tool"
    );
    let contents = std::fs::read_to_string(dir.path().join("hello.txt"))
        .expect("the child must have written hello.txt into the workspace");
    assert_eq!(contents, "from child");

    // Context disjointness: the parent's LAST request holds only the task call +
    // its result, not the child's write turns.
    let reqs = handle.requests();
    let last = reqs.last().unwrap();
    let child_leak = last.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|p| matches!(p, ac_types::ContentPart::ToolUse(tu) if tu.name == "write_file"))
    });
    assert!(
        !child_leak,
        "the child's write turns must not enter the parent's context"
    );
}

#[tokio::test]
async fn effort_flows_to_the_parent_and_a_child_overrides_via_its_definition() {
    let dir = tempfile::tempdir().unwrap();

    // Session default effort = high; the read-only `explore` agent declares
    // itself Low, so it must run cheap even under the high-effort parent.
    let provider = MockProvider::new(vec![
        // Parent step 0: delegate to explore.
        vec![
            tool_use(
                "c1",
                "task",
                json!({ "agent": "explore", "prompt": "look around" }),
            ),
            stop_tool_use(),
        ],
        // Child (explore) — one request, returns text (read-only, no writes).
        vec![text("explored"), stop_end()],
        // Parent step 1: finish.
        vec![text("done"), stop_end()],
    ]);
    let handle = provider.clone();

    let host = build_host(
        Arc::new(provider),
        dir.path(),
        "mock/model".to_string(),
        HostOptions {
            subagents: true,
            effort: Some(Effort::High),
            ..Default::default()
        },
    )
    .expect("build_host");

    let (result, _events) = run(host.session, "explore via a sub-agent").await;
    assert_eq!(result.unwrap(), StopReason::EndTurn);

    let reqs = handle.requests();
    // Parent's request carries the session default (config → request).
    assert_eq!(
        reqs[0].effort,
        Some(Effort::High),
        "parent uses the session default"
    );
    // The child's request (the second) carries the explore definition's default,
    // overriding the parent's high — the cheap-child / expensive-parent split.
    assert_eq!(
        reqs[1].effort,
        Some(Effort::Low),
        "explore's definition-default effort must override the parent's"
    );
}

/// Live proof against real OpenRouter: a real model, told to delegate, actually
/// calls `task`, and the `general` sub-agent — a real agent loop on the real
/// model — writes a real file into the workspace.
///
/// Run with:
///   OPENROUTER_API_KEY=sk-or-... cargo test -p ac-cli --test subagents -- --ignored
/// Override the model with AC_LIVE_MODEL (default: anthropic/claude-sonnet-5).
#[tokio::test]
#[ignore = "hits the live OpenRouter API; requires OPENROUTER_API_KEY"]
async fn live_parent_delegates_to_a_child_that_writes_a_file() {
    use ac_provider_openrouter::OpenRouter;

    let api_key = std::env::var("OPENROUTER_API_KEY")
        .expect("set OPENROUTER_API_KEY to run the live sub-agent proof");
    let model =
        std::env::var("AC_LIVE_MODEL").unwrap_or_else(|_| "anthropic/claude-sonnet-5".to_string());

    let dir = tempfile::tempdir().unwrap();
    let host = build_host(
        Arc::new(OpenRouter::new(api_key)),
        dir.path(),
        model,
        subagent_options(),
    )
    .expect("build_host with subagents");

    let prompt = "Delegate to the `general` sub-agent using the `task` tool: have it create a \
                  file named proof.txt in the working directory containing exactly the word \
                  PINEAPPLE (nothing else). Do NOT create the file yourself — the sub-agent must. \
                  After it finishes, tell me it is done.";
    let (result, events) = run(host.session, prompt).await;
    assert_eq!(
        result.expect("the live turn must complete"),
        StopReason::EndTurn
    );

    // The parent actually delegated...
    assert!(
        saw_task_call(&events),
        "the model was told to delegate but never called the task tool"
    );
    // ...and the sub-agent actually did the work, on disk.
    let contents = std::fs::read_to_string(dir.path().join("proof.txt"))
        .expect("the sub-agent must have written proof.txt");
    assert!(
        contents.contains("PINEAPPLE"),
        "proof.txt must contain PINEAPPLE, got: {contents:?}"
    );
    eprintln!("live sub-agent proof: proof.txt = {contents:?}");
}
