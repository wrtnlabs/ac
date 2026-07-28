//! End-to-end proof of the sub-agent seam ([docs/ac-subagents.md]): a parent
//! `Session` with the `task` tool and a [`ReferenceSpawner`] delegates to a
//! child `Session` that runs and returns — the parent seeing only the tool call
//! and its result (context disjointness, I2), and a child unable to recurse (the
//! structural guard, I1). Hermetic: MockProvider, temp dirs, no network.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::{AgentConfig, AgentEvent, ReferenceSpawner, Session};
use ac_tool::{
    AgentDefinition, AgentSpawner, SpawnRequest, SpawnResult, SpawnStatus, SubtreePolicy, ToolCtx,
    ToolRegistry, as_dyn,
};
use ac_tools::Task;
use ac_types::{ContentPart, StopReason};
use futures::future::BoxFuture;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn config(model: &str) -> AgentConfig {
    AgentConfig {
        model: model.to_string(),
        ..Default::default()
    }
}

fn task(agent: &str) -> Task {
    Task::new(&[AgentDefinition::new(agent, "Test child agent.")])
}

async fn run(mut parent: Session, prompt: &str) -> (StopReason, Vec<AgentEvent>) {
    let prompt = prompt.to_string();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let driver = tokio::spawn(async move { parent.run_turn(prompt, tx).await });
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    let stop = driver.await.expect("join").expect("parent turn ok");
    (stop, events)
}

fn task_result(events: &[AgentEvent]) -> (String, bool) {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                name,
                output,
                is_error,
                ..
            } if name == "task" => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("a task tool result")
}

#[tokio::test]
async fn a_parent_delegates_and_sees_only_the_task_call_and_result() {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().to_path_buf();

    // Records the model the assembler resolved, to prove the per-child override
    // reaches it (the reviewer-caught gap: the request must carry `model`).
    let seen_model = Arc::new(Mutex::new(None::<String>));
    let recorder = seen_model.clone();

    // The child assembler: builds a fresh child Session with NO spawner (the
    // recursion guard) and the parent-derived cancel token.
    let assemble = move |req: &SpawnRequest, cancel: CancellationToken| -> Option<Session> {
        if req.agent != "echo-agent" {
            return None;
        }
        // Honor a per-child model override when present (proves the seam plumbs it).
        let model = req
            .model
            .clone()
            .unwrap_or_else(|| "mock/child".to_string());
        *recorder.lock().unwrap() = Some(model.clone());
        let child_provider = MockProvider::new(vec![vec![text("child says hi"), stop_end()]]);
        let child_ctx =
            ToolCtx::new(Arc::new(SubtreePolicy::new(&path).unwrap())).with_cancel(cancel);
        Some(Session::new(
            Arc::new(child_provider),
            Arc::new(ToolRegistry::new()),
            Arc::new(child_ctx),
            config(&model),
        ))
    };
    let spawner = as_dyn(ReferenceSpawner::new(assemble));

    let mut parent_registry = ToolRegistry::new();
    parent_registry.register(task("echo-agent"));
    let parent_ctx = Arc::new(
        ToolCtx::new(Arc::new(SubtreePolicy::new(dir.path()).unwrap())).with_spawner(spawner),
    );
    let parent_provider = MockProvider::new(vec![
        vec![
            tool_use(
                "c1",
                "task",
                json!({ "agent": "echo-agent", "prompt": "say hi", "model": "mock/override" }),
            ),
            stop_tool_use(),
        ],
        vec![text("parent done"), stop_end()],
    ]);
    let parent = Session::new(
        Arc::new(parent_provider.clone()),
        Arc::new(parent_registry),
        parent_ctx,
        config("mock/parent"),
    );

    let (stop, events) = run(parent, "go").await;
    assert_eq!(stop, StopReason::EndTurn);

    // The per-child model override reached the assembler (the reviewer-caught gap).
    assert_eq!(
        seen_model.lock().unwrap().as_deref(),
        Some("mock/override"),
        "the task tool's model override must reach the spawner"
    );

    // The parent saw the child's final text wrapped in the completed envelope.
    let (output, is_error) = task_result(&events);
    assert!(
        !is_error,
        "a completed delegation is not an error: {output}"
    );
    assert!(
        output.contains("child says hi"),
        "envelope carries the child's final text: {output}"
    );
    assert!(output.contains("\"status\":\"completed\""));
    assert!(output.contains("session_id"));

    // Context disjointness (I2): the parent's second request grew by EXACTLY the
    // task call and its result — three messages (user, assistant+task-call,
    // user+task-result). The child's own turns are in the child's log, not here.
    let reqs = parent_provider.requests();
    assert_eq!(
        reqs[1].messages.len(),
        3,
        "the child's turns must not enter the parent's context: {:?}",
        reqs[1].messages
    );
    // And the ONE assistant tool-use in the parent's log is the task call.
    let parent_tool_uses: usize = reqs[1]
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|p| matches!(p, ContentPart::ToolUse(tu) if tu.name == "task"))
        .count();
    assert_eq!(parent_tool_uses, 1);
}

#[tokio::test]
async fn a_child_cannot_recurse_even_when_it_holds_the_task_tool() {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().to_path_buf();
    // Count assembler invocations: exactly one (the child). If the child could
    // delegate, a grandchild would be assembled and this would be two.
    let assembled = Arc::new(AtomicUsize::new(0));

    let counter = assembled.clone();
    let assemble = move |_req: &SpawnRequest, cancel: CancellationToken| -> Option<Session> {
        counter.fetch_add(1, Ordering::SeqCst);
        // The child even HOLDS the task tool — but its ctx has no spawner, so its
        // delegation attempt must self-refuse rather than recurse.
        let mut child_registry = ToolRegistry::new();
        child_registry.register(task("anything"));
        let child_provider = MockProvider::new(vec![
            vec![
                tool_use(
                    "g1",
                    "task",
                    json!({ "agent": "anything", "prompt": "recurse" }),
                ),
                stop_tool_use(),
            ],
            vec![text("child done"), stop_end()],
        ]);
        let child_ctx =
            ToolCtx::new(Arc::new(SubtreePolicy::new(&path).unwrap())).with_cancel(cancel);
        // NOTE: deliberately NO `.with_spawner(...)` — the guard under test.
        Some(Session::new(
            Arc::new(child_provider),
            Arc::new(child_registry),
            Arc::new(child_ctx),
            config("mock/child"),
        ))
    };
    let spawner = as_dyn(ReferenceSpawner::new(assemble));

    let mut parent_registry = ToolRegistry::new();
    parent_registry.register(task("recursive"));
    let parent_ctx = Arc::new(
        ToolCtx::new(Arc::new(SubtreePolicy::new(dir.path()).unwrap())).with_spawner(spawner),
    );
    let parent_provider = MockProvider::new(vec![
        vec![
            tool_use(
                "c1",
                "task",
                json!({ "agent": "recursive", "prompt": "go deep" }),
            ),
            stop_tool_use(),
        ],
        vec![text("parent done"), stop_end()],
    ]);
    let parent = Session::new(
        Arc::new(parent_provider),
        Arc::new(parent_registry),
        parent_ctx,
        config("mock/parent"),
    );

    let (stop, events) = run(parent, "go").await;
    assert_eq!(stop, StopReason::EndTurn);

    // The child completed (it recovered from its own refused delegation)...
    let (output, is_error) = task_result(&events);
    assert!(!is_error, "the child completed: {output}");
    assert!(output.contains("child done"));
    // ...and the assembler ran exactly once: no grandchild was ever built,
    // because the child's ctx carried no spawner (I1, recursion by absence).
    assert_eq!(
        assembled.load(Ordering::SeqCst),
        1,
        "a grandchild must never be assembled"
    );
}

#[tokio::test]
async fn concurrent_task_calls_keep_their_exact_invocation_ids() {
    struct RecordingSpawner {
        seen: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl AgentSpawner for RecordingSpawner {
        fn spawn(&self, req: SpawnRequest) -> BoxFuture<'static, SpawnResult> {
            let seen = self.seen.clone();
            Box::pin(async move {
                seen.lock()
                    .unwrap()
                    .push((req.prompt.clone(), req.tool_call_id));
                // Complete in the opposite order from the provider's call
                // order. Identity must come from each invocation context, not
                // a start/finish observation queue.
                if req.prompt == "slow" {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                SpawnResult {
                    session_id: format!("s_{}", req.prompt),
                    output: req.prompt,
                    status: SpawnStatus::Completed,
                }
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let spawner = as_dyn(RecordingSpawner { seen: seen.clone() });
    let mut registry = ToolRegistry::new();
    registry.register(task("general"));
    let ctx = Arc::new(
        ToolCtx::new(Arc::new(SubtreePolicy::new(dir.path()).unwrap())).with_spawner(spawner),
    );
    let provider = MockProvider::new(vec![
        vec![
            tool_use(
                "call_slow",
                "task",
                json!({ "agent": "general", "prompt": "slow" }),
            ),
            tool_use(
                "call_fast",
                "task",
                json!({ "agent": "general", "prompt": "fast" }),
            ),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);
    let parent = Session::new(
        Arc::new(provider),
        Arc::new(registry),
        ctx,
        config("mock/parent"),
    );

    let (stop, _events) = run(parent, "delegate").await;
    assert_eq!(stop, StopReason::EndTurn);
    let mut seen = seen.lock().unwrap().clone();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("fast".into(), "call_fast".into()),
            ("slow".into(), "call_slow".into()),
        ]
    );
}
